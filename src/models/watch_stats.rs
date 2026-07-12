//! `watch_stats` / `ratings` / `watchlist` — per-(account, media_item)
//! derived aggregates and explicit signals (spec §3.3).

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::FromRow;

#[derive(Debug, Clone, FromRow, Serialize, Deserialize)]
pub struct WatchStats {
    pub account_id: i64,
    pub media_item_id: i64,
    pub play_count: i32,
    pub finished_count: i32,
    pub rewatch_count: i32,
    pub total_watched_ms: i64,
    pub avg_percent: Option<f32>,
    pub last_watched_at: Option<DateTime<Utc>>,
    pub abandoned: bool,
    pub first_watched_at: Option<DateTime<Utc>>,
}

/// Full replacement set for an upsert (the recompute worker always writes
/// the complete recomputed aggregate, never a partial delta).
#[derive(Debug, Clone)]
pub struct NewWatchStats {
    pub account_id: i64,
    pub media_item_id: i64,
    pub play_count: i32,
    pub finished_count: i32,
    pub rewatch_count: i32,
    pub total_watched_ms: i64,
    pub avg_percent: Option<f32>,
    pub last_watched_at: Option<DateTime<Utc>>,
    pub abandoned: bool,
    pub first_watched_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, FromRow, Serialize, Deserialize)]
pub struct Rating {
    pub account_id: i64,
    pub media_item_id: i64,
    pub rating: Option<f32>,
    pub rated_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, FromRow, Serialize, Deserialize)]
pub struct WatchlistEntry {
    pub account_id: i64,
    pub media_item_id: i64,
    pub added_at: Option<DateTime<Utc>>,
    pub removed_at: Option<DateTime<Utc>>,
    pub fulfilled: bool,
}
