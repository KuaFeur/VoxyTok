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
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::Sha256;
use sqlx::{postgres::PgPoolOptions, FromRow, PgPool};
use std::{env, net::SocketAddr, time::Duration};
use subtle::ConstantTimeEq;
use tracing_subscriber::EnvFilter;
use uuid::Uuid;

type HmacSha256 = Hmac<Sha256>;

#[derive(Clone)]
struct AppState {
    db: PgPool,
    api_token: String,
    webhook_url: Option<String>,
    webhook_secret: Option<String>,
    http: Client,
}

#[derive(Serialize)]
struct HealthResponse {
    status: &'static str,
    service: &'static str,
    webhook_configured: bool,
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

async fn health(State(state): State<AppState>) -> Json<HealthResponse> {
    Json(HealthResponse {
        status: "ok",
        service: "voxytok",
        webhook_configured: state.webhook_url.is_some(),
    })
}

async fn readiness(State(state): State<AppState>) -> Result<Json<HealthResponse>, StatusCode> {
    sqlx::query("SELECT 1")
        .execute(&state.db)
        .await
        .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
    Ok(Json(HealthResponse {
        status: "ready",
        service: "voxytok",
        webhook_configured: state.webhook_url.is_some(),
    }))
}

async fn authenticate(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    let supplied = request
        .headers()
        .get(AUTHORIZATION)
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
    if username.is_empty()
        || username.len() > 24
        || !username.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'.' || c == b'_')
    {
        return Err(StatusCode::UNPROCESSABLE_ENTITY);
    }

