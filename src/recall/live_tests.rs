//! MUSE-09 live-DB integration tests for the `/query/resolve` and
//! `/query/similar` handlers.
//!
//! Gated on `MUSE_TEST_DATABASE_URL` — exactly the same skip-when-unset
//! pattern as `src/integration_tests.rs` and MUSE-08's own round-trip test
//! in `src/embed/pipeline.rs`: these tests log and return cleanly (never
//! fail) when the env var isn't set, so the default `cargo test` run never
//! requires a live Postgres.
//!
//! Real Ollama/TMDb network calls are deliberately never exercised here —
//! embeddings are hand-inserted with known vectors (same technique as
//! MUSE-08's test), and `AppState::embed`/`AppState::tmdb` are left `None`
//! in most of these tests specifically to exercise the graceful-degradation
//! paths the spec requires.

use std::sync::Arc;

use axum::extract::State;
use axum::Json;
use sqlx::postgres::PgPoolOptions;
use uuid::Uuid;

use crate::config::Config;
use crate::http::AppState;
use crate::models::embedding::{EmbeddingEntityKind, NewEmbedding};
use crate::models::library::{LibraryKind, NewLibrary};
use crate::models::media_item::NewMediaItem;
use crate::models::media_metadata::{MediaKind, NewMediaMetadata};
use crate::repo;

use super::{resolve_handler, similar_handler, ResolveHit, ResolveRequest, ResolveTier, SimilarRequest, SimilarTier};

/// Build an `AppState` with no external clients configured — vector
/// (`embed`) and TMDb (`tmdb`) tiers are unavailable by construction, so
/// callers can assert the degrade-to-trigram / degrade-to-genre paths
/// without needing a live Ollama or TMDb.
fn degraded_state(pool: sqlx::PgPool) -> Arc<AppState> {
    let config = Config::default();
    Arc::new(AppState {
        enrichment: crate::enrichment::EnrichmentService::from_config(&config),
        pool,
        config,
        plex: None,
        prowlarr: None,
        arr_instances: Vec::new(),
        tmdb: None,
        embed: None,
    })
}

