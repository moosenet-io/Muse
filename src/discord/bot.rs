//! MUSEX-13 (Plane TERM #389): the Discord bot's brain-driven interaction
//! logic. NOT a command table — there is no `match command_name { "recommend"
//! => ..., "watched" => ... }` dispatcher anywhere in this module. Every
//! taste-aware reply is produced by calling the SAME curation code every
//! other Muse surface uses:
//! [`crate::curation::candidates::gather_on_deck_candidates`] +
//! [`crate::curation::recommend::rank_candidates`] +
//! [`crate::curation::recommend::build_rationale`] — the identical pipeline
//! `crate::curation::recommend`'s own `/recommend` HTTP handlers call. This
//! module adds zero new recommendation logic; it only adds the Discord
//! delivery shape ([`crate::discord::client::RichEmbed`]) and the
//! opt-in/allowlist gate below.
//!
//! ## The gate ([`decide_response_mode`]) — load-bearing, proves privacy by
//! construction
//! [`ResponseMode`] has exactly three variants, and the type signatures
//! downstream of each make the privacy guarantee a COMPILE-TIME property,
//! not just a runtime check:
//! - [`ResponseMode::NotServed`] — the Discord user id isn't on the
//!   [`crate::discord::identity::TrustedFriends`] allowlist. [`respond`]
//!   returns `None` immediately; no reply is sent, no data of any kind is
//!   touched.
//! - [`ResponseMode::Generic`] — allowlisted but NOT opted in (or opted in
//!   with no linked account yet). [`respond`] calls
//!   [`build_generic_reply`], whose signature takes **zero parameters** —
//!   there is no `Candidate`, no account id, no taste/watch-data type
//!   anywhere in scope for this arm to read even by mistake. Its output is
//!   a fixed constant ([`GENERIC_REPLY_CONTENT`]).
//! - [`ResponseMode::TasteAware`] — allowlisted, opted in, AND linked to a
//!   real Muse account. ONLY this arm calls into `curation::candidates`/
//!   `curation::recommend`.
//!
//! Because [`respond`] is a single `match` over [`decide_response_mode`]'s
//! result, and the `Generic` arm's handler cannot accept taste data even if
//! it wanted to, "a non-opted-in friend's reply never carries taste/
//! watch-data" is provable by reading the match arms, not just by running a
//! test — the test below is the runtime confirmation of a guarantee the
//! types already make.

use std::sync::Arc;

use axum::extract::State;
use axum::Json;
use serde::{Deserialize, Serialize};

use crate::curation::candidates::Candidate;
use crate::curation::recommend::{build_rationale, rank_candidates};
use crate::discord::client::RichEmbed;
use crate::discord::identity::{FriendIdentity, TrustedFriends};
use crate::error::MuseResult;
use crate::http::AppState;
use crate::models::media_metadata::MediaKind;
use crate::settings::ExperienceSettings;
use crate::taste_model::chord_client::ChordClient;

/// How many on-deck candidates to fetch before ranking — small, since the
/// bot surfaces one pick per ask, mirroring a conversational cadence rather
/// than a dashboard.
const TASTE_CANDIDATE_FETCH_LIMIT: i64 = 5;

/// The fixed, deterministic content of a generic (no-taste) reply. A
/// constant, not a template interpolating anything account-shaped — see
/// the module doc.
pub const GENERIC_REPLY_CONTENT: &str = "Hey! I don't have your taste linked yet, so I can't \
    give you a personalized pick right now. Ask to be opted in if you'd like Muse to learn your \
    taste and recommend from it.";

/// Which of the three moments [`respond`] is in for a given Discord user.
/// See the module doc — this is the load-bearing privacy gate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResponseMode {
    /// Not on the [`TrustedFriends`] allowlist — not served at all.
    NotServed,
    /// Allowlisted but not (yet) opted in, or opted in with no linked
    /// account — no taste/watch-data use.
    Generic,
    /// Allowlisted, opted in, AND linked to a real Muse account — the ONLY
    /// mode that may touch taste/watch-data.
    TasteAware { muse_account_id: i64 },
}

