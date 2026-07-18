//! Typed (partial) models for Prowlarr v1 API JSON responses.
//!
//! UNVERIFIED against a live Prowlarr instance in this change (MUSE-16): the
//! shapes below are modeled from Prowlarr's public API documentation/OpenAPI
//! spec, not exercised against a real server or a fixture captured from one.
//! Fields are intentionally permissive (`Option`/`#[serde(default)]`) for the
//! same reason `plex::models` is: an indexer/release response shape can vary
//! by Prowlarr version and by which optional fields a given indexer's caps
//! populate. The orchestrator should confirm these against a live Prowlarr
//! response before treating any field as load-bearing beyond what's tested
//! here.

use chrono::{DateTime, Utc};
use serde::Deserialize;

/// `GET /api/v1/indexer` entry.
#[derive(Debug, Clone, Deserialize)]
pub struct ProwlarrIndexer {
    pub id: i32,
    pub name: String,
    #[serde(default)]
    pub protocol: Option<String>,
    #[serde(default)]
    pub privacy: Option<String>,
    /// Prowlarr's own field name is `enable`, not `enabled`.
    #[serde(rename = "enable", default = "default_true")]
    pub enable: bool,
    #[serde(default)]
    pub capabilities: Option<ProwlarrCapabilities>,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ProwlarrCapabilities {
    #[serde(rename = "categories", default)]
    pub categories: Vec<ProwlarrCategory>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ProwlarrCategory {
    pub id: i32,
    #[serde(default)]
    pub name: Option<String>,
}

impl ProwlarrIndexer {
    /// Flattened Newznab category ids this indexer's capabilities advertise
    /// (parent categories only — sub-categories are not expanded here).
    pub fn category_ids(&self) -> Vec<i32> {
        self.capabilities
            .as_ref()
            .map(|c| c.categories.iter().map(|cat| cat.id).collect())
            .unwrap_or_default()
    }
}

/// `GET /api/v1/search` entry — a single release report (RSS-mode with no
/// `query`, or a targeted search result).
#[derive(Debug, Clone, Deserialize)]
pub struct ProwlarrRelease {
    pub guid: String,
    pub title: String,
    #[serde(rename = "indexerId")]
    pub indexer_id: i32,
    #[serde(default)]
    pub indexer: Option<String>,
    #[serde(default)]
    pub protocol: Option<String>,
    #[serde(default)]
    pub size: Option<i64>,
    #[serde(rename = "publishDate", default)]
    pub publish_date: Option<DateTime<Utc>>,
    #[serde(rename = "infoUrl", default)]
    pub info_url: Option<String>,
    #[serde(rename = "downloadUrl", default)]
    pub download_url: Option<String>,
    #[serde(rename = "infoHash", default)]
    pub info_hash: Option<String>,
    #[serde(default)]
    pub seeders: Option<i32>,
    #[serde(default)]
    pub leechers: Option<i32>,
    #[serde(default)]
    pub grabs: Option<i32>,
    #[serde(default)]
    pub categories: Vec<ProwlarrCategory>,
    /// Tracker-reported flags such as `"freeleech"` / `"halfleech"` — used to
    /// derive `releases.freeleech`. Field name and casing UNVERIFIED live.
    #[serde(rename = "indexerFlags", default)]
    pub indexer_flags: Vec<String>,
    /// Prowlarr's own best-effort extraction of the release's IMDb id, from
    /// the title/site metadata at search time (blueprint §4: present on the
    /// release object itself, `0` meaning "unknown" rather than a real id --
    /// see [`ProwlarrRelease::imdb_id`] for the normalized accessor). Kept
    /// alongside, never instead of, Muse's own `parse::parse_release_name`
    /// pass -- the blueprint explicitly warns against relying solely on
    /// Prowlarr's guess.
    #[serde(rename = "imdbId", default)]
    pub imdb_id_raw: Option<i64>,
    #[serde(rename = "tmdbId", default)]
    pub tmdb_id_raw: Option<i64>,
    #[serde(rename = "tvdbId", default)]
    pub tvdb_id_raw: Option<i64>,
}

impl ProwlarrRelease {
    /// Best-effort freeleech detection from tracker-reported flags (some
    /// indexers also encode this in the title itself, which the
    /// release-name parser handles separately as a fallback).
    pub fn is_freeleech(&self) -> bool {
        self.indexer_flags
            .iter()
            .any(|f| f.eq_ignore_ascii_case("freeleech"))
    }

    pub fn category_ids(&self) -> Vec<i32> {
        self.categories.iter().map(|c| c.id).collect()
    }

    /// Normalized IMDb id: `None` when absent *or* `0` (Prowlarr's
    /// "unknown" sentinel, blueprint §4), `Some(id)` otherwise.
    pub fn imdb_id(&self) -> Option<i64> {
        self.imdb_id_raw.filter(|&id| id != 0)
    }

    /// Normalized TMDb id, same `0` = unknown convention as [`Self::imdb_id`].
    pub fn tmdb_id(&self) -> Option<i64> {
        self.tmdb_id_raw.filter(|&id| id != 0)
    }

    /// Normalized TVDB id, same `0` = unknown convention as [`Self::imdb_id`].
    pub fn tvdb_id(&self) -> Option<i64> {
        self.tvdb_id_raw.filter(|&id| id != 0)
    }
}
