//! Typed (partial) models for Radarr/Sonarr `*arr` API v3 JSON responses.
//!
//! Grounded in `<path>/spec-staging/muse/ARR-BLUEPRINT.md` §2/§3/§5
//! (live-verified shapes), not generic *arr docs. As with `plex::models`,
//! every field is best-effort (`Option`/`#[serde(default)]`) because the
//! flattened API view mixes several underlying tables and not every field
//! matters to Muse Phase 0 ingest.

use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::Value as Json;

/// A `{id, name}` language entry — identical shape on `MovieFile.languages`
/// and `EpisodeFile.languages` (blueprint §2/§3).
#[derive(Debug, Clone, Deserialize)]
pub struct ArrLanguage {
    #[serde(default)]
    pub id: i64,
    #[serde(default)]
    pub name: String,
}

/// The `quality.quality` sub-object: a source×resolution grid entry
/// (blueprint §2 `QualityDefinitions`). `id` is historical/non-contiguous —
/// never assume ordering.
#[derive(Debug, Clone, Deserialize)]
pub struct ArrQualityDefinition {
    pub id: i64,
    pub name: String,
    #[serde(default)]
    pub source: Option<String>,
    /// Resolution in pixels (e.g. `1080`), absent for sourceless tiers like
    /// CAM/TELESYNC.
    #[serde(default)]
    pub resolution: Option<i64>,
}

/// The `quality.revision` sub-object — PROPER/REPACK/REAL tracking used
/// purely for upgrade-eligibility (blueprint §2/§6).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ArrRevision {
    #[serde(default = "default_version")]
    pub version: i32,
    #[serde(default)]
    pub real: i32,
    #[serde(rename = "isRepack", default)]
    pub is_repack: bool,
}

fn default_version() -> i32 {
    1
}

/// The compound `{quality:{...}, revision:{...}}` value on `MovieFile`/
/// `EpisodeFile.quality` (blueprint §2/§7.4).
#[derive(Debug, Clone, Deserialize)]
pub struct ArrQuality {
    pub quality: ArrQualityDefinition,
    #[serde(default)]
    pub revision: ArrRevision,
}

/// `originalLanguage` on a Radarr movie / Sonarr series — `{id, name}`.
#[derive(Debug, Clone, Deserialize)]
pub struct ArrOriginalLanguage {
    #[serde(default)]
    pub id: i64,
    #[serde(default)]
    pub name: String,
}

// --- Radarr -----------------------------------------------------------

/// `movieFile` nested on a Radarr movie (blueprint §2 `MovieFiles`), or a
/// standalone `/api/v3/moviefile` entry.
#[derive(Debug, Clone, Deserialize)]
pub struct RadarrMovieFile {
    #[serde(default)]
    pub id: i64,
    #[serde(rename = "relativePath")]
    pub relative_path: String,
    #[serde(default)]
    pub size: Option<i64>,
    #[serde(rename = "releaseGroup", default)]
    pub release_group: Option<String>,
    #[serde(default)]
    pub languages: Vec<ArrLanguage>,
    #[serde(default)]
    pub quality: Option<ArrQuality>,
}

/// A Radarr movie from `GET /api/v3/movie` — the join-flattened view over
/// `MovieMetadata` + `Movies` + `MovieFiles` (blueprint §5).
#[derive(Debug, Clone, Deserialize)]
pub struct RadarrMovie {
    pub id: i64,
    pub title: String,
    #[serde(rename = "originalTitle", default)]
    pub original_title: Option<String>,
    #[serde(rename = "sortTitle", default)]
    pub sort_title: Option<String>,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub overview: Option<String>,
    #[serde(default)]
    pub year: Option<i32>,
    #[serde(default)]
    pub path: String,
    #[serde(rename = "hasFile", default)]
    pub has_file: bool,
    #[serde(default)]
    pub monitored: bool,
    #[serde(rename = "minimumAvailability", default)]
    pub minimum_availability: Option<String>,
    #[serde(rename = "tmdbId")]
    pub tmdb_id: i64,
    #[serde(rename = "imdbId", default)]
    pub imdb_id: Option<String>,
    #[serde(default)]
    pub runtime: Option<i32>,
    #[serde(default)]
    pub studio: Option<String>,
    #[serde(rename = "originalLanguage", default)]
    pub original_language: Option<ArrOriginalLanguage>,
    #[serde(default)]
    pub images: Json,
    #[serde(default)]
    pub added: Option<DateTime<Utc>>,
    #[serde(rename = "movieFile", default)]
    pub movie_file: Option<RadarrMovieFile>,
}

// --- Sonarr -------------------------------------------------------------

/// A season entry embedded in `Series.seasons` (blueprint §3: Sonarr has no
/// standalone `Seasons` table — Muse normalizes it into a first-class row
/// via `repo::season::upsert`).
#[derive(Debug, Clone, Deserialize)]
pub struct SonarrSeason {
    #[serde(rename = "seasonNumber")]
    pub season_number: i32,
    #[serde(default)]
    pub monitored: bool,
}

