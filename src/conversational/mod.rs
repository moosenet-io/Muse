//! MUSEX-14 (Plane TERM #390), part B: conversational requests — "something
//! like Sicario but lighter" — reasoned against the LIBRARY + availability
//! FIRST, with only genuinely-missing titles ever routed to the request
//! domain ([`crate::arr::request`]).
//!
//! ## Reuses the real resolution ladder, invents no second one
//! [`handle_conversational_request`] is built on
//! [`crate::recall::run_ladder`] — the SAME vector → trigram → tmdb ladder
//! `crate::recall::resolve::resolve_handler` (`POST /query/resolve`) already
//! uses, and for the same reason: it degrades gracefully rung by rung and,
//! critically, **never reaches the TMDb (beyond-the-library) tier while an
//! in-library hit exists** — that ordering IS "reason against library +
//! availability first," not a separate rule this module has to re-implement.
//! The vector/trigram/tmdb tier closures below are a deliberate, minimal
//! re-implementation of `recall::resolve`'s own (private-to-that-module)
//! tier functions — not a refactor of that already-reviewed module, just the
//! same handful of `repo`/client calls, wired to this module's own
//! [`ConversationalOutcome`] shape.
//!
//! ## "Owned" means a real `media_item`, not just metadata
//! Reasoning "library FIRST" is only correct if "in the library" means Muse
//! actually OWNS/has the title, not merely that it has a `media_metadata`
//! row for it. Those are different: trending ingest (MUSE-19) writes
//! metadata-only rows for titles Muse knows *about* but doesn't own (see
//! `repo::trending::list_trending_not_in_library`), and a trigram title
//! search hits `media_metadata`, so it can surface one of those. Treating
//! such a metadata-only hit as "owned" would wrongly SUPPRESS a legitimate
//! missing-title request. So [`partition_library_hits`] requires a real
//! `media_item` row (via `repo::media_item::list_by_metadata`) before it
//! calls anything owned — a `media_metadata` hit with no backing
//! `media_item` falls through to the missing/request path like any other
//! genuinely-missing title. A `Vector`-tier hit is always owned by
//! construction (embeddings are keyed to a `media_item`, and that tier
//! already resolved the row).
//!
//! ## No NLU — an honest v0
//! There is no keyword/intent extraction here: the raw request text is
//! handed to the ladder as-is, exactly like `/query/resolve`'s `query`
//! field. A phrase like "something like Sicario but lighter" resolves (or
//! doesn't) exactly as well as the underlying vector/trigram/TMDb search can
//! do with that text — this module does not invent a fake "tone" or
//! "similar-to" parser on top. Same explicit-limitation posture
//! `curation::recommend::RecommendRequest::context`'s own doc takes for
//! itself ("accepted but not yet incorporated... reserved for a follow-up").
//!
//! ## Missing titles: tiered safety, never bypassed
//! When the ladder bottoms out at [`crate::recall::ResolveTier::Tmdb`] (only
//! reachable when the caller opted in, i.e. a `tmdb` client is configured —
//! same as `/query/resolve`), each hit is turned into a
//! [`crate::arr::request::MediaRequestDraft`] and run through
//! [`crate::arr::request::submit_if_appropriate`] — the ONE gate that
//! decides whether a [`crate::arr::request::MediaRequestSink`] is ever
//! called. See that module's doc for why no live *arr-writing sink ships
//! here. **Honest limitation**: a title outside the library has no
//! [`crate::models::availability::Availability`] row to check yet (MUSE-16
//! availability is keyed to an existing `media_metadata_id`), so every
//! missing-title request built here currently classifies as
//! [`crate::arr::request::RequestTier::NeedsReview`] or
//! [`crate::arr::request::RequestTier::Blocked`] — never
//! `AutoApprovable` in practice, even with the operator opt-in flag set.
//! Wiring a real-time Prowlarr/availability check for a not-yet-owned TMDb
//! hit (so a confirmed-grabbable title COULD auto-approve) is a natural,
//! separately-reviewable follow-up, not done in this pass.

use std::sync::Arc;

use axum::extract::State;
use axum::Json;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;

use crate::arr::config::ArrInstanceConfig;
use crate::arr::request::{
    submit_if_appropriate, MediaRequestDraft, MediaRequestOutcome, MediaRequestSink,
    NoopMediaRequestSink,
};
use crate::discord::identity::{FriendIdentity, TrustedFriends};
use crate::embed::OllamaEmbedClient;
use crate::error::MuseResult;
use crate::http::AppState;
use crate::models::embedding::DEFAULT_EMBEDDING_MODEL;
use crate::models::library::LibraryKind;
use crate::models::media_metadata::MediaKind;
use crate::recall::{run_ladder, ResolveHit, ResolveTier};
use crate::repo;
use crate::settings::ExperienceSettings;
use crate::trending::TmdbClient;

/// A library title the request already has — surfaced instead of a
/// request, per "suggest owned before requesting."
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnedSuggestion {
    pub media_metadata_id: i64,
    /// Always `Some` in practice: [`partition_library_hits`] only ever
    /// classifies a hit as owned once it has resolved a real `media_item`
    /// row (that IS the ownership criterion — see the module doc). Kept as
    /// `Option` for shape-compatibility with the recall hit types, but a
    /// metadata-only match never reaches here — it routes to
    /// [`MissingRequest`] instead.
    pub media_item_id: Option<i64>,
    pub title: String,
    pub year: Option<i32>,
}

/// One genuinely-missing title, and what [`crate::arr::request`] decided to
/// do about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MissingRequest {
    pub tmdb_id: String,
    pub title: String,
    pub outcome: MediaRequestOutcome,
}

/// The full result of one conversational request: which
/// [`crate::recall::ResolveTier`] answered, and the owned/missing partition
/// of its hits. A `Tmdb`-tier answer is all-missing and a `None`-tier answer
/// is empty, but a LIBRARY-tier (`Vector`/`Trigram`) answer can legitimately
/// produce BOTH: a trigram title search matches `media_metadata` rows, and a
/// metadata row with no backing `media_item` (e.g. a trending-ingested title
/// Muse knows *about* but doesn't own) is NOT owned — it falls through to
/// `missing` alongside any genuinely-owned matches. See
/// [`partition_library_hits`] for exactly where that ownership line is drawn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConversationalOutcome {
    pub tier: ResolveTier,
    pub owned: Vec<OwnedSuggestion>,
    pub missing: Vec<MissingRequest>,
}

