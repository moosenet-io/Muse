//! MUSE-05 ingest routine: Radarr/Sonarr → MUSE-02 core schema.
//!
//! [`run`] iterates every configured *arr instance and maps its API
//! responses onto `libraries`/`media_metadata`/`media_items`/`seasons`/
//! `episodes`/`media_files` via the existing `repo::*` layer. It **never
//! aborts the whole ingest for one bad instance** (the operator's
//! `radarr_animated` is currently offline, per the S96 spec + blueprint) —
//! a connection failure, timeout, or non-2xx response for one instance is
//! logged and that instance is skipped; every other instance still runs.
//! Per-item failures (one malformed movie/series) are similarly isolated so
//! they don't drop the rest of that instance's library.
//!
//! Provider-id precedence follows the blueprint (§7.7): Radarr movies key on
//! `(kind, tmdb_id)` via `repo::media_metadata::upsert_by_tmdb`; Sonarr shows
//! key on `(kind, tvdb_id)` via `upsert_by_tvdb`. The long tail of provider
//! ids (`tvRageId`/`tvMazeId`/`malIds`/`aniListIds`) is carried in
//! `provider_ids` jsonb rather than dedicated columns.
//!
//! `media_files` has no natural upsert key in the MUSE-02 schema (no unique
//! constraint beyond the `(id, media_item_id)` superkey used for the
//! episode-file composite FK) — [`upsert_media_file`] adds ingest-level
//! idempotency (dedup by `(media_item_id, relative_path)`) so re-running
//! ingest doesn't create duplicate file rows on every pass.

use std::collections::HashMap;

use serde_json::Value as Json;
use sqlx::PgPool;

use crate::error::MuseResult;
use crate::models::episode::NewEpisode;
use crate::models::library::{Library, NewLibrary};
use crate::models::media_file::{MediaFile, NewMediaFile, ReleaseTypeKind, Revision};
use crate::models::media_item::NewMediaItem;
use crate::models::media_metadata::{MediaKind, NewMediaMetadata};
use crate::models::quality::NewQualityDefinition;
use crate::models::season::NewSeason;
use crate::repo;

use super::client::ArrClient;
use super::config::{ArrInstanceConfig, ArrKind};
use super::models::ArrQuality;

/// Outcome of one `run()` call across the whole configured fleet.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IngestSummary {
    pub instances_ok: Vec<String>,
    /// `(instance_name, error_message)` — never aborts the run.
    pub instances_skipped: Vec<(String, String)>,
    pub movies_upserted: usize,
    pub series_upserted: usize,
    pub episodes_upserted: usize,
    pub files_upserted: usize,
}

/// Ingest every configured *arr instance. Never returns `Err` — a failing
/// instance is recorded in [`IngestSummary::instances_skipped`] and ingest
/// continues with the rest of the fleet.
pub async fn run(pool: &PgPool, instances: &[ArrInstanceConfig]) -> IngestSummary {
    let mut summary = IngestSummary::default();

    for instance in instances {
        let result = match instance.kind {
            ArrKind::Radarr => ingest_radarr_instance(pool, instance, &mut summary).await,
            ArrKind::Sonarr => ingest_sonarr_instance(pool, instance, &mut summary).await,
        };

        match result {
            Ok(()) => summary.instances_ok.push(instance.name.clone()),
            Err(e) => {
                tracing::warn!(
                    instance = %instance.name,
                    error = %e,
                    "skipping arr instance (unreachable or erroring); ingest continues with the rest of the fleet"
                );
                summary
                    .instances_skipped
                    .push((instance.name.clone(), e.to_string()));
            }
        }
    }

    summary
}

async fn ensure_library(pool: &PgPool, instance: &ArrInstanceConfig) -> MuseResult<Library> {
    if let Some(existing) = repo::library::get_by_name(pool, &instance.name).await? {
        return Ok(existing);
    }
    repo::library::create(
        pool,
        &NewLibrary {
            name: instance.name.clone(),
            kind: instance.library_kind,
            root_folder: instance.root_folder.clone().unwrap_or_default(),
            source_arr_name: Some(instance.name.clone()),
            source_arr_url: Some(instance.base_url.clone()),
        },
    )
    .await
}

