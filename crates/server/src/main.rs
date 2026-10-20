#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

mod auth;
mod chat;
mod config;
mod db;
mod engine;
mod http;
mod ics;
mod identity;
mod mailer;
mod meetings;
mod metrics;
#[cfg(test)]
mod oidc_test_idp;
mod ratelimit;
mod recording;
mod recurrence;
mod session;
mod state;
mod tls;

use std::sync::Arc;

use anyhow::Context;
use tracing::{info, warn};

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

    let state = state::AppState::new(config.clone(), pool, engine.clone())
        .context("building application state")?;
    // TLS is validated before anything binds (issue #30, ADR 008):
    // a broken certificate path or a domain-less ACME section fails
    // fast, not at the first handshake.
    let tls = config
        .tls_selection()
        .context("validating [tls] configuration")?;
    let app = http::with_tls_headers(http::router(state), tls.is_some());

    match tls {
        None => {
            warn!(
                "no [tls] configuration: serving plain HTTP — fine for \
                 local development, never expose it to the internet"
            );
            let listener = tokio::net::TcpListener::bind(config.bind)
                .await
                .with_context(|| format!("binding {}", config.bind))?;

            info!(url = %config.base_url, "listening");
            axum::serve(
                listener,
                app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
            )
            .with_graceful_shutdown(shutdown_signal())
            .await
            .context("server error")?;
        }
        Some(selection) => {
            tls::serve(&config, selection, app).await?;
        }
    }
    engine.shutdown();
    Ok(())
}

/// Ctrl-C → graceful drain, shared by the HTTP and TLS serve paths.
pub(crate) async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
    info!("shutdown signal received");
}
