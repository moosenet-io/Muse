//! `proactive_items` — the proactive-content outbox to Lumina (spec §3.4).

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value as Json;
use sqlx::FromRow;

#[derive(Debug, Clone, FromRow, Serialize, Deserialize)]
pub struct ProactiveItem {
    pub id: i64,
    pub account_id: Option<i64>,
    pub kind: String,
    pub media_item_id: Option<i64>,
    pub headline: String,
    pub body: Option<Json>,
    pub priority: i32,
    pub earliest_at: Option<DateTime<Utc>>,
    pub expires_at: Option<DateTime<Utc>>,
    pub delivered_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    /// MUSE-12 (`migrations/0036_proactive_items_dedup_cooldown.sql`): the
    /// "same nudge" identity within a `kind`, used by
    /// `repo::proactive_item::find_recent_by_dedup_key` to make the
    /// generator's cooldown check + idempotent re-run possible. `None` for
    /// pre-MUSE-12 rows and for any item created via the original
    /// `create`/`NewProactiveItem` path, which doesn't set it.
    pub dedup_key: Option<String>,
    /// MUSE-12: explicit pending/sent/dismissed tri-state. Defaults to
    /// `'pending'` at the DB level; existing `delivered_at`-based callers
    /// (MUSE-03) are unaffected — this is an additive signal, not a
    /// replacement.
    pub status: String,
    /// MUSE-12: set when `status` transitions to `'dismissed'` via
    /// `POST /proactive/{id}/ack`.
    pub dismissed_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone)]
pub struct NewProactiveItem {
    pub account_id: Option<i64>,
    pub kind: String,
    pub media_item_id: Option<i64>,
    pub headline: String,
    pub body: Option<Json>,
    pub priority: i32,
    pub earliest_at: Option<DateTime<Utc>>,
    pub expires_at: Option<DateTime<Utc>>,
}

/// MUSE-12: the dedup/cooldown-aware insert shape used by
/// `crate::proactive::generators` — kept separate from [`NewProactiveItem`]
/// (MUSE-03's original outbox-insert shape, still used verbatim by
/// `src/integration_tests.rs`) rather than adding a field to it, so that
/// existing call site keeps compiling unchanged.
#[derive(Debug, Clone)]
pub struct NewProactiveItemDeduped {
    pub account_id: Option<i64>,
    pub kind: String,
    pub media_item_id: Option<i64>,
    pub headline: String,
    pub body: Option<Json>,
    pub priority: i32,
    pub earliest_at: Option<DateTime<Utc>>,
    pub expires_at: Option<DateTime<Utc>>,
    pub dedup_key: String,
}
