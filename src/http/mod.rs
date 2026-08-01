//! HTTP surface: the axum `Router` and shared application state.
//!
//! Phase 0 wires `/health`, (MUSE-09) `/query/resolve` + `/query/similar`,
//! and (MUSE-12) `/proactive/pending` + `/proactive/{id}/ack` for real; the
//! rest of `/ingest` and `/query` are mounted as stub route groups that
//! answer `501 Not Implemented` until their respective spec items land.

pub mod auth;
pub mod ops;
pub mod requests;

use std::sync::Arc;
use std::time::Duration;

use axum::{
    extract::State,
    middleware,
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use serde_json::json;
use sqlx::postgres::PgPool;
use tower_http::trace::TraceLayer;

use crate::arr::ArrInstanceConfig;
use crate::config::Config;
use crate::embed::ChordEmbedClient;
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
    /// (the same `ChordEmbedClient` type MUSE-08's embed pipeline uses).
    /// `None` when `MUSE_OLLAMA_URL` isn't configured — the vector tier
    /// degrades to skipped, falling through to pg_trgm.
    pub embed: Option<ChordEmbedClient>,
    /// MUSEM-05: the qBittorrent download client the request-lifecycle
    /// endpoints (`crate::http::requests`) grab through. `None` when
    /// `MUSE_QBIT_URL`/`MUSE_QBIT_USER`/`MUSE_QBIT_PASS` aren't fully
    /// configured — same graceful-degrade posture as every other optional
    /// integration on this struct: the request endpoints still persist
    /// requests, they just can never fulfill one (see
    /// `crate::acquisition::fulfill_request`'s "download client
    /// unavailable" path).
    pub download: Option<crate::download::qbit::QbitClient>,
}

/// Timeout for the `/health` DB probe — health must never hang/500 just
/// because Postgres is slow or down.
const HEALTH_DB_TIMEOUT: Duration = Duration::from_secs(2);

