use aes_gcm::{aead::{Aead, KeyInit}, Aes256Gcm, Nonce};
use axum::{
    extract::{Path, Query, Request, State},
    http::{header::AUTHORIZATION, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{delete, get, post},
    Json, Router,
};
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use hmac::{Hmac, Mac};
use rand::{rngs::OsRng, RngCore};
use reqwest::{Client, Url};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use sqlx::{postgres::PgPoolOptions, FromRow, PgPool};
use std::{env, net::SocketAddr, time::Duration};
use subtle::ConstantTimeEq;
use tracing_subscriber::EnvFilter;
use uuid::Uuid;

type HmacSha256 = Hmac<Sha256>;
const TIKTOK_AUTHORIZE_URL: &str = "https://www.tiktok.com/v2/auth/authorize/";
const TIKTOK_TOKEN_URL: &str = "https://open.tiktokapis.com/v2/oauth/token/";
const TIKTOK_USER_INFO_URL: &str = "https://open.tiktokapis.com/v2/user/info/";
const TIKTOK_VIDEO_LIST_URL: &str = "https://open.tiktokapis.com/v2/video/list/";
const POLL_INTERVAL_HOURS: i64 = 12;

#[derive(Clone)]
struct TikTokConfig {
    client_key: String,
    client_secret: String,
    redirect_uri: String,
    encryption_key: [u8; 32],
}

#[derive(Clone)]
struct AppState {
    db: PgPool,
    api_token: String,
    webhook_url: Option<String>,
    webhook_secret: Option<String>,
    tiktok: Option<TikTokConfig>,
    http: Client,
}

#[derive(Serialize)]
struct HealthResponse {
    status: &'static str,
    service: &'static str,
    webhook_configured: bool,
    tiktok_provider_configured: bool,
}

#[derive(Debug, Serialize, FromRow)]
struct Creator {
    id: Uuid,
    username: String,
    display_name: Option<String>,
    enabled: bool,
    created_at: DateTime<Utc>,
}

#[derive(Deserialize)]
struct CreateCreator {
    username: String,
}

#[derive(Deserialize)]
struct IngestEvent {
    creator_id: Uuid,
    platform_video_id: String,
    payload: Value,
    published_at: Option<DateTime<Utc>>,
}

#[derive(Serialize)]
struct IngestResponse {
    accepted: bool,
    duplicate: bool,
    video_id: Uuid,
    delivery_id: Option<Uuid>,
}

#[derive(FromRow)]
struct ClaimedDelivery {
    id: Uuid,
    attempts: i32,
    payload: Value,
}

#[derive(Serialize)]
struct OAuthStartResponse {
    authorization_url: String,
    reference: String,
    expires_in_seconds: u64,
}

#[derive(Deserialize)]
struct OAuthStartRequest {
    reference: String,
}

#[derive(Serialize, FromRow)]
struct OAuthConnectionStatus {
    status: String,
    creator_id: Option<Uuid>,
    display_name: Option<String>,
    expires_at: DateTime<Utc>,
}

#[derive(FromRow)]
struct ConsumedOAuthState {
    state_hash: String,
    client_reference_hash: Option<String>,
}

#[derive(Deserialize)]
struct OAuthCallback {
    code: Option<String>,
    state: String,
    error: Option<String>,
    error_description: Option<String>,
}

#[derive(Deserialize)]
struct TikTokTokenResponse {
    access_token: String,
    expires_in: i64,
    refresh_token: String,
    refresh_expires_in: i64,
    open_id: String,
    scope: String,
}

#[derive(Deserialize)]
struct TikTokUserInfoResponse {
    data: Option<TikTokUserInfoData>,
    error: Option<TikTokApiError>,
}

#[derive(Deserialize)]
struct TikTokUserInfoData {
    user: Option<TikTokUser>,
}

#[derive(Deserialize)]
struct TikTokUser {
    open_id: String,
    display_name: Option<String>,
}

#[derive(Deserialize)]
struct TikTokApiError {
    code: Option<String>,
    message: Option<String>,
}

#[derive(Deserialize)]
struct TikTokVideoListResponse {
    data: Option<TikTokVideoListData>,
    error: Option<TikTokApiError>,
}

#[derive(Deserialize)]
struct TikTokVideoListData {
    videos: Option<Vec<TikTokVideo>>,
    cursor: Option<i64>,
    has_more: Option<bool>,
}

#[derive(Deserialize)]
struct TikTokVideo {
    id: String,
    title: Option<String>,
    video_description: Option<String>,
    create_time: Option<i64>,
    duration: Option<i64>,
    cover_image_url: Option<String>,
    share_url: Option<String>,
    embed_link: Option<String>,
}

#[derive(Clone, FromRow)]
struct PollingCreator {
    id: Uuid,
    username: String,
    display_name: Option<String>,
    access_token_enc: String,
    refresh_token_enc: String,
    access_expires_at: DateTime<Utc>,
    refresh_expires_at: DateTime<Utc>,
}

fn encrypt_token(key: &[u8; 32], token: &str) -> anyhow::Result<String> {
    let cipher = Aes256Gcm::new_from_slice(key)
        .map_err(|_| anyhow::anyhow!("invalid token encryption key"))?;
    let mut nonce_bytes = [0_u8; 12];
    OsRng.fill_bytes(&mut nonce_bytes);
    let ciphertext = cipher
        .encrypt(Nonce::from_slice(&nonce_bytes), token.as_bytes())
        .map_err(|_| anyhow::anyhow!("could not encrypt TikTok token"))?;
    Ok(format!("{}{}", hex::encode(nonce_bytes), hex::encode(ciphertext)))
}

fn decrypt_token(key: &[u8; 32], encrypted: &str) -> anyhow::Result<String> {
    let bytes = hex::decode(encrypted)?;
    if bytes.len() <= 12 {
        anyhow::bail!("encrypted token is truncated");
    }
    let cipher = Aes256Gcm::new_from_slice(key)
        .map_err(|_| anyhow::anyhow!("invalid token encryption key"))?;
    let plaintext = cipher
        .decrypt(Nonce::from_slice(&bytes[..12]), &bytes[12..])
        .map_err(|_| anyhow::anyhow!("could not decrypt TikTok token"))?;
    Ok(String::from_utf8(plaintext)?)
}

fn validate_webhook_url(raw: &str) -> anyhow::Result<Url> {
    let url = Url::parse(raw)?;
    let host = url.host_str().unwrap_or_default();
    let loopback = matches!(host, "localhost" | "127.0.0.1" | "::1");
    if url.scheme() != "https" && !(url.scheme() == "http" && loopback) {
        anyhow::bail!("WEBHOOK_URL must use HTTPS (HTTP is allowed only for localhost)");
    }
    if !url.username().is_empty() || url.password().is_some() || url.fragment().is_some() {
        anyhow::bail!("WEBHOOK_URL must not contain credentials or a URL fragment");
    }
    Ok(url)
}

fn valid_oauth_reference(reference: &str) -> bool {
    (32..=128).contains(&reference.len())
        && reference.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
}

fn hash_state(state: &str) -> String {
    hex::encode(Sha256::digest(state.as_bytes()))
}

async fn health(State(state): State<AppState>) -> Json<HealthResponse> {
    Json(HealthResponse {
        status: "ok",
        service: "voxytok",
        webhook_configured: state.webhook_url.is_some(),
        tiktok_provider_configured: state.tiktok.is_some(),
    })
}

async fn readiness(State(state): State<AppState>) -> Result<Json<HealthResponse>, StatusCode> {
    sqlx::query("SELECT 1").execute(&state.db).await
        .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
    Ok(Json(HealthResponse {
        status: "ready",
        service: "voxytok",
        webhook_configured: state.webhook_url.is_some(),
        tiktok_provider_configured: state.tiktok.is_some(),
    }))
}

async fn authenticate(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    let supplied = request.headers().get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .ok_or(StatusCode::UNAUTHORIZED)?;
    let expected = state.api_token.as_bytes();
    let actual = supplied.as_bytes();
    if expected.len() != actual.len() || !bool::from(expected.ct_eq(actual)) {
        return Err(StatusCode::UNAUTHORIZED);
    }
    Ok(next.run(request).await)
}

async fn create_creator(
    State(state): State<AppState>,
    Json(payload): Json<CreateCreator>,
) -> Result<(StatusCode, Json<Creator>), StatusCode> {
    let username = payload.username.trim().trim_start_matches('@').to_ascii_lowercase();
    if username.is_empty() || username.len() > 64
        || !username.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'.' || c == b'_' || c == b'-') {
        return Err(StatusCode::UNPROCESSABLE_ENTITY);
    }
    let creator = sqlx::query_as::<_, Creator>(
        "INSERT INTO creators (id, username) VALUES ($1, $2)
         ON CONFLICT (username) DO UPDATE SET username = EXCLUDED.username
         RETURNING id, username, display_name, enabled, created_at",
    )
    .bind(Uuid::new_v4()).bind(username).fetch_one(&state.db).await
    .map_err(|error| { tracing::error!(%error, "could not create creator"); StatusCode::INTERNAL_SERVER_ERROR })?;
    Ok((StatusCode::CREATED, Json(creator)))
}

