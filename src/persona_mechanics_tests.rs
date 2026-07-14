//! MUSEX-02 (Plane TERM #378): persona-model mechanics tests — DB-gated
//! coverage of `crate::persona::derive` (context-cluster + explicit
//! derivation) and `crate::repo::persona` (addressability, shared/
//! multi-account membership), plus the DETERMINISTIC and EXPLAINABLE
//! acceptance criteria.
//!
//! Same idioms as `taste_mechanics_tests.rs` (MUSET-05) throughout: reuses
//! the MUSET-04 fixture loader (`crate::fixtures::loader`) for
//! account/library/media-item scaffolding, the guarded snapshot pool
//! (`crate::snapshot::load`, gated on `MUSE_SNAPSHOT_DATABASE_URL`/
//! `MUSE_TEST_DATABASE_URL` — skips cleanly when neither is set), and FIXED
//! synthetic embedding vectors ([`fixed_vector`], identical construction to
//! `taste_mechanics_tests::fixed_vector`) — never a live embedding model
//! (S9).
//!
//! ## Layout
//! - [`context_cluster_derivation`] — DB-gated determinism + correctness of
//!   `derive::derive_context_cluster_personas`, mirroring
//!   `taste_mechanics_tests::context_centroid_mechanics`'s fixture/session
//!   setup so the two "cluster into weekend_evening/weekday_morning" stories
//!   stay in sync.
//! - [`explicit_derivation`] — DB-gated determinism + correctness of
//!   `derive::derive_explicit` over a caller-declared media-item set.
//! - [`addressability_and_sharing`] — DB-gated coverage of
//!   `repo::persona::{upsert_for_account, insert_shared, add_member,
//!   list_for_account, get_by_id, get_by_name_for_account}`, including a
//!   persona spanning TWO accounts (the "a persona can span people" AC).

use chrono::Utc;
use pgvector::Vector;
use sqlx::PgPool;

use crate::fixtures::{self, loader};
use crate::models::embedding::{EmbeddingEntityKind, NewEmbedding, EMBEDDING_DIM};
use crate::models::persona::PERSONA_KIND_DERIVED;
use crate::persona::derive;
use crate::repo;
use crate::snapshot::load as snapshot_load;

/// Same skip-cleanly-without-a-DB idiom as
/// `taste_mechanics_tests::mechanics_pool_or_skip` — reused independently
/// (that helper is private to its own module) rather than re-implemented
/// from scratch.
async fn persona_pool_or_skip(test_name: &str) -> Option<PgPool> {
    let Some(database_url) = snapshot_load::snapshot_database_url_from_env() else {
        eprintln!(
            "{} / {} not set -- skipping {test_name} (expected in the default test run; \
             MUSEX-02 persona mechanics tests do not require a live DB, and never contact a \
             live/production DB either)",
            snapshot_load::SNAPSHOT_DATABASE_URL_VAR,
            snapshot_load::TEST_DATABASE_URL_VAR,
        );
        return None;
    };
    let pool = snapshot_load::connect_snapshot_db(&database_url)
        .await
        .expect("connect to the configured snapshot/test DSN (guard-checked)");
    snapshot_load::migrate_snapshot_db(&pool)
        .await
        .expect("migrations should apply cleanly to the isolated snapshot DB");
    Some(pool)
}

/// Identical construction to `taste_mechanics_tests::fixed_vector` — a
/// deterministic, purely-computed EMBEDDING_DIM-length vector. Duplicated
/// locally rather than imported (that helper is private to its own module)
/// so this module's synthetic vectors are self-contained.
fn fixed_vector(seed: f32) -> Vec<f32> {
    (0..EMBEDDING_DIM as usize)
        .map(|i| ((i as f32) * 0.01 + seed).sin())
        .collect()
}

fn assert_vectors_bit_identical(a: &Vector, b: &Vector, what: &str) {
    assert_eq!(
        a.as_slice(),
        b.as_slice(),
        "{what}: two runs on byte-identical input produced different vectors -- \
         persona derivation must be deterministic"
    );
}

// ======================================================================
// 1. Context-cluster derivation: determinism + correctness + explainability
// ======================================================================
mod context_cluster_derivation {
    use super::*;
    use crate::models::play_session::NewPlaySession;

