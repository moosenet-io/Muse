//! Repo functions for `play_sessions` + `play_session_media_info`.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::error::DatabaseError;
use sqlx::{FromRow, PgPool};

use crate::error::{MuseError, MuseResult};
use crate::models::media_metadata::MediaKind;
use crate::models::play_session::{
    DecisionKind, NewPlaySession, NewPlaySessionMediaInfo, PlaySession, PlaySessionMediaInfo,
};

/// Upsert keyed by the table's `(account_id, media_item_id, episode_id,
/// started_at)` UNIQUE — used by both the live reconstruction worker
/// (advancing an in-progress session) and the Tautulli backfill importer.
///
/// Caveat (inherited from the spec's own UNIQUE shape, not introduced
/// here): Postgres treats NULLs as distinct in a UNIQUE constraint, so this
/// ON CONFLICT only dedups when `media_item_id`/`episode_id` are non-NULL
/// (i.e. the session has been resolved to a library item). A session for an
/// unresolved `rating_key` will insert a new row per call rather than
/// updating in place until resolution happens.
pub async fn upsert(pool: &PgPool, new: &NewPlaySession) -> MuseResult<PlaySession> {
    sqlx::query_as::<_, PlaySession>(
        r#"
        INSERT INTO play_sessions (
            account_id, media_item_id, episode_id, session_key, tautulli_ref_id,
            started_at, stopped_at, duration_ms, watched_ms, view_offset_ms,
            percent_complete, paused_counter, paused_ms, is_finished, is_abandoned,
            player, platform, product, device, ip_address,
            started_hour, started_dow, is_cinema_context
        )
        VALUES (
            $1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15,
            $16, $17, $18, $19, $20, $21, $22, $23
        )
        ON CONFLICT (account_id, media_item_id, episode_id, started_at) DO UPDATE SET
            session_key = EXCLUDED.session_key,
            tautulli_ref_id = EXCLUDED.tautulli_ref_id,
            stopped_at = EXCLUDED.stopped_at,
            duration_ms = EXCLUDED.duration_ms,
            watched_ms = EXCLUDED.watched_ms,
            view_offset_ms = EXCLUDED.view_offset_ms,
            percent_complete = EXCLUDED.percent_complete,
            paused_counter = EXCLUDED.paused_counter,
            paused_ms = EXCLUDED.paused_ms,
            is_finished = EXCLUDED.is_finished,
            is_abandoned = EXCLUDED.is_abandoned,
            player = EXCLUDED.player,
            platform = EXCLUDED.platform,
            product = EXCLUDED.product,
            device = EXCLUDED.device,
            ip_address = EXCLUDED.ip_address,
            started_hour = EXCLUDED.started_hour,
            started_dow = EXCLUDED.started_dow,
            is_cinema_context = EXCLUDED.is_cinema_context
        RETURNING *
        "#,
    )
    .bind(new.account_id)
    .bind(new.media_item_id)
    .bind(new.episode_id)
    .bind(&new.session_key)
    .bind(new.tautulli_ref_id)
    .bind(new.started_at)
    .bind(new.stopped_at)
    .bind(new.duration_ms)
    .bind(new.watched_ms)
    .bind(new.view_offset_ms)
    .bind(new.percent_complete)
    .bind(new.paused_counter)
    .bind(new.paused_ms)
    .bind(new.is_finished)
    .bind(new.is_abandoned)
    .bind(&new.player)
    .bind(&new.platform)
    .bind(&new.product)
    .bind(&new.device)
    .bind(new.ip_address)
    .bind(new.started_hour)
    .bind(new.started_dow)
    .bind(new.is_cinema_context)
    .fetch_one(pool)
    .await
    .map_err(MuseError::Database)
}

pub async fn get(pool: &PgPool, id: i64) -> MuseResult<PlaySession> {
    sqlx::query_as::<_, PlaySession>("SELECT * FROM play_sessions WHERE id = $1")
        .bind(id)
        .fetch_optional(pool)
        .await
        .map_err(MuseError::Database)?
        .ok_or_else(|| MuseError::NotFound(format!("play_session {id} not found")))
}

pub async fn list_for_account(pool: &PgPool, account_id: i64, limit: i64) -> MuseResult<Vec<PlaySession>> {
    sqlx::query_as::<_, PlaySession>(
        "SELECT * FROM play_sessions WHERE account_id = $1 ORDER BY started_at DESC LIMIT $2",
    )
    .bind(account_id)
    .bind(limit)
    .fetch_all(pool)
    .await
    .map_err(MuseError::Database)
}

pub async fn list_for_media_item(pool: &PgPool, media_item_id: i64) -> MuseResult<Vec<PlaySession>> {
    sqlx::query_as::<_, PlaySession>(
        "SELECT * FROM play_sessions WHERE media_item_id = $1 ORDER BY started_at DESC",
    )
    .bind(media_item_id)
    .fetch_all(pool)
    .await
    .map_err(MuseError::Database)
}

pub async fn upsert_media_info(
    pool: &PgPool,
    play_session_id: i64,
    new: &NewPlaySessionMediaInfo,
) -> MuseResult<PlaySessionMediaInfo> {
    sqlx::query_as::<_, PlaySessionMediaInfo>(
        r#"
        INSERT INTO play_session_media_info (
            play_session_id, video_decision, audio_decision, transcode_decision,
            container, video_codec, audio_codec, audio_channels, video_resolution,
            bitrate, width, height, transcode_reason
        )
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13)
        ON CONFLICT (play_session_id) DO UPDATE SET
            video_decision = EXCLUDED.video_decision,
            audio_decision = EXCLUDED.audio_decision,
            transcode_decision = EXCLUDED.transcode_decision,
            container = EXCLUDED.container,
            video_codec = EXCLUDED.video_codec,
            audio_codec = EXCLUDED.audio_codec,
            audio_channels = EXCLUDED.audio_channels,
            video_resolution = EXCLUDED.video_resolution,
            bitrate = EXCLUDED.bitrate,
            width = EXCLUDED.width,
            height = EXCLUDED.height,
            transcode_reason = EXCLUDED.transcode_reason
        RETURNING *
        "#,
    )
    .bind(play_session_id)
    .bind(new.video_decision)
    .bind(new.audio_decision)
    .bind(new.transcode_decision)
    .bind(&new.container)
    .bind(&new.video_codec)
    .bind(&new.audio_codec)
    .bind(new.audio_channels)
    .bind(&new.video_resolution)
    .bind(new.bitrate)
    .bind(new.width)
    .bind(new.height)
    .bind(&new.transcode_reason)
    .fetch_one(pool)
    .await
    .map_err(MuseError::Database)
}

