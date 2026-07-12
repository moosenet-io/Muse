//! `media_files` — physical files with compound quality (blueprint
//! §2/§7.3/§7.4). 1:1 for movies via `media_item_id`; many-to-many for TV
//! via the `episode_files` join table (see `repo::media_file`).

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value as Json;
use sqlx::FromRow;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type)]
#[sqlx(type_name = "release_type_kind", rename_all = "snake_case")]
#[serde(rename_all = "snake_case")]
pub enum ReleaseTypeKind {
    Single,
    Multi,
    SeasonPack,
}

/// The compound quality value `{quality_tier_id, revision:{version, real,
/// is_repack}}` (blueprint §2/§7.4) — flattened onto `media_files` columns
/// in the DB, reassembled into this struct at the model layer so callers
/// don't reason about the individual `revision_*` columns.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct Revision {
    pub version: i32,
    pub real: i32,
    pub is_repack: bool,
}

#[derive(Debug, Clone, FromRow, Serialize, Deserialize)]
pub struct MediaFile {
    pub id: i64,
    pub media_item_id: i64,
    pub relative_path: String,
    pub original_file_path: Option<String>,
    pub size_bytes: Option<i64>,
    pub date_added: Option<DateTime<Utc>>,
    pub scene_name: Option<String>,
    pub media_info: Option<Json>,
    pub release_group: Option<String>,
    pub edition: Option<String>,
    pub languages: Vec<String>,
    pub subtitles: Vec<String>,
    pub indexer_flags: i32,
    pub release_type: ReleaseTypeKind,
    pub quality_tier_id: Option<i64>,
    pub revision_version: i32,
    pub revision_real: i32,
    pub revision_is_repack: bool,
    pub created_at: DateTime<Utc>,
}

impl MediaFile {
    pub fn revision(&self) -> Revision {
        Revision {
            version: self.revision_version,
            real: self.revision_real,
            is_repack: self.revision_is_repack,
        }
    }
}

#[derive(Debug, Clone)]
pub struct NewMediaFile {
    pub media_item_id: i64,
    pub relative_path: String,
    pub size_bytes: Option<i64>,
    pub release_group: Option<String>,
    pub languages: Vec<String>,
    pub release_type: ReleaseTypeKind,
    pub quality_tier_id: Option<i64>,
    pub revision: Revision,
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    fn sample_file() -> MediaFile {
        MediaFile {
            id: 1,
            media_item_id: 42,
            relative_path: "Show/Season 01/S01E01.mkv".to_string(),
            original_file_path: None,
            size_bytes: Some(4_000_000_000),
            date_added: Some(Utc::now()),
            scene_name: None,
            media_info: None,
            release_group: Some("d3g".to_string()),
            edition: None,
            languages: vec!["eng".to_string()],
            subtitles: vec![],
            indexer_flags: 0,
            release_type: ReleaseTypeKind::SeasonPack,
            quality_tier_id: Some(7),
            revision_version: 2,
            revision_real: 1,
            revision_is_repack: true,
            created_at: Utc::now(),
        }
    }

    #[test]
    fn revision_reassembles_flattened_columns() {
        let file = sample_file();
        let rev = file.revision();
        assert_eq!(rev.version, 2);
        assert_eq!(rev.real, 1);
        assert!(rev.is_repack);
    }

    #[test]
    fn release_type_kind_serde_round_trip() {
        let json = serde_json::to_string(&ReleaseTypeKind::SeasonPack).unwrap();
        assert_eq!(json, "\"season_pack\"");
        let back: ReleaseTypeKind = serde_json::from_str(&json).unwrap();
        assert_eq!(back, ReleaseTypeKind::SeasonPack);
    }
}
