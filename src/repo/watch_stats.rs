//! Repo functions for `watch_stats` / `ratings` / `watchlist`.

use chrono::{DateTime, Utc};
use sqlx::{FromRow, PgPool};

use crate::error::{MuseError, MuseResult};
use crate::models::media_metadata::MediaKind;
use crate::models::watch_stats::{NewWatchStats, Rating, WatchStats, WatchlistEntry};

// --- watch_stats -----------------------------------------------------

/// Full-replace upsert — the recompute worker always writes the complete
/// recomputed aggregate for an (account, item) pair, never a delta.
pub async fn upsert_watch_stats(pool: &PgPool, new: &NewWatchStats) -> MuseResult<WatchStats> {
    sqlx::query_as::<_, WatchStats>(
        r#"
        INSERT INTO watch_stats (
            account_id, media_item_id, play_count, finished_count, rewatch_count,
            total_watched_ms, avg_percent, last_watched_at, abandoned, first_watched_at
        )
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
        ON CONFLICT (account_id, media_item_id) DO UPDATE SET
            play_count = EXCLUDED.play_count,
            finished_count = EXCLUDED.finished_count,
            rewatch_count = EXCLUDED.rewatch_count,
            total_watched_ms = EXCLUDED.total_watched_ms,
            avg_percent = EXCLUDED.avg_percent,
            last_watched_at = EXCLUDED.last_watched_at,
            abandoned = EXCLUDED.abandoned,
            first_watched_at = EXCLUDED.first_watched_at
        RETURNING *
        "#,
    )
    .bind(new.account_id)
    .bind(new.media_item_id)
    .bind(new.play_count)
    .bind(new.finished_count)
    .bind(new.rewatch_count)
    .bind(new.total_watched_ms)
    .bind(new.avg_percent)
    .bind(new.last_watched_at)
    .bind(new.abandoned)
    .bind(new.first_watched_at)
    .fetch_one(pool)
    .await
    .map_err(MuseError::Database)
}

pub async fn get_watch_stats(pool: &PgPool, account_id: i64, media_item_id: i64) -> MuseResult<Option<WatchStats>> {
    sqlx::query_as::<_, WatchStats>(
        "SELECT * FROM watch_stats WHERE account_id = $1 AND media_item_id = $2",
    )
    .bind(account_id)
    .bind(media_item_id)
    .fetch_optional(pool)
    .await
    .map_err(MuseError::Database)
}

pub async fn list_watch_stats_for_account(pool: &PgPool, account_id: i64) -> MuseResult<Vec<WatchStats>> {
    sqlx::query_as::<_, WatchStats>(
        "SELECT * FROM watch_stats WHERE account_id = $1 ORDER BY last_watched_at DESC NULLS LAST",
    )
    .bind(account_id)
    .fetch_all(pool)
    .await
    .map_err(MuseError::Database)
}

/// One "continue watching" candidate row (MUSE-11 curation §6): a title the
/// account has started but neither finished nor abandoned, joined with the
/// display fields the curation layer needs so it doesn't have to N+1 back to
/// `media_items`/`media_metadata` per row.
#[derive(Debug, Clone, FromRow)]
pub struct OnDeckRow {
    pub media_item_id: i64,
    pub media_metadata_id: i64,
    pub title: String,
    pub year: Option<i32>,
    pub kind: MediaKind,
    pub avg_percent: Option<f32>,
    pub last_watched_at: Option<DateTime<Utc>>,
}

/// MUSE-11: on-deck / continue-watching candidates for `account_id` —
/// `watch_stats` rows that are neither finished (`finished_count = 0`) nor
/// abandoned, with a recorded, non-trivial `avg_percent` (excludes a session
/// that barely started, and excludes anything essentially done, which
/// `watch_stats::recompute` would ordinarily have already marked finished —
/// the `< 95` guard is a defensive belt-and-suspenders against a not-yet-
/// recomputed edge case, not the primary "is this finished" signal).
/// Ordered most-recently-watched first, the natural "pick this back up"
/// order.
pub async fn list_on_deck(pool: &PgPool, account_id: i64, limit: i64) -> MuseResult<Vec<OnDeckRow>> {
    sqlx::query_as::<_, OnDeckRow>(
        r#"
        SELECT
            mi.id AS media_item_id,
            mm.id AS media_metadata_id,
            mm.title AS title,
            mm.year AS year,
            mm.kind AS kind,
            ws.avg_percent AS avg_percent,
            ws.last_watched_at AS last_watched_at
        FROM watch_stats ws
        JOIN media_items mi ON mi.id = ws.media_item_id
        JOIN media_metadata mm ON mm.id = mi.media_metadata_id
        WHERE ws.account_id = $1
          AND ws.finished_count = 0
          AND ws.abandoned = false
          AND ws.avg_percent IS NOT NULL
          AND ws.avg_percent > 0
          AND ws.avg_percent < 95
        ORDER BY ws.last_watched_at DESC NULLS LAST
        LIMIT $2
        "#,
    )
    .bind(account_id)
    .bind(limit)
    .fetch_all(pool)
    .await
    .map_err(MuseError::Database)
}

/// One abandoned-title candidate for MUSE-12's abandonment-insight
/// generator: an `watch_stats.abandoned = true` row joined with the display
/// fields the generator needs (mirrors [`OnDeckRow`]'s shape/rationale).
#[derive(Debug, Clone, FromRow)]
pub struct AbandonedRow {
    pub media_item_id: i64,
    pub media_metadata_id: i64,
    pub title: String,
    pub year: Option<i32>,
    pub kind: MediaKind,
    pub avg_percent: Option<f32>,
    pub last_watched_at: Option<DateTime<Utc>>,
}