/// Build the top-level router for the service.
///
/// ## MUSEX-CAP-SEC-01 (Plane TERM #399): endpoint auth
/// The S118 Epic capstone flagged that the wired experience-layer routes,
/// the Constellation-GUI control surface, and the manual ops triggers had
/// **no auth** — `0.0.0.0` bind plus only a `TraceLayer`. Those routes are
/// now built as their own sub-router (the `protected` `Router` local to
/// this function) and mounted with
/// `Router::route_layer(middleware::from_fn_with_state(...,
/// auth::require_api_token))`, so `crate::http::auth::require_api_token`
/// runs BEFORE any of their handlers — a rejected request never touches the
/// database. `route_layer` (not `layer`) is deliberate: it applies only to
/// routes registered on `protected_routes()` itself, not to whatever this
/// function `.merge`s in afterward, so it can never accidentally leak onto
/// `/health` or the rest of the open surface.
///
/// **Left open** (unauthenticated), and why:
/// - `GET /health` — liveness/readiness; must never require a credential.
/// - `/ingest/*`, `/query/*`, `/proactive/*`, `/channels/:id/compose` — the
///   pre-WIRE, Phase-0/MUSE-xx surface the capstone finding did NOT flag; each
///   already has its own consumer (Plex's webhook poster for
///   `/ingest/plex-webhook`, the HDHomeRun/M3U/ XMLTV tuner protocol, the
///   reminders/engagement engine for `/proactive/*`) that cannot be confirmed
///   to send a bearer header without touching code outside this repo —
///   bringing them into the auth perimeter is left to a follow-up item rather
///   than guessed at here.
/// - The HDHomeRun-emulation tuner routes (`tuner_routes`) and the
///   browser-facing guide/channel-JSON/artwork surface
///   (`crate::web::public_routes`) — these are consumed by Plex/HDHomeRun
///   clients and plain `<img>`/`fetch` calls from the guide page itself,
///   neither of which can be made to send a custom `Authorization` header.
///
/// **Protected**: `/discord/respond`, `/conversational`, `/premiere` +
/// `/premiere/rsvp`, `/channels/director/refresh`, `/friends/opt-in` +
/// `/friends/opt-out` (the WIRE-01..06 experience layer the capstone named
/// directly), `crate::web::protected_routes` (`/api/graph/*` — per-friend
/// taste/watch data — and `/api/settings` GET+PUT — the control panel),
/// `/recommend*` (MUSEX-CAP-SEC-03: per-account taste/on-deck/gap candidates —
/// authenticated so an unauthenticated caller cannot enumerate any account's
/// taste data by numeric `account_id`), and `/ops/*` (manual
/// maintenance/ingest triggers).
pub fn router(state: Arc<AppState>) -> Router {
    let auth_layer = middleware::from_fn_with_state(state.clone(), auth::require_api_token);

    let protected = Router::new()
        // MUSEX-WIRE-01 (Plane TERM #398): the wired, settings-gated Discord
        // response flow — `POST /discord/respond`. See
        // `crate::discord::bot::discord_respond_handler`.
        .route("/discord/respond", post(crate::discord::bot::discord_respond_handler))
        // MUSEX-WIRE-02 (Plane TERM #398, slice 2): the wired,
        // settings-gated + consent-enforced conversational-request flow —
        // `POST /conversational`. See
        // `crate::conversational::conversational_handler`.
        .route(
            "/conversational",
            post(crate::conversational::conversational_handler),
        )
        // MUSEX-WIRE-03 (Plane TERM #398): the wired, settings-gated
        // premiere schedule + RSVP flow — `POST /premiere`, `POST
        // /premiere/rsvp`. See `crate::premiere::http`.
        .route("/premiere", post(crate::premiere::http::premiere_schedule_handler))
        .route("/premiere/rsvp", post(crate::premiere::http::premiere_rsvp_handler))
        // MUSEX-WIRE-04 (Plane TERM #398): the wired, settings-gated,
        // consent-enforced channel DIRECTOR entry point — `POST
        // /channels/director/refresh`. See
        // `crate::channels::director_route::channel_director_refresh_handler`.
        .route(
            "/channels/director/refresh",
            post(crate::channels::channel_director_refresh_handler),
        )
        // MUSEX-WIRE-05 (Plane TERM #398, slice 5): the persisted opt-in
        // store's write doors — `POST /friends/opt-in` / `POST
        // /friends/opt-out`. These are the ONLY production entry points
        // that write `repo::friend_opt_in` rows; see
        // `crate::discord::opt_in_route` and `crate::discord::roster`
        // (the resolver every WIRE handler should read from).
        .route(
            "/friends/opt-in",
            post(crate::discord::friend_opt_in_handler),
        )
        .route(
            "/friends/opt-out",
            post(crate::discord::friend_opt_out_handler),
        )
        // MUSEX-17/18: graph-visualization + Constellation GUI
        // control/settings surface — see `crate::web::protected_routes`.
        .merge(crate::web::protected_routes())
        // MUSE-31: on-demand ops routes -- manual triggers for the same
        // routines the background maintenance/trending workers run on a
        // schedule (see `crate::maintenance`). Mainly for priming a fresh
        // deploy and operator debugging.
        .nest("/ops", ops_routes())
        // MUSEX-CAP-SEC-03 (Plane TERM #399, epic-capstone finding): the
        // MUSE-11 curation/recommend engine — `POST /recommend`, `GET
        // /recommend/on_deck`, `GET /recommend/gaps`. These serve per-account
        // taste/on-deck/gap candidates for a caller-supplied `account_id`, so
        // they are SENSITIVE (an unauthenticated caller could otherwise
        // enumerate any account's taste data by numeric id). Moved from the
        // open router into `protected` so `auth::require_api_token` gates them.
        .nest("/recommend", recommend_routes())
        // MUSEM-05: the request lifecycle (`POST /requests`, `GET /requests`,
        // `POST /requests/:id/approve`, `POST /requests/:id/deny`) — can
        // trigger a real download-client grab and serves request/lifecycle
        // data, so it is gated the same as `/recommend*` above.
        .nest("/requests", request_routes())
        .route_layer(auth_layer);

    Router::new()
        .route("/health", get(health))
        // PROMEX-03: `GET /metrics` — encodes the process-global
        // `crate::metrics` registry (recommendation-engine request counts +
        // latency histogram) in the standard Prometheus text exposition
        // format. Mounted unauthenticated alongside `/health` — see
        // `crate::metrics`'s module doc for why (aggregate counts/timings
        // only, no per-account recommendation content).
        .route("/metrics", get(handle_metrics))
        .nest("/ingest", ingest_routes())
        .nest("/query", query_routes())
        .nest("/proactive", proactive_routes())
        // MUSEX-CAP-SEC-03: `/recommend*` moved to the `protected` router
        // above (it serves per-account taste data and must be authenticated).
        // MUSE-27: the channel-guide page/API + artwork proxy (`/`, `/guide`,
        // `/api/channels*`, `/art/{kind}/{id}`). Deliberately UNAUTHENTICATED
        // — see the doc comment on this function.
        .merge(crate::web::public_routes())
        // MUSE-28: HDHomeRun-emulation linear tuner (`/discover.json`,
        // `/lineup.json`, `/muse.m3u`, `/xmltv.xml`, ...).
        .merge(tuner_routes())
        // MUSE-31: the on-demand channel composer trigger (MUSE-24's
        // `compose_channel_run` had no HTTP surface until now).
        .route("/channels/:id/compose", post(crate::channels::compose_handler))
        // MUSEX-CAP-SEC-01 (Plane TERM #399): every route registered on
        // `protected` is gated by `auth::require_api_token` — see this
        // function's doc comment for the full authed/open breakdown.
        .merge(protected)
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

/// MUSE-31: on-demand ops routes — see `crate::http::ops`.
fn ops_routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/ingest/arr", post(ops::ingest_arr))
        .route("/ingest/tautulli", post(ops::ingest_tautulli))
        .route("/maintenance", post(ops::run_maintenance_now))
        // BSEED-2: on-demand re-resolution of previously-unresolved Tautulli
        // sessions against the now-populated catalog. Bearer-protected (this
        // whole `ops` router is nested under `protected`).
        .route("/library/resolve", post(ops::resolve_library))
        // MWEBX-05 (S126): on-demand read-only library scan/refresh — the
        // door the MUSE web Library screen calls to (re)build the library
        // before re-reading `/api/library*`. See
        // `crate::web::dashboard::trigger_library_scan`.
        .route("/library/scan", post(crate::web::dashboard::trigger_library_scan))
        // FOUNDRY-02: report what transcoding WOULD do. Encodes nothing, writes nothing.
        .route("/foundry/survey", post(crate::web::dashboard::foundry_survey))
        // FOUNDRY-04: really encode a diverse sample TO SCRATCH and verify it.
        // Writes only inside the Foundry work dir; never renames, replaces or
        // deletes anything in the library. This is a LONG call — up to 24 real
        // encodes, 20 minutes each, 60 minutes for the whole run.
        .route("/foundry/validate", post(crate::web::dashboard::foundry_validate))
        // SUBS-01: the subtitle system. Bearer-protected like the rest of this
        // router — fetching touches an external provider, and applying an
        // offset changes what a viewer sees.
        //
        // `propose` and `apply` are deliberately SEPARATE routes rather than
        // one route with a flag: measuring a subtitle's timing and changing it
        // must never be the same call with a different body.
        .route(
            "/subtitles/:media_item_id",
            get(crate::subtitles::routes::list_subtitles),
        )
        .route(
            "/subtitles/:media_item_id/fetch",
            post(crate::subtitles::routes::fetch_from_provider),
        )
        .route(
            "/subtitles/selection/:id/active",
            post(crate::subtitles::routes::set_active),
        )
        .route(
            "/subtitles/selection/:id/offset/propose",
            post(crate::subtitles::routes::propose_offset),
        )
        .route(
            "/subtitles/selection/:id/offset/apply",
            post(crate::subtitles::routes::apply_offset),
        )
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
        .route("/auto/v:channel_id", get(crate::streaming::stream_channel))
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

/// MUSE-12: `GET /proactive/pending` (the Lumina reminders/engagement +
/// Terminus `muse_proactive` surface's read path) + `POST /proactive/{id}/ack`.
/// Replaces the previous 501 stub.
fn proactive_routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/pending", get(crate::proactive::pending_handler))
        .route("/:id/ack", post(crate::proactive::ack_handler))
        .fallback(not_implemented)
}

