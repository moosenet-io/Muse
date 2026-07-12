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