pub async fn get_media_info(pool: &PgPool, play_session_id: i64) -> MuseResult<Option<PlaySessionMediaInfo>> {
    sqlx::query_as::<_, PlaySessionMediaInfo>(
        "SELECT * FROM play_session_media_info WHERE play_session_id = $1",
    )
    .bind(play_session_id)
    .fetch_optional(pool)
    .await
    .map_err(MuseError::Database)
}

/// Find a session previously imported from Tautulli by its `reference_id`
/// (stored as `tautulli_ref_id`). This is MUSE-06's *primary* idempotency
/// guard: the table's own `(account_id, media_item_id, episode_id,
/// started_at)` UNIQUE only dedups a re-run for *resolved* rows (Postgres
/// treats NULL media/episode refs as distinct — see the caveat on
/// [`upsert`]), so an unresolved history row would otherwise be re-inserted
/// on every backfill run. Checking `tautulli_ref_id` first covers both the
/// resolved and unresolved case uniformly.
pub async fn find_by_tautulli_ref(
    pool: &PgPool,
    tautulli_ref_id: i64,
) -> MuseResult<Option<PlaySession>> {
    sqlx::query_as::<_, PlaySession>("SELECT * FROM play_sessions WHERE tautulli_ref_id = $1")
        .bind(tautulli_ref_id)
        .fetch_optional(pool)
        .await
        .map_err(MuseError::Database)
}

/// Find a *natively*-captured session (`tautulli_ref_id IS NULL` — i.e. from
/// MUSE-07's webhook/poller path, not a prior backfill run) that overlaps a
/// Tautulli history row for the same account/media/episode within
/// `tolerance_secs` of `started_at`. Comparisons use `IS NOT DISTINCT FROM`
/// rather than `=` so an *unresolved* Tautulli row (media/episode both
/// `NULL`) can still match an equally-unresolved native row instead of
/// silently never deduping just because SQL treats `NULL <> NULL`.
///
/// Per spec §4-D: during the overlap window both native capture and the
/// backfill can observe the same watch — native capture wins (higher
/// fidelity); the caller should attach `tautulli_ref_id` to the returned row
/// for provenance rather than inserting a second, duplicate session.
pub async fn find_overlapping_native(
    pool: &PgPool,
    account_id: Option<i64>,
    media_item_id: Option<i64>,
    episode_id: Option<i64>,
    started_at: chrono::DateTime<chrono::Utc>,
    tolerance_secs: i64,
) -> MuseResult<Option<PlaySession>> {
    let window_start = started_at - chrono::Duration::seconds(tolerance_secs);
    let window_end = started_at + chrono::Duration::seconds(tolerance_secs);

    sqlx::query_as::<_, PlaySession>(
        r#"
        SELECT * FROM play_sessions
        WHERE tautulli_ref_id IS NULL
          AND account_id IS NOT DISTINCT FROM $1
          AND media_item_id IS NOT DISTINCT FROM $2
          AND episode_id IS NOT DISTINCT FROM $3
          AND started_at BETWEEN $4 AND $5
        ORDER BY started_at
        LIMIT 1
        "#,
    )
    .bind(account_id)
    .bind(media_item_id)
    .bind(episode_id)
    .bind(window_start)
    .bind(window_end)
    .fetch_optional(pool)
    .await
    .map_err(MuseError::Database)
}

/// Attach Tautulli provenance to an existing (native-captured) session
/// without touching any other field — used when the backfill importer finds
/// a native session that already covers a Tautulli history row (see
/// [`find_overlapping_native`]): the native row wins, but we still want
/// `tautulli_ref_id` recorded so the row's Tautulli provenance is visible.
/// A no-op (does not overwrite) if the row already carries a
/// `tautulli_ref_id`, so a later, spurious "overlap" can't clobber earlier
/// provenance.
pub async fn attach_tautulli_ref(
    pool: &PgPool,
    play_session_id: i64,
    tautulli_ref_id: i64,
) -> MuseResult<()> {
    sqlx::query(
        "UPDATE play_sessions SET tautulli_ref_id = $2 \
         WHERE id = $1 AND tautulli_ref_id IS NULL",
    )
    .bind(play_session_id)
    .bind(tautulli_ref_id)
    .execute(pool)
    .await
    .map_err(MuseError::Database)?;
    Ok(())
}

/// BSEED-2: an unresolved session's row, carrying the stored Plex identifying
/// keys the re-resolution pass re-matches against later-arriving `media_items`
/// (see migration `0108_play_sessions_plex_refs.sql`). Only the columns the
/// resolver needs are selected; `media_item_id`/`episode_id` are known-NULL by
/// the `list_unresolved` filter so they aren't re-selected.
#[derive(Debug, Clone, FromRow)]
pub struct UnresolvedSession {
    pub id: i64,
    pub account_id: Option<i64>,
    pub tautulli_ref_id: Option<i64>,
    pub plex_rating_key: Option<String>,
    pub plex_grandparent_rating_key: Option<String>,
    pub plex_grandparent_guid: Option<String>,
    pub plex_guids: serde_json::Value,
}

