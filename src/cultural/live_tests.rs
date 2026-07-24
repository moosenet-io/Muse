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
use crate::cultural::{
    cold_start_recommendations, recommend_cultural, the_talk_surface, CulturalRecommendations,
};
use crate::models::account::NewAccount;
use crate::models::embedding::{EmbeddingEntityKind, NewEmbedding};
use crate::models::library::{LibraryKind, NewLibrary};
use crate::models::media_item::NewMediaItem;
use crate::models::media_metadata::{MediaKind, NewMediaMetadata};
use crate::models::persona::{NewPersona, PERSONA_KIND_EXPLICIT};
use crate::models::taste::NewTasteProfile;
use crate::repo;

/// Seed a rich (non-sparse) `taste_profile` for `account_id`: ≥3
/// genre-affinity entries + a real centroid, so `is_profile_sparse` returns
/// false and the taste-intersection path (not cold-start) runs. `centroid`
/// is also what each owned title's embedding is compared against.
async fn seed_rich_taste_profile(pool: &sqlx::PgPool, account_id: i64, centroid: Vec<f32>) {
    repo::taste::upsert_profile(
        pool,
        &NewTasteProfile {
            account_id,
            genre_affinity: serde_json::json!({"drama": 0.9, "comedy": 0.6, "scifi": 0.4}),
            person_affinity: serde_json::json!({}),
            keyword_affinity: serde_json::json!({}),
            runtime_pref: None,
            quality_sensitivity: None,
            overall_centroid: Some(pgvector::Vector::from(centroid)),
            model_notes: None,
        },
    )
    .await
    .expect("seed rich taste profile");
}

/// Seed the `nomic-embed-text` embedding for a media item, so the
/// taste-intersection can compute a real cosine `taste_fit` for it.
async fn seed_item_embedding(pool: &sqlx::PgPool, media_item_id: i64, embedding: Vec<f32>) {
    repo::embedding::upsert(
        pool,
        &NewEmbedding::qwen3(
            EmbeddingEntityKind::MediaItem,
            media_item_id,
            embedding,
            None,
        ),
    )
    .await
    .expect("seed item embedding");
}

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
            centroid: pgvector::Vector::from(vec![0.1_f32; 1024]),
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

    let item = repo::media_item::upsert(
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

    // Rich taste profile + a matching item embedding so the owned+trending
    // title clears the taste-intersection's TASTE_RELEVANCE_MIN filter
    // (identical vectors -> cosine 1.0) and actually surfaces.
    seed_rich_taste_profile(&pool, account.id, vec![0.1_f32; 1024]).await;
    seed_item_embedding(&pool, item.id, vec![0.1_f32; 1024]).await;

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

/// Finding #3: proves `is_profile_sparse` is actually WIRED into the
/// cultural-layer entry point (`recommend_cultural`), not dead code — a
/// SPARSE-profile account routes to the cold-start fallback, while a RICH
/// one routes to the taste-filtered trending∩library∩taste intersection.
/// Same `MockTrendSource` (no live external call) for both.
#[tokio::test]
async fn sparse_profile_routes_to_cold_start_while_rich_profile_uses_the_intersection() {
    let Some(pool) = pool_or_skip(
        "sparse_profile_routes_to_cold_start_while_rich_profile_uses_the_intersection",
    )
    .await
    else {
        return;
    };

    let suffix = Uuid::new_v4().simple().to_string();
    let tmdb_id = format!("musex07-route-{suffix}");

    // One owned+embedded title both accounts could match against.
    let library = repo::library::create(
        &pool,
        &NewLibrary {
            name: format!("musex07_route_lib_{suffix}"),
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
            title: "MUSEX-07 Routing Show".to_string(),
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
    .expect("upsert routing show metadata");

    let item = repo::media_item::upsert(
        &pool,
        &NewMediaItem {
            library_id: library.id,
            media_metadata_id: meta.id,
            path: format!("/media/TV/MUSEX-07 Routing Show {suffix}"),
            monitored: true,
            quality_profile_id: None,
            minimum_availability: None,
            plex_rating_key: None,
            added_at: None,
        },
    )
    .await
    .expect("upsert routing show item");
    seed_item_embedding(&pool, item.id, vec![0.1_f32; 1024]).await;

    let mock = MockTrendSource::new(
        vec![TrendEntry {
            external_id: tmdb_id.clone(),
            kind: MediaKind::Show,
            title: "MUSEX-07 Routing Show".to_string(),
            year: Some(2024),
            popularity: 77.0,
        }],
        vec![],
    );
    let cache = TrendCache::new(std::time::Duration::from_secs(60));

    // --- SPARSE account: no taste_profile at all -> cold-start path -----
    let sparse_account = repo::account::create(
        &pool,
        &NewAccount {
            plex_account_id: None,
            username: Some(format!("musex07_sparse_{suffix}")),
            friendly_name: Some("MUSEX-07 Sparse".to_string()),
            is_home_user: true,
            is_primary: false,
        },
    )
    .await
    .expect("create sparse account");

    let sparse_result = recommend_cultural(&pool, sparse_account.id, &mock, &cache, "US")
        .await
        .expect("recommend_cultural (sparse) should not error");
    match sparse_result {
        CulturalRecommendations::ColdStart(picks) => {
            assert!(
                picks.iter().any(|p| p.entry.external_id == tmdb_id),
                "the sparse account's cold-start result should include the trending entry"
            );
        }
        CulturalRecommendations::TasteIntersection(_) => {
            panic!(
                "a sparse profile must route to the cold-start path, not the taste intersection"
            );
        }
    }

    // --- RICH account: full taste_profile -> taste-intersection path ----
    let rich_account = repo::account::create(
        &pool,
        &NewAccount {
            plex_account_id: None,
            username: Some(format!("musex07_rich_{suffix}")),
            friendly_name: Some("MUSEX-07 Rich".to_string()),
            is_home_user: true,
            is_primary: false,
        },
    )
    .await
    .expect("create rich account");
    seed_rich_taste_profile(&pool, rich_account.id, vec![0.1_f32; 1024]).await;

    let rich_result = recommend_cultural(&pool, rich_account.id, &mock, &cache, "US")
        .await
        .expect("recommend_cultural (rich) should not error");
    match rich_result {
        CulturalRecommendations::TasteIntersection(picks) => {
            assert!(
                picks.iter().any(|p| p.media_metadata_id == meta.id),
                "the rich account's intersection should surface the owned+embedded+taste-relevant title"
            );
        }
        CulturalRecommendations::ColdStart(_) => {
            panic!("a rich profile must route to the taste intersection, not cold-start");
        }
    }
}
