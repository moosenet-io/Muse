//! MUSEX-04 (Plane TERM #380): the "why this" narration surface — a
//! concise, Lumina-voiced "because…" line for every recommendation/slot.
//!
//! ## Why this exists
//! `curation::recommend::template_rationale`/`build_rationale` already
//! produce a user-facing sentence, and MUSET-07's
//! [`crate::taste_review::trace::ReasoningTrace`] already produces a
//! structured, INTERROGABLE record of which signals drove a rec. Neither is
//! quite this feature: the rationale is a full recommendation sentence
//! ("\"Arrival\" is recommended because …"), and the trace is a
//! machine-shaped critique target, not prose a human reads at a glance.
//! [`because_line`] is the short, trust-building surface in between — one
//! warm "because…" clause naming the actual top driver(s), meant to sit
//! next to a rec the same way a friend's one-line "because you liked X"
//! would.
//!
//! ## Grounding discipline (load-bearing)
//! [`because_line`] takes a [`ReasoningTrace`] — never a `Candidate`
//! directly, never free text — and does nothing but select and lightly
//! join a PREFIX of `trace.signals` (in trace order, which is
//! `Candidate::facts` order: see `candidates.rs`'s `gather_*_candidates`,
//! every source pushes its TIER-DEFINING fact first and any secondary
//! context fact after, so `signals[0]` is always the real primary driver,
//! never an arbitrary pick). It reuses each [`SignalContribution`]'s
//! `description` VERBATIM — the same grounded fact string
//! `build_reasoning_trace` copied unmodified from `Candidate::facts` — and
//! never invents a word beyond that. This mirrors the exact discipline
//! `curation::recommend::template_rationale` and
//! `proactive::generators::template_message` already use for their
//! deterministic, always-available templates: only fixed scaffolding words
//! ("Because", "and", punctuation) are added; every noun/number/fact comes
//! from a real, already-computed signal. There is deliberately no LLM
//! rephrase path here (unlike `build_rationale`/`build_message`) — the AC
//! calls for "concise, grounded in real signals," which the deterministic
//! template already satisfies, and skipping the optional LLM hop removes an
//! entire class of failure/latency/paraphrase-drift risk for what is meant
//! to be a small, always-present trust affordance on every rec.
//!
//! ## Narration shaping
//! Every `description` string `candidates.rs` produces is already written
//! in second person / warm plain language ("you're 61% through it — pick it
//! back up", "it's a 92% match to your overall taste profile") — the same
//! voice `proactive::generators::build_message`'s Lumina system prompt
//! describes ("a warm, concise personal assistant"). [`because_line`] only
//! adds the "Because …" framing device on top; it does not need to
//! re-voice the facts because they were already written in that voice at
//! the source.

use crate::taste_review::trace::ReasoningTrace;

/// How many of the trace's leading (highest-priority) signals go into a
/// "because…" line. Two keeps it CONCISE (the AC's own word) while still
/// covering the common case of a primary driver plus one secondary context
/// fact (e.g. taste-match percentage plus a top genre affinity).
const MAX_BECAUSE_SIGNALS: usize = 2;

