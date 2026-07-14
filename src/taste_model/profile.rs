//! Profile aggregation: fold `taste_signals` into `taste_profile`'s affinity
//! maps + the recency/rewatch-weighted embedding centroid, and bucket
//! finished `play_sessions` into `taste_context_centroids` (spec §3.4,
//! MUSE-10).
//!
//! ## Documented schema divergence — decade affinity
//! The founding spec's MUSE-10 build brief calls for weighting
//! "genres/people/decades/keywords", but the MUSE-03 `taste_profile` table
//! (`migrations/0019_taste_profile.sql`) has no dedicated decade column —
//! only `genre_affinity`, `person_affinity`, `keyword_affinity`,
//! `runtime_pref`, `quality_sensitivity`, `overall_centroid`, `model_notes`.
//! Rather than add a migration for one extra jsonb column (the build brief
//! explicitly prefers no new migration when the MUSE-03 tables already
//! suffice), decade affinity is nested inside `keyword_affinity` as a
//! sibling key: `{"keywords": {...}, "decades": {...}}`. This keeps every
//! computed dimension persisted without widening the schema, at the cost of
//! `keyword_affinity` no longer being a flat `{keyword: weight}` map — any
//! future reader of this column needs to know about the `keywords`/
//! `decades` split. If a dedicated `decade_affinity` column is ever added,
//! this is the one place that needs to change.

use std::collections::{BTreeMap, HashMap};

use chrono::{DateTime, Utc};
use pgvector::Vector;
use serde_json::{json, Value as Json};
use sqlx::PgPool;

use crate::error::MuseResult;
use crate::models::embedding::{EmbeddingEntityKind, DEFAULT_EMBEDDING_MODEL, EMBEDDING_DIM};
use crate::models::taste::NewTasteContextCentroid;
use crate::repo;

use super::signals::recency_weight;

/// Runtime-preference buckets (spec: "distribution of finished runtimes
/// (phone vs TV)") — deliberately coarse for v0.
const RUNTIME_SHORT_MAX_MINUTES: i32 = 40;
const RUNTIME_MEDIUM_MAX_MINUTES: i32 = 90;

// --- pure aggregation formula (unit-testable without a database) ----------

/// Recency-weighted sum aggregation: given rows of `(key, raw_weight,
/// observed_at)`, sum `raw_weight * recency_weight(observed_at, now,
/// half_life_days)` per key. This is the one formula every affinity
/// dimension (genre/person/decade/keyword) below is built from — only the
/// SQL join producing the input rows differs per dimension.
pub fn aggregate_weighted<K: Ord + Clone>(
    rows: &[(K, f32, DateTime<Utc>)],
    now: DateTime<Utc>,
    half_life_days: f64,
) -> BTreeMap<K, f64> {
    let mut totals: BTreeMap<K, f64> = BTreeMap::new();
    for (key, weight, observed_at) in rows {
        let decayed = *weight as f64 * recency_weight(*observed_at, now, half_life_days);
        *totals.entry(key.clone()).or_insert(0.0) += decayed;
    }
    totals
}

/// Bucket a `(started_hour, started_dow)` pair into a taste-context key
/// (spec examples: `'weekend_evening'`, `'weekday_late'`, `'phone_short'` —
/// this covers the weekend/weekday x time-of-day axis from
/// `play_sessions.started_hour`/`started_dow`; a device-based axis like
/// `'phone_short'` would need `play_sessions.is_cinema_context`, which is
/// left for a later iteration). Returns `None` when either field is
/// missing — an unresolved session's context can't be bucketed.
///
/// `started_dow` follows the spec's `0-6` convention (0 = Sunday, 6 =
/// Saturday, matching Postgres's own `EXTRACT(DOW ...)`), so the weekend
/// set is `{0, 6}`.
pub fn context_key_for(started_hour: Option<i32>, started_dow: Option<i32>) -> Option<String> {
    let hour = started_hour?;
    let dow = started_dow?;
    let is_weekend = matches!(dow, 0 | 6);
    let time_bucket = match hour {
        5..=11 => "morning",
        12..=16 => "daytime",
        17..=21 => "evening",
        _ => "late_night", // 22-23, 0-4
    };
    Some(format!("{}_{}", if is_weekend { "weekend" } else { "weekday" }, time_bucket))
}