    /// Seeds the `multi_genre` fixture (4 distinct-genre items) with fixed
    /// embeddings and finished play_sessions bucketing items 0+1 into
    /// `weekend_evening` and item 2 into `weekday_morning` (item 3 gets no
    /// session, so it must never contribute) -- identical setup to
    /// `taste_mechanics_tests::context_centroid_mechanics`'s test, so the
    /// persona-cluster story and the existing context-centroid story never
    /// drift apart.
    async fn seed(pool: &PgPool) -> loader::LoadedFixture {
        let fixture = fixtures::multi_genre();
        let loaded = loader::load(pool, &fixture)
            .await
            .expect("fixture should load");
        let vectors = [
            fixed_vector(3.0),
            fixed_vector(4.0),
            fixed_vector(5.0),
            fixed_vector(6.0),
        ];
        for ((_, item), v) in loaded.items.iter().zip(vectors.iter()) {
            repo::embedding::upsert(
                pool,
                &NewEmbedding::nomic(EmbeddingEntityKind::MediaItem, item.id, v.clone(), None),
            )
            .await
            .expect("upsert fixed embedding");
        }

        let now = Utc::now();
        let sessions: [(usize, i32, i32); 3] = [
            (0, 20, 6), // Saturday evening -> weekend_evening
            (1, 21, 0), // Sunday evening -> weekend_evening
            (2, 9, 3),  // Wednesday morning -> weekday_morning
        ];
        for (idx, hour, dow) in sessions {
            let item_id = loaded.items[idx].1.id;
            repo::play_session::upsert(
                pool,
                &NewPlaySession {
                    account_id: Some(loaded.account.id),
                    media_item_id: Some(item_id),
                    episode_id: None,
                    session_key: None,
                    tautulli_ref_id: None,
                    started_at: now,
                    stopped_at: Some(now),
                    duration_ms: Some(1_000),
                    watched_ms: Some(1_000),
                    view_offset_ms: Some(1_000),
                    percent_complete: Some(100.0),
                    paused_counter: 0,
                    paused_ms: 0,
                    is_finished: true,
                    is_abandoned: false,
                    player: None,
                    platform: None,
                    product: None,
                    device: None,
                    ip_address: None,
                    started_hour: Some(hour),
                    started_dow: Some(dow),
                    is_cinema_context: None,
                },
            )
            .await
            .expect("insert finished session");
        }
        loaded
    }

