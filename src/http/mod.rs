//! HTTP surface: the axum `Router` and shared application state.
//!
//! Phase 0 only wires `/health` for real; `/ingest`, `/query`, and
//! `/proactive` are mounted as stub route groups that answer `501 Not
//! Implemented` until their respective spec items (MUSE-04+) land.

use std::sync::Arc;
use std::time::Duration;

use axum::{
    extract::State,
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use serde_json::json;
use sqlx::postgres::PgPool;
use tower_http::trace::TraceLayer;

use crate::arr::ArrInstanceConfig;
use crate::config::Config;
use crate::enrichment::EnrichmentService;
use crate::error::MuseError;
use crate::plex::PlexClient;
use crate::prowlarr::ProwlarrClient;

/// Shared state handed to every axum handler.
pub struct AppState {
    pub pool: PgPool,
    pub config: Config,
    /// Read-only Plex client (MUSE-04). `None` when `PLEX_URL`/`PLEX_TOKEN`
    /// aren't configured — Plex-backed features degrade rather than fail.
    pub plex: Option<PlexClient>,
    /// Read-only Prowlarr availability client (MUSE-16). `None` when
    /// `PROWLARR_URL`/`PROWLARR_API_KEY` aren't configured — availability
    /// features degrade rather than fail.
    pub prowlarr: Option<ProwlarrClient>,
    /// Configured *arr fleet (MUSE-05) — empty when `MUSE_ARR_INSTANCES`
    /// isn't set or fails to parse (logged at startup, never fatal). Held
    /// as config rather than pre-built `ArrClient`s: `arr::ingest::run`
    /// constructs a short-lived client per instance per run. This is what a
    /// future scheduled ingest worker or `/ingest/arr` trigger reads.
    pub arr_instances: Vec<ArrInstanceConfig>,
    /// MUSE-14: forum/critic sentiment + "does it get good" + renewal/
    /// trailer news enrichment, cached into `external_enrichment`. Both
    /// underlying HTTP sources degrade independently and gracefully when
    /// unconfigured.
    pub enrichment: EnrichmentService,
}

/// Timeout for the `/health` DB probe — health must never hang/500 just
/// because Postgres is slow or down.
const HEALTH_DB_TIMEOUT: Duration = Duration::from_secs(2);

/// Build the top-level router for the service.
pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/health", get(health))
        .nest("/ingest", ingest_routes())
        .nest("/query", query_routes())
        .nest("/proactive", proactive_routes())
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

async fn health(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let db_status = match tokio::time::timeout(HEALTH_DB_TIMEOUT, sqlx::query("SELECT 1").execute(&state.pool)).await
    {
        Ok(Ok(_)) => "up",
        Ok(Err(_)) | Err(_) => "down",
    };

    Json(json!({
        "status": "ok",
        "version": env!("CARGO_PKG_VERSION"),
        "db": db_status,
    }))
}

fn ingest_routes() -> Router<Arc<AppState>> {
    // MUSE-07: the native Plex tracker's webhook receiver (spec §4-A) — the
    // only real route in this group so far; everything else still answers
    // 501 until its own spec item lands.
    Router::new()
        .route("/plex-webhook", post(crate::tracker::webhook::plex_webhook))
        .fallback(not_implemented)
}

fn query_routes() -> Router<Arc<AppState>> {
    Router::new().fallback(not_implemented)
}

fn proactive_routes() -> Router<Arc<AppState>> {
    Router::new().fallback(not_implemented)
}

async fn not_implemented() -> MuseError {
    MuseError::NotImplemented
}
