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

use crate::models::account::NewAccount;
use crate::models::channel::{
    ChannelKind, ChannelMode, ChannelProgramItemType, ChannelRunStatus, NewChannel,
    NewChannelProgram, NewChannelRun,
};
use crate::models::embedding::{EmbeddingEntityKind, NewEmbedding};
use crate::models::episode::NewEpisode;
use crate::models::external_enrichment::NewExternalEnrichment;
use crate::models::interstitial::{InterstitialKind, NewInterstitial};
use crate::models::library::{LibraryKind, NewLibrary};
use crate::models::media_file::{NewMediaFile, ReleaseTypeKind, Revision};
use crate::models::media_item::NewMediaItem;
use crate::models::indexer::NewIndexer;
use crate::models::media_metadata::{MediaKind, NewMediaMetadata};
use crate::models::play_event::NewPlayEvent;
use crate::models::play_session::{DecisionKind, NewPlaySession, NewPlaySessionMediaInfo};
use crate::models::proactive_item::NewProactiveItem;
use crate::models::quality::{NewCustomFormat, NewQualityDefinition, NewQualityProfile};
use crate::models::release::NewRelease;
use crate::models::season::NewSeason;
use crate::models::taste::{NewTasteContextCentroid, NewTasteProfile, NewTasteSignal};
use crate::models::trending::{NewPopulationProfile, NewStreamingAvailability, NewTrendingSnapshot};
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

    // --- MUSE-19: trending/population feed round-trip ---
    // `metadata` (the movie created above) doubles as the "resolved to
    // library" case; a bare `external_ref`-only row covers the (far more
    // common) unresolved case.
    let region = format!("XX-{suffix}"); // scoped region so this test's rows never collide with others'

    let resolved_snapshot = repo::trending::insert_snapshot(
        &pool,
        &NewTrendingSnapshot {
            source: "tmdb".to_string(),
            scope: "trending".to_string(),
            platform: None,
            region: region.clone(),
            window: "day".to_string(),
            rank: Some(1),
            media_metadata_id: Some(metadata.id),
            external_ref: None,
            popularity: Some(42.5),
        },
    )
    .await
    .expect("insert resolved trending snapshot");
    assert_eq!(resolved_snapshot.media_metadata_id, Some(metadata.id));

    let unresolved_snapshot = repo::trending::insert_snapshot(
        &pool,
        &NewTrendingSnapshot {
            source: "tmdb".to_string(),
            scope: "trending".to_string(),
            platform: None,
            region: region.clone(),
            window: "day".to_string(),
            rank: Some(2),
            media_metadata_id: None,
            external_ref: Some(serde_json::json!({"tmdb_id": "999999", "title": "Some Unlibraried Show", "year": 2026})),
            popularity: Some(10.0),
        },
    )
    .await
    .expect("insert unresolved trending snapshot");
    assert!(unresolved_snapshot.media_metadata_id.is_none());
    assert!(unresolved_snapshot.external_ref.is_some());

    let recent = repo::trending::list_recent(&pool, "trending", &region, 10)
        .await
        .expect("list recent trending snapshots");
    assert_eq!(recent.len(), 2);

    let resolved_by_tmdb = repo::media_metadata::find_by_tmdb_id(
        &pool,
        MediaKind::Movie,
        &format!("tmdb-{suffix}"),
    )
    .await
    .expect("find_by_tmdb_id should not error");
    assert_eq!(resolved_by_tmdb, Some(metadata.id));

    let availability = repo::trending::upsert_streaming_availability(
        &pool,
        &NewStreamingAvailability {
            media_metadata_id: metadata.id,
            provider: "netflix".to_string(),
            region: region.clone(),
            offer_type: "flatrate".to_string(),
            link: Some("https://example.invalid/watch".to_string()),
        },
    )
    .await
    .expect("upsert streaming availability");
    assert_eq!(availability.provider, "netflix");

    // Re-upsert with a changed link exercises the ON CONFLICT DO UPDATE path
    // rather than duplicating the (media_metadata_id, provider, region,
    // offer_type) row.
    let availability_updated = repo::trending::upsert_streaming_availability(
        &pool,
        &NewStreamingAvailability {
            media_metadata_id: metadata.id,
            provider: "netflix".to_string(),
            region: region.clone(),
            offer_type: "flatrate".to_string(),
            link: Some("https://example.invalid/watch-v2".to_string()),
        },
    )
    .await
    .expect("re-upsert streaming availability");
    assert_eq!(availability_updated.link.as_deref(), Some("https://example.invalid/watch-v2"));

    let listed_availability = repo::trending::list_streaming_availability(&pool, metadata.id)
        .await
        .expect("list streaming availability");
    assert_eq!(listed_availability.len(), 1, "re-upsert must not duplicate the row");

    let sample_size_before = repo::trending::count_recent_snapshots(&pool, &region)
        .await
        .expect("count recent snapshots");
    assert_eq!(sample_size_before, 2);

    let profile = repo::trending::insert_population_profile(
        &pool,
        &NewPopulationProfile {
            window: "week".to_string(),
            region: region.clone(),
            genre_distribution: serde_json::json!({}),
            decade_distribution: None,
            runtime_distribution: None,
            sample_size: Some(sample_size_before as i32),
        },
    )
    .await
    .expect("insert population profile");
    assert_eq!(profile.sample_size, Some(2));

    let latest_profile = repo::trending::latest_population_profile(&pool, "week", &region)
        .await
        .expect("latest population profile")
        .expect("a population profile row should exist for this window/region");
    assert_eq!(latest_profile.id, profile.id);
}

