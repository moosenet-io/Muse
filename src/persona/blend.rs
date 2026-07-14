//! Persona blending: fold N [`Persona`]s into one SESSION taste vector for
//! a group-watching context (MUSEX-03, Plane TERM #379) — the "who's on the
//! couch right now" companion to MUSEX-02's per-account/per-household
//! personas. `docs/MUSEX-experience-layer.md` §3.1 flagged this file
//! (`persona/blend.rs`) as MUSEX-03's when the `persona/` module scaffold
//! was proposed.
//!
//! ## Pure math, no DB, no live systems (S9)
//! Every function here operates on already-fetched [`Persona`] rows (the
//! caller resolves them via `crate::repo::persona::{list_for_account,
//! get_by_id}`, per that module's addressability seam docs) — this module
//! never opens a connection or calls a live embedding/LLM model. Same
//! posture as `taste_model::profile::mean_embedding`.
//!
//! ## Why "intersection", not a naive centroid average (the AC's core ask)
//! A naive average of N persona centroids is a MUSH: it pulls the session
//! vector toward whatever the group's tastes have in common by SHEER
//! ARITHMETIC, but it treats a dimension every persona agrees on and a
//! dimension where they wildly disagree identically — a horror fan and a
//! rom-com fan average to bland "horror-com," which nobody actually wants.
//! [`blend_personas`] instead computes, **per embedding dimension**, an
//! AGREEMENT WEIGHT from how much the personas' values at that dimension
//! agree (low variance = high weight, high variance = suppressed toward
//! zero), then multiplies the plain componentwise mean by that weight:
//!
//! ```text
//! mean_d      = average of persona[i].centroid[d] over i
//! variance_d  = population variance of persona[i].centroid[d] over i
//! weight_d    = 1 / (1 + variance_d / (mean_d^2 + EPSILON))   // in (0, 1]
//! blended_d   = mean_d * weight_d
//! ```
//!
//! `weight_d` is a **relative** (coefficient-of-variation-shaped) measure —
//! variance is normalized by the dimension's own squared mean, not a fixed
//! constant — so the formula is scale-invariant: it works the same whether
//! the raw embedding components are large or (as with a normalized
//! nomic-embed-text vector) small, rather than requiring the caller to
//! guess an absolute variance threshold. `weight_d == 1.0` exactly when
//! every persona has the IDENTICAL value at dimension `d` (variance = 0 —
//! genuine, total agreement); it falls toward `0.0` as the personas'
//! values at that dimension spread out relative to their mean. The result
//! is a vector reshaped toward the directions the group's tastes actually
//! share, with divergent axes suppressed rather than blindly averaged in —
//! an intersection-shaped combination, not a mush. This is additive: it
//! never touches `taste_model::profile` or any per-account
//! `taste_profile`/`overall_centroid` row, matching the
//! never-blend-taste-across-accounts invariant (`docs/MUSEX-experience-layer.md`
//! §1.1) — a session blend is a NEW aggregation over already-derived
//! persona centroids, not a mutation of anyone's own taste.
//!
//! ## No-overlap detection
//! Alongside the weighted blend, every distinct pair of input personas'
//! RAW centroids (unweighted — the group's actual taste directions, not
//! the reshaped blend) is compared by cosine similarity (see
//! [`cosine_similarity`], the same notion of similarity
//! `repo::embedding::nearest`'s pgvector `<=>` operator computes in SQL,
//! reimplemented here in pure Rust since this module never touches the
//! DB). The WEAKEST pairwise similarity — not the average — drives the
//! no-overlap call: a group only genuinely overlaps if EVERY pair does, so
//! the single most-divergent pair is what should gate the blend. When that
//! minimum is at or below [`NO_OVERLAP_COSINE_THRESHOLD`] (0.0 — the two
//! centroids are orthogonal or pointing in opposed directions, i.e. no
//! shared "liking" direction exists at all between that pair), blending
//! would paper over a real mismatch, so [`blend_personas`] reports
//! [`BlendStatus::NoOverlap`] instead of a silently-blended vector.
//!
//! ## Explanation
//! Built from the personas' `defining_signals` (via [`Persona::explain`]),
//! not from raw embedding dimensions (a bare vector index has no human
//! meaning). The genre NAMES common to every persona's `top_genres` (a
//! `BTreeSet` intersection, alphabetically ordered — never a `HashMap`) are
//! surfaced as "lands in every persona's wheelhouse because X+Y overlap";
//! when personas share no labeled top genre the explanation says so and
//! falls back to the measured embedding-space agreement instead of
//! inventing a genre story that isn't there.
//!
//! ## Determinism
//! [`blend_personas`] sorts its working copy of the input by `persona.id`
//! BEFORE computing anything (mirroring `taste_model::profile::mean_embedding`
//! and `persona::derive`'s sort-before-summing idiom) — the same *set* of
//! personas produces a bit-identical [`BlendResult`] regardless of the
//! order the caller's slice happens to list them in. See the
//! `blend_is_order_independent_and_bit_deterministic` test below, built
//! with the same "widely-different-magnitude" teeth as
//! `taste_model::profile`'s determinism test: the fixture is constructed
//! so a naive caller-order summation would visibly differ across input
//! orders, and only then is `blend_personas`'s order-independence a real
//! proof rather than a vacuous one.