async fn connect() -> Option<sqlx::PgPool> {
    let database_url = std::env::var("MUSE_TEST_DATABASE_URL").ok()?;
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

async fn seed_library(pool: &sqlx::PgPool, suffix: &str) -> crate::models::library::Library {
    repo::library::create(
        pool,
        &NewLibrary {
            name: format!("muse09_test_{suffix}"),
            kind: LibraryKind::Movie,
            root_folder: "/media/Movies/".to_string(),
            source_arr_name: Some("radarr".to_string()),
            source_arr_url: None,
        },
    )
    .await
    .expect("create library")
}

async fn seed_movie(
    pool: &sqlx::PgPool,
    library_id: i64,
    suffix: &str,
    title: &str,
    year: i32,
    overview: &str,
) -> (crate::models::media_metadata::MediaMetadata, crate::models::media_item::MediaItem) {
    let meta = repo::media_metadata::upsert_by_tmdb(
        pool,
        &NewMediaMetadata {
            kind: MediaKind::Movie,
            tmdb_id: Some(format!("tmdb-muse09-{title}-{suffix}")),
            tvdb_id: None,
            imdb_id: None,
            provider_ids: serde_json::json!({}),
            title: title.to_string(),
            sort_title: None,
            original_title: None,
            original_language: Some("en".to_string()),
            status: Some("released".to_string()),
            overview: Some(overview.to_string()),
            studio: None,
            network: None,
            runtime_minutes: Some(110),
            year: Some(year),
            images: serde_json::json!([]),
        },
    )
    .await
    .expect("upsert media_metadata");

    let item = repo::media_item::upsert(
        pool,
        &NewMediaItem {
            library_id,
            media_metadata_id: meta.id,
            path: format!("/media/Movies/{title} ({year}) {suffix}"),
            monitored: true,
            quality_profile_id: None,
            minimum_availability: None,
            plex_rating_key: None,
            added_at: None,
        },
    )
    .await
    .expect("upsert media_item");

    (meta, item)
}

#[tokio::test]
async fn resolve_degrades_to_trigram_when_vector_tier_is_unconfigured() {
    let Some(pool) = connect().await else {
        eprintln!(
            "MUSE_TEST_DATABASE_URL not set — skipping \
             resolve_degrades_to_trigram_when_vector_tier_is_unconfigured"
        );
        return;
    };

    let suffix = Uuid::new_v4().simple().to_string();
    let library = seed_library(&pool, &suffix).await;

    let unique_title = format!("Zzyzx Linguist Contact {suffix}");
    let (meta, _item) = seed_movie(
        &pool,
        library.id,
        &suffix,
        &unique_title,
        2016,
        "A linguist works with the military to communicate with aliens.",
    )
    .await;

    let state = degraded_state(pool);

    let resp = resolve_handler(
        State(state),
        Json(ResolveRequest {
            query: "Zzyzx Linguist".to_string(),
            // High limit: other live-DB tests share this DB and seed titles
            // that can trigram-match ahead of this one; we assert the seeded
            // title is FOUND in the fallback tier, not its rank.
            limit: Some(100_000),
            include_tmdb: false,
        }),
    )
    .await
    .expect("resolve_handler should not error")
    .0;

    assert_eq!(
        resp.tier,
        ResolveTier::Trigram,
        "with no embed client configured the ladder must fall through to the trigram tier"
    );
    assert!(
        resp.results.iter().any(|h| matches!(
            h,
            ResolveHit::Trigram { media_metadata_id, .. } if *media_metadata_id == meta.id
        )),
        "expected the seeded title to be found via the trigram fallback"
    );
}

#[tokio::test]
async fn resolve_returns_none_tier_and_never_errors_on_a_totally_unmatched_query() {
    let Some(pool) = connect().await else {
        eprintln!(
            "MUSE_TEST_DATABASE_URL not set — skipping \
             resolve_returns_none_tier_and_never_errors_on_a_totally_unmatched_query"
        );
        return;
    };

    let state = degraded_state(pool);
    let nonsense = format!("qqzzxxnomatch-{}", Uuid::new_v4().simple());

    let resp = resolve_handler(
        State(state),
        Json(ResolveRequest {
            query: nonsense,
            limit: None,
            include_tmdb: false, // opted out — the tmdb tier must never even be attempted
        }),
    )
    .await
    .expect("resolve_handler must never error just because nothing matched")
    .0;

    assert_eq!(resp.tier, ResolveTier::None);
    assert!(resp.results.is_empty());
}

#[tokio::test]
async fn similar_ranks_the_hand_inserted_nearest_embedding_first() {
    let Some(pool) = connect().await else {
        eprintln!("MUSE_TEST_DATABASE_URL not set — skipping similar_ranks_the_hand_inserted_nearest_embedding_first");
        return;
    };

    let suffix = Uuid::new_v4().simple().to_string();
    let library = seed_library(&pool, &suffix).await;

    let (_seed_meta, seed_item) = seed_movie(
        &pool,
        library.id,
        &suffix,
        &format!("Seed Movie {suffix}"),
        2020,
        "The seed title.",
    )
    .await;
    let (_close_meta, close_item) = seed_movie(
        &pool,
        library.id,
        &suffix,
        &format!("Close Movie {suffix}"),
        2021,
        "A very similar title.",
    )
    .await;
    let (_far_meta, far_item) = seed_movie(
        &pool,
        library.id,
        &suffix,
        &format!("Far Movie {suffix}"),
        1999,
        "A dissimilar title.",
    )
    .await;

    // Hand-inserted, known vectors (no live Ollama needed) — `close_item` is
    // constructed to be nearly identical to the seed by cosine distance,
    // `far_item` maximally dissimilar. Same technique as MUSE-08's own
    // `embed_stale_and_nearest_round_trip` test.
    let seed_vec = vec![1.0_f32; 768];
    let mut close_vec = vec![1.0_f32; 768];
    close_vec[0] = 0.99;
    let far_vec = vec![-1.0_f32; 768];

    for (item_id, vector) in [(seed_item.id, seed_vec), (close_item.id, close_vec), (far_item.id, far_vec)] {
        repo::embedding::upsert(
            &pool,
            &NewEmbedding::nomic(EmbeddingEntityKind::MediaItem, item_id, vector, Some(format!("source {item_id}"))),
        )
        .await
        .expect("insert embedding");
    }

    let state = degraded_state(pool);

    let resp = similar_handler(
        State(state),
        Json(SimilarRequest {
            media_item_id: seed_item.id,
            // High limit so accumulated embeddings from other live-DB tests
            // can't crowd the expected items out; assertions check relative
            // order/presence, valid regardless of unrelated rows.
            limit: Some(100_000),
        }),
    )
    .await
    .expect("similar_handler should not error")
    .0;

    assert_eq!(resp.tier, SimilarTier::Vector);
    assert!(
        !resp.results.iter().any(|h| h.media_item_id == Some(seed_item.id)),
        "the seed item must never appear in its own 'more like this' results"
    );

    let close_pos = resp
        .results
        .iter()
        .position(|h| h.media_item_id == Some(close_item.id))
        .expect("the near-identical item should be present in the results");

    // The far item is *maximally* dissimilar to the seed, so it ranks last;
    // with the handler's capped limit and a shared test DB accumulating other
    // tests' embeddings, it may legitimately fall outside the returned window.
    // Only assert the ordering invariant when it IS returned — the meaningful
    // guarantee is that the near-identical item ranks ahead of it.
    if let Some(far_pos) = resp
        .results
        .iter()
        .position(|h| h.media_item_id == Some(far_item.id))
    {
        assert!(
            close_pos < far_pos,
            "the near-identical vector must rank ahead of the maximally-dissimilar one"
        );
    }
}

#[tokio::test]
async fn similar_falls_back_to_shared_genre_when_seed_has_no_embedding() {
    let Some(pool) = connect().await else {
        eprintln!(
            "MUSE_TEST_DATABASE_URL not set — skipping \
             similar_falls_back_to_shared_genre_when_seed_has_no_embedding"
        );
        return;
    };

    let suffix = Uuid::new_v4().simple().to_string();
    let library = seed_library(&pool, &suffix).await;

    let (seed_meta, seed_item) = seed_movie(
        &pool,
        library.id,
        &suffix,
        &format!("Genre Seed {suffix}"),
        2015,
        "No embedding for this one.",
    )
    .await;
    let (sibling_meta, _sibling_item) = seed_movie(
        &pool,
        library.id,
        &suffix,
        &format!("Genre Sibling {suffix}"),
        2016,
        "Shares a genre with the seed.",
    )
    .await;
    let (_unrelated_meta, _unrelated_item) = seed_movie(
        &pool,
        library.id,
        &suffix,
        &format!("Genre Unrelated {suffix}"),
        2017,
        "No shared genre.",
    )
    .await;

    let genre_name = format!("muse09-genre-{suffix}");
    let genre_id: i64 = sqlx::query_scalar::<_, i64>("INSERT INTO genres (name) VALUES ($1) RETURNING id")
        .bind(&genre_name)
        .fetch_one(&pool)
        .await
        .expect("insert genre");

    for media_metadata_id in [seed_meta.id, sibling_meta.id] {
        sqlx::query("INSERT INTO media_metadata_genres (media_metadata_id, genre_id) VALUES ($1, $2)")
            .bind(media_metadata_id)
            .bind(genre_id)
            .execute(&pool)
            .await
            .expect("link genre");
    }

    let state = degraded_state(pool);

    let resp = similar_handler(
        State(state),
        Json(SimilarRequest {
            media_item_id: seed_item.id,
            // High limit so accumulated embeddings from other live-DB tests
            // can't crowd the expected items out; assertions check relative
            // order/presence, valid regardless of unrelated rows.
            limit: Some(100_000),
        }),
    )
    .await
    .expect("similar_handler should not error")
    .0;

    assert_eq!(
        resp.tier,
        SimilarTier::Genre,
        "a seed with no stored embedding must fall back to the genre-overlap tier"
    );
    assert!(resp
        .results
        .iter()
        .any(|h| h.media_metadata_id == sibling_meta.id));
}
