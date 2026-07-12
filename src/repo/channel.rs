//! Repo functions for `channels` / `channel_runs` / `channel_programs`
//! (MUSE-23). This is the data-access seam only — composing a schedule
//! (MUSE-24), driving playback (MUSE-25), and rendering the guide
//! (MUSE-27/28) are separate, later items.

use chrono::{DateTime, Utc};
use sqlx::PgPool;

use crate::error::{MuseError, MuseResult};
use crate::models::channel::{
    Channel, ChannelProgram, ChannelRun, ChannelRunStatus, NewChannel, NewChannelProgram,
    NewChannelRun,
};

// --- channels ------------------------------------------------------------

pub async fn create_channel(pool: &PgPool, new: &NewChannel) -> MuseResult<Channel> {
    sqlx::query_as::<_, Channel>(
        r#"
        INSERT INTO channels (
            account_id, name, kind, mode, channel_number, target_client_id,
            directive, rules, is_preset
        )
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
        RETURNING *
        "#,
    )
    .bind(new.account_id)
    .bind(&new.name)
    .bind(new.kind)
    .bind(new.mode)
    .bind(new.channel_number)
    .bind(new.target_client_id)
    .bind(&new.directive)
    .bind(&new.rules)
    .bind(new.is_preset)
    .fetch_one(pool)
    .await
    .map_err(MuseError::Database)
}

pub async fn get_channel(pool: &PgPool, id: i64) -> MuseResult<Channel> {
    sqlx::query_as::<_, Channel>("SELECT * FROM channels WHERE id = $1")
        .bind(id)
        .fetch_optional(pool)
        .await
        .map_err(MuseError::Database)?
        .ok_or_else(|| MuseError::NotFound(format!("channel {id} not found")))
}

pub async fn list_channels(pool: &PgPool, account_id: Option<i64>) -> MuseResult<Vec<Channel>> {
    sqlx::query_as::<_, Channel>(
        "SELECT * FROM channels WHERE ($1::bigint IS NULL OR account_id = $1) ORDER BY id",
    )
    .bind(account_id)
    .fetch_all(pool)
    .await
    .map_err(MuseError::Database)
}

pub async fn list_presets(pool: &PgPool) -> MuseResult<Vec<Channel>> {
    sqlx::query_as::<_, Channel>("SELECT * FROM channels WHERE is_preset = true ORDER BY id")
        .fetch_all(pool)
        .await
        .map_err(MuseError::Database)
}

// --- channel_runs ----------------------------------------------------------

pub async fn create_run(pool: &PgPool, new: &NewChannelRun) -> MuseResult<ChannelRun> {
    sqlx::query_as::<_, ChannelRun>(
        r#"
        INSERT INTO channel_runs (
            channel_id, account_id, target_client_id, plex_play_queue_id,
            schedule, total_duration_ms
        )
        VALUES ($1, $2, $3, $4, $5, $6)
        RETURNING *
        "#,
    )
    .bind(new.channel_id)
    .bind(new.account_id)
    .bind(new.target_client_id)
    .bind(&new.plex_play_queue_id)
    .bind(&new.schedule)
    .bind(new.total_duration_ms)
    .fetch_one(pool)
    .await
    .map_err(MuseError::Database)
}

pub async fn get_run(pool: &PgPool, id: i64) -> MuseResult<ChannelRun> {
    sqlx::query_as::<_, ChannelRun>("SELECT * FROM channel_runs WHERE id = $1")
        .bind(id)
        .fetch_optional(pool)
        .await
        .map_err(MuseError::Database)?
        .ok_or_else(|| MuseError::NotFound(format!("channel_run {id} not found")))
}

pub async fn list_runs_by_channel(pool: &PgPool, channel_id: i64) -> MuseResult<Vec<ChannelRun>> {
    sqlx::query_as::<_, ChannelRun>(
        "SELECT * FROM channel_runs WHERE channel_id = $1 ORDER BY composed_at DESC",
    )
    .bind(channel_id)
    .fetch_all(pool)
    .await
    .map_err(MuseError::Database)
}

