//! MUSE-02 migration-apply + round-trip integration test.
//!
//! Gated on `MUSE_TEST_DATABASE_URL` — when unset this test logs and
//! returns cleanly (does NOT fail), per the MUSE-02 build constraint that
//! the suite must pass with no live database. Set it to a scratch Postgres
//! 16+ database to actually exercise migrations + the repo layer, e.g.:
//!
//!   MUSE_TEST_DATABASE_URL=postgres://user:pass@host/muse_test cargo test
//!
//! This lives inside the binary crate (not `tests/`) because `muse` has no
//! `[lib]` target — an external integration test crate couldn't reach
//! `crate::repo`/`crate::models` otherwise.

use sqlx::postgres::PgPoolOptions;
use uuid::Uuid;

use crate::models::episode::NewEpisode;
use crate::models::library::{LibraryKind, NewLibrary};
use crate::models::media_file::{NewMediaFile, ReleaseTypeKind, Revision};
use crate::models::media_item::NewMediaItem;
use crate::models::media_metadata::{MediaKind, NewMediaMetadata};
use crate::models::quality::{NewCustomFormat, NewQualityDefinition, NewQualityProfile};
use crate::models::release::NewRelease;
use crate::models::season::NewSeason;
use crate::models::indexer::NewIndexer;
use crate::repo;