/// Upsert a `quality_definitions` row for an *arr quality tier the first
/// time we see it, keyed `"{radarr|sonarr}:{arr_quality_id}"` (blueprint §2:
/// *arr's own ids are historical/non-contiguous, so they're namespaced
/// per-app rather than trusted as globally unique). Returns the local
/// `quality_definitions.id` to use as `media_files.quality_tier_id`.
async fn resolve_quality_tier(
    pool: &PgPool,
    kind: ArrKind,
    quality: &ArrQuality,
) -> MuseResult<Option<i64>> {
    let quality_key = format!("{}:{}", kind.quality_key_prefix(), quality.quality.id);
    let resolution = quality.quality.resolution.map(|r| format!("{r}p"));
    let source = quality
        .quality
        .source
        .clone()
        .unwrap_or_else(|| quality.quality.name.clone());

    let def = repo::quality::create_definition(
        pool,
        &NewQualityDefinition {
            quality_key,
            title: quality.quality.name.clone(),
            source,
            resolution,
            modifier: "none".to_string(),
            sort_order: quality.quality.id as i32,
        },
    )
    .await?;

    Ok(Some(def.id))
}

fn revision_from(quality: Option<&ArrQuality>) -> Revision {
    match quality {
        Some(q) => Revision {
            version: q.revision.version,
            real: q.revision.real,
            is_repack: q.revision.is_repack,
        },
        None => Revision {
            version: 1,
            real: 0,
            is_repack: false,
        },
    }
}

/// `media_files` has no upsert key in MUSE-02 — dedup by
/// `(media_item_id, relative_path)` at the ingest layer so repeated ingest
/// runs don't create duplicate rows for the same on-disk file.
async fn upsert_media_file(pool: &PgPool, new: &NewMediaFile) -> MuseResult<MediaFile> {
    let existing = repo::media_file::list_by_media_item(pool, new.media_item_id).await?;
    if let Some(found) = existing
        .into_iter()
        .find(|f| f.relative_path == new.relative_path)
    {
        return Ok(found);
    }
    repo::media_file::create(pool, new).await
}

/// *arr's flattened API views sometimes omit array/object fields entirely
/// rather than sending `[]`/`{}`; the MUSE-02 schema's `jsonb NOT NULL
/// DEFAULT '[]'` columns reject an explicit `null` insert, so normalize.
fn non_null_json(value: Json) -> Json {
    if value.is_null() {
        Json::Array(vec![])
    } else {
        value
    }
}

/// singleEpisode/multiEpisode/seasonPack (blueprint §3) → `ReleaseTypeKind`.
/// Unknown/missing values default to `Single` — movies never have this
/// field at all (always 1:1), so this only applies to Sonarr episode files.
fn map_release_type(raw: Option<&str>) -> ReleaseTypeKind {
    match raw.map(str::to_ascii_lowercase).as_deref() {
        Some("seasonpack") => ReleaseTypeKind::SeasonPack,
        Some("multiepisode") => ReleaseTypeKind::Multi,
        _ => ReleaseTypeKind::Single,
    }
}

async fn ingest_radarr_instance(
    pool: &PgPool,
    instance: &ArrInstanceConfig,
    summary: &mut IngestSummary,
) -> MuseResult<()> {
    let client = ArrClient::from_instance(instance)?;
    // A single call doubles as the connectivity check: an unreachable host,
    // timeout, or auth/parse failure here is what makes an offline instance
    // (e.g. radarr_animated) skip cleanly via the `?` below.
    let movies = client.movies().await?;
    let library = ensure_library(pool, instance).await?;

    for movie in &movies {
        match ingest_one_radarr_movie(pool, instance, &library, movie).await {
            Ok(()) => summary.movies_upserted += 1,
            Err(e) => tracing::warn!(
                instance = %instance.name,
                movie_id = movie.id,
                error = %e,
                "skipping one radarr movie that failed to ingest"
            ),
        }
    }

    Ok(())
}