pub async fn set_run_status(
    pool: &PgPool,
    id: i64,
    status: ChannelRunStatus,
) -> MuseResult<ChannelRun> {
    let (started_at, ended_at): (Option<DateTime<Utc>>, Option<DateTime<Utc>>) = match status {
        ChannelRunStatus::Playing => (Some(Utc::now()), None),
        ChannelRunStatus::Stopped | ChannelRunStatus::Completed => (None, Some(Utc::now())),
        ChannelRunStatus::Composed | ChannelRunStatus::Paused => (None, None),
    };

    sqlx::query_as::<_, ChannelRun>(
        r#"
        UPDATE channel_runs
        SET status = $2,
            started_at = COALESCE(started_at, $3),
            ended_at = COALESCE($4, ended_at)
        WHERE id = $1
        RETURNING *
        "#,
    )
    .bind(id)
    .bind(status)
    .bind(started_at)
    .bind(ended_at)
    .fetch_optional(pool)
    .await
    .map_err(MuseError::Database)?
    .ok_or_else(|| MuseError::NotFound(format!("channel_run {id} not found")))
}

// --- channel_programs (the linear EPG grid) -------------------------------

pub async fn create_program(pool: &PgPool, new: &NewChannelProgram) -> MuseResult<ChannelProgram> {
    sqlx::query_as::<_, ChannelProgram>(
        r#"
        INSERT INTO channel_programs (
            channel_id, item_type, media_item_id, episode_id, interstitial_id,
            title, subtitle, description, artwork_url, start_at, end_at,
            duration_ms, rationale
        )
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13)
        RETURNING *
        "#,
    )
    .bind(new.channel_id)
    .bind(new.item_type)
    .bind(new.media_item_id)
    .bind(new.episode_id)
    .bind(new.interstitial_id)
    .bind(&new.title)
    .bind(&new.subtitle)
    .bind(&new.description)
    .bind(&new.artwork_url)
    .bind(new.start_at)
    .bind(new.end_at)
    .bind(new.duration_ms)
    .bind(&new.rationale)
    .fetch_one(pool)
    .await
    .map_err(MuseError::Database)
}

/// The grid a guide render needs: every program for a channel whose window
/// overlaps `[from, to)`, ordered by start time (now/next/later).
pub async fn list_programs_in_window(
    pool: &PgPool,
    channel_id: i64,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
) -> MuseResult<Vec<ChannelProgram>> {
    sqlx::query_as::<_, ChannelProgram>(
        r#"
        SELECT * FROM channel_programs
        WHERE channel_id = $1 AND start_at < $3 AND end_at > $2
        ORDER BY start_at
        "#,
    )
    .bind(channel_id)
    .bind(from)
    .bind(to)
    .fetch_all(pool)
    .await
    .map_err(MuseError::Database)
}

/// The single program airing "now" for a linear channel, if any.
pub async fn current_program(
    pool: &PgPool,
    channel_id: i64,
    at: DateTime<Utc>,
) -> MuseResult<Option<ChannelProgram>> {
    sqlx::query_as::<_, ChannelProgram>(
        r#"
        SELECT * FROM channel_programs
        WHERE channel_id = $1 AND start_at <= $2 AND end_at > $2
        ORDER BY start_at DESC
        LIMIT 1
        "#,
    )
    .bind(channel_id)
    .bind(at)
    .fetch_optional(pool)
    .await
    .map_err(MuseError::Database)
}

/// Records that a scheduled program actually played, by attaching the
/// telemetry event id once `play_events` (MUSE-03) exists. `play_event_id`
/// has no FK yet (see the migration's seam comment) — this is a plain
/// column update.
pub async fn set_program_play_event(
    pool: &PgPool,
    id: i64,
    play_event_id: i64,
) -> MuseResult<ChannelProgram> {
    sqlx::query_as::<_, ChannelProgram>(
        "UPDATE channel_programs SET play_event_id = $2 WHERE id = $1 RETURNING *",
    )
    .bind(id)
    .bind(play_event_id)
    .fetch_optional(pool)
    .await
    .map_err(MuseError::Database)?
    .ok_or_else(|| MuseError::NotFound(format!("channel_program {id} not found")))
}