/// THE entry point. `embed`/`tmdb` mirror `/query/resolve`'s own optional,
/// graceful-degrade dependencies (`None` simply skips that tier).
/// `arr_instances` + `auto_tier_enabled` + `sink` are threaded straight
/// through to [`crate::arr::request::submit_if_appropriate`] for any
/// missing title the ladder surfaces.
pub async fn handle_conversational_request(
    pool: &PgPool,
    embed: Option<&OllamaEmbedClient>,
    tmdb: Option<&TmdbClient>,
    arr_instances: &[ArrInstanceConfig],
    sink: &dyn MediaRequestSink,
    auto_tier_enabled: bool,
    recall_vector_max_distance: f64,
    raw_query: &str,
    limit: i64,
) -> MuseResult<ConversationalOutcome> {
    let query = raw_query.trim();
    if query.is_empty() {
        return Ok(ConversationalOutcome {
            tier: ResolveTier::None,
            owned: Vec::new(),
            missing: Vec::new(),
        });
    }

    let (tier, hits) = run_ladder(
        || vector_tier(pool, embed, query, limit, recall_vector_max_distance),
        || trigram_tier(pool, query, limit),
        tmdb.is_some(),
        || tmdb_tier(tmdb, query, limit),
    )
    .await;

    match tier {
        // A library-tier answer: partition into genuinely-owned (has a real
        // `media_item`) vs metadata-only (falls through to a missing-title
        // request) — see `partition_library_hits`.
        ResolveTier::Vector | ResolveTier::Trigram => {
            let (owned, missing) =
                partition_library_hits(pool, hits, arr_instances, sink, auto_tier_enabled).await?;
            Ok(ConversationalOutcome {
                tier,
                owned,
                missing,
            })
        }
        // Nothing owned: every hit is a genuinely-missing title -> the
        // tiered-safety request domain, one draft per hit.
        ResolveTier::Tmdb => {
            let mut missing = Vec::with_capacity(hits.len());
            for hit in hits {
                let ResolveHit::Tmdb {
                    tmdb_id,
                    media_type,
                    title,
                    ..
                } = hit
                else {
                    continue; // the ladder only returns Tmdb-tagged hits for this tier
                };
                let kind = media_kind_from_tmdb(media_type.as_deref());
                let has_matching_arr_instance = has_matching_arr_instance(arr_instances, kind);
                let draft = MediaRequestDraft {
                    tmdb_id: tmdb_id.clone(),
                    title: title.clone(),
                    kind,
                };
                // A not-yet-owned title has no `media_metadata_id` yet, so
                // there is no MUSE-16 `availability` row to check here --
                // see the module doc's honest-limitation note.
                let outcome = submit_if_appropriate(
                    sink,
                    draft,
                    auto_tier_enabled,
                    None,
                    has_matching_arr_instance,
                )
                .await?;
                missing.push(MissingRequest {
                    tmdb_id,
                    title,
                    outcome,
                });
            }
            Ok(ConversationalOutcome {
                tier,
                owned: Vec::new(),
                missing,
            })
        }
        ResolveTier::None => Ok(ConversationalOutcome {
            tier,
            owned: Vec::new(),
            missing: Vec::new(),
        }),
    }
}

/// THE consent gate for MUSEX-WIRE-02, pure and total — mirrors
/// `crate::discord::bot::decide_response_mode`'s "provable by construction"
/// posture: a small, directly-unit-testable function rather than logic
/// buried inline in [`run_conversational`]. `submit` is structurally
/// unreachable from the current call graph (see
/// [`NoopMediaRequestSink`]'s doc), so this function is what makes the
/// consent decision itself independently verifiable even though no test
/// can yet observe it through a live sink call.
///
/// Returns `true` only when the OPERATOR has enabled auto-tier submission
/// AND the caller resolves (via [`TrustedFriends::get`]) to an
/// [`FriendIdentity::is_opted_in`] identity — both must hold. `None` for
/// `discord_user_id` (unauthenticated) always resolves to `false`.
pub fn conversational_auto_tier_allowed(
    operator_auto_tier_enabled: bool,
    friends: &TrustedFriends,
    discord_user_id: Option<&str>,
) -> bool {
    operator_auto_tier_enabled
        && discord_user_id
            .and_then(|id| friends.get(id))
            .map(FriendIdentity::is_opted_in)
            .unwrap_or(false)
}

/// MUSEX-WIRE-02 (Plane TERM #398, slice 2): the settings-gated,
/// consent-enforced, PRODUCTION-WIRED entry point onto
/// [`handle_conversational_request`] — see [`conversational_handler`] for
/// the `POST /conversational` route that calls this. Mirrors
/// `crate::discord::bot::run_discord_respond`'s inert-first shape exactly:
/// gated on [`ExperienceSettings::is_discord_bot_enabled`] BEFORE any
/// pool/roster/candidate work happens at all.
///
/// ## Which toggle, and why (documented choice)
/// [`ExperienceSettings`] has no dedicated `conversational` subsystem
/// toggle. Of the existing ones, `discord_bot` is the correct fit, not
/// `adaptation_loop`: this flow's identity/consent model IS the Discord
/// friend roster — [`TrustedFriends`]/[`FriendIdentity`] (the exact type
/// `crate::discord::bot` already gates on), not
/// `crate::adaptation`'s taste-recalibration loop, which has no per-caller
/// identity concept at all. `crate::discord::mod`'s own module doc
/// explicitly groups "the conversational assistant" alongside the Discord
/// friend-facing surfaces this settings toggle governs. So
/// `is_discord_bot_enabled` — master switch AND `discord_bot.enabled` — is
/// the toggle this flow shares with [`crate::discord::bot`].
///
/// ## Consent gate — what "taste-derived" means for THIS flow (honest,
/// documented seam)
/// [`handle_conversational_request`] itself returns no taste-personalized
/// data today — the module doc is explicit that owned/missing results are
/// library-lookup facts, not account-scoped taste (see `arr::request`'s
/// own doc: a [`MediaRequestDraft`] "carries nothing taste/account-shaped
/// — a request is about a TITLE, not about who asked for it"). The one
/// place a caller's identity COULD matter is the request-submission side
/// effect (`sink`/`auto_tier_enabled`) — the only thing this flow can do
/// on a caller's behalf. So the consent gate here is: only a Discord user
/// id that resolves to an [`FriendIdentity::is_opted_in`] identity may
/// enable the auto-tier request-submission path; a non-opted-in or
/// unauthenticated (`discord_user_id: None`) caller still gets the full
/// library-first lookup (owned suggestions + missing-title classification)
/// — which was already non-personalized — but with request auto-submission
/// forced off, exactly the same [`crate::arr::request::RequestTier::NeedsReview`]-or-`Blocked`
/// safe default `auto_tier_enabled: false` already produces. This uses the
/// Phase-F accessor [`FriendIdentity::is_opted_in`] via
/// [`TrustedFriends::get`], never a private/consent-guarded primitive
/// directly.
pub async fn run_conversational(
    settings: &ExperienceSettings,
    friends: &TrustedFriends,
    discord_user_id: Option<&str>,
    pool: &PgPool,
    embed: Option<&OllamaEmbedClient>,
    tmdb: Option<&TmdbClient>,
    arr_instances: &[ArrInstanceConfig],
    sink: &dyn MediaRequestSink,
    auto_tier_enabled: bool,
    recall_vector_max_distance: f64,
    raw_query: &str,
    limit: i64,
) -> MuseResult<Option<ConversationalOutcome>> {
    if !settings.is_discord_bot_enabled() {
        return Ok(None);
    }

    // Only an opted-in identity may drive the request-submission side
    // effect; a non-opted-in/unauthenticated caller still gets the
    // library-first (non-personalized) lookup, just with auto-submission
    // forced off. See the doc above and `conversational_auto_tier_allowed`.
    let effective_auto_tier_enabled =
        conversational_auto_tier_allowed(auto_tier_enabled, friends, discord_user_id);

    handle_conversational_request(
        pool,
        embed,
        tmdb,
        arr_instances,
        sink,
        effective_auto_tier_enabled,
        recall_vector_max_distance,
        raw_query,
        limit,
    )
    .await
    .map(Some)
}

