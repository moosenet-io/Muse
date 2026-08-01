//! Plex playback-control client + player discovery (MUSE-22, spec §4d-A).
//!
//! This module is the *mechanism* piece of the Channels feature: discover
//! Plex Companion / client-control targets (`plex_clients`), build play
//! queues, and drive transport commands (`play`/`pause`/`skipNext`/`stop`/
//! timeline poll). These are all benign control actions — they start/steer
//! playback and never mutate the Plex library.
//!
//! Deliberately **out of scope** here:
//! - Confirm-gate / approval logic for issuing these commands — that's
//!   MUSE-25, layered on top of this mechanism.
//! - The channel composer (schedule generation) — MUSE-2x, consumes
//!   `create_play_queue`/`CastController` from here.
//! - Google Cast (raw Cast v2) for bare Chromecasts without a Plex
//!   receiver — `cast::GoogleCastController` reserves the seam only.

mod cast;
mod client;
mod models;
mod repo;

pub use cast::{CastController, GoogleCastController};
pub use client::PlexControlClient;
pub use models::{PlayQueue, PlayQueueRequest, PlexPlayer, TimelinePoll, TransportCommand};
pub use repo::upsert_players;

use sqlx::PgPool;

use crate::error::MuseResult;

/// Outcome of resolving a `session_key` to a controllable Companion target
/// for MACT-02's `POST /api/sessions/:session_key/terminate` — see
/// [`resolve_live_target`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolveOutcome {
    /// No `stopped_at IS NULL` `play_sessions` row matches this
    /// `session_key`. Covers both "Muse never saw this key" and
    /// "already stopped" — both are correctly `404`, no relay attempted.
    NotFound,
    /// More than one currently-open `play_sessions` row matches this
    /// `session_key` (Plex reuses the key; see
    /// [`crate::repo::play_session::find_live_by_session_key`]'s doc
    /// comment). Refused, never a silent pick of "the newest one" — a
    /// mutation with this blast radius must not guess which live session
    /// the caller meant. Reported `409`, distinct from `NotFound`'s `404`.
    AmbiguousSession,
    /// The unique open row for this `session_key` exists but classifies as
    /// `Stale` (MACT-01's own `classify_session_state` — no `play_events`
    /// row within the liveness grace window). Review finding, cycle 2
    /// (codex, confirmed): MACT-01 deliberately RETAINS stale open rows
    /// (that's what the `stale` state is for), so without this check an old
    /// stale session's player name could resolve and stop a NEWER session
    /// actually running on that same device — the harm would only be
    /// noticed after the fact, by the post-stop timeline-poll downgrade.
    /// Refusing here closes it at the source instead: terminate requires
    /// fresh `playing`/`paused` evidence, exactly the bar `GET
    /// /api/sessions/live` already applies, reused rather than
    /// reimplemented so the two items can't drift into two definitions of
    /// "live". Reported `404`, grouped with `NotFound` — both mean "no
    /// session Muse will currently vouch for as live", not two different
    /// facts a caller needs to distinguish.
    StaleSession,
    /// The session is live, but Muse has no `plex_clients` row whose name
    /// matches the session's reported `player` at all — nowhere to send a
    /// stop. Same "never fabricate success" posture as no controller
    /// configured.
    NoTarget,
    /// A `plex_clients` row matches the session's reported `player` name,
    /// but only a STALE one (`last_seen_at` older than
    /// `Config::terminate_target_fresh_within_secs` — see
    /// [`repo::find_machine_identifier_by_name`]'s doc comment). Refused
    /// distinctly from `NoTarget` (there WAS a match, it's just not
    /// current) — an obsolete row is not evidence of where a NEW session on
    /// a same-named device actually lives. Also `503`, same status as
    /// `NoTarget`: both mean "nothing Muse currently trusts enough to relay
    /// a stop to", the distinction is for logs/diagnostics, not the caller.
    StaleTarget,
    /// More than one FRESH `plex_clients` row shares the session's reported
    /// `player` display name (see
    /// [`repo::find_machine_identifier_by_name`]'s doc comment) — refused
    /// for the same reason as `AmbiguousSession`, also `409`.
    AmbiguousTarget,
    /// Resolved to exactly one FRESH Companion `machine_identifier` — safe
    /// to relay a `CastController::stop` to.
    Resolved { machine_identifier: String },
}