/// BSEED-2: sessions that never resolved to a library item/episode
/// (`media_item_id IS NULL AND episode_id IS NULL`), oldest-first, bounded by
/// `limit`. These are the rows the re-resolution pass re-matches against the
/// (now arr-populated) catalog. Backed by the partial index added in migration
/// 0108 so the scan stays cheap as resolved sessions accumulate.
pub async fn list_unresolved(pool: &PgPool, limit: i64) -> MuseResult<Vec<UnresolvedSession>> {
    sqlx::query_as::<_, UnresolvedSession>(
        r#"
        SELECT id, account_id, tautulli_ref_id,
               plex_rating_key, plex_grandparent_rating_key,
               plex_grandparent_guid, plex_guids
        FROM play_sessions
        WHERE media_item_id IS NULL AND episode_id IS NULL
        ORDER BY started_at ASC
        LIMIT $1
        "#,
    )
    .bind(limit)
    .fetch_all(pool)
    .await
    .map_err(MuseError::Database)
}

/// Persist the Plex identifying keys a session resolved from (BSEED-2), so a
/// future re-resolution can re-match it offline (no Tautulli round-trip).
/// `guids` is the JSON array of provider-GUID strings. Never touches any other
/// column.
pub async fn set_plex_refs(
    pool: &PgPool,
    play_session_id: i64,
    plex_rating_key: Option<&str>,
    plex_grandparent_rating_key: Option<&str>,
    plex_guids: &serde_json::Value,
    plex_grandparent_guid: Option<&str>,
) -> MuseResult<()> {
    sqlx::query(
        r#"
        UPDATE play_sessions SET
            plex_rating_key = $2,
            plex_grandparent_rating_key = $3,
            plex_guids = $4,
            plex_grandparent_guid = $5
        WHERE id = $1
        "#,
    )
    .bind(play_session_id)
    .bind(plex_rating_key)
    .bind(plex_grandparent_rating_key)
    .bind(plex_guids)
    .bind(plex_grandparent_guid)
    .execute(pool)
    .await
    .map_err(MuseError::Database)?;
    Ok(())
}

/// Outcome of [`set_media_ref`] — distinguishes a clean resolution from one
/// that collided with the `(account_id, media_item_id, episode_id,
/// started_at)` UNIQUE (BSEED-2's flagged hazard: two previously-NULL rows can
/// now resolve to the same key).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetMediaRefOutcome {
    /// The session was updated in place with its resolved media/episode ref.
    Updated,
    /// The update would have duplicated an already-resolved session for the
    /// same `(account, item, episode, started_at)`; this now-redundant
    /// duplicate row was deleted instead (the equivalent watch is already
    /// counted by the surviving row).
    ConflictDeduped,
}

/// BSEED-2: attach a freshly-resolved `(media_item_id, episode_id)` to a
/// previously-unresolved session. Because resolving two distinct NULL-keyed
/// rows can now land on the same `(account_id, media_item_id, episode_id,
/// started_at)` (the table's UNIQUE — Postgres treated the NULL refs as
/// distinct until now), a plain `UPDATE` can raise a unique violation. That is
/// handled, not propagated: on `23505` the redundant duplicate is deleted
/// (the surviving, already-resolved row represents the same watch), keeping
/// re-resolution idempotent and collision-safe.
pub async fn set_media_ref(
    pool: &PgPool,
    play_session_id: i64,
    media_item_id: Option<i64>,
    episode_id: Option<i64>,
) -> MuseResult<SetMediaRefOutcome> {
    let result = sqlx::query(
        "UPDATE play_sessions SET media_item_id = $2, episode_id = $3 WHERE id = $1",
    )
    .bind(play_session_id)
    .bind(media_item_id)
    .bind(episode_id)
    .execute(pool)
    .await;

    match result {
        Ok(_) => Ok(SetMediaRefOutcome::Updated),
        Err(sqlx::Error::Database(db)) if db.code().as_deref() == Some("23505") => {
            // An equivalent, already-resolved session exists for this
            // (account, item, episode, started_at). Drop this duplicate rather
            // than leaving a permanently-unresolvable row behind.
            sqlx::query("DELETE FROM play_sessions WHERE id = $1")
                .bind(play_session_id)
                .execute(pool)
                .await
                .map_err(MuseError::Database)?;
            Ok(SetMediaRefOutcome::ConflictDeduped)
        }
        Err(e) => Err(MuseError::Database(e)),
    }
}

/// One finished session's context fields — the raw input MUSE-10's
/// `taste_model::profile::compute_context_centroids` buckets into
/// weekend/weekday x time-of-day contexts (spec §3.4
/// `taste_context_centroids`, "Friday-night != Sunday-morning !=
/// phone-commute"). Scoped to `is_finished = true` (an abandoned or
/// still-in-progress session shouldn't anchor a "you love this in this
/// context" centroid) and `media_item_id IS NOT NULL` (an unresolved
/// session has nothing to embed).
#[derive(Debug, Clone, FromRow)]
pub struct FinishedSessionContextRow {
    pub media_item_id: i64,
    pub started_hour: Option<i32>,
    pub started_dow: Option<i32>,
}

pub async fn list_finished_context_rows(
    pool: &PgPool,
    account_id: i64,
) -> MuseResult<Vec<FinishedSessionContextRow>> {
    sqlx::query_as::<_, FinishedSessionContextRow>(
        r#"
        SELECT media_item_id, started_hour, started_dow
        FROM play_sessions
        WHERE account_id = $1 AND is_finished = true AND media_item_id IS NOT NULL
        "#,
    )
    .bind(account_id)
    .fetch_all(pool)
    .await
    .map_err(MuseError::Database)
}

// ===========================================================================
// MACT-01 (Plane MUSE #121): `GET /api/sessions/live` + `GET /api/sessions/history`
// ===========================================================================
//
// The missing read path over `play_sessions`. Deliberately agnostic about who
// WRITES the table (epic §8.8 / spec J will make Maestro's plex adapter the
// sole Plex session observer, with Muse's tracker becoming a consumer of its
// event stream) — nothing here references the poller module, assumes it is
// running, or keys behaviour on the poll ingest source string. Both queries
// read `play_events` only for its stored vocabulary (the newest row's
// `event_type`/`received_at`), which is data, not a dependency on the code
// that wrote it. See `web::dashboard`'s MACT-01 section for the source-scan
// test that pins this for both files.