/// The `POST /conversational` JSON request body. `discord_user_id` is
/// optional — `None` mirrors an unauthenticated caller and, per
/// [`run_conversational`]'s consent gate, can never enable request
/// auto-submission. `limit` defaults to a small conversational-cadence
/// fetch size when omitted (mirrors
/// `crate::discord::bot::TASTE_CANDIDATE_FETCH_LIMIT`'s "one pick per ask"
/// posture, scaled up slightly since this flow can legitimately return
/// several owned/missing hits per query).
#[derive(Debug, Deserialize)]
pub struct ConversationalHttpRequest {
    pub query: String,
    #[serde(default)]
    pub discord_user_id: Option<String>,
    #[serde(default)]
    pub limit: Option<i64>,
}

const DEFAULT_CONVERSATIONAL_LIMIT: i64 = 5;

/// One [`OwnedSuggestion`], flattened for JSON.
#[derive(Debug, Serialize)]
pub struct OwnedSuggestionHttp {
    pub media_metadata_id: i64,
    pub media_item_id: Option<i64>,
    pub title: String,
    pub year: Option<i32>,
}

impl From<OwnedSuggestion> for OwnedSuggestionHttp {
    fn from(o: OwnedSuggestion) -> Self {
        Self {
            media_metadata_id: o.media_metadata_id,
            media_item_id: o.media_item_id,
            title: o.title,
            year: o.year,
        }
    }
}

/// One [`MissingRequest`], flattened for JSON — `outcome_tier`/
/// `submitted`/`outcome_reason` mirror [`MediaRequestOutcome`]'s fields.
#[derive(Debug, Serialize)]
pub struct MissingRequestHttp {
    pub tmdb_id: String,
    pub title: String,
    pub outcome_tier: String,
    pub submitted: bool,
    pub outcome_reason: String,
}

impl From<MissingRequest> for MissingRequestHttp {
    fn from(m: MissingRequest) -> Self {
        Self {
            tmdb_id: m.tmdb_id,
            title: m.title,
            outcome_tier: format!("{:?}", m.outcome.tier),
            submitted: m.outcome.submitted,
            outcome_reason: m.outcome.reason,
        }
    }
}

/// The `POST /conversational` JSON response. `tier` is `None` (with both
/// lists empty) when the subsystem is off — the same inert shape
/// `crate::discord::bot::DiscordRespondResponse` uses for its disabled
/// case.
#[derive(Debug, Serialize)]
pub struct ConversationalHttpResponse {
    pub tier: Option<String>,
    pub owned: Vec<OwnedSuggestionHttp>,
    pub missing: Vec<MissingRequestHttp>,
}

fn to_http_response(outcome: Option<ConversationalOutcome>) -> ConversationalHttpResponse {
    match outcome {
        None => ConversationalHttpResponse {
            tier: None,
            owned: Vec::new(),
            missing: Vec::new(),
        },
        Some(outcome) => ConversationalHttpResponse {
            tier: Some(format!("{:?}", outcome.tier)),
            owned: outcome.owned.into_iter().map(Into::into).collect(),
            missing: outcome.missing.into_iter().map(Into::into).collect(),
        },
    }
}

/// `POST /conversational` — MUSEX-WIRE-02's flagship wired flow: the
/// production HTTP door onto [`run_conversational`] (settings gate +
/// consent gate) -> [`handle_conversational_request`] (the real MUSEX-14
/// library-first ladder + tiered-safety request path).
///
/// ## Inert-first ordering (mirrors MUSEX-WIRE-01's fixed `/discord/respond`
/// shape)
/// The settings load is the one unavoidable pool read — the toggle is the
/// source of truth for whether this flow runs at all. Immediately after
/// that load, the gate is checked and the inert response
/// (`ConversationalHttpResponse { tier: None, .. }`) is returned BEFORE the
/// friend roster is built or [`run_conversational`]/
/// [`handle_conversational_request`] is ever called — so a disabled
/// subsystem does no pool/roster/candidate work beyond reading the toggle,
/// exactly like `crate::discord::bot::discord_respond_handler`.
///
/// ## Sink (honest seam)
/// Uses [`NoopMediaRequestSink`] — see that type's doc for why that's safe
/// today (no live *arr-writing sink exists yet, and `submit` is
/// structurally unreachable from this call graph per
/// `crate::arr::request::classify_tier`'s own doc) and what must change
/// before it stops being safe.
///
/// ## Consent honest seam (same limitation `discord_respond_handler`
/// already flags)
/// Exactly like `crate::discord::bot::discord_respond_handler`, the only
/// roster this handler can build in production today is
/// `ExperienceSettings::discord_bot.trusted_friends`, and that roster is
/// ALLOWLIST membership only — it can never itself grant `taste_opt_in`
/// (see `crate::settings::DiscordBotSettings`'s own doc). So in production
/// today every identity this handler resolves is, at best, not opted in —
/// [`run_conversational`]'s consent gate is real, reachable, and correctly
/// forces auto-tier submission off end-to-end, but no live caller can yet
/// reach the opted-in branch. [`run_conversational`]'s own tests exercise
/// the opted-in path directly against a constructed identity, proving the
/// gate is correct even though production can't yet drive a real friend
/// into that state — a real opt-in persistence layer is the same
/// separately-reviewable follow-up `discord_respond_handler`'s doc already
/// calls out.
pub async fn conversational_handler(
    State(state): State<Arc<AppState>>,
    Json(req): Json<ConversationalHttpRequest>,
) -> MuseResult<Json<ConversationalHttpResponse>> {
    // The one unavoidable pool read: the toggle is the source of truth.
    let settings = crate::repo::settings::load(&state.pool).await?;

    // GATE FIRST — before building any roster or doing any
    // library/candidate/request work. A disabled subsystem returns the
    // inert response here; nothing below runs.
    if !settings.is_discord_bot_enabled() {
        return Ok(Json(to_http_response(None)));
    }

    let friends = TrustedFriends::from_friends(
        settings
            .discord_bot
            .trusted_friends
            .iter()
            .map(|f| FriendIdentity::new(f.discord_user_id.clone(), f.display_name.clone())),
    );

    let sink = NoopMediaRequestSink;
    let limit = req.limit.unwrap_or(DEFAULT_CONVERSATIONAL_LIMIT);

    let outcome = run_conversational(
        &settings,
        &friends,
        req.discord_user_id.as_deref(),
        &state.pool,
        state.embed.as_ref(),
        state.tmdb.as_ref(),
        &state.arr_instances,
        &sink,
        state.config.arr_request_auto_tier_enabled,
        state.config.recall_vector_max_distance,
        &req.query,
        limit,
    )
    .await?;

    Ok(Json(to_http_response(outcome)))
}