use std::collections::BTreeSet;

use pgvector::Vector;

use crate::models::embedding::EMBEDDING_DIM;
use crate::models::persona::Persona;

/// The minimum pairwise cosine similarity, among every pair of input
/// personas' raw centroids, at or below which [`blend_personas`] refuses
/// to emit a blended session vector and reports [`BlendStatus::NoOverlap`]
/// instead. `0.0` = the two centroids are orthogonal or negatively
/// correlated — there is no shared "liking" direction between that pair at
/// all, so averaging them (weighted or not) would manufacture a session
/// vector nobody in the pair actually wants.
pub const NO_OVERLAP_COSINE_THRESHOLD: f32 = 0.0;

/// Normalizes a dimension's variance by its own squared mean
/// (coefficient-of-variation shape) so the agreement weight in
/// [`blend_personas`] is scale-invariant — a tiny constant added to avoid
/// dividing by zero when a dimension's mean is exactly zero across every
/// persona (which then falls back to the raw variance).
const AGREEMENT_VARIANCE_EPSILON: f64 = 1e-6;

/// How a [`blend_personas`] call resolved.
#[derive(Debug, Clone, PartialEq)]
pub enum BlendStatus {
    /// N >= 2 personas were blended; `BlendResult::session_vector` is the
    /// agreement-weighted intersection vector (see the module doc).
    Blended,
    /// Exactly one persona was "blended" — the degenerate case. The AC's
    /// "blend of 1 = that persona": `BlendResult::session_vector` is that
    /// persona's own centroid, unmodified.
    SinglePersona,
    /// The group's tastes do not genuinely overlap (the weakest pairwise
    /// cosine similarity was at or below [`NO_OVERLAP_COSINE_THRESHOLD`]).
    /// `BlendResult::session_vector` is still populated (the plain
    /// UNWEIGHTED mean — a labeled compromise, never silently presented as
    /// "the" blend) but callers MUST branch on this status rather than
    /// using the vector as if it were a genuine intersection; `suggestion`
    /// is a human-readable compromise/split recommendation.
    NoOverlap { suggestion: String },
}

/// The result of [`blend_personas`]: the session taste vector, why it
/// landed where it did, and whether a genuine blend was even possible.
#[derive(Debug, Clone, PartialEq)]
pub struct BlendResult {
    /// `Blended`: the agreement-weighted intersection vector.
    /// `SinglePersona`: that one persona's own centroid.
    /// `NoOverlap`: the plain unweighted mean, presented as a labeled
    /// compromise only — see [`BlendStatus::NoOverlap`].
    pub session_vector: Vector,
    /// Human-readable "why this session vector" — see the module doc's
    /// "Explanation" section.
    pub explanation: String,
    pub status: BlendStatus,
}

