//! HTTP surface: the axum `Router` and shared application state.
//!
//! Phase 0 wires `/health` and (MUSE-09) `/query/resolve` + `/query/similar`
//! for real; the rest of `/ingest`, `/query`, and `/proactive` are mounted
//! as stub route groups that answer `501 Not Implemented` until their
//! respective spec items land.

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
use crate::embed::OllamaEmbedClient;
use crate::enrichment::EnrichmentService;
use crate::error::MuseError;
use crate::plex::PlexClient;
use crate::prowlarr::ProwlarrClient;
use crate::trending::TmdbClient;

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
    /// Read-only TMDb client (MUSE-19), also used by MUSE-09's
    /// `/query/resolve` beyond-the-library tier. `None` when
    /// `TMDB_API_KEY` isn't configured — that tier degrades to unreachable
    /// (never a 500) rather than failing.
    pub tmdb: Option<TmdbClient>,
    /// Query-embedding client for MUSE-09's `/query/resolve` vector tier
    /// (the same `OllamaEmbedClient` type MUSE-08's embed pipeline uses).
    /// `None` when `MUSE_OLLAMA_URL` isn't configured — the vector tier
    /// degrades to skipped, falling through to pg_trgm.
    pub embed: Option<OllamaEmbedClient>,
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
        // MUSE-11: the curation/recommend engine — `POST /recommend`,
        // `GET /recommend/on_deck`, `GET /recommend/gaps`.
        .nest("/recommend", recommend_routes())
        // MUSE-27: the channel-guide page/API + artwork proxy (`/`, `/guide`,
        // `/api/channels*`, `/art/{kind}/{id}`).
        .merge(crate::web::routes())
        // MUSE-28: HDHomeRun-emulation linear tuner (`/discover.json`,
        // `/lineup.json`, `/muse.m3u`, `/xmltv.xml`, ...).
        .merge(tuner_routes())
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

/// MUSE-28/29: the linear tuner surface — HDHomeRun-emulation discovery
/// (`/discover.json`, `/lineup_status.json`, `/lineup.json`), the M3U+XMLTV
/// alternative (`/muse.m3u`, `/xmltv.xml`), and the MUSE-29 ffmpeg streaming
/// engine (`/auto/v{channel_id}`) every one of the above advertises a URL
/// for. Mounted at the router root (not nested) — HDHomeRun/M3U/XMLTV
/// clients expect these exact top-level paths, not a namespaced prefix.
fn tuner_routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/discover.json", get(crate::tuner::hdhr::discover_json))
        .route("/lineup_status.json", get(crate::tuner::hdhr::lineup_status_json))
        .route("/lineup.json", get(crate::tuner::hdhr::lineup_json))
        .route("/muse.m3u", get(crate::tuner::m3u::muse_m3u))
        .route("/xmltv.xml", get(crate::tuner::xmltv::xmltv_xml))
        .route("/auto/v{channel_id}", get(crate::streaming::stream_channel))
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
    // MUSE-09: the vector-recall / search API. `/resolve` and `/similar` are
    // the only real routes in this group so far; everything else still
    // answers 501 until its own spec item lands.
    Router::new()
        .route("/resolve", post(crate::recall::resolve_handler))
        .route("/similar", post(crate::recall::similar_handler))
        .fallback(not_implemented)
}

fn proactive_routes() -> Router<Arc<AppState>> {
    Router::new().fallback(not_implemented)
}

/// MUSE-11: `POST /recommend` (mounted at nest root, i.e. `POST /recommend`
/// itself) + `GET /recommend/on_deck` + `GET /recommend/gaps`.
fn recommend_routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/", post(crate::curation::recommend_handler))
        .route("/on_deck", get(crate::curation::on_deck_handler))
        .route("/gaps", get(crate::curation::gaps_handler))
}

async fn not_implemented() -> MuseError {
    MuseError::NotImplemented
}
