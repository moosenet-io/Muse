//! Web surface (MUSE-27, spec §4d-F): the channel-guide JSON API
//! (`/api/channels`, `/api/channels/{id}/lineup`), the artwork proxy
//! (`/art/{kind}/{id}`), and a self-contained EPG-style guide page (`/`,
//! `/guide`) that consumes the JSON API. This is a **functional stub** — the
//! seed of the fuller Muse dashboard — not a polished UI.
//!
//! This module owns two route groups — [`public_routes`] and
//! [`protected_routes`] (MUSEX-CAP-SEC-01, Plane TERM #399) — merged into
//! the top-level router by `crate::http::router`, which applies
//! `crate::http::auth::require_api_token` to the latter only; this module
//! does not stand up its own HTTP server or app state.

pub mod artwork;
pub mod artwork_render;
pub mod dashboard;
pub mod graph;
pub mod household;
pub mod guide;
pub mod search;
pub mod settings;

use std::sync::Arc;

use axum::{
    routing::{get, post},
    Router,
};

use crate::http::AppState;

/// Routes this module contributes to the top-level router that stay
/// UNAUTHENTICATED (MUSEX-CAP-SEC-01, Plane TERM #399): the browser-facing
/// channel-guide page + its backing JSON API + the artwork proxy. These are
/// consumed by a plain `<img>`/`fetch` from the guide page itself (and by
/// Plex, which links to the guide) — neither sends a custom
/// `Authorization` header, and the capstone finding that motivated
/// MUSEX-CAP-SEC-01 did not flag this read-only metadata surface. See
/// `crate::http::router` for where [`protected_routes`] gets the auth
/// layer instead.
pub fn public_routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/", get(guide::guide_page))
        .route("/guide", get(guide::guide_page))
        .route("/api/channels", get(guide::list_channels_handler))
        .route("/api/channels/:id/lineup", get(guide::lineup_handler))
        .route("/art/:kind/:id", get(artwork::art_handler))
        // MWEBX-05 (S126): the MUSE web "detail bench" READ surface — the
        // non-sensitive browse screens (library grid/table/detail, discover,
        // and the honest subsystem health grid). Deliberately UNAUTHENTICATED,
        // same posture as the channel-guide JSON above: these carry only
        // library titles + same-origin `/art` proxy URLs + wiring-state, no
        // per-account or credential data. The per-account/operational reads
        // (requests, taste, curation, indexers/rss) live on
        // [`protected_routes`] instead. See `crate::web::dashboard`.
        .route("/api/library", get(dashboard::get_library))
        .route("/api/library/table", get(dashboard::get_library_table))
        .route("/api/library/:id", get(dashboard::get_library_detail))
        .route("/api/discover", get(dashboard::get_discover))
        // MUSE #108: free-text metadata search across configured providers.
        .route("/api/search", get(search::get_search))
        .route("/api/subsystems", get(dashboard::get_subsystems))
        // MUSE #84: the Constellation web GUI's Muse dashboard cards. Only the
        // two whole-library aggregates are public — `/stats` is four counts and
        // a timestamp, `/gaps` is the same `wanted_titles` projection
        // `/api/library` already returns here. Neither has a per-account
        // component. `/on_deck` (viewing history) and `/premiere` stay on
        // [`protected_routes`]; do not move them here.
        .route("/stats", get(dashboard::get_stats))
        .route("/gaps", get(dashboard::get_gaps))
}