    #[tokio::test]
    async fn derives_one_persona_per_context_bucket_deterministically() {
        let Some(pool) =
            persona_pool_or_skip("derives_one_persona_per_context_bucket_deterministically").await
        else {
            return;
        };
        let loaded = seed(&pool).await;

        let first = derive::derive_context_cluster_personas(&pool, loaded.account.id)
            .await
            .expect("derivation #1");
        let second = derive::derive_context_cluster_personas(&pool, loaded.account.id)
            .await
            .expect("derivation #2");

        let mut names1: Vec<String> = first.iter().map(|p| p.name.clone()).collect();
        let mut names2: Vec<String> = second.iter().map(|p| p.name.clone()).collect();
        names1.sort();
        names2.sort();
        assert_eq!(
            names1, names2,
            "the same account must derive the same set of persona names every run"
        );
        assert_eq!(
            names1,
            vec!["weekday_morning".to_string(), "weekend_evening".to_string()],
            "exactly the two seeded buckets must produce a persona; item 3 (no session) \
             must never produce a third"
        );

        // DETERMINISM AC: bit-identical centroids AND identical
        // defining_signals across two independent derivation runs.
        for p1 in &first {
            let p2 = second
                .iter()
                .find(|p| p.name == p1.name)
                .expect("the same persona name must exist in both runs");
            assert_vectors_bit_identical(
                &p1.centroid,
                &p2.centroid,
                &format!("persona '{}'", p1.name),
            );
            assert_eq!(
                p1.defining_signals, p2.defining_signals,
                "persona '{}': defining_signals must be identical across runs, not just the vector",
                p1.name
            );
            assert_eq!(p1.sample_size, p2.sample_size);
        }

        // CORRECTNESS: weekend_evening is the exact unweighted mean of
        // items 0+1's fixed vectors (mirrors compute_context_centroids'
        // documented contract, since context-cluster derivation reuses the
        // same bucketing + mean primitive).
        let weekend = first
            .iter()
            .find(|p| p.name == "weekend_evening")
            .expect("weekend_evening persona must exist");
        assert_eq!(weekend.sample_size, 2);
        let expected_weekend_mean: Vec<f32> = fixed_vector(3.0)
            .iter()
            .zip(fixed_vector(4.0).iter())
            .map(|(a, b)| (a + b) / 2.0)
            .collect();
        assert_eq!(
            weekend.centroid.as_slice(),
            expected_weekend_mean.as_slice(),
            "weekend_evening persona centroid must be the exact unweighted mean of items 0+1"
        );

        // EXPLAINABILITY AC: weekend_evening's defining signals must surface
        // its context bucket, its two source media items, and its top
        // genres (scifi + comedy, tied at count 1 -> alphabetical tiebreak
        // from repo::persona::genre_counts_for_media_items' SQL ORDER BY).
        let comedy = loaded
            .suffixed_genre("comedy")
            .expect("comedy genre seeded");
        let scifi = loaded.suffixed_genre("scifi").expect("scifi genre seeded");
        let weekend_top_genres = weekend.defining_signals["top_genres"]
            .as_array()
            .expect("top_genres must be an array");
        let weekend_genre_names: Vec<&str> = weekend_top_genres
            .iter()
            .map(|g| g["genre"].as_str().unwrap())
            .collect();
        assert_eq!(
            weekend_genre_names,
            vec![comedy, scifi],
            "tied-count genres must break alphabetically, deterministically"
        );
        assert_eq!(weekend.defining_signals["context_key"], "weekend_evening");
        let item0_id = loaded.items[0].1.id;
        let item1_id = loaded.items[1].1.id;
        let mut expected_ids = vec![item0_id, item1_id];
        expected_ids.sort();
        assert_eq!(
            weekend.defining_signals["source_media_item_ids"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_i64().unwrap())
                .collect::<Vec<_>>(),
            expected_ids,
            "defining_signals must surface exactly the source media items behind the centroid"
        );

        let weekday = first
            .iter()
            .find(|p| p.name == "weekday_morning")
            .expect("weekday_morning persona must exist");
        assert_eq!(weekday.sample_size, 1);
        assert_eq!(
            weekday.centroid.as_slice(),
            fixed_vector(5.0).as_slice(),
            "a single-sample bucket's persona centroid must be exactly that sample's vector"
        );

        loader::cleanup(&pool, &loaded).await.ok();
    }

    #[tokio::test]
    async fn a_bucket_with_no_embeddable_titles_produces_no_persona() {
        let Some(pool) =
            persona_pool_or_skip("a_bucket_with_no_embeddable_titles_produces_no_persona").await
        else {
            return;
        };
        // cold_start_empty: a library exists but no watch history at all --
        // no finished sessions means no context bucket has anything to
        // cluster, so derivation must return an empty Vec, never error.
        let fixture = fixtures::cold_start_empty();
        let loaded = loader::load(&pool, &fixture)
            .await
            .expect("fixture should load");

        let personas = derive::derive_context_cluster_personas(&pool, loaded.account.id)
            .await
            .expect("derivation must succeed even with nothing to cluster");
        assert!(
            personas.is_empty(),
            "no finished sessions -> no personas, never an error"
        );

        loader::cleanup(&pool, &loaded).await.ok();
    }
}

// ======================================================================
// 2. Explicit derivation: an operator/user-declared persona over a
//    caller-chosen media-item set.
// ======================================================================
mod explicit_derivation {
    use super::*;