/// Build the concise "because…" narration line for one recommendation's
/// reasoning trace, or `None` when the trace carries no signals to ground
/// one in. Deterministic (same trace in, same output out) and grounded
/// ONLY in `trace.signals` — see the module doc for the exact grounding
/// contract.
///
/// A trace with no signals yields `None`, NOT an invented line: the honest
/// behavior is "no grounded reason → no because line." This function must
/// never reference `trace.title`, a score, or any quality claim that isn't
/// the verbatim `description` of a real [`crate::taste_review::trace::SignalContribution`];
/// synthesizing a "still looks like a solid pick" style reason for a
/// signal-less trace would be exactly the fabrication the grounding
/// contract forbids, so the empty case is a `None`, full stop.
pub fn because_line(trace: &ReasoningTrace) -> Option<String> {
    if trace.signals.is_empty() {
        return None;
    }

    let reasons: Vec<&str> = trace
        .signals
        .iter()
        .take(MAX_BECAUSE_SIGNALS)
        .map(|signal| signal.description.as_str())
        .collect();

    Some(format!("Because {}.", reasons.join(" and ")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::curation::candidates::{Candidate, CandidateSource};
    use crate::models::media_metadata::MediaKind;
    use crate::taste_review::trace::build_reasoning_trace;

    fn candidate_with(source: CandidateSource, facts: Vec<&str>, taste_fit: f64) -> Candidate {
        Candidate {
            media_metadata_id: 99,
            media_item_id: Some(99),
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
    fn because_line_names_the_real_top_signal() {
        let candidate = candidate_with(
            CandidateSource::Taste,
            vec![
                "it's a 92% match to your overall taste profile",
                "you rate sci-fi highly",
            ],
            0.92,
        );
        let trace = build_reasoning_trace(&candidate, 0.644);
        let because = because_line(&trace);

        assert_eq!(
            because.as_deref(),
            Some(
                "Because it's a 92% match to your overall taste profile and you rate sci-fi highly."
            )
        );
    }

    #[test]
    fn because_line_is_deterministic_for_the_same_trace() {
        let candidate = candidate_with(
            CandidateSource::OnDeck,
            vec!["you're 61% through it — pick it back up"],
            0.61,
        );
        let trace = build_reasoning_trace(&candidate, 0.55);

        assert_eq!(because_line(&trace), because_line(&trace));
    }

    #[test]
    fn because_line_caps_at_two_signals_even_with_more_facts_in_the_trace() {
        // Not a real single-source combination in practice (dedup can merge
        // more than two facts onto one winning candidate — see
        // `trace.rs`'s cross-source test) but exercises the cap explicitly.
        let candidate = candidate_with(
            CandidateSource::Taste,
            vec![
                "it's a 92% match to your overall taste profile",
                "you rate sci-fi highly",
                "it's trending right now (popularity 87)",
            ],
            0.92,
        );
        let trace = build_reasoning_trace(&candidate, 0.5);
        let because = because_line(&trace).expect("a trace with signals must yield a because line");

        assert!(because.contains("92% match"));
        assert!(because.contains("sci-fi"));
        // Anti-fabrication + conciseness: the third, lower-priority signal
        // must NOT bleed into the line even though it's a real fact in the
        // trace — the AC calls for CONCISE, and the third fact wasn't in
        // the selected prefix.
        assert!(!because.contains("trending"));
    }

    /// The anti-fabrication guard the AC explicitly calls for: for a rec
    /// whose known signals are ONLY a taste-profile match and a genre
    /// affinity, the "because…" line must never mention vocabulary from a
    /// signal category that is genuinely absent from this trace (on-deck
    /// progress, show-gap airing, trending popularity, or availability) —
    /// proving the line is derived strictly from the real signals present,
    /// never invented.
    #[test]
    fn because_line_never_mentions_a_signal_absent_from_the_trace() {
        let candidate = candidate_with(
            CandidateSource::Taste,
            vec![
                "it's a 92% match to your overall taste profile",
                "you rate sci-fi highly",
            ],
            0.92,
        );
        let trace = build_reasoning_trace(&candidate, 0.644);
        let because = because_line(&trace).expect("a trace with signals must yield a because line");

        // Vocabulary drawn from signal categories genuinely absent from
        // this trace's `signals` (on-deck / gap / trending / availability
        // templates in `candidates.rs`) — none of it may appear.
        let absent_signal_vocabulary = [
            "pick it back up",
            "through it",
            "episode is scheduled",
            "isn't done airing",
            "deep into this show",
            "trending",
            "grabbable",
            "not currently available",
            "hasn't been checked yet",
        ];
        for phrase in absent_signal_vocabulary {
            assert!(
                !because.contains(phrase),
                "because line {because:?} must not mention {phrase:?} — that signal is absent from this trace"
            );
        }
    }

    /// Anti-fabrication guard for the EMPTY case: a trace with no signals
    /// has nothing real to ground a reason in, so `because_line` must
    /// return `None` — never a synthesized "still looks like a solid pick"
    /// line or anything referencing `trace.title`. "No grounded reason →
    /// no because line" is the honest behavior; inventing a reason here
    /// would be exactly the fabrication the grounding contract forbids.
    #[test]
    fn because_line_returns_none_and_never_fabricates_when_the_trace_has_no_signals() {
        let candidate = candidate_with(CandidateSource::Taste, vec![], 0.5);
        let trace = build_reasoning_trace(&candidate, 0.35);

        // The trace genuinely carries no signals, and the title alone is
        // not a grounded reason — so there is no because line at all.
        assert!(trace.signals.is_empty());
        assert_eq!(
            because_line(&trace),
            None,
            "an empty-signal trace must yield NO because line, never an invented one"
        );
    }

    #[test]
    fn because_line_for_on_deck_uses_the_real_progress_fact_only() {
        let candidate = candidate_with(
            CandidateSource::OnDeck,
            vec!["you're 61% through it — pick it back up"],
            0.61,
        );
        let trace = build_reasoning_trace(&candidate, 0.55);
        let because = because_line(&trace).expect("a trace with signals must yield a because line");

        assert_eq!(because, "Because you're 61% through it — pick it back up.");
        assert!(!because.contains("taste profile"));
        assert!(!because.contains("trending"));
    }
}