/// MUSE-12: abandoned titles for `account_id` — never finished, and flagged
/// `abandoned = true` (see `taste_model::signals::SIGNAL_ABANDONED`).
/// Ordered most-recently-abandoned first (the freshest "give it another
/// shot" candidate).
pub async fn list_abandoned(pool: &PgPool, account_id: i64, limit: i64) -> MuseResult<Vec<AbandonedRow>> {
    sqlx::query_as::<_, AbandonedRow>(
        r#"
        SELECT
            mi.id AS media_item_id,
            mm.id AS media_metadata_id,
            mm.title AS title,
            mm.year AS year,
            mm.kind AS kind,
            ws.avg_percent AS avg_percent,
            ws.last_watched_at AS last_watched_at
        FROM watch_stats ws
        JOIN media_items mi ON mi.id = ws.media_item_id
        JOIN media_metadata mm ON mm.id = mi.media_metadata_id
        WHERE ws.account_id = $1
          AND ws.abandoned = true
          AND ws.finished_count = 0
        ORDER BY ws.last_watched_at DESC NULLS LAST
        LIMIT $2
        "#,
    )
    .bind(account_id)
    .bind(limit)
    .fetch_all(pool)
    .await
    .map_err(MuseError::Database)
}

/// MUSE-12: how many *other* accounts finished this `media_item_id` — the
/// "or that others finished" abandonment-insight grounding signal (spec
/// MUSE-12). Deliberately a count rather than a list: the generator only
/// needs "did anyone else push through this", never another account's
/// identity (multi-user isolation — no cross-account data leaves this
/// query as anything but an aggregate).
pub async fn count_other_accounts_finished(
    pool: &PgPool,
    media_item_id: i64,
    exclude_account_id: i64,
) -> MuseResult<i64> {
    sqlx::query_scalar::<_, i64>(
        r#"
        SELECT COUNT(*) FROM watch_stats
        WHERE media_item_id = $1 AND account_id != $2 AND finished_count > 0
        "#,
    )
    .bind(media_item_id)
    .bind(exclude_account_id)
    .fetch_one(pool)
    .await
    .map_err(MuseError::Database)
}

// --- ratings -----------------------------------------------------------

pub async fn upsert_rating(
    pool: &PgPool,
    account_id: i64,
    media_item_id: i64,
    rating: f32,
    rated_at: chrono::DateTime<chrono::Utc>,
) -> MuseResult<Rating> {
    sqlx::query_as::<_, Rating>(
        r#"
        INSERT INTO ratings (account_id, media_item_id, rating, rated_at)
        VALUES ($1, $2, $3, $4)
        ON CONFLICT (account_id, media_item_id) DO UPDATE SET
            rating = EXCLUDED.rating,
            rated_at = EXCLUDED.rated_at
        RETURNING *
        "#,
    )
    .bind(account_id)
    .bind(media_item_id)
    .bind(rating)
    .bind(rated_at)
    .fetch_one(pool)
    .await
    .map_err(MuseError::Database)
}

pub async fn list_ratings_for_account(pool: &PgPool, account_id: i64) -> MuseResult<Vec<Rating>> {
    sqlx::query_as::<_, Rating>("SELECT * FROM ratings WHERE account_id = $1 ORDER BY rated_at DESC NULLS LAST")
        .bind(account_id)
        .fetch_all(pool)
        .await
        .map_err(MuseError::Database)
}

// --- watchlist -----------------------------------------------------------

pub async fn add_to_watchlist(
    pool: &PgPool,
    account_id: i64,
    media_item_id: i64,
    added_at: chrono::DateTime<chrono::Utc>,
) -> MuseResult<WatchlistEntry> {
    sqlx::query_as::<_, WatchlistEntry>(
        r#"
        INSERT INTO watchlist (account_id, media_item_id, added_at)
        VALUES ($1, $2, $3)
        ON CONFLICT (account_id, media_item_id) DO UPDATE SET
            added_at = EXCLUDED.added_at,
            removed_at = NULL
        RETURNING *
        "#,
    )
    .bind(account_id)
    .bind(media_item_id)
    .bind(added_at)
    .fetch_one(pool)
    .await
    .map_err(MuseError::Database)
}

pub async fn remove_from_watchlist(
    pool: &PgPool,
    account_id: i64,
    media_item_id: i64,
    removed_at: chrono::DateTime<chrono::Utc>,
) -> MuseResult<()> {
    sqlx::query("UPDATE watchlist SET removed_at = $3 WHERE account_id = $1 AND media_item_id = $2")
        .bind(account_id)
        .bind(media_item_id)
        .bind(removed_at)
        .execute(pool)
        .await
        .map_err(MuseError::Database)?;
    Ok(())
}

/// Mark a watchlist entry fulfilled — flipped once `watch_stats`/`play_sessions`
/// shows the item was actually watched (intent -> action signal for taste).
pub async fn mark_fulfilled(pool: &PgPool, account_id: i64, media_item_id: i64) -> MuseResult<()> {
    sqlx::query("UPDATE watchlist SET fulfilled = true WHERE account_id = $1 AND media_item_id = $2")
        .bind(account_id)
        .bind(media_item_id)
        .execute(pool)
        .await
        .map_err(MuseError::Database)?;
    Ok(())
}

pub async fn list_watchlist_for_account(pool: &PgPool, account_id: i64) -> MuseResult<Vec<WatchlistEntry>> {
    sqlx::query_as::<_, WatchlistEntry>(
        "SELECT * FROM watchlist WHERE account_id = $1 ORDER BY added_at DESC NULLS LAST",
    )
    .bind(account_id)
    .fetch_all(pool)
    .await
    .map_err(MuseError::Database)
}
