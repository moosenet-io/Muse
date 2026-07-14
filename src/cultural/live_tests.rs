//! MUSEX-07 (Plane TERM #383) live-DB round-trip: seeds a real account,
//! persona, and an owned/embedded library title, then exercises the actual
//! `crate::cultural::the_talk_surface` / `cold_start_recommendations`
//! orchestration end-to-end against Postgres, with a [`MockTrendSource`]
//! standing in for TMDb/Trakt (see `crate::cultural::source`'s module doc —
//! this crate never makes a live external call in tests).
//!
//! Gated on `MUSE_TEST_DATABASE_URL`, identical skip-when-unset posture as
//! `curation::live_tests`/`recall::live_tests` and every other live-DB test
//! in this crate.
//!
//! The load-bearing assertion here is [`orchestration_never_forwards_account_data_to_the_trend_source`]:
//! it seeds an account and persona with deliberately PII-SHAPED
//! values (a distinctive username, a persona name embedding the account
//! id) and asserts that after running the real orchestration functions,
//! NONE of the `MockTrendSource`'s recorded, serialized requests contain
//! those values — proving (not just type-asserting) that account-scoped
//! data never crosses the `TrendSource` boundary.

use sqlx::postgres::PgPoolOptions;
use uuid::Uuid;

use crate::cultural::cache::TrendCache;
use crate::cultural::source::{MockTrendSource, TrendEntry};
use crate::cultural::{cold_start_recommendations, the_talk_surface};
use crate::models::account::NewAccount;
use crate::models::library::{LibraryKind, NewLibrary};
use crate::models::media_item::NewMediaItem;
use crate::models::media_metadata::{MediaKind, NewMediaMetadata};
use crate::models::persona::{NewPersona, PERSONA_KIND_EXPLICIT};
use crate::repo;

/// Connect + migrate, or print the standard skip message and return `None`
/// — same idiom as every other `live_tests` module in this crate.
async fn pool_or_skip(test_name: &str) -> Option<sqlx::PgPool> {
    let database_url = std::env::var("MUSE_TEST_DATABASE_URL").ok()?;
    if database_url.is_empty() {
        eprintln!("MUSE_TEST_DATABASE_URL not set — skipping {test_name} (expected in the default test run)");
        return None;
    }

    let pool = PgPoolOptions::new()
        .max_connections(5)
        .connect(&database_url)
        .await
        .expect("connect to MUSE_TEST_DATABASE_URL");

    sqlx::migrate!("./migrations")
        .run(&pool)
        .await
        .expect("migrations should apply cleanly");

    Some(pool)
}