/// A household never has 100 concurrent streams; this is the hard bound on
/// [`list_live`] (see its doc comment) so a corrupted/runaway table can never
/// turn a "who's watching" read into an unbounded scan.
const LIVE_SESSIONS_LIMIT: i64 = 100;

/// Whether a LIVE (`stopped_at IS NULL`) session's player is actually
/// `playing`/`paused` right now, or has gone quiet without a matching stop
/// event ([`classify_session_state`]'s `Stale` case) — a crashed player, a
/// missed stop event, or an ingest outage would otherwise leave a row open
/// (and therefore reported as "playing") forever. `Stale` sessions are
/// reported, never dropped and never shown as playing — see MACT-01's spec.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionPlayState {
    Playing,
    Paused,
    Stale,
}

/// The liveness rule (MACT-01): a session's newest `play_events` row must be
/// within `grace_secs` of `now` to be trusted as still playing at all — an
/// open-but-stale row is `Stale`, never `Playing`/`Paused`. Within the grace
/// window, the newest event's kind decides `Playing` vs `Paused` (a pause
/// event ⇒ paused; anything else inside the window — play/resume/a poll
/// progress tick — ⇒ playing, matching the Plex-vocabulary strings
/// `play_events.event_type` is stored in verbatim: see
/// `tracker::interpret::PlayStateEventKind::to_plex_event_type`).
///
/// A session whose `session_key` never matched any `play_events` row (no
/// `last_event_at` at all) is `Stale` — it has no telemetry to trust as
/// "currently playing".
///
/// Pure function, no I/O — exercised directly by unit tests without a
/// database (see the `tests` module below).
pub fn classify_session_state(
    last_event_type: Option<&str>,
    last_event_at: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
    grace_secs: u64,
) -> SessionPlayState {
    let Some(at) = last_event_at else {
        return SessionPlayState::Stale;
    };
    let elapsed_secs = (now - at).num_seconds();
    // A negative elapsed (clock skew / a future-dated event) is treated the
    // same as "too old" -- neither is a trustworthy "fresh" signal.
    if elapsed_secs < 0 || elapsed_secs as u64 > grace_secs {
        return SessionPlayState::Stale;
    }
    // Enumerate the recognized Plex-vocabulary event kinds explicitly
    // (`tracker::interpret::PlayStateEventKind::to_plex_event_type` is the
    // authoritative mapping this mirrors) rather than defaulting
    // everything-that-isn't-pause to `Playing`. A TERMINAL event
    // (`media.stop`/`media.scrobble`) landing fresh on a row that hasn't
    // been marked `stopped_at` yet (an ingest race) means playback ended,
    // not that it's still playing -- and an unrecognized event type is not
    // a trustworthy "still playing" signal either. Both fall through to
    // `Stale`: the spec's rule is "a pause event => paused; a newer
    // play/progress event => playing" -- a stop is neither.
    match last_event_type {
        Some("media.pause") => SessionPlayState::Paused,
        Some("media.play") | Some("media.resume") => SessionPlayState::Playing,
        _ => SessionPlayState::Stale,
    }
}

/// Raw joined row for one `play_sessions` record + its account/item/decision
/// context + the newest matching `play_events` row (for liveness
/// classification) — the shape [`LIVE_SESSIONS_SQL`] and
/// [`HISTORY_SESSIONS_SQL`] both select into (history simply never populates
/// `last_event_type`/`last_event_at`, since a stopped session's liveness is
/// moot).
#[derive(Debug, Clone, FromRow)]
pub struct SessionJoinRow {
    pub session_id: i64,
    pub account_id: Option<i64>,
    pub account_display_name: Option<String>,
    pub media_item_id: Option<i64>,
    pub episode_id: Option<i64>,
    pub media_metadata_id: Option<i64>,
    pub kind: Option<MediaKind>,
    pub title: Option<String>,
    pub year: Option<i32>,
    pub season_number: Option<i32>,
    pub episode_number: Option<i32>,
    pub episode_title: Option<String>,
    pub session_key: Option<String>,
    pub view_offset_ms: Option<i64>,
    pub duration_ms: Option<i64>,
    pub percent_complete: Option<f32>,
    pub player: Option<String>,
    pub platform: Option<String>,
    pub product: Option<String>,
    pub device: Option<String>,
    pub started_at: DateTime<Utc>,
    pub last_event_type: Option<String>,
    pub last_event_at: Option<DateTime<Utc>>,
    pub video_decision: Option<DecisionKind>,
    pub audio_decision: Option<DecisionKind>,
    pub transcode_decision: Option<DecisionKind>,
    pub container: Option<String>,
    pub video_codec: Option<String>,
    pub audio_codec: Option<String>,
    pub audio_channels: Option<f32>,
    pub video_resolution: Option<String>,
    pub bitrate: Option<i32>,
    pub transcode_reason: Option<String>,
}

/// One [`SessionJoinRow`], already classified into a [`SessionPlayState`] via
/// [`classify_session_state`] — returned by [`list_live`].
#[derive(Debug, Clone)]
pub struct LiveSession {
    pub row: SessionJoinRow,
    pub state: SessionPlayState,
}

