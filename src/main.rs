//! Muse — AI-native media curation & taste companion.
//!
//! Phase 0 scaffold (MUSE-01): boots tracing, config, a lazy Postgres pool,
//! the axum HTTP server, and the (currently empty) background-worker
//! harness. No domain logic lives here yet — see the founding spec
//! `specs/S96-muse-foundation.md`.

pub mod arr;
mod config;
mod db;
pub mod embed;
pub mod enrichment;
mod error;
mod http;
#[cfg(test)]
mod integration_tests;
pub mod models;
mod plex;
mod plex_control;
mod prowlarr;
mod radar;
pub mod repo;
pub mod tautulli;
mod tracker;
mod trending;
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

    let plex_client = crate::plex::PlexClient::from_config(&config);
    tracing::info!(plex_configured = plex_client.is_some(), "plex client initialized");

    let prowlarr_client = crate::prowlarr::ProwlarrClient::from_config(&config);
    tracing::info!(
        prowlarr_configured = prowlarr_client.is_some(),
        "prowlarr client initialized"
    );

    // MUSE-05: parse the configured *arr fleet. A malformed MUSE_ARR_INSTANCES
    // degrades to zero instances (logged, not fatal) — same posture as an
    // unconfigured Plex client above.
    let arr_instances = match config.arr_instances() {
        Ok(instances) => {
            tracing::info!(count = instances.len(), "arr fleet configured");
            instances
        }
        Err(e) => {
            tracing::warn!(error = %e, "failed to parse MUSE_ARR_INSTANCES; arr ingest will have no instances");
            Vec::new()
        }
    };

    // MUSE-19: constructed for parity with the Plex client above (and so
    // TMDB_API_KEY misconfiguration is visible at boot); the scheduled
    // trending-ingest worker that actually calls
    // `trending::snapshot_trending` on a cadence is a follow-on wiring item
    // — see `src/trending/mod.rs`.
    let tmdb_configured = crate::trending::TmdbClient::from_config(&config).is_some();
    tracing::info!(tmdb_configured, "tmdb client initialized");

    // MUSE-14: forum/critic sentiment + "does it get good" + renewal/
    // trailer news, cached into `external_enrichment`. Both sub-clients
    // degrade independently and gracefully — see `EnrichmentService`.
    let enrichment = crate::enrichment::EnrichmentService::from_config(&config);

    let state = Arc::new(AppState {
        pool,
        config: config.clone(),
        plex: plex_client,
        prowlarr: prowlarr_client,
        arr_instances,
        enrichment,
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
