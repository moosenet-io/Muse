//! `media_files` — physical files with compound quality (blueprint
//! §2/§7.3/§7.4). 1:1 for movies via `media_item_id`; many-to-many for TV
//! via the `episode_files` join table (see `repo::media_file`).

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value as Json;
use sqlx::FromRow;

use crate::media::doc::{StoredMediaInfo, StoredProbeState};

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
    /// The versioned probe document (`src/media/doc.rs`). **Read it through
    /// [`MediaFile::stored_media_info`], never by reaching into the JSON** — a
    /// grep-guard test in `media::doc` fails the build on ad-hoc key access.
    pub media_info: Option<Json>,
    /// MPRB-05: indexable mirror of `media_info -> 'schema_version'`. The
    /// document is authoritative; this column exists only so the backfill queue
    /// predicate can use an index (`jsonb ->> …` cannot, absent a functional
    /// index). `None` on every row written before `0113`.
    pub media_info_version: Option<i32>,
    /// When the probe last ran — set on success AND on failure.
    pub probed_at: Option<DateTime<Utc>>,
    /// `ok` | `suspicious` | `unreadable` | `probe_failed`, or `None` for never
    /// probed. Read it through [`MediaFile::probe_state_parsed`].
    pub probe_state: Option<String>,
    /// What is wrong with this file, for every unhappy state: the `ProbeError`
    /// description for a failure, the suspicion description for a result that
    /// parsed but looks wrong.
    pub probe_error: Option<String>,
    /// Failed probe attempts. Bounds the backfill so a handful of files that will
    /// never parse cannot become an infinite retry loop.
    pub probe_attempts: i32,
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

    /// **The one typed reader of `media_info`.** Every caller goes through here;
    /// nothing else in `src/` may reach into the jsonb, and a grep-guard test in
    /// [`crate::media::doc`] enforces it.
    ///
    /// Total: a `NULL`, a legacy `{"container": "mkv"}`, a document from a newer
    /// binary and a structurally corrupt one all yield a value rather than an
    /// error. A bad row must not break a list endpoint.
    pub fn stored_media_info(&self) -> StoredMediaInfo {
        StoredMediaInfo::from_json(self.media_info.as_ref())
    }

    /// The persisted probe state, parsed.
    ///
    /// `None` means either "never probed" (the column is `NULL`) or "a state this
    /// binary does not know" — which during a rolling deploy is a state a NEWER
    /// binary wrote. The two are deliberately not distinguished here: neither is
    /// a state this binary may act on, and inventing a distinction would invite
    /// acting on the second.
    pub fn probe_state_parsed(&self) -> Option<StoredProbeState> {
        self.probe_state.as_deref().and_then(StoredProbeState::parse)
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
            media_info_version: None,
            probed_at: None,
            probe_state: None,
            probe_error: None,
            probe_attempts: 0,
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
    fn stored_media_info_reads_the_three_shapes_a_row_can_hold() {
        let mut file = sample_file();
        assert_eq!(file.stored_media_info(), StoredMediaInfo::Absent);

        file.media_info = Some(serde_json::json!({ "container": "mkv" }));
        assert!(matches!(
            file.stored_media_info(),
            StoredMediaInfo::Legacy(_)
        ));
        assert!(file.stored_media_info().needs_probe());

        file.media_info = Some(serde_json::json!({ "schema_version": 99 }));
        assert!(matches!(
            file.stored_media_info(),
            StoredMediaInfo::UnknownVersion { version: 99 }
        ));
    }

    #[test]
    fn probe_state_parsed_refuses_a_state_this_binary_does_not_know() {
        let mut file = sample_file();
        assert_eq!(file.probe_state_parsed(), None);
        file.probe_state = Some("suspicious".to_string());
        assert_eq!(
            file.probe_state_parsed(),
            Some(StoredProbeState::Suspicious)
        );
        file.probe_state = Some("quarantined_by_a_newer_binary".to_string());
        assert_eq!(file.probe_state_parsed(), None);
    }

    #[test]
    fn release_type_kind_serde_round_trip() {
        let json = serde_json::to_string(&ReleaseTypeKind::SeasonPack).unwrap();
        assert_eq!(json, "\"season_pack\"");
        let back: ReleaseTypeKind = serde_json::from_str(&json).unwrap();
        assert_eq!(back, ReleaseTypeKind::SeasonPack);
    }
}