/// MUSE-11: `POST /recommend` (mounted at nest root, i.e. `POST /recommend`
/// itself) + `GET /recommend/on_deck` + `GET /recommend/gaps`.
fn recommend_routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/", post(crate::curation::recommend_handler))
        .route("/on_deck", get(crate::curation::on_deck_handler))
        .route("/gaps", get(crate::curation::gaps_handler))
}

/// MUSEM-05: `POST /requests` (mounted at nest root), `GET /requests`,
/// `POST /requests/:id/approve`, `POST /requests/:id/deny` — see
/// `crate::http::requests`.
fn request_routes() -> Router<Arc<AppState>> {
    Router::new()
        .route(
            "/",
            post(requests::create_request_handler).get(requests::list_requests_handler),
        )
        .route("/:id/approve", post(requests::approve_request_handler))
        .route("/:id/deny", post(requests::deny_request_handler))
}

async fn not_implemented() -> MuseError {
    MuseError::NotImplemented
}

/// PROMEX-03: `GET /metrics` — see this module's `router` doc comment and
/// `crate::metrics`'s module doc.
async fn handle_metrics() -> impl IntoResponse {
    (
        axum::http::StatusCode::OK,
        [("content-type", "text/plain; version=0.0.4")],
        crate::metrics::gather_text(),
    )
}