#[tokio::test]
async fn orchestration_never_forwards_account_data_to_the_trend_source() {
    let Some(pool) =
        pool_or_skip("orchestration_never_forwards_account_data_to_the_trend_source").await
    else {
        return;
    };

    let suffix = Uuid::new_v4().simple().to_string();

    // Deliberately PII-shaped seed values: a distinctive username, and a
    // persona name that embeds the account id -- if either ever showed up
    // in a serialized outbound TrendQuery/TalkQuery, that would be a real
    // sovereignty leak, not a hypothetical one.
    let username = format!("secret_musex07_account_{suffix}");
    let account = repo::account::create(
        &pool,
        &NewAccount {
            plex_account_id: None,
            username: Some(username.clone()),
            friendly_name: Some("MUSEX-07 PII Probe Account".to_string()),
            is_home_user: true,
            is_primary: false,
        },
    )
    .await
    .expect("create test account");

    let persona_name = format!("do-not-leak-persona-for-account-{}", account.id);
    repo::persona::upsert_for_account(
        &pool,
        &NewPersona {
            account_id: Some(account.id),
            name: persona_name.clone(),
            kind: PERSONA_KIND_EXPLICIT.to_string(),
            centroid: pgvector::Vector::from(vec![0.1_f32; 768]),
            defining_signals: serde_json::json!({}),
            metadata: serde_json::json!({}),
            sample_size: 1,
        },
    )
    .await
    .expect("seed persona");

    // One owned title Muse has metadata + a library instance for, so the
    // trending∩library intersection has something real to resolve against.
    let tmdb_id = format!("musex07-{suffix}");
    let library = repo::library::create(
        &pool,
        &NewLibrary {
            name: format!("musex07_lib_{suffix}"),
            kind: LibraryKind::Tv,
            root_folder: "/media/TV/".to_string(),
            source_arr_name: Some("sonarr".to_string()),
            source_arr_url: None,
        },
    )
    .await
    .expect("create library");

    let meta = repo::media_metadata::upsert_by_tmdb(
        &pool,
        &NewMediaMetadata {
            kind: MediaKind::Show,
            tmdb_id: Some(tmdb_id.clone()),
            tvdb_id: None,
            imdb_id: None,
            provider_ids: serde_json::json!({}),
            title: "MUSEX-07 Owned Trending Show".to_string(),
            sort_title: None,
            original_title: None,
            original_language: Some("en".to_string()),
            status: Some("Continuing".to_string()),
            overview: None,
            studio: None,
            network: None,
            runtime_minutes: Some(45),
            year: Some(2024),
            images: serde_json::json!([]),
        },
    )
    .await
    .expect("upsert owned show metadata");

    repo::media_item::upsert(
        &pool,
        &NewMediaItem {
            library_id: library.id,
            media_metadata_id: meta.id,
            path: format!("/media/TV/MUSEX-07 Owned Trending Show {suffix}"),
            monitored: true,
            quality_profile_id: None,
            minimum_availability: None,
            plex_rating_key: None,
            added_at: None,
        },
    )
    .await
    .expect("upsert owned show item");

    let mock = MockTrendSource::new(
        vec![TrendEntry {
            external_id: tmdb_id.clone(),
            kind: MediaKind::Show,
            title: "MUSEX-07 Owned Trending Show".to_string(),
            year: Some(2024),
            popularity: 88.0,
        }],
        vec![],
    );
    let cache = TrendCache::new(std::time::Duration::from_secs(60));

    // --- exercise the real orchestration ------------------------------
    let picks = the_talk_surface(&pool, account.id, &mock, &cache, "US")
        .await
        .expect("the_talk_surface should not error");
    assert_eq!(
        picks.len(),
        1,
        "the seeded owned+trending title should surface"
    );
    assert_eq!(picks[0].media_metadata_id, meta.id);

    let cold_start = cold_start_recommendations(&pool, account.id, &mock, &cache, "US")
        .await
        .expect("cold_start_recommendations should not error");
    assert!(
        !cold_start.is_empty(),
        "cold-start must return at least the seeded trend entry"
    );

    // --- the load-bearing sovereignty assertion ------------------------
    let recorded_trending = mock.trending_calls.lock().unwrap();
    let recorded_talk = mock.talk_calls.lock().unwrap();
    assert!(
        !recorded_trending.is_empty(),
        "the orchestration must have actually called TrendSource::trending"
    );

    for query in recorded_trending.iter() {
        let json = serde_json::to_string(query).expect("TrendQuery serializes");
        assert!(
            !json.contains(&username),
            "TrendQuery leaked the account's username"
        );
        assert!(
            !json.contains(&persona_name),
            "TrendQuery leaked the persona name"
        );
        assert!(
            !json.contains(&account.id.to_string()),
            "TrendQuery leaked the account id"
        );
    }
    for query in recorded_talk.iter() {
        let json = serde_json::to_string(query).expect("TalkQuery serializes");
        assert!(
            !json.contains(&username),
            "TalkQuery leaked the account's username"
        );
        assert!(
            !json.contains(&persona_name),
            "TalkQuery leaked the persona name"
        );
        assert!(
            !json.contains(&account.id.to_string()),
            "TalkQuery leaked the account id"
        );
    }
}