async fn list_creators(State(state): State<AppState>) -> Result<Json<Vec<Creator>>, StatusCode> {
    let creators = sqlx::query_as::<_, Creator>(
        "SELECT id, username, display_name, enabled, created_at FROM creators ORDER BY created_at DESC",
    ).fetch_all(&state.db).await
        .map_err(|error| { tracing::error!(%error, "could not list creators"); StatusCode::INTERNAL_SERVER_ERROR })?;
    Ok(Json(creators))
}

async fn delete_creator(State(state): State<AppState>, Path(id): Path<Uuid>) -> Result<StatusCode, StatusCode> {
    let result = sqlx::query("DELETE FROM creators WHERE id = $1").bind(id).execute(&state.db).await
        .map_err(|error| { tracing::error!(%error, "could not delete creator"); StatusCode::INTERNAL_SERVER_ERROR })?;
    if result.rows_affected() == 0 { return Err(StatusCode::NOT_FOUND); }
    Ok(StatusCode::NO_CONTENT)
}

async fn start_tiktok_oauth(
    State(state): State<AppState>,
    Json(input): Json<OAuthStartRequest>,
) -> Result<Json<OAuthStartResponse>, StatusCode> {
    let config = state.tiktok.as_ref().ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
    if !valid_oauth_reference(&input.reference) {
        return Err(StatusCode::UNPROCESSABLE_ENTITY);
    }

    // Keep only a digest of the consuming application's opaque correlation token.
    let reference_hash = hash_state(&input.reference);
    let state_token = Uuid::new_v4().to_string() + &Uuid::new_v4().simple().to_string();
    sqlx::query("DELETE FROM oauth_states WHERE expires_at < NOW() - INTERVAL '1 day'")
        .execute(&state.db).await
        .map_err(|error| { tracing::error!(%error, "could not clean old OAuth states"); StatusCode::INTERNAL_SERVER_ERROR })?;
    sqlx::query(
        "INSERT INTO oauth_states (state_hash, client_reference_hash, expires_at)
         VALUES ($1,$2,NOW()+INTERVAL '10 minutes')",
    ).bind(hash_state(&state_token)).bind(reference_hash).execute(&state.db).await
        .map_err(|error| {
            if error.as_database_error().is_some_and(|db| db.code().as_deref() == Some("23505")) {
                StatusCode::CONFLICT
            } else {
                tracing::error!(%error, "could not persist OAuth state");
                StatusCode::INTERNAL_SERVER_ERROR
            }
        })?;

    let mut url = Url::parse(TIKTOK_AUTHORIZE_URL).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    url.query_pairs_mut()
        .append_pair("client_key", &config.client_key)
        .append_pair("response_type", "code")
        .append_pair("scope", "user.info.basic,video.list")
        .append_pair("redirect_uri", &config.redirect_uri)
        .append_pair("state", &state_token)
        .append_pair("disable_auto_auth", "1");
    Ok(Json(OAuthStartResponse {
        authorization_url: url.to_string(),
        reference: input.reference,
        expires_in_seconds: 600,
    }))
}

