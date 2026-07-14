//! Web surface (MUSE-27, spec §4d-F): the channel-guide JSON API
//! (`/api/channels`, `/api/channels/{id}/lineup`), the artwork proxy
//! (`/art/{kind}/{id}`), and a self-contained EPG-style guide page (`/`,
//! `/guide`) that consumes the JSON API. This is a **functional stub** — the
//! seed of the fuller Muse dashboard — not a polished UI.
//!
//! This module owns its own route group ([`routes`]) and is merged into the
//! top-level router by `crate::http::router`; it does not stand up its own
//! HTTP server or app state.

pub mod artwork;
pub mod graph;
pub mod guide;

use std::sync::Arc;

use axum::{
    routing::{get, post},
    Router,
};

use crate::http::AppState;

/// Routes this module contributes to the top-level router.
pub fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/", get(guide::guide_page))
        .route("/guide", get(guide::guide_page))
        .route("/api/channels", get(guide::list_channels_handler))
        .route("/api/channels/:id/lineup", get(guide::lineup_handler))
        .route("/art/:kind/:id", get(artwork::art_handler))
        // MUSEX-17: graph-visualization endpoints — see `graph`'s module doc.
        .route("/api/graph/taste-map", post(graph::taste_map_handler))
        .route("/api/graph/group-dynamics", post(graph::group_dynamics_handler))
        .route("/api/graph/watch-history", post(graph::watch_history_handler))
        .route("/api/graph/taste-clusters", post(graph::taste_clusters_handler))
}
