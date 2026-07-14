//! MUSET-06 (Plane TERM #371): TASTE golden-set regression (degradation
//! floor).
//!
//! ## Scope
//! MUSET-04 gave the pipeline reusable fixtures + documented
//! [`crate::fixtures::ProfileExpectation`]s. MUSET-05 gave it a mechanics
//! floor (determinism of the pgvector/centroid/context-bucketing plumbing).
//! Neither of those asserts a *specific numeric* taste-lean and catches it
//! drifting away from a known-good value over time — that's this module's
//! job: a small GOLDEN SET of `(history -> expected taste-lean)` cases,
//! graded against a tuned tolerance, that FAILS when a change moves the
//! computed lean away from its documented known-good value.
//!
//! ## Grounding
//! Read before touching this file: the MUSET-04 fixtures
//! (`crate::fixtures::{heavy_rewatcher, multi_genre, cold_start_empty,
//! sparse_metadata}` + their `ProfileExpectation`s), the real taste model
//! (`taste_model::signals::{WEIGHT_FINISH, WEIGHT_REWATCH_PER,
//! recency_weight}`, `taste_model::profile::{aggregate_weighted,
//! compute_genre_affinity}`), MUSET-05's `taste_mechanics_tests` (the
//! `mechanics_pool_or_skip` / fixture-loader idiom reused below as
//! [`golden_pool_or_skip`]), and MUSET-02's golden idiom (a fixed,
//! documented "known good" value a regression is measured against).
//!
//! ## Design: what "golden" means here
//! Every golden case's "known-good" value ([`HEAVY_REWATCHER_EXPECTED_SHARE`],
//! [`MULTI_GENRE_EXPECTED_SHARE`] below) is a LITERAL constant, computed BY
//! HAND from the documented weight formula at the time this module was
//! written — never re-derived from the production weight constants at test
//! time. That distinction is load-bearing: if a golden expectation were
//! computed by calling `WEIGHT_REWATCH_PER` etc. itself, a real regression
//! to that constant would move the "expected" value and the "actual" value
//! together and the test would never fail. Hardcoding the literal is what
//! makes the golden set catch a real regression instead of trivially
//! passing forever.
//!
//! `heavy_rewatcher`'s comfort-drama share: item 0 gets a `finished` signal
//! (`WEIGHT_FINISH` = 1.0) plus a `rewatched` signal
//! (`WEIGHT_REWATCH_PER * rewatch_count` = `2.5 * 5` = 12.5), for a total of
//! 13.5; item 1 (documentary) gets only `finished` = 1.0. Both signals share
//! the same `days_ago` (5), so recency decay is identical on both sides and
//! cancels out of the ratio. Expected comfort-drama share of total genre
//! affinity mass: `13.5 / (13.5 + 1.0)` = `0.9310344827586207`.
//!
//! `multi_genre`'s per-genre share: four titles, each a lone `finished`
//! signal (weight 1.0) with the same recency (`days_ago` = 10 for all
//! four) -> perfectly even weight -> each genre's share is exactly `0.25`.
//!
//! ## The tolerance
//! [`LEAN_TOLERANCE`] = 0.05 (5 percentage points of affinity-mass share).
//! Rationale: the only real "noise" between when a golden literal was fixed
//! and when a test runs it is (a) floating-point summation order and (b)
//! the wall-clock gap between the fixture's `days_ago`-based seed
//! timestamps and the `Utc::now()` read at assertion time — both are on the
//! order of `1e-6` or smaller here (same `days_ago` on both sides of every
//! ratio cancels the decay factor to first order; a few milliseconds of
//! test-execution skew perturbs it by an utterly negligible fraction of a
//! half-life). `0.05` is two to three orders of magnitude looser than that
//! noise floor, so it will not flap, yet it is far tighter than the kind of
//! swing a real regression produces: the negative test below shows a
//! plausible "dropped rewatch signal" bug moves the heavy_rewatcher share
//! from ~0.93 to 0.5 (distance ~0.43), and a formula/weight-constant change
//! of the kind that would matter in practice moves shares by whole tenths,
//! not by a couple of percentage points. In short: loose enough to absorb
//! floating-point/timing noise, tight enough that nothing but an actual
//! behavior change gets through.
pub const LEAN_TOLERANCE: f64 = 0.05;

