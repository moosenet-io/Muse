//! MUSEX-14 (Plane TERM #390), part A: taste-TARGETED promotion for newly-
//! available LIBRARY content — never a broadcast firehose.
//!
//! ## Why this module exists
//! When a title lands in the library (already owned/available media), the
//! naive move is to announce it to every opted-in friend. That's a firehose,
//! not curation, and it's exactly what this module refuses to do:
//! [`targeting::promote_new_title`] scores the NEW title against each
//! opted-in friend's OWN taste centroid and only ever targets the friends
//! whose match clears [`crate::config::Config::promotion_match_threshold`]
//! — a friend whose taste doesn't match gets nothing, silently, same as if
//! the title never landed for them.
//!
//! ## Reuses the real brain, invents nothing new
//! Per the AC, this module does NOT invent a second recommendation/rationale
//! path: [`targeting::promote_new_title`] builds a real
//! [`crate::curation::candidates::Candidate`] (source
//! [`crate::curation::candidates::CandidateSource::Taste`] — a fresh,
//! taste-matched pick is exactly what that source already means) and hands
//! it to [`crate::curation::recommend::build_rationale`] +
//! [`crate::discord::bot::build_taste_reply`] — the SAME rationale/embed
//! pipeline `crate::discord::bot::respond`'s `TasteAware` arm and the
//! `/recommend` HTTP handlers use. Only the SCORING primitive
//! ([`cosine_similarity_01`], below) and the per-friend fan-out loop
//! ([`targeting::promote_new_title`]) are new.
//!
//! ## Consent, by construction
//! [`targeting::promote_new_title`] enumerates candidates via
//! [`crate::discord::identity::TrustedFriends::opted_in_friends`] — the
//! ONLY iterator that type exposes over consenting friends (see that
//! method's own doc). A non-opted-in or non-allowlisted friend is not
//! merely filtered out by a runtime check here; it never enters the loop at
//! all, because it's absent from the iterator this module consumes. This is
//! the same "provable by construction" posture
//! `crate::discord::bot::decide_response_mode` documents for its own gate.
//!
//! ## Scoring: pure, in-memory, no DB round trip
//! [`cosine_similarity_01`] mirrors the `[0.0, 1.0]` "match" scale
//! `crate::curation::candidates::gather_taste_candidates` already
//! establishes for a pgvector cosine distance
//! (`(1.0 - distance / 2.0).clamp(0.0, 1.0)`), but computes it directly in
//! Rust: this module scores ONE new title against N friends' centroids
//! in-process, not a database nearest-neighbor search, so there's no `<=>`
//! query to issue per friend.

pub mod targeting;

pub use targeting::{dispatch_promotions, promote_new_title, TargetedPromotion};

/// Cosine similarity between two equal-length vectors, mapped onto the same
/// `[0.0, 1.0]` "match" scale `curation::candidates::gather_taste_candidates`
/// uses for a pgvector cosine distance. Returns `0.0` — never panics, never
/// `NaN` — for mismatched lengths, an empty vector, or a zero vector on
/// either side (an unembedded/all-zero input degrades to "no match," never a
/// spurious perfect or undefined score).
pub fn cosine_similarity_01(a: &[f32], b: &[f32]) -> f64 {
    if a.is_empty() || a.len() != b.len() {
        return 0.0;
    }

    let dot: f64 = a
        .iter()
        .zip(b)
        .map(|(x, y)| f64::from(*x) * f64::from(*y))
        .sum();
    let norm_a: f64 = a.iter().map(|x| f64::from(*x).powi(2)).sum::<f64>().sqrt();
    let norm_b: f64 = b.iter().map(|x| f64::from(*x).powi(2)).sum::<f64>().sqrt();

    if norm_a == 0.0 || norm_b == 0.0 {
        return 0.0;
    }

    let cosine_similarity = (dot / (norm_a * norm_b)).clamp(-1.0, 1.0);
    // pgvector's `<=>` cosine distance is `1 - cosine_similarity`, range
    // [0.0, 2.0] — mirrored exactly so this pure function and a real
    // pgvector nearest-neighbor query would agree on the same pair.
    let cosine_distance = 1.0 - cosine_similarity;
    (1.0 - (cosine_distance / 2.0)).clamp(0.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identical_vectors_score_a_perfect_match() {
        let v = vec![0.5, 0.5, 0.0];
        assert!((cosine_similarity_01(&v, &v) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn opposite_vectors_score_zero() {
        let a = vec![1.0, 0.0];
        let b = vec![-1.0, 0.0];
        assert!(cosine_similarity_01(&a, &b).abs() < 1e-9);
    }

    #[test]
    fn orthogonal_vectors_score_a_midpoint_match() {
        let a = vec![1.0, 0.0];
        let b = vec![0.0, 1.0];
        assert!((cosine_similarity_01(&a, &b) - 0.5).abs() < 1e-9);
    }

    #[test]
    fn mismatched_lengths_never_panics_and_scores_zero() {
        let a = vec![1.0, 0.0];
        let b = vec![1.0, 0.0, 0.0];
        assert_eq!(cosine_similarity_01(&a, &b), 0.0);
    }

    #[test]
    fn empty_vectors_score_zero_not_nan() {
        let a: Vec<f32> = Vec::new();
        assert_eq!(cosine_similarity_01(&a, &a), 0.0);
    }

    #[test]
    fn zero_vector_scores_zero_not_nan() {
        let zero = vec![0.0, 0.0];
        let other = vec![1.0, 1.0];
        assert_eq!(cosine_similarity_01(&zero, &other), 0.0);
    }

    #[test]
    fn scaled_but_parallel_vectors_still_score_a_perfect_match() {
        // Cosine similarity is scale-invariant -- a centroid/embedding pair
        // pointing the same direction at different magnitudes must still
        // read as a full match.
        let a = vec![1.0, 2.0, 3.0];
        let b = vec![2.0, 4.0, 6.0];
        assert!((cosine_similarity_01(&a, &b) - 1.0).abs() < 1e-9);
    }
}
