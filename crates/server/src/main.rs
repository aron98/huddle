#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

mod chat;
mod config;
mod db;
mod engine;
mod http;
mod identity;
mod mailer;
mod meetings;
mod session;
mod state;

use std::sync::Arc;

use anyhow::Context;
use tracing::info;

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(run())
}

async fn run() -> anyhow::Result<()> {
    let config = Arc::new(config::Config::load().context("loading configuration")?);
    info!(bind = %config.bind, "starting huddle");

    tokio::fs::create_dir_all(&config.data_dir)
        .await
        .with_context(|| format!("creating data dir {}", config.data_dir.display()))?;

    let pool = db::connect(&config.data_dir)
        .await
        .context("opening database")?;

    let (engine, media_addr) = engine::spawn(&config).context("starting media engine")?;
    info!(media = %media_addr, "media engine ready");

    let app = http::router(state::AppState::new(config.clone(), pool, engine.clone()));
    let listener = tokio::net::TcpListener::bind(config.bind)
        .await
        .with_context(|| format!("binding {}", config.bind))?;

    info!(url = %config.base_url, "listening");
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("server error")?;
    engine.shutdown();
    Ok(())
}

async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
    info!("shutdown signal received");
}