fn share_map_to_json<K: ToString>(map: &BTreeMap<K, f64>) -> Json {
    let obj: serde_json::Map<String, Json> = map
        .iter()
        .map(|(k, v)| (k.to_string(), json!(v)))
        .collect();
    Json::Object(obj)
}

// --- DB-backed aggregation ---------------------------------------------------

/// Recency-weighted genre affinity map (`{genre: weight}`), from
/// `repo::taste::signal_genre_rows`.
pub async fn compute_genre_affinity(
    pool: &PgPool,
    account_id: i64,
    now: DateTime<Utc>,
    half_life_days: f64,
) -> MuseResult<Json> {
    let rows = repo::taste::signal_genre_rows(pool, account_id).await?;
    let pairs: Vec<(String, f32, DateTime<Utc>)> =
        rows.into_iter().map(|r| (r.genre, r.weight, r.observed_at)).collect();
    Ok(share_map_to_json(&aggregate_weighted(&pairs, now, half_life_days)))
}

/// Recency-weighted person affinity map (`{person_id: weight}` — jsonb keys
/// are always strings, so `person_id` is stringified), from
/// `repo::taste::signal_person_rows`.
pub async fn compute_person_affinity(
    pool: &PgPool,
    account_id: i64,
    now: DateTime<Utc>,
    half_life_days: f64,
) -> MuseResult<Json> {
    let rows = repo::taste::signal_person_rows(pool, account_id).await?;
    let pairs: Vec<(i64, f32, DateTime<Utc>)> =
        rows.into_iter().map(|r| (r.person_id, r.weight, r.observed_at)).collect();
    Ok(share_map_to_json(&aggregate_weighted(&pairs, now, half_life_days)))
}

/// Combined `keyword_affinity` column value: `{"keywords": {...}, "decades":
/// {...}}` — see the module doc comment for why decades live here instead
/// of a dedicated column.
pub async fn compute_keyword_affinity(
    pool: &PgPool,
    account_id: i64,
    now: DateTime<Utc>,
    half_life_days: f64,
) -> MuseResult<Json> {
    let keyword_rows = repo::taste::signal_keyword_rows(pool, account_id).await?;
    let keyword_pairs: Vec<(String, f32, DateTime<Utc>)> = keyword_rows
        .into_iter()
        .map(|r| (r.keyword, r.weight, r.observed_at))
        .collect();
    let keywords = aggregate_weighted(&keyword_pairs, now, half_life_days);

    let decade_rows = repo::taste::signal_decade_rows(pool, account_id).await?;
    let decade_pairs: Vec<(i32, f32, DateTime<Utc>)> = decade_rows
        .into_iter()
        .map(|r| (r.decade, r.weight, r.observed_at))
        .collect();
    let decades = aggregate_weighted(&decade_pairs, now, half_life_days);

    Ok(json!({
        "keywords": share_map_to_json(&keywords),
        "decades": share_map_to_json(&decades),
    }))
}

