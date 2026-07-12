//! The taste-divergence math: account-vs-population genre/decade
//! over/under-index, `mainstream_score`, `adventurousness`,
//! `contrarian_index`, `were_early`, `blind_spots`, `guilty_pleasures`
//! (spec §3.7/§4c, MUSE-20).
//!
//! Formulas are kept deliberately simple and explainable (this feeds
//! proactive "you were early"/"worth the hype?" messages — an operator or
//! Lumina should be able to say *why* a number is what it is):
//!
//! - **genre/decade shares**: an account's or the population's weight for a
//!   genre/decade, normalized so all shares for that distribution sum to 1
//!   ([`normalize`]).
//! - **genre_index/decade_index**: `(account_share + ε) / (population_share
//!   + ε)` per key ([`index_map`]) — >1 over-indexed, <1 under-indexed. The
//!   epsilon avoids both a division-by-zero for a population-absent key and
//!   an unbounded ratio for an account-absent one.
//! - **mainstream_score**: histogram intersection (`Σ min(account_share,
//!   population_share)`) of the genre distribution, blended 70/30 with the
//!   decade distribution's intersection when one is available
//!   ([`overlap`], [`mainstream_score`]). 1.0 = your genre/decade footprint
//!   is indistinguishable from the mainstream's; 0.0 = totally disjoint.
//!   This is the spec's `mainstream_score` dimension — computed from
//!   distribution overlap rather than the spec's literal "cosine of
//!   centroids" because this crate has no embeddings pipeline wired up yet
//!   (see `migrations/0044_taste_divergence.sql`'s divergence note).
//! - **adventurousness**: `1 - mainstream_score` ([`adventurousness`]) —
//!   the simplest legible complement.
//! - **contrarian_index**: derived from the Pearson correlation between the
//!   account's and population's genre shares ([`pearson_correlation`],
//!   [`contrarian_index`]) — a *negative* correlation (into what the masses
//!   aren't) scores high, distinguishing "your favorite niche genre happens
//!   to be small" (low correlation magnitude, moderate contrarian_index)
//!   from "you actively watch the opposite of what's popular" (strong
//!   negative correlation, high contrarian_index).
//! - **were_early**: an account-watched title whose `first_watched_at`
//!   predates the population sample's earliest `trended_at` for that title
//!   ([`compute_were_early`]) — the taste-maker signal.
//! - **blind_spots**: population-sample titles (ranked by popularity) the
//!   account has never watched ([`compute_blind_spots`]).
//! - **guilty_pleasures**: account titles with `rewatch_count > 0` that
//!   aren't in the current trending population sample at all
//!   ([`compute_guilty_pleasures`]) — off the mainstream radar, but you
//!   keep going back.

use std::collections::{BTreeMap, BTreeSet, HashSet};

use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::Value as Json;
use sqlx::PgPool;

use crate::error::MuseResult;
use crate::models::taste_divergence::{NewTasteDivergence, TasteDivergence};
use crate::models::trending::{NewPopulationProfile, PopulationProfile};
use crate::repo;
use crate::repo::taste_divergence::{AccountWatchRow, PopulationSampleRow};

/// Epsilon added to both sides of the over/under-index ratio (see the
/// module doc comment) — small enough not to distort a genre either side
/// has real presence in, large enough to keep the ratio finite and legible
/// at the edges.
pub const EPSILON: f64 = 0.01;

/// How many entries each of `were_early`/`blind_spots`/`guilty_pleasures`
/// is capped to — these are meant to be the *interesting* highlights for a
/// proactive message, not an exhaustive dump.
pub const DIVERGENCE_LIST_LIMIT: usize = 10;

// --- pure formulas (unit-tested without a database) ------------------------

/// Normalize non-negative weights into shares summing to 1.0. Returns an
/// empty map when the input is empty or all weights are non-positive —
/// there's no meaningful distribution to normalize in that case, and an
/// empty map is a safe identity for [`index_map`]/[`overlap`] (an absent
/// key reads as 0 share on either side).
pub fn normalize(weights: &[(String, f64)]) -> BTreeMap<String, f64> {
    let total: f64 = weights.iter().map(|(_, w)| w.max(0.0)).sum();
    if total <= 0.0 {
        return BTreeMap::new();
    }
    weights
        .iter()
        .map(|(k, w)| (k.clone(), w.max(0.0) / total))
        .collect()
}

