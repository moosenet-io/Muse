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
//! appear in the output. See `tests::privacy_is_enforced_by_the_real_async_handlers`,
//! which drives the actual `async fn` endpoints (not the inner helpers) and
//! asserts both the exclusion of a non-opted-in user and the inclusion of
//! opted-in users across all four responses.
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
    use serde_json::json;

    /// A raw JSON REQUEST BODY (the flattened [`GraphSourceInput`] shape a
    /// client actually POSTs) where a NON-opted-in friend (Jamie) genuinely
    /// has watch/co-view/persona relations. Alex and Sam are opted in
    /// (`opted_in_account_id` present); Jamie is allowlisted but not (the
    /// key is omitted → `None` via `#[serde(default)]`). Returned as JSON —
    /// not a pre-built struct — so each test deserializes it through the
    /// real serde path (including `#[serde(flatten)]`) before handing it to
    /// the actual async handler, exactly as an HTTP request would. Alex's
    /// and Sam's persona centroids are near-identical (they must cluster /
    /// be taste-neighbors); Jamie's is near-identical to Alex's too, so a
    /// bypassed filter WOULD leak Jamie into taste output — making the
    /// exclusion assertions genuine, not vacuous.
    fn source_json_with_opted_out_jamie() -> serde_json::Value {
        json!({
            "friends": [
                {"discord_user_id": "discord-alex", "display_name": "Alex", "opted_in_account_id": 1},
                {"discord_user_id": "discord-sam", "display_name": "Sam", "opted_in_account_id": 2},
                // Allowlisted but NOT opted in — key omitted → None.
                {"discord_user_id": "discord-jamie", "display_name": "Jamie"}
            ],
            "watches": [
                {"discord_user_id": "discord-alex", "media_item_id": 100, "title": "Severance", "watched_at": "2026-07-14T10:00:00Z"},
                {"discord_user_id": "discord-jamie", "media_item_id": 200, "title": "Jamie's Secret Show", "watched_at": "2026-07-14T10:00:00Z"}
            ],
            "co_views": [
                {"person_a": "discord-alex", "person_b": "discord-sam", "session_key": "sess-alex-sam", "media_item_id": 100, "title": "Severance", "watched_at": "2026-07-14T10:00:00Z"},
                // Jamie co-viewed WITH an opted-in friend — must still be
                // excluded because Jamie's own end isn't opted in.
                {"person_a": "discord-alex", "person_b": "discord-jamie", "session_key": "sess-alex-jamie", "media_item_id": 100, "title": "Severance", "watched_at": "2026-07-14T10:00:00Z"}
            ],
            "personas": [
                {"discord_user_id": "discord-alex", "persona_id": 1, "persona_name": "alex-primary", "centroid": [1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0]},
                {"discord_user_id": "discord-sam", "persona_id": 2, "persona_name": "sam-primary", "centroid": [0.98, 0.02, 0.02, 0.02, 0.02, 0.02, 0.02, 0.02]},
                {"discord_user_id": "discord-jamie", "persona_id": 3, "persona_name": "jamie-primary", "centroid": [1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0]}
            ]
        })
    }

    /// Merge a selector field (e.g. `discord_user_id`) into a source-body
    /// JSON object so it deserializes into a request type that flattens
    /// [`GraphSourceInput`] alongside a selector.
    fn with_selector(mut body: serde_json::Value, key: &str, value: &str) -> serde_json::Value {
        body.as_object_mut()
            .expect("source body is a JSON object")
            .insert(key.to_string(), json!(value));
        body
    }

    /// A minimal real [`AppState`] for unit-testing the async handlers
    /// without a live DB. The pool is built with `connect_lazy`, which never
    /// connects until first use — and these graph handlers never touch
    /// `state.pool` at all (they read only `state.config`), so no DB is
    /// required. Same DB-free handler-unit-test pattern as
    /// `crate::channels::routes`'s `compose_handler` test. Uses the default
    /// config (threshold `0.5`, watch-history limit `200`), so the test also
    /// exercises the config-backed values flowing from `State`.
    fn test_state() -> Arc<AppState> {
        let pool = sqlx::postgres::PgPoolOptions::new()
            .connect_lazy("postgres://user:pass@127.0.0.1:1/muse_test_lazy")
            .expect("connect_lazy never fails synchronously");
        let config = crate::config::Config::default();
        Arc::new(AppState {
            pool,
            enrichment: crate::enrichment::EnrichmentService::from_config(&config),
            config,
            plex: None,
            prowlarr: None,
            arr_instances: Vec::new(),
            tmdb: None,
            embed: None,
        })
    }

    /// The load-bearing WEB-LAYER privacy test — it drives the REAL async
    /// handler functions end to end (deserialize a JSON request body → call
    /// the `async fn` with a hand-built `State` → inspect the JSON response),
    /// not the inner `assemble`/`build_*` helpers, so it exercises exactly
    /// the path production traffic takes: `#[serde(flatten)]` request
    /// deserialization, the config-backed threshold/limit pulled from
    /// `State`, the `discord_user_id → person:` selector mapping, and
    /// `Json` response construction. For every one of the four endpoints it
    /// asserts BOTH directions:
    ///   (a) non-opted-in Jamie's nodes/edges/titles appear in NONE of the
    ///       responses (the filter is enforced at the HTTP boundary), and
    ///   (b) the opted-in users' data DOES appear (so it's a real filter,
    ///       not a filter-everything bug).
    #[tokio::test]
    async fn privacy_is_enforced_by_the_real_async_handlers() {
        let jamie_id = person_node_id("discord-jamie");
        let alex_id = person_node_id("discord-alex");
        let sam_id = person_node_id("discord-sam");
        let jamie_title_id = title_node_id(200);
        let state = test_state();

        // Sanity: the raw request body genuinely carries Jamie's relations,
        // so the assertions below test the FILTER, not empty input.
        let probe = source_json_with_opted_out_jamie();
        assert!(probe["watches"]
            .as_array()
            .unwrap()
            .iter()
            .any(|w| w["discord_user_id"] == "discord-jamie"));
        assert!(probe["co_views"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c["person_a"] == "discord-jamie" || c["person_b"] == "discord-jamie"));
        assert!(probe["personas"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p["discord_user_id"] == "discord-jamie"));

        // 1. taste-map AS ALEX, through the real handler.
        let body = with_selector(
            source_json_with_opted_out_jamie(),
            "discord_user_id",
            "discord-alex",
        );
        let req: TasteMapRequest =
            serde_json::from_value(body).expect("taste-map body deserializes (flatten)");
        let Json(alex_map) = taste_map_handler(State(state.clone()), Json(req)).await;
        // (b) inclusion: Alex's own persona + opted-in Sam as a neighbor.
        assert_eq!(alex_map.label.as_deref(), Some("Alex"));
        assert!(alex_map.personas.iter().any(|p| p.label == "alex-primary"));
        assert!(
            alex_map.neighbors.iter().any(|n| n.person_id == sam_id),
            "opted-in Sam must appear as Alex's taste-neighbor: {:?}",
            alex_map.neighbors
        );
        // (a) exclusion: Jamie never a neighbor.
        assert!(!alex_map.neighbors.iter().any(|n| n.person_id == jamie_id));

        // taste-map AS JAMIE → degrades to no-data (never leaks personas).
        let body = with_selector(
            source_json_with_opted_out_jamie(),
            "discord_user_id",
            "discord-jamie",
        );
        let req: TasteMapRequest = serde_json::from_value(body).unwrap();
        let Json(jamie_map) = taste_map_handler(State(state.clone()), Json(req)).await;
        assert!(jamie_map.label.is_none());
        assert!(jamie_map.personas.is_empty());
        assert!(jamie_map.neighbors.is_empty());

        // 2. group-dynamics, through the real handler.
        let req: GroupDynamicsRequest =
            serde_json::from_value(source_json_with_opted_out_jamie()).unwrap();
        let Json(gd) = group_dynamics_handler(State(state.clone()), Json(req)).await;
        // (b) inclusion: opted-in Alex+Sam nodes and their co-view edge.
        assert!(gd.nodes.iter().any(|n| n.id == alex_id));
        assert!(gd.nodes.iter().any(|n| n.id == sam_id));
        assert!(
            gd.edges
                .iter()
                .any(|e| (e.source == alex_id && e.target == sam_id)
                    || (e.source == sam_id && e.target == alex_id)),
            "opted-in Alex<->Sam co-view edge must be present: {:?}",
            gd.edges
        );
        // (a) exclusion: no node/edge references Jamie.
        assert!(!gd.nodes.iter().any(|n| n.id == jamie_id));
        assert!(!gd
            .edges
            .iter()
            .any(|e| e.source == jamie_id || e.target == jamie_id));

        // 3. watch-history (ALL opted-in people), through the real handler.
        let req: WatchHistoryRequest =
            serde_json::from_value(source_json_with_opted_out_jamie()).unwrap();
        let Json(wh) = watch_history_handler(State(state.clone()), Json(req)).await;
        // (b) inclusion: opted-in Alex's watch appears.
        assert!(
            wh.entries
                .iter()
                .any(|e| e.person_id == alex_id && e.title == "Severance"),
            "opted-in Alex's watch must appear: {:?}",
            wh.entries
        );
        // (a) exclusion: none of Jamie's history (by person, title, or id).
        assert!(!wh.entries.iter().any(|e| e.person_id == jamie_id));
        assert!(!wh.entries.iter().any(|e| e.title == "Jamie's Secret Show"));
        assert!(!wh.entries.iter().any(|e| e.title_id == jamie_title_id));

        // watch-history SCOPED to Alex → exercises the selector→node-id
        // mapping in the handler; result must contain only Alex.
        let body = with_selector(
            source_json_with_opted_out_jamie(),
            "discord_user_id",
            "discord-alex",
        );
        let req: WatchHistoryRequest = serde_json::from_value(body).unwrap();
        let Json(wh_alex) = watch_history_handler(State(state.clone()), Json(req)).await;
        assert!(!wh_alex.entries.is_empty());
        assert!(
            wh_alex.entries.iter().all(|e| e.person_id == alex_id),
            "scoped watch-history must contain only Alex: {:?}",
            wh_alex.entries
        );

        // 4. taste-clusters, through the real handler.
        let req: TasteClustersRequest =
            serde_json::from_value(source_json_with_opted_out_jamie()).unwrap();
        let Json(tc) = taste_clusters_handler(State(state.clone()), Json(req)).await;
        // (b) inclusion: opted-in Alex+Sam (near-identical taste) cluster
        //     together.
        let alex_cluster = tc
            .clusters
            .iter()
            .find(|c| c.iter().any(|m| m.person_id == alex_id))
            .expect("Alex must appear in some cluster");
        assert!(
            alex_cluster.iter().any(|m| m.person_id == sam_id),
            "opted-in Alex+Sam must cluster together: {:?}",
            tc.clusters
        );
        // (a) exclusion: no cluster contains Jamie, despite the near-
        //     identical seeded centroid.
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
