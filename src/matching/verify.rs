//! MUSEL-C2: `verify_match` -- THE critical "is the match real?" check.
//!
//! Given an identified file's observed properties, its provider metadata
//! ([`ProviderMetadata`], MUSEL-A1), and a set of sample stills extracted
//! from it ([`extract_sample_stills`](crate::matching::stills::extract_sample_stills),
//! MUSEL-C1), [`verify_match`] combines three optional/graceful signals --
//! a local VLM via Chord ([`crate::matching::vision`]), still-liveness
//! ([`crate::matching::liveness`]), and metadata consistency -- into a
//! single [`MatchVerdict`].
//!
//! **This is verdict-only.** `verify_match` takes every input by shared
//! reference and returns a plain value -- it has no path to delete,
//! re-tag, or otherwise mutate the file, the library, or the metadata. An
//! `Inconsistent` verdict FLAGS the match for operator review; it never
//! acts on its own. Callers that build a scan report are the ones with
//! write access, and this module gives them no write API to call.

use crate::matching::liveness::{self, LivenessOutcome};
use crate::matching::stills::Still;
use crate::matching::vision::{VisionAnswer, VisionVerifier};
use crate::metadata::{MediaKind, ProviderMetadata};

/// A gross runtime disagreement -- beyond this fraction of the provider's
/// stated runtime -- is treated as a hard metadata contradiction, one that
/// alone can drive an `Inconsistent` verdict even with no vision signal.
const GROSS_RUNTIME_DISAGREEMENT_FRACTION: f64 = 0.5;

/// Ignore small absolute runtime differences (container padding,
/// end-credits, a few seconds of rounding) even if they happen to clear
/// the fractional threshold on a very short title.
const MISMATCH_RUNTIME_MIN_ABS_MS: i64 = 5 * 60_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerdictOutcome {
    Consistent,
    Inconsistent,
    Inconclusive,
}

#[derive(Debug, Clone, PartialEq)]
pub struct MatchVerdict {
    pub outcome: VerdictOutcome,
    pub confidence: f32,
    pub reasons: Vec<String>,
}

/// What's observed about the file itself -- distinct from the provider's
/// *claim* in [`ProviderMetadata`] -- used to sanity-check one against the
/// other. `observed_runtime_ms` is expected to come from a real probe of
/// the file (e.g. an ffprobe duration), not from the identification step
/// that produced `metadata` in the first place.
#[derive(Debug, Clone, PartialEq)]
pub struct FileObservation {
    pub path: String,
    pub kind: MediaKind,
    pub observed_runtime_ms: Option<i64>,
}

/// Combine the VLM, still-liveness, and metadata-consistency signals into
/// one [`MatchVerdict`]. Never panics, never mutates any input, and never
/// fails the caller on a missing/erroring signal -- each signal degrades
/// gracefully and the verdict is computed from whatever is available.
///
/// - `vision` present: the strongest discriminator. A "no" verdict from
///   the model drives `Inconsistent` even if metadata looks fine; a "yes"
///   is only trusted as `Consistent` when metadata doesn't hard-contradict
///   it.
/// - `vision` absent: the verdict comes from liveness + metadata alone --
///   a weaker `Consistent` (never the vision-backed confidence level) when
///   nothing contradicts, `Inconclusive` when there's nothing to judge,
///   and still a clear `Inconsistent` on a hard liveness/metadata
///   contradiction (a mislabeled file doesn't get a free pass just
///   because no vision model is configured).
/// - Liveness failure (all sampled stills near-uniform/blank, or all
///   identical) is a HARD stop: it drives `Inconsistent` regardless of
///   what the VLM says, because dead/stuck content can't be verified
///   against any title, "yes" answer or not.
pub async fn verify_match(
    file: &FileObservation,
    metadata: &ProviderMetadata,
    stills: &[Still],
    vision: Option<&dyn VisionVerifier>,
) -> MatchVerdict {
    let mut reasons = Vec::new();

    let liveness_verdict = liveness::check_liveness(stills);
    reasons.extend(liveness_verdict.reasons.clone());

    let (metadata_hard_contradiction, metadata_reasons) = metadata_consistency(file, metadata);
    reasons.extend(metadata_reasons);

    let mut vision_answer: Option<VisionAnswer> = None;
    if let (Some(verifier), Some(title), Some(still)) = (vision, metadata.title.as_deref(), stills.first()) {
        match verifier.is_consistent(still, title, file.kind, metadata.year).await {
            Ok(Some(answer)) => {
                reasons.push(format!(
                    "vlm: {} (confidence {:.2}) - {}",
                    if answer.consistent { "consistent" } else { "inconsistent" },
                    answer.confidence,
                    answer.explanation
                ));
                vision_answer = Some(answer);
            }
            Ok(None) => {
                reasons.push("vlm: no parseable answer returned; signal skipped".to_string());
            }
            Err(e) => {
                reasons.push(format!("vlm: request failed ({e}); signal skipped"));
            }
        }
    } else if vision.is_some() {
        reasons.push("vlm: no title/stills available to ask about; signal skipped".to_string());
    }

    combine(liveness_verdict.outcome, metadata_hard_contradiction, vision_answer, reasons)
}

