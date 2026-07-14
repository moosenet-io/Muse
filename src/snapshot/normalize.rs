//! MUSET-03 AC2: normalize snapshot-shaped records into muse's real Postgres
//! schema, so muse's own logic (recall/curation/recommend) can run against
//! the loaded snapshot exactly as it would against production-shaped data.
//!
//! Scope note: this module normalizes already-EXTRACTED, plain-data records
//! (e.g. rows an operator's acquisition tooling pulled out of a snapshotted
//! SQLite file with `sqlite3 -json` or equivalent, or a `pg_dump` restore
//! already applied to the isolated DB) into muse's own `models::*`/`repo::*`
//! types. It intentionally does NOT embed a SQLite driver or a Plex/Tautulli/
//! *arr schema parser in this crate -- that would be a second, heavier
//! access path to those source formats living inside the *service* binary,
//! when the whole point of AC1 is that acquisition is an out-of-band,
//! operator-run step. Normalization's job is the seam between "some record
//! extracted from a snapshot" and "a row muse's own schema understands."

use chrono::{DateTime, Utc};
use serde_json::Value as Json;

use crate::error::MuseResult;
use crate::models::library::Library;
use crate::models::library::{LibraryKind, NewLibrary};
use crate::models::media_item::MediaItem;
use crate::models::media_item::NewMediaItem;
use crate::models::media_metadata::MediaMetadata;
use crate::models::media_metadata::{MediaKind, NewMediaMetadata};
use crate::models::play_event::NewPlayEvent;
use crate::repo;

/// A library/section record as extracted from a Plex library-SQLite
/// snapshot's `library_sections` table.
#[derive(Debug, Clone)]
pub struct RawPlexLibrary {
    pub name: String,
    /// Plex's own `section_type` -- 1 = movie, 2 = show (the two kinds this
    /// normalizer maps; anything else is rejected rather than guessed at).
    pub section_type: i32,
    pub root_folder: String,
}

/// Normalize a Plex library-section snapshot record into muse's
/// [`NewLibrary`] shape.
pub fn normalize_plex_library(raw: &RawPlexLibrary) -> MuseResult<NewLibrary> {
    let kind = match raw.section_type {
        1 => LibraryKind::Movie,
        2 => LibraryKind::Tv,
        other => {
            return Err(crate::error::MuseError::BadRequest(format!(
                "unsupported Plex section_type {other} (expected 1=movie or 2=show)"
            )))
        }
    };
    Ok(NewLibrary {
        name: raw.name.clone(),
        kind,
        root_folder: raw.root_folder.clone(),
        source_arr_name: None,
        source_arr_url: None,
    })
}

/// A media item record as extracted from a Plex library-SQLite snapshot's
/// `metadata_items` table (movie or show row).
#[derive(Debug, Clone)]
pub struct RawPlexMediaItem {
    pub title: String,
    pub is_show: bool,
    pub year: Option<i32>,
    pub tmdb_id: Option<String>,
    pub tvdb_id: Option<String>,
    pub file_path: String,
    pub rating_key: String,
}

/// Normalize a Plex metadata-item snapshot record into muse's
/// `(NewMediaMetadata, NewMediaItem)` pair -- the two inserts every media
/// item needs (metadata upserted by external id, then the item itself tied
/// to a library).
pub fn normalize_plex_media_item(
    raw: &RawPlexMediaItem,
    library_id: i64,
) -> (NewMediaMetadata, NewMediaItem) {
    let kind = if raw.is_show {
        MediaKind::Show
    } else {
        MediaKind::Movie
    };
    let metadata = NewMediaMetadata {
        kind,
        tmdb_id: raw.tmdb_id.clone(),
        tvdb_id: raw.tvdb_id.clone(),
        imdb_id: None,
        provider_ids: Json::Object(Default::default()),
        title: raw.title.clone(),
        sort_title: None,
        original_title: None,
        original_language: None,
        status: None,
        overview: None,
        studio: None,
        network: None,
        runtime_minutes: None,
        year: raw.year,
        images: Json::Array(Vec::new()),
    };
    let item = NewMediaItem {
        library_id,
        // media_metadata_id is filled in by the caller once the metadata
        // upsert has returned its real id -- see `load_plex_media_item`.
        media_metadata_id: 0,
        path: raw.file_path.clone(),
        monitored: true,
        quality_profile_id: None,
        minimum_availability: None,
        plex_rating_key: Some(raw.rating_key.clone()),
        added_at: None,
    };
    (metadata, item)
}

