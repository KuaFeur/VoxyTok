use axum::{
    extract::{Path, Request, State},
    http::{header::AUTHORIZATION, StatusCode},
    middleware::{self, Next},
    response::Response,
    routing::{delete, get, post},
    Json, Router,
};
use chrono::{DateTime, Utc};
use hmac::{Hmac, Mac};
use reqwest::{Client, Url};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::Sha256;
use sqlx::{postgres::PgPoolOptions, FromRow, PgPool};
use std::{env, net::SocketAddr, time::Duration};
use subtle::ConstantTimeEq;
use tracing_subscriber::EnvFilter;
use uuid::Uuid;

mod sources;
mod public_monitor;

type HmacSha256 = Hmac<Sha256>;
#[derive(Clone)]
struct AppState {
    db: PgPool,
    api_token: String,
    webhook_url: Option<String>,
    webhook_secret: Option<String>,
    public_source_count: usize,
    http: Client,
}

#[derive(Serialize)]
struct HealthResponse {
    status: &'static str,
    service: &'static str,
    webhook_configured: bool,
    public_monitor_configured: bool,
    public_source_count: usize,
}

#[derive(Debug, Serialize, FromRow)]
struct Creator {
    id: Uuid,
    username: String,
    display_name: Option<String>,
    enabled: bool,
    created_at: DateTime<Utc>,
    last_successful_poll_at: Option<DateTime<Utc>>,
    next_poll_at: Option<DateTime<Utc>>,
    poll_failures: i32,
    last_error: Option<String>,
}

#[derive(Deserialize)]
struct CreateCreator {
    username: String,
}