/// Blend `personas` into one session taste vector. See the module doc for
/// the full formula and no-overlap rule. Degrades to
/// [`BlendStatus::SinglePersona`] for exactly one persona, and to a
/// zero-vector/no-suggestion-needed [`BlendStatus::NoOverlap`] for an EMPTY
/// slice (nothing to blend — surfaced explicitly rather than fabricating a
/// vector) so this function never panics on caller input.
pub fn blend_personas(personas: &[Persona]) -> BlendResult {
    let mut sorted: Vec<&Persona> = personas.iter().collect();
    sorted.sort_unstable_by_key(|p| p.id);

    match sorted.len() {
        0 => BlendResult {
            session_vector: Vector::from(vec![0.0f32; EMBEDDING_DIM as usize]),
            explanation: "no personas were provided -- nothing to blend".to_string(),
            status: BlendStatus::NoOverlap {
                suggestion: "select at least one persona to build a session vector".to_string(),
            },
        },
        1 => {
            let p = sorted[0];
            BlendResult {
                session_vector: p.centroid.clone(),
                explanation: single_persona_explanation(p),
                status: BlendStatus::SinglePersona,
            }
        }
        _ => blend_many(&sorted),
    }
}

/// The N >= 2 case, factored out of [`blend_personas`] for readability.
/// `sorted` is already sorted by `persona.id` (the determinism seam).
fn blend_many(sorted: &[&Persona]) -> BlendResult {
    let n = sorted.len();
    let dim = EMBEDDING_DIM as usize;

    // Componentwise mean, in the fixed sorted-by-id order.
    let mut sums = vec![0.0f64; dim];
    for p in sorted {
        for (i, v) in p.centroid.as_slice().iter().enumerate() {
            sums[i] += *v as f64;
        }
    }
    let means: Vec<f64> = sums.iter().map(|s| s / n as f64).collect();

    // Componentwise population variance (also fixed sorted order).
    let mut sq_diff_sums = vec![0.0f64; dim];
    for p in sorted {
        for (i, v) in p.centroid.as_slice().iter().enumerate() {
            let diff = *v as f64 - means[i];
            sq_diff_sums[i] += diff * diff;
        }
    }
    let variances: Vec<f64> = sq_diff_sums.iter().map(|s| s / n as f64).collect();

    // Agreement weight per dimension + the weighted (intersection) vector,
    // plus the plain unweighted mean kept around as the NoOverlap
    // compromise vector -- see the module doc's formula.
    let mut blended = vec![0.0f32; dim];
    let mut naive_mean = vec![0.0f32; dim];
    for i in 0..dim {
        let mean = means[i];
        let variance = variances[i];
        let weight = 1.0 / (1.0 + variance / (mean * mean + AGREEMENT_VARIANCE_EPSILON));
        blended[i] = (mean * weight) as f32;
        naive_mean[i] = mean as f32;
    }

    // Weakest pairwise cosine similarity among the RAW centroids -- the
    // no-overlap gate. Deterministic i<j iteration over the sorted slice.
    let mut weakest: Option<(f32, (i64, i64))> = None;
    for i in 0..n {
        for j in (i + 1)..n {
            let sim =
                cosine_similarity(sorted[i].centroid.as_slice(), sorted[j].centroid.as_slice());
            let is_new_min = match weakest {
                None => true,
                Some((current_min, _)) => sim < current_min,
            };
            if is_new_min {
                weakest = Some((sim, (sorted[i].id, sorted[j].id)));
            }
        }
    }
    let (min_similarity, (weakest_a, weakest_b)) =
        weakest.expect("n >= 2 guarantees at least one pair");

    let intersecting_genres = intersecting_top_genres(sorted);

    if min_similarity <= NO_OVERLAP_COSINE_THRESHOLD {
        let suggestion = format!(
            "No genuine taste overlap across all {n} personas -- the weakest pairwise agreement \
             is between persona #{weakest_a} and persona #{weakest_b} (cosine similarity {min_similarity:.3}, \
             at or below the no-overlap threshold {NO_OVERLAP_COSINE_THRESHOLD:.2}). Rather than force a \
             single session pick, split into subgroups whose tastes actually align, or fall back to a \
             deliberate compromise pick everyone opts into knowingly."
        );
        return BlendResult {
            session_vector: Vector::from(naive_mean),
            explanation: format!(
                "No overlap detected: personas #{weakest_a} and #{weakest_b} have the weakest \
                 pairwise taste agreement (cosine similarity {min_similarity:.3}) among the {n} \
                 personas -- their tastes point in genuinely different directions, not just \
                 different-but-compatible ones. The session vector shown is a plain compromise \
                 average, not a genuine intersection."
            ),
            status: BlendStatus::NoOverlap { suggestion },
        };
    }

    let explanation = if intersecting_genres.is_empty() {
        format!(
            "Session vector blends {n} personas by up-weighting embedding dimensions where they \
             genuinely agree and suppressing dimensions where they diverge (not a naive average). \
             The weakest pairwise taste similarity is {min_similarity:.3} -- above the no-overlap \
             threshold, so the agreement is real even though these personas share no top-listed \
             genre; the overlap lives in the finer-grained taste signal rather than a labeled genre."
        )
    } else {
        format!(
            "Session vector blends {n} personas by up-weighting embedding dimensions where they \
             genuinely agree and suppressing dimensions where they diverge (not a naive average). \
             It lands in every persona's wheelhouse because {} overlap across all {n} personas' top \
             genres (weakest pairwise taste similarity {min_similarity:.3}).",
            intersecting_genres.join("+"),
        )
    };

    BlendResult {
        session_vector: Vector::from(blended),
        explanation,
        status: BlendStatus::Blended,
    }
}

