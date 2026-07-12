//! Repo functions for `watch_stats` / `ratings` / `watchlist`.

use sqlx::PgPool;

use crate::error::{MuseError, MuseResult};
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
