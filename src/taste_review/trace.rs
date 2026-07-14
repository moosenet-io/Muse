//! MUSET-07: the INTERROGABLE reasoning trace behind one recommendation.
//!
//! Muse's recommend path did not previously emit anything explanation-shaped
//! beyond `curation::recommend::template_rationale`/`build_rationale` — a
//! *user-facing sentence*, not a structured, machine-interrogable record of
//! which signals drove the result. This module adds the minimal structure
//! needed for an adversarial reviewer to ask "is this specific signal a
//! defensible driver, or spurious?" It is deliberately NOT a new signal
//! source: [`build_reasoning_trace`] only reads fields
//! `curation::candidates`/`curation::recommend` already computed
//! (`Candidate::facts`, `Candidate::taste_fit`, `Candidate::source`, and
//! `recommend::source_weight`/`recommend::score_candidate`'s own formula) —
//! every entry in [`ReasoningTrace::signals`] traces to a real, already-
//! computed value, never an invented one, same discipline as
//! `Candidate::facts` itself.

use serde::Serialize;

use crate::curation::candidates::{Candidate, CandidateSource};
use crate::curation::recommend::source_weight;

/// One signal's contribution to a recommendation's score/reasoning — the
/// unit an adversarial reviewer critiques.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SignalContribution {
    /// A short, stable identifier for which signal this is (e.g.
    /// `"taste_profile_cosine_similarity"`, `"watch_stats_percent_complete"`)
    /// — not free text, so a panel/finding can refer to *which* signal it's
    /// critiquing.
    pub signal: String,
    /// The signal's numeric weight in this candidate's scoring — currently
    /// `Candidate::taste_fit`, the normalized `[0.0, 1.0]` strength every
    /// source produces (see `candidates::Candidate::taste_fit` doc).
    pub weight: f64,
    /// The grounded, human-readable fact this signal came from — reused
    /// verbatim from `Candidate::facts`, never re-derived or invented.
    pub description: String,
}

/// The full interrogable reasoning trace behind one ranked recommendation.
#[derive(Debug, Clone, Serialize)]
pub struct ReasoningTrace {
    pub media_metadata_id: i64,
    pub title: String,
    pub source: CandidateSource,
    /// The final rank score `recommend::score_candidate` produced for this
    /// candidate (source-weight * taste_fit, availability-adjusted).
    pub score: f64,
    /// `Candidate::taste_fit` as-is — the raw, source-normalized signal
    /// strength the score was built from, before the source-tier weight.
    pub taste_fit: f64,
    /// `recommend::source_weight(source)` — the source-tier multiplier
    /// (on-deck > gap > taste > available-now) applied to `taste_fit`.
    pub source_weight: f64,
    /// The individual signals behind this recommendation, in the same
    /// order as `Candidate::facts`.
    pub signals: Vec<SignalContribution>,
    /// A short, human-readable description of the rule/path that produced
    /// this score — grounded in the real formula
    /// (`recommend::score_candidate`'s doc comment), not invented.
    pub path: String,
}

/// A short, stable label for the `index`-th fact of a candidate from
/// `source` — used only to give an adversarial reviewer a stable handle to
/// refer to a specific signal by; the actual grounded content always lives
/// in [`SignalContribution::description`] (verbatim from `Candidate::facts`).
fn signal_label(source: CandidateSource, index: usize) -> String {
    match (source, index) {
        (CandidateSource::Taste, 0) => "taste_profile_cosine_similarity".to_string(),
        (CandidateSource::Taste, _) => "genre_affinity_top".to_string(),
        (CandidateSource::OnDeck, _) => "watch_stats_percent_complete".to_string(),
        (CandidateSource::Gap, _) => "show_gap_engagement".to_string(),
        (CandidateSource::AvailableNow, 0) => "trending_popularity".to_string(),
        (CandidateSource::AvailableNow, _) => "availability_grabbability".to_string(),
    }
}

/// Build the reasoning trace for one already-scored candidate. Pure and
/// non-invasive: reads only fields the recommend pipeline already computed,
/// performs no I/O, and never changes `candidate`/`score` — this is meant to
/// be called alongside `recommend::build_rationale` in `score_and_explain`,
/// gated behind `RecommendRequest::include_trace` so a caller that doesn't
/// ask for a trace gets byte-for-byte the same response shape as before
/// this module existed.
pub fn build_reasoning_trace(candidate: &Candidate, score: f64) -> ReasoningTrace {
    let weight = source_weight(candidate.source);

    let signals = candidate
        .facts
        .iter()
        .enumerate()
        .map(|(i, fact)| SignalContribution {
            signal: signal_label(candidate.source, i),
            weight: candidate.taste_fit,
            description: fact.clone(),
        })
        .collect();

    let path = format!(
        "{:?} tier: score = source_weight({:?}) [{weight:.2}] * taste_fit [{:.2}], availability-adjusted, clamped >= 0.0",
        candidate.source, candidate.source, candidate.taste_fit
    );

    ReasoningTrace {
        media_metadata_id: candidate.media_metadata_id,
        title: candidate.title.clone(),
        source: candidate.source,
        score,
        taste_fit: candidate.taste_fit,
        source_weight: weight,
        signals,
        path,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::media_metadata::MediaKind;

    fn taste_candidate(facts: Vec<&str>, taste_fit: f64) -> Candidate {
        Candidate {
            media_metadata_id: 42,
            media_item_id: Some(42),
            title: "Arrival".to_string(),
            year: Some(2016),
            kind: MediaKind::Movie,
            source: CandidateSource::Taste,
            taste_fit,
            facts: facts.into_iter().map(str::to_string).collect(),
            availability: None,
        }
    }

    #[test]
    fn build_reasoning_trace_carries_every_real_fact_as_a_signal() {
        let candidate = taste_candidate(
            vec![
                "it's a 92% match to your overall taste profile",
                "you rate sci-fi highly",
            ],
            0.92,
        );
        let trace = build_reasoning_trace(&candidate, 0.644);

        assert_eq!(trace.media_metadata_id, 42);
        assert_eq!(trace.title, "Arrival");
        assert_eq!(trace.signals.len(), 2);
        assert_eq!(
            trace.signals[0].description,
            "it's a 92% match to your overall taste profile"
        );
        assert_eq!(trace.signals[1].description, "you rate sci-fi highly");
        // Every signal's weight is the real, already-computed taste_fit —
        // never invented.
        assert!((trace.signals[0].weight - 0.92).abs() < f64::EPSILON);
    }

    #[test]
    fn build_reasoning_trace_uses_the_real_source_weight() {
        let candidate = taste_candidate(vec!["a fact"], 0.5);
        let trace = build_reasoning_trace(&candidate, 0.35);

        assert!((trace.source_weight - source_weight(CandidateSource::Taste)).abs() < f64::EPSILON);
        assert!(trace.path.contains("Taste"));
    }

    #[test]
    fn build_reasoning_trace_never_invents_a_signal_beyond_the_facts() {
        let candidate = taste_candidate(vec![], 0.5);
        let trace = build_reasoning_trace(&candidate, 0.35);
        assert!(trace.signals.is_empty());
    }
}
