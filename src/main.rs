use axum::{
    extract::{Path, State},
    http::{header::AUTHORIZATION, Request, StatusCode},
    middleware::{self, Next},
    response::Response,
    routing::{delete, get, post},
    Json, Router,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{postgres::PgPoolOptions, FromRow, PgPool};
use std::{env, net::SocketAddr, time::Duration};
use subtle::ConstantTimeEq;
use tracing_subscriber::EnvFilter;
use uuid::Uuid;

#[derive(Clone)]
struct AppState {
    db: PgPool,
    api_token: String,
}

#[derive(Serialize)]
struct HealthResponse {
    status: &'static str,
    service: &'static str,
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

async fn health() -> Json<HealthResponse> {
    Json(HealthResponse { status: "ok", service: "voxytok" })
}

async fn readiness(State(state): State<AppState>) -> Result<Json<HealthResponse>, StatusCode> {
    sqlx::query("SELECT 1")
        .execute(&state.db)
        .await
        .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
    Ok(Json(HealthResponse { status: "ready", service: "voxytok" }))
}

async fn authenticate<B>(
    State(state): State<AppState>,
    request: Request<B>,
    next: Next,
) -> Result<Response, StatusCode>
where
    B: Send + 'static,
{
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
         RETURNING id, username, display_name, enabled, created_at"
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
         FROM creators ORDER BY created_at DESC"
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

    sqlx::query(
        "CREATE TABLE IF NOT EXISTS creators (
            id UUID PRIMARY KEY,
            username TEXT NOT NULL UNIQUE,
            display_name TEXT,
            enabled BOOLEAN NOT NULL DEFAULT TRUE,
            created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
        )"
    )
    .execute(&db)
    .await?;

    let state = AppState { db, api_token };
    let protected = Router::new()
        .route("/creators", post(create_creator).get(list_creators))
        .route("/creators/{id}", delete(delete_creator))
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