    #[tokio::test]
    async fn derive_explicit_averages_exactly_the_declared_items_deterministically() {
        let Some(pool) = persona_pool_or_skip(
            "derive_explicit_averages_exactly_the_declared_items_deterministically",
        )
        .await
        else {
            return;
        };
        let fixture = fixtures::multi_genre();
        let loaded = loader::load(&pool, &fixture)
            .await
            .expect("fixture should load");
        let vectors = [
            fixed_vector(3.0),
            fixed_vector(4.0),
            fixed_vector(5.0),
            fixed_vector(6.0),
        ];
        for ((_, item), v) in loaded.items.iter().zip(vectors.iter()) {
            repo::embedding::upsert(
                &pool,
                &NewEmbedding::nomic(EmbeddingEntityKind::MediaItem, item.id, v.clone(), None),
            )
            .await
            .expect("upsert fixed embedding");
        }

        // Declare an explicit "prestige-drama" persona over items 2+3 (no
        // play_session/watch history involved at all -- this is the
        // operator/user-declared path, distinct from clustering).
        let item2_id = loaded.items[2].1.id;
        let item3_id = loaded.items[3].1.id;

        // Pass the ids in TWO different orders across the two runs -- the
        // determinism AC requires the SAME SET of inputs to produce the
        // same persona regardless of caller-supplied ordering.
        let forward = derive::derive_explicit(&pool, "prestige-drama", &[item2_id, item3_id])
            .await
            .expect("derive_explicit must succeed")
            .expect("both items have embeddings -- must produce a persona");
        let reversed = derive::derive_explicit(&pool, "prestige-drama", &[item3_id, item2_id])
            .await
            .expect("derive_explicit must succeed")
            .expect("both items have embeddings -- must produce a persona");

        assert_vectors_bit_identical(
            &forward.centroid,
            &reversed.centroid,
            "explicit persona centroid must not depend on input id order",
        );
        assert_eq!(forward.defining_signals, reversed.defining_signals);
        assert_eq!(forward.sample_size, 2);

        let expected_mean: Vec<f32> = fixed_vector(5.0)
            .iter()
            .zip(fixed_vector(6.0).iter())
            .map(|(a, b)| (a + b) / 2.0)
            .collect();
        assert_eq!(
            forward.centroid.as_slice(),
            expected_mean.as_slice(),
            "explicit persona centroid must be the exact unweighted mean of the declared items"
        );
        // Explicit personas have no context bucket.
        assert!(forward.defining_signals["context_key"].is_null());

        loader::cleanup(&pool, &loaded).await.ok();
    }

    #[tokio::test]
    async fn derive_explicit_returns_none_for_an_empty_or_unembeddable_set() {
        let Some(pool) =
            persona_pool_or_skip("derive_explicit_returns_none_for_an_empty_or_unembeddable_set")
                .await
        else {
            return;
        };
        let empty = derive::derive_explicit(&pool, "empty-persona", &[])
            .await
            .expect("must not error on an empty id set");
        assert!(empty.is_none());

        // An id that plausibly cannot exist (no such media_items row, hence
        // no embedding either) -- must degrade to None, never error.
        let unembeddable = derive::derive_explicit(&pool, "no-embedding-persona", &[i64::MAX])
            .await
            .expect("must not error on an id with no stored embedding");
        assert!(unembeddable.is_none());
    }
}

// ======================================================================
// 3. Addressability + sharing (a persona spanning multiple accounts)
// ======================================================================
mod addressability_and_sharing {
    use super::*;

    #[tokio::test]
    async fn upsert_for_account_is_addressable_by_id_and_by_name() {
        let Some(pool) =
            persona_pool_or_skip("upsert_for_account_is_addressable_by_id_and_by_name").await
        else {
            return;
        };
        let fixture = fixtures::cold_start_empty();
        let loaded = loader::load(&pool, &fixture)
            .await
            .expect("fixture should load");

        let new_persona = crate::models::persona::NewPersona {
            account_id: Some(loaded.account.id),
            name: "solo-2am".to_string(),
            kind: PERSONA_KIND_DERIVED.to_string(),
            centroid: Vector::from(fixed_vector(9.0)),
            defining_signals: serde_json::json!({"context_key": "weekday_late_night", "top_genres": [], "source_media_item_ids": []}),
            metadata: serde_json::json!({}),
            sample_size: 1,
        };
        let persisted = repo::persona::upsert_for_account(&pool, &new_persona)
            .await
            .expect("upsert must succeed");
        assert_eq!(persisted.name, "solo-2am");

        let by_id = repo::persona::get_by_id(&pool, persisted.id)
            .await
            .expect("get_by_id must succeed")
            .expect("persona must be found by id");
        assert_eq!(by_id.id, persisted.id);

        let by_name = repo::persona::get_by_name_for_account(&pool, loaded.account.id, "solo-2am")
            .await
            .expect("get_by_name_for_account must succeed")
            .expect("persona must be found by (account, name)");
        assert_eq!(by_name.id, persisted.id);

        let listed = repo::persona::list_for_account(&pool, loaded.account.id)
            .await
            .expect("list_for_account must succeed");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, persisted.id);