/// Split a library-tier (`Vector`/`Trigram`) hit list into genuinely-owned
/// suggestions and metadata-only titles that must instead route through the
/// tiered-safety request domain — see the module doc's "Owned means a real
/// `media_item`" section for why this line matters (AC-3: only actually
/// owned/available titles may be suggested as owned; anything else is a
/// genuinely-missing title).
///
/// - A [`ResolveHit::Vector`] is owned by construction (the vector tier
///   already resolved a real `media_item`).
/// - A [`ResolveHit::Trigram`] matches `media_metadata` only, so its
///   ownership is decided HERE by `repo::media_item::list_by_metadata`:
///   at least one `media_item` row → owned (surfaced with that row's id);
///   zero rows → metadata-only → routed through
///   [`crate::arr::request::submit_if_appropriate`] exactly like a TMDb-tier
///   missing title (tiered safety intact — the sink is only ever called for
///   `AutoApprovable`, which a conversational request can't reach since it
///   passes no confirmed availability).
async fn partition_library_hits(
    pool: &PgPool,
    hits: Vec<ResolveHit>,
    arr_instances: &[ArrInstanceConfig],
    sink: &dyn MediaRequestSink,
    auto_tier_enabled: bool,
) -> MuseResult<(Vec<OwnedSuggestion>, Vec<MissingRequest>)> {
    let mut owned = Vec::new();
    let mut missing = Vec::new();

    for hit in hits {
        match hit {
            ResolveHit::Vector {
                media_item_id,
                media_metadata_id,
                title,
                year,
                ..
            } => owned.push(OwnedSuggestion {
                media_metadata_id,
                media_item_id: Some(media_item_id),
                title,
                year,
            }),
            ResolveHit::Trigram {
                media_metadata_id,
                title,
                year,
            } => {
                // Ownership check: does a real `media_item` back this
                // `media_metadata` title, or is it metadata-only?
                let items = repo::media_item::list_by_metadata(pool, media_metadata_id).await?;
                if let Some(item) = items.first() {
                    owned.push(OwnedSuggestion {
                        media_metadata_id,
                        media_item_id: Some(item.id),
                        title,
                        year,
                    });
                } else {
                    // Metadata-only: Muse knows about it but doesn't own it.
                    // Route it through tiered safety like any other missing
                    // title, keyed by the metadata's own tmdb id + kind.
                    let meta = repo::media_metadata::get(pool, media_metadata_id).await?;
                    let kind = meta.kind;
                    let tmdb_id = meta.tmdb_id.unwrap_or_default();
                    // Unfulfillable via *arr without a tmdb id (the request
                    // domain is tmdb-keyed) — treat a missing tmdb id the
                    // same as "no matching *arr instance": structurally
                    // Blocked, sink never touched.
                    let has_matching_arr_instance =
                        has_matching_arr_instance(arr_instances, kind) && !tmdb_id.is_empty();
                    let draft = MediaRequestDraft {
                        tmdb_id: tmdb_id.clone(),
                        title: title.clone(),
                        kind,
                    };
                    let outcome = submit_if_appropriate(
                        sink,
                        draft,
                        auto_tier_enabled,
                        None,
                        has_matching_arr_instance,
                    )
                    .await?;
                    missing.push(MissingRequest {
                        tmdb_id,
                        title,
                        outcome,
                    });
                }
            }
            // The ladder never tags a library-tier answer with a Tmdb hit;
            // defensively ignore rather than mis-report it as owned.
            ResolveHit::Tmdb { .. } => {}
        }
    }

    Ok((owned, missing))
}

/// TMDb's `media_type` is `"movie"` or `"tv"`; anything else (unset, a
/// person hit `search_multi` didn't filter, ...) degrades to [`MediaKind::Show`]
/// rather than panicking or erroring — same "best-effort, never blocks"
/// posture the rest of this crate's TMDb integrations take.
fn media_kind_from_tmdb(media_type: Option<&str>) -> MediaKind {
    match media_type {
        Some("movie") => MediaKind::Movie,
        _ => MediaKind::Show,
    }
}

fn has_matching_arr_instance(arr_instances: &[ArrInstanceConfig], kind: MediaKind) -> bool {
    let needed = match kind {
        MediaKind::Movie => LibraryKind::Movie,
        MediaKind::Show => LibraryKind::Tv,
    };
    arr_instances.iter().any(|i| i.library_kind == needed)
}