/// THE gate. Pure and total: given the allowlist lookup result for one
/// Discord user, decide which of the three modes applies. See the module
/// doc for why this makes the privacy guarantee provable by construction.
pub fn decide_response_mode(friend: Option<&FriendIdentity>) -> ResponseMode {
    let Some(friend) = friend else {
        return ResponseMode::NotServed;
    };
    if !friend.is_opted_in() {
        return ResponseMode::Generic;
    }
    match friend.linked_account() {
        Some(muse_account_id) => ResponseMode::TasteAware { muse_account_id },
        // Opted in but not yet linked to an account: there is genuinely no
        // taste to draw on, so this degrades to Generic rather than
        // erroring — same graceful-degrade posture as every optional
        // integration elsewhere in this crate.
        None => ResponseMode::Generic,
    }
}

/// One reply the bot may send: plain content plus an optional
/// [`RichEmbed`]. Produced by exactly one of [`build_generic_reply`] /
/// [`build_taste_reply`] — see the module doc.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BotReply {
    pub content: String,
    pub embed: Option<RichEmbed>,
}

/// The [`ResponseMode::Generic`] handler. Takes NO parameters — there is no
/// way to smuggle a `Candidate`, account id, or any other taste/watch-data
/// value into this function's output even by mistake; its return value is
/// always [`GENERIC_REPLY_CONTENT`] with no embed. This is the "provable by
/// construction" half of the module doc's privacy guarantee.
pub fn build_generic_reply() -> BotReply {
    BotReply {
        content: GENERIC_REPLY_CONTENT.to_string(),
        embed: None,
    }
}

/// Build the [`RichEmbed`] for a taste-aware pick — title + synopsis
/// (joined from the candidate's real, grounded
/// [`Candidate::facts`](crate::curation::candidates::Candidate::facts),
/// exactly what `curation::recommend::template_rationale` builds its
/// sentence from — never invented text) + poster art URL.
///
/// The poster URL is the SAME same-origin `/art/{kind}/{id}` proxy
/// `crate::web::artwork::art_handler` already serves — never a raw
/// upstream (Plex/TMDb) URL and never a URL carrying a credential (that
/// handler's own doc: the Plex token stays server-side). `public_base_url`
/// is `None` when unconfigured (`Config::public_base_url`), same
/// graceful-degrade posture as the tuner routes that already depend on it —
/// the embed simply omits the poster rather than failing.
pub fn build_rich_embed(candidate: &Candidate, public_base_url: Option<&str>) -> RichEmbed {
    let synopsis = candidate.facts.join("; ");
    let poster_url = public_base_url.map(|base| {
        let kind = match candidate.kind {
            MediaKind::Movie => "movie",
            MediaKind::Show => "show",
        };
        format!(
            "{}/art/{kind}/{}",
            base.trim_end_matches('/'),
            candidate.media_metadata_id
        )
    });
    RichEmbed {
        title: candidate.title.clone(),
        poster_url,
        synopsis,
    }
}

/// The [`ResponseMode::TasteAware`] handler: pairs a real rationale
/// (produced by [`crate::curation::recommend::build_rationale`] — the SAME
/// Muse-brain rationale function `/recommend` uses, template-grounded in
/// `candidate.facts`, optionally Chord-phrased) with the candidate's
/// [`RichEmbed`].
pub fn build_taste_reply(
    candidate: &Candidate,
    rationale: &str,
    public_base_url: Option<&str>,
) -> BotReply {
    BotReply {
        content: rationale.to_string(),
        embed: Some(build_rich_embed(candidate, public_base_url)),
    }
}

/// The full orchestration: allowlist lookup -> [`decide_response_mode`] ->
/// exactly one handler. `None` means "the bot sends nothing" (not
/// allowlisted). This is the single public entry point
/// `crate::http`/a Discord gateway handler is expected to call — there is
/// no second, parallel path into the brain that skips the gate.
///
/// `chord` mirrors `curation::recommend::build_rationale`'s own signature:
/// `None` (or an unreachable Chord) degrades to the deterministic
/// templated rationale, never blocks the reply.
pub async fn respond(
    friends: &TrustedFriends,
    discord_user_id: &str,
    pool: &sqlx::PgPool,
    chord: Option<&ChordClient>,
    public_base_url: Option<&str>,
) -> MuseResult<Option<BotReply>> {
    match decide_response_mode(friends.get(discord_user_id)) {
        ResponseMode::NotServed => Ok(None),
        ResponseMode::Generic => Ok(Some(build_generic_reply())),
        ResponseMode::TasteAware { muse_account_id } => {
            let candidates = crate::curation::candidates::gather_on_deck_candidates(
                pool,
                muse_account_id,
                TASTE_CANDIDATE_FETCH_LIMIT,
            )
            .await?;

            let ranked = rank_candidates(candidates);
            let Some((top, _score)) = ranked.into_iter().next() else {
                // No on-deck candidates for this account right now — still
                // taste-aware in principle, just nothing to recommend this
                // moment. Falls back to the generic reply rather than
                // erroring; this is NOT a privacy fallback (the account IS
                // opted in), just "nothing to say yet."
                return Ok(Some(build_generic_reply()));
            };

            let rationale = build_rationale(chord, &top).await;
            Ok(Some(build_taste_reply(&top, &rationale, public_base_url)))
        }
    }
}

