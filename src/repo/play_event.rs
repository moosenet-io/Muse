//! Repo functions for `play_events` — the append-only raw telemetry stream.

use sqlx::PgPool;

use crate::error::{MuseError, MuseResult};
use crate::models::play_event::{NewPlayEvent, PlayEvent};

/// Insert a raw event. The table's UNIQUE constraint on
/// `(source, event_type, session_key, view_offset_ms)` makes a duplicate
/// delivery (webhook retries, overlapping poll ticks) a no-op — see
/// `migrations/0014_play_events.sql` for why `received_at` is deliberately
/// NOT part of that key. Returns `Ok(None)` in the dedup case rather than
/// erroring — the caller (webhook handler / poller) treats a duplicate
/// delivery as success, not a failure to insert.
pub async fn insert(pool: &PgPool, new: &NewPlayEvent) -> MuseResult<Option<PlayEvent>> {
    sqlx::query_as::<_, PlayEvent>(
        r#"
        INSERT INTO play_events (
            source, event_type, account_ref, session_key, rating_key,
            view_offset_ms, player, platform, product, device, ip_address, raw
        )
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)
        ON CONFLICT (source, event_type, session_key, view_offset_ms) DO NOTHING
        RETURNING *
        "#,
    )
    .bind(&new.source)
    .bind(&new.event_type)
    .bind(&new.account_ref)
    .bind(&new.session_key)
    .bind(&new.rating_key)
    .bind(new.view_offset_ms)
    .bind(&new.player)
    .bind(&new.platform)
    .bind(&new.product)
    .bind(&new.device)
    .bind(new.ip_address)
    .bind(&new.raw)
    .fetch_optional(pool)
    .await
    .map_err(MuseError::Database)
}

pub async fn list_for_session(pool: &PgPool, session_key: &str) -> MuseResult<Vec<PlayEvent>> {
    sqlx::query_as::<_, PlayEvent>(
        "SELECT * FROM play_events WHERE session_key = $1 ORDER BY received_at",
    )
    .bind(session_key)
    .fetch_all(pool)
    .await
    .map_err(MuseError::Database)
}

pub async fn list_recent(pool: &PgPool, limit: i64) -> MuseResult<Vec<PlayEvent>> {
    sqlx::query_as::<_, PlayEvent>("SELECT * FROM play_events ORDER BY received_at DESC LIMIT $1")
        .bind(limit)
        .fetch_all(pool)
        .await
        .map_err(MuseError::Database)
}

/// Every `play_events` row with a non-null `session_key`, ordered so that a
/// caller can fold consecutive rows into per-session groups with one linear
/// pass (`(session_key, received_at)` — the same ordering
/// [`crate::tracker::reconstruct::fold_events`] re-sorts within a session
/// anyway, but grouping-by-adjacent-rows is cheaper when the whole table is
/// already in this order).
///
/// A read-only, unbounded listing — used by the MUSET-08 shadow runner
/// (`crate::shadow`) to fold an entire snapshot's ingested play data in one
/// pass; NOT used by the live webhook/poller path (which always folds one
/// `session_key` at a time via [`list_for_session`]) or exposed over HTTP.
pub async fn list_all_with_session_key(pool: &PgPool) -> MuseResult<Vec<PlayEvent>> {
    sqlx::query_as::<_, PlayEvent>(
        "SELECT * FROM play_events WHERE session_key IS NOT NULL ORDER BY session_key, received_at",
    )
    .fetch_all(pool)
    .await
    .map_err(MuseError::Database)
}