async fn tiktok_callback(
    State(state): State<AppState>,
    Query(callback): Query<OAuthCallback>,
) -> Result<impl IntoResponse, StatusCode> {
    let config = state.tiktok.as_ref().ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
    let consumed = sqlx::query_as::<_, ConsumedOAuthState>(
        "UPDATE oauth_states SET consumed_at=NOW(),status='processing'
         WHERE state_hash=$1 AND expires_at>NOW() AND consumed_at IS NULL
         RETURNING state_hash,client_reference_hash",
    ).bind(hash_state(&callback.state)).fetch_optional(&state.db).await
        .map_err(|error| { tracing::error!(%error, "could not consume OAuth state"); StatusCode::INTERNAL_SERVER_ERROR })?;
    let Some(consumed) = consumed else { return Err(StatusCode::BAD_REQUEST); };

    if consumed.client_reference_hash.is_none() {
        return Err(StatusCode::BAD_REQUEST);
    }
    let result = complete_tiktok_authorization(&state, config, &callback).await;
    match result {
        Ok((creator_id, display_name)) => {
            sqlx::query("UPDATE oauth_states SET status='connected',creator_id=$2 WHERE state_hash=$1")
                .bind(&consumed.state_hash).bind(creator_id).execute(&state.db).await
                .map_err(|error| { tracing::error!(%error, "could not finalize OAuth connection"); StatusCode::INTERNAL_SERVER_ERROR })?;
            Ok((StatusCode::OK, Json(json!({
                "connected": true,
                "display_name": display_name,
                "message": "TikTok account connected. You can close this page."
            }))))
        }
        Err(status) => {
            let _ = sqlx::query("UPDATE oauth_states SET status='failed' WHERE state_hash=$1")
                .bind(&consumed.state_hash).execute(&state.db).await;
            Err(status)
        }
    }
}

async fn complete_tiktok_authorization(
    state: &AppState,
    config: &TikTokConfig,
    callback: &OAuthCallback,
) -> Result<(Uuid, Option<String>), StatusCode> {
    if callback.error.is_some() {
        tracing::warn!(error=?callback.error, description=?callback.error_description, "TikTok authorization was declined");
        return Err(StatusCode::BAD_REQUEST);
    }
    let code = callback.code.as_deref().ok_or(StatusCode::BAD_REQUEST)?;
    let token: TikTokTokenResponse = state.http.post(TIKTOK_TOKEN_URL)
        .form(&[
            ("client_key", config.client_key.as_str()),
            ("client_secret", config.client_secret.as_str()),
            ("code", code),
            ("grant_type", "authorization_code"),
            ("redirect_uri", config.redirect_uri.as_str()),
        ]).send().await.map_err(|error| {
            tracing::error!(%error, "TikTok token exchange request failed"); StatusCode::BAD_GATEWAY
        })?.error_for_status().map_err(|error| {
            tracing::error!(%error, "TikTok token exchange was rejected"); StatusCode::BAD_GATEWAY
        })?.json().await.map_err(|error| {
            tracing::error!(%error, "TikTok token response was invalid"); StatusCode::BAD_GATEWAY
        })?;

    if !token.scope.split(',').any(|scope| scope.trim() == "video.list") {
        return Err(StatusCode::FORBIDDEN);
    }
    let user: TikTokUserInfoResponse = state.http.get(TIKTOK_USER_INFO_URL)
        .query(&[("fields", "open_id,display_name")])
        .bearer_auth(&token.access_token).send().await
        .map_err(|error| { tracing::error!(%error, "TikTok user info request failed"); StatusCode::BAD_GATEWAY })?
        .error_for_status().map_err(|error| { tracing::error!(%error, "TikTok user info rejected"); StatusCode::BAD_GATEWAY })?
        .json().await.map_err(|error| { tracing::error!(%error, "TikTok user info response invalid"); StatusCode::BAD_GATEWAY })?;
    if let Some(api_error) = user.error {
        tracing::error!(code=?api_error.code, message=?api_error.message, "TikTok user info failed");
        return Err(StatusCode::BAD_GATEWAY);
    }
    let profile = user.data.and_then(|data| data.user).ok_or(StatusCode::BAD_GATEWAY)?;
    if profile.open_id != token.open_id { return Err(StatusCode::BAD_GATEWAY); }

    let access_enc = encrypt_token(&config.encryption_key, &token.access_token)
        .map_err(|error| { tracing::error!(%error, "token encryption failed"); StatusCode::INTERNAL_SERVER_ERROR })?;
    let refresh_enc = encrypt_token(&config.encryption_key, &token.refresh_token)
        .map_err(|error| { tracing::error!(%error, "token encryption failed"); StatusCode::INTERNAL_SERVER_ERROR })?;
    let username = format!("tt_{}", profile.open_id.replace('-', ""));
    let access_expiry = Utc::now() + ChronoDuration::seconds(token.expires_in.max(60));
    let refresh_expiry = Utc::now() + ChronoDuration::seconds(token.refresh_expires_in.max(60));
    let creator_id = sqlx::query_scalar::<_,Uuid>(
        "INSERT INTO creators
            (id,username,display_name,tiktok_open_id,access_token_enc,refresh_token_enc,
             access_expires_at,refresh_expires_at,enabled,last_polled_at)
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,TRUE,NULL)
         ON CONFLICT (tiktok_open_id) DO UPDATE SET
            username=EXCLUDED.username,display_name=EXCLUDED.display_name,
            access_token_enc=EXCLUDED.access_token_enc,refresh_token_enc=EXCLUDED.refresh_token_enc,
            access_expires_at=EXCLUDED.access_expires_at,refresh_expires_at=EXCLUDED.refresh_expires_at,
            enabled=TRUE,last_polled_at=NULL,next_poll_at=NULL,poll_failures=0,last_error=NULL
         RETURNING id",
    ).bind(Uuid::new_v4()).bind(username).bind(profile.display_name.clone())
        .bind(profile.open_id).bind(access_enc).bind(refresh_enc)
        .bind(access_expiry).bind(refresh_expiry).fetch_one(&state.db).await
        .map_err(|error| { tracing::error!(%error, "could not store authorized TikTok creator"); StatusCode::INTERNAL_SERVER_ERROR })?;
    Ok((creator_id, profile.display_name))
}

async fn get_tiktok_connection(
    State(state): State<AppState>,
    Path(reference): Path<String>,
) -> Result<Json<OAuthConnectionStatus>, StatusCode> {
    if !valid_oauth_reference(&reference) {
        return Err(StatusCode::NOT_FOUND);
    }
    let connection = sqlx::query_as::<_, OAuthConnectionStatus>(
        "SELECT CASE
             WHEN s.status IN ('pending','processing') AND s.expires_at<=NOW() THEN 'expired'
             WHEN s.status='connected' AND (c.id IS NULL OR c.enabled=FALSE) THEN 'reauthorization_required'
             ELSE s.status
         END AS status,
         s.creator_id,c.display_name,s.expires_at
         FROM oauth_states s LEFT JOIN creators c ON c.id=s.creator_id
         WHERE s.client_reference_hash=$1",
    ).bind(hash_state(&reference)).fetch_optional(&state.db).await
        .map_err(|error| { tracing::error!(%error, "could not fetch OAuth connection status"); StatusCode::INTERNAL_SERVER_ERROR })?;
    connection.map(Json).ok_or(StatusCode::NOT_FOUND)
}

