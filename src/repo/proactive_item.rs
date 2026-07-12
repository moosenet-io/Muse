//! Repo functions for `proactive_items` — the outbox to Lumina (spec §3.4).

use chrono::{DateTime, Utc};
use sqlx::PgPool;

use crate::error::{MuseError, MuseResult};
use crate::models::proactive_item::{NewProactiveItem, NewProactiveItemDeduped, ProactiveItem};

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

// --- MUSE-12: proactive content generator support -------------------------

/// Dedup/cooldown-aware insert — `crate::proactive::generators`' write path.
/// Sets `dedup_key`; `status` takes the column default (`'pending'`).
pub async fn create_with_dedup(pool: &PgPool, new: &NewProactiveItemDeduped) -> MuseResult<ProactiveItem> {
    sqlx::query_as::<_, ProactiveItem>(
        r#"
        INSERT INTO proactive_items (
            account_id, kind, media_item_id, headline, body, priority, earliest_at, expires_at, dedup_key
        )
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
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
    .bind(&new.dedup_key)
    .fetch_one(pool)
    .await
    .map_err(MuseError::Database)
}

/// The generator's cooldown/idempotent-re-run check: does an item for this
/// exact `(account_id, kind, dedup_key)` already exist, created at or after
/// `since` (the cooldown window's start)? `account_id` is compared with
/// `IS NOT DISTINCT FROM` so a `None` account (a household-wide nudge, if
/// ever used that way) still dedups against itself rather than every row
/// silently comparing unequal under plain `=`.
pub async fn find_recent_by_dedup_key(
    pool: &PgPool,
    account_id: Option<i64>,
    kind: &str,
    dedup_key: &str,
    since: DateTime<Utc>,
) -> MuseResult<Option<ProactiveItem>> {
    sqlx::query_as::<_, ProactiveItem>(
        r#"
        SELECT * FROM proactive_items
        WHERE account_id IS NOT DISTINCT FROM $1
          AND kind = $2
          AND dedup_key = $3
          AND created_at >= $4
        ORDER BY created_at DESC
        LIMIT 1
        "#,
    )
    .bind(account_id)
    .bind(kind)
    .bind(dedup_key)
    .bind(since)
    .fetch_optional(pool)
    .await
    .map_err(MuseError::Database)
}

/// `POST /proactive/{id}/ack` target status — `'sent'` sets `delivered_at`
/// (reusing the MUSE-03 column so `list_pending_for_account`'s existing
/// `delivered_at IS NULL` filter keeps excluding it correctly), `'dismissed'`
/// sets `dismissed_at` instead and leaves `delivered_at` untouched.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AckOutcome {
    Sent,
    Dismissed,
}

impl AckOutcome {
    pub fn as_status(self) -> &'static str {
        match self {
            AckOutcome::Sent => "sent",
            AckOutcome::Dismissed => "dismissed",
        }
    }
}

pub async fn ack(pool: &PgPool, id: i64, outcome: AckOutcome, now: DateTime<Utc>) -> MuseResult<ProactiveItem> {
    let updated = match outcome {
        AckOutcome::Sent => {
            sqlx::query_as::<_, ProactiveItem>(
                "UPDATE proactive_items SET status = $2, delivered_at = $3 WHERE id = $1 RETURNING *",
            )
            .bind(id)
            .bind(outcome.as_status())
            .bind(now)
            .fetch_optional(pool)
            .await
        }
        AckOutcome::Dismissed => {
            sqlx::query_as::<_, ProactiveItem>(
                "UPDATE proactive_items SET status = $2, dismissed_at = $3 WHERE id = $1 RETURNING *",
            )
            .bind(id)
            .bind(outcome.as_status())
            .bind(now)
            .fetch_optional(pool)
            .await
        }
    }
    .map_err(MuseError::Database)?;

    updated.ok_or_else(|| MuseError::NotFound(format!("proactive_item {id} not found")))
}