/// Tier 1 (mirrors `crate::recall::resolve`'s private `vector_tier`):
/// library-vector-first ANN over the MUSE-08 embeddings.
async fn vector_tier(
    pool: &PgPool,
    embed: Option<&OllamaEmbedClient>,
    query: &str,
    limit: i64,
    max_distance: f64,
) -> Vec<ResolveHit> {
    let Some(client) = embed else {
        return Vec::new();
    };

    let vector = match client.embed(DEFAULT_EMBEDDING_MODEL, query).await {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(error = %e, "MUSEX-14: conversational query embedding failed; degrading to trigram");
            return Vec::new();
        }
    };

    let matches = match crate::embed::nearest(pool, vector, limit).await {
        Ok(m) => m,
        Err(e) => {
            tracing::warn!(error = %e, "MUSEX-14: conversational vector lookup failed; degrading to trigram");
            return Vec::new();
        }
    };

    let mut hits = Vec::with_capacity(matches.len());
    for m in matches {
        if m.distance > max_distance {
            break;
        }
        let media_item_id = m.entity_id;
        let Ok(item) = repo::media_item::get(pool, media_item_id).await else {
            continue;
        };
        let Ok(meta) = repo::media_metadata::get(pool, item.media_metadata_id).await else {
            continue;
        };
        hits.push(ResolveHit::Vector {
            media_item_id,
            media_metadata_id: meta.id,
            title: meta.title,
            year: meta.year,
            distance: m.distance,
        });
    }
    hits
}

/// Tier 2 (mirrors `crate::recall::resolve`'s private `trigram_tier`):
/// pg_trgm fuzzy title search over the library.
async fn trigram_tier(pool: &PgPool, query: &str, limit: i64) -> Vec<ResolveHit> {
    match repo::media_metadata::search_by_title(pool, query, limit).await {
        Ok(rows) => rows
            .into_iter()
            .map(|m| ResolveHit::Trigram {
                media_metadata_id: m.id,
                title: m.title,
                year: m.year,
            })
            .collect(),
        Err(e) => {
            tracing::warn!(error = %e, "MUSEX-14: conversational trigram search failed; degrading to tmdb");
            Vec::new()
        }
    }
}