/// Over/under-index of `account` vs `population` shares across the union of
/// their keys — see the module doc comment for the formula and rationale.
pub fn index_map(
    account: &BTreeMap<String, f64>,
    population: &BTreeMap<String, f64>,
) -> BTreeMap<String, f64> {
    union_keys(account, population)
        .into_iter()
        .map(|k| {
            let a = account.get(&k).copied().unwrap_or(0.0);
            let p = population.get(&k).copied().unwrap_or(0.0);
            (k, (a + EPSILON) / (p + EPSILON))
        })
        .collect()
}

/// Histogram intersection (`Σ min(account[k], population[k])`) over the
/// union of keys — 1.0 for identical distributions, 0.0 for totally
/// disjoint ones. Returns 0.0 if either distribution is empty (nothing to
/// overlap).
pub fn overlap(account: &BTreeMap<String, f64>, population: &BTreeMap<String, f64>) -> f64 {
    if account.is_empty() || population.is_empty() {
        return 0.0;
    }
    union_keys(account, population)
        .into_iter()
        .map(|k| {
            account
                .get(&k)
                .copied()
                .unwrap_or(0.0)
                .min(population.get(&k).copied().unwrap_or(0.0))
        })
        .sum()
}

fn union_keys(a: &BTreeMap<String, f64>, b: &BTreeMap<String, f64>) -> BTreeSet<String> {
    let mut keys: BTreeSet<String> = a.keys().cloned().collect();
    keys.extend(b.keys().cloned());
    keys
}

/// `mainstream_score` — genre overlap weighted 0.7, decade overlap weighted
/// 0.3 when a decade overlap is available (genre is the primary taste axis
/// in the spec's radar; decade distributions are sparser for a typical home
/// library, so the score falls back to genre-only when there's no usable
/// decade data on either side).
pub fn mainstream_score(genre_overlap: f64, decade_overlap: Option<f64>) -> f64 {
    match decade_overlap {
        Some(d) => (0.7 * genre_overlap + 0.3 * d).clamp(0.0, 1.0),
        None => genre_overlap.clamp(0.0, 1.0),
    }
}

/// `adventurousness` — the complement of [`mainstream_score`].
pub fn adventurousness(mainstream_score: f64) -> f64 {
    (1.0 - mainstream_score).clamp(0.0, 1.0)
}

/// Pearson correlation coefficient between two share distributions over
/// their shared key union (an absent key reads as 0 share on that side).
/// `None` when either side has zero variance across the union (e.g. an
/// empty distribution, or one where every key has the same share) — the
/// coefficient is undefined there, not zero.
pub fn pearson_correlation(
    account: &BTreeMap<String, f64>,
    population: &BTreeMap<String, f64>,
) -> Option<f64> {
    let keys = union_keys(account, population);
    let n = keys.len() as f64;
    if n < 2.0 {
        return None;
    }

    let xs: Vec<f64> = keys
        .iter()
        .map(|k| account.get(k).copied().unwrap_or(0.0))
        .collect();
    let ys: Vec<f64> = keys
        .iter()
        .map(|k| population.get(k).copied().unwrap_or(0.0))
        .collect();

    let mean_x = xs.iter().sum::<f64>() / n;
    let mean_y = ys.iter().sum::<f64>() / n;

    let mut cov = 0.0;
    let mut var_x = 0.0;
    let mut var_y = 0.0;
    for i in 0..xs.len() {
        let dx = xs[i] - mean_x;
        let dy = ys[i] - mean_y;
        cov += dx * dy;
        var_x += dx * dx;
        var_y += dy * dy;
    }

    if var_x <= 0.0 || var_y <= 0.0 {
        return None;
    }
    Some(cov / (var_x.sqrt() * var_y.sqrt()))
}

/// `contrarian_index` — `(1 - r) / 2` where `r` is [`pearson_correlation`]
/// between account and population genre shares: `r = 1` (perfectly aligned
/// with the mainstream) maps to 0.0; `r = -1` (into exactly what the masses
/// aren't) maps to 1.0; `r = 0` (uncorrelated) maps to 0.5. `None`
/// (undefined correlation) also maps to 0.5 — "unknown" is deliberately
/// treated as neutral, not as "aligned".
pub fn contrarian_index(r: Option<f64>) -> f64 {
    match r {
        Some(r) => ((1.0 - r.clamp(-1.0, 1.0)) / 2.0).clamp(0.0, 1.0),
        None => 0.5,
    }
}

/// One `were_early` entry — an account title watched before it showed up in
/// the trending population sample.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct WereEarlyEntry {
    pub media_metadata_id: i64,
    pub title: String,
    pub watched_at: DateTime<Utc>,
    pub trended_at: DateTime<Utc>,
    pub lead_days: i64,
}