/// Human explanation for the [`BlendStatus::SinglePersona`] degenerate
/// case: the session vector IS that persona, so the explanation says so
/// plainly and reuses [`Persona::explain`] rather than inventing a second
/// "why" story.
fn single_persona_explanation(p: &Persona) -> String {
    let explanation = p.explain();
    if explanation.top_genres.is_empty() {
        format!(
            "Only one persona ('{}') is in this session, so the session vector is exactly that \
             persona's own taste centroid -- nothing to blend against.",
            p.name
        )
    } else {
        let genres: Vec<String> = explanation
            .top_genres
            .iter()
            .map(|(genre, _count)| genre.clone())
            .collect();
        format!(
            "Only one persona ('{}') is in this session, so the session vector is exactly that \
             persona's own taste centroid -- nothing to blend against. Its defining genres: {}.",
            p.name,
            genres.join(", ")
        )
    }
}

/// The genre names present in EVERY `sorted` persona's `top_genres`
/// (`Persona::explain`), as an alphabetically-ordered `Vec` (via
/// `BTreeSet` intersection -- deterministic, never a `HashMap`). Empty
/// when there are no personas or no genre is shared by all of them.
fn intersecting_top_genres(sorted: &[&Persona]) -> Vec<String> {
    let mut acc: Option<BTreeSet<String>> = None;
    for p in sorted {
        let genres: BTreeSet<String> = p
            .explain()
            .top_genres
            .into_iter()
            .map(|(genre, _count)| genre)
            .collect();
        acc = Some(match acc {
            None => genres,
            Some(current) => current.intersection(&genres).cloned().collect(),
        });
    }
    acc.unwrap_or_default().into_iter().collect()
}