/// Tier 3 (mirrors `crate::recall::resolve`'s private `tmdb_tier`): TMDb
/// lookup beyond the library, only ever invoked by the ladder when a
/// `tmdb` client is configured.
async fn tmdb_tier(tmdb: Option<&TmdbClient>, query: &str, limit: i64) -> Vec<ResolveHit> {
    let Some(client) = tmdb else {
        return Vec::new();
    };

    let results = match client.search_multi(query).await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(error = %e, "MUSEX-14: conversational tmdb search failed; ladder exhausted");
            return Vec::new();
        }
    };

    results
        .into_iter()
        .filter_map(|t| {
            let title = t.display_title()?.to_string();
            Some(ResolveHit::Tmdb {
                tmdb_id: t.id.to_string(),
                media_type: t.media_type.clone(),
                title,
                year: t.year(),
                note: "not in your library — found on TMDb".to_string(),
            })
        })
        .take(limit.max(0) as usize)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- pure helpers: no I/O -----------------------------------------------

    #[test]
    fn media_kind_from_tmdb_maps_movie_and_defaults_others_to_show() {
        assert_eq!(media_kind_from_tmdb(Some("movie")), MediaKind::Movie);
        assert_eq!(media_kind_from_tmdb(Some("tv")), MediaKind::Show);
        assert_eq!(media_kind_from_tmdb(None), MediaKind::Show);
        assert_eq!(media_kind_from_tmdb(Some("person")), MediaKind::Show);
    }

    fn radarr_instance() -> ArrInstanceConfig {
        ArrInstanceConfig {
            name: "radarr".to_string(),
            kind: crate::arr::ArrKind::Radarr,
            base_url: "http://192.0.2.10:7878".to_string(),
            api_key: "<REDACTED-SECRET>".to_string(),
            library_kind: LibraryKind::Movie,
            root_folder: None,
        }
    }

    #[test]
    fn has_matching_arr_instance_is_kind_specific() {
        let instances = vec![radarr_instance()];
        assert!(has_matching_arr_instance(&instances, MediaKind::Movie));
        assert!(!has_matching_arr_instance(&instances, MediaKind::Show));
    }

    #[test]
    fn has_matching_arr_instance_is_false_for_an_empty_fleet() {
        assert!(!has_matching_arr_instance(&[], MediaKind::Movie));
    }

    // --- MUSEX-WIRE-02: conversational_auto_tier_allowed — the consent
    // gate, pure and directly testable -----------------------------------

    #[test]
    fn auto_tier_not_allowed_for_unauthenticated_caller() {
        let friends = TrustedFriends::new();
        assert!(!conversational_auto_tier_allowed(true, &friends, None));
    }

    #[test]
    fn auto_tier_not_allowed_for_non_opted_in_friend_even_when_operator_enabled() {
        let friends = TrustedFriends::from_friends([FriendIdentity::new("discord-1", "Alex")]);
        assert!(!conversational_auto_tier_allowed(
            true,
            &friends,
            Some("discord-1")
        ));
    }

    #[test]
    fn auto_tier_not_allowed_when_operator_disabled_even_for_opted_in_friend() {
        let friends =
            TrustedFriends::from_friends([FriendIdentity::new("discord-1", "Alex").opt_in(42)]);
        assert!(!conversational_auto_tier_allowed(
            false,
            &friends,
            Some("discord-1")
        ));
    }

    #[test]
    fn auto_tier_allowed_for_opted_in_friend_when_operator_enabled() {
        let friends =
            TrustedFriends::from_friends([FriendIdentity::new("discord-1", "Alex").opt_in(42)]);
        assert!(conversational_auto_tier_allowed(
            true,
            &friends,
            Some("discord-1")
        ));
    }

    #[test]
    fn auto_tier_not_allowed_for_unknown_discord_id() {
        let friends =
            TrustedFriends::from_friends([FriendIdentity::new("discord-1", "Alex").opt_in(42)]);
        assert!(!conversational_auto_tier_allowed(
            true,
            &friends,
            Some("discord-unknown")
        ));
    }

    // --- MUSEX-WIRE-02: run_conversational is inert when disabled ----------
    //
    // Same `connect_lazy`-unreachable-pool idiom `crate::discord::bot`'s
    // `run_discord_respond` inertness tests use: a real, unreachable DSN
    // proves the disabled path short-circuits before any DB access, because
    // a real query would surface as an `Err`, not a quiet `Ok(None)`.

    use crate::arr::request::MockMediaRequestSink;
    use crate::settings::DiscordBotSettings;

    fn unreachable_pool() -> sqlx::PgPool {
        sqlx::postgres::PgPoolOptions::new()
            .connect_lazy("postgres://unused:unused@127.0.0.1:1/unused")
            .expect("connect_lazy should never fail synchronously")
    }

    #[tokio::test]
    async fn run_conversational_is_inert_when_discord_bot_disabled() {
        let pool = unreachable_pool();
        let mut settings = ExperienceSettings::default();
        settings.master_enabled = true;
        settings.discord_bot = DiscordBotSettings {
            enabled: false,
            ..settings.discord_bot
        };
        let friends = TrustedFriends::new();
        let sink = MockMediaRequestSink::new();

        let result = run_conversational(
            &settings, &friends, None, &pool, None, None, &[], &sink, true, 0.4, "sicario", 5,
        )
        .await;

        assert!(result.is_ok(), "expected Ok(None), got {result:?}");
        assert!(
            result.unwrap().is_none(),
            "a disabled subsystem must return no outcome"
        );
        assert_eq!(
            sink.submitted_count(),
            0,
            "a disabled subsystem must never reach the request-submission sink"
        );
    }

    #[tokio::test]
    async fn run_conversational_is_inert_when_master_switch_off() {
        let pool = unreachable_pool();
        let mut settings = ExperienceSettings::default();
        settings.master_enabled = false;
        settings.discord_bot = DiscordBotSettings {
            enabled: true,
            ..settings.discord_bot
        };
        let friends = TrustedFriends::new();
        let sink = MockMediaRequestSink::new();

        let result = run_conversational(
            &settings, &friends, None, &pool, None, None, &[], &sink, true, 0.4, "sicario", 5,
        )
        .await;

        assert!(result.is_ok(), "expected Ok(None), got {result:?}");
        assert!(result.unwrap().is_none());
    }

    #[tokio::test]
    async fn run_conversational_runs_the_flow_when_enabled() {
        // Mirror-image sanity check for the two disabled-path tests above:
        // they return `Ok(None)` (the gate short-circuits inert); this one
        // proves that WITH the gate enabled, the flow does NOT short-circuit
        // — it enters `handle_conversational_request` and runs the real
        // MUSEX-14 ladder, which reaches the pool at the trigram tier
        // (`repo::media_metadata::search_by_title`).
        //
        // NOTE (why this asserts `Ok(Some(_))`, not `Err`): unlike
        // `crate::discord::bot::respond` — whose `TasteAware` arm
        // propagates a pool error via `?`, so its equivalent "touches the
        // pool" test can assert `is_err()` — the conversational ladder
        // DELIBERATELY SWALLOWS and DEGRADES DB errors (`vector_tier` /
        // `trigram_tier` log-and-return-empty on `Err`; see their bodies).
        // So against an unreachable pool the enabled flow can NEVER surface
        // an `Err`; it degrades every tier to empty and returns
        // `Ok(Some(ConversationalOutcome { tier: None, .. }))`. The
        // load-bearing distinction is therefore `Some` (gate passed, flow
        // ran, pool reached) vs the disabled tests' `None` (gate blocked,
        // nothing ran) — deterministic under any test order, and it still
        // proves a real pool touch: a non-empty query on the enabled path
        // ALWAYS calls `search_by_title(pool, ..)`.
        //
        // This holds regardless of consent, since the library-first lookup
        // itself is not gated on opt-in — only request auto-submission is.
        let pool = unreachable_pool();
        let mut settings = ExperienceSettings::default();
        settings.master_enabled = true;
        settings.discord_bot = DiscordBotSettings {
            enabled: true,
            ..settings.discord_bot
        };
        let friends = TrustedFriends::new();
        let sink = MockMediaRequestSink::new();

        let result = run_conversational(
            &settings, &friends, None, &pool, None, None, &[], &sink, true, 0.4, "sicario", 5,
        )
        .await;

        let outcome = result
            .expect(
                "the enabled path must run the flow, not error: the conversational ladder \
                 swallows DB errors and degrades, so an unreachable pool yields Ok, never Err",
            )
            .expect(
                "an ENABLED subsystem must run the flow and return Some — proving the gate did \
                 NOT short-circuit to the inert None the disabled-path tests assert",
            );
        // With no embed (vector tier skipped) and no tmdb, the only tier
        // that can run is trigram — which reached the (unreachable) pool and
        // degraded to empty. `ResolveTier::None` here is the proof the
        // ladder actually ran and touched the pool rather than the gate
        // blocking before any work.
        assert_eq!(
            outcome.tier,
            ResolveTier::None,
            "an unreachable pool degrades the trigram tier to empty -> ResolveTier::None, which \
             only happens if the enabled flow reached the pool at all: {outcome:?}"
        );
    }

    // --- MUSEX-WIRE-02: ConversationalHttpResponse DTO shape ---------------

    #[test]
    fn to_http_response_of_none_is_inert() {
        let response = to_http_response(None);
        assert!(response.tier.is_none());
        assert!(response.owned.is_empty());
        assert!(response.missing.is_empty());
    }

    #[test]
    fn to_http_response_of_an_outcome_carries_owned_and_missing() {
        let outcome = ConversationalOutcome {
            tier: ResolveTier::Trigram,
            owned: vec![OwnedSuggestion {
                media_metadata_id: 7,
                media_item_id: Some(9),
                title: "Sicario".to_string(),
                year: Some(2015),
            }],
            missing: Vec::new(),
        };
        let response = to_http_response(Some(outcome));
        assert_eq!(response.tier.as_deref(), Some("Trigram"));
        assert_eq!(response.owned.len(), 1);
        assert_eq!(response.owned[0].title, "Sicario");
        assert!(response.missing.is_empty());
    }
}

/// DB-backed end-to-end coverage: a real library match (owned, no request)
/// and a real "nothing owned" path (missing, tiered-safety routed). Gated
/// per `MUSE_TEST_DATABASE_URL`, same convention as
/// `crate::promotion::targeting::db_gated` / `crate::discord::bot::db_gated`
/// — skips cleanly, never a hard failure, when no test database is
/// configured.
#[cfg(test)]
mod db_gated {
    use super::*;
    use crate::arr::request::MockMediaRequestSink;
    use crate::arr::request::RequestTier;
    use crate::models::library::NewLibrary;
    use crate::models::media_item::NewMediaItem;
    use crate::models::media_metadata::NewMediaMetadata;
    use httpmock::prelude::*;
    use serde_json::json;