/// The shared projection: account, item (title/year/kind/series-episode/
/// media_item_id), position/duration/progress, device, and the joined
/// `play_session_media_info` decision block — deliberately does NOT select
/// `ip_address` (it is on [`crate::models::play_session::PlaySession`] but
/// nothing here needs it, see MACT-01's spec). One query per call site, with
/// a `LATERAL` join for the newest `play_events` row per session rather than
/// an N+1 per-session lookup.
const SESSION_JOIN_SELECT: &str = r#"
    SELECT
        ps.id AS session_id,
        ps.account_id,
        acc.friendly_name AS account_display_name,
        ps.media_item_id,
        ps.episode_id,
        mi.media_metadata_id,
        md.kind,
        md.title,
        md.year,
        se.season_number,
        ep.episode_number,
        ep.title AS episode_title,
        ps.session_key,
        ps.view_offset_ms,
        ps.duration_ms,
        ps.percent_complete,
        ps.player,
        ps.platform,
        ps.product,
        ps.device,
        ps.started_at,
        ev.event_type AS last_event_type,
        ev.received_at AS last_event_at,
        smi.video_decision,
        smi.audio_decision,
        smi.transcode_decision,
        smi.container,
        smi.video_codec,
        smi.audio_codec,
        smi.audio_channels,
        smi.video_resolution,
        smi.bitrate,
        smi.transcode_reason
    FROM play_sessions ps
    LEFT JOIN accounts acc ON acc.id = ps.account_id
    LEFT JOIN media_items mi ON mi.id = ps.media_item_id
    LEFT JOIN media_metadata md ON md.id = mi.media_metadata_id
    LEFT JOIN episodes ep ON ep.id = ps.episode_id
    LEFT JOIN seasons se ON se.id = ep.season_id
    LEFT JOIN play_session_media_info smi ON smi.play_session_id = ps.id
    LEFT JOIN LATERAL (
        SELECT pe.event_type, pe.received_at
        FROM play_events pe
        WHERE pe.session_key = ps.session_key
          -- `session_key` has no uniqueness constraint on either table (Plex
          -- session keys are per-server counters that DO get reused), so an
          -- unbounded correlation can attach a PREVIOUS session's newest
          -- event -- including one that predates this session even
          -- starting -- and falsely mark a dead session as live. Bound the
          -- correlation to this session's own lifetime: no earlier than
          -- `started_at`, no later than `stopped_at` when the session has
          -- one (an open session has no upper bound yet).
          AND pe.received_at >= ps.started_at
          AND (ps.stopped_at IS NULL OR pe.received_at <= ps.stopped_at)
        -- `id DESC` as a deterministic tiebreak: two events can share the
        -- same `received_at` (timestamp granularity), and without a
        -- secondary key `ORDER BY received_at DESC LIMIT 1` picks between
        -- them non-deterministically.
        ORDER BY pe.received_at DESC, pe.id DESC
        LIMIT 1
    ) ev ON true
"#;

/// `GET /api/sessions/live`'s query: open (`stopped_at IS NULL`) sessions,
/// newest first, hard-bounded at [`LIVE_SESSIONS_LIMIT`] — see its doc
/// comment. No caller-supplied limit; a household never has anywhere near
/// 100 concurrent streams.
fn live_sessions_sql() -> String {
    format!(
        "{SESSION_JOIN_SELECT} WHERE ps.stopped_at IS NULL ORDER BY ps.started_at DESC LIMIT $1"
    )
}

/// `GET /api/sessions/history`'s query: stopped sessions, newest first,
/// bounded by the caller's `limit`.
fn history_sessions_sql() -> String {
    format!(
        "{SESSION_JOIN_SELECT} WHERE ps.stopped_at IS NOT NULL ORDER BY ps.started_at DESC LIMIT $1"
    )
}

/// The derived live view: `stopped_at IS NULL` sessions passing (or failing)
/// the [`classify_session_state`] liveness rule. Never fails open to an
/// empty `Vec` on a query error — the caller (`web::dashboard::get_live_sessions`)
/// must NOT let a DB failure render as "nobody is watching" (see MACT-01's
/// spec + `get_gaps`/`get_stats`'s doc comments for the same reasoning).
pub async fn list_live(pool: &PgPool, grace_secs: u64) -> MuseResult<Vec<LiveSession>> {
    let rows = sqlx::query_as::<_, SessionJoinRow>(&live_sessions_sql())
        .bind(LIVE_SESSIONS_LIMIT)
        .fetch_all(pool)
        .await
        .map_err(MuseError::Database)?;

    let now = Utc::now();
    Ok(rows
        .into_iter()
        .map(|row| {
            let state = classify_session_state(
                row.last_event_type.as_deref(),
                row.last_event_at,
                now,
                grace_secs,
            );
            LiveSession { row, state }
        })
        .collect())
}

/// Muse's permanent historical record over stopped sessions (does NOT change
/// when spec J flips who writes `play_sessions`). Same error-propagation
/// posture as [`list_live`] — a query failure is never a false "no history".
pub async fn list_history(pool: &PgPool, limit: i64) -> MuseResult<Vec<SessionJoinRow>> {
    sqlx::query_as::<_, SessionJoinRow>(&history_sessions_sql())
        .bind(limit)
        .fetch_all(pool)
        .await
        .map_err(MuseError::Database)
}

#[cfg(test)]
mod mact01_tests {
    use super::*;
    use chrono::Duration;

    fn at(secs_ago: i64) -> DateTime<Utc> {
        Utc::now() - Duration::seconds(secs_ago)
    }

    #[test]
    fn fresh_pause_event_classifies_as_paused() {
        let now = Utc::now();
        let last = now - Duration::seconds(5);
        assert_eq!(
            classify_session_state(Some("media.pause"), Some(last), now, 60),
            SessionPlayState::Paused
        );
    }

    #[test]
    fn fresh_play_event_classifies_as_playing() {
        let now = Utc::now();
        let last = now - Duration::seconds(5);
        assert_eq!(
            classify_session_state(Some("media.play"), Some(last), now, 60),
            SessionPlayState::Playing
        );
    }

    #[test]
    fn fresh_resume_event_classifies_as_playing() {
        let now = Utc::now();
        let last = now - Duration::seconds(5);
        assert_eq!(
            classify_session_state(Some("media.resume"), Some(last), now, 60),
            SessionPlayState::Playing
        );
    }

    /// The core edge case: `stopped_at IS NULL` alone is NOT "active" — an
    /// open row whose newest event fell outside the grace window is `Stale`,
    /// never `Playing`/`Paused`, however the event was classified.
    #[test]
    fn stale_event_beyond_grace_window_is_stale_even_if_last_kind_was_playing() {
        let now = Utc::now();
        let last = now - Duration::seconds(120);
        assert_eq!(
            classify_session_state(Some("media.play"), Some(last), now, 60),
            SessionPlayState::Stale
        );
        assert_eq!(
            classify_session_state(Some("media.pause"), Some(last), now, 60),
            SessionPlayState::Stale
        );
    }

