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
    init_tracing();

    let args = CliArgs::parse();
    let state = AppState::new(args.clone())
        .await
        .context("failed to initialize MinerU Rust API state")?;
    let app = server::routes::build_router(state);
    let bind_addr = format!("{}:{}", args.host, args.port);
    let listener = TcpListener::bind(&bind_addr)
        .await
        .with_context(|| format!("failed to bind {}", bind_addr))?;

    println!(
        "Start MinerU FastAPI Service: http://{}:{}",
        args.host, args.port
    );
    println!("API documentation: http://{}:{}/docs", args.host, args.port);

    axum::serve(listener, app)
        .await
        .context("MinerU API server stopped unexpectedly")
}

fn init_tracing() {
    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| "mineru=info,tower_http=info".into());
    tracing_subscriber::registry()
        .with(env_filter)
        .with(tracing_subscriber::fmt::layer())
        .init();
}