/// A playback-history record as extracted from a Tautulli history-SQLite
/// snapshot's `session_history` table (one row per completed/partial play).
#[derive(Debug, Clone)]
pub struct RawTautulliPlayRecord {
    pub rating_key: Option<String>,
    pub user: Option<String>,
    pub view_offset_ms: Option<i64>,
    pub player: Option<String>,
    pub platform: Option<String>,
    /// The raw Tautulli row, preserved verbatim for provenance/debugging --
    /// same posture as `PlayEvent::raw` for live-ingested events.
    pub raw: Json,
}

/// Normalize a Tautulli history-snapshot record into muse's
/// [`NewPlayEvent`] shape, tagging its `source` as `"snapshot:tautulli"` so
/// snapshot-derived events are distinguishable from live-ingested ones in
/// query results (never silently indistinguishable from real live data).
pub fn normalize_tautulli_play_record(raw: &RawTautulliPlayRecord) -> NewPlayEvent {
    NewPlayEvent {
        source: "snapshot:tautulli".to_string(),
        event_type: "snapshot.history".to_string(),
        account_ref: raw.user.clone(),
        session_key: None,
        rating_key: raw.rating_key.clone(),
        view_offset_ms: raw.view_offset_ms,
        player: raw.player.clone(),
        platform: raw.platform.clone(),
        product: None,
        device: None,
        ip_address: None,
        raw: raw.raw.clone(),
    }
}

/// Result of loading one normalized Plex library + its media items into the
/// isolated snapshot database.
#[derive(Debug, Clone)]
pub struct LoadedPlexLibrary {
    pub library: Library,
    pub media_items: Vec<(MediaMetadata, MediaItem)>,
}

/// End-to-end: normalize + INSERT a Plex library snapshot (the library row
/// plus its media items) into the isolated snapshot/test Postgres.
///
/// Callers MUST pass a `pool` already validated via
/// `snapshot::guard::validate_snapshot_dsn` (see `snapshot::load`, the one
/// entry point that performs that validation before handing out a pool) --
/// this function trusts its caller on that, exactly like every other
/// `repo::*` function trusts the pool it's given.
pub async fn load_plex_library_snapshot(
    pool: &sqlx::PgPool,
    library: &RawPlexLibrary,
    items: &[RawPlexMediaItem],
) -> MuseResult<LoadedPlexLibrary> {
    let new_library = normalize_plex_library(library)?;
    let created_library = repo::library::create(pool, &new_library).await?;

    let mut media_items = Vec::with_capacity(items.len());
    for raw_item in items {
        let (new_metadata, mut new_item) = normalize_plex_media_item(raw_item, created_library.id);
        let metadata = match (&new_metadata.tmdb_id, &new_metadata.tvdb_id) {
            (Some(_), _) => repo::media_metadata::upsert_by_tmdb(pool, &new_metadata).await?,
            (None, Some(_)) => repo::media_metadata::upsert_by_tvdb(pool, &new_metadata).await?,
            (None, None) => repo::media_metadata::upsert_by_tmdb(pool, &new_metadata).await?,
        };
        new_item.media_metadata_id = metadata.id;
        let item = repo::media_item::upsert(pool, &new_item).await?;
        media_items.push((metadata, item));
    }

    Ok(LoadedPlexLibrary {
        library: created_library,
        media_items,
    })
}

