//! `releases` — rolling grabbability snapshot fed by the Prowlarr
//! report-pull worker (MUSE-16, blueprint §4b).

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::FromRow;

#[derive(Debug, Clone, FromRow, Serialize, Deserialize)]
pub struct Release {
    pub id: i64,
    pub media_metadata_id: Option<i64>,
    pub episode_id: Option<i64>,
    pub indexer_id: i64,
    pub guid: String,
    pub title: String,
    pub info_url: Option<String>,
    pub download_url: Option<String>,
    pub info_hash: Option<String>,
    pub size_bytes: Option<i64>,
    pub publish_date: Option<DateTime<Utc>>,
    pub seeders: Option<i32>,
    pub leechers: Option<i32>,
    pub grabs: Option<i32>,
    pub freeleech: bool,
    pub freeleech_pct: Option<f32>,
    pub categories: Vec<i32>,
    pub parsed_title: Option<String>,
    pub parsed_year: Option<i32>,
    pub quality: Option<String>,
    pub resolution: Option<String>,
    pub source: Option<String>,
    pub video_codec: Option<String>,
    pub audio_codec: Option<String>,
    pub audio_channels: Option<f32>,
    pub hdr: Vec<String>,
    pub edition: Option<String>,
    pub release_group: Option<String>,
    pub proper_repack: bool,
    pub languages: Vec<String>,
    pub subtitles: Vec<String>,
    pub parse_confidence: Option<f32>,
    pub first_seen_at: DateTime<Utc>,
    pub last_seen_at: DateTime<Utc>,
    pub expires_at: Option<DateTime<Utc>>,
}

/// Fields accepted on upsert, keyed by `(indexer_id, guid)` — see
/// `repo::release::upsert`. Carries both the raw Prowlarr report fields and
/// the deterministic-parser output (`prowlarr::parse::parse_release_name`)
/// so a single call can populate the whole row.
#[derive(Debug, Clone, Default)]
pub struct NewRelease {
    pub media_metadata_id: Option<i64>,
    pub episode_id: Option<i64>,
    pub indexer_id: i64,
    pub guid: String,
    pub title: String,
    pub info_url: Option<String>,
    pub download_url: Option<String>,
    pub info_hash: Option<String>,
    pub size_bytes: Option<i64>,
    pub publish_date: Option<DateTime<Utc>>,
    pub seeders: Option<i32>,
    pub leechers: Option<i32>,
    pub grabs: Option<i32>,
    pub freeleech: bool,
    pub freeleech_pct: Option<f32>,
    pub categories: Vec<i32>,
    pub parsed_title: Option<String>,
    pub parsed_year: Option<i32>,
    pub quality: Option<String>,
    pub resolution: Option<String>,
    pub source: Option<String>,
    pub video_codec: Option<String>,
    pub audio_codec: Option<String>,
    pub audio_channels: Option<f32>,
    pub hdr: Vec<String>,
    pub edition: Option<String>,
    pub release_group: Option<String>,
    pub proper_repack: bool,
    pub languages: Vec<String>,
    pub subtitles: Vec<String>,
    pub parse_confidence: Option<f32>,
    pub expires_at: Option<DateTime<Utc>>,
}