/// Coarse runtime-preference distribution over *finished* titles (spec:
/// "distribution of finished runtimes (phone vs TV)"), bucketed
/// short/medium/long and weighted by recency (using each title's
/// `watch_stats.last_watched_at`). Returns `None` when the account has no
/// finished titles with a known runtime — nothing to build a distribution
/// from.
pub async fn compute_runtime_pref(
    pool: &PgPool,
    account_id: i64,
    now: DateTime<Utc>,
    half_life_days: f64,
) -> MuseResult<Option<Json>> {
    let stats = repo::watch_stats::list_watch_stats_for_account(pool, account_id).await?;
    let finished_ids: Vec<i64> = stats
        .iter()
        .filter(|s| s.finished_count > 0)
        .map(|s| s.media_item_id)
        .collect();
    if finished_ids.is_empty() {
        return Ok(None);
    }

    let runtimes = repo::media_item::runtimes_for_media_items(pool, &finished_ids).await?;
    let runtime_by_id: HashMap<i64, i32> = runtimes.into_iter().collect();

    let mut buckets: BTreeMap<&'static str, f64> = BTreeMap::new();
    for s in stats.iter().filter(|s| s.finished_count > 0) {
        let Some(runtime) = runtime_by_id.get(&s.media_item_id).copied() else {
            continue; // no runtime on record; skip cleanly
        };
        let bucket = if runtime <= RUNTIME_SHORT_MAX_MINUTES {
            "short"
        } else if runtime <= RUNTIME_MEDIUM_MAX_MINUTES {
            "medium"
        } else {
            "long"
        };
        let observed_at = s.last_watched_at.unwrap_or(now);
        let w = recency_weight(observed_at, now, half_life_days);
        *buckets.entry(bucket).or_insert(0.0) += w;
    }

    if buckets.is_empty() {
        return Ok(None);
    }
    Ok(Some(share_map_to_json(&buckets)))
}

/// Recency-and-rewatch-weighted mean embedding of finished titles —
/// `taste_profile.overall_centroid` (spec: "centroid of loved items"). A
/// finished title with no stored MUSE-08 embedding yet is skipped cleanly
/// (per the MUSE-10 build brief) rather than failing the whole recompute.
/// Returns `None` when there's nothing to average (no finished titles, or
/// none of them have an embedding yet).
pub async fn compute_overall_centroid(
    pool: &PgPool,
    account_id: i64,
    now: DateTime<Utc>,
    half_life_days: f64,
) -> MuseResult<Option<Vector>> {
    let stats = repo::watch_stats::list_watch_stats_for_account(pool, account_id).await?;
    let finished: Vec<&crate::models::watch_stats::WatchStats> =
        stats.iter().filter(|s| s.finished_count > 0).collect();
    if finished.is_empty() {
        return Ok(None);
    }

    let ids: Vec<i64> = finished.iter().map(|s| s.media_item_id).collect();
    let embeddings = repo::embedding::get_many(
        pool,
        EmbeddingEntityKind::MediaItem.as_str(),
        DEFAULT_EMBEDDING_MODEL,
        &ids,
    )
    .await?;
    let by_id: HashMap<i64, &crate::models::embedding::Embedding> =
        embeddings.iter().map(|e| (e.entity_id, e)).collect();

    let mut sum = vec![0.0f64; EMBEDDING_DIM as usize];
    let mut weight_total = 0.0f64;

    for s in finished {
        let Some(embedding) = by_id.get(&s.media_item_id) else {
            continue; // no embedding yet -- skip cleanly, per the build brief
        };
        let observed_at = s.last_watched_at.unwrap_or(now);
        // Recency decay, boosted for titles rewatched more (a rewatched
        // favorite should pull the centroid harder than a one-time finish).
        let w = recency_weight(observed_at, now, half_life_days) * (1.0 + s.rewatch_count as f64 * 0.5);
        if w <= 0.0 {
            continue;
        }
        weight_total += w;
        for (i, v) in embedding.embedding.as_slice().iter().enumerate() {
            if let Some(slot) = sum.get_mut(i) {
                *slot += (*v as f64) * w;
            }
        }
    }

    if weight_total <= 0.0 {
        return Ok(None);
    }

    let mean: Vec<f32> = sum.iter().map(|v| (v / weight_total) as f32).collect();
    Ok(Some(Vector::from(mean)))
}