async fn ingest_event(
    State(state): State<AppState>,
    Json(input): Json<IngestEvent>,
) -> Result<(StatusCode, Json<IngestResponse>), StatusCode> {
    if input.platform_video_id.trim().is_empty() || input.platform_video_id.len() > 128 || !input.payload.is_object() {
        return Err(StatusCode::UNPROCESSABLE_ENTITY);
    }
    let creator_exists: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM creators WHERE id = $1 AND enabled = TRUE)",
    ).bind(input.creator_id).fetch_one(&state.db).await
        .map_err(|error| { tracing::error!(%error, "could not validate creator"); StatusCode::INTERNAL_SERVER_ERROR })?;
    if !creator_exists { return Err(StatusCode::NOT_FOUND); }

    let mut tx = state.db.begin().await.map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let video_id = Uuid::new_v4();
    let inserted = sqlx::query_scalar::<_, Uuid>(
        "INSERT INTO videos (id, creator_id, platform_video_id, payload, published_at)
         VALUES ($1,$2,$3,$4,$5) ON CONFLICT (creator_id, platform_video_id) DO NOTHING RETURNING id",
    ).bind(video_id).bind(input.creator_id).bind(&input.platform_video_id)
        .bind(&input.payload).bind(input.published_at).fetch_optional(&mut *tx).await
        .map_err(|error| { tracing::error!(%error, "could not persist publication"); StatusCode::INTERNAL_SERVER_ERROR })?;
    let Some(video_id) = inserted else {
        let existing_id: Uuid = sqlx::query_scalar(
            "SELECT id FROM videos WHERE creator_id=$1 AND platform_video_id=$2",
        ).bind(input.creator_id).bind(&input.platform_video_id).fetch_one(&mut *tx).await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
        tx.commit().await.map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
        return Ok((StatusCode::OK, Json(IngestResponse { accepted:true, duplicate:true, video_id:existing_id, delivery_id:None })));
    };
    let delivery_id = Uuid::new_v4();
    sqlx::query("INSERT INTO deliveries (id, video_id) VALUES ($1,$2)")
        .bind(delivery_id).bind(video_id).execute(&mut *tx).await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    tx.commit().await.map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok((StatusCode::ACCEPTED, Json(IngestResponse { accepted:true, duplicate:false, video_id, delivery_id:Some(delivery_id) })))
}

async fn list_events(State(state): State<AppState>) -> Result<Json<Vec<Value>>, StatusCode> {
    let rows = sqlx::query_scalar::<_, Value>(
        "SELECT jsonb_build_object('video_id',v.id,'creator_id',v.creator_id,
         'platform_video_id',v.platform_video_id,'payload',v.payload,'published_at',v.published_at,
         'detected_at',v.detected_at,'delivery',jsonb_build_object('id',d.id,'status',d.status,
         'attempts',d.attempts,'last_error',d.last_error,'delivered_at',d.delivered_at))
         FROM videos v JOIN deliveries d ON d.video_id=v.id ORDER BY v.detected_at DESC LIMIT 100",
    ).fetch_all(&state.db).await
        .map_err(|error| { tracing::error!(%error, "could not list events"); StatusCode::INTERNAL_SERVER_ERROR })?;
    Ok(Json(rows))
}

async fn retry_delivery(State(state): State<AppState>, Path(id): Path<Uuid>) -> Result<StatusCode, StatusCode> {
    let result = sqlx::query(
        "UPDATE deliveries SET status='retry',next_attempt_at=NOW(),last_error=NULL
         WHERE id=$1 AND status IN ('failed','retry')",
    ).bind(id).execute(&state.db).await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    if result.rows_affected()==0 { return Err(StatusCode::NOT_FOUND); }
    Ok(StatusCode::ACCEPTED)
}

async fn claim_delivery(db: &PgPool) -> anyhow::Result<Option<ClaimedDelivery>> {
    sqlx::query("UPDATE deliveries SET status='retry',next_attempt_at=NOW(),processing_started_at=NULL,last_error=COALESCE(last_error,'Recovered stale processing delivery') WHERE status='processing' AND processing_started_at < NOW()-INTERVAL '5 minutes'").execute(db).await?;
    Ok(sqlx::query_as::<_, ClaimedDelivery>(
        "WITH candidate AS (SELECT id FROM deliveries WHERE status IN ('pending','retry')
         AND next_attempt_at<=NOW() ORDER BY next_attempt_at,created_at
         FOR UPDATE SKIP LOCKED LIMIT 1)
         UPDATE deliveries d SET status='processing',attempts=d.attempts+1,processing_started_at=NOW()
         FROM candidate c,videos v WHERE d.id=c.id AND v.id=d.video_id
         RETURNING d.id,d.attempts,v.payload",
    ).fetch_optional(db).await?)
}

async fn dispatch_loop(state: AppState) {
    let Some(webhook_url) = state.webhook_url.clone() else {
        tracing::warn!("WEBHOOK_URL is not configured; outbound delivery worker is disabled");
        return;
    };
    loop {
        match claim_delivery(&state.db).await {
            Ok(Some(delivery)) => {
                let body = match serde_json::to_vec(&delivery.payload) {
                    Ok(body) => body,
                    Err(error) => { tracing::error!(%error, delivery_id=%delivery.id, "invalid event payload"); continue; }
                };
                let mut request = state.http.post(&webhook_url).header("Content-Type","application/json")
                    .header("Idempotency-Key",delivery.id.to_string());
                if let Some(secret)=&state.webhook_secret {
                    let mut mac=<HmacSha256 as Mac>::new_from_slice(secret.as_bytes()).expect("HMAC accepts any key size");
                    mac.update(&body);
                    request=request.header("X-VoxyTok-Signature",format!("sha256={}",hex::encode(mac.finalize().into_bytes())));
                }
                let result=request.body(body).send().await.and_then(|response|response.error_for_status());
                match result {
                    Ok(_) => { let _=sqlx::query("UPDATE deliveries SET status='delivered',delivered_at=NOW(),last_error=NULL,processing_started_at=NULL WHERE id=$1")
                        .bind(delivery.id).execute(&state.db).await; }
                    Err(error) => {
                        let terminal=delivery.attempts>=10;
                        let delay=2_i64.pow(delivery.attempts.min(10) as u32).min(3600);
                        let status=if terminal {"failed"} else {"retry"};
                        let message=error.to_string().chars().take(1000).collect::<String>();
                        let _=sqlx::query("UPDATE deliveries SET status=$2,last_error=$3,next_attempt_at=NOW()+($4*INTERVAL '1 second'),processing_started_at=NULL WHERE id=$1")
                            .bind(delivery.id).bind(status).bind(message).bind(delay).execute(&state.db).await;
                    }
                }
            }
            Ok(None)=>tokio::time::sleep(Duration::from_secs(2)).await,
            Err(error)=>{tracing::error!(%error,"delivery worker database error");tokio::time::sleep(Duration::from_secs(5)).await;}
        }
    }
}

