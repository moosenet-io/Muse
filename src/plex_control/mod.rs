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
    /// The session is live, but Muse has no `plex_clients` row whose name
    /// matches the session's reported `player` — nowhere to send a stop.
    /// Same "never fabricate success" posture as no controller configured.
    NoTarget,
    /// Resolved to a Companion `machine_identifier` — safe to relay a
    /// `CastController::stop` to.
    Resolved { machine_identifier: String },
}

/// Resolve `session_key` against the LIVE set (MACT-01's `list_live` scope)
/// and, only if it's live, against a discovered `plex_clients` target.
///
/// This is the entire safety property MACT-02 exists for: a caller supplies
/// only a `session_key` that Muse itself already knows names a currently
/// live session — never an arbitrary player target that would let a caller
/// relay a stop to a device of their choosing. See
/// [`crate::repo::play_session::find_live_by_session_key`]'s doc comment
/// for why an already-stopped session and an unknown key are
/// indistinguishable here (both `NotFound`) — that's deliberate, not a gap.
pub async fn resolve_live_target(pool: &PgPool, session_key: &str) -> MuseResult<ResolveOutcome> {
    let Some(row) =
        crate::repo::play_session::find_live_by_session_key(pool, session_key).await?
    else {
        return Ok(ResolveOutcome::NotFound);
    };

    let Some(player_name) = row.player.as_deref().filter(|s| !s.is_empty()) else {
        return Ok(ResolveOutcome::NoTarget);
    };

    match repo::find_machine_identifier_by_name(pool, player_name).await? {
        Some(machine_identifier) => Ok(ResolveOutcome::Resolved { machine_identifier }),
        None => Ok(ResolveOutcome::NoTarget),
    }
}