/// Runtime vs provider runtime, the metadata-consistency signal named in
/// the spec. Returns whether the disagreement is severe enough to be a
/// hard contradiction, plus the human-readable reasons either way.
/// Unavailable data (no observed runtime, or the provider doesn't state
/// one) is reported but never treated as a contradiction on its own --
/// missing data isn't evidence of a mismatch.
fn metadata_consistency(file: &FileObservation, metadata: &ProviderMetadata) -> (bool, Vec<String>) {
    let mut reasons = Vec::new();

    match (file.observed_runtime_ms, metadata.runtime_minutes) {
        (Some(observed_ms), Some(provider_minutes)) if provider_minutes > 0 => {
            let provider_ms = provider_minutes as i64 * 60_000;
            let diff_ms = (observed_ms - provider_ms).abs();
            let fraction_off = diff_ms as f64 / provider_ms as f64;

            if diff_ms >= MISMATCH_RUNTIME_MIN_ABS_MS && fraction_off >= GROSS_RUNTIME_DISAGREEMENT_FRACTION {
                reasons.push(format!(
                    "runtime mismatch: observed {}m vs provider {}m ({:.0}% off)",
                    observed_ms / 60_000,
                    provider_minutes,
                    fraction_off * 100.0
                ));
                (true, reasons)
            } else {
                reasons.push("runtime consistent with provider metadata".to_string());
                (false, reasons)
            }
        }
        _ => {
            reasons.push("runtime unavailable for comparison (observed and/or provider runtime missing)".to_string());
            (false, reasons)
        }
    }
}

