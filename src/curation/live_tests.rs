//! MUSE-11 live-DB round-trip: seed one account with an on-deck movie and an
//! engaged-with, still-airing show, then exercise the real candidate-gather
//! → dedup → rank → rationale pipeline end-to-end against Postgres.
//!
//! Gated on `MUSE_TEST_DATABASE_URL`, identical skip-when-unset posture as
//! `src/integration_tests.rs` and every other live-DB test in this crate.
//! Every assertion is scoped to this test's own seeded account/items (a
//! unique suffix per run) — the shared `muse_test` database accumulates rows
//! across test runs, so nothing here asserts over an unscoped/global query.

use sqlx::postgres::PgPoolOptions;
use uuid::Uuid;

use crate::curation::candidates;
use crate::curation::recommend::{build_rationale, rank_candidates};
use crate::models::account::NewAccount;
use crate::models::library::{LibraryKind, NewLibrary};
use crate::models::media_item::NewMediaItem;
use crate::models::media_metadata::{MediaKind, NewMediaMetadata};
use crate::models::watch_stats::NewWatchStats;
use crate::repo;

#[tokio::test]
async fn muse11_recommend_round_trip_on_deck_and_gap() {
    let Ok(database_url) = std::env::var("MUSE_TEST_DATABASE_URL") else {
        eprintln!(
            "MUSE_TEST_DATABASE_URL not set — skipping muse11_recommend_round_trip_on_deck_and_gap \
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
        .expect("migrations should apply cleanly");

    let suffix = Uuid::new_v4().simple().to_string();
    let now = chrono::Utc::now();

    let account = repo::account::create(
        &pool,
        &NewAccount {
            plex_account_id: None,
            username: Some(format!("muse11_test_{suffix}")),
            friendly_name: Some("MUSE-11 Test Account".to_string()),
            is_home_user: true,
            is_primary: false,
        },
    )
    .await
    .expect("create test account");

    // --- on-deck fixture: a movie the account is partway through ----------
    let movie_library = repo::library::create(
        &pool,
        &NewLibrary {
            name: format!("muse11_movies_{suffix}"),
            kind: LibraryKind::Movie,
            root_folder: "/media/Movies/".to_string(),
            source_arr_name: Some("radarr".to_string()),
            source_arr_url: None,
        },
    )
    .await
    .expect("create movie library");

    let movie_meta = repo::media_metadata::upsert_by_tmdb(
        &pool,
        &NewMediaMetadata {
            kind: MediaKind::Movie,
            tmdb_id: Some(format!("tmdb-muse11-ondeck-{suffix}")),
            tvdb_id: None,
            imdb_id: None,
            provider_ids: serde_json::json!({}),
            title: format!("MUSE-11 On-Deck Movie {suffix}"),
            sort_title: None,
            original_title: None,
            original_language: Some("en".to_string()),
            status: Some("released".to_string()),
            overview: Some("A movie you started but haven't finished.".to_string()),
            studio: None,
            network: None,
            runtime_minutes: Some(110),
            year: Some(2023),
            images: serde_json::json!([]),
        },
    )
    .await
    .expect("upsert on-deck movie metadata");

    let movie_item = repo::media_item::upsert(
        &pool,
        &NewMediaItem {
            library_id: movie_library.id,
            media_metadata_id: movie_meta.id,
            path: format!("/media/Movies/MUSE-11 On-Deck Movie {suffix}"),
            monitored: true,
            quality_profile_id: None,
            minimum_availability: None,
            plex_rating_key: None,
            added_at: None,
        },
    )
    .await
    .expect("upsert on-deck movie item");

    repo::watch_stats::upsert_watch_stats(
        &pool,
        &NewWatchStats {
            account_id: account.id,
            media_item_id: movie_item.id,
            play_count: 1,
            finished_count: 0,
            rewatch_count: 0,
            total_watched_ms: 40 * 60 * 1000,
            // Deliberately high (but still on-deck, below the 95%
            // "essentially finished" cutoff `list_on_deck` applies) so this
            // fixture's on-deck score (source_weight 1.0 * 0.90 = 0.90)
            // provably outranks the gap fixture below (0.85 * 0.70 = 0.595)
            // — see the `on_deck_rank < gap_rank` assertion.
            avg_percent: Some(90.0),
            last_watched_at: Some(now),
            abandoned: false,
            first_watched_at: Some(now),
        },
    )
    .await
    .expect("upsert on-deck watch_stats");

    // --- gap fixture: a still-airing show the account has finished at least
    // one tracked watch of -----------------------------------------------
    let show_library = repo::library::create(
        &pool,
        &NewLibrary {
            name: format!("muse11_shows_{suffix}"),
            kind: LibraryKind::Tv,
            root_folder: "/media/TV/".to_string(),
            source_arr_name: Some("sonarr".to_string()),
            source_arr_url: None,
        },
    )
    .await
    .expect("create show library");

    let show_meta = repo::media_metadata::upsert_by_tvdb(
        &pool,
        &NewMediaMetadata {
            kind: MediaKind::Show,
            tmdb_id: None,
            tvdb_id: Some(format!("tvdb-muse11-gap-{suffix}")),
            imdb_id: None,
            provider_ids: serde_json::json!({}),
            title: format!("MUSE-11 Gap Show {suffix}"),
            sort_title: None,
            original_title: None,
            original_language: Some("en".to_string()),
            status: Some("Continuing".to_string()),
            overview: Some("A show that's still airing.".to_string()),
            studio: None,
            network: None,
            runtime_minutes: Some(45),
            year: Some(2021),
            images: serde_json::json!([]),
        },
    )
    .await
    .expect("upsert gap show metadata");

    let show_item = repo::media_item::upsert(
        &pool,
        &NewMediaItem {
            library_id: show_library.id,
            media_metadata_id: show_meta.id,
            path: format!("/media/TV/MUSE-11 Gap Show {suffix}"),
            monitored: true,
            quality_profile_id: None,
            minimum_availability: None,
            plex_rating_key: None,
            added_at: None,
        },
    )
    .await
    .expect("upsert gap show item");

    repo::watch_stats::upsert_watch_stats(
        &pool,
        &NewWatchStats {
            account_id: account.id,
            media_item_id: show_item.id,
            play_count: 12,
            finished_count: 12,
            rewatch_count: 0,
            total_watched_ms: 12 * 45 * 60 * 1000,
            // Lower than the on-deck fixture's percent on purpose (see the
            // comment above it) so the "on-deck outranks gap" assertion
            // below is provably about source-tier weighting, not an
            // accident of higher taste_fit on the gap side.
            avg_percent: Some(70.0),
            last_watched_at: Some(now - chrono::Duration::hours(1)),
            abandoned: false,
            first_watched_at: Some(now - chrono::Duration::days(30)),
        },
    )
    .await
    .expect("upsert gap watch_stats");

    // --- exercise the real pipeline: gather -> dedup -> rank -> rationale --
    let on_deck = candidates::gather_on_deck_candidates(&pool, account.id, 1_000)
        .await
        .expect("gather_on_deck_candidates should not error");
    let gap = candidates::gather_gap_candidates(&pool, account.id, 1_000)
        .await
        .expect("gather_gap_candidates should not error");

    let on_deck_hit = on_deck
        .iter()
        .find(|c| c.media_metadata_id == movie_meta.id)
        .expect("the seeded on-deck movie must appear in gather_on_deck_candidates");
    assert!(
        on_deck_hit.facts.iter().any(|f| f.contains("90%")),
        "on-deck candidate must ground its facts in the real avg_percent: {:?}",
        on_deck_hit.facts
    );

    let gap_hit = gap
        .iter()
        .find(|c| c.media_metadata_id == show_meta.id)
        .expect("the seeded still-airing show must appear in gather_gap_candidates");
    assert!(
        gap_hit
            .facts
            .iter()
            .any(|f| f.to_lowercase().contains("continuing")),
        "gap candidate must ground its facts in the real status: {:?}",
        gap_hit.facts
    );

    let deduped = candidates::dedup_candidates(vec![on_deck_hit.clone(), gap_hit.clone()]);
    assert_eq!(
        deduped.len(),
        2,
        "two distinct titles must not be collapsed by dedup"
    );

    let ranked = rank_candidates(deduped);
    let on_deck_rank = ranked
        .iter()
        .position(|(c, _)| c.media_metadata_id == movie_meta.id)
        .expect("on-deck candidate present in ranked output");
    let gap_rank = ranked
        .iter()
        .position(|(c, _)| c.media_metadata_id == show_meta.id)
        .expect("gap candidate present in ranked output");
    assert!(
        on_deck_rank < gap_rank,
        "on-deck (source_weight 1.0 * 0.90 taste_fit = 0.90) must outrank gap \
         (source_weight 0.85 * 0.70 taste_fit = 0.595): on_deck_rank={on_deck_rank} gap_rank={gap_rank}"
    );

    for (candidate, _score) in &ranked {
        let rationale = build_rationale(None, candidate).await;
        assert!(
            rationale.contains(&candidate.title),
            "rationale must mention the actual title: {rationale}"
        );
        for fact in &candidate.facts {
            assert!(
                rationale.contains(fact.as_str()),
                "the no-chord-configured rationale must be exactly the fact-grounded template: \
                 fact {fact:?} missing from rationale {rationale:?}"
            );
        }
    }
}