/// One `blind_spots` entry — a popular trending title the account has never
/// watched.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct BlindSpotEntry {
    pub media_metadata_id: i64,
    pub title: String,
    pub best_rank: Option<i32>,
    pub popularity: Option<f32>,
}

/// One `guilty_pleasures` entry — a title the account rewatches that isn't
/// currently trending at all.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct GuiltyPleasureEntry {
    pub media_metadata_id: i64,
    pub title: String,
    pub rewatch_count: i32,
}

/// The taste-maker signal: account titles watched before their earliest
/// appearance in the trending population sample, sorted by lead time
/// (biggest "you called it" first), capped to [`DIVERGENCE_LIST_LIMIT`].
pub fn compute_were_early(
    account_rows: &[AccountWatchRow],
    population: &[PopulationSampleRow],
) -> Vec<WereEarlyEntry> {
    let mut entries: Vec<WereEarlyEntry> = account_rows
        .iter()
        .filter_map(|row| {
            let watched_at = row.first_watched_at?;
            let sample = population
                .iter()
                .find(|p| p.media_metadata_id == row.media_metadata_id)?;
            if watched_at >= sample.trended_at {
                return None;
            }
            Some(WereEarlyEntry {
                media_metadata_id: row.media_metadata_id,
                title: row.title.clone(),
                watched_at,
                trended_at: sample.trended_at,
                lead_days: (sample.trended_at - watched_at).num_days(),
            })
        })
        .collect();

    entries.sort_by(|a, b| b.lead_days.cmp(&a.lead_days));
    entries.truncate(DIVERGENCE_LIST_LIMIT);
    entries
}

/// Popular trending titles the account has never watched, ranked by
/// popularity (best rank first, falling back to raw popularity when rank is
/// missing on both sides), capped to [`DIVERGENCE_LIST_LIMIT`].
pub fn compute_blind_spots(
    account_rows: &[AccountWatchRow],
    population: &[PopulationSampleRow],
) -> Vec<BlindSpotEntry> {
    let watched: HashSet<i64> = account_rows.iter().map(|r| r.media_metadata_id).collect();

    let mut candidates: Vec<&PopulationSampleRow> = population
        .iter()
        .filter(|p| !watched.contains(&p.media_metadata_id))
        .collect();

    candidates.sort_by(|a, b| match (a.best_rank, b.best_rank) {
        (Some(ra), Some(rb)) => ra.cmp(&rb),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => b
            .popularity
            .partial_cmp(&a.popularity)
            .unwrap_or(std::cmp::Ordering::Equal),
    });

    candidates
        .into_iter()
        .take(DIVERGENCE_LIST_LIMIT)
        .map(|p| BlindSpotEntry {
            media_metadata_id: p.media_metadata_id,
            title: p.title.clone(),
            best_rank: p.best_rank,
            popularity: p.popularity,
        })
        .collect()
}

/// Account titles rewatched at least once that aren't in the current
/// trending population sample at all, ranked by rewatch count, capped to
/// [`DIVERGENCE_LIST_LIMIT`].
pub fn compute_guilty_pleasures(
    account_rows: &[AccountWatchRow],
    population: &[PopulationSampleRow],
) -> Vec<GuiltyPleasureEntry> {
    let trending: HashSet<i64> = population.iter().map(|p| p.media_metadata_id).collect();

    let mut entries: Vec<GuiltyPleasureEntry> = account_rows
        .iter()
        .filter(|r| r.rewatch_count > 0 && !trending.contains(&r.media_metadata_id))
        .map(|r| GuiltyPleasureEntry {
            media_metadata_id: r.media_metadata_id,
            title: r.title.clone(),
            rewatch_count: r.rewatch_count,
        })
        .collect();

    entries.sort_by(|a, b| b.rewatch_count.cmp(&a.rewatch_count));
    entries.truncate(DIVERGENCE_LIST_LIMIT);
    entries
}

fn json_to_share_map(value: &Json) -> BTreeMap<String, f64> {
    value
        .as_object()
        .map(|obj| {
            obj.iter()
                .filter_map(|(k, v)| v.as_f64().map(|f| (k.clone(), f)))
                .collect()
        })
        .unwrap_or_default()
}

fn is_empty_distribution(value: &Json) -> bool {
    value.as_object().map(|o| o.is_empty()).unwrap_or(true)
}

fn weights_to_pairs<T>(rows: &[T], key: impl Fn(&T) -> String, weight: impl Fn(&T) -> f64) -> Vec<(String, f64)> {
    rows.iter().map(|r| (key(r), weight(r))).collect()
}

// --- orchestration (needs a database) --------------------------------------

