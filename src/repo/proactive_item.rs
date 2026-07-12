//! Repo functions for `proactive_items` — the outbox to Lumina (spec §3.4).

use chrono::{DateTime, Utc};
use sqlx::PgPool;

use crate::error::{MuseError, MuseResult};
use crate::models::proactive_item::{NewProactiveItem, ProactiveItem};

pub async fn create(pool: &PgPool, new: &NewProactiveItem) -> MuseResult<ProactiveItem> {
    sqlx::query_as::<_, ProactiveItem>(
        r#"
        INSERT INTO proactive_items (
            account_id, kind, media_item_id, headline, body, priority, earliest_at, expires_at
        )
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
        RETURNING *
        "#,
    )
    .bind(new.account_id)
    .bind(&new.kind)
    .bind(new.media_item_id)
    .bind(&new.headline)
    .bind(&new.body)
    .bind(new.priority)
    .bind(new.earliest_at)
    .bind(new.expires_at)
    .fetch_one(pool)
    .await
    .map_err(MuseError::Database)
}

/// Undelivered, currently-eligible items for an account — the cooldown /
/// dedup query the proactive-scheduler worker reads from: `earliest_at`
/// already passed (or unset), `expires_at` not yet passed (or unset),
/// highest priority first.
pub async fn list_pending_for_account(pool: &PgPool, account_id: i64, now: DateTime<Utc>) -> MuseResult<Vec<ProactiveItem>> {
    sqlx::query_as::<_, ProactiveItem>(
        r#"
        SELECT * FROM proactive_items
        WHERE account_id = $1
          AND delivered_at IS NULL
          AND (earliest_at IS NULL OR earliest_at <= $2)
          AND (expires_at IS NULL OR expires_at > $2)
        ORDER BY priority DESC, created_at
        "#,
    )
    .bind(account_id)
    .bind(now)
    .fetch_all(pool)
    .await
    .map_err(MuseError::Database)
}

pub async fn mark_delivered(pool: &PgPool, id: i64, delivered_at: DateTime<Utc>) -> MuseResult<()> {
    sqlx::query("UPDATE proactive_items SET delivered_at = $2 WHERE id = $1")
        .bind(id)
        .bind(delivered_at)
        .execute(pool)
        .await
        .map_err(MuseError::Database)?;
    Ok(())
}

pub async fn get(pool: &PgPool, id: i64) -> MuseResult<ProactiveItem> {
    sqlx::query_as::<_, ProactiveItem>("SELECT * FROM proactive_items WHERE id = $1")
        .bind(id)
        .fetch_optional(pool)
        .await
        .map_err(MuseError::Database)?
        .ok_or_else(|| MuseError::NotFound(format!("proactive_item {id} not found")))
}