fn is_permanent_tiktok_auth_error(code: &str) -> bool {
    matches!(code, "invalid_grant" | "invalid_token" | "token_expired" | "user_not_authorized")
}

fn is_invalid_access_token_code(code: &str) -> bool {
    matches!(code, "invalid_token" | "token_expired" | "access_token_invalid" | "access_token_expired")
}

fn provider_error_code(payload: &Value) -> Option<&str> {
    match payload.get("error") {
        Some(Value::String(code)) => Some(code.as_str()),
        Some(Value::Object(error)) => error.get("code").and_then(Value::as_str),
        _ => None,
    }
}

fn parse_video_page_response(
    status: StatusCode,
    payload: Value,
) -> anyhow::Result<TikTokVideoListResponse> {
    let code = provider_error_code(&payload).unwrap_or("unknown");
    if !status.is_success() {
        if is_invalid_access_token_code(code) || status == StatusCode::UNAUTHORIZED {
            anyhow::bail!("invalid_access_token: TikTok rejected the access token (HTTP {status}, code {code})");
        }
        anyhow::bail!("TikTok video.list failed with HTTP {status} (provider code: {code})");
    }
    let parsed: TikTokVideoListResponse = serde_json::from_value(payload)?;
    if let Some(api_error) = &parsed.error
        && let Some(code) = api_error.code.as_deref().filter(|code| *code != "ok")
    {
        if is_invalid_access_token_code(code) {
            anyhow::bail!("invalid_access_token: TikTok rejected the access token ({code})");
        }
        anyhow::bail!("TikTok video.list API error ({code}): {}", api_error.message.as_deref().unwrap_or_default());
    }
    Ok(parsed)
}

async fn request_video_page(
    state: &AppState,
    access_token: &str,
    body: &Value,
) -> anyhow::Result<TikTokVideoListResponse> {
    let response = state.http.post(TIKTOK_VIDEO_LIST_URL)
        .query(&[("fields", "id,title,video_description,create_time,duration,cover_image_url,share_url,embed_link")])
        .bearer_auth(access_token)
        .json(body)
        .send()
        .await?;
    let status = response.status();
    let payload: Value = response.json().await?;
    parse_video_page_response(status, payload)
}

fn is_reauthorization_required(error: &anyhow::Error) -> bool {
    error.to_string().starts_with("reauthorization_required:")
}

async fn refresh_creator_token(state: &AppState, creator: &mut PollingCreator, config: &TikTokConfig) -> anyhow::Result<String> {
    let refresh_token=decrypt_token(&config.encryption_key,&creator.refresh_token_enc)?;
    let response=state.http.post(TIKTOK_TOKEN_URL)
        .form(&[
            ("client_key",config.client_key.as_str()),
            ("client_secret",config.client_secret.as_str()),
            ("grant_type","refresh_token"),
            ("refresh_token",refresh_token.as_str()),
        ]).send().await?;
    let status=response.status();
    let body: Value=response.json().await?;
    if !status.is_success() {
        let code=body.get("error").and_then(Value::as_str).unwrap_or("unknown");
        if is_permanent_tiktok_auth_error(code) {
            anyhow::bail!("reauthorization_required: TikTok rejected the refresh token ({code})");
        }
        anyhow::bail!("TikTok token refresh failed with HTTP {status} (provider code: {code})");
    }
    if let Some(code)=body.get("error").and_then(Value::as_str)
        && !code.is_empty() && code!="ok" {
        if is_permanent_tiktok_auth_error(code) {
            anyhow::bail!("reauthorization_required: TikTok rejected the refresh token ({code})");
        }
        anyhow::bail!("TikTok token refresh failed (provider code: {code})");
    }
    let response: TikTokTokenResponse=serde_json::from_value(body)?;
    let access_enc=encrypt_token(&config.encryption_key,&response.access_token)?;
    let refresh_enc=encrypt_token(&config.encryption_key,&response.refresh_token)?;
    let access_expires_at=Utc::now()+ChronoDuration::seconds(response.expires_in.max(60));
    let refresh_expires_at=Utc::now()+ChronoDuration::seconds(response.refresh_expires_in.max(60));
    sqlx::query("UPDATE creators SET access_token_enc=$2,refresh_token_enc=$3,access_expires_at=$4,refresh_expires_at=$5 WHERE id=$1")
        .bind(creator.id).bind(&access_enc).bind(&refresh_enc)
        .bind(access_expires_at).bind(refresh_expires_at)
        .execute(&state.db).await?;
    // Keep the in-memory copy in sync: a second refresh in this same poll
    // must use TikTok's rotated refresh token, never the now-stale one.
    creator.access_token_enc=access_enc;
    creator.refresh_token_enc=refresh_enc;
    creator.access_expires_at=access_expires_at;
    creator.refresh_expires_at=refresh_expires_at;
    Ok(response.access_token)
}