async fn ingest_one_radarr_movie(
    pool: &PgPool,
    instance: &ArrInstanceConfig,
    library: &Library,
    movie: &super::models::RadarrMovie,
) -> MuseResult<()> {
    let metadata = repo::media_metadata::upsert_by_tmdb(
        pool,
        &NewMediaMetadata {
            kind: MediaKind::Movie,
            tmdb_id: Some(movie.tmdb_id.to_string()),
            tvdb_id: None,
            imdb_id: movie.imdb_id.clone(),
            provider_ids: serde_json::json!({}),
            title: movie.title.clone(),
            sort_title: movie.sort_title.clone(),
            original_title: movie.original_title.clone(),
            original_language: movie.original_language.as_ref().map(|l| l.name.clone()),
            status: movie.status.clone(),
            overview: movie.overview.clone(),
            studio: movie.studio.clone(),
            network: None,
            runtime_minutes: movie.runtime,
            year: movie.year,
            images: non_null_json(movie.images.clone()),
        },
    )
    .await?;

    let item = repo::media_item::upsert(
        pool,
        &NewMediaItem {
            library_id: library.id,
            media_metadata_id: metadata.id,
            path: movie.path.clone(),
            monitored: movie.monitored,
            quality_profile_id: None,
            minimum_availability: movie.minimum_availability.clone(),
            plex_rating_key: None,
            added_at: movie.added,
        },
    )
    .await?;

    if movie.has_file {
        if let Some(file) = &movie.movie_file {
            let quality_tier_id = match &file.quality {
                Some(q) => resolve_quality_tier(pool, instance.kind, q).await?,
                None => None,
            };

            upsert_media_file(
                pool,
                &NewMediaFile {
                    media_item_id: item.id,
                    relative_path: file.relative_path.clone(),
                    size_bytes: file.size,
                    release_group: file.release_group.clone(),
                    languages: file.languages.iter().map(|l| l.name.clone()).collect(),
                    // Movies are always 1:1 with their file (blueprint §2/§7.3).
                    release_type: ReleaseTypeKind::Single,
                    quality_tier_id,
                    revision: revision_from(file.quality.as_ref()),
                },
            )
            .await?;
        }
    }

    Ok(())
}

async fn ingest_sonarr_instance(
    pool: &PgPool,
    instance: &ArrInstanceConfig,
    summary: &mut IngestSummary,
) -> MuseResult<()> {
    let client = ArrClient::from_instance(instance)?;
    // Connectivity check + data fetch in one call, same as Radarr.
    let all_series = client.series().await?;
    let library = ensure_library(pool, instance).await?;

    for series in &all_series {
        match ingest_one_sonarr_series(pool, &client, instance, &library, series).await {
            Ok((episodes, files)) => {
                summary.series_upserted += 1;
                summary.episodes_upserted += episodes;
                summary.files_upserted += files;
            }
            Err(e) => tracing::warn!(
                instance = %instance.name,
                series_id = series.id,
                error = %e,
                "skipping one sonarr series that failed to ingest"
            ),
        }
    }

    Ok(())
}

