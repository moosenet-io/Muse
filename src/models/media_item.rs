//! `media_items` — thin per-library instance state, referencing shared
//! `media_metadata` (blueprint §2/§7.1/§7.9).

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::FromRow;

#[derive(Debug, Clone, FromRow, Serialize, Deserialize)]
pub struct MediaItem {
    pub id: i64,
    pub library_id: i64,
    pub media_metadata_id: i64,
    pub path: String,
    pub monitored: bool,
    pub in_library: bool,
    pub quality_profile_id: Option<i64>,
    pub minimum_availability: Option<String>,
    pub plex_rating_key: Option<String>,
    pub added_at: Option<DateTime<Utc>>,
    pub last_search_time: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct NewMediaItem {
    pub library_id: i64,
    pub media_metadata_id: i64,
    pub path: String,
    pub monitored: bool,
    pub quality_profile_id: Option<i64>,
    pub minimum_availability: Option<String>,
    pub plex_rating_key: Option<String>,
    pub added_at: Option<DateTime<Utc>>,
}
