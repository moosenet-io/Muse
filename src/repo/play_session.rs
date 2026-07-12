//! Repo functions for `play_sessions` + `play_session_media_info`.

use sqlx::{FromRow, PgPool};

use crate::error::{MuseError, MuseResult};
use crate::models::play_session::{
    NewPlaySession, NewPlaySessionMediaInfo, PlaySession, PlaySessionMediaInfo,
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
