//! MUSET-07: orchestration — turn a [`ReasoningTrace`] + panel + sink into a
//! [`ReviewOutcome`].
//!
//! Three, and only three, outcomes:
//! 1. [`ReviewOutcome::Sound`] — panel reached consensus the reasoning is
//!    defensible. No finding, no escalation.
//! 2. [`ReviewOutcome::FiledSpurious`] — panel reached consensus the
//!    reasoning is spurious/overfit/stale. A [`TasteQualityFinding`] is
//!    filed via [`FindingSink`] and returned.
//! 3. [`ReviewOutcome::EscalatedNoConsensus`] — the panel split. This is
//!    deliberately NOT collapsed into either of the other two outcomes: a
//!    split vote is neither "confirmed sound" nor "confirmed spurious," and
//!    silently defaulting either way would either bury a real reasoning
//!    defect or file a finding nobody actually agreed on. It escalates to a
//!    human instead.

use crate::error::MuseResult;
use crate::taste_review::panel::{PanelVerdict, ReasoningPanel, RecommendationSummary};
use crate::taste_review::sink::{FindingSink, TasteQualityFinding};
use crate::taste_review::trace::ReasoningTrace;

/// The outcome of running one recommendation's reasoning through adversarial
/// review.
#[derive(Debug, Clone, PartialEq)]
pub enum ReviewOutcome {
    /// Panel reached consensus that the reasoning is a defensible driver.
    Sound,
    /// Panel reached consensus the reasoning is spurious/overfit/stale; the
    /// returned finding was successfully filed via the configured
    /// [`FindingSink`].
    FiledSpurious(TasteQualityFinding),
    /// The panel did not reach consensus (a split vote) -- routed to a
    /// human instead of being silently resolved either way.
    EscalatedNoConsensus(PanelVerdict),
}

