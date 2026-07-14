//! MUSET-05 (Plane TERM #370): TASTE model mechanics tests — the
//! deterministic, automatable floor of TASTE validation.
//!
//! Scope (per the MUSET-05 build brief): unit/integration coverage for the
//! MECHANICS underneath the taste model — embedding handling, pgvector
//! operations, "clustering" (the weekend/weekday x time-of-day context
//! bucketing in [`crate::taste_model::profile::compute_context_centroids`]),
//! and scoring/ranking consistency — asserting that identical input always
//! produces identical output. This is deliberately NOT a test of taste
//! *quality* (whether a recommendation is "good") — it's the mechanical
//! contract every higher-level taste behavior depends on.
//!
//! ## Grounding
//! Read before writing anything here: `taste_model::{signals,profile,mod}`,
//! `repo::taste`/`repo::embedding` (the only pgvector-touching repo module —
//! `embedding <=> $query`, cosine distance, confirmed in
//! `src/repo/embedding.rs`), `models::embedding` (dim + model pin),
//! `curation::{candidates,recommend}` (scoring/ranking/dedup), and the
//! MUSET-04 fixtures (`src/fixtures/*`) + their loader idiom
//! (`src/fixtures/loader.rs`'s `tests::fixture_pool_or_skip`, reused below
//! verbatim as [`mechanics_pool_or_skip`]).
//!
//! ## Embedding dimension
//! **768** — confirmed from `models::embedding::EMBEDDING_DIM` (pinned to
//! `nomic-embed-text`) AND cross-checked against the actual schema,
//! `migrations/0018_embeddings.sql`'s `embedding vector(768) NOT NULL`
//! column. Every fixed vector below is built at exactly this dimension via
//! [`fixed_vector`].
//!
//! ## No live model, no raw pool (S9)
//! Every vector here is a pure, seed-derived synthetic constant
//! ([`fixed_vector`]) — never a call to Ollama/Chord or any embedding
//! service. Every DB-touching test goes through the SAME guarded path
//! MUSET-03/04 established
//! (`crate::snapshot::load::connect_snapshot_db`/`_from_env`, gated on
//! `MUSE_SNAPSHOT_DATABASE_URL`/`MUSE_TEST_DATABASE_URL`) and reuses the
//! MUSET-04 fixture loader (`crate::fixtures::loader::{load,cleanup}`) for
//! account/library/media-item scaffolding — nothing here opens its own raw
//! `PgPool`, and nothing here ever contacts a live/production database.
//!
//! ## Layout
//! - [`pure_math`] — scoring/ranking/dedup determinism with NO database
//!   (runs unconditionally, every `cargo test`). Contains the negative
//!   nondeterminism finding (see its module doc).
//! - [`pgvector_mechanics`] — DB-gated pgvector correctness + determinism
//!   (`repo::embedding::{upsert,get,get_many,nearest}`) against fixed
//!   vectors.
//! - [`centroid_mechanics`] — DB-gated determinism of
//!   `taste_model::profile::compute_overall_centroid`.
//! - [`context_centroid_mechanics`] — DB-gated determinism + correctness of
//!   the context-bucketing/"clustering" step,
//!   `taste_model::profile::compute_context_centroids`.

use chrono::Utc;
use pgvector::Vector;
use sqlx::PgPool;

use crate::fixtures::{self, loader};
use crate::models::embedding::{
    EmbeddingEntityKind, NewEmbedding, DEFAULT_EMBEDDING_MODEL, EMBEDDING_DIM,
};
use crate::repo;
use crate::snapshot::load as snapshot_load;
use crate::taste_model::profile;
use crate::taste_model::signals::{replace_derived_signals, DEFAULT_HALF_LIFE_DAYS};