/// Bucket finished `play_sessions` into weekend/weekday x time-of-day
/// contexts ([`context_key_for`]) and average each bucket's embeddings
/// (unweighted mean within a bucket — the recency/rewatch weighting already
/// lives in [`compute_overall_centroid`]; a context centroid is meant to
/// answer "what does this account like in this context", not "what have
/// they liked *recently* in this context"). A bucket with no embeddable
/// titles is skipped (no zero-sample row is written).
pub async fn compute_context_centroids(
    pool: &PgPool,
    account_id: i64,
) -> MuseResult<Vec<NewTasteContextCentroid>> {
    let rows = repo::play_session::list_finished_context_rows(pool, account_id).await?;

    let mut by_bucket: BTreeMap<String, Vec<i64>> = BTreeMap::new();
    for row in &rows {
        if let Some(key) = context_key_for(row.started_hour, row.started_dow) {
            by_bucket.entry(key).or_default().push(row.media_item_id);
        }
    }

    let all_ids: Vec<i64> = by_bucket.values().flatten().copied().collect();
    if all_ids.is_empty() {
        return Ok(Vec::new());
    }

    let embeddings = repo::embedding::get_many(
        pool,
        EmbeddingEntityKind::MediaItem.as_str(),
        DEFAULT_EMBEDDING_MODEL,
        &all_ids,
    )
    .await?;
    let by_id: HashMap<i64, &crate::models::embedding::Embedding> =
        embeddings.iter().map(|e| (e.entity_id, e)).collect();

    let mut out = Vec::new();
    for (context_key, media_item_ids) in by_bucket {
        let mut sum = vec![0.0f64; EMBEDDING_DIM as usize];
        let mut n: i32 = 0;
        for media_item_id in &media_item_ids {
            let Some(embedding) = by_id.get(media_item_id) else { continue };
            n += 1;
            for (i, v) in embedding.embedding.as_slice().iter().enumerate() {
                if let Some(slot) = sum.get_mut(i) {
                    *slot += *v as f64;
                }
            }
        }
        if n == 0 {
            continue; // no embeddable titles in this bucket -- skip cleanly
        }
        let mean: Vec<f32> = sum.iter().map(|v| (v / n as f64) as f32).collect();
        out.push(NewTasteContextCentroid {
            account_id,
            context_key,
            centroid: Vector::from(mean),
            sample_size: media_item_ids.len() as i32,
        });
    }

    Ok(out)
}