/// Load a batch of normalized Tautulli history records into the isolated
/// snapshot/test Postgres as `play_events` rows.
pub async fn load_tautulli_history_snapshot(
    pool: &sqlx::PgPool,
    records: &[RawTautulliPlayRecord],
) -> MuseResult<usize> {
    let mut inserted = 0usize;
    for raw in records {
        let new_event = normalize_tautulli_play_record(raw);
        if repo::play_event::insert(pool, &new_event).await?.is_some() {
            inserted += 1;
        }
    }
    Ok(inserted)
}

/// A snapshot-vintage timestamp helper: parse a source-format timestamp
/// (Tautulli/Plex both commonly store unix epoch seconds) into a proper
/// `DateTime<Utc>` for provenance recording. Returns `None` (never panics)
/// on an out-of-range/invalid value -- a bad vintage timestamp degrades to
/// "unknown," it does not fail the whole load.
pub fn parse_epoch_seconds(epoch_secs: i64) -> Option<DateTime<Utc>> {
    DateTime::<Utc>::from_timestamp(epoch_secs, 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_plex_library_maps_movie_section_type() {
        let raw = RawPlexLibrary {
            name: "Movies".to_string(),
            section_type: 1,
            root_folder: "/test/movies".to_string(),
        };
        let new_library = normalize_plex_library(&raw).unwrap();
        assert!(matches!(new_library.kind, LibraryKind::Movie));
        assert_eq!(new_library.name, "Movies");
    }

    #[test]
    fn normalize_plex_library_maps_show_section_type() {
        let raw = RawPlexLibrary {
            name: "TV Shows".to_string(),
            section_type: 2,
            root_folder: "/test/tv".to_string(),
        };
        let new_library = normalize_plex_library(&raw).unwrap();
        assert!(matches!(new_library.kind, LibraryKind::Tv));
    }

    #[test]
    fn normalize_plex_library_rejects_unknown_section_type() {
        let raw = RawPlexLibrary {
            name: "Music".to_string(),
            section_type: 8,
            root_folder: "/test/music".to_string(),
        };
        assert!(normalize_plex_library(&raw).is_err());
    }

    #[test]
    fn normalize_plex_media_item_sets_movie_kind_and_placeholder_metadata_id() {
        let raw = RawPlexMediaItem {
            title: "Test Movie".to_string(),
            is_show: false,
            year: Some(2020),
            tmdb_id: Some("12345".to_string()),
            tvdb_id: None,
            file_path: "/test/movies/test.mkv".to_string(),
            rating_key: "999".to_string(),
        };
        let (metadata, item) = normalize_plex_media_item(&raw, 42);
        assert!(matches!(metadata.kind, MediaKind::Movie));
        assert_eq!(metadata.tmdb_id.as_deref(), Some("12345"));
        assert_eq!(item.library_id, 42);
        assert_eq!(item.plex_rating_key.as_deref(), Some("999"));
        // media_metadata_id is a placeholder here -- the loader fills in the
        // real id after the metadata upsert returns.
        assert_eq!(item.media_metadata_id, 0);
    }

    #[test]
    fn normalize_tautulli_play_record_tags_source_as_snapshot() {
        let raw = RawTautulliPlayRecord {
            rating_key: Some("999".to_string()),
            user: Some("test-user".to_string()),
            view_offset_ms: Some(120_000),
            player: Some("Chrome".to_string()),
            platform: Some("Web".to_string()),
            raw: serde_json::json!({"row": "fixture"}),
        };
        let event = normalize_tautulli_play_record(&raw);
        assert_eq!(event.source, "snapshot:tautulli");
        assert_eq!(event.event_type, "snapshot.history");
        assert_eq!(event.rating_key.as_deref(), Some("999"));
        assert_eq!(event.view_offset_ms, Some(120_000));
    }

    #[test]
    fn parse_epoch_seconds_handles_a_normal_value() {
        // 2021-01-01T00:00:00Z
        let dt = parse_epoch_seconds(1_609_459_200).unwrap();
        assert_eq!(dt.to_rfc3339(), "2021-01-01T00:00:00+00:00");
    }

    #[test]
    fn parse_epoch_seconds_returns_none_for_out_of_range() {
        assert!(parse_epoch_seconds(i64::MAX).is_none());
    }
}