    #[test]
    fn event_exactly_at_the_grace_boundary_is_not_stale() {
        let now = Utc::now();
        let last = now - Duration::seconds(60);
        assert_eq!(
            classify_session_state(Some("media.play"), Some(last), now, 60),
            SessionPlayState::Playing
        );
    }

    /// Review finding (codex, confirmed): a fresh TERMINAL event
    /// (`media.stop`/`media.scrobble`) on a row that hasn't been marked
    /// `stopped_at` yet (an ingest race between the reconstruction worker
    /// and this read) must NOT classify as `Playing` — the old
    /// `_ => Playing` fallthrough got this wrong for anything that wasn't
    /// literally `media.pause`.
    #[test]
    fn fresh_stop_event_is_not_playing() {
        let now = Utc::now();
        let last = now - Duration::seconds(5);
        assert_eq!(
            classify_session_state(Some("media.stop"), Some(last), now, 60),
            SessionPlayState::Stale
        );
    }

    #[test]
    fn fresh_scrobble_completion_event_is_not_playing() {
        let now = Utc::now();
        let last = now - Duration::seconds(5);
        assert_eq!(
            classify_session_state(Some("media.scrobble"), Some(last), now, 60),
            SessionPlayState::Stale
        );
    }

    #[test]
    fn fresh_unrecognized_event_type_is_not_playing() {
        let now = Utc::now();
        let last = now - Duration::seconds(5);
        assert_eq!(
            classify_session_state(Some("media.rate"), Some(last), now, 60),
            SessionPlayState::Stale
        );
    }

    #[test]
    fn no_matching_event_at_all_is_stale() {
        let now = Utc::now();
        assert_eq!(
            classify_session_state(None, None, now, 60),
            SessionPlayState::Stale
        );
    }

    #[test]
    fn future_dated_event_is_treated_as_stale_not_playing() {
        // Clock skew / a future-dated row is not a trustworthy "fresh" signal.
        let now = Utc::now();
        let last = now + Duration::seconds(30);
        assert_eq!(
            classify_session_state(Some("media.play"), Some(last), now, 60),
            SessionPlayState::Stale
        );
    }

    #[test]
    fn helper_at_matches_expected_elapsed() {
        // Sanity check on the test helper itself.
        let now = Utc::now();
        let ago = at(10);
        assert!((now - ago).num_seconds() >= 9 && (now - ago).num_seconds() <= 11);
    }

    /// MACT-01's sqlx integration test: an open session + a recent
    /// `play_events` row appears in [`list_live`] with its joined media
    /// info, and a stopped session appears only in [`list_history`]. Gated
    /// on `MUSE_TEST_DATABASE_URL` exactly like `repo::dashboard`'s
    /// `db_gated` module — skipped (not failed) when no live DB is
    /// configured for this harness.
    #[cfg(test)]
    mod db_gated {
        use super::*;
        use crate::models::library::{LibraryKind, NewLibrary};
        use crate::models::media_item::NewMediaItem;
        use crate::models::media_metadata::{MediaKind, NewMediaMetadata};
        use crate::models::play_event::NewPlayEvent;
        use sqlx::PgPool;

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