fn combine(
    liveness: LivenessOutcome,
    metadata_hard_contradiction: bool,
    vision: Option<VisionAnswer>,
    reasons: Vec<String>,
) -> MatchVerdict {
    // Nothing to judge at all -- distinct from a confirmed failure.
    if matches!(liveness, LivenessOutcome::Empty) {
        return MatchVerdict { outcome: VerdictOutcome::Inconclusive, confidence: 0.2, reasons };
    }

    // A hard liveness failure overrides everything, including a "yes"
    // from vision: dead/stuck content can't actually confirm a title.
    if matches!(liveness, LivenessOutcome::Uniform | LivenessOutcome::AllIdentical) {
        return MatchVerdict { outcome: VerdictOutcome::Inconsistent, confidence: 0.7, reasons };
    }

    match vision {
        Some(answer) if answer.consistent && !metadata_hard_contradiction => MatchVerdict {
            outcome: VerdictOutcome::Consistent,
            confidence: (0.75 + 0.2 * answer.confidence as f64) as f32,
            reasons,
        },
        Some(answer) if answer.consistent => {
            // Vision says yes, but metadata screams no -- don't let one
            // signal blindly override a hard contradiction in the other.
            MatchVerdict {
                outcome: VerdictOutcome::Inconsistent,
                confidence: (0.6 + 0.1 * answer.confidence as f64) as f32,
                reasons,
            }
        }
        Some(answer) => {
            // Vision says the frame does NOT match -- the strongest
            // discriminator available, trusted directly.
            MatchVerdict {
                outcome: VerdictOutcome::Inconsistent,
                confidence: (0.75 + 0.2 * answer.confidence as f64) as f32,
                reasons,
            }
        }
        None if metadata_hard_contradiction => {
            MatchVerdict { outcome: VerdictOutcome::Inconsistent, confidence: 0.6, reasons }
        }
        None => {
            // VLM-absent, nothing contradicts: a weaker Consistent, never
            // the vision-backed confidence level (never a false claim of
            // vision confirmation with no vision signal present).
            MatchVerdict { outcome: VerdictOutcome::Consistent, confidence: 0.5, reasons }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::MuseResult;
    use async_trait::async_trait;

    struct MockVision {
        consistent: bool,
        confidence: f32,
    }

    #[async_trait]
    impl VisionVerifier for MockVision {
        async fn is_consistent(
            &self,
            _still: &Still,
            _title: &str,
            _kind: MediaKind,
            _year: Option<i32>,
        ) -> MuseResult<Option<VisionAnswer>> {
            Ok(Some(VisionAnswer {
                consistent: self.consistent,
                confidence: self.confidence,
                explanation: "mock vision answer".to_string(),
            }))
        }
    }

    /// A synthetic "live" still -- varied byte content whose mean/variance
    /// genuinely differ per `seed` (see `liveness.rs`'s test-helper doc
    /// comment for why a plain `% 256` wraparound shift doesn't actually
    /// vary across seeds). This heuristic never decodes real JPEG pixels
    /// (see `liveness.rs`'s module doc comment), so a deterministic varied
    /// byte pattern is a faithful stand-in without needing a real media
    /// fixture.
    fn live_still(seed: i64) -> Still {
        Still {
            bytes: (0..4000_i64).map(|i| ((i % 200) + seed * 25).clamp(0, 255) as u8).collect(),
            timestamp_ms: seed * 1000,
        }
    }

    fn black_still(seed: i64) -> Still {
        Still { bytes: vec![0u8; 3000], timestamp_ms: seed * 1000 }
    }

    fn correct_metadata() -> ProviderMetadata {
        ProviderMetadata {
            title: Some("Arrival".to_string()),
            year: Some(2016),
            runtime_minutes: Some(116),
            ..Default::default()
        }
    }

    fn file_with_runtime(observed_ms: i64) -> FileObservation {
        FileObservation {
            path: "/media/Movies/Arrival (2016)/Arrival.mkv".to_string(),
            kind: MediaKind::Movie,
            observed_runtime_ms: Some(observed_ms),
        }
    }

    // --- Positive: a correct match should come back Consistent. ---

    #[tokio::test]
    async fn positive_correct_match_with_vision_yes_is_consistent() {
        let stills = vec![live_still(1), live_still(2), live_still(3)];
        let file = file_with_runtime(116 * 60_000); // matches provider exactly
        let metadata = correct_metadata();
        let vision = MockVision { consistent: true, confidence: 0.95 };

        let verdict = verify_match(&file, &metadata, &stills, Some(&vision)).await;

        assert_eq!(verdict.outcome, VerdictOutcome::Consistent);
        assert!(verdict.confidence >= 0.8, "expected high confidence, got {}", verdict.confidence);
    }

    // --- Negative / mismatch: THE critical discrimination tests. ---

    #[tokio::test]
    async fn mismatch_vision_says_no_and_wrong_runtime_is_inconsistent() {
        // Provider metadata claims Arrival (116m), but this is actually a
        // mislabeled 45-minute file -- both the runtime and the vision
        // model disagree with the claimed title.
        let stills = vec![live_still(1), live_still(2), live_still(3)];
        let file = file_with_runtime(45 * 60_000);
        let metadata = correct_metadata();
        let vision = MockVision { consistent: false, confidence: 0.9 };

        let verdict = verify_match(&file, &metadata, &stills, Some(&vision)).await;

        assert_eq!(verdict.outcome, VerdictOutcome::Inconsistent);
        assert!(verdict.reasons.iter().any(|r| r.contains("runtime mismatch")));
        assert!(verdict.reasons.iter().any(|r| r.contains("vlm: inconsistent")));
    }

    #[tokio::test]
    async fn all_black_stills_are_inconsistent_regardless_of_a_vision_yes() {
        // A file that decodes to nothing but black frames must never come
        // back Consistent, even if a (implausible) vision "yes" is
        // returned -- dead content can't confirm anything.
        let stills = vec![black_still(1), black_still(2), black_still(3)];
        let file = file_with_runtime(116 * 60_000);
        let metadata = correct_metadata();
        let vision = MockVision { consistent: true, confidence: 0.95 };

        let verdict = verify_match(&file, &metadata, &stills, Some(&vision)).await;

        assert_ne!(verdict.outcome, VerdictOutcome::Consistent);
        assert!(matches!(
            verdict.outcome,
            VerdictOutcome::Inconsistent | VerdictOutcome::Inconclusive
        ));
        assert!(verdict.reasons.iter().any(|r| r.contains("uniform") || r.contains("blank")));
    }

    #[tokio::test]
    async fn gross_runtime_disagreement_flags_even_without_vision() {
        // Provider says 116m; the file observably runs 22m -- grossly
        // wrong, and this must be caught even with no VLM configured.
        let stills = vec![live_still(1), live_still(2), live_still(3)];
        let file = file_with_runtime(22 * 60_000);
        let metadata = correct_metadata();

        let verdict = verify_match(&file, &metadata, &stills, None).await;

        assert_eq!(verdict.outcome, VerdictOutcome::Inconsistent);
        assert!(verdict.reasons.iter().any(|r| r.contains("runtime mismatch")));
    }

    // --- VLM-absent path. ---

    #[tokio::test]
    async fn vlm_absent_never_fabricates_a_vision_backed_consistent() {
        let stills = vec![live_still(1), live_still(2), live_still(3)];
        let file = file_with_runtime(116 * 60_000); // runtime matches, no contradiction
        let metadata = correct_metadata();

        let verdict = verify_match(&file, &metadata, &stills, None).await;

        // No panic (already true if we got here). No fabricated
        // vision-backed reasoning, and never the same high-confidence
        // Consistent a real vision "yes" would produce.
        assert!(!verdict.reasons.iter().any(|r| r.starts_with("vlm: consistent")));
        assert_ne!(verdict.outcome, VerdictOutcome::Inconsistent);
        assert!(
            verdict.confidence < 0.8,
            "VLM-absent confidence should be lower than a vision-backed match, got {}",
            verdict.confidence
        );
    }

    #[tokio::test]
    async fn vlm_absent_with_no_stills_is_inconclusive_not_a_crash() {
        let file = file_with_runtime(116 * 60_000);
        let metadata = correct_metadata();

        let verdict = verify_match(&file, &metadata, &[], None).await;

        assert_eq!(verdict.outcome, VerdictOutcome::Inconclusive);
    }

    // --- Verdict-only / never-mutates. ---

    #[tokio::test]
    async fn verify_match_leaves_every_input_unchanged() {
        let stills = vec![live_still(1), live_still(2)];
        let file = file_with_runtime(116 * 60_000);
        let metadata = correct_metadata();
        let before_metadata = metadata.clone();
        let before_stills = stills.clone();
        let before_file = file.clone();

        let _ = verify_match(&file, &metadata, &stills, None).await;

        // `verify_match` only ever takes `&`-references (see its
        // signature above) -- structurally incapable of mutating any
        // input. This equality check is a behavioral cross-check on top
        // of that: nothing was mutated in place either.
        assert_eq!(metadata, before_metadata);
        assert_eq!(stills, before_stills);
        assert_eq!(file, before_file);
    }
}