#[tokio::test]
async fn core_schema_migrates_and_round_trips() {
    let Ok(database_url) = std::env::var("MUSE_TEST_DATABASE_URL") else {
        eprintln!(
            "MUSE_TEST_DATABASE_URL not set — skipping core_schema_migrates_and_round_trips \
             (this is expected in the default test run; the crate does not require a live DB)"
        );
        return;
    };

    let pool = PgPoolOptions::new()
        .max_connections(5)
        .connect(&database_url)
        .await
        .expect("connect to MUSE_TEST_DATABASE_URL");

    sqlx::migrate!("./migrations")
        .run(&pool)
        .await
        .expect("migrations should apply cleanly (FK ordering)");

    let suffix = Uuid::new_v4().simple().to_string();

    // --- library ---
    let library = repo::library::create(
        &pool,
        &NewLibrary {
            name: format!("radarr_test_{suffix}"),
            kind: LibraryKind::Movie,
            root_folder: "/media/Movies/".to_string(),
            source_arr_name: Some("radarr".to_string()),
            source_arr_url: None,
        },
    )
    .await
    .expect("create library");
    assert_eq!(library.kind, LibraryKind::Movie);

    // --- quality definition + profile + custom format (scorer seam) ---
    let quality_def = repo::quality::create_definition(
        &pool,
        &NewQualityDefinition {
            quality_key: format!("bluray-1080p-remux-{suffix}"),
            title: "Bluray-1080p Remux".to_string(),
            source: "remux".to_string(),
            resolution: Some("1080p".to_string()),
            modifier: "none".to_string(),
            sort_order: 30,
        },
    )
    .await
    .expect("create quality definition");

    let profile = repo::quality::create_profile(
        &pool,
        &NewQualityProfile {
            name: format!("HD-1080p-{suffix}"),
            cutoff_quality_id: Some(quality_def.id),
            items: serde_json::json!([]),
            upgrade_allowed: true,
            natural_language_intent: Some("small, good-enough, no HDR".to_string()),
        },
    )
    .await
    .expect("create quality profile");

    let custom_format = repo::quality::create_custom_format(
        &pool,
        &NewCustomFormat {
            name: format!("no-cam-{suffix}"),
            specifications: serde_json::json!([{"implementation": "SourceSpecification", "negate": true, "required": true, "fields": {"value": "cam"}}]),
            include_when_renaming: false,
        },
    )
    .await
    .expect("create custom format");

    repo::quality::set_profile_format_score(&pool, profile.id, custom_format.id, -1000)
        .await
        .expect("set profile format score");
    let scores = repo::quality::list_profile_format_scores(&pool, profile.id)
        .await
        .expect("list profile format scores");
    assert_eq!(scores.len(), 1);
    assert_eq!(scores[0].score, -1000);

    // --- media_metadata (movie) shared across libraries ---
    let metadata = repo::media_metadata::upsert_by_tmdb(
        &pool,
        &NewMediaMetadata {
            kind: MediaKind::Movie,
            tmdb_id: Some(format!("tmdb-{suffix}")),
            tvdb_id: None,
            imdb_id: None,
            provider_ids: serde_json::json!({}),
            title: "Test Movie".to_string(),
            sort_title: Some("test movie".to_string()),
            original_title: None,
            original_language: Some("en".to_string()),
            status: Some("released".to_string()),
            overview: None,
            studio: None,
            network: None,
            runtime_minutes: Some(120),
            year: Some(2020),
            images: serde_json::json!([]),
        },
    )
    .await
    .expect("upsert media_metadata by tmdb");
    assert_eq!(metadata.kind, MediaKind::Movie);

    // --- media_item (per-library instance) ---
    let item = repo::media_item::upsert(
        &pool,
        &NewMediaItem {
            library_id: library.id,
            media_metadata_id: metadata.id,
            path: "/media/Movies/Test Movie (2020)".to_string(),
            monitored: true,
            quality_profile_id: Some(profile.id),
            minimum_availability: Some("released".to_string()),
            plex_rating_key: None,
            added_at: None,
        },
    )
    .await
    .expect("upsert media_item");

    // --- movie file: 1:1 via media_item_id ---
    let movie_file = repo::media_file::create(
        &pool,
        &NewMediaFile {
            media_item_id: item.id,
            relative_path: "Test Movie (2020)/Test.Movie.2020.1080p.BluRay.Remux.mkv".to_string(),
            size_bytes: Some(30_000_000_000),
            release_group: Some("FGT".to_string()),
            languages: vec!["eng".to_string()],
            release_type: ReleaseTypeKind::Single,
            quality_tier_id: Some(quality_def.id),
            revision: Revision {
                version: 1,
                real: 0,
                is_repack: false,
            },
        },
    )
    .await
    .expect("create movie media_file");
    assert_eq!(movie_file.revision().version, 1);

    // --- TV hierarchy: a second library + show metadata + season/episode + season-pack file ---
    let tv_library = repo::library::create(
        &pool,
        &NewLibrary {
            name: format!("sonarr_test_{suffix}"),
            kind: LibraryKind::Tv,
            root_folder: "/media/TV/".to_string(),
            source_arr_name: Some("sonarr".to_string()),
            source_arr_url: None,
        },
    )
    .await
    .expect("create tv library");

    let show_metadata = repo::media_metadata::upsert_by_tvdb(
        &pool,
        &NewMediaMetadata {
            kind: MediaKind::Show,
            tmdb_id: None,
            tvdb_id: Some(format!("tvdb-{suffix}")),
            imdb_id: None,
            provider_ids: serde_json::json!({"tvmaze": "12345"}),
            title: "Test Show".to_string(),
            sort_title: None,
            original_title: None,
            original_language: Some("en".to_string()),
            status: Some("continuing".to_string()),
            overview: None,
            studio: None,
            network: Some("Test Network".to_string()),
            runtime_minutes: Some(30),
            year: Some(2021),
            images: serde_json::json!([]),
        },
    )
    .await
    .expect("upsert media_metadata by tvdb");

    let show_item = repo::media_item::upsert(
        &pool,
        &NewMediaItem {
            library_id: tv_library.id,
            media_metadata_id: show_metadata.id,
            path: "/media/TV/Test Show".to_string(),
            monitored: true,
            quality_profile_id: Some(profile.id),
            minimum_availability: None,
            plex_rating_key: None,
            added_at: None,
        },
    )
    .await
    .expect("upsert show media_item");

    let season = repo::season::upsert(
        &pool,
        &NewSeason {
            media_item_id: show_item.id,
            season_number: 1,
            title: None,
            overview: None,
            monitored: true,
            air_date: None,
        },
    )
    .await
    .expect("upsert season");

    let ep1 = repo::episode::upsert(
        &pool,
        &NewEpisode {
            season_id: season.id,
            media_item_id: show_item.id,
            episode_number: 1,
            absolute_episode_number: Some(1),
            title: Some("Pilot".to_string()),
            overview: None,
            air_date: None,
            air_date_utc: None,
            runtime_minutes: Some(30),
            monitored: true,
            tvdb_id: None,
        },
    )
    .await
    .expect("upsert episode 1");
    let ep2 = repo::episode::upsert(
        &pool,
        &NewEpisode {
            season_id: season.id,
            media_item_id: show_item.id,
            episode_number: 2,
            absolute_episode_number: Some(2),
            title: Some("Episode Two".to_string()),
            overview: None,
            air_date: None,
            air_date_utc: None,
            runtime_minutes: Some(30),
            monitored: true,
            tvdb_id: None,
        },
    )
    .await
    .expect("upsert episode 2");

    // A single season-pack file satisfies BOTH episodes — the many-to-many case.
    let season_pack_file = repo::media_file::create(
        &pool,
        &NewMediaFile {
            media_item_id: show_item.id,
            relative_path: "Test Show/Season 01/Test.Show.S01.1080p.WEB-DL.mkv".to_string(),
            size_bytes: Some(8_000_000_000),
            release_group: Some("NTb".to_string()),
            languages: vec!["eng".to_string()],
            release_type: ReleaseTypeKind::SeasonPack,
            quality_tier_id: Some(quality_def.id),
            revision: Revision {
                version: 1,
                real: 0,
                is_repack: false,
            },
        },
    )
    .await
    .expect("create season-pack media_file");

    repo::media_file::attach_to_episode(&pool, ep1.id, season_pack_file.id)
        .await
        .expect("attach season pack to ep1");
    repo::media_file::attach_to_episode(&pool, ep2.id, season_pack_file.id)
        .await
        .expect("attach season pack to ep2");
    repo::episode::set_has_file(&pool, ep1.id, true)
        .await
        .expect("set ep1 has_file");
    repo::episode::set_has_file(&pool, ep2.id, true)
        .await
        .expect("set ep2 has_file");

    let files_for_ep1 = repo::media_file::list_for_episode(&pool, ep1.id)
        .await
        .expect("list files for ep1");
    assert_eq!(files_for_ep1.len(), 1);
    assert_eq!(files_for_ep1[0].id, season_pack_file.id);

    let episode_ids_for_file = repo::media_file::list_episode_ids_for_file(&pool, season_pack_file.id)
        .await
        .expect("list episode ids for season pack file");
    assert_eq!(episode_ids_for_file.len(), 2);
    assert!(episode_ids_for_file.contains(&ep1.id));
    assert!(episode_ids_for_file.contains(&ep2.id));

    let episodes = repo::episode::list_by_season(&pool, season.id)
        .await
        .expect("list episodes by season");
    assert_eq!(episodes.len(), 2);
    assert!(episodes.iter().all(|e| e.has_file));

    let fetched_item = repo::media_item::get(&pool, item.id).await.expect("get media_item");
    assert_eq!(fetched_item.media_metadata_id, metadata.id);

    let searched = repo::media_metadata::search_by_title(&pool, "Test Movie", 5)
        .await
        .expect("trigram search by title");
    assert!(searched.iter().any(|m| m.id == metadata.id));

    // Cross-show integrity: attaching a file that belongs to a DIFFERENT
    // media_item (the movie file) to a show episode must be rejected by the
    // episode_files composite FK — not silently accepted.
    let cross_show = repo::media_file::attach_to_episode(&pool, ep1.id, movie_file.id).await;
    assert!(
        cross_show.is_err(),
        "attaching a file from another media_item to an episode must be blocked by the composite FK"
    );

    // --- MUSE-16: indexers / releases / availability round trip ---
    let prowlarr_id: i32 = (Uuid::new_v4().as_u128() % 1_000_000) as i32;
    let indexer = repo::indexer::upsert(
        &pool,
        &NewIndexer {
            prowlarr_id,
            name: format!("test-indexer-{suffix}"),
            protocol: Some("torrent".to_string()),
            privacy: Some("public".to_string()),
            enabled: true,
            categories: vec![2000, 2010],
            polite_min_interval_secs: 900,
        },
    )
    .await
    .expect("upsert indexer");
    assert!(indexer.enabled);
    assert_eq!(indexer.categories, vec![2000, 2010]);

    let enabled = repo::indexer::list_enabled(&pool)
        .await
        .expect("list enabled indexers");
    assert!(enabled.iter().any(|i| i.id == indexer.id));

    repo::indexer::mark_rss_pulled(&pool, indexer.id)
        .await
        .expect("mark rss pulled");
    let refetched_indexer = repo::indexer::get(&pool, indexer.id).await.expect("get indexer");
    assert!(refetched_indexer.last_rss_pull_at.is_some());

    // A release that resolves to the movie metadata upserted above.
    let good_release = repo::release::upsert(
        &pool,
        &NewRelease {
            media_metadata_id: Some(metadata.id),
            indexer_id: indexer.id,
            guid: format!("guid-good-{suffix}"),
            title: "Test.Movie.2020.2160p.BluRay.REMUX.x265-GRP".to_string(),
            seeders: Some(120),
            leechers: Some(4),
            size_bytes: Some(45_000_000_000),
            freeleech: true,
            quality: Some("BluRay-2160p".to_string()),
            resolution: Some("2160p".to_string()),
            source: Some("BluRay".to_string()),
            video_codec: Some("x265".to_string()),
            release_group: Some("GRP".to_string()),
            parse_confidence: Some(0.8),
            ..Default::default()
        },
    )
    .await
    .expect("upsert resolved release");
    assert_eq!(good_release.media_metadata_id, Some(metadata.id));

    // A lower-seeder release for the same title (still resolved).
    repo::release::upsert(
        &pool,
        &NewRelease {
            media_metadata_id: Some(metadata.id),
            indexer_id: indexer.id,
            guid: format!("guid-second-{suffix}"),
            title: "Test.Movie.2020.1080p.WEB-DL.x264-TEAM".to_string(),
            seeders: Some(10),
            size_bytes: Some(6_000_000_000),
            freeleech: false,
            quality: Some("WEB-DL-1080p".to_string()),
            ..Default::default()
        },
    )
    .await
    .expect("upsert second release");

    // An unresolved release (no title match yet) — negative-space discovery.
    repo::release::upsert(
        &pool,
        &NewRelease {
            media_metadata_id: None,
            indexer_id: indexer.id,
            guid: format!("guid-unresolved-{suffix}"),
            title: "Some.Unmatched.Release.2019.720p.HDTV.x264-XYZ".to_string(),
            ..Default::default()
        },
    )
    .await
    .expect("upsert unresolved release");

    let releases_for_title = repo::release::list_by_media_metadata(&pool, metadata.id)
        .await
        .expect("list releases by media_metadata");
    assert_eq!(releases_for_title.len(), 2);

    let unresolved = repo::release::list_unresolved(&pool, 50)
        .await
        .expect("list unresolved releases");
    assert!(unresolved.iter().any(|r| r.guid == format!("guid-unresolved-{suffix}")));

    // Re-upserting the same (indexer_id, guid) should update in place, not
    // duplicate, and must not clobber a resolved media_metadata_id with a
    // NULL from a later re-seen (unresolved-at-that-moment) report.
    let reupserted = repo::release::upsert(
        &pool,
        &NewRelease {
            media_metadata_id: None,
            indexer_id: indexer.id,
            guid: format!("guid-good-{suffix}"),
            title: "Test.Movie.2020.2160p.BluRay.REMUX.x265-GRP".to_string(),
            seeders: Some(150),
            // Prowlarr-reported fields (seeders, freeleech, ...) are always
            // refreshed wholesale on a re-pull — only media_metadata_id (an
            // out-of-band Muse resolution, not something Prowlarr reports)
            // is coalesced against the prior row. This re-observes freeleech
            // still true, same as the original upsert.
            freeleech: true,
            ..Default::default()
        },
    )
    .await
    .expect("re-upsert good release");
    assert_eq!(reupserted.id, good_release.id);
    assert_eq!(reupserted.media_metadata_id, Some(metadata.id));
    assert_eq!(reupserted.seeders, Some(150));

    let rollup = repo::availability::recompute(&pool, metadata.id)
        .await
        .expect("recompute availability rollup");
    assert_eq!(rollup.media_metadata_id, metadata.id);
    assert_eq!(rollup.release_count, 2);
    assert_eq!(rollup.best_seeders, Some(150));
    assert!(rollup.has_freeleech);

    let fetched_rollup = repo::availability::get(&pool, metadata.id)
        .await
        .expect("get availability rollup");
    assert_eq!(fetched_rollup.release_count, 2);

    let no_release_metadata = repo::media_metadata::upsert_by_tmdb(
        &pool,
        &NewMediaMetadata {
            kind: MediaKind::Movie,
            tmdb_id: Some(format!("tmdb-no-releases-{suffix}")),
            tvdb_id: None,
            imdb_id: None,
            provider_ids: serde_json::json!({}),
            title: "Nothing Available Yet".to_string(),
            sort_title: None,
            original_title: None,
            original_language: None,
            status: None,
            overview: None,
            studio: None,
            network: None,
            runtime_minutes: None,
            year: None,
            images: serde_json::json!([]),
        },
    )
    .await
    .expect("upsert media_metadata with no releases");
    let empty_rollup = repo::availability::recompute(&pool, no_release_metadata.id)
        .await
        .expect("recompute availability rollup with zero releases");
    assert_eq!(empty_rollup.release_count, 0);
    assert!(!empty_rollup.has_freeleech);
    assert!(empty_rollup.best_quality.is_none());

    let pruned = repo::release::prune_expired(&pool)
        .await
        .expect("prune expired releases should not error even with none expired");
    assert_eq!(pruned, 0);
}