/// Compute and persist a fresh `population_profile` row with real
/// genre/decade distributions (populating the seams
/// `crate::trending::compute_population_profile` — MUSE-19 — deliberately
/// left empty/NULL). `runtime_distribution`/`mainstream_centroid` remain
/// untouched: the former isn't consumed by any `taste_divergence` dimension
/// yet, the latter needs an embeddings pipeline this crate doesn't have
/// (see the module doc comment).
pub async fn compute_population_distributions(
    pool: &PgPool,
    region: &str,
) -> MuseResult<PopulationProfile> {
    let genre_rows = repo::taste_divergence::population_genre_weights(pool, region).await?;
    let decade_rows = repo::taste_divergence::population_decade_weights(pool, region).await?;
    let sample_size = repo::taste_divergence::population_sample(pool, region).await?.len() as i32;

    let genre_shares = normalize(&weights_to_pairs(&genre_rows, |g| g.genre.clone(), |g| g.weight));
    let decade_shares = normalize(&weights_to_pairs(
        &decade_rows,
        |d| d.decade.to_string(),
        |d| d.weight,
    ));

    let genre_json = serde_json::to_value(&genre_shares).unwrap_or_else(|_| serde_json::json!({}));
    let decade_json = if decade_shares.is_empty() {
        None
    } else {
        Some(serde_json::to_value(&decade_shares).unwrap_or_else(|_| serde_json::json!({})))
    };

    repo::trending::insert_population_profile(
        pool,
        &NewPopulationProfile {
            window: "week".to_string(),
            region: region.to_string(),
            genre_distribution: genre_json,
            decade_distribution: decade_json,
            runtime_distribution: None,
            sample_size: Some(sample_size),
        },
    )
    .await
}

/// Recompute and persist a fresh `taste_divergence` row for `account_id`,
/// using [`crate::trending::DEFAULT_REGION`] as the population region.
pub async fn recompute_divergence(pool: &PgPool, account_id: i64) -> MuseResult<TasteDivergence> {
    recompute_divergence_for_region(pool, account_id, crate::trending::DEFAULT_REGION).await
}

/// Recompute and persist a fresh `taste_divergence` row for `account_id`
/// against a specific population `region`. Reuses the latest stored
/// `population_profile` for that region/window when it already carries a
/// real (non-empty) genre distribution; otherwise computes one fresh via
/// [`compute_population_distributions`] rather than reading MUSE-19's
/// placeholder `{}` distribution.
pub async fn recompute_divergence_for_region(
    pool: &PgPool,
    account_id: i64,
    region: &str,
) -> MuseResult<TasteDivergence> {
    let account_genre_rows = repo::taste_divergence::account_genre_weights(pool, account_id).await?;
    let account_decade_rows = repo::taste_divergence::account_decade_weights(pool, account_id).await?;
    let account_genre_shares = normalize(&weights_to_pairs(
        &account_genre_rows,
        |g| g.genre.clone(),
        |g| g.weight,
    ));
    let account_decade_shares = normalize(&weights_to_pairs(
        &account_decade_rows,
        |d| d.decade.to_string(),
        |d| d.weight,
    ));

    let population_profile = match repo::trending::latest_population_profile(pool, "week", region).await? {
        Some(p) if !is_empty_distribution(&p.genre_distribution) => p,
        _ => compute_population_distributions(pool, region).await?,
    };
    let population_genre_shares = json_to_share_map(&population_profile.genre_distribution);
    let population_decade_shares = population_profile
        .decade_distribution
        .as_ref()
        .map(json_to_share_map)
        .unwrap_or_default();

    let genre_index = index_map(&account_genre_shares, &population_genre_shares);
    let decade_index = if account_decade_shares.is_empty() && population_decade_shares.is_empty() {
        None
    } else {
        Some(index_map(&account_decade_shares, &population_decade_shares))
    };

    let genre_overlap = overlap(&account_genre_shares, &population_genre_shares);
    let decade_overlap = if population_decade_shares.is_empty() {
        None
    } else {
        Some(overlap(&account_decade_shares, &population_decade_shares))
    };
    let m_score = mainstream_score(genre_overlap, decade_overlap);
    let adv = adventurousness(m_score);
    let r = pearson_correlation(&account_genre_shares, &population_genre_shares);
    let contrarian = contrarian_index(r);

    let account_watch_rows = repo::taste_divergence::account_watch_rows(pool, account_id).await?;
    let population_sample = repo::taste_divergence::population_sample(pool, region).await?;

    let were_early = compute_were_early(&account_watch_rows, &population_sample);
    let blind_spots = compute_blind_spots(&account_watch_rows, &population_sample);
    let guilty_pleasures = compute_guilty_pleasures(&account_watch_rows, &population_sample);

    let new = NewTasteDivergence {
        account_id,
        genre_index: serde_json::to_value(&genre_index).unwrap_or_else(|_| serde_json::json!({})),
        decade_index: decade_index
            .map(|m| serde_json::to_value(&m).unwrap_or_else(|_| serde_json::json!({}))),
        mainstream_score: Some(m_score as f32),
        adventurousness: Some(adv as f32),
        contrarian_index: Some(contrarian as f32),
        were_early: serde_json::to_value(&were_early).unwrap_or_else(|_| serde_json::json!([])),
        blind_spots: serde_json::to_value(&blind_spots).unwrap_or_else(|_| serde_json::json!([])),
        guilty_pleasures: serde_json::to_value(&guilty_pleasures)
            .unwrap_or_else(|_| serde_json::json!([])),
    };

    repo::taste_divergence::insert_divergence(pool, &new).await
}