#[derive(Serialize, FromRow)]
struct CreatorSourceState {
    source_key: String,
    status: String,
    last_attempt_at: Option<DateTime<Utc>>,
    last_success_at: Option<DateTime<Utc>>,
    next_attempt_at: Option<DateTime<Utc>>,
    consecutive_failures: i32,
    total_failures: i64,
    total_successes: i64,
    last_latency_ms: Option<i64>,
    last_http_status: Option<i32>,
    last_error_code: Option<String>,
    last_error: Option<String>,
    updated_at: DateTime<Utc>,
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

async fn health(State(state): State<AppState>) -> Json<HealthResponse> {
    Json(HealthResponse {
        status: "ok",
        service: "voxytok",
        webhook_configured: state.webhook_url.is_some(),
        public_monitor_configured: state.public_source_count > 0,
        public_source_count: state.public_source_count,
    })
}

async fn readiness(State(state): State<AppState>) -> Result<Json<HealthResponse>, StatusCode> {
    sqlx::query("SELECT 1").execute(&state.db).await
        .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
    Ok(Json(HealthResponse {
        status: "ready",
        service: "voxytok",
        webhook_configured: state.webhook_url.is_some(),
        public_monitor_configured: state.public_source_count > 0,
        public_source_count: state.public_source_count,
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
        "INSERT INTO creators (id, username, monitoring_started_at, next_poll_at) VALUES ($1, $2, NOW(), NOW())
         ON CONFLICT (username) DO UPDATE SET
            enabled = TRUE,
            monitoring_started_at = COALESCE(creators.monitoring_started_at,NOW()),
            next_poll_at = LEAST(COALESCE(creators.next_poll_at,NOW()),NOW())
         RETURNING id, username, display_name, enabled, created_at,
                   last_successful_poll_at, next_poll_at, poll_failures, last_error",
    )
    .bind(Uuid::new_v4()).bind(username).fetch_one(&state.db).await
    .map_err(|error| { tracing::error!(%error, "could not create creator"); StatusCode::INTERNAL_SERVER_ERROR })?;
    Ok((StatusCode::CREATED, Json(creator)))
}

async fn list_creators(State(state): State<AppState>) -> Result<Json<Vec<Creator>>, StatusCode> {
    let creators = sqlx::query_as::<_, Creator>(
        "SELECT id, username, display_name, enabled, created_at,
                last_successful_poll_at, next_poll_at, poll_failures, last_error
         FROM creators ORDER BY created_at DESC",
    ).fetch_all(&state.db).await
        .map_err(|error| { tracing::error!(%error, "could not list creators"); StatusCode::INTERNAL_SERVER_ERROR })?;
    Ok(Json(creators))
}

async fn list_creator_sources(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Json<Vec<CreatorSourceState>>, StatusCode> {
    let exists: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM creators WHERE id=$1)",
    ).bind(id).fetch_one(&state.db).await
        .map_err(|error| { tracing::error!(%error, "could not inspect creator before listing source status"); StatusCode::INTERNAL_SERVER_ERROR })?;
    if !exists { return Err(StatusCode::NOT_FOUND); }
    let rows = sqlx::query_as::<_, CreatorSourceState>(
        "SELECT source_key,status,last_attempt_at,last_success_at,next_attempt_at,
         consecutive_failures,total_failures,total_successes,last_latency_ms,
         last_http_status,last_error_code,last_error,updated_at
         FROM creator_source_states WHERE creator_id=$1 ORDER BY source_key",
    ).bind(id).fetch_all(&state.db).await
        .map_err(|error| { tracing::error!(%error, "could not list creator source status"); StatusCode::INTERNAL_SERVER_ERROR })?;
    Ok(Json(rows))
}

async fn delete_creator(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, StatusCode> {
    let result = sqlx::query("DELETE FROM creators WHERE id = $1")
        .bind(id).execute(&state.db).await
        .map_err(|error| { tracing::error!(%error, "could not delete creator"); StatusCode::INTERNAL_SERVER_ERROR })?;
    if result.rows_affected() == 0 { return Err(StatusCode::NOT_FOUND); }
    Ok(StatusCode::NO_CONTENT)
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
        "UPDATE deliveries SET status='retry',next_attempt_at=NOW(),attempts=0,
         last_error=NULL,processing_started_at=NULL
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

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt().with_env_filter(
        EnvFilter::try_from_default_env().unwrap_or_else(|_|EnvFilter::new("voxytok=info,tower_http=info"))
    ).init();

    let api_token=env::var("API_TOKEN").map_err(|_|anyhow::anyhow!("API_TOKEN must be configured"))?;
    if api_token.len()<32 { anyhow::bail!("API_TOKEN must contain at least 32 characters"); }
    let database_url=env::var("DATABASE_URL").map_err(|_|anyhow::anyhow!("DATABASE_URL must be configured"))?;
    let db=PgPoolOptions::new().max_connections(10).acquire_timeout(Duration::from_secs(5)).connect(&database_url).await?;
    // Hold a session-level advisory lock on one connection for the complete
    // migration sequence. This serializes concurrent replicas at first boot.
    let mut migration_connection = db.acquire().await?;
    const MIGRATION_LOCK_ID: i64 = 7_604_202_611;
    sqlx::query("SELECT pg_advisory_lock($1)")
        .bind(MIGRATION_LOCK_ID)
        .fetch_one(&mut *migration_connection).await?;
    let migration_result: anyhow::Result<()> = async {
        sqlx::raw_sql(include_str!("../migrations/0001_initial.sql")).execute(&mut *migration_connection).await?;
        sqlx::raw_sql(include_str!("../migrations/0002_single_instance_outbox.sql")).execute(&mut *migration_connection).await?;
        sqlx::raw_sql(include_str!("../migrations/0004_worker_reliability.sql")).execute(&mut *migration_connection).await?;
        sqlx::raw_sql(include_str!("../migrations/0006_public_profile_sources.sql")).execute(&mut *migration_connection).await?;
        sqlx::raw_sql(include_str!("../migrations/0007_shared_source_cooldowns.sql")).execute(&mut *migration_connection).await?;
        sqlx::raw_sql(include_str!("../migrations/0008_remove_tiktok_oauth.sql")).execute(&mut *migration_connection).await?;
        Ok(())
    }.await;
    let unlock_result = sqlx::query("SELECT pg_advisory_unlock($1)")
        .bind(MIGRATION_LOCK_ID)
        .fetch_one(&mut *migration_connection).await;
    migration_result?;
    unlock_result?;

    let webhook_url=env::var("WEBHOOK_URL").ok().filter(|value|!value.trim().is_empty());
    if let Some(url)=&webhook_url { validate_webhook_url(url)?; }
    let webhook_secret=env::var("WEBHOOK_SECRET").ok().filter(|value|!value.is_empty());
    if webhook_secret.as_ref().is_some_and(|secret| secret.len() < 32) {
        anyhow::bail!("WEBHOOK_SECRET must contain at least 32 characters");
    }
    // Do not forward signed event bodies or secrets to a redirect target.
    let http=Client::builder()
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(15))
        .pool_idle_timeout(Duration::from_secs(90))
        .tcp_keepalive(Duration::from_secs(30))
        .redirect(reqwest::redirect::Policy::none())
        .user_agent("VoxyTok/1.0.0-rc.2 (+https://github.com/KuaFeur/VoxyTok)")
        .build()?;
    let mut public_sources: Vec<sources::SourceAdapter> = Vec::new();
    public_sources.push(sources::SourceAdapter::TikTokPublicProfile(sources::TikTokPublicProfileSource::new(http.clone())));
    let rss_bases = env::var("RSSHUB_BASE_URLS")
        .unwrap_or_else(|_| "https://rsshub.app,https://rsshub.rssforever.com,https://rsshub.joshrandall.net".to_owned());
    for raw_base in rss_bases.split(',').map(str::trim).filter(|value| !value.is_empty()) {
        let base = Url::parse(raw_base)?;
        if base.scheme() != "https" || base.host_str().is_none() {
            anyhow::bail!("RSSHUB_BASE_URLS entries must be HTTPS URLs");
        }
        public_sources.push(sources::SourceAdapter::RssHub(sources::RssProfileSource::new(http.clone(), base)));
    }
    let poll_interval_seconds = env::var("PUBLIC_POLL_INTERVAL_SECONDS")
        .unwrap_or_else(|_| "300".to_owned()).parse::<u64>()?.clamp(60, 3600);
    let public_source_count = public_sources.len();
    let public_db = db.clone();
    let public_poll_interval = Duration::from_secs(poll_interval_seconds);

    let state=AppState{
        db,api_token,webhook_url,webhook_secret,public_source_count,http,
    };
    tokio::spawn(dispatch_loop(state.clone()));
    tokio::spawn(public_monitor::run(public_db, public_sources, public_poll_interval));

    let protected=Router::new()
        .route("/creators",post(create_creator).get(list_creators))
        .route("/creators/{id}",delete(delete_creator))
        .route("/creators/{id}/sources",get(list_creator_sources))
        .route("/events",post(ingest_event).get(list_events))
        .route("/events/{id}/retry",post(retry_delivery))
        .route_layer(middleware::from_fn_with_state(state.clone(),authenticate));
    let app=Router::new()
        .route("/healthz",get(health))
        .route("/readyz",get(readiness))
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
    use serde_json::json;

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
        sqlx::raw_sql(include_str!("../migrations/0004_worker_reliability.sql")).execute(&db).await.unwrap();
        sqlx::raw_sql(include_str!("../migrations/0006_public_profile_sources.sql")).execute(&db).await.unwrap();
        sqlx::raw_sql(include_str!("../migrations/0007_shared_source_cooldowns.sql")).execute(&db).await.unwrap();
        // Simulate an RC1 database before applying the RC2 cleanup migration.
        sqlx::raw_sql("ALTER TABLE creators ADD COLUMN IF NOT EXISTS tiktok_open_id TEXT UNIQUE, ADD COLUMN IF NOT EXISTS access_token_enc TEXT, ADD COLUMN IF NOT EXISTS refresh_token_enc TEXT, ADD COLUMN IF NOT EXISTS access_expires_at TIMESTAMPTZ, ADD COLUMN IF NOT EXISTS refresh_expires_at TIMESTAMPTZ; CREATE TABLE IF NOT EXISTS oauth_states (state_hash TEXT PRIMARY KEY, expires_at TIMESTAMPTZ NOT NULL); CREATE INDEX creators_due_poll_idx ON creators (next_poll_at,last_polled_at) WHERE enabled=TRUE AND tiktok_open_id IS NOT NULL;")
            .execute(&db).await.unwrap();
        // Re-run the updated worker migration as an RC1 upgrade would: it must
        // replace the old partial index before the OAuth identity column drops.
        sqlx::raw_sql(include_str!("../migrations/0004_worker_reliability.sql")).execute(&db).await.unwrap();
        sqlx::raw_sql(include_str!("../migrations/0008_remove_tiktok_oauth.sql")).execute(&db).await.unwrap();
        let oauth_table_exists: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM information_schema.tables WHERE table_schema=current_schema() AND table_name='oauth_states')"
        ).fetch_one(&db).await.unwrap();
        assert!(!oauth_table_exists);
        let oauth_column_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM information_schema.columns WHERE table_schema=current_schema() AND table_name='creators' AND column_name IN ('tiktok_open_id','access_token_enc','refresh_token_enc','access_expires_at','refresh_expires_at')"
        ).fetch_one(&db).await.unwrap();
        assert_eq!(oauth_column_count, 0);

        let creator_id = Uuid::new_v4();
        let username = format!("ci_{}", Uuid::new_v4().simple());
        sqlx::query("INSERT INTO creators (id,username) VALUES ($1,$2)")
            .bind(creator_id).bind(&username).execute(&db).await.unwrap();

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

        let delivery_id = Uuid::new_v4();
        sqlx::query("INSERT INTO deliveries (id,video_id) VALUES ($1,$2)")
            .bind(delivery_id).bind(video_id).execute(&db).await.unwrap();
        let delivery_count:i64=sqlx::query_scalar("SELECT COUNT(*) FROM deliveries WHERE video_id=$1")
            .bind(video_id).fetch_one(&db).await.unwrap();
        assert_eq!(delivery_count,1);

        // A process crash after claiming a webhook must not strand it forever.
        sqlx::query(
            "UPDATE deliveries SET status='processing',
             processing_started_at=NOW()-INTERVAL '6 minutes' WHERE id=$1",
        ).bind(delivery_id).execute(&db).await.unwrap();
        let recovered = claim_delivery(&db).await.unwrap().unwrap();
        assert_eq!(recovered.id, delivery_id);
        assert_eq!(recovered.attempts, 1);
        let (delivery_status, delivery_attempts):(String,i32) = sqlx::query_as(
            "SELECT status,attempts FROM deliveries WHERE id=$1",
        ).bind(delivery_id).fetch_one(&db).await.unwrap();
        assert_eq!(delivery_status, "processing");
        assert_eq!(delivery_attempts, 1);

        // A manual retry starts a fresh attempt budget after terminal failure.
        sqlx::query(
            "UPDATE deliveries SET status='failed',attempts=10,processing_started_at=NULL WHERE id=$1",
        ).bind(delivery_id).execute(&db).await.unwrap();
        let retried = sqlx::query(
            "UPDATE deliveries SET status='retry',next_attempt_at=NOW(),attempts=0,
             last_error=NULL,processing_started_at=NULL
             WHERE id=$1 AND status IN ('failed','retry')",
        ).bind(delivery_id).execute(&db).await.unwrap();
        assert_eq!(retried.rows_affected(), 1);
        let retry_attempts: i32 = sqlx::query_scalar(
            "SELECT attempts FROM deliveries WHERE id=$1",
        ).bind(delivery_id).fetch_one(&db).await.unwrap();
        assert_eq!(retry_attempts, 0);
        let manually_reclaimed = claim_delivery(&db).await.unwrap().unwrap();
        assert_eq!(manually_reclaimed.id, delivery_id);
        assert_eq!(manually_reclaimed.attempts, 1);
        sqlx::query("DELETE FROM creators WHERE id=$1").bind(creator_id).execute(&db).await.unwrap();
        db.close().await;
    }

    #[test]
    fn webhook_url_requires_tls_except_for_loopback() {
        assert!(validate_webhook_url("https://hooks.example.com/voxytok").is_ok());
        assert!(validate_webhook_url("http://127.0.0.1:3000/voxytok").is_ok());
        assert!(validate_webhook_url("http://hooks.example.com/voxytok").is_err());
        assert!(validate_webhook_url("https://user:pass@hooks.example.com/voxytok").is_err());
        assert!(validate_webhook_url("https://hooks.example.com/voxytok#fragment").is_err());
    }


}
