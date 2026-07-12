//! Muse — AI-native media curation & taste companion.
//!
//! Phase 0 scaffold (MUSE-01): boots tracing, config, a lazy Postgres pool,
//! the axum HTTP server, and the (currently empty) background-worker
//! harness. No domain logic lives here yet — see the founding spec
//! `specs/S96-muse-foundation.md`.

mod config;
mod db;
mod error;
mod http;
#[cfg(test)]
mod integration_tests;
pub mod models;
pub mod repo;
mod workers;

use std::sync::Arc;

use tracing_subscriber::EnvFilter;

use crate::config::Config;
use crate::http::AppState;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let config = Config::from_env();

    init_tracing(&config.log_level);

    tracing::info!(bind_addr = %config.bind_addr, "starting muse");

    let pool = db::build_pool(&config)
        .map_err(|e| anyhow::anyhow!("failed to construct database pool: {e}"))?;

    let state = Arc::new(AppState {
        pool,
        config: config.clone(),
    });

    // Best-effort migration attempt at startup. This is a scaffold: if the DB
    // isn't reachable yet, log and continue — /health will report db:down
    // and MUSE-02+ will make this a harder gate once real schema exists.
    if let Err(e) = db::migrate(&state.pool).await {
        tracing::warn!(error = %e, "startup migration did not complete; continuing (db may be unavailable)");
    }

    workers::spawn_workers(state.clone());

    let listener = tokio::net::TcpListener::bind(&config.bind_addr)
        .await
        .map_err(|e| anyhow::anyhow!("failed to bind {}: {e}", config.bind_addr))?;

    let app = http::router(state);

    tracing::info!(bind_addr = %config.bind_addr, "muse listening");

    axum::serve(listener, app)
        .await
        .map_err(|e| anyhow::anyhow!("http server error: {e}"))?;

    Ok(())
}

fn init_tracing(log_level: &str) {
    let filter = EnvFilter::try_new(log_level).unwrap_or_else(|_| EnvFilter::new("info"));

    tracing_subscriber::fmt().with_env_filter(filter).init();
}
