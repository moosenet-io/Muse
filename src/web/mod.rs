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
pub mod graph;
pub mod guide;
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
        .route("/api/graph/group-dynamics", post(graph::group_dynamics_handler))
        .route("/api/graph/watch-history", post(graph::watch_history_handler))
        .route("/api/graph/taste-clusters", post(graph::taste_clusters_handler))
        // MUSEX-18: the Constellation GUI control + tuning panel surface —
        // see `settings`'s module doc.
        .route(
            "/api/settings",
            get(settings::get_settings_handler).put(settings::put_settings_handler),
        )
}