/// Read the most recent radar snapshot for an account, if any has been
/// computed yet.
pub async fn latest_divergence(pool: &PgPool, account_id: i64) -> MuseResult<Option<TasteDivergence>> {
    repo::taste_divergence::latest_divergence(pool, account_id).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shares(pairs: &[(&str, f64)]) -> BTreeMap<String, f64> {
        pairs.iter().map(|(k, v)| (k.to_string(), *v)).collect()
    }

    #[test]
    fn normalize_sums_to_one() {
        let weights = vec![
            ("scifi".to_string(), 3.0),
            ("horror".to_string(), 1.0),
        ];
        let shares = normalize(&weights);
        assert!((shares.values().sum::<f64>() - 1.0).abs() < 1e-9);
        assert!((shares["scifi"] - 0.75).abs() < 1e-9);
        assert!((shares["horror"] - 0.25).abs() < 1e-9);
    }

    #[test]
    fn normalize_empty_or_zero_weights_yields_empty_map() {
        assert!(normalize(&[]).is_empty());
        assert!(normalize(&[("scifi".to_string(), 0.0)]).is_empty());
    }

    #[test]
    fn index_map_over_and_under_index() {
        // Account is 3x as into scifi as the population, and never
        // touches romance (which the population loves).
        let account = shares(&[("scifi", 0.9), ("horror", 0.1)]);
        let population = shares(&[("scifi", 0.3), ("romance", 0.7)]);

        let idx = index_map(&account, &population);

        assert!(idx["scifi"] > 1.0, "over-indexed genre should be > 1.0, got {}", idx["scifi"]);
        assert!(idx["romance"] < 1.0, "account-absent genre should be < 1.0, got {}", idx["romance"]);
        assert!(idx["horror"] > 1.0, "population-absent genre should be > 1.0, got {}", idx["horror"]);
    }

    #[test]
    fn overlap_identical_distributions_is_one() {
        let dist = shares(&[("scifi", 0.6), ("horror", 0.4)]);
        assert!((overlap(&dist, &dist) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn overlap_disjoint_distributions_is_zero() {
        let account = shares(&[("scifi", 1.0)]);
        let population = shares(&[("romance", 1.0)]);
        assert_eq!(overlap(&account, &population), 0.0);
    }

    #[test]
    fn overlap_partial_matches_expected_intersection() {
        let account = shares(&[("scifi", 0.5), ("horror", 0.5)]);
        let population = shares(&[("scifi", 0.2), ("horror", 0.8)]);
        // min(0.5,0.2) + min(0.5,0.8) = 0.2 + 0.5 = 0.7
        assert!((overlap(&account, &population) - 0.7).abs() < 1e-9);
    }

    #[test]
    fn mainstream_score_blends_genre_and_decade_when_both_present() {
        let score = mainstream_score(1.0, Some(0.0));
        assert!((score - 0.7).abs() < 1e-9);
    }

    #[test]
    fn mainstream_score_falls_back_to_genre_only() {
        assert_eq!(mainstream_score(0.42, None), 0.42);
    }

    #[test]
    fn adventurousness_is_complement_of_mainstream_score() {
        assert!((adventurousness(0.3) - 0.7).abs() < 1e-9);
        assert_eq!(adventurousness(1.5), 0.0); // clamped
        assert_eq!(adventurousness(-0.5), 1.0); // clamped
    }

    #[test]
    fn pearson_correlation_perfect_alignment_is_one() {
        let account = shares(&[("scifi", 0.6), ("horror", 0.4)]);
        let population = shares(&[("scifi", 0.6), ("horror", 0.4)]);
        let r = pearson_correlation(&account, &population).expect("defined correlation");
        assert!((r - 1.0).abs() < 1e-6);
    }

    #[test]
    fn pearson_correlation_inverse_is_negative_one() {
        let account = shares(&[("scifi", 0.9), ("romance", 0.1)]);
        let population = shares(&[("scifi", 0.1), ("romance", 0.9)]);
        let r = pearson_correlation(&account, &population).expect("defined correlation");
        assert!((r + 1.0).abs() < 1e-6, "expected r near -1.0, got {r}");
    }

    #[test]
    fn pearson_correlation_undefined_for_too_little_data() {
        let account = shares(&[("scifi", 1.0)]);
        assert!(pearson_correlation(&account, &BTreeMap::new()).is_none());
    }

    #[test]
    fn contrarian_index_maps_correlation_to_0_1_range() {
        assert!((contrarian_index(Some(1.0)) - 0.0).abs() < 1e-9);
        assert!((contrarian_index(Some(-1.0)) - 1.0).abs() < 1e-9);
        assert!((contrarian_index(Some(0.0)) - 0.5).abs() < 1e-9);
        assert!((contrarian_index(None) - 0.5).abs() < 1e-9);
    }

    fn watch_row(media_metadata_id: i64, title: &str, first_watched_at: Option<DateTime<Utc>>, rewatch_count: i32) -> AccountWatchRow {
        AccountWatchRow {
            media_metadata_id,
            title: title.to_string(),
            first_watched_at,
            rewatch_count,
        }
    }

    fn sample_row(media_metadata_id: i64, title: &str, trended_at: DateTime<Utc>, popularity: Option<f32>, best_rank: Option<i32>) -> PopulationSampleRow {
        PopulationSampleRow {
            media_metadata_id,
            title: title.to_string(),
            trended_at,
            popularity,
            best_rank,
        }
    }

    #[test]
    fn were_early_detects_and_ranks_by_lead_time() {
        use chrono::TimeZone;

        let trended_at = Utc.with_ymd_and_hms(2026, 6, 1, 0, 0, 0).unwrap();
        let watched_early = Utc.with_ymd_and_hms(2026, 4, 1, 0, 0, 0).unwrap(); // 61 days early
        let watched_late = Utc.with_ymd_and_hms(2026, 5, 20, 0, 0, 0).unwrap(); // 12 days early
        let watched_after = Utc.with_ymd_and_hms(2026, 7, 1, 0, 0, 0).unwrap(); // after trending, not early

        let account_rows = vec![
            watch_row(1, "Early Pick", Some(watched_early), 0),
            watch_row(2, "Slightly Early", Some(watched_late), 0),
            watch_row(3, "Watched After Trending", Some(watched_after), 0),
            watch_row(4, "Never Watched", None, 0),
        ];
        let population = vec![
            sample_row(1, "Early Pick", trended_at, None, Some(5)),
            sample_row(2, "Slightly Early", trended_at, None, Some(3)),
            sample_row(3, "Watched After Trending", trended_at, None, Some(1)),
        ];

        let result = compute_were_early(&account_rows, &population);

        assert_eq!(result.len(), 2);
        assert_eq!(result[0].media_metadata_id, 1);
        assert_eq!(result[0].lead_days, 61);
        assert_eq!(result[1].media_metadata_id, 2);
        assert_eq!(result[1].lead_days, 12);
    }

    #[test]
    fn blind_spots_excludes_watched_titles_and_ranks_by_popularity() {
        use chrono::TimeZone;
        let now = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();

        let account_rows = vec![watch_row(1, "Already Watched", Some(now), 0)];
        let population = vec![
            sample_row(1, "Already Watched", now, None, Some(1)),
            sample_row(2, "Huge But Untouched", now, Some(500.0), Some(2)),
            sample_row(3, "Less Huge But Untouched", now, Some(100.0), Some(9)),
        ];

        let result = compute_blind_spots(&account_rows, &population);

        assert_eq!(result.len(), 2);
        assert_eq!(result[0].media_metadata_id, 2, "lower rank should sort first");
        assert_eq!(result[1].media_metadata_id, 3);
    }

    #[test]
    fn guilty_pleasures_requires_rewatch_and_absence_from_trending() {
        use chrono::TimeZone;
        let now = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();

        let account_rows = vec![
            watch_row(1, "Rewatched Niche Comfort Movie", Some(now), 5),
            watch_row(2, "Rewatched But Still Trending", Some(now), 3),
            watch_row(3, "Watched Once, Never Rewatched", Some(now), 0),
        ];
        let population = vec![sample_row(2, "Rewatched But Still Trending", now, None, Some(1))];

        let result = compute_guilty_pleasures(&account_rows, &population);

        assert_eq!(result.len(), 1);
        assert_eq!(result[0].media_metadata_id, 1);
        assert_eq!(result[0].rewatch_count, 5);
    }

    #[test]
    fn json_to_share_map_round_trips_numeric_object() {
        let value = serde_json::json!({"scifi": 0.6, "horror": 0.4});
        let map = json_to_share_map(&value);
        assert_eq!(map.len(), 2);
        assert!((map["scifi"] - 0.6).abs() < 1e-9);
    }

    #[test]
    fn is_empty_distribution_detects_muse19_placeholder() {
        assert!(is_empty_distribution(&serde_json::json!({})));
        assert!(!is_empty_distribution(&serde_json::json!({"scifi": 1.0})));
    }

    // --- live-DB round-trip test ---------------------------------------
    //
    // Gated on MUSE_TEST_DATABASE_URL: skips cleanly (does NOT fail) when
    // unset, per the MUSE-02 build constraint that the suite must pass with
    // no live database — same posture as `arr::ingest`'s and
    // `plex_control::repo`'s live-DB tests.
    #[tokio::test]
    async fn recompute_divergence_then_latest_divergence_round_trips() {
        let Ok(database_url) = std::env::var("MUSE_TEST_DATABASE_URL") else {
            eprintln!(
                "MUSE_TEST_DATABASE_URL not set — skipping recompute_divergence_then_latest_divergence_round_trips \
                 (this is expected in the default test run; the crate does not require a live DB)"
            );
            return;
        };

        use sqlx::postgres::PgPoolOptions;
        use uuid::Uuid;

        use crate::models::account::NewAccount;
        use crate::models::library::{LibraryKind, NewLibrary};
        use crate::models::media_item::NewMediaItem;
        use crate::models::media_metadata::{MediaKind, NewMediaMetadata};
        use crate::models::trending::NewTrendingSnapshot;
        use crate::models::watch_stats::NewWatchStats;

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
        let region = format!("XT-{suffix}"); // scoped to this test run's own region

        let account = repo::account::create(
            &pool,
            &NewAccount {
                plex_account_id: Some(format!("radar-test-account-{suffix}")),
                username: Some(format!("radar_test_{suffix}")),
                friendly_name: Some("Radar Test Account".to_string()),
                is_home_user: false,
                is_primary: false,
            },
        )
        .await
        .expect("create account");

        let library = repo::library::create(
            &pool,
            &NewLibrary {
                name: format!("radar-test-library-{suffix}"),
                kind: LibraryKind::Movie,
                root_folder: "/media/radar-test".to_string(),
                source_arr_name: None,
                source_arr_url: None,
            },
        )
        .await
        .expect("create library");

        // A niche genre the account is heavily into (rewatched) and a
        // mainstream genre the population is heavily into.
        let niche_genre_id: i64 = sqlx::query_scalar::<_, i64>(
            "INSERT INTO genres (name) VALUES ($1) RETURNING id",
        )
        .bind(format!("radar-niche-{suffix}"))
        .fetch_one(&pool)
        .await
        .expect("insert niche genre");

        let mainstream_genre_id: i64 = sqlx::query_scalar::<_, i64>(
            "INSERT INTO genres (name) VALUES ($1) RETURNING id",
        )
        .bind(format!("radar-mainstream-{suffix}"))
        .fetch_one(&pool)
        .await
        .expect("insert mainstream genre");

        // Account's own comfort rewatch: niche genre, never trending ->
        // should surface as a guilty pleasure.
        let comfort_metadata = repo::media_metadata::upsert_by_tmdb(
            &pool,
            &NewMediaMetadata {
                kind: MediaKind::Movie,
                tmdb_id: Some(format!("radar-comfort-{suffix}")),
                tvdb_id: None,
                imdb_id: None,
                provider_ids: serde_json::json!({}),
                title: format!("Radar Comfort Rewatch {suffix}"),
                sort_title: None,
                original_title: None,
                original_language: None,
                status: None,
                overview: None,
                studio: None,
                network: None,
                runtime_minutes: Some(100),
                year: Some(1994),
                images: serde_json::json!({}),
            },
        )
        .await
        .expect("upsert comfort media_metadata");

        sqlx::query("INSERT INTO media_metadata_genres (media_metadata_id, genre_id) VALUES ($1, $2)")
            .bind(comfort_metadata.id)
            .bind(niche_genre_id)
            .execute(&pool)
            .await
            .expect("tag comfort item with niche genre");

        let comfort_item = repo::media_item::upsert(
            &pool,
            &NewMediaItem {
                library_id: library.id,
                media_metadata_id: comfort_metadata.id,
                path: format!("/media/radar-test/comfort-{suffix}.mkv"),
                monitored: true,
                quality_profile_id: None,
                minimum_availability: None,
                plex_rating_key: Some(format!("radar-comfort-rk-{suffix}")),
                added_at: None,
            },
        )
        .await
        .expect("upsert comfort media_item");

        repo::watch_stats::upsert_watch_stats(
            &pool,
            &NewWatchStats {
                account_id: account.id,
                media_item_id: comfort_item.id,
                play_count: 6,
                finished_count: 6,
                rewatch_count: 5,
                total_watched_ms: 6 * 100 * 60 * 1000,
                avg_percent: Some(0.98),
                last_watched_at: Some(Utc::now()),
                abandoned: false,
                first_watched_at: Some(Utc::now() - chrono::Duration::days(400)),
            },
        )
        .await
        .expect("upsert comfort watch_stats");

        // A trending title in the mainstream genre the account has never
        // watched -> should surface as a blind spot.
        let trending_metadata = repo::media_metadata::upsert_by_tmdb(
            &pool,
            &NewMediaMetadata {
                kind: MediaKind::Movie,
                tmdb_id: Some(format!("radar-trending-{suffix}")),
                tvdb_id: None,
                imdb_id: None,
                provider_ids: serde_json::json!({}),
                title: format!("Radar Trending Blockbuster {suffix}"),
                sort_title: None,
                original_title: None,
                original_language: None,
                status: None,
                overview: None,
                studio: None,
                network: None,
                runtime_minutes: Some(120),
                year: Some(2024),
                images: serde_json::json!({}),
            },
        )
        .await
        .expect("upsert trending media_metadata");

        sqlx::query("INSERT INTO media_metadata_genres (media_metadata_id, genre_id) VALUES ($1, $2)")
            .bind(trending_metadata.id)
            .bind(mainstream_genre_id)
            .execute(&pool)
            .await
            .expect("tag trending item with mainstream genre");

        repo::trending::insert_snapshot(
            &pool,
            &NewTrendingSnapshot {
                source: "tmdb".to_string(),
                scope: "trending".to_string(),
                platform: None,
                region: region.clone(),
                window: "day".to_string(),
                rank: Some(1),
                media_metadata_id: Some(trending_metadata.id),
                external_ref: None,
                popularity: Some(999.0),
            },
        )
        .await
        .expect("insert trending snapshot");

        let divergence = divergence_or_panic(recompute_divergence_for_region(&pool, account.id, &region).await);

        assert_eq!(divergence.account_id, account.id);
        assert!(divergence.mainstream_score.is_some());
        assert!(divergence.adventurousness.is_some());
        assert!(divergence.contrarian_index.is_some());

        let genre_index = divergence
            .genre_index
            .as_object()
            .expect("genre_index should be a JSON object");
        assert!(
            genre_index.contains_key(&format!("radar-niche-{suffix}")),
            "account's watched genre should appear in genre_index: {genre_index:?}"
        );

        let guilty_pleasures = divergence
            .guilty_pleasures
            .as_ref()
            .expect("guilty_pleasures should be populated")
            .as_array()
            .expect("guilty_pleasures should be a JSON array");
        assert!(
            guilty_pleasures
                .iter()
                .any(|g| g["media_metadata_id"].as_i64() == Some(comfort_metadata.id)),
            "rewatched, non-trending comfort item should surface as a guilty pleasure: {guilty_pleasures:?}"
        );

        let blind_spots = divergence
            .blind_spots
            .as_ref()
            .expect("blind_spots should be populated")
            .as_array()
            .expect("blind_spots should be a JSON array");
        assert!(
            blind_spots
                .iter()
                .any(|b| b["media_metadata_id"].as_i64() == Some(trending_metadata.id)),
            "untouched trending item should surface as a blind spot: {blind_spots:?}"
        );

        // Round-trip: latest_divergence should read back the row we just
        // wrote (comparing by id since computed_at has DB-side precision
        // that can differ trivially from a client-side clock read).
        let latest = latest_divergence(&pool, account.id)
            .await
            .expect("latest_divergence query")
            .expect("a divergence row should now exist for this account");
        assert_eq!(latest.id, divergence.id);
        assert_eq!(latest.account_id, account.id);
    }

    /// Small helper so the live-DB test above reads top-to-bottom instead
    /// of interleaving `.expect(...)` messages mid-assertion-setup.
    fn divergence_or_panic(result: MuseResult<TasteDivergence>) -> TasteDivergence {
        result.expect("recompute_divergence_for_region should succeed")
    }
}
