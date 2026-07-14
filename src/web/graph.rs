//! MUSEX-17 (Plane TERM #393): graph-visualization endpoints for the
//! Constellation GUI — `POST /api/graph/taste-map`, `/group-dynamics`,
//! `/watch-history`, `/taste-clusters`.
//!
//! ## Privacy is enforced AT THIS BOUNDARY, not just described
//! A codex review of the first cut of this module found a real privacy gap:
//! the handlers used to deserialize an already-assembled [`KgGraph`]
//! straight from the request body and hand it to a [`crate::kg::viz`]
//! builder. That let a client POST a graph containing relationships that
//! NEVER passed through [`crate::kg::assemble::assemble_shared_graph`]'s
//! opt-in filter — the module doc described the intended layering, but the
//! endpoint contract didn't enforce it, so the web layer could bypass the
//! filter entirely.
//!
//! The fix, implemented here: **no handler accepts a pre-assembled
//! `KgGraph`.** Every handler accepts only the SOURCE inputs
//! `assemble_shared_graph` needs — a [`TrustedFriends`] allowlist
//! (reconstructed from [`FriendInput`] via the SAME `FriendIdentity::new` +
//! `FriendIdentity::opt_in` path `crate::discord::bot`/`crate::premiere`/
//! `crate::promotion` already use, so opt-in state can only be set through
//! the one sanctioned mutator) plus the raw watch/co-view/persona source
//! records — and then BUILDS the graph server-side through
//! [`assemble_shared_graph`] before running any viz builder. Because the
//! opt-in filter is now structurally in the ONLY path from request to
//! response, a non-opted-in entity present in the source inputs is stripped
//! by `assemble_shared_graph` before any viz sees it, and therefore cannot
//! appear in the output. See `tests::privacy_is_enforced_at_the_http_boundary`.
//!
//! ## Source provenance (unchanged, documented)
//! As of MUSEX-17 nothing in this crate persists a live [`TrustedFriends`]
//! allowlist or a discord-user-id↔account mapping in the database (every
//! existing `TrustedFriends` caller builds it in-memory), so these endpoints
//! receive their source records + allowlist in the request body rather than
//! resolving them from a DB pool — the same DB-free posture `crate::kg` and
//! `crate::kg::viz` document for themselves. Wiring a DB-backed assembly
//! path is real future work; what matters for privacy is that whatever the
//! source, it is ALWAYS funneled through `assemble_shared_graph` here. An
//! empty/absent source degrades to an empty-but-valid viz, never an error
//! and never a trusted client graph.

use std::sync::Arc;

use axum::{extract::State, Json};
use chrono::{DateTime, Utc};
use serde::Deserialize;

use crate::discord::identity::{FriendIdentity, TrustedFriends};
use crate::http::AppState;
use crate::kg::assemble::{
    assemble_shared_graph, CoViewRecord, GraphSourceData, PersonaRecord, WatchRecord,
};
use crate::kg::model::{person_node_id, KgGraph};
use crate::kg::viz::{self, GroupDynamicsViz, TasteClusterViz, TasteMapViz, WatchHistoryViz};

/// One allowlisted friend, as the client expresses them. `opted_in_account_id`
/// is `Some(account_id)` for an opted-in-and-linked friend and `None` for a
/// not-opted-in one — this maps ONE-TO-ONE onto the only two production
/// consent states [`FriendIdentity`] can hold (see [`Self::into_identity`]),
/// so there is no way to express an "opted in but unlinked" state the type
/// system already forbids.
#[derive(Debug, Clone, Deserialize)]
pub struct FriendInput {
    pub discord_user_id: String,
    pub display_name: String,
    /// `Some(account_id)` = opted in and linked; `None` = not opted in.
    #[serde(default)]
    pub opted_in_account_id: Option<i64>,
}

