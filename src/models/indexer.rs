//! `indexers` — read-only mirror of Prowlarr's own indexer registry
//! (MUSE-16, blueprint §4b).

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::FromRow;

#[derive(Debug, Clone, FromRow, Serialize, Deserialize)]
pub struct Indexer {
    pub id: i64,
    pub prowlarr_id: i32,
    pub name: String,
    pub protocol: Option<String>,
    pub privacy: Option<String>,
    pub enabled: bool,
    pub categories: Vec<i32>,
    pub last_rss_pull_at: Option<DateTime<Utc>>,
    pub polite_min_interval_secs: i32,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Fields accepted on upsert, keyed by `prowlarr_id` — see
/// `repo::indexer::upsert`.
#[derive(Debug, Clone)]
pub struct NewIndexer {
    pub prowlarr_id: i32,
    pub name: String,
    pub protocol: Option<String>,
    pub privacy: Option<String>,
    pub enabled: bool,
    pub categories: Vec<i32>,
    pub polite_min_interval_secs: i32,
}
