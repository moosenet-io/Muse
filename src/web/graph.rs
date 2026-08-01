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
//! The fix, implemented then: **no handler accepts a pre-assembled
//! `KgGraph`.** Every handler accepts only the SOURCE inputs
//! `assemble_shared_graph` needs — a [`TrustedFriends`] allowlist plus the
//! raw watch/co-view/persona source records — and then BUILDS the graph
//! server-side through [`assemble_shared_graph`] before running any viz
//! builder.
//!
//! ## MUSEX-CAP-SEC-02 (Plane TERM #399, finding 2): consent is now
//! resolved SERVER-SIDE, not client-supplied
//! The MUSEX-17 fix above closed the "client posts a pre-assembled graph"
//! gap, but left a second, more subtle one: [`GraphSourceInput::assemble`]
//! reconstructed the [`TrustedFriends`] allowlist's OPT-IN state from the
//! client's own [`FriendInput::opted_in_account_id`] field. A caller could
//! mark any `discord_user_id` as `opted_in_account_id: Some(_)` in the
//! request body and receive that person's watch/co-view/persona data —
//! "opt-in-by-construction" only holds if whatever constructs the
//! `TrustedFriends` allowlist is itself trustworthy, and a client-supplied
//! JSON field is not.
//!
//! Since MUSEX-WIRE-05 there is a PERSISTED, server-authoritative opt-in
//! store (`crate::repo::friend_opt_in`, `migrations/0103_friend_opt_in.sql`)
//! and a sanctioned resolver (`crate::discord::roster::resolve_trusted_friends`)
//! that turns a persisted row into a [`FriendIdentity`] via the same
//! `FriendIdentity::new` + `FriendIdentity::opt_in` path production code
//! already uses. [`GraphSourceInput::assemble`] now uses that store
//! directly — one `repo::friend_opt_in::get` lookup per client-referenced
//! `discord_user_id` — instead of trusting the client's claim:
//! [`FriendInput::opted_in_account_id`] is DESERIALIZED (so a legacy/naive
//! client body still parses) but is **never read** when deciding opt-in
//! state; only the persisted row decides. A client claiming
//! `opted_in_account_id` for someone who has no persisted opted-in row (or
//! whose row has `opted_in = false`) gets NONE of that person's nodes,
//! edges, or viz output — `assemble_shared_graph`'s existing opt-in filter
//! (unchanged) does the stripping, it now just receives a
//! server-resolved `TrustedFriends` instead of a client-constructed one.
//! This is defense-in-depth on top of MUSEX-CAP-SEC-01's endpoint auth:
//! even an authenticated caller cannot see a non-consented user's graph.
//! See `tests::persisted_opt_in_governs_client_cannot_inflate_the_opted_in_set`
//! (db_gated) for the load-bearing proof, and
//! `tests::privacy_is_enforced_by_the_real_async_handlers` (updated to the
//! server-authoritative model) for the non-DB shape of the same guarantee.
//!
//! ## Why not `resolve_trusted_friends` directly
//! `crate::discord::roster::resolve_trusted_friends` additionally gates on
//! `ExperienceSettings.discord_bot.trusted_friends` (the Discord-bot
//! allowlist) before it will even look up a persisted opt-in row — that is
//! the right gate for the Discord bot surface, but it would silently drop
//! any `discord_user_id` the Constellation GUI's caller supplies who isn't
//! also on the bot's separate allowlist, changing this endpoint's shape in
//! a way the capstone finding didn't ask for. `GraphSourceInput::assemble`
//! instead runs the SAME two-step `repo::friend_opt_in::get` →
//! `FriendIdentity::new(..).opt_in(account_id)` resolution
//! `resolve_trusted_friends` uses, scoped to the `discord_user_id`s the
//! client's `friends` list names (the person SELECTOR, not the opt-in
//! DECISION) — the consent decision is still exclusively server-side.
//!
//! ## Source provenance (unchanged, documented)
//! Nothing in this crate persists a live in-memory `TrustedFriends`
//! allowlist keyed by an HTTP session, so these endpoints still receive
//! their raw watch/co-view/persona SOURCE records in the request body
//! rather than resolving them from a DB pool (that remains real future
//! work) — but the opt-in state that gates which of those source records
//! survive into the graph is, as of MUSEX-CAP-SEC-02, always a server-side
//! lookup. An empty/absent source degrades to an empty-but-valid viz, never
//! an error and never a trusted client graph.