        #[tokio::test]
        async fn live_finds_an_open_session_with_a_recent_event_and_history_finds_only_the_stopped_one(
        ) {
            let Some(pool) = test_pool_or_skip(
                "live_finds_an_open_session_with_a_recent_event_and_history_finds_only_the_stopped_one",
            )
            .await
            else {
                return;
            };

            let (account_id,): (i64,) = sqlx::query_as(
                "INSERT INTO accounts (username, friendly_name, is_home_user, is_primary) \
                 VALUES ('mact01-fixture', 'MACT01 Fixture', true, false) RETURNING id",
            )
            .fetch_one(&pool)
            .await
            .expect("seed account");
            let suffix = account_id;

            let library = crate::repo::library::create(
                &pool,
                &NewLibrary {
                    name: format!("mact01-lib-{suffix}"),
                    kind: LibraryKind::Movie,
                    root_folder: "/media/mact01-test".to_string(),
                    source_arr_name: None,
                    source_arr_url: None,
                },
            )
            .await
            .expect("create library");

            let metadata = crate::repo::media_metadata::upsert_by_tmdb(
                &pool,
                &NewMediaMetadata {
                    kind: MediaKind::Movie,
                    tmdb_id: Some(format!("mact01-{suffix}")),
                    tvdb_id: None,
                    imdb_id: None,
                    provider_ids: serde_json::json!({}),
                    title: format!("MACT01 Fixture Film {suffix}"),
                    sort_title: None,
                    original_title: None,
                    original_language: None,
                    status: None,
                    overview: None,
                    studio: None,
                    network: None,
                    runtime_minutes: Some(100),
                    year: Some(2021),
                    images: serde_json::json!({}),
                },
            )
            .await
            .expect("upsert media_metadata");

            let item = crate::repo::media_item::upsert(
                &pool,
                &NewMediaItem {
                    library_id: library.id,
                    media_metadata_id: metadata.id,
                    path: format!("/media/mact01-test/film-{suffix}.mkv"),
                    monitored: true,
                    quality_profile_id: None,
                    minimum_availability: None,
                    plex_rating_key: Some(format!("mact01-rk-{suffix}")),
                    added_at: None,
                },
            )
            .await
            .expect("upsert media_item");

            let session_key = format!("mact01-session-{suffix}");
            let now = Utc::now();

            // An OPEN session (stopped_at = None) whose newest play_events row
            // is fresh -- must appear in `list_live` as "playing".
            let open_session = upsert(
                &pool,
                &NewPlaySession {
                    account_id: Some(account_id),
                    media_item_id: Some(item.id),
                    episode_id: None,
                    session_key: Some(session_key.clone()),
                    tautulli_ref_id: None,
                    started_at: now,
                    stopped_at: None,
                    duration_ms: Some(6_000_000),
                    watched_ms: Some(1_000_000),
                    view_offset_ms: Some(1_000_000),
                    percent_complete: Some(0.48),
                    paused_counter: 0,
                    paused_ms: 0,
                    is_finished: false,
                    is_abandoned: false,
                    player: Some("Living Room".to_string()),
                    platform: Some("Plex Web".to_string()),
                    product: Some("Plex Web".to_string()),
                    device: Some("Chrome".to_string()),
                    ip_address: None,
                    started_hour: None,
                    started_dow: None,
                    is_cinema_context: None,
                },
            )
            .await
            .expect("upsert open session");

            crate::repo::play_event::insert(
                &pool,
                &NewPlayEvent {
                    source: "mact01_test".to_string(),
                    event_type: "media.play".to_string(),
                    account_ref: None,
                    session_key: Some(session_key.clone()),
                    rating_key: Some(format!("mact01-rk-{suffix}")),
                    view_offset_ms: Some(1_000_000),
                    player: Some("Living Room".to_string()),
                    platform: Some("Plex Web".to_string()),
                    product: Some("Plex Web".to_string()),
                    device: Some("Chrome".to_string()),
                    ip_address: None,
                    raw: serde_json::json!({}),
                },
            )
            .await
            .expect("insert play_event");

            // Review finding (codex, confirmed): the joined media-info decision
            // block was asserted nowhere -- populate it and check it comes back
            // verbatim through `list_live`.
            upsert_media_info(
                &pool,
                open_session.id,
                &NewPlaySessionMediaInfo {
                    video_decision: Some(DecisionKind::Transcode),
                    audio_decision: Some(DecisionKind::Copy),
                    transcode_decision: Some(DecisionKind::Transcode),
                    container: Some("mkv".to_string()),
                    video_codec: Some("hevc".to_string()),
                    audio_codec: Some("aac".to_string()),
                    audio_channels: Some(2.0),
                    video_resolution: Some("1080".to_string()),
                    bitrate: Some(8_000_000),
                    width: Some(1920),
                    height: Some(1080),
                    transcode_reason: Some("video codec unsupported by device".to_string()),
                },
            )
            .await
            .expect("upsert play_session_media_info");

            // A STOPPED session -- must appear only in `list_history`, never
            // in `list_live`.
            let stopped_session = upsert(
                &pool,
                &NewPlaySession {
                    account_id: Some(account_id),
                    media_item_id: Some(item.id),
                    episode_id: None,
                    session_key: Some(format!("{session_key}-stopped")),
                    tautulli_ref_id: None,
                    started_at: now - chrono::Duration::hours(2),
                    stopped_at: Some(now - chrono::Duration::hours(1)),
                    duration_ms: Some(6_000_000),
                    watched_ms: Some(6_000_000),
                    view_offset_ms: Some(6_000_000),
                    percent_complete: Some(1.0),
                    paused_counter: 0,
                    paused_ms: 0,
                    is_finished: true,
                    is_abandoned: false,
                    player: Some("Living Room".to_string()),
                    platform: Some("Plex Web".to_string()),
                    product: Some("Plex Web".to_string()),
                    device: Some("Chrome".to_string()),
                    ip_address: None,
                    started_hour: None,
                    started_dow: None,
                    is_cinema_context: None,
                },
            )
            .await
            .expect("upsert stopped session");

            let live = list_live(&pool, 60).await.expect("list_live");
            let live_hit = live
                .iter()
                .find(|s| s.row.session_id == open_session.id)
                .expect("open session must appear in list_live");
            assert_eq!(live_hit.state, SessionPlayState::Playing);
            assert_eq!(live_hit.row.media_metadata_id, Some(metadata.id));
            assert_eq!(live_hit.row.title.as_deref(), Some(metadata.title.as_str()));
            // The joined `play_session_media_info` decision block, verbatim.
            assert_eq!(live_hit.row.video_decision, Some(DecisionKind::Transcode));
            assert_eq!(live_hit.row.audio_decision, Some(DecisionKind::Copy));
            assert_eq!(live_hit.row.transcode_decision, Some(DecisionKind::Transcode));
            assert_eq!(live_hit.row.container.as_deref(), Some("mkv"));
            assert_eq!(live_hit.row.video_codec.as_deref(), Some("hevc"));
            assert_eq!(live_hit.row.audio_codec.as_deref(), Some("aac"));
            assert_eq!(live_hit.row.audio_channels, Some(2.0));
            assert_eq!(live_hit.row.video_resolution.as_deref(), Some("1080"));
            assert_eq!(live_hit.row.bitrate, Some(8_000_000));
            assert_eq!(
                live_hit.row.transcode_reason.as_deref(),
                Some("video codec unsupported by device")
            );
            assert!(live
                .iter()
                .all(|s| s.row.session_id != stopped_session.id));

            let history = list_history(&pool, 500).await.expect("list_history");
            assert!(history
                .iter()
                .any(|s| s.session_id == stopped_session.id));
            assert!(history.iter().all(|s| s.session_id != open_session.id));
        }