/// Ingest one series: metadata + instance row + seasons + episode files +
/// episodes (files are fetched/upserted *before* episodes so each episode
/// can attach to its file by *arr's `episodeFileId` — the mechanism that
/// naturally captures season-pack many-to-many linkage, since N episodes
/// sharing one `episodeFileId` all resolve to the same `media_files` row).
/// Returns `(episodes_upserted, files_upserted)`.
async fn ingest_one_sonarr_series(
    pool: &PgPool,
    client: &ArrClient,
    instance: &ArrInstanceConfig,
    library: &Library,
    series: &super::models::SonarrSeries,
) -> MuseResult<(usize, usize)> {
    let mut provider_ids = serde_json::Map::new();
    if let Some(v) = series.tv_rage_id {
        provider_ids.insert("tvRageId".to_string(), serde_json::json!(v));
    }
    if let Some(v) = series.tv_maze_id {
        provider_ids.insert("tvMazeId".to_string(), serde_json::json!(v));
    }
    if !series.mal_ids.is_empty() {
        provider_ids.insert("malIds".to_string(), serde_json::json!(series.mal_ids));
    }
    if !series.anilist_ids.is_empty() {
        provider_ids.insert(
            "aniListIds".to_string(),
            serde_json::json!(series.anilist_ids),
        );
    }

    let metadata = repo::media_metadata::upsert_by_tvdb(
        pool,
        &NewMediaMetadata {
            kind: MediaKind::Show,
            tmdb_id: series.tmdb_id_opt(),
            tvdb_id: Some(series.tvdb_id.to_string()),
            imdb_id: series.imdb_id.clone(),
            provider_ids: Json::Object(provider_ids),
            title: series.title.clone(),
            sort_title: series.sort_title.clone(),
            original_title: None,
            original_language: series.original_language.as_ref().map(|l| l.name.clone()),
            status: series.status.clone(),
            overview: series.overview.clone(),
            studio: None,
            network: series.network.clone(),
            runtime_minutes: series.runtime,
            year: series.year,
            images: non_null_json(series.images.clone()),
        },
    )
    .await?;

    let item = repo::media_item::upsert(
        pool,
        &NewMediaItem {
            library_id: library.id,
            media_metadata_id: metadata.id,
            path: series.path.clone(),
            monitored: series.monitored,
            quality_profile_id: None,
            minimum_availability: None,
            plex_rating_key: None,
            added_at: series.added,
        },
    )
    .await?;

    let mut season_ids: HashMap<i32, i64> = HashMap::new();
    for season in &series.seasons {
        let row = repo::season::upsert(
            pool,
            &NewSeason {
                media_item_id: item.id,
                season_number: season.season_number,
                title: None,
                overview: None,
                monitored: season.monitored,
                air_date: None,
            },
        )
        .await?;
        season_ids.insert(season.season_number, row.id);
    }

    // Episode files first (see doc comment above) — a per-series 404/empty
    // response here shouldn't drop the whole series, so degrade to empty.
    let episode_files = client
        .episode_files(series.id)
        .await
        .unwrap_or_else(|e| {
            tracing::warn!(series_id = series.id, error = %e, "failed to fetch episode files; continuing with none");
            Vec::new()
        });

    let mut arr_file_id_to_ours: HashMap<i64, i64> = HashMap::new();
    let mut files_upserted = 0usize;
    for file in &episode_files {
        let quality_tier_id = match &file.quality {
            Some(q) => resolve_quality_tier(pool, instance.kind, q).await?,
            None => None,
        };

        let row = upsert_media_file(
            pool,
            &NewMediaFile {
                media_item_id: item.id,
                relative_path: file.relative_path.clone(),
                size_bytes: file.size,
                release_group: file.release_group.clone(),
                languages: file.languages.iter().map(|l| l.name.clone()).collect(),
                release_type: map_release_type(file.release_type.as_deref()),
                quality_tier_id,
                revision: revision_from(file.quality.as_ref()),
            },
        )
        .await?;

        arr_file_id_to_ours.insert(file.id, row.id);
        files_upserted += 1;
    }

    let episodes = client.episodes(series.id).await.unwrap_or_else(|e| {
        tracing::warn!(series_id = series.id, error = %e, "failed to fetch episodes; continuing with none");
        Vec::new()
    });

    let mut episodes_upserted = 0usize;
    for episode in &episodes {
        let season_id = match season_ids.get(&episode.season_number) {
            Some(id) => *id,
            None => {
                // Sonarr's Series.seasons didn't mention this season number
                // (unusual) — upsert a fallback row rather than drop every
                // episode in it.
                let row = repo::season::upsert(
                    pool,
                    &NewSeason {
                        media_item_id: item.id,
                        season_number: episode.season_number,
                        title: None,
                        overview: None,
                        monitored: false,
                        air_date: None,
                    },
                )
                .await?;
                season_ids.insert(episode.season_number, row.id);
                row.id
            }
        };

        let air_date = episode
            .air_date
            .as_deref()
            .and_then(|s| chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d").ok());

        let ep_row = match repo::episode::upsert(
            pool,
            &NewEpisode {
                season_id,
                media_item_id: item.id,
                episode_number: episode.episode_number,
                absolute_episode_number: episode.absolute_episode_number,
                title: episode.title.clone(),
                overview: episode.overview.clone(),
                air_date,
                air_date_utc: episode.air_date_utc,
                runtime_minutes: episode.runtime,
                monitored: episode.monitored,
                tvdb_id: episode.tvdb_id.map(|v| v.to_string()),
            },
        )
        .await
        {
            Ok(row) => row,
            Err(e) => {
                tracing::warn!(
                    series_id = series.id,
                    episode_id = episode.id,
                    error = %e,
                    "skipping one episode that failed to ingest"
                );
                continue;
            }
        };
        episodes_upserted += 1;

        if let Some(arr_file_id) = episode.episode_file_id_opt() {
            if let Some(&our_file_id) = arr_file_id_to_ours.get(&arr_file_id) {
                repo::media_file::attach_to_episode(pool, ep_row.id, our_file_id).await?;
                repo::episode::set_has_file(pool, ep_row.id, true).await?;
            }
        }
    }

    Ok((episodes_upserted, files_upserted))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn map_release_type_covers_all_arr_variants() {
        assert_eq!(
            map_release_type(Some("seasonPack")),
            ReleaseTypeKind::SeasonPack
        );
        assert_eq!(
            map_release_type(Some("multiEpisode")),
            ReleaseTypeKind::Multi
        );
        assert_eq!(
            map_release_type(Some("singleEpisode")),
            ReleaseTypeKind::Single
        );
        assert_eq!(map_release_type(None), ReleaseTypeKind::Single);
        assert_eq!(
            map_release_type(Some("something-unexpected")),
            ReleaseTypeKind::Single
        );
    }

    #[test]
    fn non_null_json_normalizes_null_to_empty_array() {
        assert_eq!(non_null_json(Json::Null), Json::Array(vec![]));
        let arr = serde_json::json!([{"a": 1}]);
        assert_eq!(non_null_json(arr.clone()), arr);
    }

    #[test]
    fn revision_from_defaults_when_no_quality_present() {
        let rev = revision_from(None);
        assert_eq!(rev.version, 1);
        assert_eq!(rev.real, 0);
        assert!(!rev.is_repack);
    }

    // --- DB-gated integration test (mirrors src/integration_tests.rs) ---
    //
    // Gated on MUSE_TEST_DATABASE_URL: skips cleanly (does NOT fail) when
    // unset, per the MUSE-02 build constraint that the suite must pass with
    // no live database. This is the "graceful-skip" requirement: one
    // instance (an unreachable port, standing in for the operator's
    // offline radarr_animated) must not abort ingest for the rest of the
    // fleet.
    #[tokio::test]
    async fn run_skips_an_unreachable_instance_without_aborting_the_rest() {
        let Ok(database_url) = std::env::var("MUSE_TEST_DATABASE_URL") else {
            eprintln!(
                "MUSE_TEST_DATABASE_URL not set — skipping run_skips_an_unreachable_instance_without_aborting_the_rest \
                 (this is expected in the default test run; the crate does not require a live DB)"
            );
            return;
        };

        use httpmock::prelude::*;
        use sqlx::postgres::PgPoolOptions;
        use uuid::Uuid;

        let pool = PgPoolOptions::new()
            .max_connections(5)
            .connect(&database_url)
            .await
            .expect("connect to MUSE_TEST_DATABASE_URL");
        sqlx::migrate!("./migrations")
            .run(&pool)
            .await
            .expect("migrations should apply cleanly");

        let suffix = Uuid::new_v4().simple().to_string();

        let healthy = MockServer::start();
        healthy.mock(|when, then| {
            when.method(GET).path("/api/v3/movie");
            then.status(200)
                .header("content-type", "application/json")
                .body(format!(
                    r#"[{{
                        "id": 1,
                        "title": "Ingest Test Movie {suffix}",
                        "tmdbId": 900001,
                        "imdbId": "tt9000001",
                        "year": 2022,
                        "path": "/media/Movies/Ingest Test Movie",
                        "hasFile": false,
                        "monitored": true,
                        "minimumAvailability": "released"
                    }}]"#
                ));
        });

        let instances = vec![
            ArrInstanceConfig {
                name: format!("radarr_healthy_{suffix}"),
                kind: ArrKind::Radarr,
                base_url: healthy.base_url(),
                api_key: "<REDACTED-SECRET>".to_string(),
                library_kind: crate::models::library::LibraryKind::Movie,
                root_folder: None,
            },
            ArrInstanceConfig {
                // Stands in for the offline `radarr_animated` instance: a
                // port nothing listens on.
                name: format!("radarr_offline_{suffix}"),
                kind: ArrKind::Radarr,
                base_url: "http://127.0.0.1:1".to_string(),
                api_key: "<REDACTED-SECRET>".to_string(),
                library_kind: crate::models::library::LibraryKind::Movie,
                root_folder: None,
            },
        ];

        let summary = run(&pool, &instances).await;

        assert_eq!(summary.instances_ok, vec![instances[0].name.clone()]);
        assert_eq!(summary.instances_skipped.len(), 1);
        assert_eq!(summary.instances_skipped[0].0, instances[1].name);
        assert_eq!(summary.movies_upserted, 1);

        let library = repo::library::get_by_name(&pool, &instances[0].name)
            .await
            .expect("query library")
            .expect("healthy instance's library should have been created");
        let items = repo::media_item::list_by_library(&pool, library.id)
            .await
            .expect("list media items");
        assert_eq!(items.len(), 1);

        // The offline instance must NOT have created a library row at all —
        // it never got past the connectivity-check `?` in
        // ingest_radarr_instance.
        let offline_library = repo::library::get_by_name(&pool, &instances[1].name)
            .await
            .expect("query offline library");
        assert!(offline_library.is_none());
    }
}
