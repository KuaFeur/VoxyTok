use axum::{routing::get, Json, Router};
use serde::Serialize;
use std::{env, net::SocketAddr};
use tracing_subscriber::EnvFilter;

#[derive(Serialize)]
struct HealthResponse {
    status: &'static str,
    service: &'static str,
}

async fn health() -> Json<HealthResponse> {
    Json(HealthResponse {
        status: "ok",
        service: "voxytok",
    })
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("voxytok=info,tower_http=info")),
        )
        .init();

    let port = env::var("PORT")
        .unwrap_or_else(|_| "8080".to_string())
        .parse::<u16>()?;
    let address = SocketAddr::from(([0, 0, 0, 0], port));

    let app = Router::new()
        .route("/healthz", get(health))
        .route("/readyz", get(health));

    let listener = tokio::net::TcpListener::bind(address).await?;
    tracing::info!(%address, "VoxyTok HTTP server started");
    axum::serve(listener, app).await?;

    Ok(())
}
