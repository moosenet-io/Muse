//! MUSEX-17 (Plane TERM #393): graph-visualization endpoints for the
//! Constellation GUI — `POST /api/graph/taste-map`, `/group-dynamics`,
//! `/watch-history`, `/taste-clusters`.
//!
//! ## Why POST-with-graph-body, not a DB-backed GET
//! Every handler here takes an already-ASSEMBLED, privacy-scoped
//! [`KgGraph`] ([`crate::kg::assemble::assemble_shared_graph`]'s output) as
//! its request body and returns a render-ready [`crate::kg::viz`]
//! structure. This mirrors `crate::kg`'s own documented posture (pure
//! function, "the caller resolves [source data]... this module doesn't
//! prescribe the source") one layer up: as of MUSEX-17, nothing in this
//! crate persists a live [`crate::discord::identity::TrustedFriends`]
//! allowlist or a discord-user-id↔account mapping anywhere in the database
//! (`grep -rn discord_user_id src/repo` turns up nothing — every existing
//! caller of `TrustedFriends` builds it in-memory: `crate::discord::bot`,
//! `crate::premiere`, `crate::promotion`). Wiring a DB-backed assembly path
//! is real future work, not something this module invents unverified
//! schema for. Whatever process already has the assembled graph today (an
//! ops/debugging caller, or a future scheduled KG-build job) POSTs it here;
//! these handlers never touch the DB or re-derive privacy scoping
//! themselves — that inherited-by-construction property is exactly what
//! [`crate::kg::viz`]'s `tests::privacy` module exercises one layer below
//! this thin HTTP wrapper.
//!
//! No DB pool access is needed for these handlers — the computation is pure
//! over the request body, the same DB-free posture as `crate::kg::viz`
//! itself. Only the watch-history handler touches [`crate::http::AppState`]
//! at all, and only to read the configured series-length cap.

use std::sync::Arc;

use axum::{extract::State, Json};
use serde::Deserialize;

use crate::http::AppState;
use crate::kg::model::KgGraph;
use crate::kg::viz::{self, GroupDynamicsViz, TasteClusterViz, TasteMapViz, WatchHistoryViz};

#[derive(Debug, Deserialize)]
pub struct TasteMapRequest {
    pub graph: KgGraph,
    pub person_id: String,
}

/// `POST /api/graph/taste-map` — one opted-in person's persona
/// constellation + taste-neighbor edges. See [`viz::build_taste_map`]. A
/// `person_id` absent from `graph` (not opted in, or no data yet) degrades
/// to an empty-but-valid [`TasteMapViz`], never an error response.
pub async fn taste_map_handler(Json(req): Json<TasteMapRequest>) -> Json<TasteMapViz> {
    Json(viz::build_taste_map(&req.graph, &req.person_id))
}

#[derive(Debug, Deserialize)]
pub struct GroupDynamicsRequest {
    pub graph: KgGraph,
}

/// `POST /api/graph/group-dynamics` — who-bridges-whom, with bridge/
/// centrality annotations. See [`viz::build_group_dynamics`].
pub async fn group_dynamics_handler(
    Json(req): Json<GroupDynamicsRequest>,
) -> Json<GroupDynamicsViz> {
    Json(viz::build_group_dynamics(&req.graph))
}

#[derive(Debug, Deserialize)]
pub struct WatchHistoryRequest {
    pub graph: KgGraph,
    /// `None` = every opted-in person's watch history (already
    /// privacy-scoped by construction — see this module's doc).
    #[serde(default)]
    pub person_id: Option<String>,
}

/// `POST /api/graph/watch-history` — a temporal watch-history series. See
/// [`viz::build_watch_history`]. The series length is capped by
/// `MUSE_KG_VIZ_WATCH_HISTORY_LIMIT` (`Config::kg_viz_watch_history_limit`)
/// — never a bare literal here, same config discipline as every other
/// MUSEX-16/17 threshold.
pub async fn watch_history_handler(
    State(state): State<Arc<AppState>>,
    Json(req): Json<WatchHistoryRequest>,
) -> Json<WatchHistoryViz> {
    let limit = state.config.kg_viz_watch_history_limit as usize;
    Json(viz::build_watch_history(
        &req.graph,
        req.person_id.as_deref(),
        limit,
    ))
}

#[derive(Debug, Deserialize)]
pub struct TasteClustersRequest {
    pub graph: KgGraph,
}

/// `POST /api/graph/taste-clusters` — taste-neighbor cluster groupings.
/// See [`viz::build_taste_clusters`].
pub async fn taste_clusters_handler(
    Json(req): Json<TasteClustersRequest>,
) -> Json<TasteClusterViz> {
    Json(viz::build_taste_clusters(&req.graph))
}