/// See the module doc's "Design" section for the derivation.
pub const HEAVY_REWATCHER_EXPECTED_SHARE: f64 = 0.9310344827586207;
/// See the module doc's "Design" section for the derivation.
pub const MULTI_GENRE_EXPECTED_SHARE: f64 = 0.25;

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde_json::Value as Json;
use sqlx::PgPool;

use crate::fixtures::{self, loader};
use crate::snapshot::load as snapshot_load;
use crate::taste_model::profile::{self, aggregate_weighted};
use crate::taste_model::signals::{
    replace_derived_signals, DEFAULT_HALF_LIFE_DAYS, WEIGHT_FINISH, WEIGHT_REWATCH_PER,
};

/// Same skip-cleanly-without-a-DB idiom as
/// `crate::taste_mechanics_tests::mechanics_pool_or_skip` /
/// `crate::fixtures::loader::tests::fixture_pool_or_skip` — reused
/// (independently, since both are private to their own modules) rather than
/// re-implemented from scratch: always try
/// `MUSE_SNAPSHOT_DATABASE_URL`/`MUSE_TEST_DATABASE_URL` through the guarded
/// `crate::snapshot::load` path, and skip (never fail, never touch a live
/// DB) when neither is configured.
async fn golden_pool_or_skip(test_name: &str) -> Option<PgPool> {
    let Some(database_url) = snapshot_load::snapshot_database_url_from_env() else {
        eprintln!(
            "{} / {} not set -- skipping {test_name} (expected in the default test run; \
             the grader-sanity + tolerance-math + negative-perturbation tests in this module \
             run unconditionally with no DB)",
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

// ===========================================================================
// The grader.
// ===========================================================================

/// The result of grading one golden case: how far the computed lean is from
/// its known-good expectation, and whether that distance is within
/// [`LEAN_TOLERANCE`] (or a case-supplied override).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GradeResult {
    pub distance: f64,
    pub pass: bool,
}

/// Grade one computed lean against its documented known-good expectation.
/// This is the WHOLE golden-set contract in one function: everything below
/// either calls this directly or (for the DB-gated fixture cases) computes
/// `actual_share` via the real taste pipeline first.
pub fn grade_lean(actual_share: f64, expected_share: f64, tolerance: f64) -> GradeResult {
    let distance = (actual_share - expected_share).abs();
    GradeResult {
        distance,
        pass: distance <= tolerance,
    }
}

/// Given a `{genre: weight}` totals map (as produced by
/// `taste_model::profile::aggregate_weighted`, or read back out of a
/// `compute_genre_affinity` JSON result via [`json_totals`]), compute one
/// genre's share of the total POSITIVE weight mass. Mirrors
/// `fixtures::loader::tests::top_genre_and_shares`'s share math (negative
/// weights don't count toward the positive mass a "lean" is measured
/// against — same convention that module uses), reimplemented here rather
/// than imported since that helper is private to its own test module.
fn share_of<K: Ord>(totals: &BTreeMap<K, f64>, key: &K) -> f64 {
    let total_positive: f64 = totals.values().map(|w| w.max(0.0)).sum();
    if total_positive <= 0.0 {
        return 0.0;
    }
    totals.get(key).copied().unwrap_or(0.0).max(0.0) / total_positive
}

/// Read a `compute_genre_affinity` JSON object back into a plain
/// `BTreeMap<String, f64>` totals map, so [`share_of`] can grade it exactly
/// like the pure-math reconstructions below.
fn json_totals(affinity: &Json) -> BTreeMap<String, f64> {
    let Json::Object(map) = affinity else {
        return BTreeMap::new();
    };
    map.iter()
        .map(|(k, v)| (k.clone(), v.as_f64().unwrap_or(0.0)))
        .collect()
}

// ===========================================================================
// 1. Grader sanity-check (H4 lesson: prove the grader discriminates BEFORE
//    trusting it across the golden set). Runs unconditionally -- no DB.
// ===========================================================================

#[test]
fn grader_sanity_check_passes_a_known_good_result_and_fails_a_known_bad_one() {
    // Known-good: the actual share equals the documented expectation
    // exactly -> the grader must PASS with ~zero distance.
    let known_good = grade_lean(
        HEAVY_REWATCHER_EXPECTED_SHARE,
        HEAVY_REWATCHER_EXPECTED_SHARE,
        LEAN_TOLERANCE,
    );
    assert!(
        known_good.pass,
        "grader must PASS an exact match against its own expectation"
    );
    assert!(
        known_good.distance < 1e-9,
        "an exact match's distance should be ~0, got {}",
        known_good.distance
    );

    // Known-bad: a hand-picked, obviously-wrong share (comfort-drama and
    // documentary tied at 50/50, as if the rewatch signal never happened)
    // -> the grader must FAIL, and by a margin well past LEAN_TOLERANCE, not
    // just barely.
    let known_bad = grade_lean(0.5, HEAVY_REWATCHER_EXPECTED_SHARE, LEAN_TOLERANCE);
    assert!(
        !known_bad.pass,
        "grader must FAIL a known-bad result (0.5 vs {HEAVY_REWATCHER_EXPECTED_SHARE}), \
         distance {}",
        known_bad.distance
    );
    assert!(
        known_bad.distance > LEAN_TOLERANCE * 2.0,
        "the known-bad case should miss by well more than the tolerance, not sit right at the \
         edge (distance {}, tolerance {LEAN_TOLERANCE})",
        known_bad.distance
    );

    // A grader that always passes, or always fails, would trivially "pass"
    // if only one branch above were checked -- assert the two results
    // actually differ, proving the grader is discriminating, not constant.
    assert_ne!(
        known_good.pass, known_bad.pass,
        "the grader must discriminate: known-good and known-bad must not agree"
    );
}

#[test]
fn lean_tolerance_is_tuned_between_floating_point_noise_and_a_real_regression() {
    // Pure sanity on the tuned constant itself (see the module doc's
    // "The tolerance" section for the full rationale): loose enough that it
    // isn't a brittle exact-float comparison, tight enough that it can't
    // possibly be satisfied by the ~0.43 swing the negative test below
    // demonstrates for a dropped-rewatch-signal regression.
    assert!(
        LEAN_TOLERANCE > 0.0 && LEAN_TOLERANCE < 0.15,
        "LEAN_TOLERANCE ({LEAN_TOLERANCE}) should be a small slice of the [0,1] share space, \
         not a near-vacuous bound"
    );
    let dropped_rewatch_regression_distance = 0.931_034_482_758_620_7 - 0.5;
    assert!(
        LEAN_TOLERANCE < dropped_rewatch_regression_distance / 2.0,
        "LEAN_TOLERANCE must sit well below a realistic regression's distance \
         ({dropped_rewatch_regression_distance}), or the golden set would miss it"
    );
}

// ===========================================================================
// 2. Negative test (load-bearing): a DELIBERATELY-WORSENED scoring input,
//    run through the REAL `aggregate_weighted` production function, must
//    FAIL the golden set. Proves the set catches degradation, not just
//    passes trivially. Runs unconditionally -- no DB, and the real app code
//    is never modified: only a LOCAL copy of the per-signal rows is
//    perturbed, inside this test.
// ===========================================================================

/// Reconstruct the exact `(genre, weight, observed_at)` rows
/// `taste_model::signals::derive_signals_for_account` +
/// `taste_model::profile::compute_genre_affinity`'s SQL join would produce
/// for `heavy_rewatcher`, as a LOCAL, perturbable `Vec` -- mirrors the
/// private `signals_from_watch_stats` (the "finished" + "rewatch beyond the
/// first" atoms), but lives here as test-owned data so it can be worsened
/// without touching any production module. Using the production
/// `WEIGHT_FINISH`/`WEIGHT_REWATCH_PER` constants here is safe (doesn't
/// undermine the golden literal) because [`HEAVY_REWATCHER_EXPECTED_SHARE`]
/// is a hardcoded literal, independent of these constants -- see the module
/// doc.
fn heavy_rewatcher_genre_rows(now: DateTime<Utc>) -> Vec<(String, f32, DateTime<Utc>)> {
    vec![
        ("comfort-drama".to_string(), WEIGHT_FINISH, now),
        ("comfort-drama".to_string(), WEIGHT_REWATCH_PER * 5.0, now),
        ("documentary".to_string(), WEIGHT_FINISH, now),
    ]
}

#[test]
fn unperturbed_reconstruction_passes_the_golden_set() {
    // Sanity check on the reconstruction itself, before perturbing it below:
    // run it through the REAL `aggregate_weighted` and confirm it lands
    // within tolerance of the golden literal. If this ever failed, it would
    // mean the reconstruction above has drifted from the real signal
    // derivation, invalidating the negative test that follows.
    let now = Utc::now();
    let rows = heavy_rewatcher_genre_rows(now);
    let totals = aggregate_weighted(&rows, now, DEFAULT_HALF_LIFE_DAYS);
    let actual_share = share_of(&totals, &"comfort-drama".to_string());
    let grade = grade_lean(actual_share, HEAVY_REWATCHER_EXPECTED_SHARE, LEAN_TOLERANCE);
    assert!(
        grade.pass,
        "the unperturbed row reconstruction should pass the golden set (share {actual_share}, \
         expected {HEAVY_REWATCHER_EXPECTED_SHARE}, distance {})",
        grade.distance
    );
}

#[test]
fn a_deliberately_worsened_model_fails_the_golden_set() {
    // The load-bearing negative test: perturb a LOCAL COPY of the scoring
    // rows -- simulating a plausible real regression (a bug that drops the
    // "rewatched" signal, e.g. a wrong `> 0` guard on `rewatch_count`) --
    // and feed it through the SAME real `aggregate_weighted` production
    // function used everywhere else in this module. The real app
    // (`taste_model::signals`/`taste_model::profile`) is never touched.
    let now = Utc::now();
    let worsened_rows: Vec<(String, f32, DateTime<Utc>)> = vec![
        ("comfort-drama".to_string(), WEIGHT_FINISH, now),
        // The rewatch row is gone -- this is the injected degradation.
        ("documentary".to_string(), WEIGHT_FINISH, now),
    ];
    let worsened_totals = aggregate_weighted(&worsened_rows, now, DEFAULT_HALF_LIFE_DAYS);
    let worsened_share = share_of(&worsened_totals, &"comfort-drama".to_string());

    let grade = grade_lean(
        worsened_share,
        HEAVY_REWATCHER_EXPECTED_SHARE,
        LEAN_TOLERANCE,
    );
    assert!(
        !grade.pass,
        "the golden set must FAIL a deliberately-worsened model (dropped rewatch signal): \
         worsened share {worsened_share}, expected {HEAVY_REWATCHER_EXPECTED_SHARE}, distance \
         {} (tolerance {LEAN_TOLERANCE}) -- if this ever passes, the golden set is no longer \
         catching degradation",
        grade.distance
    );

    // A second, independently-shaped perturbation: instead of dropping a
    // signal, invert the weight sign (as if a rewatch were mis-scored as an
    // abandonment) -- a different plausible bug, checked against the same
    // golden expectation, must also fail.
    let inverted_rows: Vec<(String, f32, DateTime<Utc>)> = vec![
        ("comfort-drama".to_string(), WEIGHT_FINISH, now),
        (
            "comfort-drama".to_string(),
            -(WEIGHT_REWATCH_PER * 5.0),
            now,
        ),
        ("documentary".to_string(), WEIGHT_FINISH, now),
    ];
    let inverted_totals = aggregate_weighted(&inverted_rows, now, DEFAULT_HALF_LIFE_DAYS);
    let inverted_share = share_of(&inverted_totals, &"comfort-drama".to_string());
    let inverted_grade = grade_lean(
        inverted_share,
        HEAVY_REWATCHER_EXPECTED_SHARE,
        LEAN_TOLERANCE,
    );
    assert!(
        !inverted_grade.pass,
        "the golden set must also FAIL an inverted-sign rewatch regression: share \
         {inverted_share}, distance {} (tolerance {LEAN_TOLERANCE})",
        inverted_grade.distance
    );
}

#[test]
fn a_worsened_multi_genre_model_fails_the_golden_set() {
    // Same idea against the multi_genre golden case: a regression that
    // makes one genre's weight dominate (e.g. a recency-decay bug that
    // effectively zeroes out three of the four titles) should fail against
    // the documented "no genre dominates" expectation of an even ~0.25
    // share each.
    let now = Utc::now();
    let worsened_rows: Vec<(String, f32, DateTime<Utc>)> = vec![
        ("scifi".to_string(), WEIGHT_FINISH * 10.0, now), // regression: 10x over-weighted
        ("comedy".to_string(), WEIGHT_FINISH, now),
        ("horror".to_string(), WEIGHT_FINISH, now),
        ("romance".to_string(), WEIGHT_FINISH, now),
    ];
    let totals = aggregate_weighted(&worsened_rows, now, DEFAULT_HALF_LIFE_DAYS);
    let scifi_share = share_of(&totals, &"scifi".to_string());
    let grade = grade_lean(scifi_share, MULTI_GENRE_EXPECTED_SHARE, LEAN_TOLERANCE);
    assert!(
        !grade.pass,
        "the golden set must FAIL a lopsided multi_genre regression: scifi share {scifi_share}, \
         expected {MULTI_GENRE_EXPECTED_SHARE}, distance {} (tolerance {LEAN_TOLERANCE})",
        grade.distance
    );
}

// ===========================================================================
// 3. The golden set itself, run against the REAL DB-backed taste pipeline
//    (fixture load -> replace_derived_signals -> compute_genre_affinity).
//    DB-gated: skips cleanly with no MUSE_SNAPSHOT_DATABASE_URL /
//    MUSE_TEST_DATABASE_URL configured (S9 -- never a live DB).
// ===========================================================================

#[tokio::test]
async fn golden_heavy_rewatcher_lean_matches_known_good_within_tolerance() {
    let Some(pool) =
        golden_pool_or_skip("golden_heavy_rewatcher_lean_matches_known_good_within_tolerance")
            .await
    else {
        return;
    };
    let fixture = fixtures::heavy_rewatcher();
    let loaded = loader::load(&pool, &fixture)
        .await
        .expect("fixture should load");
    replace_derived_signals(&pool, loaded.account.id)
        .await
        .expect("deriving taste_signals should succeed");

    let now = Utc::now();
    let affinity =
        profile::compute_genre_affinity(&pool, loaded.account.id, now, DEFAULT_HALF_LIFE_DAYS)
            .await
            .expect("computing genre affinity should succeed");
    let totals = json_totals(&affinity);
    let suffixed_top = loaded
        .suffixed_genre("comfort-drama")
        .expect("this load seeded comfort-drama")
        .to_string();
    let actual_share = share_of(&totals, &suffixed_top);

    let grade = grade_lean(actual_share, HEAVY_REWATCHER_EXPECTED_SHARE, LEAN_TOLERANCE);
    assert!(
        grade.pass,
        "heavy_rewatcher's real computed comfort-drama lean ({actual_share}) drifted from its \
         known-good golden value ({HEAVY_REWATCHER_EXPECTED_SHARE}) by {} -- exceeds \
         LEAN_TOLERANCE ({LEAN_TOLERANCE}); this is the degradation the golden set exists to \
         catch",
        grade.distance
    );

    loader::cleanup(&pool, &loaded).await.ok();
}

#[tokio::test]
async fn golden_multi_genre_lean_matches_the_known_good_even_split_within_tolerance() {
    let Some(pool) = golden_pool_or_skip(
        "golden_multi_genre_lean_matches_the_known_good_even_split_within_tolerance",
    )
    .await
    else {
        return;
    };
    let fixture = fixtures::multi_genre();
    let loaded = loader::load(&pool, &fixture)
        .await
        .expect("fixture should load");
    replace_derived_signals(&pool, loaded.account.id)
        .await
        .expect("deriving taste_signals should succeed");

    let now = Utc::now();
    let affinity =
        profile::compute_genre_affinity(&pool, loaded.account.id, now, DEFAULT_HALF_LIFE_DAYS)
            .await
            .expect("computing genre affinity should succeed");
    let totals = json_totals(&affinity);

    // Every one of the four genres should independently grade against the
    // ~0.25 golden expectation -- not just the top one -- since the
    // documented known-good shape here is "evenly split", not "one winner".
    for base_genre in ["scifi", "comedy", "horror", "romance"] {
        let suffixed = loaded
            .suffixed_genre(base_genre)
            .unwrap_or_else(|| panic!("this load seeded {base_genre}"))
            .to_string();
        let actual_share = share_of(&totals, &suffixed);
        let grade = grade_lean(actual_share, MULTI_GENRE_EXPECTED_SHARE, LEAN_TOLERANCE);
        assert!(
            grade.pass,
            "multi_genre's real computed {base_genre} share ({actual_share}) drifted from the \
             known-good even-split golden value ({MULTI_GENRE_EXPECTED_SHARE}) by {} -- exceeds \
             LEAN_TOLERANCE ({LEAN_TOLERANCE})",
            grade.distance
        );
    }

    loader::cleanup(&pool, &loaded).await.ok();
}

#[tokio::test]
async fn golden_cold_start_and_sparse_metadata_leans_stay_at_the_known_good_empty_baseline() {
    let Some(pool) = golden_pool_or_skip(
        "golden_cold_start_and_sparse_metadata_leans_stay_at_the_known_good_empty_baseline",
    )
    .await
    else {
        return;
    };

    // Both of these fixtures' documented golden expectation is "nothing to
    // lean toward" -- an empty genre-affinity map, never an error and never
    // a spurious nonzero lean. Round out the golden set with them so all
    // four MUSET-04 fixtures are represented, not just the two with a
    // nontrivial numeric lean.
    for fixture in [fixtures::cold_start_empty(), fixtures::sparse_metadata()] {
        let name = fixture.name;
        let loaded = loader::load(&pool, &fixture)
            .await
            .expect("fixture should load");
        replace_derived_signals(&pool, loaded.account.id)
            .await
            .expect("deriving taste_signals should succeed");

        let now = Utc::now();
        let affinity =
            profile::compute_genre_affinity(&pool, loaded.account.id, now, DEFAULT_HALF_LIFE_DAYS)
                .await
                .expect("computing genre affinity should succeed");
        assert_eq!(
            affinity,
            Json::Object(Default::default()),
            "fixture {name:?}'s known-good golden baseline is an EMPTY genre-affinity map \
             (nothing to lean toward) -- got {affinity:?} instead"
        );

        loader::cleanup(&pool, &loaded).await.ok();
    }
}