use std::sync::Arc;

use axum::{extract::State, Json};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use sqlx::PgPool;

use crate::discord::identity::{FriendIdentity, TrustedFriends};
use crate::error::MuseResult;
use crate::http::AppState;
use crate::kg::assemble::{
    assemble_shared_graph, CoViewRecord, GraphSourceData, PersonaRecord, WatchRecord,
};
use crate::kg::model::{person_node_id, KgGraph};
use crate::kg::viz::{self, GroupDynamicsViz, TasteClusterViz, TasteMapViz, WatchHistoryViz};
use crate::repo;

/// One friend the client names for a graph request — a person SELECTOR,
/// not a consent grant. `discord_user_id`/`display_name` are used as-is
/// (a display name carries no consent semantics, same posture
/// `crate::discord::identity`'s module doc gives `FriendIdentity::display_name`).
///
/// `opted_in_account_id` is still accepted for request-body compatibility
/// but is **DELIBERATELY IGNORED** by [`GraphSourceInput::assemble`] as of
/// MUSEX-CAP-SEC-02 — seeing this field is not what makes a person
/// opted-in; a persisted `repo::friend_opt_in` row is. Kept `#[allow(dead_code)]`-free
/// (it's read by `serde` and by the one test guarding that it truly has no
/// effect) rather than removed outright, so a client that still sends the
/// old shape doesn't fail to deserialize.
#[derive(Debug, Clone, Deserialize)]
pub struct FriendInput {
    pub discord_user_id: String,
    pub display_name: String,
    /// Accepted, but ignored — see the struct doc and the module doc's
    /// MUSEX-CAP-SEC-02 section. NEVER consulted when deciding opt-in
    /// state.
    #[serde(default)]
    pub opted_in_account_id: Option<i64>,
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

/// The SOURCE inputs every graph endpoint accepts — the person selector
/// (`friends`, naming who to consider) plus the raw records — deliberately
/// NOT a pre-assembled [`KgGraph`] and, as of MUSEX-CAP-SEC-02, NOT a
/// client-supplied opt-in decision either. Every field defaults to empty
/// so a caller can send only what a given visualization needs (e.g.
/// group-dynamics can omit `personas`). [`Self::assemble`] is the ONLY way
/// this becomes a graph, and it always runs the opt-in filter against the
/// SERVER's persisted consent store.
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
    ///
    /// MUSEX-CAP-SEC-02: the [`TrustedFriends`] allowlist's OPT-IN state is
    /// resolved from the persisted `repo::friend_opt_in` store, one lookup
    /// per `discord_user_id` the client's `friends` list names — mirroring
    /// `crate::discord::roster::resolve_trusted_friends`'s resolution
    /// (`repo::friend_opt_in::get` → sanctioned `FriendIdentity::new(..)
    /// .opt_in(account_id)` only when a row exists AND `opted_in = true`
    /// AND a `muse_account_id` is linked), without also gating on the
    /// Discord-bot's separate `trusted_friends` config allowlist (see the
    /// module doc's "Why not `resolve_trusted_friends` directly"). The
    /// client's [`FriendInput::opted_in_account_id`] is NEVER read here.
    /// `taste_neighbor_threshold` comes from config, never a bare literal.
    async fn assemble(self, pool: &PgPool, taste_neighbor_threshold: f32) -> MuseResult<KgGraph> {
        let mut resolved = Vec::with_capacity(self.friends.len());
        for f in &self.friends {
            let identity = FriendIdentity::new(f.discord_user_id.clone(), f.display_name.clone());
            let opted_in_identity = match repo::friend_opt_in::get(pool, &f.discord_user_id).await?
            {
                Some(row) if row.opted_in => row
                    .muse_account_id
                    .map(|account_id| identity.clone().opt_in(account_id)),
                _ => None,
            };
            resolved.push(opted_in_identity.unwrap_or(identity));
        }
        let friends = TrustedFriends::from_friends(resolved);

        let data = GraphSourceData {
            watches: self.watches.into_iter().map(Into::into).collect(),
            co_views: self.co_views.into_iter().map(Into::into).collect(),
            personas: self.personas.into_iter().map(Into::into).collect(),
        };
        Ok(assemble_shared_graph(
            &friends,
            &data,
            taste_neighbor_threshold,
        ))
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
/// constellation + taste-neighbor edges, assembled (and opt-in-filtered,
/// server-side per MUSEX-CAP-SEC-02) server-side. See
/// [`viz::build_taste_map`]. A `discord_user_id` that is not persisted-
/// opted-in (so `assemble_shared_graph` gave them no node) degrades to an
/// empty-but-valid [`TasteMapViz`], never an error.
pub async fn taste_map_handler(
    State(state): State<Arc<AppState>>,
    Json(req): Json<TasteMapRequest>,
) -> MuseResult<Json<TasteMapViz>> {
    // MUSEX-CAP-SEC-04 (epic-capstone finding): inert-first — the KG-viz
    // subsystem must not run when its toggle (or the master switch) is off.
    // Read settings first and return an empty viz before any graph assembly
    // touches the pool, exactly as the WIRE-01..06 handlers gate their work.
    if !repo::settings::load(&state.pool).await?.is_kg_viz_enabled() {
        return Ok(Json(TasteMapViz::default()));
    }
    let graph = req
        .source
        .assemble(&state.pool, state.config.kg_taste_neighbor_threshold)
        .await?;
    let person_id = person_node_id(&req.discord_user_id);
    Ok(Json(viz::build_taste_map(&graph, &person_id)))
}

#[derive(Debug, Clone, Deserialize)]
pub struct GroupDynamicsRequest {
    #[serde(flatten)]
    pub source: GraphSourceInput,
}

/// `POST /api/graph/group-dynamics` — who-bridges-whom, with bridge/
/// centrality annotations, assembled (and opt-in-filtered, server-side per
/// MUSEX-CAP-SEC-02) server-side. See [`viz::build_group_dynamics`].
pub async fn group_dynamics_handler(
    State(state): State<Arc<AppState>>,
    Json(req): Json<GroupDynamicsRequest>,
) -> MuseResult<Json<GroupDynamicsViz>> {
    // MUSEX-CAP-SEC-04: inert-first KG-viz toggle gate (see `taste_map_handler`).
    if !repo::settings::load(&state.pool).await?.is_kg_viz_enabled() {
        return Ok(Json(GroupDynamicsViz::default()));
    }
    let graph = req
        .source
        .assemble(&state.pool, state.config.kg_taste_neighbor_threshold)
        .await?;
    Ok(Json(viz::build_group_dynamics(&graph)))
}

#[derive(Debug, Clone, Deserialize)]
pub struct WatchHistoryRequest {
    #[serde(flatten)]
    pub source: GraphSourceInput,
    /// `None` = every opted-in person's watch history. When `Some`, scoped
    /// to that discord user id (resolved to its `person:` node id
    /// server-side). Either way the underlying graph is opt-in-filtered
    /// first (server-side per MUSEX-CAP-SEC-02), so a non-opted-in
    /// person's history is unreachable.
    #[serde(default)]
    pub discord_user_id: Option<String>,
}

/// `POST /api/graph/watch-history` — a temporal watch-history series,
/// assembled (and opt-in-filtered, server-side per MUSEX-CAP-SEC-02)
/// server-side. See [`viz::build_watch_history`]. The series length is
/// capped by `MUSE_KG_VIZ_WATCH_HISTORY_LIMIT`
/// (`Config::kg_viz_watch_history_limit`) — never a bare literal here,
/// same config discipline as every other MUSEX-16/17 threshold.
pub async fn watch_history_handler(
    State(state): State<Arc<AppState>>,
    Json(req): Json<WatchHistoryRequest>,
) -> MuseResult<Json<WatchHistoryViz>> {
    // MUSEX-CAP-SEC-04: inert-first KG-viz toggle gate (see `taste_map_handler`).
    if !repo::settings::load(&state.pool).await?.is_kg_viz_enabled() {
        return Ok(Json(WatchHistoryViz::default()));
    }
    let graph = req
        .source
        .assemble(&state.pool, state.config.kg_taste_neighbor_threshold)
        .await?;
    let limit = state.config.kg_viz_watch_history_limit as usize;
    let person_node = req.discord_user_id.as_deref().map(person_node_id);
    Ok(Json(viz::build_watch_history(
        &graph,
        person_node.as_deref(),
        limit,
    )))
}

#[derive(Debug, Clone, Deserialize)]
pub struct TasteClustersRequest {
    #[serde(flatten)]
    pub source: GraphSourceInput,
}

/// `POST /api/graph/taste-clusters` — taste-neighbor cluster groupings,
/// assembled (and opt-in-filtered, server-side per MUSEX-CAP-SEC-02)
/// server-side. See [`viz::build_taste_clusters`].
pub async fn taste_clusters_handler(
    State(state): State<Arc<AppState>>,
    Json(req): Json<TasteClustersRequest>,
) -> MuseResult<Json<TasteClusterViz>> {
    // MUSEX-CAP-SEC-04: inert-first KG-viz toggle gate (see `taste_map_handler`).
    if !repo::settings::load(&state.pool).await?.is_kg_viz_enabled() {
        return Ok(Json(TasteClusterViz::default()));
    }
    let graph = req
        .source
        .assemble(&state.pool, state.config.kg_taste_neighbor_threshold)
        .await?;
    Ok(Json(viz::build_taste_clusters(&graph)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kg::model::{person_node_id, title_node_id};
    use serde_json::json;

    /// A raw JSON REQUEST BODY (the flattened [`GraphSourceInput`] shape a
    /// client actually POSTs) where a NON-opted-in friend (Jamie) genuinely
    /// has watch/co-view/persona relations, AND — the MUSEX-CAP-SEC-02
    /// adversarial shape — Jamie's `opted_in_account_id` is set as if the
    /// CLIENT is claiming Jamie opted in. No persisted `friend_opt_in` row
    /// backs that claim (this fn doesn't touch a DB), so the server-side
    /// resolver in [`GraphSourceInput::assemble`] must still treat Jamie as
    /// not opted in — the whole point of this fixture is that the client's
    /// claim alone is never sufficient. Alex and Sam are also given
    /// `opted_in_account_id` (also un-persisted, also irrelevant with a
    /// lazy/unconnected pool) — see `test_state`'s doc for why every one of
    /// these client-side claims must be ignored, and see the db_gated test
    /// below for the version with a REAL persisted store proving inclusion.
    fn source_json_with_opted_out_jamie() -> serde_json::Value {
        json!({
            "friends": [
                {"discord_user_id": "discord-alex", "display_name": "Alex", "opted_in_account_id": 1},
                {"discord_user_id": "discord-sam", "display_name": "Sam", "opted_in_account_id": 2},
                // Client claims Jamie is opted in too — MUST be ignored.
                {"discord_user_id": "discord-jamie", "display_name": "Jamie", "opted_in_account_id": 3}
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

    /// A minimal real [`AppState`] for unit-testing the async handlers. The
    /// pool is built with `connect_lazy`, which never connects until first
    /// use. As of MUSEX-CAP-SEC-02 the graph handlers DO query
    /// `repo::friend_opt_in` (via `state.pool`), so a test driving them
    /// through this lazy, never-actually-connected pool would error the
    /// first time a query runs — which is exactly why
    /// `privacy_is_enforced_by_the_real_async_handlers` below no longer
    /// exercises the real handlers end-to-end without a DB: the
    /// server-authoritative resolution genuinely needs one. This state
    /// constructor stays for the (non-DB-touching) config-shape assertions
    /// tests still use.
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
            download: None,
            cast_controller: None,
        })
    }

    /// `FriendInput::opted_in_account_id` is deserialized (compat) but must
    /// never influence identity construction outside of
    /// [`GraphSourceInput::assemble`]'s server-side resolution — this just
    /// guards that the field itself still parses cleanly, independent of
    /// any consent decision (the consent decision is exercised in the
    /// db_gated tests below since it now genuinely requires the DB).
    #[test]
    fn friend_input_still_deserializes_the_now_ignored_opted_in_field() {
        let v = json!({"discord_user_id": "d", "display_name": "n", "opted_in_account_id": 42});
        let parsed: FriendInput = serde_json::from_value(v).expect("still deserializes");
        assert_eq!(parsed.discord_user_id, "d");
        assert_eq!(parsed.opted_in_account_id, Some(42));

        let v_omitted = json!({"discord_user_id": "d", "display_name": "n"});
        let parsed_omitted: FriendInput =
            serde_json::from_value(v_omitted).expect("field is optional");
        assert_eq!(parsed_omitted.opted_in_account_id, None);
    }

    /// Empty source input assembles to an empty graph (no `friends` means
    /// no `repo::friend_opt_in` lookups at all, so this never touches the
    /// lazy/unconnected pool) and every builder degrades to an
    /// empty-but-valid viz.
    #[tokio::test]
    async fn empty_source_degrades_to_empty_viz() {
        let state = test_state();
        let empty = GraphSourceInput::default()
            .assemble(&state.pool, 0.5)
            .await
            .expect("no friends means no DB lookups occur");
        assert!(viz::build_taste_map(&empty, "person:nobody")
            .personas
            .is_empty());
        assert!(viz::build_group_dynamics(&empty).nodes.is_empty());
        assert!(viz::build_watch_history(&empty, None, 100)
            .entries
            .is_empty());
        assert!(viz::build_taste_clusters(&empty).clusters.is_empty());
    }

    /// Sanity check on the adversarial fixture itself: it genuinely carries
    /// Jamie's relations AND a client-side opt-in claim for Jamie, so the
    /// db_gated proof test below exercises the filter, not empty/absent
    /// input.
    #[test]
    fn fixture_carries_jamies_relations_and_a_client_side_opt_in_claim() {
        let probe = source_json_with_opted_out_jamie();
        assert_eq!(probe["friends"][2]["discord_user_id"], "discord-jamie");
        assert_eq!(probe["friends"][2]["opted_in_account_id"], 3);
        assert!(probe["watches"]
            .as_array()
            .unwrap()
            .iter()
            .any(|w| w["discord_user_id"] == "discord-jamie"));
        assert!(probe["personas"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p["discord_user_id"] == "discord-jamie"));
    }

    #[cfg(test)]
    mod db_gated {
        use super::*;

        async fn test_pool_or_skip(test_name: &str) -> Option<PgPool> {
            let Ok(database_url) = std::env::var("MUSE_TEST_DATABASE_URL") else {
                eprintln!(
                    "MUSE_TEST_DATABASE_URL not set — skipping {test_name} \
                     (expected in the default test run; this harness does not \
                     require a live DB)"
                );
                return None;
            };
            let pool = sqlx::postgres::PgPoolOptions::new()
                .max_connections(5)
                .connect(&database_url)
                .await
                .expect("connect to MUSE_TEST_DATABASE_URL");
            sqlx::migrate!("./migrations")
                .run(&pool)
                .await
                .expect("migrations should apply cleanly");
            Some(pool)
        }

        async fn seed_account(pool: &PgPool, label: &str) -> i64 {
            let row: (i64,) = sqlx::query_as(
                "INSERT INTO accounts (username, friendly_name, is_home_user, is_primary) \
                 VALUES ($1, $2, false, false) RETURNING id",
            )
            .bind(format!("capsec02-{label}-{}", uuid_ish()))
            .bind(format!("CAP-SEC-02 {label} Account"))
            .fetch_one(pool)
            .await
            .expect("seed account");
            row.0
        }

        fn uuid_ish() -> u128 {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system clock")
                .as_nanos()
        }

        /// MUSEX-CAP-SEC-04: `kg_viz.enabled` defaults `false` (opt-in,
        /// default-private), so the graph handlers' new inert-first toggle
        /// gate would return an empty viz on a fresh DB. Persist an ENABLED
        /// settings document so these consent tests exercise the real
        /// assembly/filter path; the gate itself (disabled → inert) is proven
        /// separately by `kg_viz_disabled_returns_inert_empty_viz`.
        async fn enable_kg_viz(pool: &PgPool) {
            let mut settings = crate::settings::ExperienceSettings::default();
            settings.kg_viz.enabled = true; // master_enabled already true by default
            repo::settings::save(pool, &settings)
                .await
                .expect("persist kg-viz-enabled settings");
        }

        fn state_with_pool(pool: PgPool) -> Arc<AppState> {
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
                download: None,
                cast_controller: None,
            })
        }

        /// THE LOAD-BEARING PROOF (MUSEX-CAP-SEC-02): drives the real async
        /// handlers end to end against a REAL persisted `friend_opt_in`
        /// store. Persist opt-in for Alex only; the request body — via
        /// [`source_json_with_opted_out_jamie`] — claims BOTH Alex AND
        /// Jamie are opted in (`opted_in_account_id` set for both). If the
        /// server were still trusting the client's claim (the pre-CAP-SEC-02
        /// bug), Jamie's data would leak into every response. It must not:
        /// only Alex's (persisted-opted-in) data may appear; Jamie's
        /// (client-claimed-only) data must be absent from all four
        /// endpoints, mirroring WIRE-05's own proof-test shape
        /// (`resolve_trusted_friends`'s `db_gated` tests).
        #[tokio::test]
        async fn persisted_opt_in_governs_client_cannot_inflate_the_opted_in_set() {
            let Some(pool) = test_pool_or_skip(
                "persisted_opt_in_governs_client_cannot_inflate_the_opted_in_set",
            )
            .await
            else {
                return;
            };

            // Unique discord ids per run so repeated CI runs never collide.
            let suffix = uuid_ish();
            let alex_discord = format!("discord-capsec02-alex-{suffix}");
            let sam_discord = format!("discord-capsec02-sam-{suffix}");
            let jamie_discord = format!("discord-capsec02-jamie-{suffix}");

            // Persist opt-in for Alex and Sam ONLY. Jamie gets NO row at
            // all — the client will still claim Jamie is opted in below.
            let alex_account = seed_account(&pool, "alex").await;
            let sam_account = seed_account(&pool, "sam").await;
            repo::friend_opt_in::set_opt_in(&pool, &alex_discord, alex_account)
                .await
                .expect("persist Alex's opt-in");
            repo::friend_opt_in::set_opt_in(&pool, &sam_discord, sam_account)
                .await
                .expect("persist Sam's opt-in");

            let jamie_title = format!("Jamie's Secret Show {suffix}");
            let body = json!({
                "friends": [
                    // Client claims opted_in_account_id for ALL THREE — only
                    // Alex/Sam's claims are actually backed by a persisted row.
                    {"discord_user_id": alex_discord, "display_name": "Alex", "opted_in_account_id": alex_account},
                    {"discord_user_id": sam_discord, "display_name": "Sam", "opted_in_account_id": sam_account},
                    {"discord_user_id": jamie_discord, "display_name": "Jamie", "opted_in_account_id": 999999},
                ],
                "watches": [
                    {"discord_user_id": alex_discord, "media_item_id": 100, "title": "Severance", "watched_at": "2026-07-14T10:00:00Z"},
                    {"discord_user_id": jamie_discord, "media_item_id": 200, "title": jamie_title, "watched_at": "2026-07-14T10:00:00Z"}
                ],
                "co_views": [
                    {"person_a": alex_discord, "person_b": sam_discord, "session_key": "sess-alex-sam", "media_item_id": 100, "title": "Severance", "watched_at": "2026-07-14T10:00:00Z"},
                    {"person_a": alex_discord, "person_b": jamie_discord, "session_key": "sess-alex-jamie", "media_item_id": 100, "title": "Severance", "watched_at": "2026-07-14T10:00:00Z"}
                ],
                "personas": [
                    {"discord_user_id": alex_discord, "persona_id": 1, "persona_name": "alex-primary", "centroid": [1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0]},
                    {"discord_user_id": sam_discord, "persona_id": 2, "persona_name": "sam-primary", "centroid": [0.98, 0.02, 0.02, 0.02, 0.02, 0.02, 0.02, 0.02]},
                    {"discord_user_id": jamie_discord, "persona_id": 3, "persona_name": "jamie-primary", "centroid": [1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0]}
                ]
            });

            let jamie_id = person_node_id(&jamie_discord);
            let alex_id = person_node_id(&alex_discord);
            let sam_id = person_node_id(&sam_discord);
            let jamie_title_id = title_node_id(200);
            enable_kg_viz(&pool).await; // MUSEX-CAP-SEC-04: gate is on by default-off
            let state = state_with_pool(pool);

            // 1. taste-map AS ALEX.
            let req_body = with_selector(body.clone(), "discord_user_id", &alex_discord);
            let req: TasteMapRequest =
                serde_json::from_value(req_body).expect("taste-map body deserializes");
            let Json(alex_map) = taste_map_handler(State(state.clone()), Json(req))
                .await
                .expect("handler succeeds");
            assert_eq!(alex_map.label.as_deref(), Some("Alex"));
            assert!(
                alex_map.neighbors.iter().any(|n| n.person_id == sam_id),
                "persisted-opted-in Sam must appear as Alex's taste-neighbor: {:?}",
                alex_map.neighbors
            );
            assert!(
                !alex_map.neighbors.iter().any(|n| n.person_id == jamie_id),
                "client-claimed-only Jamie must NEVER appear despite opted_in_account_id in the request"
            );

            // taste-map AS JAMIE (client-claimed opt-in, no persisted row)
            // → degrades to no-data.
            let req_body = with_selector(body.clone(), "discord_user_id", &jamie_discord);
            let req: TasteMapRequest = serde_json::from_value(req_body).unwrap();
            let Json(jamie_map) = taste_map_handler(State(state.clone()), Json(req))
                .await
                .expect("handler succeeds");
            assert!(jamie_map.label.is_none());
            assert!(jamie_map.personas.is_empty());

            // 2. group-dynamics.
            let req: GroupDynamicsRequest = serde_json::from_value(body.clone()).unwrap();
            let Json(gd) = group_dynamics_handler(State(state.clone()), Json(req))
                .await
                .expect("handler succeeds");
            assert!(gd.nodes.iter().any(|n| n.id == alex_id));
            assert!(gd.nodes.iter().any(|n| n.id == sam_id));
            assert!(
                !gd.nodes.iter().any(|n| n.id == jamie_id),
                "Jamie must not appear as a node despite the client's claim: {:?}",
                gd.nodes
            );
            assert!(!gd
                .edges
                .iter()
                .any(|e| e.source == jamie_id || e.target == jamie_id));

            // 3. watch-history (ALL opted-in people).
            let req: WatchHistoryRequest = serde_json::from_value(body.clone()).unwrap();
            let Json(wh) = watch_history_handler(State(state.clone()), Json(req))
                .await
                .expect("handler succeeds");
            assert!(
                wh.entries
                    .iter()
                    .any(|e| e.person_id == alex_id && e.title == "Severance"),
                "persisted-opted-in Alex's watch must appear: {:?}",
                wh.entries
            );
            assert!(!wh.entries.iter().any(|e| e.person_id == jamie_id));
            assert!(!wh.entries.iter().any(|e| e.title_id == jamie_title_id));

            // 4. taste-clusters.
            let req: TasteClustersRequest = serde_json::from_value(body).unwrap();
            let Json(tc) = taste_clusters_handler(State(state), Json(req))
                .await
                .expect("handler succeeds");
            let alex_cluster = tc
                .clusters
                .iter()
                .find(|c| c.iter().any(|m| m.person_id == alex_id))
                .expect("Alex must appear in some cluster");
            assert!(
                alex_cluster.iter().any(|m| m.person_id == sam_id),
                "persisted-opted-in Alex+Sam must cluster together: {:?}",
                tc.clusters
            );
            assert!(!tc
                .clusters
                .iter()
                .any(|c| c.iter().any(|m| m.person_id == jamie_id)));
        }

        /// The flip side of the load-bearing proof: a friend with NO
        /// client-supplied `opted_in_account_id` at all (the field omitted
        /// entirely) but a REAL persisted opt-in row must still be
        /// included — the server-side lookup grants inclusion on its own,
        /// independent of anything the client sends.
        #[tokio::test]
        async fn persisted_opt_in_grants_inclusion_even_with_no_client_side_claim() {
            let Some(pool) = test_pool_or_skip(
                "persisted_opt_in_grants_inclusion_even_with_no_client_side_claim",
            )
            .await
            else {
                return;
            };

            let suffix = uuid_ish();
            let solo_discord = format!("discord-capsec02-solo-{suffix}");
            let account = seed_account(&pool, "solo").await;
            repo::friend_opt_in::set_opt_in(&pool, &solo_discord, account)
                .await
                .expect("persist opt-in");

            let body = json!({
                "friends": [
                    // No opted_in_account_id key at all.
                    {"discord_user_id": solo_discord, "display_name": "Solo"}
                ],
                "watches": [
                    {"discord_user_id": solo_discord, "media_item_id": 300, "title": "Alone Together", "watched_at": "2026-07-14T10:00:00Z"}
                ],
                "co_views": [],
                "personas": []
            });

            let solo_id = person_node_id(&solo_discord);
            enable_kg_viz(&pool).await; // MUSEX-CAP-SEC-04: gate is on by default-off
            let state = state_with_pool(pool);
            let req: WatchHistoryRequest = serde_json::from_value(body).unwrap();
            let Json(wh) = watch_history_handler(State(state), Json(req))
                .await
                .expect("handler succeeds");
            assert!(
                wh.entries
                    .iter()
                    .any(|e| e.person_id == solo_id && e.title == "Alone Together"),
                "a persisted-opted-in friend must be included with no client-side claim: {:?}",
                wh.entries
            );
        }

        /// MUSEX-CAP-SEC-04 (epic-capstone finding): inert-first — the KG-viz
        /// handlers must return an EMPTY viz when the `kg_viz` toggle (or the
        /// master switch) is off, before any graph assembly runs. This is the
        /// proof: identical persisted-opted-in data to
        /// `persisted_opt_in_grants_inclusion_even_with_no_client_side_claim`
        /// (Alex opted in, a real watch), but WITHOUT `enable_kg_viz`, so the
        /// default-off toggle governs. Where the enabled tests above return a
        /// populated viz, here every handler must return its `Default`
        /// (empty) — so the assertion cannot pass vacuously (the data is
        /// present; only the gate suppresses it).
        #[tokio::test]
        async fn kg_viz_disabled_returns_inert_empty_viz() {
            let Some(pool) = test_pool_or_skip("kg_viz_disabled_returns_inert_empty_viz").await
            else {
                return;
            };

            let suffix = uuid_ish();
            let alex_discord = format!("discord-capsec04-alex-{suffix}");
            let alex_account = seed_account(&pool, "alex").await;
            repo::friend_opt_in::set_opt_in(&pool, &alex_discord, alex_account)
                .await
                .expect("persist Alex's opt-in");

            let body = json!({
                "friends": [
                    {"discord_user_id": alex_discord, "display_name": "Alex", "opted_in_account_id": alex_account},
                ],
                "watches": [
                    {"discord_user_id": alex_discord, "media_item_id": 100, "title": "Severance", "watched_at": "2026-07-14T10:00:00Z"}
                ],
                "co_views": [],
                "personas": [
                    {"discord_user_id": alex_discord, "persona_id": 1, "persona_name": "alex-primary", "centroid": [1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0]}
                ]
            });

            // NOTE: no `enable_kg_viz(&pool)` — `kg_viz.enabled` stays at its
            // default `false`, so `is_kg_viz_enabled()` is false and the gate
            // fires.
            let state = state_with_pool(pool);

            // taste-map AS ALEX — populated in the enabled tests; inert here.
            let req_body = with_selector(body.clone(), "discord_user_id", &alex_discord);
            let req: TasteMapRequest =
                serde_json::from_value(req_body).expect("taste-map body deserializes");
            let Json(alex_map) = taste_map_handler(State(state.clone()), Json(req))
                .await
                .expect("handler succeeds");
            assert_eq!(
                alex_map,
                TasteMapViz::default(),
                "kg_viz disabled must yield an empty taste-map even for opted-in Alex"
            );

            // watch-history — likewise inert.
            let req: WatchHistoryRequest =
                serde_json::from_value(body.clone()).expect("watch-history body deserializes");
            let Json(wh) = watch_history_handler(State(state.clone()), Json(req))
                .await
                .expect("handler succeeds");
            assert_eq!(
                wh,
                WatchHistoryViz::default(),
                "kg_viz disabled must yield an empty watch-history"
            );

            // group-dynamics + taste-clusters — likewise inert.
            let req: GroupDynamicsRequest =
                serde_json::from_value(body.clone()).expect("group-dynamics body deserializes");
            let Json(gd) = group_dynamics_handler(State(state.clone()), Json(req))
                .await
                .expect("handler succeeds");
            assert_eq!(gd, GroupDynamicsViz::default(), "kg_viz disabled → empty");

            let req: TasteClustersRequest =
                serde_json::from_value(body).expect("taste-clusters body deserializes");
            let Json(tc) = taste_clusters_handler(State(state), Json(req))
                .await
                .expect("handler succeeds");
            assert_eq!(tc, TasteClusterViz::default(), "kg_viz disabled → empty");
        }
    }
}