async fn poll_creator(state: &AppState, mut creator: PollingCreator, config: &TikTokConfig) -> anyhow::Result<()> {
    let mut access_token=if creator.access_expires_at<=Utc::now()+ChronoDuration::minutes(10) {
        if creator.refresh_expires_at<=Utc::now() {
            sqlx::query("UPDATE creators SET enabled=FALSE,next_poll_at=NULL,last_error='TikTok refresh token expired; creator must reconnect' WHERE id=$1")
                .bind(creator.id).execute(&state.db).await?;
            anyhow::bail!("reauthorization_required: TikTok refresh token expired");
        }
        refresh_creator_token(state,&mut creator,config).await?
    } else {
        decrypt_token(&config.encryption_key,&creator.access_token_enc)?
    };
    // A provider may invalidate an access token before its advertised expiry.
    // Refresh at most once per synchronization run, then retry the rejected page.
    let mut forced_refresh_used=false;

    let mut inserted_count=0usize;
    let mut cursor: Option<i64>=None;
    let mut pages_fetched=0usize;
    loop {
        pages_fetched+=1;
        let mut body=json!({"max_count":20});
        if let Some(value)=cursor { body["cursor"]=json!(value); }
        let response = match request_video_page(state, &access_token, &body).await {
            Ok(response) => response,
            Err(error) if error.to_string().starts_with("invalid_access_token:") && !forced_refresh_used => {
                if creator.refresh_expires_at<=Utc::now() {
                    anyhow::bail!("reauthorization_required: TikTok refresh token expired");
                }
                access_token=refresh_creator_token(state,&creator,config).await?;
                forced_refresh_used=true;
                request_video_page(state,&access_token,&body).await?
            }
            Err(error) => return Err(error),
        };
        let Some(data)=response.data else { break; };
        let videos=data.videos.unwrap_or_default();
        for video in videos {
        let published_at=video.create_time.and_then(|timestamp|DateTime::from_timestamp(timestamp,0));
        let payload=json!({
            "event":"tiktok.video.published",
            "provider":"tiktok_display_api",
            "creator":{"id":creator.id,"username":creator.username,"display_name":creator.display_name},
            "video":{
                "id":video.id,
                "title":video.title,
                "description":video.video_description,
                "duration":video.duration,
                "cover_image_url":video.cover_image_url,
                "url":video.share_url,
                "embed_url":video.embed_link,
                "published_at":published_at
            }
        });
        let mut tx=state.db.begin().await?;
        let video_id=sqlx::query_scalar::<_,Uuid>(
            "INSERT INTO videos (id,creator_id,platform_video_id,payload,published_at)
             VALUES ($1,$2,$3,$4,$5) ON CONFLICT (creator_id,platform_video_id) DO NOTHING RETURNING id",
        ).bind(Uuid::new_v4()).bind(creator.id).bind(&video.id).bind(&payload).bind(published_at)
            .fetch_optional(&mut *tx).await?;
        if let Some(video_id)=video_id {
            sqlx::query("INSERT INTO deliveries (id,video_id) VALUES ($1,$2)")
                .bind(Uuid::new_v4()).bind(video_id).execute(&mut *tx).await?;
            inserted_count+=1;
        }
        tx.commit().await?;

        }
        if data.has_more != Some(true) || pages_fetched>=20 { break; }
        let Some(next_cursor)=data.cursor else { break; };
        if Some(next_cursor)==cursor { break; }
        cursor=Some(next_cursor);
    }
    sqlx::query("UPDATE creators SET last_polled_at=NOW(),next_poll_at=NOW()+($2*INTERVAL '1 hour'),poll_failures=0,last_error=NULL WHERE id=$1")
        .bind(creator.id).bind(POLL_INTERVAL_HOURS).execute(&state.db).await?;
    tracing::info!(creator=%creator.username,new_videos=inserted_count,"TikTok creator synchronized");
    Ok(())
}