/// Deterministic unweighted mean embedding over a set of entity ids: sums
/// each id's embedding (looked up in `by_id`) in ascending-id order --
/// `ids` is sorted (and deduped) internally before summing, so the result
/// never depends on the order `ids` happens to be passed in, nor on
/// `by_id`'s `HashMap` iteration order (only used for lookup, never
/// iterated) -- and divides by the count of ids that actually had a stored
/// embedding. Added for MUSEX-02 (Plane TERM #378) persona derivation
/// (`crate::persona::derive`), which needs the exact "average these
/// titles' embeddings" primitive [`compute_context_centroids`] already
/// contains inline; factored out here as a new, additive function so both
/// call sites share it without duplicating the summation logic.
/// Returns `None` when none of `ids` have a stored embedding.
pub fn mean_embedding(
    ids: &[i64],
    by_id: &HashMap<i64, &crate::models::embedding::Embedding>,
) -> Option<Vector> {
    let mut sorted_ids = ids.to_vec();
    sorted_ids.sort_unstable();
    sorted_ids.dedup();

    let mut sum = vec![0.0f64; EMBEDDING_DIM as usize];
    let mut n: i32 = 0;
    for media_item_id in &sorted_ids {
        let Some(embedding) = by_id.get(media_item_id) else {
            continue; // no embedding yet -- skip cleanly
        };
        n += 1;
        for (i, v) in embedding.embedding.as_slice().iter().enumerate() {
            if let Some(slot) = sum.get_mut(i) {
                *slot += *v as f64;
            }
        }
    }

    if n == 0 {
        return None;
    }
    let mean: Vec<f32> = sum.iter().map(|v| (v / n as f64) as f32).collect();
    Some(Vector::from(mean))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aggregate_weighted_sums_multiple_signals_for_the_same_key() {
        let now = Utc::now();
        let rows = vec![
            ("scifi".to_string(), 1.0f32, now),
            ("scifi".to_string(), 2.5f32, now),
            ("horror".to_string(), -1.5f32, now),
        ];
        let totals = aggregate_weighted(&rows, now, DEFAULT_HALF_LIFE_DAYS_FOR_TEST);
        assert!((totals["scifi"] - 3.5).abs() < 1e-9);
        assert!((totals["horror"] + 1.5).abs() < 1e-9);
    }

    #[test]
    fn aggregate_weighted_applies_recency_decay() {
        let now = Utc::now();
        let old = now - chrono::Duration::days(180);
        let rows = vec![("scifi".to_string(), 1.0f32, old)];
        let totals = aggregate_weighted(&rows, now, 180.0);
        assert!((totals["scifi"] - 0.5).abs() < 1e-6, "expected ~0.5 after one half-life, got {}", totals["scifi"]);
    }

    #[test]
    fn abandonment_lowers_a_genre_relative_to_a_clean_finish() {
        let now = Utc::now();
        let finished_only = vec![("scifi".to_string(), 1.0f32, now)];
        let finished_then_abandoned_elsewhere = vec![
            ("scifi".to_string(), 1.0f32, now),
            ("scifi".to_string(), -1.5f32, now),
        ];

        let clean = aggregate_weighted(&finished_only, now, DEFAULT_HALF_LIFE_DAYS_FOR_TEST);
        let with_abandon = aggregate_weighted(&finished_then_abandoned_elsewhere, now, DEFAULT_HALF_LIFE_DAYS_FOR_TEST);

        assert!(
            with_abandon["scifi"] < clean["scifi"],
            "abandonment should lower the genre's aggregate weight: {} vs {}",
            with_abandon["scifi"],
            clean["scifi"]
        );
    }

    #[test]
    fn rewatch_dominates_relative_to_a_single_finish() {
        let now = Utc::now();
        let single_finish = vec![("comfort".to_string(), 1.0f32, now)];
        let rewatched = vec![("comfort".to_string(), 1.0f32, now), ("comfort".to_string(), 2.5f32 * 3.0, now)];

        let clean = aggregate_weighted(&single_finish, now, DEFAULT_HALF_LIFE_DAYS_FOR_TEST);
        let with_rewatch = aggregate_weighted(&rewatched, now, DEFAULT_HALF_LIFE_DAYS_FOR_TEST);

        assert!(with_rewatch["comfort"] > clean["comfort"] * 5.0, "rewatch should dominate the aggregate");
    }

    #[test]
    fn context_key_for_buckets_weekend_and_weekday_correctly() {
        assert_eq!(context_key_for(Some(20), Some(6)), Some("weekend_evening".to_string())); // Saturday evening
        assert_eq!(context_key_for(Some(20), Some(0)), Some("weekend_evening".to_string())); // Sunday evening
        assert_eq!(context_key_for(Some(20), Some(3)), Some("weekday_evening".to_string())); // Wednesday evening
        assert_eq!(context_key_for(Some(9), Some(3)), Some("weekday_morning".to_string()));
        assert_eq!(context_key_for(Some(1), Some(3)), Some("weekday_late_night".to_string()));
    }

    #[test]
    fn context_key_for_returns_none_when_fields_missing() {
        assert_eq!(context_key_for(None, Some(3)), None);
        assert_eq!(context_key_for(Some(9), None), None);
    }

    #[test]
    fn share_map_to_json_round_trips_a_map() {
        let mut map = BTreeMap::new();
        map.insert("scifi".to_string(), 1.5f64);
        let value = share_map_to_json(&map);
        assert_eq!(value["scifi"].as_f64(), Some(1.5));
    }

    // Local alias purely to make the "same half-life for both sides of a
    // comparison" intent explicit in the tests above without repeating the
    // magic number inline everywhere.
    const DEFAULT_HALF_LIFE_DAYS_FOR_TEST: f64 = super::super::signals::DEFAULT_HALF_LIFE_DAYS;

    // --- mean_embedding: pure, DB-free determinism coverage ----------------
    //
    // Added for MUSEX-02 (persona derivation reuses this helper). Runs
    // unconditionally (no MUSE_TEST_DATABASE_URL needed) -- same posture as
    // every other pure-math test in this module.

    fn fake_embedding(entity_id: i64, values: Vec<f32>) -> crate::models::embedding::Embedding {
        assert_eq!(
            values.len(),
            EMBEDDING_DIM as usize,
            "test fixture must be full-width"
        );
        crate::models::embedding::Embedding {
            id: entity_id,
            entity_kind: "media_item".to_string(),
            entity_id,
            model: DEFAULT_EMBEDDING_MODEL.to_string(),
            dim: EMBEDDING_DIM,
            embedding: Vector::from(values),
            source_text: None,
            embedded_at: Utc::now(),
        }
    }

    /// A deterministic, purely-computed EMBEDDING_DIM-length vector -- same
    /// "fixed synthetic embedding" idiom as `taste_mechanics_tests`'s
    /// `fixed_vector`, duplicated locally (rather than made `pub(crate)`
    /// and imported) so this module's tests don't take on a cross-module
    /// test-only dependency for one helper function.
    fn fixed_vector(seed: f32) -> Vec<f32> {
        (0..EMBEDDING_DIM as usize)
            .map(|i| ((i as f32) * 0.01 + seed).sin())
            .collect()
    }

    #[test]
    fn mean_embedding_returns_none_for_ids_with_no_stored_embedding() {
        let by_id: HashMap<i64, &crate::models::embedding::Embedding> = HashMap::new();
        assert!(mean_embedding(&[1, 2, 3], &by_id).is_none());
    }

    #[test]
    fn mean_embedding_skips_missing_ids_and_averages_only_present_ones() {
        let e1 = fake_embedding(1, fixed_vector(0.1));
        let mut by_id: HashMap<i64, &crate::models::embedding::Embedding> = HashMap::new();
        by_id.insert(1, &e1);
        // id 2 has no entry in by_id -- must be skipped, not treated as zero.
        let mean = mean_embedding(&[1, 2], &by_id).expect("one embeddable id is enough");
        assert_eq!(mean.as_slice(), Vector::from(fixed_vector(0.1)).as_slice());
    }

    /// The negative-nondeterminism guard for MUSEX-02's persona derivation:
    /// `mean_embedding` must produce a BIT-IDENTICAL vector no matter what
    /// order its `ids` slice is passed in, and no matter what order `by_id`
    /// (a `HashMap`) happens to iterate -- this is exactly the class of bug
    /// `taste_mechanics_tests`'s `dedup_then_rank_output_order_is_not_guaranteed_deterministic_for_tied_scores`
    /// documents for `dedup_candidates`'s `HashMap`-collect (MUSET-05). If a
    /// future edit made `mean_embedding` sum over `ids` (or over `by_id`)
    /// without sorting first, floating-point addition's non-associativity
    /// would make this test flaky/fail.
    #[test]
    fn mean_embedding_is_order_independent_and_bit_deterministic() {
        let e1 = fake_embedding(1, fixed_vector(0.1));
        let e2 = fake_embedding(2, fixed_vector(0.7));
        let e3 = fake_embedding(3, fixed_vector(1.3));
        let mut by_id: HashMap<i64, &crate::models::embedding::Embedding> = HashMap::new();
        by_id.insert(1, &e1);
        by_id.insert(2, &e2);
        by_id.insert(3, &e3);

        let forward = mean_embedding(&[1, 2, 3], &by_id).expect("all three ids are embeddable");
        let reversed = mean_embedding(&[3, 2, 1], &by_id).expect("all three ids are embeddable");
        let shuffled = mean_embedding(&[2, 3, 1], &by_id).expect("all three ids are embeddable");
        let with_duplicate = mean_embedding(&[1, 2, 3, 2], &by_id)
            .expect("a duplicate id must be deduped, not double-counted");

        assert_eq!(
            forward.as_slice(),
            reversed.as_slice(),
            "mean_embedding must be bit-identical regardless of input id order"
        );
        assert_eq!(forward.as_slice(), shuffled.as_slice());
        assert_eq!(
            forward.as_slice(),
            with_duplicate.as_slice(),
            "a duplicate id in the input must not skew the mean"
        );

        // Re-running twice more with the same inputs must also stay
        // bit-identical -- this is the "same inputs -> same persona
        // vectors" determinism AC itself, at the primitive level.
        let again = mean_embedding(&[1, 2, 3], &by_id).expect("all three ids are embeddable");
        assert_eq!(forward.as_slice(), again.as_slice());
    }
}