/// MUSE-23: interstitials + channels + channel_runs + channel_programs
/// (the linear EPG grid) round-trip. Gated on `MUSE_TEST_DATABASE_URL` per
/// the same skip pattern as `core_schema_migrates_and_round_trips` above —
/// unset means "skip cleanly," not "fail."
///
/// NOTE: this migrates cleanly only once `plex_clients` (MUSE-22,
/// `migrations/0090_plex_clients.sql`) exists ahead of MUSE-23's 0091-0099
/// block — see the ordering-assumption comment in
/// `migrations/0092_channels.sql`. This test never inserts a `plex_clients`
/// row itself (all `target_client_id` fields are left `None`), so it does
/// not need one to exist as data — only the table itself, for the FK.
#[tokio::test]
async fn channels_schema_round_trips() {
    let Ok(database_url) = std::env::var("MUSE_TEST_DATABASE_URL") else {
        eprintln!(
            "MUSE_TEST_DATABASE_URL not set — skipping channels_schema_round_trips \
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
        .expect("migrations should apply cleanly (FK ordering, incl. plex_clients ahead of MUSE-23)");

    let suffix = Uuid::new_v4().simple().to_string();

    // --- minimal content fixtures: a show + episode for the grid to reference ---
    let library = repo::library::create(
        &pool,
        &NewLibrary {
            name: format!("sonarr_muse23_{suffix}"),
            kind: LibraryKind::Tv,
            root_folder: "/media/TV/".to_string(),
            source_arr_name: Some("sonarr".to_string()),
            source_arr_url: None,
        },
    )
    .await
    .expect("create library");

    let show_metadata = repo::media_metadata::upsert_by_tvdb(
        &pool,
        &NewMediaMetadata {
            kind: MediaKind::Show,
            tmdb_id: None,
            tvdb_id: Some(format!("tvdb-muse23-{suffix}")),
            imdb_id: None,
            provider_ids: serde_json::json!({}),
            title: "Muse Test Show".to_string(),
            sort_title: None,
            original_title: None,
            original_language: Some("en".to_string()),
            status: Some("continuing".to_string()),
            overview: None,
            studio: None,
            network: None,
            runtime_minutes: Some(22),
            year: Some(1994),
            images: serde_json::json!([]),
        },
    )
    .await
    .expect("upsert show media_metadata");

    let show_item = repo::media_item::upsert(
        &pool,
        &NewMediaItem {
            library_id: library.id,
            media_metadata_id: show_metadata.id,
            path: "/media/TV/Muse Test Show".to_string(),
            monitored: true,
            quality_profile_id: None,
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

    let episode = repo::episode::upsert(
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
            runtime_minutes: Some(22),
            monitored: true,
            tvdb_id: None,
        },
    )
    .await
    .expect("upsert episode");

    // --- interstitials: a themed pool the composer would pick from ---
    let bumper = repo::interstitial::upsert(
        &pool,
        &NewInterstitial {
            plex_rating_key: Some(format!("plex-bumper-{suffix}")),
            kind: InterstitialKind::Bumper,
            title: Some("Saturday Morning Bumper".to_string()),
            decade: Some(1990),
            theme: Some("saturday_morning".to_string()),
            genre: None,
            mood: Some("upbeat".to_string()),
            duration_ms: Some(15_000),
            tags: vec!["retro".to_string(), "cartoon".to_string()],
            source: Some("plex_library".to_string()),
        },
    )
    .await
    .expect("upsert interstitial");
    assert_eq!(bumper.kind, InterstitialKind::Bumper);

    let queried = repo::interstitial::list_by_kind_decade_theme(
        &pool,
        Some(InterstitialKind::Bumper),
        Some(1990),
        Some("saturday_morning"),
    )
    .await
    .expect("query interstitials by kind/decade/theme");
    assert!(queried.iter().any(|i| i.id == bumper.id));

    let by_tag = repo::interstitial::list_by_tag(&pool, "retro")
        .await
        .expect("query interstitials by tag");
    assert!(by_tag.iter().any(|i| i.id == bumper.id));

    // --- channel definition (on_demand, per spec §3.8) ---
    let channel = repo::channel::create_channel(
        &pool,
        &NewChannel {
            account_id: None, // seam: accounts not yet built (MUSE-03)
            name: format!("Saturday Morning {suffix}"),
            kind: ChannelKind::Preset,
            mode: ChannelMode::OnDemand,
            channel_number: None,
            target_client_id: None, // seam: plex_clients row not needed for this test
            directive: Some("an ep of each cartoon + retro ads, 30 min".to_string()),
            rules: serde_json::json!({"interstitial_ratio": 0.2}),
            is_preset: true,
        },
    )
    .await
    .expect("create channel");
    assert_eq!(channel.mode, ChannelMode::OnDemand);

    let fetched_channel = repo::channel::get_channel(&pool, channel.id)
        .await
        .expect("get channel");
    assert_eq!(fetched_channel.id, channel.id);

    let presets = repo::channel::list_presets(&pool)
        .await
        .expect("list presets");
    assert!(presets.iter().any(|c| c.id == channel.id));

    // --- a composed run for that channel ---
    let run = repo::channel::create_run(
        &pool,
        &NewChannelRun {
            channel_id: Some(channel.id),
            account_id: None,
            target_client_id: None,
            plex_play_queue_id: None,
            schedule: serde_json::json!([
                {"type": "interstitial", "ref": bumper.id, "title": "Saturday Morning Bumper"},
                {"type": "episode", "ref": episode.id, "title": "Pilot"},
            ]),
            total_duration_ms: Some(bumper.duration_ms.unwrap_or(0) + 22 * 60_000),
        },
    )
    .await
    .expect("create channel_run");
    assert_eq!(run.status, ChannelRunStatus::Composed);

    let started = repo::channel::set_run_status(&pool, run.id, ChannelRunStatus::Playing)
        .await
        .expect("transition run to playing");
    assert_eq!(started.status, ChannelRunStatus::Playing);
    assert!(started.started_at.is_some());

    let completed = repo::channel::set_run_status(&pool, run.id, ChannelRunStatus::Completed)
        .await
        .expect("transition run to completed");
    assert_eq!(completed.status, ChannelRunStatus::Completed);
    assert!(completed.ended_at.is_some());

    let runs_for_channel = repo::channel::list_runs_by_channel(&pool, channel.id)
        .await
        .expect("list runs by channel");
    assert!(runs_for_channel.iter().any(|r| r.id == run.id));

    // --- linear EPG grid: the channel_programs the future XMLTV guide reads ---
    let now = chrono::Utc::now();
    let bumper_program = repo::channel::create_program(
        &pool,
        &NewChannelProgram {
            channel_id: channel.id,
            item_type: ChannelProgramItemType::Interstitial,
            media_item_id: None,
            episode_id: None,
            interstitial_id: Some(bumper.id),
            title: "Saturday Morning Bumper".to_string(),
            subtitle: None,
            description: None,
            artwork_url: None,
            start_at: now,
            end_at: now + chrono::Duration::milliseconds(15_000),
            duration_ms: 15_000,
            rationale: Some("opened with the era-matched bumper".to_string()),
        },
    )
    .await
    .expect("create bumper channel_program");
    assert_eq!(bumper_program.item_type, ChannelProgramItemType::Interstitial);

    let episode_start = now + chrono::Duration::milliseconds(15_000);
    let episode_program = repo::channel::create_program(
        &pool,
        &NewChannelProgram {
            channel_id: channel.id,
            item_type: ChannelProgramItemType::Episode,
            media_item_id: None,
            episode_id: Some(episode.id),
            interstitial_id: None,
            title: "Muse Test Show".to_string(),
            subtitle: Some("S1E1 — Pilot".to_string()),
            description: None,
            artwork_url: None,
            start_at: episode_start,
            end_at: episode_start + chrono::Duration::minutes(22),
            duration_ms: 22 * 60_000,
            rationale: Some("next-unwatched episode".to_string()),
        },
    )
    .await
    .expect("create episode channel_program");
    assert_eq!(episode_program.episode_id, Some(episode.id));

    let grid = repo::channel::list_programs_in_window(
        &pool,
        channel.id,
        now,
        episode_start + chrono::Duration::minutes(22),
    )
    .await
    .expect("list programs in window");
    assert_eq!(grid.len(), 2);
    assert_eq!(grid[0].id, bumper_program.id, "grid must be ordered by start_at");
    assert_eq!(grid[1].id, episode_program.id);

    let now_playing = repo::channel::current_program(&pool, channel.id, now)
        .await
        .expect("query current program")
        .expect("a program should be airing at `now`");
    assert_eq!(now_playing.id, bumper_program.id);

    let with_play_event =
        repo::channel::set_program_play_event(&pool, bumper_program.id, 42)
            .await
            .expect("attach a play_event seam id");
    assert_eq!(with_play_event.play_event_id, Some(42));

    // Negative test: a channel_program with no content reference at all
    // (no media_item/episode/interstitial) must be rejected by the
    // `CHECK` constraint, not silently accepted as an empty slot.
    let no_content_ref = sqlx::query(
        r#"
        INSERT INTO channel_programs (
            channel_id, item_type, title, start_at, end_at, duration_ms
        ) VALUES ($1, 'movie', 'Nothing', now(), now() + interval '1 hour', 3600000)
        "#,
    )
    .bind(channel.id)
    .execute(&pool)
    .await;
    assert!(
        no_content_ref.is_err(),
        "a channel_program with no media_item/episode/interstitial reference must be rejected"
    );
}

/// MUSE-03: migration-apply + round-trip test for the telemetry / taste /
/// embeddings / enrichment schema (migrations 0012-0022). Gated on
/// `MUSE_TEST_DATABASE_URL` exactly like `core_schema_migrates_and_round_trips`
/// above — skips cleanly (does not fail) when unset.
#[tokio::test]
async fn telemetry_taste_embeddings_schema_migrates_and_round_trips() {
    let Ok(database_url) = std::env::var("MUSE_TEST_DATABASE_URL") else {
        eprintln!(
            "MUSE_TEST_DATABASE_URL not set — skipping \
             telemetry_taste_embeddings_schema_migrates_and_round_trips \
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

    // --- account (per-account separation is mandatory for telemetry/taste) ---
    let account = repo::account::upsert_by_plex_account_id(
        &pool,
        &NewAccount {
            plex_account_id: Some(format!("plex-{suffix}")),
            username: Some(format!("user_{suffix}")),
            friendly_name: Some("Test User".to_string()),
            is_home_user: true,
            is_primary: false,
        },
    )
    .await
    .expect("upsert account");

    let other_account = repo::account::create(
        &pool,
        &NewAccount {
            plex_account_id: Some(format!("plex-other-{suffix}")),
            username: Some(format!("other_{suffix}")),
            friendly_name: None,
            is_home_user: true,
            is_primary: false,
        },
    )
    .await
    .expect("create second account (never blend users)");
    assert_ne!(account.id, other_account.id);

    // --- minimal library + media_metadata + media_item (a movie) to hang telemetry off ---
    let library = repo::library::create(
        &pool,
        &NewLibrary {
            name: format!("radarr_muse03_{suffix}"),
            kind: LibraryKind::Movie,
            root_folder: "/media/Movies/".to_string(),
            source_arr_name: Some("radarr".to_string()),
            source_arr_url: None,
        },
    )
    .await
    .expect("create library");

    let metadata = repo::media_metadata::upsert_by_tmdb(
        &pool,
        &NewMediaMetadata {
            kind: MediaKind::Movie,
            tmdb_id: Some(format!("tmdb-muse03-{suffix}")),
            tvdb_id: None,
            imdb_id: None,
            provider_ids: serde_json::json!({}),
            title: "Telemetry Test Movie".to_string(),
            sort_title: None,
            original_title: None,
            original_language: Some("en".to_string()),
            status: Some("released".to_string()),
            overview: None,
            studio: None,
            network: None,
            runtime_minutes: Some(110),
            year: Some(2022),
            images: serde_json::json!([]),
        },
    )
    .await
    .expect("upsert media_metadata");

    let item = repo::media_item::upsert(
        &pool,
        &NewMediaItem {
            library_id: library.id,
            media_metadata_id: metadata.id,
            path: "/media/Movies/Telemetry Test Movie (2022)".to_string(),
            monitored: true,
            quality_profile_id: None,
            minimum_availability: None,
            plex_rating_key: Some(format!("plex-rk-{suffix}")),
            added_at: None,
        },
    )
    .await
    .expect("upsert media_item");

    // --- play_events: raw stream, dedup on the UNIQUE constraint ---
    let started_at = chrono::Utc::now();
    let new_event = NewPlayEvent {
        source: "plex_webhook".to_string(),
        event_type: "media.play".to_string(),
        account_ref: account.plex_account_id.clone(),
        session_key: Some(format!("session-{suffix}")),
        rating_key: item.plex_rating_key.clone(),
        view_offset_ms: Some(0),
        player: Some("Living Room TV".to_string()),
        platform: Some("Plex for Android TV".to_string()),
        product: Some("Plex for Android (TV)".to_string()),
        device: Some("Chromecast".to_string()),
        ip_address: None,
        raw: serde_json::json!({"event": "media.play"}),
    };
    let event = repo::play_event::insert(&pool, &new_event)
        .await
        .expect("insert play_event")
        .expect("first insert must not be deduped");
    let deduped = repo::play_event::insert(&pool, &new_event)
        .await
        .expect("insert play_event (duplicate)");
    assert!(
        deduped.is_none(),
        "identical event delivery must dedup via the UNIQUE constraint, not double-insert"
    );

    let events_for_session = repo::play_event::list_for_session(&pool, &event.session_key.clone().unwrap())
        .await
        .expect("list events for session");
    assert_eq!(events_for_session.len(), 1);

    // --- play_sessions + play_session_media_info (Tautulli parity) ---
    let session = repo::play_session::upsert(
        &pool,
        &NewPlaySession {
            account_id: Some(account.id),
            media_item_id: Some(item.id),
            episode_id: None,
            session_key: Some(format!("session-{suffix}")),
            tautulli_ref_id: None,
            started_at,
            stopped_at: Some(started_at + chrono::Duration::minutes(100)),
            duration_ms: Some(110 * 60 * 1000),
            watched_ms: Some(100 * 60 * 1000),
            view_offset_ms: Some(100 * 60 * 1000),
            percent_complete: Some(0.91),
            paused_counter: 1,
            paused_ms: 30_000,
            is_finished: true,
            is_abandoned: false,
            player: Some("Living Room TV".to_string()),
            platform: Some("Plex for Android TV".to_string()),
            product: Some("Plex for Android (TV)".to_string()),
            device: Some("Chromecast".to_string()),
            ip_address: None,
            started_hour: Some(20),
            started_dow: Some(5),
            is_cinema_context: Some(true),
        },
    )
    .await
    .expect("upsert play_session");
    assert_eq!(session.account_id, Some(account.id));
    assert!(session.is_finished);

    repo::play_session::upsert_media_info(
        &pool,
        session.id,
        &NewPlaySessionMediaInfo {
            video_decision: Some(DecisionKind::DirectPlay),
            audio_decision: Some(DecisionKind::DirectPlay),
            transcode_decision: Some(DecisionKind::DirectPlay),
            container: Some("mkv".to_string()),
            video_codec: Some("hevc".to_string()),
            audio_codec: Some("eac3".to_string()),
            audio_channels: Some(6.0),
            video_resolution: Some("2160".to_string()),
            bitrate: Some(45_000),
            width: Some(3840),
            height: Some(2160),
            transcode_reason: None,
        },
    )
    .await
    .expect("upsert play_session_media_info");

    let media_info = repo::play_session::get_media_info(&pool, session.id)
        .await
        .expect("get media info")
        .expect("media info should exist");
    assert_eq!(media_info.video_decision, Some(DecisionKind::DirectPlay));

    let sessions_for_account = repo::play_session::list_for_account(&pool, account.id, 10)
        .await
        .expect("list sessions for account");
    assert!(sessions_for_account.iter().any(|s| s.id == session.id));

    // Cross-account isolation: the other account must see no sessions.
    let sessions_for_other = repo::play_session::list_for_account(&pool, other_account.id, 10)
        .await
        .expect("list sessions for other account");
    assert!(sessions_for_other.is_empty(), "telemetry must never blend across accounts");

    // --- watch_stats / ratings / watchlist ---
    let stats = repo::watch_stats::upsert_watch_stats(
        &pool,
        &crate::models::watch_stats::NewWatchStats {
            account_id: account.id,
            media_item_id: item.id,
            play_count: 1,
            finished_count: 1,
            rewatch_count: 0,
            total_watched_ms: 100 * 60 * 1000,
            avg_percent: Some(0.91),
            last_watched_at: Some(started_at),
            abandoned: false,
            first_watched_at: Some(started_at),
        },
    )
    .await
    .expect("upsert watch_stats");
    assert_eq!(stats.play_count, 1);

    repo::watch_stats::upsert_rating(&pool, account.id, item.id, 9.0, started_at)
        .await
        .expect("upsert rating");
    let ratings = repo::watch_stats::list_ratings_for_account(&pool, account.id)
        .await
        .expect("list ratings");
    assert_eq!(ratings.len(), 1);

    repo::watch_stats::add_to_watchlist(&pool, account.id, item.id, started_at)
        .await
        .expect("add to watchlist");
    repo::watch_stats::mark_fulfilled(&pool, account.id, item.id)
        .await
        .expect("mark watchlist entry fulfilled");
    let watchlist = repo::watch_stats::list_watchlist_for_account(&pool, account.id)
        .await
        .expect("list watchlist");
    assert_eq!(watchlist.len(), 1);
    assert!(watchlist[0].fulfilled);

    // --- embeddings (pgvector) ---
    let vector = vec![0.01_f32; 768];
    let embedding = repo::embedding::upsert(
        &pool,
        &NewEmbedding::nomic(
            EmbeddingEntityKind::MediaItem,
            item.id,
            vector.clone(),
            Some("Telemetry Test Movie".to_string()),
        ),
    )
    .await
    .expect("upsert embedding");
    assert_eq!(embedding.dim, 768);

    let fetched_embedding = repo::embedding::get(&pool, "media_item", item.id, "nomic-embed-text")
        .await
        .expect("get embedding")
        .expect("embedding should exist");
    assert_eq!(fetched_embedding.entity_id, item.id);

    let neighbors = repo::embedding::nearest(
        &pool,
        "media_item",
        "nomic-embed-text",
        &pgvector::Vector::from(vector),
        5,
    )
    .await
    .expect("nearest-neighbor query");
    assert!(neighbors.iter().any(|m| m.entity_id == item.id));
    assert!(neighbors[0].distance >= 0.0);

    // --- taste_profile / taste_context_centroids / taste_signals ---
    let profile = repo::taste::upsert_profile(
        &pool,
        &NewTasteProfile {
            account_id: account.id,
            genre_affinity: serde_json::json!({"sci-fi": 0.8}),
            person_affinity: serde_json::json!({}),
            keyword_affinity: serde_json::json!({"slow-burn": 0.6}),
            runtime_pref: None,
            quality_sensitivity: None,
            overall_centroid: Some(pgvector::Vector::from(vec![0.02_f32; 768])),
            model_notes: Some("Loves cerebral, slow-burn sci-fi.".to_string()),
        },
    )
    .await
    .expect("upsert taste_profile");
    assert_eq!(profile.account_id, account.id);

    repo::taste::upsert_context_centroid(
        &pool,
        &NewTasteContextCentroid {
            account_id: account.id,
            context_key: "weekend_evening".to_string(),
            centroid: pgvector::Vector::from(vec![0.03_f32; 768]),
            sample_size: 12,
        },
    )
    .await
    .expect("upsert taste_context_centroid");
    let centroids = repo::taste::list_context_centroids(&pool, account.id)
        .await
        .expect("list context centroids");
    assert_eq!(centroids.len(), 1);

    repo::taste::record_signal(
        &pool,
        &NewTasteSignal {
            account_id: account.id,
            media_item_id: Some(item.id),
            signal_type: "finished".to_string(),
            weight: 1.0,
            context_key: Some("weekend_evening".to_string()),
            note: None,
        },
    )
    .await
    .expect("record taste_signal");
    let signals = repo::taste::list_signals_for_media_item(&pool, account.id, item.id)
        .await
        .expect("list signals for media item");
    assert_eq!(signals.len(), 1);
    assert_eq!(signals[0].signal_type, "finished");

    // --- proactive_items ---
    let proactive = repo::proactive_item::create(
        &pool,
        &NewProactiveItem {
            account_id: Some(account.id),
            kind: "finish_nudge".to_string(),
            media_item_id: Some(item.id),
            headline: "You just finished Telemetry Test Movie -- want something similar?".to_string(),
            body: Some(serde_json::json!({"rationale": "cosine match to overall_centroid"})),
            priority: 5,
            earliest_at: None,
            expires_at: None,
        },
    )
    .await
    .expect("create proactive_item");

    let pending = repo::proactive_item::list_pending_for_account(&pool, account.id, chrono::Utc::now())
        .await
        .expect("list pending proactive_items");
    assert!(pending.iter().any(|p| p.id == proactive.id));

    repo::proactive_item::mark_delivered(&pool, proactive.id, chrono::Utc::now())
        .await
        .expect("mark proactive_item delivered");
    let pending_after = repo::proactive_item::list_pending_for_account(&pool, account.id, chrono::Utc::now())
        .await
        .expect("list pending proactive_items after delivery");
    assert!(!pending_after.iter().any(|p| p.id == proactive.id));

    // --- external_enrichment ---
    repo::external_enrichment::upsert(
        &pool,
        &NewExternalEnrichment {
            media_item_id: item.id,
            kind: "forum_sentiment".to_string(),
            source: "reddit".to_string(),
            payload: serde_json::json!({"score": 0.7, "summary": "generally positive"}),
            confidence: Some(0.6),
            ttl_seconds: 604_800,
        },
    )
    .await
    .expect("upsert external_enrichment");
    let enrichment = repo::external_enrichment::list_for_media_item(&pool, item.id)
        .await
        .expect("list external_enrichment");
    assert_eq!(enrichment.len(), 1);
    assert_eq!(enrichment[0].kind, "forum_sentiment");

    // An entry fetched "now" with a 0-second TTL should show up as expired
    // immediately -- exercises the refresh-worker query path.
    repo::external_enrichment::upsert(
        &pool,
        &NewExternalEnrichment {
            media_item_id: item.id,
            kind: "critic_score".to_string(),
            source: "metacritic".to_string(),
            payload: serde_json::json!({"score": 82}),
            confidence: Some(0.9),
            ttl_seconds: 0,
        },
    )
    .await
    .expect("upsert already-expired external_enrichment");
    let expired = repo::external_enrichment::list_expired(&pool, chrono::Utc::now() + chrono::Duration::seconds(1))
        .await
        .expect("list expired external_enrichment");
    assert!(expired.iter().any(|e| e.kind == "critic_score"));

    // --- TV path: episode-level session, to exercise episode_id (not just media_item_id) ---
    let tv_library = repo::library::create(
        &pool,
        &NewLibrary {
            name: format!("sonarr_muse03_{suffix}"),
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
            tvdb_id: Some(format!("tvdb-muse03-{suffix}")),
            imdb_id: None,
            provider_ids: serde_json::json!({}),
            title: "Telemetry Test Show".to_string(),
            sort_title: None,
            original_title: None,
            original_language: Some("en".to_string()),
            status: Some("continuing".to_string()),
            overview: None,
            studio: None,
            network: None,
            runtime_minutes: Some(30),
            year: Some(2023),
            images: serde_json::json!([]),
        },
    )
    .await
    .expect("upsert show media_metadata");

    let show_item = repo::media_item::upsert(
        &pool,
        &NewMediaItem {
            library_id: tv_library.id,
            media_metadata_id: show_metadata.id,
            path: "/media/TV/Telemetry Test Show".to_string(),
            monitored: true,
            quality_profile_id: None,
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

    let episode = repo::episode::upsert(
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
    .expect("upsert episode");

    let episode_session = repo::play_session::upsert(
        &pool,
        &NewPlaySession {
            account_id: Some(account.id),
            media_item_id: Some(show_item.id),
            episode_id: Some(episode.id),
            session_key: Some(format!("session-ep-{suffix}")),
            tautulli_ref_id: None,
            started_at: started_at + chrono::Duration::hours(1),
            stopped_at: Some(started_at + chrono::Duration::hours(1) + chrono::Duration::minutes(28)),
            duration_ms: Some(30 * 60 * 1000),
            watched_ms: Some(28 * 60 * 1000),
            view_offset_ms: Some(28 * 60 * 1000),
            percent_complete: Some(0.93),
            paused_counter: 0,
            paused_ms: 0,
            is_finished: true,
            is_abandoned: false,
            player: None,
            platform: None,
            product: None,
            device: None,
            ip_address: None,
            started_hour: Some(21),
            started_dow: Some(5),
            is_cinema_context: Some(true),
        },
    )
    .await
    .expect("upsert episode-level play_session");
    assert_eq!(episode_session.episode_id, Some(episode.id));

    let episode_sessions = repo::play_session::list_for_media_item(&pool, show_item.id)
        .await
        .expect("list sessions for show media_item");
    assert!(episode_sessions.iter().any(|s| s.id == episode_session.id));
}

/// MUSE-16: Prowlarr availability layer — indexers / releases / availability
/// rollup round-trip against a live Postgres. Self-contained (creates its own
/// movie metadata) so it does not depend on the core round-trip's fixtures.
#[tokio::test]
async fn availability_schema_round_trips() {
    let Ok(database_url) = std::env::var("MUSE_TEST_DATABASE_URL") else {
        eprintln!(
            "MUSE_TEST_DATABASE_URL not set — skipping availability_schema_round_trips \
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

    // The title a resolved release points at.
    let metadata = repo::media_metadata::upsert_by_tmdb(
        &pool,
        &NewMediaMetadata {
            kind: MediaKind::Movie,
            tmdb_id: Some(format!("tmdb-avail-{suffix}")),
            tvdb_id: None,
            imdb_id: None,
            provider_ids: serde_json::json!({}),
            title: "Test Movie".to_string(),
            sort_title: None,
            original_title: None,
            original_language: None,
            status: None,
            overview: None,
            studio: None,
            network: None,
            runtime_minutes: None,
            year: Some(2020),
            images: serde_json::json!([]),
        },
    )
    .await
    .expect("upsert movie metadata");

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

    // A title with zero releases rolls up to an empty availability row.
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