/// A Sonarr series from `GET /api/v3/series` (blueprint §3/§5).
#[derive(Debug, Clone, Deserialize)]
pub struct SonarrSeries {
    pub id: i64,
    pub title: String,
    #[serde(rename = "sortTitle", default)]
    pub sort_title: Option<String>,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub overview: Option<String>,
    #[serde(default)]
    pub network: Option<String>,
    #[serde(default)]
    pub path: String,
    #[serde(default)]
    pub monitored: bool,
    #[serde(rename = "tvdbId")]
    pub tvdb_id: i64,
    #[serde(rename = "tvRageId", default)]
    pub tv_rage_id: Option<i64>,
    #[serde(rename = "tvMazeId", default)]
    pub tv_maze_id: Option<i64>,
    /// Radarr-style TMDb id; Sonarr defaults this to `0` when unknown
    /// (blueprint §3) rather than omitting it — normalized to `None` by
    /// [`SonarrSeries::tmdb_id_opt`].
    #[serde(rename = "tmdbId", default)]
    pub tmdb_id: i64,
    #[serde(rename = "imdbId", default)]
    pub imdb_id: Option<String>,
    #[serde(rename = "malIds", default)]
    pub mal_ids: Vec<i64>,
    #[serde(rename = "aniListIds", default)]
    pub anilist_ids: Vec<i64>,
    #[serde(default)]
    pub year: Option<i32>,
    #[serde(default)]
    pub runtime: Option<i32>,
    #[serde(rename = "originalLanguage", default)]
    pub original_language: Option<ArrOriginalLanguage>,
    #[serde(default)]
    pub images: Json,
    #[serde(default)]
    pub seasons: Vec<SonarrSeason>,
    #[serde(default)]
    pub added: Option<DateTime<Utc>>,
}

impl SonarrSeries {
    /// `tmdbId` normalized to `None` when Sonarr reports the `0` sentinel
    /// (blueprint §3: `TmdbId INTEGER NOT NULL DEFAULT 0`).
    pub fn tmdb_id_opt(&self) -> Option<String> {
        if self.tmdb_id == 0 {
            None
        } else {
            Some(self.tmdb_id.to_string())
        }
    }
}

/// A Sonarr episode from `GET /api/v3/episode?seriesId=`.
#[derive(Debug, Clone, Deserialize)]
pub struct SonarrEpisode {
    pub id: i64,
    #[serde(rename = "seasonNumber")]
    pub season_number: i32,
    #[serde(rename = "episodeNumber")]
    pub episode_number: i32,
    #[serde(rename = "absoluteEpisodeNumber", default)]
    pub absolute_episode_number: Option<i32>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub overview: Option<String>,
    #[serde(rename = "airDate", default)]
    pub air_date: Option<String>,
    #[serde(rename = "airDateUtc", default)]
    pub air_date_utc: Option<DateTime<Utc>>,
    #[serde(default)]
    pub runtime: Option<i32>,
    #[serde(default)]
    pub monitored: bool,
    #[serde(rename = "hasFile", default)]
    pub has_file: bool,
    /// `0` (Sonarr's "no file" sentinel) is normalized to `None` by
    /// [`SonarrEpisode::episode_file_id_opt`].
    #[serde(rename = "episodeFileId", default)]
    pub episode_file_id: i64,
    #[serde(rename = "tvdbId", default)]
    pub tvdb_id: Option<i64>,
}

impl SonarrEpisode {
    pub fn episode_file_id_opt(&self) -> Option<i64> {
        if self.episode_file_id == 0 {
            None
        } else {
            Some(self.episode_file_id)
        }
    }
}

/// A Sonarr episode file from `GET /api/v3/episodefile?seriesId=` (blueprint
/// §3: `ReleaseType` distinguishes single/multi/season-pack — the
/// many-to-many-with-episodes case movies never need).
#[derive(Debug, Clone, Deserialize)]
pub struct SonarrEpisodeFile {
    pub id: i64,
    #[serde(rename = "seasonNumber", default)]
    pub season_number: i32,
    #[serde(rename = "relativePath")]
    pub relative_path: String,
    #[serde(default)]
    pub size: Option<i64>,
    #[serde(rename = "releaseGroup", default)]
    pub release_group: Option<String>,
    #[serde(default)]
    pub languages: Vec<ArrLanguage>,
    #[serde(default)]
    pub quality: Option<ArrQuality>,
    /// `"singleEpisode"` | `"multiEpisode"` | `"seasonPack"` (blueprint §3).
    #[serde(rename = "releaseType", default)]
    pub release_type: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sonarr_series_normalizes_zero_tmdb_id_to_none() {
        let series = SonarrSeries {
            id: 1,
            title: "Test".to_string(),
            sort_title: None,
            status: None,
            overview: None,
            network: None,
            path: String::new(),
            monitored: true,
            tvdb_id: 12345,
            tv_rage_id: None,
            tv_maze_id: None,
            tmdb_id: 0,
            imdb_id: None,
            mal_ids: vec![],
            anilist_ids: vec![],
            year: None,
            runtime: None,
            original_language: None,
            images: Json::Null,
            seasons: vec![],
            added: None,
        };
        assert_eq!(series.tmdb_id_opt(), None);
    }

    #[test]
    fn sonarr_episode_normalizes_zero_file_id_to_none() {
        let episode = SonarrEpisode {
            id: 1,
            season_number: 1,
            episode_number: 1,
            absolute_episode_number: None,
            title: None,
            overview: None,
            air_date: None,
            air_date_utc: None,
            runtime: None,
            monitored: true,
            has_file: false,
            episode_file_id: 0,
            tvdb_id: None,
        };
        assert_eq!(episode.episode_file_id_opt(), None);
    }
}