/// Run one recommendation's [`ReasoningTrace`] through the adversarial
/// panel, and route the result: consensus-spurious -> file a finding via
/// `sink`; no-consensus -> escalate to a human; consensus-sound -> nothing
/// further happens. `panel`/`sink` are trait objects so this function is
/// identical whether called with the real Terminus-backed impls or the
/// in-process mocks -- no live external calls happen here directly, only
/// through whichever seam implementation the caller supplied.
pub async fn review_recommendation(
    trace: &ReasoningTrace,
    rec: &RecommendationSummary,
    panel: &dyn ReasoningPanel,
    sink: &dyn FindingSink,
) -> MuseResult<ReviewOutcome> {
    let verdict = panel.critique(trace, rec).await?;

    if !verdict.consensus {
        tracing::warn!(
            media_metadata_id = trace.media_metadata_id,
            title = %trace.title,
            "MUSET-07: adversarial panel reached no consensus on reasoning soundness for this recommendation; escalating to human review"
        );
        return Ok(ReviewOutcome::EscalatedNoConsensus(verdict));
    }

    if verdict.spurious {
        let summary = format!(
            "Adversarial reasoning panel reached consensus that the reasoning behind recommending \"{}\" \
            (media_metadata_id {}) is spurious/overfit/stale, not a defensible driver. Path: {}. Per-agent critiques: {}",
            trace.title,
            trace.media_metadata_id,
            trace.path,
            verdict
                .per_agent
                .iter()
                .map(|a| format!("{} ({}): {}", a.agent, if a.spurious { "spurious" } else { "sound" }, a.critique))
                .collect::<Vec<_>>()
                .join(" | "),
        );

        let finding = TasteQualityFinding {
            media_metadata_id: trace.media_metadata_id,
            title: trace.title.clone(),
            trace_path: trace.path.clone(),
            verdict: verdict.clone(),
            summary,
        };

        sink.file(&finding).await?;
        tracing::info!(
            media_metadata_id = trace.media_metadata_id,
            title = %trace.title,
            "MUSET-07: filed a taste-quality finding — adversarial panel consensus on spurious reasoning"
        );
        return Ok(ReviewOutcome::FiledSpurious(finding));
    }

    Ok(ReviewOutcome::Sound)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::curation::candidates::{Candidate, CandidateSource};
    use crate::curation::recommend::score_candidate;
    use crate::models::media_metadata::MediaKind;
    use crate::taste_review::panel::{AgentCritique, MockReasoningPanel};
    use crate::taste_review::sink::MockFindingSink;
    use crate::taste_review::trace::build_reasoning_trace;

    /// A SOUND candidate: taste_fit and facts are grounded in a broad,
    /// current signal (a genuinely high cross-genre taste-profile match) --
    /// nothing here should look spurious to a reasonable reviewer.
    fn sound_candidate() -> Candidate {
        Candidate {
            media_metadata_id: 1,
            media_item_id: Some(1),
            title: "Arrival".to_string(),
            year: Some(2016),
            kind: MediaKind::Movie,
            source: CandidateSource::Taste,
            taste_fit: 0.91,
            facts: vec![
                "it's a 91% match to your overall taste profile".to_string(),
                "you rate sci-fi highly".to_string(),
            ],
            availability: None,
        }
    }

    /// The PLANTED-SPURIOUS-SIGNAL negative test fixture: a candidate whose
    /// only grounding fact is a single, old, one-off rating driving the
    /// entire recommendation -- exactly the "single-genre overfit / stale
    /// signal" shape the adversarial panel exists to catch. The `taste_fit`
    /// is inflated (0.97) purely off that one stale signal, with no
    /// corroborating broad-profile evidence -- planted so a critique flow
    /// that's actually looking at the trace (not just rubber-stamping) has
    /// something real to flag.
    fn spurious_candidate() -> Candidate {
        Candidate {
            media_metadata_id: 2,
            media_item_id: Some(2),
            title: "Ancient Slasher VII".to_string(),
            year: Some(1998),
            kind: MediaKind::Movie,
            source: CandidateSource::Taste,
            taste_fit: 0.97,
            facts: vec![
                "you rated one horror movie 10/10 three years ago and nothing else in that genre since"
                    .to_string(),
            ],
            availability: None,
        }
    }

    fn rec_for(candidate: &Candidate, score: f64) -> RecommendationSummary {
        RecommendationSummary {
            media_metadata_id: candidate.media_metadata_id,
            title: candidate.title.clone(),
            rationale: format!(
                "\"{}\" is recommended because {}.",
                candidate.title,
                candidate.facts.join("; ")
            ),
        }
    }

    /// A stub panel whose "critique" is deterministic and content-aware: it
    /// flags a trace as spurious whenever a signal's grounding text mentions
    /// staleness/one-off evidence ("years ago", "nothing else... since"),
    /// and sound otherwise. This stands in for a real adversarial LLM panel
    /// reading the trace and reacting to what's actually in it, without
    /// needing a live model -- the orchestration logic under test doesn't
    /// know or care that the "reasoning" behind this stub is a keyword
    /// check rather than an LLM.
    struct ContentAwareStubPanel;

    #[async_trait::async_trait]
    impl ReasoningPanel for ContentAwareStubPanel {
        async fn critique(
            &self,
            trace: &ReasoningTrace,
            _rec: &RecommendationSummary,
        ) -> MuseResult<PanelVerdict> {
            let looks_stale_or_overfit = trace.signals.iter().any(|s| {
                let d = s.description.to_lowercase();
                (d.contains("years ago") && d.contains("nothing else")) || d.contains("one-off")
            });

            let per_agent = vec![
                AgentCritique {
                    agent: "opus".to_string(),
                    spurious: looks_stale_or_overfit,
                    critique: if looks_stale_or_overfit {
                        "a single old rating with no corroborating recent signal is a stale, overfit driver"
                            .to_string()
                    } else {
                        "grounded in a broad, current taste-profile match -- defensible".to_string()
                    },
                },
                AgentCritique {
                    agent: "diffusion-gemma".to_string(),
                    spurious: looks_stale_or_overfit,
                    critique: if looks_stale_or_overfit {
                        "single-genre overfit off one signal from years ago".to_string()
                    } else {
                        "consistent with multiple current signals -- defensible".to_string()
                    },
                },
            ];

            Ok(crate::taste_review::panel::aggregate_verdict(per_agent))
        }
    }

    #[tokio::test]
    async fn planted_spurious_signal_is_caught_and_routed_to_the_finding_sink() {
        let candidate = spurious_candidate();
        let score = score_candidate(&candidate);
        let trace = build_reasoning_trace(&candidate, score);
        let rec = rec_for(&candidate, score);

        let panel = ContentAwareStubPanel;
        let sink = MockFindingSink::new();

        let outcome = review_recommendation(&trace, &rec, &panel, &sink)
            .await
            .expect("review should not error");

        match &outcome {
            ReviewOutcome::FiledSpurious(finding) => {
                assert_eq!(finding.media_metadata_id, 2);
                assert_eq!(finding.title, "Ancient Slasher VII");
                assert!(finding.verdict.consensus);
                assert!(finding.verdict.spurious);
            }
            other => panic!("expected FiledSpurious, got {other:?}"),
        }

        let filed = sink.filed_findings();
        assert_eq!(
            filed.len(),
            1,
            "the spurious finding must actually reach the sink"
        );
        assert_eq!(filed[0].media_metadata_id, 2);
        assert!(
            filed[0].summary.contains("stale") || filed[0].summary.contains("overfit"),
            "filed finding summary should carry the critique content: {}",
            filed[0].summary
        );
    }

    #[tokio::test]
    async fn a_sound_trace_produces_no_finding() {
        let candidate = sound_candidate();
        let score = score_candidate(&candidate);
        let trace = build_reasoning_trace(&candidate, score);
        let rec = rec_for(&candidate, score);

        let panel = ContentAwareStubPanel;
        let sink = MockFindingSink::new();

        let outcome = review_recommendation(&trace, &rec, &panel, &sink)
            .await
            .expect("review should not error");

        assert_eq!(outcome, ReviewOutcome::Sound);
        assert!(
            sink.filed_findings().is_empty(),
            "a sound trace must never file a finding"
        );
    }

    #[tokio::test]
    async fn no_consensus_escalates_to_human_and_never_files() {
        let candidate = spurious_candidate();
        let score = score_candidate(&candidate);
        let trace = build_reasoning_trace(&candidate, score);
        let rec = rec_for(&candidate, score);

        // A split-vote mock: one agent says spurious, the other says sound.
        let panel = MockReasoningPanel::new(vec![
            AgentCritique {
                agent: "opus".to_string(),
                spurious: true,
                critique: "looks overfit".to_string(),
            },
            AgentCritique {
                agent: "diffusion-gemma".to_string(),
                spurious: false,
                critique: "seems fine to me".to_string(),
            },
        ]);
        let sink = MockFindingSink::new();

        let outcome = review_recommendation(&trace, &rec, &panel, &sink)
            .await
            .expect("review should not error");

        match &outcome {
            ReviewOutcome::EscalatedNoConsensus(verdict) => {
                assert!(!verdict.consensus);
            }
            other => panic!("expected EscalatedNoConsensus, got {other:?}"),
        }
        assert!(
            sink.filed_findings().is_empty(),
            "a no-consensus split vote must never silently file a finding"
        );
    }

    #[tokio::test]
    async fn mock_reasoning_panel_end_to_end_with_sound_candidate_files_nothing() {
        let candidate = sound_candidate();
        let score = score_candidate(&candidate);
        let trace = build_reasoning_trace(&candidate, score);
        let rec = rec_for(&candidate, score);

        let panel = MockReasoningPanel::new(vec![
            AgentCritique {
                agent: "opus".to_string(),
                spurious: false,
                critique: "defensible".to_string(),
            },
            AgentCritique {
                agent: "diffusion-gemma".to_string(),
                spurious: false,
                critique: "defensible".to_string(),
            },
        ]);
        let sink = MockFindingSink::new();

        let outcome = review_recommendation(&trace, &rec, &panel, &sink)
            .await
            .expect("no error");
        assert_eq!(outcome, ReviewOutcome::Sound);
    }
}
