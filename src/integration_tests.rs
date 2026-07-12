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
use crate::models::embedding::{EmbeddingEntityKind, NewEmbedding};
use crate::models::episode::NewEpisode;
use crate::models::external_enrichment::NewExternalEnrichment;
use crate::models::library::{LibraryKind, NewLibrary};
use crate::models::media_file::{NewMediaFile, ReleaseTypeKind, Revision};
use crate::models::media_item::NewMediaItem;
use crate::models::media_metadata::{MediaKind, NewMediaMetadata};
use crate::models::play_event::NewPlayEvent;
use crate::models::play_session::{DecisionKind, NewPlaySession, NewPlaySessionMediaInfo};
use crate::models::proactive_item::NewProactiveItem;
use crate::models::quality::{NewCustomFormat, NewQualityDefinition, NewQualityProfile};
use crate::models::season::NewSeason;
use crate::models::taste::{NewTasteContextCentroid, NewTasteProfile, NewTasteSignal};
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