async fn tiktok_poll_loop(state: AppState) {
    let Some(config)=state.tiktok.clone() else {
        tracing::info!("TikTok provider is not configured; acquisition worker is disabled");
        return;
    };
    loop {
        let creators=sqlx::query_as::<_,PollingCreator>(
            "SELECT id,username,display_name,access_token_enc,refresh_token_enc,
             access_expires_at,refresh_expires_at FROM creators
             WHERE enabled=TRUE AND tiktok_open_id IS NOT NULL
             AND ((next_poll_at IS NULL AND (last_polled_at IS NULL OR last_polled_at<=NOW()-($1*INTERVAL '1 hour'))) OR next_poll_at<=NOW())
             ORDER BY COALESCE(next_poll_at,last_polled_at) NULLS FIRST LIMIT 10",
        ).bind(POLL_INTERVAL_HOURS).fetch_all(&state.db).await;
        match creators {
            Ok(creators)=>{
                for creator in creators {
                    if let Err(error)=poll_creator(&state,creator.clone(),&config).await {
                        let message=error.to_string().chars().take(1000).collect::<String>();
                        if is_reauthorization_required(&error) {
                            let persisted=sqlx::query("UPDATE creators SET enabled=FALSE,next_poll_at=NULL,last_error=$2 WHERE id=$1")
                                .bind(creator.id).bind(&message).execute(&state.db).await;
                            if let Err(db_error)=persisted {
                                tracing::error!(%db_error,creator=%creator.username,"could not persist reauthorization requirement");
                            }
                            tracing::warn!(creator=%creator.username,"TikTok authorization is no longer valid; creator must reconnect");
                        } else {
                            let persisted=sqlx::query("UPDATE creators SET poll_failures=poll_failures+1,next_poll_at=NOW()+make_interval(secs => LEAST(300 * (1 << LEAST(poll_failures,4)),3600)),last_error=$2 WHERE id=$1")
                                .bind(creator.id).bind(&message).execute(&state.db).await;
                            if let Err(db_error)=persisted {
                                tracing::error!(%db_error,creator=%creator.username,"could not persist polling backoff");
                            }
                            tracing::error!(%error,creator=%creator.username,"TikTok synchronization failed; retry scheduled");
                        }
                    }
                }
            }
            Err(error)=>tracing::error!(%error,"could not load TikTok creators for polling"),
        }
        tokio::time::sleep(Duration::from_secs(60)).await;
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt().with_env_filter(
        EnvFilter::try_from_default_env().unwrap_or_else(|_|EnvFilter::new("voxytok=info,tower_http=info"))
    ).init();

    let api_token=env::var("API_TOKEN").map_err(|_|anyhow::anyhow!("API_TOKEN must be configured"))?;
    if api_token.len()<32 { anyhow::bail!("API_TOKEN must contain at least 32 characters"); }
    let database_url=env::var("DATABASE_URL").map_err(|_|anyhow::anyhow!("DATABASE_URL must be configured"))?;
    let db=PgPoolOptions::new().max_connections(10).acquire_timeout(Duration::from_secs(5)).connect(&database_url).await?;
    sqlx::raw_sql(include_str!("../migrations/0001_initial.sql")).execute(&db).await?;
    sqlx::raw_sql(include_str!("../migrations/0002_single_instance_outbox.sql")).execute(&db).await?;
    sqlx::raw_sql(include_str!("../migrations/0003_tiktok_oauth.sql")).execute(&db).await?;
    sqlx::raw_sql(include_str!("../migrations/0004_worker_reliability.sql")).execute(&db).await?;
    sqlx::raw_sql(include_str!("../migrations/0005_oauth_connection_tracking.sql")).execute(&db).await?;

    let webhook_url=env::var("WEBHOOK_URL").ok().filter(|value|!value.trim().is_empty());
    if let Some(url)=&webhook_url { validate_webhook_url(url)?; }
    let webhook_secret=env::var("WEBHOOK_SECRET").ok().filter(|value|!value.is_empty());
    if webhook_secret.as_ref().is_some_and(|secret| secret.len() < 32) {
        anyhow::bail!("WEBHOOK_SECRET must contain at least 32 characters");
    }
    let tiktok_values=(
        env::var("TIKTOK_CLIENT_KEY").ok(),
        env::var("TIKTOK_CLIENT_SECRET").ok(),
        env::var("TIKTOK_REDIRECT_URI").ok(),
        env::var("TOKEN_ENCRYPTION_KEY").ok(),
    );
    let tiktok=match tiktok_values {
        (None,None,None,None)=>None,
        (Some(client_key),Some(client_secret),Some(redirect_uri),Some(key_hex))=>{
            if client_key.trim().is_empty() || client_secret.trim().is_empty() || redirect_uri.trim().is_empty() {
                anyhow::bail!("TikTok client key, client secret and redirect URI must not be empty");
            }
            let parsed=Url::parse(&redirect_uri)?;
            if parsed.scheme()!="https" || parsed.host_str().is_none()
                || !parsed.username().is_empty() || parsed.password().is_some() || parsed.fragment().is_some() {
                anyhow::bail!("TIKTOK_REDIRECT_URI must be an HTTPS URL without credentials or a fragment");
            }
            let key_vec=hex::decode(key_hex)?;
            let encryption_key:[u8;32]=key_vec.try_into().map_err(|_|anyhow::anyhow!("TOKEN_ENCRYPTION_KEY must be exactly 64 hexadecimal characters"))?;
            Some(TikTokConfig{client_key,client_secret,redirect_uri,encryption_key})
        }
        _=>anyhow::bail!("Configure all four TikTok variables together: TIKTOK_CLIENT_KEY, TIKTOK_CLIENT_SECRET, TIKTOK_REDIRECT_URI, TOKEN_ENCRYPTION_KEY"),
    };
    // Do not forward signed event bodies or secrets to a redirect target.
    let http=Client::builder()
        .timeout(Duration::from_secs(20))
        .redirect(reqwest::redirect::Policy::none())
        .user_agent("VoxyTok/0.3 (+https://github.com/KuaFeur/VoxyTok)")
        .build()?;
    let state=AppState{db,api_token,webhook_url,webhook_secret,tiktok,http};
    tokio::spawn(dispatch_loop(state.clone()));
    tokio::spawn(tiktok_poll_loop(state.clone()));

    let protected=Router::new()
        .route("/creators",post(create_creator).get(list_creators))
        .route("/creators/{id}",delete(delete_creator))
        .route("/events",post(ingest_event).get(list_events))
        .route("/events/{id}/retry",post(retry_delivery))
        .route("/oauth/tiktok/start",post(start_tiktok_oauth))
        .route("/oauth/tiktok/connections/{reference}",get(get_tiktok_connection))
        .route_layer(middleware::from_fn_with_state(state.clone(),authenticate));
    let app=Router::new()
        .route("/healthz",get(health))
        .route("/readyz",get(readiness))
        .route("/oauth/tiktok/callback",get(tiktok_callback))
        .nest("/api/v1",protected)
        .with_state(state);
    let port=env::var("PORT").unwrap_or_else(|_|"8080".to_string()).parse::<u16>()?;
    let address=SocketAddr::from(([0,0,0,0],port));
    let listener=tokio::net::TcpListener::bind(address).await?;
    tracing::info!(%address,"VoxyTok HTTP server started");
    axum::serve(listener,app).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_encryption_round_trip_and_authentication() {
        let key=[7_u8;32];
        let encrypted=encrypt_token(&key,"example-secret-token").unwrap();
        assert_ne!(encrypted,"example-secret-token");
        assert_eq!(decrypt_token(&key,&encrypted).unwrap(),"example-secret-token");
        assert!(decrypt_token(&[8_u8;32],&encrypted).is_err());
    }


    #[tokio::test]
    async fn postgres_publication_outbox_is_idempotent() {
        let Ok(database_url) = std::env::var("DATABASE_URL") else {
            // Local unit-test runs do not need a database. CI supplies a real
            // PostgreSQL service and therefore executes this integration test.
            return;
        };
        let db = PgPoolOptions::new().max_connections(2).connect(&database_url).await.unwrap();
        sqlx::raw_sql(include_str!("../migrations/0001_initial.sql")).execute(&db).await.unwrap();
        sqlx::raw_sql(include_str!("../migrations/0002_single_instance_outbox.sql")).execute(&db).await.unwrap();
        sqlx::raw_sql(include_str!("../migrations/0003_tiktok_oauth.sql")).execute(&db).await.unwrap();
        sqlx::raw_sql(include_str!("../migrations/0004_worker_reliability.sql")).execute(&db).await.unwrap();
        sqlx::raw_sql(include_str!("../migrations/0005_oauth_connection_tracking.sql")).execute(&db).await.unwrap();

        let creator_id = Uuid::new_v4();
        let username = format!("ci_{}", Uuid::new_v4().simple());
        sqlx::query("INSERT INTO creators (id,username) VALUES ($1,$2)")
            .bind(creator_id).bind(&username).execute(&db).await.unwrap();

        // Exercise the one-time OAuth reference lifecycle against PostgreSQL.
        let oauth_state = Uuid::new_v4().to_string();
        let app_reference = format!("ci_{}", Uuid::new_v4().simple());
        let reference_hash = hash_state(&app_reference);
        sqlx::query("INSERT INTO oauth_states (state_hash,client_reference_hash,expires_at) VALUES ($1,$2,NOW()+INTERVAL '10 minutes')")
            .bind(hash_state(&oauth_state)).bind(&reference_hash).execute(&db).await.unwrap();
        let consumed = sqlx::query_scalar::<_,String>(
            "UPDATE oauth_states SET consumed_at=NOW(),status='processing'
             WHERE state_hash=$1 AND expires_at>NOW() AND consumed_at IS NULL RETURNING state_hash",
        ).bind(hash_state(&oauth_state)).fetch_optional(&db).await.unwrap();
        assert!(consumed.is_some());
        let consumed_again = sqlx::query_scalar::<_,String>(
            "UPDATE oauth_states SET consumed_at=NOW(),status='processing'
             WHERE state_hash=$1 AND expires_at>NOW() AND consumed_at IS NULL RETURNING state_hash",
        ).bind(hash_state(&oauth_state)).fetch_optional(&db).await.unwrap();
        assert!(consumed_again.is_none());
        sqlx::query("UPDATE oauth_states SET status='connected',creator_id=$2 WHERE state_hash=$1")
            .bind(hash_state(&oauth_state)).bind(creator_id).execute(&db).await.unwrap();
        let (connection_status, connected_creator):(String,Option<Uuid>) = sqlx::query_as(
            "SELECT status,creator_id FROM oauth_states WHERE client_reference_hash=$1",
        ).bind(reference_hash).fetch_one(&db).await.unwrap();
        assert_eq!(connection_status,"connected");
        assert_eq!(connected_creator,Some(creator_id));

        let video_id = Uuid::new_v4();
        let payload = json!({"event":"tiktok.video.published","video":{"id":"ci-video"}});
        let first = sqlx::query_scalar::<_,Uuid>(
            "INSERT INTO videos (id,creator_id,platform_video_id,payload)
             VALUES ($1,$2,'ci-video',$3)
             ON CONFLICT (creator_id,platform_video_id) DO NOTHING RETURNING id",
        ).bind(video_id).bind(creator_id).bind(&payload).fetch_optional(&db).await.unwrap();
        assert_eq!(first,Some(video_id));

        let duplicate = sqlx::query_scalar::<_,Uuid>(
            "INSERT INTO videos (id,creator_id,platform_video_id,payload)
             VALUES ($1,$2,'ci-video',$3)
             ON CONFLICT (creator_id,platform_video_id) DO NOTHING RETURNING id",
        ).bind(Uuid::new_v4()).bind(creator_id).bind(&payload).fetch_optional(&db).await.unwrap();
        assert!(duplicate.is_none());

        sqlx::query("INSERT INTO deliveries (id,video_id) VALUES ($1,$2)")
            .bind(Uuid::new_v4()).bind(video_id).execute(&db).await.unwrap();
        let delivery_count:i64=sqlx::query_scalar("SELECT COUNT(*) FROM deliveries WHERE video_id=$1")
            .bind(video_id).fetch_one(&db).await.unwrap();
        assert_eq!(delivery_count,1);
        sqlx::query("DELETE FROM creators WHERE id=$1").bind(creator_id).execute(&db).await.unwrap();
        let deleted_connection_status: String = sqlx::query_scalar(
            "SELECT CASE
                WHEN s.status IN ('pending','processing') AND s.expires_at<=NOW() THEN 'expired'
                WHEN s.status='connected' AND (c.id IS NULL OR c.enabled=FALSE) THEN 'reauthorization_required'
                ELSE s.status
             END
             FROM oauth_states s LEFT JOIN creators c ON c.id=s.creator_id
             WHERE s.client_reference_hash=$1",
        ).bind(hash_state(&app_reference)).fetch_one(&db).await.unwrap();
        assert_eq!(deleted_connection_status,"reauthorization_required");
        db.close().await;
    }

    #[test]
    fn video_page_parser_distinguishes_auth_and_transient_errors() {
        let invalid_nested = json!({
            "data": null,
            "error": {"code": "access_token_invalid", "message": "token rejected"}
        });
        let error = parse_video_page_response(StatusCode::OK, invalid_nested).err().unwrap();
        assert!(error.to_string().starts_with("invalid_access_token:"));

        let unauthorized = parse_video_page_response(
            StatusCode::UNAUTHORIZED,
            json!({"error": {"code": "unknown", "message": "unauthorized"}}),
        ).err().unwrap();
        assert!(unauthorized.to_string().starts_with("invalid_access_token:"));

        let transient = parse_video_page_response(
            StatusCode::SERVICE_UNAVAILABLE,
            json!({"error": {"code": "server_error", "message": "try later"}}),
        ).err().unwrap();
        assert!(!transient.to_string().starts_with("invalid_access_token:"));

        let success = parse_video_page_response(
            StatusCode::OK,
            json!({"data": {"videos": [], "has_more": false}, "error": {"code": "ok"}}),
        ).unwrap();
        assert!(success.data.is_some());
    }

    #[test]
    fn invalid_access_token_errors_trigger_one_refresh_path() {
        assert!(is_invalid_access_token_code("invalid_token"));
        assert!(is_invalid_access_token_code("token_expired"));
        assert!(is_invalid_access_token_code("access_token_invalid"));
        assert!(is_invalid_access_token_code("access_token_expired"));
        assert!(!is_invalid_access_token_code("rate_limit_exceeded"));
        assert!(!is_invalid_access_token_code("server_error"));
    }

    #[test]
    fn permanent_tiktok_auth_errors_require_reauthorization() {
        assert!(is_permanent_tiktok_auth_error("invalid_grant"));
        assert!(is_permanent_tiktok_auth_error("invalid_token"));
        assert!(is_permanent_tiktok_auth_error("token_expired"));
        assert!(is_permanent_tiktok_auth_error("user_not_authorized"));
        assert!(!is_permanent_tiktok_auth_error("server_error"));
        assert!(!is_permanent_tiktok_auth_error("rate_limit_exceeded"));
    }

    #[test]
    fn oauth_reference_validation_rejects_short_or_unsafe_values() {
        assert!(valid_oauth_reference(&"a".repeat(32)));
        assert!(valid_oauth_reference(&"Z".repeat(128)));
        assert!(!valid_oauth_reference("too-short"));
        assert!(!valid_oauth_reference(&"a".repeat(129)));
        assert!(!valid_oauth_reference("unsafe/reference________________"));
    }

    #[test]
    fn webhook_url_requires_tls_except_for_loopback() {
        assert!(validate_webhook_url("https://hooks.example.com/voxytok").is_ok());
        assert!(validate_webhook_url("http://127.0.0.1:3000/voxytok").is_ok());
        assert!(validate_webhook_url("http://hooks.example.com/voxytok").is_err());
        assert!(validate_webhook_url("https://user:pass@hooks.example.com/voxytok").is_err());
        assert!(validate_webhook_url("https://hooks.example.com/voxytok#fragment").is_err());
    }

    #[test]
    fn oauth_state_is_hashed() {
        let state="unguessable-state-value";
        assert_ne!(hash_state(state),state);
        assert_eq!(hash_state(state),hash_state(state));
    }
}