        // A re-derivation (same account/name/kind) must UPSERT in place,
        // not accumulate a duplicate row.
        let mut updated = new_persona.clone();
        updated.centroid = Vector::from(fixed_vector(10.0));
        updated.sample_size = 2;
        let reupserted = repo::persona::upsert_for_account(&pool, &updated)
            .await
            .expect("re-upsert must succeed");
        assert_eq!(
            reupserted.id, persisted.id,
            "re-deriving must update the same row, not insert a new one"
        );
        assert_eq!(reupserted.sample_size, 2);
        let listed_after = repo::persona::list_for_account(&pool, loaded.account.id)
            .await
            .expect("list_for_account must succeed");
        assert_eq!(
            listed_after.len(),
            1,
            "no duplicate persona row after re-derivation"
        );

        loader::cleanup(&pool, &loaded).await.ok();
        // personas.account_id references accounts(id) ON DELETE CASCADE --
        // cleanup's account delete above already removed the persona row.
        assert!(repo::persona::get_by_id(&pool, persisted.id)
            .await
            .unwrap()
            .is_none());
    }

    /// A persona can SPAN PEOPLE (the couch-group case): a shared persona
    /// (`account_id IS NULL`) with two member accounts, addressable from
    /// either account.
    #[tokio::test]
    async fn a_shared_persona_is_addressable_from_every_member_account() {
        let Some(pool) =
            persona_pool_or_skip("a_shared_persona_is_addressable_from_every_member_account").await
        else {
            return;
        };
        let fixture_a = fixtures::cold_start_empty();
        let loaded_a = loader::load(&pool, &fixture_a)
            .await
            .expect("fixture A should load");
        let fixture_b = fixtures::sparse_metadata();
        let loaded_b = loader::load(&pool, &fixture_b)
            .await
            .expect("fixture B should load");

        let shared = crate::models::persona::NewPersona {
            account_id: None,
            name: "household-movie-night".to_string(),
            kind: PERSONA_KIND_DERIVED.to_string(),
            centroid: Vector::from(fixed_vector(11.0)),
            defining_signals: serde_json::json!({"context_key": null, "top_genres": [], "source_media_item_ids": []}),
            metadata: serde_json::json!({}),
            sample_size: 0,
        };
        let persisted = repo::persona::insert_shared(&pool, &shared)
            .await
            .expect("insert_shared must succeed");
        assert!(persisted.account_id.is_none());

        repo::persona::add_member(&pool, persisted.id, loaded_a.account.id)
            .await
            .expect("add_member A must succeed");
        repo::persona::add_member(&pool, persisted.id, loaded_b.account.id)
            .await
            .expect("add_member B must succeed");

        let members = repo::persona::list_members(&pool, persisted.id)
            .await
            .expect("list_members must succeed");
        let mut expected_members = vec![loaded_a.account.id, loaded_b.account.id];
        expected_members.sort();
        assert_eq!(members, expected_members);

        for account_id in [loaded_a.account.id, loaded_b.account.id] {
            let found =
                repo::persona::get_by_name_for_account(&pool, account_id, "household-movie-night")
                    .await
                    .expect("get_by_name_for_account must succeed")
                    .expect("the shared persona must be addressable from every member account");
            assert_eq!(found.id, persisted.id);

            let listed = repo::persona::list_for_account(&pool, account_id)
                .await
                .expect("list_for_account must succeed");
            assert!(listed.iter().any(|p| p.id == persisted.id));
        }

        // Re-derivation of a shared persona goes through replace_centroid
        // (no natural upsert key for account_id IS NULL rows).
        let replaced = repo::persona::replace_centroid(
            &pool,
            persisted.id,
            &Vector::from(fixed_vector(12.0)),
            &serde_json::json!({"context_key": null, "top_genres": [], "source_media_item_ids": []}),
            0,
        )
        .await
        .expect("replace_centroid must succeed");
        assert_eq!(replaced.id, persisted.id);
        assert_eq!(
            replaced.centroid.as_slice(),
            Vector::from(fixed_vector(12.0)).as_slice()
        );

        // Cleanup: fixture cleanup cascades persona_members (accounts ON
        // DELETE CASCADE) but the orphaned account_id-IS-NULL persona row
        // itself needs an explicit delete.
        loader::cleanup(&pool, &loaded_a).await.ok();
        loader::cleanup(&pool, &loaded_b).await.ok();
        sqlx::query("DELETE FROM personas WHERE id = $1")
            .bind(persisted.id)
            .execute(&pool)
            .await
            .ok();
    }
}