/// MUSEX-WIRE-01 (Plane TERM #398, first slice): the settings-gated,
/// PRODUCTION-WIRED entry point onto [`respond`] — see
/// [`discord_respond_handler`] for the `POST /discord/respond` route that
/// calls this. Mirrors `crate::promotion::run_promotion_dispatch`'s
/// inert-when-off contract EXACTLY: gated on
/// [`ExperienceSettings::is_discord_bot_enabled`] BEFORE `friends`/`pool` is
/// touched at all, so the disabled path is provable the same way an
/// unreachable `connect_lazy` pool proves `run_promotion_dispatch`'s gate
/// (see the `db_free` tests below) — if this gate were ever bypassed, the
/// disabled-path test would observe a connection error instead of a quiet
/// `Ok(None)`.
///
/// This does not duplicate or weaken [`respond`]'s own consent gate
/// ([`decide_response_mode`]) — it wraps it with the SECOND, independent
/// gate this crate's experience layer requires (MUSEX-18's settings panel):
/// a friend can be allowlisted and opted in and still get nothing if the
/// operator has switched the Discord bot subsystem off. Both gates must
/// clear for a taste-aware reply to ever leave this function.
pub async fn run_discord_respond(
    settings: &ExperienceSettings,
    friends: &TrustedFriends,
    discord_user_id: &str,
    pool: &sqlx::PgPool,
    chord: Option<&ChordClient>,
    public_base_url: Option<&str>,
) -> MuseResult<Option<BotReply>> {
    if !settings.is_discord_bot_enabled() {
        return Ok(None);
    }
    respond(friends, discord_user_id, pool, chord, public_base_url).await
}

/// The `POST /discord/respond` JSON request body: just the Discord user id
/// the reply is being computed for. No account id, no taste data — the
/// whole point of this route is that the SERVER resolves consent, never the
/// caller.
#[derive(Debug, Deserialize)]
pub struct DiscordRespondRequest {
    pub discord_user_id: String,
}

/// The `POST /discord/respond` JSON response — a flattened, HTTP-friendly
/// view of [`BotReply`] (`None` fields throughout when the bot has nothing
/// to say, e.g. the subsystem is off or the user isn't allowlisted).
#[derive(Debug, Serialize)]
pub struct DiscordRespondResponse {
    pub content: Option<String>,
    pub embed_title: Option<String>,
    pub embed_poster_url: Option<String>,
    pub embed_synopsis: Option<String>,
}

fn to_response(reply: Option<BotReply>) -> DiscordRespondResponse {
    match reply {
        None => DiscordRespondResponse {
            content: None,
            embed_title: None,
            embed_poster_url: None,
            embed_synopsis: None,
        },
        Some(reply) => DiscordRespondResponse {
            content: Some(reply.content),
            embed_title: reply.embed.as_ref().map(|e| e.title.clone()),
            embed_poster_url: reply.embed.as_ref().and_then(|e| e.poster_url.clone()),
            embed_synopsis: reply.embed.as_ref().map(|e| e.synopsis.clone()),
        },
    }
}

