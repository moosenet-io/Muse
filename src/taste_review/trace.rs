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
//!
//! Each signal is labelled by the FACT'S OWN provenance
//! ([`classify_signal`]), not by the candidate's winning `source` — because
//! `curation::candidates::dedup_candidates` merges the facts of losing
//! sources into the highest-priority winner, so a fact's origin is not the
//! same as the candidate's `source`. Labelling by `source` would mislabel a
//! merged cross-source fact and hand the adversarial panel reasoning that
//! never happened; see [`classify_signal`]'s own doc.

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

/// A short, stable label identifying which signal a fact came from —
/// derived from the FACT'S OWN CONTENT, never from the candidate's winning
/// `source`. This distinction is load-bearing: `dedup_candidates`
/// (`crate::curation::candidates::dedup_candidates`) keeps the
/// highest-priority source as the winner's `source` but MERGES in the facts
/// of every losing source, so a candidate whose `source` is `OnDeck` can
/// legitimately carry a taste-tier fact (e.g. "it's a 92% match to your
/// overall taste profile"). Labelling by `source` would then mislabel that
/// merged fact as an on-deck signal — inventing reasoning that never
/// happened, exactly what an adversarial critique must not be fed. Keying
/// off the fact's own distinctive phrasing (each source in `candidates.rs`
/// produces a unique, stable template — the sole producer of these strings)
/// labels every fact by its TRUE origin regardless of which source won
/// dedup. Anything unrecognized falls back to `"unclassified_signal"` — a
/// neutral label, never a wrong source's label.
///
/// The substrings matched here are the invariant parts of the `format!`
/// templates in `crate::curation::candidates` (`gather_taste_candidates`,
/// `gather_on_deck_candidates`, `gather_gap_candidates`,
/// `gather_available_now_candidates`); the module test
/// `every_candidate_source_fact_is_classified_by_its_own_content` guards
/// against those templates drifting out from under this mapping.
pub(crate) fn classify_signal(fact: &str) -> &'static str {
    // Taste tier
    if fact.contains("match to your overall taste profile") {
        "taste_profile_cosine_similarity"
    } else if fact.contains("you rate ") && fact.contains(" highly") {
        "genre_affinity_top"
    // On-deck / continue-watching tier
    } else if fact.contains("through it") || fact.contains("pick it back up") {
        "watch_stats_percent_complete"
    // Gap-analysis tier
    } else if fact.contains("a new episode is scheduled for")
        || fact.contains("isn't done airing yet")
        || fact.contains("deep into this show")
    {
        "show_gap_engagement"
    // Availability-aware, not-in-library tier
    } else if fact.contains("trending right now") {
        "trending_popularity"
    } else if fact.contains("grabbable now")
        || fact.contains("not currently available")
        || fact.contains("availability hasn't been checked yet")
    {
        "availability_grabbability"
    } else {
        "unclassified_signal"
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
        .map(|fact| SignalContribution {
            signal: classify_signal(fact).to_string(),
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
    use crate::curation::candidates::dedup_candidates;
    use crate::models::media_metadata::MediaKind;

    fn taste_candidate(facts: Vec<&str>, taste_fit: f64) -> Candidate {
        candidate_with(42, CandidateSource::Taste, facts, taste_fit)
    }

    fn candidate_with(
        id: i64,
        source: CandidateSource,
        facts: Vec<&str>,
        taste_fit: f64,
    ) -> Candidate {
        Candidate {
            media_metadata_id: id,
            media_item_id: Some(id),
            title: "Arrival".to_string(),
            year: Some(2016),
            kind: MediaKind::Movie,
            source,
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
        // Signals are labelled from each fact's own content.
        assert_eq!(trace.signals[0].signal, "taste_profile_cosine_similarity");
        assert_eq!(trace.signals[1].signal, "genre_affinity_top");
        // Every signal's weight is the real, already-computed taste_fit —
        // never invented.
        assert!((trace.signals[0].weight - 0.92).abs() < f64::EPSILON);
    }

    /// The codex finding: `dedup_candidates` keeps the highest-priority
    /// SOURCE as the winner but MERGES in the losing sources' facts, so a
    /// winner whose `source` is `OnDeck` can carry a taste-tier fact. The
    /// trace must label that merged fact by ITS OWN provenance (a taste
    /// signal), never as the winner's on-deck signal — otherwise it feeds
    /// the adversarial panel reasoning that never actually happened.
    #[test]
    fn merged_cross_source_fact_is_labelled_by_its_own_provenance_not_the_winning_source() {
        // Same media_metadata_id surfaced by two sources: on-deck (wins
        // dedup) + taste (loses, but its fact is merged into the winner).
        let on_deck = candidate_with(
            7,
            CandidateSource::OnDeck,
            vec!["you're 61% through it — pick it back up"],
            0.61,
        );
        let taste = candidate_with(
            7,
            CandidateSource::Taste,
            vec!["it's a 92% match to your overall taste profile"],
            0.92,
        );

        let deduped = dedup_candidates(vec![on_deck, taste]);
        assert_eq!(deduped.len(), 1);
        let winner = &deduped[0];
        // On-deck won the source, but carries both facts.
        assert_eq!(winner.source, CandidateSource::OnDeck);
        assert_eq!(winner.facts.len(), 2);

        let trace = build_reasoning_trace(winner, 0.55);
        assert_eq!(trace.source, CandidateSource::OnDeck);
        assert_eq!(trace.signals.len(), 2);

        // Locate each fact by its content and assert its TRUE signal label.
        let on_deck_sig = trace
            .signals
            .iter()
            .find(|s| s.description.contains("through it"))
            .expect("on-deck fact must be present");
        let taste_sig = trace
            .signals
            .iter()
            .find(|s| {
                s.description
                    .contains("match to your overall taste profile")
            })
            .expect("merged taste fact must be present");

        assert_eq!(on_deck_sig.signal, "watch_stats_percent_complete");
        assert_eq!(
            taste_sig.signal, "taste_profile_cosine_similarity",
            "the MERGED taste fact must be labelled a taste signal, NOT the winning OnDeck source's signal"
        );
    }

    /// Guards `classify_signal` against the `candidates.rs` fact templates
    /// drifting out from under it: every real fact string each source
    /// produces must classify to a concrete signal, never the
    /// `"unclassified_signal"` fallback.
    #[test]
    fn every_candidate_source_fact_is_classified_by_its_own_content() {
        let cases: &[(&str, &str)] = &[
            (
                "it's a 92% match to your overall taste profile",
                "taste_profile_cosine_similarity",
            ),
            ("you rate sci-fi highly", "genre_affinity_top"),
            (
                "you're 61% through it — pick it back up",
                "watch_stats_percent_complete",
            ),
            (
                "a new episode is scheduled for 2026-08-01",
                "show_gap_engagement",
            ),
            (
                "its status (\"Continuing\") means it isn't done airing yet",
                "show_gap_engagement",
            ),
            ("you're deep into this show", "show_gap_engagement"),
            (
                "it's trending right now (popularity 87)",
                "trending_popularity",
            ),
            (
                "grabbable now (40 seeders, freeleech)",
                "availability_grabbability",
            ),
            ("not currently available", "availability_grabbability"),
            (
                "availability hasn't been checked yet",
                "availability_grabbability",
            ),
        ];

        for (fact, expected) in cases {
            let got = classify_signal(fact);
            assert_eq!(
                got, *expected,
                "fact {fact:?} should classify as {expected:?}, got {got:?}"
            );
            assert_ne!(got, "unclassified_signal");
        }

        // An unrecognized fact falls back neutrally — never mislabelled.
        assert_eq!(
            classify_signal("some fact we have never seen before"),
            "unclassified_signal"
        );
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