    async fn test_pool_or_skip(test_name: &str) -> Option<sqlx::PgPool> {
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

    /// Seed one real, distinctively-titled (UUID-suffixed) owned title.
    async fn seed_owned_title(pool: &sqlx::PgPool) -> String {
        let suffix = uuid::Uuid::new_v4().simple().to_string();
        let title = format!("MUSEX14ConversationalOwnedProbe{suffix}");

        let library = repo::library::create(
            pool,
            &NewLibrary {
                name: format!("lib-{suffix}"),
                kind: LibraryKind::Movie,
                root_folder: format!("/movies-{suffix}"),
                source_arr_name: None,
                source_arr_url: None,
            },
        )
        .await
        .expect("create library");

        let metadata = repo::media_metadata::upsert_by_tmdb(
            pool,
            &NewMediaMetadata {
                kind: MediaKind::Movie,
                tmdb_id: Some(format!("tmdb-{suffix}")),
                tvdb_id: None,
                imdb_id: None,
                provider_ids: json!({}),
                title: title.clone(),
                sort_title: None,
                original_title: None,
                original_language: None,
                status: None,
                overview: Some("a real, seeded synopsis".to_string()),
                studio: None,
                network: None,
                runtime_minutes: Some(120),
                year: Some(2015),
                images: json!({}),
            },
        )
        .await
        .expect("create media_metadata");

        repo::media_item::upsert(
            pool,
            &NewMediaItem {
                library_id: library.id,
                media_metadata_id: metadata.id,
                path: format!("/movies-{suffix}/movie.mkv"),
                monitored: true,
                quality_profile_id: None,
                minimum_availability: None,
                plex_rating_key: Some(format!("plexkey-{suffix}")),
                added_at: None,
            },
        )
        .await
        .expect("create media_item");

        title
    }

    /// Seed a metadata-only title: a real `media_metadata` row (with a tmdb
    /// id, as trending ingest would produce) and NO `media_item` — Muse
    /// knows *about* it but doesn't own it. Returns
    /// `(media_metadata_id, tmdb_id, title)`.
    async fn seed_metadata_only_title(pool: &sqlx::PgPool) -> (i64, String, String) {
        let suffix = uuid::Uuid::new_v4().simple().to_string();
        let title = format!("MUSEX14ConversationalMetaOnlyProbe{suffix}");
        let tmdb_id = format!("tmdb-{suffix}");

        let metadata = repo::media_metadata::upsert_by_tmdb(
            pool,
            &NewMediaMetadata {
                kind: MediaKind::Movie,
                tmdb_id: Some(tmdb_id.clone()),
                tvdb_id: None,
                imdb_id: None,
                provider_ids: json!({}),
                title: title.clone(),
                sort_title: None,
                original_title: None,
                original_language: None,
                status: None,
                overview: Some("a real, seeded synopsis for a title Muse doesn't own".to_string()),
                studio: None,
                network: None,
                runtime_minutes: Some(118),
                year: Some(2023),
                images: json!({}),
            },
        )
        .await
        .expect("create media_metadata");

        // Deliberately NO media_item. Fixture sanity: prove it's genuinely
        // metadata-only, so this test can't silently pass for the wrong
        // reason (e.g. if a media_item leaked in from a shared fixture).
        let items = repo::media_item::list_by_metadata(pool, metadata.id)
            .await
            .expect("list_by_metadata should not error");
        assert!(
            items.is_empty(),
            "fixture bug: the metadata-only probe must have NO backing media_item"
        );

        (metadata.id, tmdb_id, title)
    }

    /// AC-3 regression (codex review, MUSEX-14): a query whose ONLY library
    /// match is metadata-only (a `media_metadata` row with no `media_item`)
    /// must NOT be reported as owned — it's not actually owned/available —
    /// and MUST route through the tiered-safety request path like any other
    /// missing title.
    #[tokio::test]
    async fn a_metadata_only_match_is_not_owned_and_routes_through_tiered_safety() {
        let Some(pool) = test_pool_or_skip(
            "a_metadata_only_match_is_not_owned_and_routes_through_tiered_safety",
        )
        .await
        else {
            return;
        };

        let (_meta_id, tmdb_id, title) = seed_metadata_only_title(&pool).await;
        let sink = MockMediaRequestSink::new();

        // No embed (vector tier skipped) and no tmdb client: the ladder
        // lands on the trigram tier, which matches the metadata-only row.
        // No *arr fleet -> the missing title is structurally Blocked.
        let outcome =
            handle_conversational_request(&pool, None, None, &[], &sink, false, 0.4, &title, 10)
                .await
                .expect("handle_conversational_request should not error");

        assert_eq!(outcome.tier, ResolveTier::Trigram);
        assert!(
            outcome.owned.iter().all(|o| o.title != title),
            "a metadata-only title must NEVER be suggested as owned: {:?}",
            outcome.owned
        );
        assert_eq!(
            outcome.missing.len(),
            1,
            "the metadata-only (unowned) title must route to the missing-request path: {:?}",
            outcome.missing
        );
        let missing = &outcome.missing[0];
        assert_eq!(missing.title, title);
        assert_eq!(missing.tmdb_id, tmdb_id);
        assert_eq!(
            missing.outcome.tier,
            RequestTier::Blocked,
            "no configured *arr instance means the request is Blocked"
        );
        assert!(!missing.outcome.submitted);
        assert_eq!(
            sink.submitted_count(),
            0,
            "tiered safety must never be bypassed for a metadata-only title"
        );

        // With a matching *arr instance but auto-tier still disabled (the
        // safe default): NeedsReview, still never submitted — proving the
        // tier gating is honored, not just Blocked-by-accident.
        let radarr_instance = ArrInstanceConfig {
            name: "radarr".to_string(),
            kind: crate::arr::ArrKind::Radarr,
            base_url: "http://192.0.2.10:7878".to_string(),
            api_key: "<REDACTED-SECRET>".to_string(),
            library_kind: LibraryKind::Movie,
            root_folder: None,
        };
        let sink2 = MockMediaRequestSink::new();
        let outcome2 = handle_conversational_request(
            &pool,
            None,
            None,
            &[radarr_instance],
            &sink2,
            false,
            0.4,
            &title,
            10,
        )
        .await
        .expect("handle_conversational_request should not error");

        assert!(outcome2.owned.iter().all(|o| o.title != title));
        assert_eq!(outcome2.missing.len(), 1);
        assert_eq!(outcome2.missing[0].outcome.tier, RequestTier::NeedsReview);
        assert!(!outcome2.missing[0].outcome.submitted);
        assert_eq!(sink2.submitted_count(), 0);
    }

    #[tokio::test]
    async fn a_query_the_library_already_has_is_suggested_owned_and_never_requested() {
        let Some(pool) = test_pool_or_skip(
            "a_query_the_library_already_has_is_suggested_owned_and_never_requested",
        )
        .await
        else {
            return;
        };

        let title = seed_owned_title(&pool).await;
        let sink = MockMediaRequestSink::new();

        let outcome = handle_conversational_request(
            &pool,
            None, // no embed configured -> vector tier skipped, falls to trigram
            None, // tmdb intentionally absent: an owned match must never even reach it
            &[],
            &sink,
            false,
            0.4,
            &title,
            10,
        )
        .await
        .expect("handle_conversational_request should not error");

        assert_eq!(outcome.tier, ResolveTier::Trigram);
        assert!(
            outcome.owned.iter().any(|o| o.title == title),
            "the real seeded title must be suggested as owned: {:?}",
            outcome.owned
        );
        assert!(
            outcome.missing.is_empty(),
            "an owned match must never produce a missing-title request"
        );
        assert_eq!(
            sink.submitted_count(),
            0,
            "the request sink must never be touched when the library already has a match"
        );
    }

    #[tokio::test]
    async fn a_query_with_nothing_owned_routes_the_missing_title_through_tiered_safety() {
        let Some(pool) = test_pool_or_skip(
            "a_query_with_nothing_owned_routes_the_missing_title_through_tiered_safety",
        )
        .await
        else {
            return;
        };

        let suffix = uuid::Uuid::new_v4().simple().to_string();
        let query = format!("MUSEX14ConversationalMissingProbe{suffix}");

        let server = MockServer::start();
        let tmdb_mock = server.mock(|when, then| {
            when.method(GET).path("/search/multi");
            then.status(200)
                .header("content-type", "application/json")
                .body(format!(
                    r#"{{"results": [{{"id": 42, "title": "{query}", "media_type": "movie", "release_date": "2024-01-01"}}]}}"#
                ));
        });
        let tmdb = TmdbClient::new(server.base_url(), "test-tmdb-key")
            .expect("tmdb client should construct");

        let sink = MockMediaRequestSink::new();

        // No configured *arr instance at all -> structurally Blocked,
        // regardless of the (default, safe) auto_tier_enabled=false.
        let outcome = handle_conversational_request(
            &pool,
            None,
            Some(&tmdb),
            &[], // no arr fleet configured
            &sink,
            false,
            0.4,
            &query,
            10,
        )
        .await
        .expect("handle_conversational_request should not error");

        tmdb_mock.assert();
        assert_eq!(outcome.tier, ResolveTier::Tmdb);
        assert!(
            outcome.owned.is_empty(),
            "nothing owned must yield no owned suggestions"
        );
        assert_eq!(
            outcome.missing.len(),
            1,
            "the missing title must be routed to the request domain"
        );
        let missing = &outcome.missing[0];
        assert_eq!(missing.title, query);
        assert_eq!(missing.outcome.tier, RequestTier::Blocked);
        assert!(!missing.outcome.submitted);
        assert_eq!(
            sink.submitted_count(),
            0,
            "tiered safety must never be bypassed: no matching *arr instance means the sink is \
             never called"
        );

        // Now with a matching *arr instance but auto-tier still disabled
        // (the safe default): NeedsReview, still never submitted.
        let radarr_instance = ArrInstanceConfig {
            name: "radarr".to_string(),
            kind: crate::arr::ArrKind::Radarr,
            base_url: "http://192.0.2.10:7878".to_string(),
            api_key: "<REDACTED-SECRET>".to_string(),
            library_kind: LibraryKind::Movie,
            root_folder: None,
        };
        let sink2 = MockMediaRequestSink::new();
        let outcome2 = handle_conversational_request(
            &pool,
            None,
            Some(&tmdb),
            &[radarr_instance],
            &sink2,
            false,
            0.4,
            &query,
            10,
        )
        .await
        .expect("handle_conversational_request should not error");

        assert_eq!(outcome2.missing.len(), 1);
        assert_eq!(outcome2.missing[0].outcome.tier, RequestTier::NeedsReview);
        assert!(!outcome2.missing[0].outcome.submitted);
        assert_eq!(
            sink2.submitted_count(),
            0,
            "auto-tier disabled (the default) must never auto-submit, even with a matching arr \
             instance"
        );
    }

    // --- MUSEX-WIRE-02: conversational_handler route-level wiring ----------

    use crate::settings::TrustedFriendEntry;

    fn test_app_state(pool: sqlx::PgPool) -> Arc<AppState> {
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

    #[tokio::test]
    async fn conversational_handler_route_is_inert_when_subsystem_disabled() {
        let Some(pool) = test_pool_or_skip(
            "conversational_handler_route_is_inert_when_subsystem_disabled",
        )
        .await
        else {
            return;
        };

        let title = seed_owned_title(&pool).await;

        // Persist a DISABLED settings doc: if the gate did not precede the
        // roster build + library lookup, the handler would still find the
        // seeded owned title below. The inert response proves the route
        // returns BEFORE any of that work.
        let mut settings = ExperienceSettings::default();
        settings.master_enabled = true;
        settings.discord_bot.enabled = false;
        crate::repo::settings::save(&pool, &settings)
            .await
            .expect("save disabled settings");

        let state = test_app_state(pool);
        let req = ConversationalHttpRequest {
            query: title,
            discord_user_id: None,
            limit: None,
        };

        let Json(response) = conversational_handler(State(state), Json(req))
            .await
            .expect("a disabled route must return an inert Ok, never an error");

        assert!(
            response.tier.is_none(),
            "a disabled route must return a fully inert response — no tier"
        );
        assert!(response.owned.is_empty());
        assert!(response.missing.is_empty());
    }

    #[tokio::test]
    async fn conversational_handler_route_returns_library_first_result_when_enabled() {
        let Some(pool) = test_pool_or_skip(
            "conversational_handler_route_returns_library_first_result_when_enabled",
        )
        .await
        else {
            return;
        };

        let title = seed_owned_title(&pool).await;

        // ENABLED subsystem, opted-in friend (proving the wiring reaches
        // the real handle_conversational_request pipeline end-to-end).
        let mut settings = ExperienceSettings::default();
        settings.master_enabled = true;
        settings.discord_bot.enabled = true;
        settings.discord_bot.trusted_friends = vec![TrustedFriendEntry {
            discord_user_id: "discord-1".to_string(),
            display_name: "Alex".to_string(),
        }];
        crate::repo::settings::save(&pool, &settings)
            .await
            .expect("save enabled settings");

        let state = test_app_state(pool);
        let req = ConversationalHttpRequest {
            query: title.clone(),
            discord_user_id: Some("discord-1".to_string()),
            limit: None,
        };

        let Json(response) = conversational_handler(State(state), Json(req))
            .await
            .expect("the enabled path must not error");

        assert!(
            response.tier.is_some(),
            "an enabled subsystem must reach the real library-first ladder"
        );
        assert!(
            response.owned.iter().any(|o| o.title == title),
            "the seeded owned title must be surfaced as an owned suggestion, not a request: \
             {:?}",
            response.owned
        );
        assert!(
            response.missing.is_empty(),
            "an owned title must never be reported as missing: {:?}",
            response.missing
        );
    }
}