/// Resolve `session_key` against the LIVE set (MACT-01's `list_live` scope)
/// and, only if it's live AND fresh, against a discovered `plex_clients`
/// target that is ALSO fresh.
///
/// This is the entire safety property MACT-02 exists for: a caller supplies
/// only a `session_key` that Muse itself already knows names a currently
/// live session — never an arbitrary player target that would let a caller
/// relay a stop to a device of their choosing. See
/// [`crate::repo::play_session::find_live_by_session_key`]'s doc comment
/// for why an already-stopped session and an unknown key are
/// indistinguishable here (both `NotFound`) — that's deliberate, not a gap.
///
/// Two review cycles hardened this beyond plain uniqueness:
/// - Cycle 1: ambiguity (more than one candidate) at EITHER resolution step
///   is a refusal (`AmbiguousSession`/`AmbiguousTarget`), never a silent
///   "pick the newest" — see [`crate::repo::AtMostOne`].
/// - Cycle 2: uniqueness alone isn't identity. A `session_key` row that IS
///   unique can still be `Stale` (MACT-01's own liveness classification,
///   reused here via `grace_secs`), and a `plex_clients` name match that IS
///   unique can still be an obsolete row (`fresh_within_secs`). Both are
///   refusals too (`StaleSession`/`StaleTarget`) — see [`crate::repo::FreshnessLookup`].
///
/// Neither `grace_secs` nor `fresh_within_secs` fully closes the underlying
/// gap — name-based resolution fundamentally cannot establish identity, only
/// bound how wrong a guess can be. `TODO(S130-J)`: the durable fix is
/// `play_sessions` stamping a stable Plex client id at ingest, removing the
/// need for this whole function. Until then, refusing under any doubt is
/// the defensible mitigation this function exists to provide.
pub async fn resolve_live_target(
    pool: &PgPool,
    session_key: &str,
    grace_secs: u64,
    fresh_within_secs: u64,
) -> MuseResult<ResolveOutcome> {
    use crate::repo::{AtMostOne, FreshnessLookup};

    let row = match crate::repo::play_session::find_live_by_session_key(pool, session_key).await?
    {
        AtMostOne::None => return Ok(ResolveOutcome::NotFound),
        AtMostOne::Ambiguous => return Ok(ResolveOutcome::AmbiguousSession),
        AtMostOne::One(row) => row,
    };

    // Cycle 2: reuse MACT-01's own liveness judgement rather than
    // reimplementing it -- a session that `GET /api/sessions/live` itself
    // would report `state: "stale"` is not evidence of anything currently
    // playing on that device, and must not be trusted to resolve a target.
    let state = crate::repo::play_session::classify_session_state(
        row.last_event_type.as_deref(),
        row.last_event_at,
        chrono::Utc::now(),
        grace_secs,
    );
    if state == crate::repo::play_session::SessionPlayState::Stale {
        return Ok(ResolveOutcome::StaleSession);
    }

    let Some(player_name) = row.player.as_deref().filter(|s| !s.is_empty()) else {
        return Ok(ResolveOutcome::NoTarget);
    };

    match repo::find_machine_identifier_by_name(pool, player_name, fresh_within_secs).await? {
        FreshnessLookup::NoMatch => Ok(ResolveOutcome::NoTarget),
        FreshnessLookup::StaleOnly => Ok(ResolveOutcome::StaleTarget),
        FreshnessLookup::Ambiguous => Ok(ResolveOutcome::AmbiguousTarget),
        FreshnessLookup::Found(machine_identifier) => {
            Ok(ResolveOutcome::Resolved { machine_identifier })
        }
    }
}
