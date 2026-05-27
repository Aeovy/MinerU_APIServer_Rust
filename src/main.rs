mod config;
mod domain;
mod error;
mod io;
mod server;
mod vlm;

use anyhow::Context;
use clap::Parser;
use tokio::net::TcpListener;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

use crate::config::CliArgs;
use crate::server::state::AppState;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    load_dotenv();
    init_tracing();

    let args = CliArgs::parse();
    let state = AppState::new(args.clone())
        .await
        .context("failed to initialize MinerU Rust API state")?;
    let config = state.config().clone();
    let app = server::routes::build_router(state);
    let bind_addr = format!("{}:{}", args.host, args.port);
    let listener = TcpListener::bind(&bind_addr)
        .await
        .with_context(|| format!("failed to bind {}", bind_addr))?;
    let local_addr = listener
        .local_addr()
        .context("failed to read listener address")?;

    let base_url = format!("http://{}:{}", args.host, local_addr.port());
    tracing::info!(
        bind_addr = %bind_addr,
        local_addr = %local_addr,
        output_root = %config.output_root.display(),
        max_concurrent_requests = config.max_concurrent_requests,
        max_in_flight_tasks = config.max_in_flight_tasks,
        max_upload_size_bytes = config.max_upload_size_bytes,
        processing_window_size = config.processing_window_size,
        vlm_max_concurrency = config.vlm_max_concurrency,
        image_preprocess_threads = config.image_preprocess_threads,
        public_bind_exposed = config.public_bind_exposed,
        allow_public_http_client = config.allow_public_http_client,
        "MinerU API server started"
    );
    tracing::info!(url = %base_url, "Start MinerU FastAPI Service");
    tracing::info!(url = %format!("{base_url}/docs"), "API documentation");
    tracing::info!(url = %format!("{base_url}/openapi.json"), "OpenAPI schema");
    tracing::info!(url = %format!("{base_url}/health"), "Health check");

    axum::serve(listener, app)
        .await
        .context("MinerU API server stopped unexpectedly")
}

/// Load local `.env` before tracing and config read environment variables.
///
/// Existing process environment values keep priority over `.env` values.
fn load_dotenv() {
    let _ = dotenvy::dotenv();
}

fn init_tracing() {
    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| "mineru=info,tower_http=info".into());
    tracing_subscriber::registry()
        .with(env_filter)
        .with(tracing_subscriber::fmt::layer())
        .init();
}