    let creator = sqlx::query_as::<_, Creator>(
        "INSERT INTO creators (id, username) VALUES ($1, $2)
         ON CONFLICT (username) DO UPDATE SET username = EXCLUDED.username
         RETURNING id, username, display_name, enabled, created_at",
    )
    .bind(Uuid::new_v4())
    .bind(username)
    .fetch_one(&state.db)
    .await
    .map_err(|error| {
        tracing::error!(%error, "could not create creator");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    Ok((StatusCode::CREATED, Json(creator)))
}

async fn list_creators(State(state): State<AppState>) -> Result<Json<Vec<Creator>>, StatusCode> {
    let creators = sqlx::query_as::<_, Creator>(
        "SELECT id, username, display_name, enabled, created_at
         FROM creators ORDER BY created_at DESC",
    )
    .fetch_all(&state.db)
    .await
    .map_err(|error| {
        tracing::error!(%error, "could not list creators");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    Ok(Json(creators))
}

async fn delete_creator(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, StatusCode> {
    let result = sqlx::query("DELETE FROM creators WHERE id = $1")
        .bind(id)
        .execute(&state.db)
        .await
        .map_err(|error| {
            tracing::error!(%error, "could not delete creator");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
    if result.rows_affected() == 0 {
        return Err(StatusCode::NOT_FOUND);
    }
    Ok(StatusCode::NO_CONTENT)
}

async fn ingest_event(
    State(state): State<AppState>,
    Json(input): Json<IngestEvent>,
) -> Result<(StatusCode, Json<IngestResponse>), StatusCode> {
    if input.platform_video_id.trim().is_empty() || input.platform_video_id.len() > 128 {
        return Err(StatusCode::UNPROCESSABLE_ENTITY);
    }
    if !input.payload.is_object() {
        return Err(StatusCode::UNPROCESSABLE_ENTITY);
    }

    let creator_exists: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM creators WHERE id = $1 AND enabled = TRUE)",
    )
    .bind(input.creator_id)
    .fetch_one(&state.db)
    .await
    .map_err(|error| {
        tracing::error!(%error, "could not validate creator");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    if !creator_exists {
        return Err(StatusCode::NOT_FOUND);
    }

    let video_id = Uuid::new_v4();
    let inserted = sqlx::query_scalar::<_, Uuid>(
        "INSERT INTO videos (id, creator_id, platform_video_id, payload, published_at)
         VALUES ($1, $2, $3, $4, $5)
         ON CONFLICT (creator_id, platform_video_id) DO NOTHING
         RETURNING id",
    )
    .bind(video_id)
    .bind(input.creator_id)
    .bind(&input.platform_video_id)
    .bind(&input.payload)
    .bind(input.published_at)
    .fetch_optional(&state.db)
    .await
    .map_err(|error| {
        tracing::error!(%error, "could not persist publication");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    let Some(video_id) = inserted else {
        let existing_id: Uuid = sqlx::query_scalar(
            "SELECT id FROM videos WHERE creator_id = $1 AND platform_video_id = $2",
        )
        .bind(input.creator_id)
        .bind(&input.platform_video_id)
        .fetch_one(&state.db)
        .await
        .map_err(|error| {
            tracing::error!(%error, "could not retrieve duplicate publication");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
        return Ok((
            StatusCode::OK,
            Json(IngestResponse {
                accepted: true,
                duplicate: true,
                video_id: existing_id,
                delivery_id: None,
            }),
        ));
    };

    let delivery_id = Uuid::new_v4();
    sqlx::query("INSERT INTO deliveries (id, video_id) VALUES ($1, $2)")
        .bind(delivery_id)
        .bind(video_id)
        .execute(&state.db)
        .await
        .map_err(|error| {
            tracing::error!(%error, "could not enqueue publication delivery");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    Ok((
        StatusCode::ACCEPTED,
        Json(IngestResponse {
            accepted: true,
            duplicate: false,
            video_id,
            delivery_id: Some(delivery_id),
        }),
    ))
}

async fn list_events(State(state): State<AppState>) -> Result<Json<Vec<Value>>, StatusCode> {
    let rows = sqlx::query_scalar::<_, Value>(
        "SELECT jsonb_build_object(
            'video_id', v.id,
            'creator_id', v.creator_id,
            'platform_video_id', v.platform_video_id,
            'payload', v.payload,
            'published_at', v.published_at,
            'detected_at', v.detected_at,
            'delivery', jsonb_build_object(
                'id', d.id, 'status', d.status, 'attempts', d.attempts,
                'last_error', d.last_error, 'delivered_at', d.delivered_at
            )
         )
         FROM videos v JOIN deliveries d ON d.video_id = v.id
         ORDER BY v.detected_at DESC LIMIT 100",
    )
    .fetch_all(&state.db)
    .await
    .map_err(|error| {
        tracing::error!(%error, "could not list events");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    Ok(Json(rows))
}

async fn retry_delivery(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, StatusCode> {
    let result = sqlx::query(
        "UPDATE deliveries
         SET status = 'retry', next_attempt_at = NOW(), last_error = NULL
         WHERE id = $1 AND status IN ('failed', 'retry')",
    )
    .bind(id)
    .execute(&state.db)
    .await
    .map_err(|error| {
        tracing::error!(%error, "could not schedule delivery retry");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    if result.rows_affected() == 0 {
        return Err(StatusCode::NOT_FOUND);
    }
    Ok(StatusCode::ACCEPTED)
}

async fn claim_delivery(db: &PgPool) -> anyhow::Result<Option<ClaimedDelivery>> {
    let delivery = sqlx::query_as::<_, ClaimedDelivery>(
        "WITH candidate AS (
            SELECT id FROM deliveries
            WHERE status IN ('pending', 'retry') AND next_attempt_at <= NOW()
            ORDER BY next_attempt_at, created_at
            FOR UPDATE SKIP LOCKED LIMIT 1
         )
         UPDATE deliveries d
         SET status = 'processing', attempts = d.attempts + 1
         FROM candidate c, videos v
         WHERE d.id = c.id AND v.id = d.video_id
         RETURNING d.id, d.attempts, v.payload",
    )
    .fetch_optional(db)
    .await?;
    Ok(delivery)
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
                    Err(error) => {
                        tracing::error!(%error, delivery_id = %delivery.id, "invalid event payload");
                        continue;
                    }
                };

                let mut request = state.http
                    .post(&webhook_url)
                    .header("Content-Type", "application/json")
                    .header("Idempotency-Key", delivery.id.to_string());

                if let Some(secret) = &state.webhook_secret {
                    let mut mac = HmacSha256::new_from_slice(secret.as_bytes())
                        .expect("HMAC accepts keys of any size");
                    mac.update(&body);
                    request = request.header(
                        "X-VoxyTok-Signature",
                        format!("sha256={}", hex::encode(mac.finalize().into_bytes())),
                    );
                }

                let result = request.body(body).send().await
                    .and_then(|response| response.error_for_status());

                match result {
                    Ok(_) => {
                        if let Err(error) = sqlx::query(
                            "UPDATE deliveries SET status = 'delivered', delivered_at = NOW(), last_error = NULL WHERE id = $1",
                        )
                        .bind(delivery.id)
                        .execute(&state.db)
                        .await {
                            tracing::error!(%error, delivery_id = %delivery.id, "could not mark delivery complete");
                        }
                    }
                    Err(error) => {
                        let terminal = delivery.attempts >= 10;
                        let delay = 2_i64.pow(delivery.attempts.min(10) as u32).min(3600);
                        let status = if terminal { "failed" } else { "retry" };
                        let message = error.to_string().chars().take(1000).collect::<String>();
                        if let Err(db_error) = sqlx::query(
                            "UPDATE deliveries
                             SET status = $2, last_error = $3,
                                 next_attempt_at = NOW() + ($4 * INTERVAL '1 second')
                             WHERE id = $1",
                        )
                        .bind(delivery.id)
                        .bind(status)
                        .bind(message)
                        .bind(delay)
                        .execute(&state.db)
                        .await {
                            tracing::error!(%db_error, delivery_id = %delivery.id, "could not schedule delivery retry");
                        }
                    }
                }
            }
            Ok(None) => tokio::time::sleep(Duration::from_secs(2)).await,
            Err(error) => {
                tracing::error!(%error, "delivery worker database error");
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        }
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("voxytok=info,tower_http=info")),
        )
        .init();

    let api_token = env::var("API_TOKEN")
        .map_err(|_| anyhow::anyhow!("API_TOKEN must be configured"))?;
    if api_token.len() < 32 {
        anyhow::bail!("API_TOKEN must contain at least 32 characters");
    }

    let database_url = env::var("DATABASE_URL")
        .map_err(|_| anyhow::anyhow!("DATABASE_URL must be configured"))?;
    let db = PgPoolOptions::new()
        .max_connections(10)
        .acquire_timeout(Duration::from_secs(5))
        .connect(&database_url)
        .await?;

    sqlx::raw_sql(include_str!("../migrations/0001_initial.sql"))
        .execute(&db)
        .await?;

    let webhook_url = env::var("WEBHOOK_URL").ok().filter(|value| !value.trim().is_empty());
    if let Some(url) = &webhook_url {
        let parsed = reqwest::Url::parse(url)?;
        if !matches!(parsed.scheme(), "http" | "https") {
            anyhow::bail!("WEBHOOK_URL must use http or https");
        }
    }
    let webhook_secret = env::var("WEBHOOK_SECRET").ok().filter(|value| !value.is_empty());
    let http = Client::builder().timeout(Duration::from_secs(15)).build()?;

    let state = AppState { db, api_token, webhook_url, webhook_secret, http };
    tokio::spawn(dispatch_loop(state.clone()));

    let protected = Router::new()
        .route("/creators", post(create_creator).get(list_creators))
        .route("/creators/{id}", delete(delete_creator))
        .route("/events", post(ingest_event).get(list_events))
        .route("/events/{id}/retry", post(retry_delivery))
        .route_layer(middleware::from_fn_with_state(state.clone(), authenticate));

    let app = Router::new()
        .route("/healthz", get(health))
        .route("/readyz", get(readiness))
        .nest("/api/v1", protected)
        .with_state(state);

    let port = env::var("PORT").unwrap_or_else(|_| "8080".to_string()).parse::<u16>()?;
    let address = SocketAddr::from(([0, 0, 0, 0], port));
    let listener = tokio::net::TcpListener::bind(address).await?;
    tracing::info!(%address, "VoxyTok HTTP server started");
    axum::serve(listener, app).await?;
    Ok(())
}