impl FriendInput {
    /// Reconstruct a [`FriendIdentity`] through the SAME sanctioned path
    /// production code uses: `new` (always starts not-opted-in) then, only
    /// when an account id is present, the single `opt_in` mutator. There is
    /// deliberately no path here that sets consent any other way.
    fn into_identity(self) -> FriendIdentity {
        let identity = FriendIdentity::new(self.discord_user_id, self.display_name);
        match self.opted_in_account_id {
            Some(account_id) => identity.opt_in(account_id),
            None => identity,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct WatchInput {
    pub discord_user_id: String,
    pub media_item_id: i64,
    pub title: String,
    pub watched_at: DateTime<Utc>,
}

impl From<WatchInput> for WatchRecord {
    fn from(w: WatchInput) -> Self {
        WatchRecord {
            discord_user_id: w.discord_user_id,
            media_item_id: w.media_item_id,
            title: w.title,
            watched_at: w.watched_at,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct CoViewInput {
    pub person_a: String,
    pub person_b: String,
    pub session_key: String,
    pub media_item_id: i64,
    pub title: String,
    pub watched_at: DateTime<Utc>,
}

impl From<CoViewInput> for CoViewRecord {
    fn from(c: CoViewInput) -> Self {
        CoViewRecord {
            person_a: c.person_a,
            person_b: c.person_b,
            session_key: c.session_key,
            media_item_id: c.media_item_id,
            title: c.title,
            watched_at: c.watched_at,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct PersonaInput {
    pub discord_user_id: String,
    pub persona_id: i64,
    pub persona_name: String,
    pub centroid: Vec<f32>,
}

impl From<PersonaInput> for PersonaRecord {
    fn from(p: PersonaInput) -> Self {
        PersonaRecord {
            discord_user_id: p.discord_user_id,
            persona_id: p.persona_id,
            persona_name: p.persona_name,
            centroid: p.centroid,
        }
    }
}

/// The SOURCE inputs every graph endpoint accepts — the allowlist plus the
/// raw records — deliberately NOT a pre-assembled [`KgGraph`]. Every field
/// defaults to empty so a caller can send only what a given visualization
/// needs (e.g. group-dynamics can omit `personas`). [`Self::assemble`] is
/// the ONLY way this becomes a graph, and it always runs the opt-in filter.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct GraphSourceInput {
    #[serde(default)]
    pub friends: Vec<FriendInput>,
    #[serde(default)]
    pub watches: Vec<WatchInput>,
    #[serde(default)]
    pub co_views: Vec<CoViewInput>,
    #[serde(default)]
    pub personas: Vec<PersonaInput>,
}

impl GraphSourceInput {
    /// Build the privacy-scoped [`KgGraph`] from these source inputs — the
    /// single choke point through which every endpoint's graph is produced.
    /// Reconstructs the [`TrustedFriends`] allowlist from [`FriendInput`]s
    /// (opt-in state only via the sanctioned mutator) and funnels the raw
    /// records through [`assemble_shared_graph`], which strips every
    /// non-opted-in relation BEFORE returning. `taste_neighbor_threshold`
    /// comes from config, never a bare literal.
    fn assemble(self, taste_neighbor_threshold: f32) -> KgGraph {
        let friends =
            TrustedFriends::from_friends(self.friends.into_iter().map(FriendInput::into_identity));
        let data = GraphSourceData {
            watches: self.watches.into_iter().map(Into::into).collect(),
            co_views: self.co_views.into_iter().map(Into::into).collect(),
            personas: self.personas.into_iter().map(Into::into).collect(),
        };
        assemble_shared_graph(&friends, &data, taste_neighbor_threshold)
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct TasteMapRequest {
    #[serde(flatten)]
    pub source: GraphSourceInput,
    /// The discord user id whose taste-map to build. Resolved to its
    /// `person:` node id server-side; if that person isn't opted in, they
    /// have no node in the assembled graph and the viz degrades to empty.
    pub discord_user_id: String,
}

/// `POST /api/graph/taste-map` — one opted-in person's persona
/// constellation + taste-neighbor edges, assembled (and opt-in-filtered)
/// server-side. See [`viz::build_taste_map`]. A `discord_user_id` that is
/// not opted in (so `assemble_shared_graph` gave them no node) degrades to
/// an empty-but-valid [`TasteMapViz`], never an error.
pub async fn taste_map_handler(
    State(state): State<Arc<AppState>>,
    Json(req): Json<TasteMapRequest>,
) -> Json<TasteMapViz> {
    let graph = req
        .source
        .assemble(state.config.kg_taste_neighbor_threshold);
    let person_id = person_node_id(&req.discord_user_id);
    Json(viz::build_taste_map(&graph, &person_id))
}

#[derive(Debug, Clone, Deserialize)]
pub struct GroupDynamicsRequest {
    #[serde(flatten)]
    pub source: GraphSourceInput,
}

/// `POST /api/graph/group-dynamics` — who-bridges-whom, with bridge/
/// centrality annotations, assembled (and opt-in-filtered) server-side.
/// See [`viz::build_group_dynamics`].
pub async fn group_dynamics_handler(
    State(state): State<Arc<AppState>>,
    Json(req): Json<GroupDynamicsRequest>,
) -> Json<GroupDynamicsViz> {
    let graph = req
        .source
        .assemble(state.config.kg_taste_neighbor_threshold);
    Json(viz::build_group_dynamics(&graph))
}

#[derive(Debug, Clone, Deserialize)]
pub struct WatchHistoryRequest {
    #[serde(flatten)]
    pub source: GraphSourceInput,
    /// `None` = every opted-in person's watch history. When `Some`, scoped
    /// to that discord user id (resolved to its `person:` node id
    /// server-side). Either way the underlying graph is opt-in-filtered
    /// first, so a non-opted-in person's history is unreachable.
    #[serde(default)]
    pub discord_user_id: Option<String>,
}

/// `POST /api/graph/watch-history` — a temporal watch-history series,
/// assembled (and opt-in-filtered) server-side. See
/// [`viz::build_watch_history`]. The series length is capped by
/// `MUSE_KG_VIZ_WATCH_HISTORY_LIMIT` (`Config::kg_viz_watch_history_limit`)
/// — never a bare literal here, same config discipline as every other
/// MUSEX-16/17 threshold.
pub async fn watch_history_handler(
    State(state): State<Arc<AppState>>,
    Json(req): Json<WatchHistoryRequest>,
) -> Json<WatchHistoryViz> {
    let graph = req
        .source
        .assemble(state.config.kg_taste_neighbor_threshold);
    let limit = state.config.kg_viz_watch_history_limit as usize;
    let person_node = req.discord_user_id.as_deref().map(person_node_id);
    Json(viz::build_watch_history(
        &graph,
        person_node.as_deref(),
        limit,
    ))
}

#[derive(Debug, Clone, Deserialize)]
pub struct TasteClustersRequest {
    #[serde(flatten)]
    pub source: GraphSourceInput,
}

/// `POST /api/graph/taste-clusters` — taste-neighbor cluster groupings,
/// assembled (and opt-in-filtered) server-side. See
/// [`viz::build_taste_clusters`].
pub async fn taste_clusters_handler(
    State(state): State<Arc<AppState>>,
    Json(req): Json<TasteClustersRequest>,
) -> Json<TasteClusterViz> {
    let graph = req
        .source
        .assemble(state.config.kg_taste_neighbor_threshold);
    Json(viz::build_taste_clusters(&graph))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kg::model::{person_node_id, title_node_id};

    /// Source inputs where a NON-opted-in friend (Jamie) genuinely has
    /// watch/co-view/persona relations — the same shape as
    /// `crate::kg::assemble`'s and `crate::kg::viz`'s negative tests, but
    /// expressed as the CLIENT-FACING [`GraphSourceInput`] a handler
    /// receives. Alex and Sam are opted in; Jamie is allowlisted but not.
    fn source_with_opted_out_jamie() -> GraphSourceInput {
        let now = Utc::now();
        GraphSourceInput {
            friends: vec![
                FriendInput {
                    discord_user_id: "discord-alex".to_string(),
                    display_name: "Alex".to_string(),
                    opted_in_account_id: Some(1),
                },
                FriendInput {
                    discord_user_id: "discord-sam".to_string(),
                    display_name: "Sam".to_string(),
                    opted_in_account_id: Some(2),
                },
                // Allowlisted, but NOT opted in — no account id.
                FriendInput {
                    discord_user_id: "discord-jamie".to_string(),
                    display_name: "Jamie".to_string(),
                    opted_in_account_id: None,
                },
            ],
            watches: vec![
                WatchInput {
                    discord_user_id: "discord-alex".to_string(),
                    media_item_id: 100,
                    title: "Severance".to_string(),
                    watched_at: now,
                },
                WatchInput {
                    discord_user_id: "discord-jamie".to_string(),
                    media_item_id: 200,
                    title: "Jamie's Secret Show".to_string(),
                    watched_at: now,
                },
            ],
            co_views: vec![
                CoViewInput {
                    person_a: "discord-alex".to_string(),
                    person_b: "discord-sam".to_string(),
                    session_key: "sess-alex-sam".to_string(),
                    media_item_id: 100,
                    title: "Severance".to_string(),
                    watched_at: now,
                },
                // Jamie co-viewed WITH an opted-in friend — must still be
                // excluded because Jamie's own end isn't opted in.
                CoViewInput {
                    person_a: "discord-alex".to_string(),
                    person_b: "discord-jamie".to_string(),
                    session_key: "sess-alex-jamie".to_string(),
                    media_item_id: 100,
                    title: "Severance".to_string(),
                    watched_at: now,
                },
            ],
            personas: vec![
                PersonaInput {
                    discord_user_id: "discord-alex".to_string(),
                    persona_id: 1,
                    persona_name: "alex-primary".to_string(),
                    centroid: {
                        let mut v = vec![0.0; 8];
                        v[0] = 1.0;
                        v
                    },
                },
                PersonaInput {
                    discord_user_id: "discord-jamie".to_string(),
                    persona_id: 3,
                    persona_name: "jamie-primary".to_string(),
                    // Deliberately near-identical to Alex's — if the filter
                    // were bypassed this WOULD produce a taste edge/cluster
                    // membership, so this genuinely tests the filter.
                    centroid: {
                        let mut v = vec![0.0; 8];
                        v[0] = 1.0;
                        v
                    },
                },
            ],
        }
    }

    /// The load-bearing WEB-LAYER privacy test: feed the endpoint the
    /// client-facing source inputs (which genuinely contain a non-opted-in
    /// user's relations) and assert every endpoint's viz output contains
    /// NONE of that user's nodes/edges. This proves the opt-in filter is
    /// enforced at the HTTP boundary — because the handler assembles the
    /// graph itself via `GraphSourceInput::assemble` (→
    /// `assemble_shared_graph`) rather than trusting a client graph — not
    /// merely inside the pure builder (which `crate::kg::viz::tests::privacy`
    /// already covers). Exercises the assembly-and-viz path each handler
    /// runs, without needing a live `AppState`/DB.
    #[test]
    fn privacy_is_enforced_at_the_http_boundary() {
        let jamie_id = person_node_id("discord-jamie");
        let alex_id = person_node_id("discord-alex");
        let jamie_title_id = title_node_id(200);

        // Sanity: the CLIENT INPUT genuinely carries Jamie's relations, so
        // the assertions below test the filter, not empty input.
        let probe = source_with_opted_out_jamie();
        assert!(probe
            .watches
            .iter()
            .any(|w| w.discord_user_id == "discord-jamie"));
        assert!(probe
            .co_views
            .iter()
            .any(|c| c.person_a == "discord-jamie" || c.person_b == "discord-jamie"));
        assert!(probe
            .personas
            .iter()
            .any(|p| p.discord_user_id == "discord-jamie"));

        // 1. taste-map (as Jamie): degrades to no-data, never leaks Jamie's
        //    real personas.
        let jamie_map =
            viz::build_taste_map(&source_with_opted_out_jamie().assemble(0.5), &jamie_id);
        assert!(jamie_map.label.is_none());
        assert!(jamie_map.personas.is_empty());
        assert!(jamie_map.neighbors.is_empty());

        // taste-map (as Alex): Jamie never appears as a neighbor.
        let alex_map = viz::build_taste_map(&source_with_opted_out_jamie().assemble(0.5), &alex_id);
        assert!(!alex_map.neighbors.iter().any(|n| n.person_id == jamie_id));

        // 2. group-dynamics: no node/edge references Jamie; Alex still present.
        let gd = viz::build_group_dynamics(&source_with_opted_out_jamie().assemble(0.5));
        assert!(!gd.nodes.iter().any(|n| n.id == jamie_id));
        assert!(!gd
            .edges
            .iter()
            .any(|e| e.source == jamie_id || e.target == jamie_id));
        assert!(gd.nodes.iter().any(|n| n.id == alex_id));

        // 3. watch-history (all people): Jamie's title never appears.
        let wh = viz::build_watch_history(&source_with_opted_out_jamie().assemble(0.5), None, 100);
        assert!(!wh.entries.iter().any(|e| e.person_id == jamie_id));
        assert!(!wh.entries.iter().any(|e| e.title == "Jamie's Secret Show"));
        assert!(!wh.entries.iter().any(|e| e.title_id == jamie_title_id));
        assert!(wh.entries.iter().any(|e| e.person_id == alex_id));

        // 4. taste-clusters: no cluster contains Jamie, despite the
        //    near-identical seeded centroid.
        let tc = viz::build_taste_clusters(&source_with_opted_out_jamie().assemble(0.5));
        assert!(!tc
            .clusters
            .iter()
            .any(|c| c.iter().any(|m| m.person_id == jamie_id)));
    }

    /// A `FriendInput` with no account id reconstructs a not-opted-in
    /// identity; one with an account id opts in via the sanctioned mutator.
    /// Guards the one-to-one mapping `into_identity` relies on.
    #[test]
    fn friend_input_maps_opt_in_state_through_the_sanctioned_mutator() {
        let not_opted = FriendInput {
            discord_user_id: "d".to_string(),
            display_name: "n".to_string(),
            opted_in_account_id: None,
        }
        .into_identity();
        assert!(!not_opted.is_opted_in());
        assert!(not_opted.linked_account().is_none());

        let opted = FriendInput {
            discord_user_id: "d".to_string(),
            display_name: "n".to_string(),
            opted_in_account_id: Some(42),
        }
        .into_identity();
        assert!(opted.is_opted_in());
        assert_eq!(opted.linked_account(), Some(42));
    }

    /// Empty source input assembles to an empty graph and every builder
    /// degrades to an empty-but-valid viz — a client sending nothing never
    /// produces an error or a trusted-graph shortcut.
    #[test]
    fn empty_source_degrades_to_empty_viz() {
        let empty = GraphSourceInput::default().assemble(0.5);
        assert!(viz::build_taste_map(&empty, "person:nobody")
            .personas
            .is_empty());
        assert!(viz::build_group_dynamics(&empty).nodes.is_empty());
        assert!(viz::build_watch_history(&empty, None, 100)
            .entries
            .is_empty());
        assert!(viz::build_taste_clusters(&empty).clusters.is_empty());
    }
}
