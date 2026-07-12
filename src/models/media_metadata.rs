//! `media_metadata` — shared, provider-keyed descriptive metadata
//! (blueprint §2/§7.1: split out of the per-library instance row).

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value as Json;
use sqlx::FromRow;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type)]
#[sqlx(type_name = "media_kind", rename_all = "snake_case")]
#[serde(rename_all = "snake_case")]
pub enum MediaKind {
    Movie,
    Show,
}

#[derive(Debug, Clone, FromRow, Serialize, Deserialize)]
pub struct MediaMetadata {
    pub id: i64,
    pub kind: MediaKind,
    pub tmdb_id: Option<String>,
    pub tvdb_id: Option<String>,
    pub imdb_id: Option<String>,
    pub provider_ids: Json,
    pub title: String,
    pub sort_title: Option<String>,
    pub clean_title: Option<String>,
    pub original_title: Option<String>,
    pub clean_original_title: Option<String>,
    pub original_language: Option<String>,
    pub status: Option<String>,
    pub overview: Option<String>,
    pub tagline: Option<String>,
    pub studio: Option<String>,
    pub network: Option<String>,
    pub website: Option<String>,
    pub youtube_trailer_id: Option<String>,
    pub certification: Option<String>,
    pub runtime_minutes: Option<i32>,
    pub year: Option<i32>,
    pub secondary_year: Option<i32>,
    pub in_cinemas: Option<DateTime<Utc>>,
    pub physical_release: Option<DateTime<Utc>>,
    pub digital_release: Option<DateTime<Utc>>,
    pub first_aired: Option<DateTime<Utc>>,
    pub last_aired: Option<DateTime<Utc>>,
    pub next_airing: Option<DateTime<Utc>>,
    pub images: Json,
    pub keywords: Json,
    pub ratings: Json,
    pub recommendations: Json,
    pub popularity: Option<f32>,
    pub collection_tmdb_id: Option<String>,
    pub collection_title: Option<String>,
    pub last_info_sync: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Fields accepted on upsert (keyed by `(kind, tmdb_id)` or `(kind, tvdb_id)`
/// depending on media type — see `repo::media_metadata`).
#[derive(Debug, Clone)]
pub struct NewMediaMetadata {
    pub kind: MediaKind,
    pub tmdb_id: Option<String>,
    pub tvdb_id: Option<String>,
    pub imdb_id: Option<String>,
    pub provider_ids: Json,
    pub title: String,
    pub sort_title: Option<String>,
    pub original_title: Option<String>,
    pub original_language: Option<String>,
    pub status: Option<String>,
    pub overview: Option<String>,
    pub studio: Option<String>,
    pub network: Option<String>,
    pub runtime_minutes: Option<i32>,
    pub year: Option<i32>,
    pub images: Json,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn media_kind_serde_round_trip() {
        for kind in [MediaKind::Movie, MediaKind::Show] {
            let json = serde_json::to_string(&kind).unwrap();
            let back: MediaKind = serde_json::from_str(&json).unwrap();
            assert_eq!(kind, back);
        }
        assert_eq!(
            serde_json::to_string(&MediaKind::Movie).unwrap(),
            "\"movie\""
        );
    }
}