/// Routes this module contributes that MUST be protected by
/// `crate::http::auth::require_api_token` (MUSEX-CAP-SEC-01, Plane TERM
/// #399): the graph-visualization endpoints return per-friend taste/watch
/// data, and `/api/settings` is the Constellation GUI's control + tuning
/// panel (read AND write — a GET here still leaks operational config, so
/// both verbs are protected, not just the PUT). `crate::http::router`
/// mounts this group with `Router::route_layer` rather than merging it
/// unauthenticated like [`public_routes`].
pub fn protected_routes() -> Router<Arc<AppState>> {
    Router::new()
        // MUSEX-17: graph-visualization endpoints — see `graph`'s module doc.
        .route("/api/graph/taste-map", post(graph::taste_map_handler))
        // MUSE #85: GET and POST at these three paths serve DIFFERENT things,
        // deliberately. POST is the unchanged MUSEX-17 client-fed KG viz over
        // Discord friend identities; GET is the household account analytics
        // the Constellation web GUI's `useMuse.ts` actually fetches (it sends
        // a parameterless GET and expects `{rows:[...]}`/`{series:[...]}`).
        // Adding a GET that reused the POST handlers would assemble an EMPTY
        // GraphSourceInput and return an empty viz on every call — which the
        // GUI renders AS DATA. See `crate::web::household`'s module doc.
        .route(
            "/api/graph/group-dynamics",
            get(household::group_dynamics_get_handler).post(graph::group_dynamics_handler),
        )
        .route(
            "/api/graph/watch-history",
            get(household::watch_history_get_handler).post(graph::watch_history_handler),
        )
        .route(
            "/api/graph/taste-clusters",
            get(household::taste_clusters_get_handler).post(graph::taste_clusters_handler),
        )
        // MUSEX-18: the Constellation GUI control + tuning panel surface —
        // see `settings`'s module doc.
        .route(
            "/api/settings",
            get(settings::get_settings_handler).put(settings::put_settings_handler),
        )
        // MWEBX-05 (S126): the per-account / operational READ screens. These
        // are protected for the same reason `/api/settings` + `/api/graph/*`
        // are (CAP-SEC-01/03): requests/queue expose the operator's request
        // pipeline, taste/curation are per-account taste data, and the
        // Prowlarr indexer list is operational config (private-tracker names).
        // All read-only — the write/approve/grab path stays on the separate
        // MUSEM-05 `/requests` router. `/api/requests/queue` is registered
        // BEFORE `/api/requests/:id` so the static segment wins.
        .route("/api/requests", get(dashboard::get_requests))
        .route("/api/requests/queue", get(dashboard::get_requests_queue))
        .route("/api/requests/:id", get(dashboard::get_request_detail))
        .route("/api/taste", get(dashboard::get_taste))
        .route("/api/curation", get(dashboard::get_curation))
        .route("/api/indexers", get(dashboard::get_indexers))
        .route("/api/indexers/rss", get(dashboard::get_rss))
        .route("/api/rss", get(dashboard::get_rss))
        // MUSE #84: per-account viewing history + premiere schedule. PROTECTED
        // (CAP-SEC-03) — `/on_deck` is "who left what half-watched", which is
        // exactly the per-account data this group exists to gate.
        .route("/on_deck", get(dashboard::get_on_deck))
        .route("/premiere", get(dashboard::get_premiere))
        // MACT-01 (Plane MUSE #121): live + historical session activity.
        // PROTECTED for the same reason `/on_deck` is (CAP-SEC-03) — a live
        // session names which household account is watching what, right now
        // (and history is the same fact over time), so it gets the same gate
        // as every other per-account viewing-activity read in this group.
        .route("/api/sessions/live", get(dashboard::get_live_sessions))
        .route("/api/sessions/history", get(dashboard::get_session_history))
        // MACT-02 (Plane MUSE #122): the one mutation in this group — stop
        // a live stream. Protected for the same CAP-SEC-03 reason as
        // `/api/sessions/live` above, AND because it's a mutation with
        // real-world blast radius (see `dashboard::terminate_session`'s doc
        // comment). Terminus's `proxy_muse` layers `enforce_viewer_role_gate`
        // in front of this at the Constellation-web boundary — a viewer's
        // POST never reaches this handler at all; this bearer gate is the
        // second, independent layer.
        .route(
            "/api/sessions/:session_key/terminate",
            post(dashboard::terminate_session),
        )
}