/// Cosine similarity between two equal-length vectors, in `[-1.0, 1.0]` --
/// the same notion of similarity `repo::embedding`'s pgvector `<=>`
/// operator computes in SQL (`distance = 1 - cosine_similarity`),
/// reimplemented here in pure Rust because this module never touches the
/// DB. Accumulates in `f64` for precision (matching
/// `taste_model::profile::mean_embedding`'s posture) and returns `0.0`
/// (treated as "no measurable relationship," not an error) if either
/// vector has zero magnitude, since cosine similarity is undefined there.
pub fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    let dot: f64 = a
        .iter()
        .zip(b.iter())
        .map(|(x, y)| *x as f64 * *y as f64)
        .sum();
    let norm_a: f64 = a
        .iter()
        .map(|x| (*x as f64) * (*x as f64))
        .sum::<f64>()
        .sqrt();
    let norm_b: f64 = b
        .iter()
        .map(|x| (*x as f64) * (*x as f64))
        .sum::<f64>()
        .sqrt();
    if norm_a <= 0.0 || norm_b <= 0.0 {
        return 0.0;
    }
    (dot / (norm_a * norm_b)) as f32
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::persona::PERSONA_KIND_DERIVED;
    use chrono::Utc;
    use serde_json::json;

    fn persona(
        id: i64,
        name: &str,
        centroid: Vec<f32>,
        top_genres: &[(&str, i64)],
        sample_size: i32,
    ) -> Persona {
        let genres_json: Vec<serde_json::Value> = top_genres
            .iter()
            .map(|(g, c)| json!({"genre": g, "count": c}))
            .collect();
        Persona {
            id,
            account_id: Some(1),
            name: name.to_string(),
            kind: PERSONA_KIND_DERIVED.to_string(),
            centroid: Vector::from(centroid),
            defining_signals: json!({
                "context_key": null,
                "top_genres": genres_json,
                "source_media_item_ids": [],
            }),
            metadata: json!({}),
            sample_size,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    fn full_width(components: &[(usize, f32)]) -> Vec<f32> {
        let mut v = vec![0.0f32; EMBEDDING_DIM as usize];
        for (i, value) in components {
            v[*i] = *value;
        }
        v
    }

    // ------------------------------------------------------------------
    // Single-persona degrade
    // ------------------------------------------------------------------

    #[test]
    fn blend_of_one_persona_is_exactly_that_persona() {
        let p = persona(
            7,
            "solo-2am",
            full_width(&[(0, 1.0), (1, -0.5)]),
            &[("horror", 4)],
            10,
        );
        let result = blend_personas(std::slice::from_ref(&p));
        assert_eq!(result.status, BlendStatus::SinglePersona);
        assert_eq!(result.session_vector.as_slice(), p.centroid.as_slice());
        assert!(
            result.explanation.contains("solo-2am") && result.explanation.contains("horror"),
            "single-persona explanation should name the persona and its defining genre: {}",
            result.explanation
        );
    }

    #[test]
    fn blend_of_zero_personas_is_surfaced_not_fabricated() {
        let result = blend_personas(&[]);
        match result.status {
            BlendStatus::NoOverlap { suggestion } => assert!(!suggestion.is_empty()),
            other => panic!("expected NoOverlap for an empty input, got {other:?}"),
        }
        assert_eq!(
            result.session_vector.as_slice(),
            vec![0.0f32; EMBEDDING_DIM as usize].as_slice()
        );
    }

    // ------------------------------------------------------------------
    // Agreement is rewarded, divergence is suppressed (the "not a mush" AC)
    // ------------------------------------------------------------------

    #[test]
    fn a_dimension_every_persona_agrees_on_is_weighted_at_full_strength() {
        // Three personas all have EXACTLY 5.0 at dimension 0 -- zero
        // variance, so the agreement weight there must be exactly 1.0 and
        // the blended value must equal the (trivially identical) mean.
        let a = persona(1, "a", full_width(&[(0, 5.0)]), &[], 1);
        let b = persona(2, "b", full_width(&[(0, 5.0)]), &[], 1);
        let c = persona(3, "c", full_width(&[(0, 5.0)]), &[], 1);
        let result = blend_personas(&[a, b, c]);
        assert_eq!(result.status, BlendStatus::Blended);
        assert_eq!(
            result.session_vector.as_slice()[0],
            5.0,
            "a dimension with zero cross-persona variance must be preserved at full strength"
        );
    }

    #[test]
    fn a_divergent_dimension_is_suppressed_relative_to_the_naive_average() {
        // Dimensions 0-4: a big block of STRONG agreement (all three
        // personas = 10.0), so the overall pairwise cosine similarity
        // stays well above the no-overlap threshold -- this fixture must
        // land Blended, not NoOverlap, so it can actually exercise
        // per-dimension suppression. Dimension 5: a genuinely divergent
        // dimension (a/b=1.0, c=-1.0) whose ABSOLUTE magnitude is small
        // enough not to flip the overall similarity negative, but whose
        // RELATIVE divergence (variance vs. its own mean) is large --
        // the agreement-weighted blend must pull dimension 5 STRICTLY
        // TOWARD ZERO relative to the plain average there, proving
        // divergence is suppressed, not averaged in uncritically.
        // Dimension 6: all three agree (same value) as a control, and
        // must come through at full strength regardless.
        let anchor = [(0, 10.0), (1, 10.0), (2, 10.0), (3, 10.0), (4, 10.0)];
        let a_dims: Vec<(usize, f32)> =
            anchor.iter().copied().chain([(5, 1.0), (6, 2.0)]).collect();
        let b_dims = a_dims.clone();
        let c_dims: Vec<(usize, f32)> = anchor
            .iter()
            .copied()
            .chain([(5, -1.0), (6, 2.0)])
            .collect();

        let a = persona(1, "a", full_width(&a_dims), &[], 1);
        let b = persona(2, "b", full_width(&b_dims), &[], 1);
        let c = persona(3, "c", full_width(&c_dims), &[], 1);
        let naive_mean_dim5 = (1.0 + 1.0 - 1.0) / 3.0f32;

        let result = blend_personas(&[a, b, c]);
        assert_eq!(
            result.status,
            BlendStatus::Blended,
            "the agreement anchor dims must keep overall similarity positive: {:?}",
            result.status
        );
        let blended5 = result.session_vector.as_slice()[5];
        assert!(
            blended5.abs() < naive_mean_dim5.abs(),
            "divergent dimension 5 must be suppressed below the naive mean: blended={blended5} \
             naive_mean={naive_mean_dim5}"
        );
        assert_eq!(
            result.session_vector.as_slice()[6],
            2.0,
            "the agreeing control dimension must survive at full strength"
        );
        assert_eq!(
            result.session_vector.as_slice()[0],
            10.0,
            "an agreement-anchor dimension must survive at full strength too"
        );
        assert!(
            result.explanation.contains("agree") || result.explanation.contains("diverge"),
            "explanation should describe the agreement/divergence mechanism: {}",
            result.explanation
        );
    }

    // ------------------------------------------------------------------
    // Explanation surfaces shared genres from defining_signals
    // ------------------------------------------------------------------

    #[test]
    fn explanation_names_the_genres_shared_by_every_persona() {
        let a = persona(
            1,
            "a",
            full_width(&[(0, 1.0)]),
            &[("horror", 5), ("comedy", 2)],
            3,
        );
        let b = persona(
            2,
            "b",
            full_width(&[(0, 1.0)]),
            &[("comedy", 9), ("horror", 1)],
            3,
        );
        let c = persona(
            3,
            "c",
            full_width(&[(0, 1.0)]),
            &[("horror", 3), ("comedy", 3), ("documentary", 1)],
            3,
        );
        let result = blend_personas(&[a, b, c]);
        assert_eq!(result.status, BlendStatus::Blended);
        assert!(
            result.explanation.contains("horror") && result.explanation.contains("comedy"),
            "explanation must name genres shared by ALL personas: {}",
            result.explanation
        );
        assert!(
            !result.explanation.contains("documentary"),
            "a genre only one persona has must not be claimed as shared: {}",
            result.explanation
        );
    }

    #[test]
    fn explanation_degrades_cleanly_when_no_top_genre_is_shared() {
        let a = persona(1, "a", full_width(&[(0, 1.0)]), &[("horror", 5)], 3);
        let b = persona(2, "b", full_width(&[(0, 1.0)]), &[("romance", 5)], 3);
        let result = blend_personas(&[a, b]);
        assert_eq!(result.status, BlendStatus::Blended);
        assert!(
            !result.explanation.is_empty(),
            "must still produce a non-empty explanation with no genre overlap"
        );
    }

    // ------------------------------------------------------------------
    // No-overlap: surfaced, not hidden (negative test)
    // ------------------------------------------------------------------

    #[test]
    fn genuinely_opposed_personas_are_detected_as_no_overlap_not_silently_blended() {
        // Perfectly opposed centroids (cosine similarity == -1.0):
        // dimension 0 is +1.0 for "a" and -1.0 for "b", everything else
        // zero for both. There is no shared "liking" direction at all.
        let a = persona(
            1,
            "horror-fan",
            full_width(&[(0, 1.0)]),
            &[("horror", 5)],
            3,
        );
        let b = persona(
            2,
            "anti-horror",
            full_width(&[(0, -1.0)]),
            &[("romance", 5)],
            3,
        );
        let result = blend_personas(&[a, b]);
        match &result.status {
            BlendStatus::NoOverlap { suggestion } => {
                assert!(
                    !suggestion.is_empty(),
                    "a NoOverlap result must carry an actionable suggestion, not an empty string"
                );
                assert!(
                    suggestion.contains("subgroup")
                        || suggestion.contains("split")
                        || suggestion.contains("compromise"),
                    "suggestion should propose a compromise or a split: {suggestion}"
                );
            }
            other => panic!(
                "genuinely opposed personas must be detected as NoOverlap, not silently blended \
                 into {other:?}"
            ),
        }
        assert!(
            result.explanation.contains("No overlap") || result.explanation.contains("no overlap"),
            "the no-overlap finding must be surfaced in the explanation, not hidden: {}",
            result.explanation
        );
        // The session_vector for NoOverlap is a labeled compromise (plain
        // mean), never presented as a genuine intersection -- confirm it's
        // still the arithmetic mean, not e.g. a zeroed-out vector.
        let expected_compromise = (1.0f32 + -1.0f32) / 2.0;
        assert_eq!(result.session_vector.as_slice()[0], expected_compromise);
    }

    // ------------------------------------------------------------------
    // Determinism, WITH TEETH (same idiom as
    // taste_model::profile::mean_embedding_is_order_independent_and_bit_deterministic)
    // ------------------------------------------------------------------

    /// The exact wrong implementation `blend_personas` must NOT be: sums
    /// each persona's centroid in caller-slice order (no sort-by-id).
    /// Used only to prove the fixture below has enough magnitude spread
    /// that summation order genuinely changes the componentwise mean --
    /// giving the real determinism assertion teeth.
    fn naive_mean_in_caller_order(personas: &[Persona]) -> Vec<f32> {
        let dim = EMBEDDING_DIM as usize;
        let mut sum = vec![0.0f64; dim];
        for p in personas {
            for (i, v) in p.centroid.as_slice().iter().enumerate() {
                sum[i] += *v as f64;
            }
        }
        let n = personas.len() as f64;
        sum.iter().map(|v| (v / n) as f32).collect()
    }

    #[test]
    fn blend_is_order_independent_and_bit_deterministic() {
        // Both the naive helper and blend_personas' internal mean
        // accumulate in f64, so -- exactly like
        // taste_model::profile's determinism test -- dimension 0's values
        // must differ in magnitude by more than 2^53 (~9.0e15) for
        // addition order to matter: 1e17 / 1.0 / -1e17 across ids 1/2/3.
        let a = persona(
            1,
            "a",
            full_width(&[(0, 1e17), (1, 3.0)]),
            &[("horror", 3)],
            1,
        );
        let b = persona(
            2,
            "b",
            full_width(&[(0, 1.0), (1, 3.0)]),
            &[("horror", 2)],
            1,
        );
        let c = persona(
            3,
            "c",
            full_width(&[(0, -1e17), (1, 3.0)]),
            &[("horror", 1)],
            1,
        );

        // (This magnitude spread also makes dimension 0 dominate the
        // pairwise cosine similarity between a/c, so this fixture lands
        // BlendStatus::NoOverlap -- that's fine and not the point here:
        // the determinism guarantee below must hold regardless of which
        // status is reached, since sorting-by-id happens before either
        // the mean/variance pass or the pairwise-similarity pass.)
        //
        // TEETH: prove this fixture's magnitude spread makes a naive
        // caller-order summation genuinely order-dependent. If this
        // assertion ever fails, the fixture lost its spread and the real
        // determinism assertions below would be vacuous.
        let forward_order = vec![a.clone(), b.clone(), c.clone()];
        let shuffled_order = vec![c.clone(), a.clone(), b.clone()];
        let naive_forward = naive_mean_in_caller_order(&forward_order);
        let naive_shuffled = naive_mean_in_caller_order(&shuffled_order);
        assert_ne!(
            naive_forward, naive_shuffled,
            "fixture sanity: with this magnitude spread, a naive caller-order sum MUST differ \
             across input orders -- otherwise the determinism assertions below have no teeth"
        );

        // The real guard: blend_personas (sorts by persona.id before
        // summing) is bit-identical across every input ordering of the
        // same persona SET, despite that magnitude spread.
        let result_forward = blend_personas(&forward_order);
        let result_reversed = blend_personas(&vec![c.clone(), b.clone(), a.clone()]);
        let result_shuffled = blend_personas(&shuffled_order);

        assert_eq!(
            result_forward.session_vector.as_slice(),
            result_reversed.session_vector.as_slice(),
            "blend_personas must be bit-identical regardless of input persona order, even when \
             the underlying centroids have magnitudes spread wide enough to make sum order matter"
        );
        assert_eq!(
            result_forward.session_vector.as_slice(),
            result_shuffled.session_vector.as_slice()
        );
        assert_eq!(result_forward.explanation, result_reversed.explanation);
        assert_eq!(result_forward.explanation, result_shuffled.explanation);
        assert_eq!(result_forward.status, result_reversed.status);
    }
}