/// `POST /discord/respond` — MUSEX-WIRE-01's flagship wired flow: the
/// production HTTP door onto [`run_discord_respond`] (settings gate) ->
/// [`respond`] (consent gate) -> the real MUSE-11
/// `curation::candidates`/`curation::recommend` pipeline, the same one
/// `/recommend` uses.
///
/// ## Honest seam (be explicit, don't paper over it)
/// This crate has no persisted per-friend consent store yet —
/// `crate::discord::identity`'s own module doc says as much ("this module
/// doesn't prescribe the source, only the shape"). The only roster this
/// handler can build in production today is
/// `ExperienceSettings::discord_bot.trusted_friends`, and that settings
/// document is explicit (see `crate::settings::DiscordBotSettings`'s own
/// doc) that its roster entries are ALLOWLIST membership only — they can
/// never themselves grant `taste_opt_in`, which stays gated behind
/// `FriendIdentity::opt_in`'s private fields. So every identity this
/// handler resolves is, in production today, at best
/// `ResponseMode::Generic` — the route is real, reachable, and enforces
/// both gates correctly end-to-end, but the `TasteAware` arm needs a real
/// opt-in persistence layer (a separate, natural follow-up item) before a
/// live Discord friend can reach it. [`run_discord_respond`]'s own tests
/// exercise the `TasteAware` arm directly against a constructed opted-in
/// identity, proving the gate + pipeline are correct even though
/// production can't yet drive a real friend into that state.
pub async fn discord_respond_handler(
    State(state): State<Arc<AppState>>,
    Json(req): Json<DiscordRespondRequest>,
) -> MuseResult<Json<DiscordRespondResponse>> {
    let settings = crate::repo::settings::load(&state.pool).await?;

    // See the module-level "Honest seam" note above: the roster this
    // handler can build today carries allowlist membership only, never
    // opt-in.
    let friends = TrustedFriends::from_friends(
        settings
            .discord_bot
            .trusted_friends
            .iter()
            .map(|f| FriendIdentity::new(f.discord_user_id.clone(), f.display_name.clone())),
    );

    let chord = ChordClient::from_config(&state.config);
    let reply = run_discord_respond(
        &settings,
        &friends,
        &req.discord_user_id,
        &state.pool,
        chord.as_ref(),
        state.config.public_base_url.as_deref(),
    )
    .await?;

    Ok(Json(to_response(reply)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::curation::candidates::CandidateSource;

    fn candidate() -> Candidate {
        Candidate {
            media_metadata_id: 7,
            media_item_id: Some(9),
            title: "Severance".to_string(),
            year: Some(2022),
            kind: MediaKind::Show,
            source: CandidateSource::OnDeck,
            taste_fit: 0.8,
            facts: vec!["you're 42% through it".to_string()],
            availability: None,
        }
    }

    // --- the gate: pure, unconditional -------------------------------------

    #[test]
    fn not_allowlisted_is_not_served() {
        assert_eq!(decide_response_mode(None), ResponseMode::NotServed);
    }

    #[test]
    fn allowlisted_but_not_opted_in_is_generic() {
        let friend = FriendIdentity::new("discord-1", "Alex");
        assert_eq!(decide_response_mode(Some(&friend)), ResponseMode::Generic);
    }

    #[test]
    fn opted_in_but_unlinked_account_is_still_generic() {
        // Can't be constructed via the production API (opt_in always sets
        // the account together with the flag, and the consent fields are
        // private) -- but decide_response_mode must still degrade safely if
        // that combination is ever reached by a future refactor. The
        // test-only `from_parts_for_test` constructor is the sole way to
        // build this impossible-in-production state, precisely because
        // production code has no path to it.
        let friend = FriendIdentity::from_parts_for_test("discord-1", "Alex", true, None);
        assert_eq!(decide_response_mode(Some(&friend)), ResponseMode::Generic);
    }

    #[test]
    fn opted_in_and_linked_is_taste_aware() {
        let friend = FriendIdentity::new("discord-1", "Alex").opt_in(42);
        assert_eq!(
            decide_response_mode(Some(&friend)),
            ResponseMode::TasteAware {
                muse_account_id: 42
            }
        );
    }

    // --- negative: generic reply carries no taste/watch-data ---------------

    #[test]
    fn generic_reply_has_no_embed_and_fixed_content() {
        let reply = build_generic_reply();
        assert!(reply.embed.is_none());
        assert_eq!(reply.content, GENERIC_REPLY_CONTENT);
    }

    #[test]
    fn generic_reply_never_mentions_a_title_or_watch_signal() {
        // Even though this looks redundant with the fixed-content
        // assertion above, it documents the actual property under test in
        // case GENERIC_REPLY_CONTENT is ever edited: it must never grow
        // account-scoped substance.
        let reply = build_generic_reply();
        let lower = reply.content.to_lowercase();
        for leaked_term in ["severance", "42%", "watched", "on deck", "finished"] {
            assert!(
                !lower.contains(leaked_term),
                "generic reply must never contain {leaked_term:?}"
            );
        }
    }

    // --- positive: taste reply is grounded in the real candidate ----------

    #[test]
    fn rich_embed_is_grounded_in_the_real_candidate_facts() {
        let c = candidate();
        let embed = build_rich_embed(&c, None);
        assert_eq!(embed.title, "Severance");
        assert_eq!(embed.synopsis, "you're 42% through it");
        assert!(embed.poster_url.is_none());
    }

    #[test]
    fn rich_embed_poster_url_uses_the_same_origin_art_proxy() {
        let c = candidate();
        let embed = build_rich_embed(&c, Some("http://192.0.2.10:8090"));
        assert_eq!(
            embed.poster_url.as_deref(),
            Some("http://192.0.2.10:8090/art/show/7")
        );
    }

    #[test]
    fn rich_embed_poster_url_trims_a_trailing_slash() {
        let c = candidate();
        let embed = build_rich_embed(&c, Some("http://192.0.2.10:8090/"));
        assert_eq!(
            embed.poster_url.as_deref(),
            Some("http://192.0.2.10:8090/art/show/7")
        );
    }

    #[test]
    fn taste_reply_carries_the_rationale_and_the_embed() {
        let c = candidate();
        let reply = build_taste_reply(&c, "Continue \"Severance\" — you're 42% through it.", None);
        assert_eq!(
            reply.content,
            "Continue \"Severance\" — you're 42% through it."
        );
        assert_eq!(reply.embed.as_ref().unwrap().title, "Severance");
    }

    // --- respond(): not-allowlisted / generic never touch the DB path -----

    #[tokio::test]
    async fn respond_returns_none_for_a_non_allowlisted_user_without_a_pool() {
        // A real PgPool is required by respond()'s signature (the
        // TasteAware arm needs one), but PgPool::connect_lazy never opens a
        // real connection -- this proves NotServed short-circuits before
        // any DB access is attempted (a real connect would hang/err against
        // this bogus DSN if the code path ever reached it).
        let pool = sqlx::postgres::PgPoolOptions::new()
            .connect_lazy("postgres://unused:unused@127.0.0.1:1/unused")
            .expect("lazy pool never connects eagerly");
        let friends = TrustedFriends::new();

        let reply = respond(&friends, "discord-1", &pool, None, None)
            .await
            .expect("NotServed must never error");
        assert!(reply.is_none());
    }

    #[tokio::test]
    async fn respond_returns_generic_for_a_non_opted_in_allowlisted_user_without_a_pool() {
        // Same lazy-pool trick: proves the Generic arm never reaches the DB
        // candidate-fetch call either.
        let pool = sqlx::postgres::PgPoolOptions::new()
            .connect_lazy("postgres://unused:unused@127.0.0.1:1/unused")
            .expect("lazy pool never connects eagerly");
        let friends = TrustedFriends::from_friends([FriendIdentity::new("discord-1", "Alex")]);

        let reply = respond(&friends, "discord-1", &pool, None, None)
            .await
            .expect("Generic must never error or touch the DB");
        let reply = reply.expect("allowlisted user is served");
        assert!(reply.embed.is_none());
        assert_eq!(reply.content, GENERIC_REPLY_CONTENT);
    }

    // --- MUSEX-WIRE-01: run_discord_respond is inert when disabled ---------
    //
    // Same `connect_lazy`-unreachable-pool idiom as
    // `crate::promotion::tests`' `run_promotion_dispatch` inertness tests:
    // an opted-in friend (so if the settings gate were bypassed, the
    // TasteAware arm WOULD try to touch the pool) proves the disabled path
    // short-circuits before any DB access, because a real query against
    // this bogus DSN would surface as an `Err`, not a quiet `Ok(None)`.

    use crate::settings::DiscordBotSettings;

    fn unreachable_pool() -> sqlx::PgPool {
        sqlx::postgres::PgPoolOptions::new()
            .connect_lazy("postgres://unused:unused@127.0.0.1:1/unused")
            .expect("connect_lazy should never fail synchronously")
    }

    fn one_opted_in_friend() -> TrustedFriends {
        TrustedFriends::from_friends([FriendIdentity::new("discord-1", "Alex").opt_in(42)])
    }

    #[tokio::test]
    async fn run_discord_respond_is_inert_when_discord_bot_disabled() {
        let pool = unreachable_pool();
        let mut settings = ExperienceSettings::default();
        settings.master_enabled = true;
        settings.discord_bot = DiscordBotSettings {
            enabled: false,
            ..settings.discord_bot
        };
        let friends = one_opted_in_friend();

        let result = run_discord_respond(&settings, &friends, "discord-1", &pool, None, None).await;

        assert!(result.is_ok(), "expected Ok(None), got {result:?}");
        assert!(
            result.unwrap().is_none(),
            "a disabled subsystem must return no reply"
        );
    }

    #[tokio::test]
    async fn run_discord_respond_is_inert_when_master_switch_off() {
        // Even with discord_bot.enabled = true, the master switch alone
        // must be enough — mirrors
        // `is_discord_bot_enabled`'s AND-gate documented on
        // `ExperienceSettings`.
        let pool = unreachable_pool();
        let mut settings = ExperienceSettings::default();
        settings.master_enabled = false;
        settings.discord_bot = DiscordBotSettings {
            enabled: true,
            ..settings.discord_bot
        };
        let friends = one_opted_in_friend();

        let result = run_discord_respond(&settings, &friends, "discord-1", &pool, None, None).await;

        assert!(result.is_ok(), "expected Ok(None), got {result:?}");
        assert!(result.unwrap().is_none());
    }

    #[tokio::test]
    async fn run_discord_respond_touches_the_pool_when_enabled_and_opted_in() {
        // Mirror-image sanity check (same idiom as
        // `run_promotion_dispatch_touches_the_pool_when_enabled`): WITH the
        // gate enabled and a genuinely opted-in friend, this must actually
        // reach the (unreachable) pool and fail with a database error --
        // proving the two disabled-path tests above assert something real.
        let pool = unreachable_pool();
        let mut settings = ExperienceSettings::default();
        settings.master_enabled = true;
        settings.discord_bot = DiscordBotSettings {
            enabled: true,
            ..settings.discord_bot
        };
        let friends = one_opted_in_friend();

        let result = run_discord_respond(&settings, &friends, "discord-1", &pool, None, None).await;

        assert!(
            result.is_err(),
            "expected a database connection error proving the enabled+opted-in path really \
             reaches the pool, got Ok({:?})",
            result.ok()
        );
    }

    #[tokio::test]
    async fn run_discord_respond_returns_generic_for_non_opted_in_friend_even_when_enabled() {
        // Non-opted-in is a DIFFERENT arm (`ResponseMode::Generic`) that
        // `respond` itself already proves never touches the DB -- so with
        // the subsystem enabled but the friend only allowlisted (not opted
        // in), this must still return the generic reply, not an error and
        // not taste data, even against an unreachable pool.
        let pool = unreachable_pool();
        let mut settings = ExperienceSettings::default();
        settings.master_enabled = true;
        settings.discord_bot = DiscordBotSettings {
            enabled: true,
            ..settings.discord_bot
        };
        let friends = TrustedFriends::from_friends([FriendIdentity::new("discord-1", "Alex")]);

        let result = run_discord_respond(&settings, &friends, "discord-1", &pool, None, None).await;

        let reply = result
            .expect("Generic arm must never error")
            .expect("allowlisted user is served");
        assert!(
            reply.embed.is_none(),
            "non-opted-in reply carries no taste data"
        );
        assert_eq!(reply.content, GENERIC_REPLY_CONTENT);
    }

    // --- MUSEX-WIRE-01: DiscordRespondResponse DTO shape --------------------

    #[test]
    fn to_response_of_none_is_all_none_fields() {
        let response = to_response(None);
        assert!(response.content.is_none());
        assert!(response.embed_title.is_none());
        assert!(response.embed_poster_url.is_none());
        assert!(response.embed_synopsis.is_none());
    }

    #[test]
    fn to_response_of_a_taste_reply_carries_the_embed_fields() {
        let c = candidate();
        let reply = build_taste_reply(&c, "Continue \"Severance\" — you're 42% through it.", None);
        let response = to_response(Some(reply));
        assert_eq!(
            response.content.as_deref(),
            Some("Continue \"Severance\" — you're 42% through it.")
        );
        assert_eq!(response.embed_title.as_deref(), Some("Severance"));
        assert_eq!(
            response.embed_synopsis.as_deref(),
            Some("you're 42% through it")
        );
    }
}

/// DB-backed end-to-end coverage: real `curation::candidates`/`recommend`
/// against a real account's real watch data. `db_gated` (per
/// `MUSE_TEST_DATABASE_URL`, same convention as
/// `crate::endpoint_tests::db_gated`) — skips cleanly, never a hard failure,
/// when no test database is configured.
#[cfg(test)]
mod db_gated {
    use super::*;
    use crate::models::account::NewAccount;
    use crate::models::library::{LibraryKind, NewLibrary};
    use crate::models::media_item::NewMediaItem;
    use crate::models::media_metadata::{MediaKind, NewMediaMetadata};
    use crate::models::watch_stats::NewWatchStats;
    use crate::repo;
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

    /// Seed one account with one real, distinctively-titled on-deck watch
    /// signal (UUID-suffixed so this test's rows never collide with
    /// another concurrent run's), and return `(account_id, expected_title)`.
    async fn seed_on_deck_account(pool: &sqlx::PgPool) -> (i64, String) {
        let suffix = uuid::Uuid::new_v4().simple().to_string();
        let title = format!("MUSEX13-PrivacyProbe-{suffix}");

        let account = repo::account::create(
            pool,
            &NewAccount {
                plex_account_id: Some(format!("plex-{suffix}")),
                username: Some(format!("user-{suffix}")),
                friendly_name: Some("Discord Privacy Probe".to_string()),
                is_home_user: false,
                is_primary: false,
            },
        )
        .await
        .expect("create account");

        let library = repo::library::create(
            pool,
            &NewLibrary {
                name: format!("lib-{suffix}"),
                kind: LibraryKind::Tv,
                root_folder: format!("/tv-{suffix}"),
                source_arr_name: None,
                source_arr_url: None,
            },
        )
        .await
        .expect("create library");

        let metadata = repo::media_metadata::upsert_by_tmdb(
            pool,
            &NewMediaMetadata {
                kind: MediaKind::Show,
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
                runtime_minutes: Some(45),
                year: Some(2024),
                images: json!({}),
            },
        )
        .await
        .expect("create media_metadata");

        let media_item = repo::media_item::upsert(
            pool,
            &NewMediaItem {
                library_id: library.id,
                media_metadata_id: metadata.id,
                path: format!("/tv-{suffix}/show"),
                monitored: true,
                quality_profile_id: None,
                minimum_availability: None,
                plex_rating_key: Some(format!("plexkey-{suffix}")),
                added_at: None,
            },
        )
        .await
        .expect("create media_item");

        repo::watch_stats::upsert_watch_stats(
            pool,
            &NewWatchStats {
                account_id: account.id,
                media_item_id: media_item.id,
                play_count: 3,
                finished_count: 0,
                rewatch_count: 0,
                total_watched_ms: 1_000_000,
                avg_percent: Some(42.0),
                last_watched_at: Some(chrono::Utc::now()),
                abandoned: false,
                first_watched_at: Some(chrono::Utc::now()),
            },
        )
        .await
        .expect("create watch_stats");

        (account.id, title)
    }

    #[tokio::test]
    async fn opted_in_friend_gets_a_real_grounded_taste_reply() {
        let Some(pool) =
            test_pool_or_skip("opted_in_friend_gets_a_real_grounded_taste_reply").await
        else {
            return;
        };
        let (account_id, expected_title) = seed_on_deck_account(&pool).await;

        let friends =
            TrustedFriends::from_friends([
                FriendIdentity::new("discord-opted-in", "Alex").opt_in(account_id)
            ]);

        let reply = respond(&friends, "discord-opted-in", &pool, None, None)
            .await
            .expect("respond should not error")
            .expect("opted-in allowlisted friend is served");

        assert!(
            reply.content.contains(&expected_title),
            "taste-aware reply must be grounded in the real seeded title, got: {}",
            reply.content
        );
        let embed = reply.embed.expect("taste-aware reply carries an embed");
        assert_eq!(embed.title, expected_title);
    }

    /// MUSEX-WIRE-01: the wired, settings-gated entry point produces the
    /// SAME real, grounded taste reply as `respond` itself when the
    /// subsystem is enabled — non-vacuous confirmation that
    /// `run_discord_respond`'s gate is additive, not a second place taste
    /// data could get lost or substituted.
    #[tokio::test]
    async fn run_discord_respond_produces_a_real_grounded_reply_when_enabled_and_opted_in() {
        let Some(pool) = test_pool_or_skip(
            "run_discord_respond_produces_a_real_grounded_reply_when_enabled_and_opted_in",
        )
        .await
        else {
            return;
        };
        let (account_id, expected_title) = seed_on_deck_account(&pool).await;

        let mut settings = crate::settings::ExperienceSettings::default();
        settings.master_enabled = true;
        settings.discord_bot.enabled = true;
        let friends =
            TrustedFriends::from_friends([
                FriendIdentity::new("discord-opted-in", "Alex").opt_in(account_id)
            ]);

        let reply = run_discord_respond(&settings, &friends, "discord-opted-in", &pool, None, None)
            .await
            .expect("run_discord_respond should not error")
            .expect("enabled + opted-in friend is served");

        assert!(
            reply.content.contains(&expected_title),
            "wired taste-aware reply must be grounded in the real seeded title, got: {}",
            reply.content
        );
        assert!(reply.embed.is_some());
    }

    /// LOAD-BEARING PRIVACY NEGATIVE TEST (end-to-end). The seeded account
    /// has real, distinctive watch data — but the Discord friend record is
    /// NOT opted in. Prove the reply contains NONE of it: not the title,
    /// not the synopsis, not an embed at all. This is the runtime
    /// confirmation of the compile-time guarantee `decide_response_mode`'s
    /// `Generic` arm makes (see the module doc): real watch-data exists and
    /// is reachable in this account, yet the non-opted-in path never
    /// surfaces any of it.
    #[tokio::test]
    async fn non_opted_in_friend_gets_no_taste_or_watch_data_even_with_real_seeded_data() {
        let Some(pool) = test_pool_or_skip(
            "non_opted_in_friend_gets_no_taste_or_watch_data_even_with_real_seeded_data",
        )
        .await
        else {
            return;
        };
        let (account_id, seeded_title) = seed_on_deck_account(&pool).await;

        // Allowlisted (so this isn't just NotServed) but the account link
        // exists ONLY via the test-only constructor to simulate "we
        // technically know the account" WITHOUT ever calling opt_in() --
        // taste_opt_in stays false. This is the strictest version of the
        // negative test: even a friend record that knows the account id must
        // not leak it without the explicit opt-in. Note production code
        // cannot even build this state (the consent fields are private and
        // opt_in() sets both together) -- only from_parts_for_test can.
        let friend = FriendIdentity::from_parts_for_test(
            "discord-not-opted-in",
            "Sam",
            false,
            Some(account_id),
        );
        assert!(!friend.is_opted_in(), "sanity: default is not opted in");
        let friends = TrustedFriends::from_friends([friend]);

        let reply = respond(&friends, "discord-not-opted-in", &pool, None, None)
            .await
            .expect("respond should not error")
            .expect("allowlisted friend is still served (generically)");

        assert!(
            reply.embed.is_none(),
            "non-opted-in reply must carry no embed"
        );
        assert_eq!(
            reply.content, GENERIC_REPLY_CONTENT,
            "non-opted-in reply must be the fixed generic content"
        );
        assert!(
            !reply.content.contains(&seeded_title),
            "non-opted-in reply must never contain the real seeded title"
        );
        assert!(
            !reply.content.contains("42"),
            "non-opted-in reply must never contain the real seeded watch percentage"
        );
    }

    #[tokio::test]
    async fn non_allowlisted_user_is_not_served_even_with_real_seeded_data() {
        let Some(pool) =
            test_pool_or_skip("non_allowlisted_user_is_not_served_even_with_real_seeded_data")
                .await
        else {
            return;
        };
        let (_account_id, _title) = seed_on_deck_account(&pool).await;

        // Empty allowlist: nobody is trusted yet.
        let friends = TrustedFriends::new();
        let reply = respond(&friends, "discord-stranger", &pool, None, None)
            .await
            .expect("respond should not error");
        assert!(
            reply.is_none(),
            "a non-allowlisted user must get no reply at all"
        );
    }
}