/// Same skip-cleanly-without-a-DB idiom as
/// `crate::fixtures::loader::tests::fixture_pool_or_skip` — reused
/// (independently, since that helper is private to its own test module)
/// rather than re-implemented from scratch: always try
/// `MUSE_SNAPSHOT_DATABASE_URL`/`MUSE_TEST_DATABASE_URL` through the
/// guarded `crate::snapshot::load` path, and skip (never fail) when neither
/// is configured.
async fn mechanics_pool_or_skip(test_name: &str) -> Option<PgPool> {
    let Some(database_url) = snapshot_load::snapshot_database_url_from_env() else {
        eprintln!(
            "{} / {} not set -- skipping {test_name} (expected in the default test run; \
             MUSET-05 mechanics tests do not require a live DB for the pure-math suite, \
             and never contact a live/production DB for the DB-gated suite either)",
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

/// A deterministic, purely-computed `EMBEDDING_DIM`-length (768) vector —
/// distinct `seed`s produce distinct vectors, and the SAME `seed` always
/// produces a bit-identical vector (no RNG, no clock, no I/O). This is the
/// "FIXED, synthetic embedding vector" every DB-gated test below feeds into
/// the real pgvector/taste-mechanics code instead of ever calling a live
/// embedding model.
fn fixed_vector(seed: f32) -> Vec<f32> {
    (0..EMBEDDING_DIM as usize)
        .map(|i| ((i as f32) * 0.01 + seed).sin())
        .collect()
}

fn assert_vectors_bit_identical(a: &Vector, b: &Vector, what: &str) {
    assert_eq!(
        a.as_slice(),
        b.as_slice(),
        "{what}: two runs on byte-identical input produced different vectors — \
         taste-model mechanics must be deterministic"
    );
}

// ======================================================================
// 1. Pure-math determinism (no database) — scoring / ranking / dedup
//    mechanics. Runs unconditionally on every `cargo test`.
// ======================================================================
mod pure_math {
    use super::*;
    use crate::curation::candidates::{dedup_candidates, Candidate, CandidateSource};
    use crate::curation::recommend::{rank_candidates, score_candidate};
    use crate::models::media_metadata::MediaKind;

    fn tied_candidate(id: i64, fact: &str) -> Candidate {
        Candidate {
            media_metadata_id: id,
            media_item_id: Some(id),
            title: format!("MUSET-05 Tied Candidate {id}"),
            year: Some(2020),
            kind: MediaKind::Movie,
            source: CandidateSource::Taste,
            // Identical taste_fit + identical source -> identical score for
            // every candidate below (score_candidate = source_weight *
            // taste_fit, no availability adjustment here).
            taste_fit: 0.5,
            facts: vec![fact.to_string()],
            availability: None,
        }
    }

    #[test]
    fn score_candidate_is_deterministic_for_identical_input() {
        let c = tied_candidate(1, "fact");
        let a = score_candidate(&c);
        let b = score_candidate(&c);
        // Bit-identical, not just numerically close: score_candidate is
        // pure arithmetic over the same f64 inputs every time.
        assert_eq!(
            a.to_bits(),
            b.to_bits(),
            "score_candidate must be a pure, bit-deterministic function of its input"
        );
    }

    #[test]
    fn rank_candidates_is_deterministic_when_scores_are_distinct() {
        // Distinct sources/taste_fit -> distinct scores -> the ranked order
        // is fully determined by score alone, with no ties to worry about.
        let make_input = || {
            vec![
                {
                    let mut c = tied_candidate(1, "on-deck");
                    c.source = CandidateSource::OnDeck;
                    c.taste_fit = 0.9;
                    c
                },
                {
                    let mut c = tied_candidate(2, "gap");
                    c.source = CandidateSource::Gap;
                    c.taste_fit = 0.9;
                    c
                },
                {
                    let mut c = tied_candidate(3, "taste");
                    c.source = CandidateSource::Taste;
                    c.taste_fit = 0.9;
                    c
                },
            ]
        };

        let run = || {
            rank_candidates(make_input())
                .into_iter()
                .map(|(c, s)| (c.media_metadata_id, s))
                .collect::<Vec<_>>()
        };

        let first = run();
        let second = run();
        assert_eq!(
            first, second,
            "rank_candidates must produce identical (id, score) pairs, in the same order, \
             on every run over identical input"
        );
        assert_eq!(
            first.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
            vec![1, 2, 3],
            "on-deck > gap > taste at equal taste_fit, deterministically"
        );
    }

    /// `rank_candidates` itself (its `slice::sort_by`, which is documented
    /// and here verified stable) is NOT the source of nondeterminism — feed
    /// it a hand-built, already-ordered `Vec` directly (bypassing
    /// `dedup_candidates` entirely) and confirm ties keep exactly the input
    /// order, every run. This isolates `rank_candidates`'s own mechanics
    /// from the `dedup_candidates` finding below.
    #[test]
    fn rank_candidates_preserves_insertion_order_for_tied_scores_when_fed_directly() {
        let make_input = || {
            (1..=10i64)
                .map(|id| tied_candidate(id, "same fact"))
                .collect::<Vec<_>>()
        };

        let run = || {
            rank_candidates(make_input())
                .into_iter()
                .map(|(c, _)| c.media_metadata_id)
                .collect::<Vec<_>>()
        };

        let first = run();
        let second = run();
        assert_eq!(
            first, second,
            "rank_candidates' stable sort must reproduce the same tied-score order every run"
        );
        assert_eq!(
            first,
            (1..=10i64).collect::<Vec<_>>(),
            "ties must preserve exact input order (a stable sort), not reorder them"
        );
    }

    #[test]
    fn dedup_candidates_merges_same_id_and_leaves_distinct_ids_untouched() {
        // Positive/regression check on dedup_candidates' CONTENT contract
        // (independent of the order finding below): same id merges to one
        // survivor with both sources' facts; distinct ids are untouched.
        let candidates = vec![
            tied_candidate(1, "fact a"),
            {
                let mut c = tied_candidate(1, "fact b");
                c.source = CandidateSource::OnDeck; // higher priority than Taste
                c
            },
            tied_candidate(2, "fact c"),
        ];
        let deduped = dedup_candidates(candidates);
        assert_eq!(
            deduped.len(),
            2,
            "id 1's two entries must merge into one survivor"
        );
        let merged = deduped
            .iter()
            .find(|c| c.media_metadata_id == 1)
            .expect("id 1 survivor");
        assert!(merged.facts.iter().any(|f| f == "fact a"));
        assert!(merged.facts.iter().any(|f| f == "fact b"));
        assert!(deduped.iter().any(|c| c.media_metadata_id == 2));
    }

    /// NEGATIVE / regression-guard test — a DOCUMENTED FINDING, not fixed
    /// here (per the MUSET-05 build brief: "if you find a REAL
    /// nondeterminism bug in the taste code, DO NOT fix the app here...
    /// document it as an `#[ignore]`d regression guard with a clear
    /// reason, like MUSET-01 did for the route bug").
    ///
    /// ## The finding
    /// `curation::candidates::dedup_candidates` (`src/curation/candidates.rs`)
    /// collects its de-duplicated result via
    /// `HashMap<i64, Candidate>::into_values().collect()`. `std::collections::
    /// HashMap`'s iteration order is a function of its `RandomState` hash
    /// keys, and the standard library derives a FRESH key for every
    /// `HashMap::new()` call within a thread (a per-thread counter is folded
    /// into the seed on each call) — so two calls to `dedup_candidates` on
    /// byte-identical input, even within the very same process/thread, are
    /// NOT guaranteed to hand back their surviving candidates in the same
    /// order.
    ///
    /// `rank_candidates`'s sort is stable
    /// (`rank_candidates_preserves_insertion_order_for_tied_scores_when_fed_directly`
    /// above proves `slice::sort_by` itself is fine), so the nondeterminism
    /// is entirely upstream, in `dedup_candidates`'s HashMap collect. The
    /// practical consequence: for any group of `/recommend` candidates that
    /// end up with EQUAL scores after scoring (a realistic case — several
    /// taste-tier picks can easily land on the same rounded `taste_fit`),
    /// the final response order for that tied group silently depends on
    /// `HashMap` bucket layout, not on any deterministic tiebreak (e.g.
    /// `media_metadata_id`) and not on original candidate-gathering order
    /// either. Two otherwise-identical `/recommend` calls can return the
    /// same items in a different order.
    ///
    /// ## Why this is `#[ignore]`d rather than a normal failing test
    /// The bug is a *missing guarantee*, not a guaranteed-wrong output: two
    /// small `HashMap`s CAN happen to iterate in the same order (the
    /// per-thread key counter cycles through a large keyspace, but a
    /// collision in RELATIVE bucket order for a specific small key set is
    /// possible), so asserting inequality unconditionally would itself be a
    /// flaky/non-deterministic test — that unreliability *is* the bug being
    /// documented, not a separate problem with the test. Run this manually,
    /// several times, to observe it in practice:
    /// `cargo test --release -- --ignored \
    ///  dedup_then_rank_output_order_is_not_guaranteed_deterministic_for_tied_scores`
    ///
    /// ## Suggested fix (NOT applied here — flag as a follow-up item)
    /// Replace the `HashMap<i64, Candidate>` in `dedup_candidates` with a
    /// `BTreeMap<i64, Candidate>` (deterministic key-sorted iteration), or
    /// keep the `HashMap` for O(1) lookup but collect into a `Vec` sorted by
    /// `media_metadata_id` (or original first-seen order) before returning;
    /// alternately, have `rank_candidates` apply an explicit secondary sort
    /// key (`media_metadata_id`) after the score comparison so tied-score
    /// output order is reproducible regardless of what feeds it.
    #[test]
    #[ignore = "MUSET-05 finding (documented, not fixed here): dedup_candidates' \
                HashMap<i64,Candidate>::into_values().collect() makes tied-score \
                /recommend ordering non-deterministic across repeated calls with \
                identical input. See this test's doc comment for the full analysis \
                and the suggested fix (BTreeMap, or an explicit tiebreak key in \
                rank_candidates). Tracked as a follow-up item, not fixed by MUSET-05."]
    fn dedup_then_rank_output_order_is_not_guaranteed_deterministic_for_tied_scores() {
        let make_input = || {
            (1..=10i64)
                .map(|id| tied_candidate(id, "same fact"))
                .collect::<Vec<_>>()
        };

        let run = || {
            let deduped = dedup_candidates(make_input());
            rank_candidates(deduped)
                .into_iter()
                .map(|(c, _)| c.media_metadata_id)
                .collect::<Vec<_>>()
        };

        let first = run();
        let second = run();
        assert_eq!(
            first, second,
            "two runs of dedup_candidates -> rank_candidates on identical tied-score input \
             produced DIFFERENT orderings ({first:?} vs {second:?}) — this reproduces the \
             HashMap-iteration-order nondeterminism documented above"
        );
    }
}

// ======================================================================
// 2. DB-gated pgvector correctness + determinism
//    (repo::embedding::{upsert,get_many,nearest}).
// ======================================================================
mod pgvector_mechanics {
    use super::*;

    /// Seed the MUSET-04 `multi_genre` fixture (4 real, distinct media
    /// items — reused purely for its library/media-item scaffolding; its
    /// watch signals are irrelevant here) and upsert three FIXED, distinct
    /// synthetic embeddings onto the first three items, deliberately
    /// leaving the fourth item with NO stored embedding (used by
    /// `get_many_...omits_ids_with_no_stored_embedding` below).
    async fn seed_three_embedded_items(pool: &PgPool) -> (loader::LoadedFixture, [i64; 3]) {
        let fixture = fixtures::multi_genre();
        let loaded = loader::load(pool, &fixture)
            .await
            .expect("fixture should load");
        assert!(
            loaded.items.len() >= 4,
            "multi_genre fixture must have at least 4 items for this test's setup"
        );

        let ids: [i64; 3] = [
            loaded.items[0].1.id,
            loaded.items[1].1.id,
            loaded.items[2].1.id,
        ];
        // item 0 and item 1 are a small perturbation apart; item 2 is far
        // away -- an unambiguous nearest-to-farthest ordering.
        let vectors = [fixed_vector(0.0), fixed_vector(0.01), fixed_vector(9.0)];
        for (id, v) in ids.iter().zip(vectors.iter()) {
            repo::embedding::upsert(
                pool,
                &NewEmbedding::nomic(EmbeddingEntityKind::MediaItem, *id, v.clone(), None),
            )
            .await
            .expect("upsert fixed embedding");
        }
        (loaded, ids)
    }

    /// `repo::embedding::nearest` orders across the WHOLE `embeddings`
    /// table for the given `(entity_kind, model)`, not scoped to any one
    /// fixture load — so under `cargo test`'s default parallelism, other
    /// tests' rows can legitimately be closer to a query than this test's
    /// own seeded vectors. A high `limit` (same idiom
    /// `integration_tests.rs`'s embedding test already uses, with the same
    /// comment) plus filtering the result down to just this test's own
    /// entity ids (preserving pgvector's returned order) gives a
    /// rank/order assertion that is correct regardless of what else is in
    /// the shared scratch DB.
    const OVER_FETCH_LIMIT: i64 = 100_000;

    fn filter_to_own_ids(
        matches: &[crate::models::embedding::EmbeddingMatch],
        ids: &[i64],
    ) -> Vec<crate::models::embedding::EmbeddingMatch> {
        matches
            .iter()
            .filter(|m| ids.contains(&m.entity_id))
            .cloned()
            .collect()
    }

    #[tokio::test]
    async fn nearest_neighbor_query_is_deterministic_across_repeated_runs() {
        let Some(pool) =
            mechanics_pool_or_skip("nearest_neighbor_query_is_deterministic_across_repeated_runs")
                .await
        else {
            return;
        };
        let (loaded, ids) = seed_three_embedded_items(&pool).await;

        let query = Vector::from(fixed_vector(0.0));
        let first = repo::embedding::nearest(
            &pool,
            "media_item",
            DEFAULT_EMBEDDING_MODEL,
            &query,
            OVER_FETCH_LIMIT,
        )
        .await
        .expect("nearest #1");
        let second = repo::embedding::nearest(
            &pool,
            "media_item",
            DEFAULT_EMBEDDING_MODEL,
            &query,
            OVER_FETCH_LIMIT,
        )
        .await
        .expect("nearest #2");

        let own1 = filter_to_own_ids(&first, &ids);
        let own2 = filter_to_own_ids(&second, &ids);

        let ids1: Vec<i64> = own1.iter().map(|m| m.entity_id).collect();
        let ids2: Vec<i64> = own2.iter().map(|m| m.entity_id).collect();
        assert_eq!(
            ids1, ids2,
            "same query against unchanged data -- pgvector nearest() must return this test's \
             own three entities in the same relative order on every run"
        );

        let d1: Vec<f64> = own1.iter().map(|m| m.distance).collect();
        let d2: Vec<f64> = own2.iter().map(|m| m.distance).collect();
        assert_eq!(
            d1, d2,
            "cosine distances must be identical across repeated runs of the same query"
        );

        loader::cleanup(&pool, &loaded).await.ok();
    }

    #[tokio::test]
    async fn nearest_neighbor_query_orders_by_the_correct_known_cosine_distance() {
        let Some(pool) = mechanics_pool_or_skip(
            "nearest_neighbor_query_orders_by_the_correct_known_cosine_distance",
        )
        .await
        else {
            return;
        };
        let (loaded, ids) = seed_three_embedded_items(&pool).await;

        // Query exactly matches item 0's stored vector -> item 0 must win,
        // item 1 (a small perturbation) is next, item 2 (far) is last --
        // among THIS TEST'S OWN three entities (see OVER_FETCH_LIMIT's doc
        // comment for why the raw top-3 of the whole table isn't the right
        // assertion under parallel test execution).
        let query = Vector::from(fixed_vector(0.0));
        let all_matches = repo::embedding::nearest(
            &pool,
            "media_item",
            DEFAULT_EMBEDDING_MODEL,
            &query,
            OVER_FETCH_LIMIT,
        )
        .await
        .expect("nearest");
        let matches = filter_to_own_ids(&all_matches, &ids);
        assert_eq!(
            matches.len(),
            3,
            "all three of this test's own seeded entities must appear in the result"
        );
        assert_eq!(
            matches[0].entity_id, ids[0],
            "the item embedded with the exact query vector must be the closest match"
        );
        assert!(
            matches[0].distance < 1e-4,
            "cosine distance from a vector to itself must be ~0, got {}",
            matches[0].distance
        );
        assert_eq!(
            matches[1].entity_id, ids[1],
            "the nearby perturbed vector must be the second-closest match"
        );
        assert_eq!(
            matches[2].entity_id, ids[2],
            "the far vector must be the last match"
        );
        assert!(
            matches[0].distance < matches[1].distance && matches[1].distance < matches[2].distance,
            "distances must be strictly increasing in the known nearest-to-farthest order: {:?}",
            matches.iter().map(|m| m.distance).collect::<Vec<_>>()
        );

        loader::cleanup(&pool, &loaded).await.ok();
    }

    #[tokio::test]
    async fn get_many_is_deterministic_and_omits_ids_with_no_stored_embedding() {
        let Some(pool) = mechanics_pool_or_skip(
            "get_many_is_deterministic_and_omits_ids_with_no_stored_embedding",
        )
        .await
        else {
            return;
        };
        let (loaded, ids) = seed_three_embedded_items(&pool).await;
        // multi_genre has 4 items; item 3 never got an embedding above.
        let unembedded_id = loaded.items[3].1.id;

        let request_ids = vec![ids[0], ids[1], ids[2], unembedded_id];
        let first =
            repo::embedding::get_many(&pool, "media_item", DEFAULT_EMBEDDING_MODEL, &request_ids)
                .await
                .expect("get_many #1");
        let second =
            repo::embedding::get_many(&pool, "media_item", DEFAULT_EMBEDDING_MODEL, &request_ids)
                .await
                .expect("get_many #2");

        let mut set1: Vec<i64> = first.iter().map(|e| e.entity_id).collect();
        let mut set2: Vec<i64> = second.iter().map(|e| e.entity_id).collect();
        set1.sort();
        set2.sort();
        assert_eq!(
            set1, set2,
            "get_many must return the same set of ids for the same request, every run"
        );
        let mut expected = ids.to_vec();
        expected.sort();
        assert_eq!(
            set1, expected,
            "get_many must return exactly the embedded ids, and skip the id with no stored embedding"
        );
        assert!(
            !set1.contains(&unembedded_id),
            "an id with no stored embedding must never appear in get_many's result \
             (this is the 'skip cleanly' contract compute_overall_centroid depends on)"
        );

        loader::cleanup(&pool, &loaded).await.ok();
    }
}

// ======================================================================
// 3. DB-gated determinism of taste_model::profile::compute_overall_centroid
//    (the recency/rewatch-weighted mean-embedding "scoring" mechanic).
// ======================================================================
mod centroid_mechanics {
    use super::*;

    #[tokio::test]
    async fn compute_overall_centroid_is_deterministic_given_fixed_embeddings() {
        let Some(pool) = mechanics_pool_or_skip(
            "compute_overall_centroid_is_deterministic_given_fixed_embeddings",
        )
        .await
        else {
            return;
        };

        // heavy_rewatcher: 2 finished items, both with the same days_ago (5)
        // -- their recency-decay weight is identical, so summing their two
        // fixed vectors is a plain (commutative) addition regardless of
        // which order the DB happens to return the two watch_stats rows in.
        let fixture = fixtures::heavy_rewatcher();
        let loaded = loader::load(&pool, &fixture)
            .await
            .expect("fixture should load");
        replace_derived_signals(&pool, loaded.account.id)
            .await
            .expect("deriving taste_signals should succeed");

        let vectors = [fixed_vector(1.0), fixed_vector(2.0)];
        for ((_, item), v) in loaded.items.iter().zip(vectors.iter()) {
            repo::embedding::upsert(
                &pool,
                &NewEmbedding::nomic(EmbeddingEntityKind::MediaItem, item.id, v.clone(), None),
            )
            .await
            .expect("upsert fixed embedding");
        }

        let now = Utc::now();
        let first = profile::compute_overall_centroid(
            &pool,
            loaded.account.id,
            now,
            DEFAULT_HALF_LIFE_DAYS,
        )
        .await
        .expect("centroid #1")
        .expect("a centroid should exist -- both finished items have embeddings");
        let second = profile::compute_overall_centroid(
            &pool,
            loaded.account.id,
            now,
            DEFAULT_HALF_LIFE_DAYS,
        )
        .await
        .expect("centroid #2")
        .expect("a centroid should exist -- both finished items have embeddings");

        assert_vectors_bit_identical(&first, &second, "compute_overall_centroid");
        assert_eq!(first.as_slice().len(), EMBEDDING_DIM as usize);

        loader::cleanup(&pool, &loaded).await.ok();
    }

    #[tokio::test]
    async fn compute_overall_centroid_skips_finished_items_with_no_embedding_deterministically() {
        let Some(pool) = mechanics_pool_or_skip(
            "compute_overall_centroid_skips_finished_items_with_no_embedding_deterministically",
        )
        .await
        else {
            return;
        };

        // No embeddings uploaded at all -> must degrade to None, never an
        // error, and that degrade must itself be stable across two calls.
        let fixture = fixtures::heavy_rewatcher();
        let loaded = loader::load(&pool, &fixture)
            .await
            .expect("fixture should load");
        replace_derived_signals(&pool, loaded.account.id)
            .await
            .expect("deriving taste_signals should succeed");

        let now = Utc::now();
        let first = profile::compute_overall_centroid(
            &pool,
            loaded.account.id,
            now,
            DEFAULT_HALF_LIFE_DAYS,
        )
        .await
        .expect("centroid #1 should not error");
        let second = profile::compute_overall_centroid(
            &pool,
            loaded.account.id,
            now,
            DEFAULT_HALF_LIFE_DAYS,
        )
        .await
        .expect("centroid #2 should not error");
        assert!(
            first.is_none() && second.is_none(),
            "no stored embeddings anywhere -> None, deterministically, on every run"
        );

        loader::cleanup(&pool, &loaded).await.ok();
    }
}

// ======================================================================
// 4. DB-gated determinism + correctness of the context-bucketing
//    ("clustering") mechanic, taste_model::profile::compute_context_centroids.
// ======================================================================
mod context_centroid_mechanics {
    use super::*;
    use crate::models::play_session::NewPlaySession;

    #[tokio::test]
    async fn compute_context_centroids_buckets_deterministically_and_matches_the_known_mean() {
        let Some(pool) = mechanics_pool_or_skip(
            "compute_context_centroids_buckets_deterministically_and_matches_the_known_mean",
        )
        .await
        else {
            return;
        };

        // multi_genre: 4 distinct library items. Embed all four with fixed,
        // distinct vectors.
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

        // Two finished sessions bucket into "weekend_evening" (items 0 and
        // 1: Saturday/Sunday evening), one into "weekday_morning" (item 2:
        // Wednesday morning). Item 3 gets no finished session at all, so it
        // must never contribute to any centroid.
        let now = Utc::now();
        let sessions: [(usize, i32, i32); 3] = [
            (0, 20, 6), // Saturday evening
            (1, 21, 0), // Sunday evening
            (2, 9, 3),  // Wednesday morning
        ];
        for (idx, hour, dow) in sessions {
            let item_id = loaded.items[idx].1.id;
            repo::play_session::upsert(
                &pool,
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

        let first = profile::compute_context_centroids(&pool, loaded.account.id)
            .await
            .expect("context centroids #1");
        let second = profile::compute_context_centroids(&pool, loaded.account.id)
            .await
            .expect("context centroids #2");

        let mut keys1: Vec<String> = first.iter().map(|c| c.context_key.clone()).collect();
        let mut keys2: Vec<String> = second.iter().map(|c| c.context_key.clone()).collect();
        keys1.sort();
        keys2.sort();
        assert_eq!(
            keys1, keys2,
            "compute_context_centroids must bucket into the same set of context keys every run"
        );
        assert_eq!(
            keys1,
            vec!["weekday_morning".to_string(), "weekend_evening".to_string()],
            "sessions must land in exactly the expected two buckets"
        );

        // Bit-identical centroids per bucket across both runs.
        for c1 in &first {
            let c2 = second
                .iter()
                .find(|c| c.context_key == c1.context_key)
                .expect("the same bucket must exist in both runs");
            assert_vectors_bit_identical(
                &c1.centroid,
                &c2.centroid,
                &format!("context centroid '{}'", c1.context_key),
            );
        }

        // Correctness: "weekend_evening" must be the exact UNWEIGHTED mean
        // of items 0 and 1's fixed vectors (per profile.rs's own doc
        // comment: context centroids are unweighted within a bucket), with
        // sample_size 2; "weekday_morning" is item 2 alone.
        let weekend = first
            .iter()
            .find(|c| c.context_key == "weekend_evening")
            .expect("weekend_evening bucket must exist");
        assert_eq!(weekend.sample_size, 2);
        let expected_weekend_mean: Vec<f32> = fixed_vector(3.0)
            .iter()
            .zip(fixed_vector(4.0).iter())
            .map(|(a, b)| (a + b) / 2.0)
            .collect();
        assert_eq!(
            weekend.centroid.as_slice(),
            expected_weekend_mean.as_slice(),
            "weekend_evening centroid must be the exact unweighted mean of items 0 and 1's vectors"
        );

        let weekday = first
            .iter()
            .find(|c| c.context_key == "weekday_morning")
            .expect("weekday_morning bucket must exist");
        assert_eq!(weekday.sample_size, 1);
        assert_eq!(
            weekday.centroid.as_slice(),
            fixed_vector(5.0).as_slice(),
            "a single-sample bucket's centroid must be exactly that sample's vector"
        );

        loader::cleanup(&pool, &loaded).await.ok();
    }
}