        /// Review finding (codex, confirmed): `session_key` carries no
        /// uniqueness constraint on either table (Plex session keys are
        /// per-server counters that DO get reused), so an unbounded
        /// correlated-subquery join could attach a PREVIOUS session's event
        /// to a brand new session reusing the same key. Proves the fix: an
        /// older, STOPPED session's event does not leak onto a new OPEN
        /// session that reuses its `session_key`.
        #[tokio::test]
        async fn reused_session_key_does_not_leak_an_older_sessions_event() {
            let Some(pool) = test_pool_or_skip(
                "reused_session_key_does_not_leak_an_older_sessions_event",
            )
            .await
            else {
                return;
            };

            let (account_id,): (i64,) = sqlx::query_as(
                "INSERT INTO accounts (username, friendly_name, is_home_user, is_primary) \
                 VALUES ('mact01-reuse-fixture', 'MACT01 Reuse Fixture', true, false) \
                 RETURNING id",
            )
            .fetch_one(&pool)
            .await
            .expect("seed account");
            let suffix = account_id;

            let library = crate::repo::library::create(
                &pool,
                &NewLibrary {
                    name: format!("mact01-reuse-lib-{suffix}"),
                    kind: LibraryKind::Movie,
                    root_folder: "/media/mact01-reuse-test".to_string(),
                    source_arr_name: None,
                    source_arr_url: None,
                },
            )
            .await
            .expect("create library");

            let metadata = crate::repo::media_metadata::upsert_by_tmdb(
                &pool,
                &NewMediaMetadata {
                    kind: MediaKind::Movie,
                    tmdb_id: Some(format!("mact01-reuse-{suffix}")),
                    tvdb_id: None,
                    imdb_id: None,
                    provider_ids: serde_json::json!({}),
                    title: format!("MACT01 Reuse Fixture Film {suffix}"),
                    sort_title: None,
                    original_title: None,
                    original_language: None,
                    status: None,
                    overview: None,
                    studio: None,
                    network: None,
                    runtime_minutes: Some(100),
                    year: Some(2021),
                    images: serde_json::json!({}),
                },
            )
            .await
            .expect("upsert media_metadata");

            let item = crate::repo::media_item::upsert(
                &pool,
                &NewMediaItem {
                    library_id: library.id,
                    media_metadata_id: metadata.id,
                    path: format!("/media/mact01-reuse-test/film-{suffix}.mkv"),
                    monitored: true,
                    quality_profile_id: None,
                    minimum_availability: None,
                    plex_rating_key: Some(format!("mact01-reuse-rk-{suffix}")),
                    added_at: None,
                },
            )
            .await
            .expect("upsert media_item");

            let reused_key = format!("mact01-reused-key-{suffix}");
            let now = Utc::now();

            // Older session: started 2h ago, stopped 1h ago.
            let older_started_at = now - chrono::Duration::hours(2);
            let older_stopped_at = now - chrono::Duration::hours(1);
            let older_session = upsert(
                &pool,
                &NewPlaySession {
                    account_id: Some(account_id),
                    media_item_id: Some(item.id),
                    episode_id: None,
                    session_key: Some(reused_key.clone()),
                    tautulli_ref_id: None,
                    started_at: older_started_at,
                    stopped_at: Some(older_stopped_at),
                    duration_ms: Some(6_000_000),
                    watched_ms: Some(6_000_000),
                    view_offset_ms: Some(6_000_000),
                    percent_complete: Some(1.0),
                    paused_counter: 0,
                    paused_ms: 0,
                    is_finished: true,
                    is_abandoned: false,
                    player: Some("Living Room".to_string()),
                    platform: Some("Plex Web".to_string()),
                    product: Some("Plex Web".to_string()),
                    device: Some("Chrome".to_string()),
                    ip_address: None,
                    started_hour: None,
                    started_dow: None,
                    is_cinema_context: None,
                },
            )
            .await
            .expect("upsert older session");

            // The older session's event, backdated (via raw SQL -- `insert()`
            // always stamps `received_at = now()`, which cannot express "this
            // event happened within the OLDER session's window" for a test
            // fixture) to land squarely inside the older session's lifetime,
            // well before the new session below even starts.
            let older_event_at = now - chrono::Duration::minutes(90);
            sqlx::query(
                r#"
                INSERT INTO play_events
                    (received_at, source, event_type, session_key, rating_key, raw)
                VALUES ($1, 'mact01_test', 'media.play', $2, $3, '{}'::jsonb)
                "#,
            )
            .bind(older_event_at)
            .bind(&reused_key)
            .bind(format!("mact01-reuse-rk-{suffix}"))
            .execute(&pool)
            .await
            .expect("insert backdated play_event for the older session");

            // A NEW session reusing the SAME `session_key`, opened after the
            // older session stopped, with NO event of its own yet.
            let new_session = upsert(
                &pool,
                &NewPlaySession {
                    account_id: Some(account_id),
                    media_item_id: Some(item.id),
                    episode_id: None,
                    session_key: Some(reused_key.clone()),
                    tautulli_ref_id: None,
                    started_at: now,
                    stopped_at: None,
                    duration_ms: Some(6_000_000),
                    watched_ms: Some(0),
                    view_offset_ms: Some(0),
                    percent_complete: Some(0.0),
                    paused_counter: 0,
                    paused_ms: 0,
                    is_finished: false,
                    is_abandoned: false,
                    player: Some("Living Room".to_string()),
                    platform: Some("Plex Web".to_string()),
                    product: Some("Plex Web".to_string()),
                    device: Some("Chrome".to_string()),
                    ip_address: None,
                    started_hour: None,
                    started_dow: None,
                    is_cinema_context: None,
                },
            )
            .await
            .expect("upsert new session reusing the key");

            let live = list_live(&pool, 60).await.expect("list_live");
            let new_hit = live
                .iter()
                .find(|s| s.row.session_id == new_session.id)
                .expect("the new session must appear in list_live");

            // The bug: without the `received_at >= ps.started_at` bound, the
            // LATERAL join would attach the OLDER session's 90-minutes-ago
            // event to this brand new session (same `session_key`), and
            // 90 minutes is well outside any sane grace window -- but the
            // deeper bug is attaching it AT ALL, since that event predates
            // `new_session.started_at` entirely. Assert it was NOT attached.
            assert_eq!(
                new_hit.row.last_event_at, None,
                "the older, out-of-window session's event must not be joined onto \
                 the new session that reused its session_key"
            );
            assert_eq!(new_hit.row.last_event_type, None);
            // With no matching event, the new session is honestly `Stale`,
            // never fabricated as `Playing` from a leaked older event.
            assert_eq!(new_hit.state, SessionPlayState::Stale);

            // The older session must never appear in `list_live` at all
            // (it's stopped) regardless of the key reuse.
            assert!(live.iter().all(|s| s.row.session_id != older_session.id));
        }
    }
}
