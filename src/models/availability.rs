//! `availability` — per-title "grabbable now" rollup (MUSE-16, blueprint
//! §4b), recomputed from `releases` by `repo::availability::recompute`.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::FromRow;

#[derive(Debug, Clone, FromRow, Serialize, Deserialize)]
pub struct Availability {
    pub media_metadata_id: i64,
    pub best_quality: Option<String>,
    pub best_seeders: Option<i32>,
    pub release_count: i32,
    pub has_freeleech: bool,
    pub cheapest_size_bytes: Option<i64>,
    pub newest_release_at: Option<DateTime<Utc>>,
    pub computed_at: DateTime<Utc>,
}
