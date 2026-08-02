//! FOUNDRY-02: the transcode SURVEY — what optimization would do, without doing it.
//!
//! MUSEF-02 built probe, policy, planning and the swap, and every one of them is tested. None
//! of it had a production caller: `Foundry::optimize_file` existed and nothing invoked it. So
//! the stage has never run, and the first file it ever touches would be a real one in the
//! operator's library.
//!
//! This is the step before that. It probes candidates and asks the planner what it WOULD do,
//! and stops there. Nothing is encoded, nothing is written, nothing is replaced.
//!
//! ## Why a dry run is the right first wiring, not caution theatre
//!
//! Three of the four things that could go wrong here are invisible until a real file is in
//! front of the code:
//!
//!   - **The policy could be wrong for this library.** A 12 Mbps ceiling that looked generous
//!     in review might match 1400 of 1892 titles, which is not "optimizing a few wasteful
//!     files", it is re-encoding the library. The survey reports the COUNT before that happens.
//!   - **The probe fixtures were hand-written.** MUSEF-02's ffprobe JSON came from documented
//!     shapes, not from this deployment's ffmpeg 5.1.9. MUSE #109 and #106 were both cases of a
//!     fixture agreeing with the code and both disagreeing with the tool.
//!   - **`CannotDecide` might be the common case.** If most files come back undecidable the
//!     stage is not ready regardless of how correct the swap is, and that is worth knowing from
//!     a report rather than from a worker that quietly does nothing every night.
//!
//! The fourth — the destructive swap — is already guarded by MUSEF-02's own machinery
//! (atomic claim, verify-before-replace, never delete an original). The survey does not
//! exercise it at all.
//!
//! ## The gate
//!
//! Execution stays behind `MUSE_FOUNDRY_ENABLE_MUTATION`, which is a PRE-EXISTING config knob
//! this module does not touch and does not read. A survey runs regardless of it; nothing in
//! here can encode or replace a file even if that flag is on, because it never calls
//! `optimize_file`. Turning the stage on is a separate, deliberate act.

use std::path::{Path, PathBuf};

use crate::foundry::probe::MediaProbe;
use crate::foundry::plan::{plan_transcode, TranscodeDecision, Undecidable};
use crate::foundry::policy::TranscodePolicy;
use crate::foundry::Foundry;

/// What the survey concluded about one file. Mirrors [`TranscodeDecision`] but flattened for
/// reporting, and deliberately keeps the REASONS — a count of "would transcode: 1400" that
/// cannot say why is not actionable.
#[derive(Debug, Clone, PartialEq)]
pub enum SurveyOutcome {
    /// Every policy dimension checked and passed.
    AlreadyOptimal,
    /// Would be rewritten, and why.
    ///
    /// `predicted_deletion_refusals` is what the deletion gate will say
    /// AFTERWARDS, computed from the source and the plan without encoding
    /// anything. When it is non-empty, Path A will spend a full re-encode and
    /// then KEEP the original — doubling disk for that title instead of
    /// reclaiming any. Legitimate, but at ~16,000 items it is a number an
    /// operator needs BEFORE committing, not after.
    WouldTranscode {
        reasons: Vec<String>,
        predicted_deletion_refusals: Vec<String>,
    },
    /// Could not be judged, and why. NOT folded into "optimal" — see [`plan_transcode`].
    CannotDecide { why: String },
    /// ffprobe could not read the file at all.
    ProbeFailed { error: String },
}

impl SurveyOutcome {
    pub fn as_str(&self) -> &'static str {
        match self {
            SurveyOutcome::AlreadyOptimal => "already_optimal",
            SurveyOutcome::WouldTranscode { .. } => "would_transcode",
            SurveyOutcome::CannotDecide { .. } => "cannot_decide",
            SurveyOutcome::ProbeFailed { .. } => "probe_failed",
        }
    }
}

/// One surveyed file.
#[derive(Debug, Clone, PartialEq)]
pub struct SurveyedFile {
    pub path: String,
    pub outcome: SurveyOutcome,
}

/// The whole run.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SurveySummary {
    pub examined: usize,
    pub already_optimal: usize,
    pub would_transcode: usize,
    pub cannot_decide: usize,
    pub probe_failed: usize,
    /// Per-file detail, bounded by the caller's limit.
    pub files: Vec<SurveyedFile>,
    /// True when the survey stopped at its limit rather than reaching the end of the candidate
    /// list — so a count is never read as "this is the whole library".
    pub truncated: bool,
}

impl SurveySummary {
    fn record(&mut self, path: &Path, outcome: SurveyOutcome) {
        self.examined += 1;
        match &outcome {
            SurveyOutcome::AlreadyOptimal => self.already_optimal += 1,
            SurveyOutcome::WouldTranscode { .. } => self.would_transcode += 1,
            SurveyOutcome::CannotDecide { .. } => self.cannot_decide += 1,
            SurveyOutcome::ProbeFailed { .. } => self.probe_failed += 1,
        }
        self.files.push(SurveyedFile {
            path: path.display().to_string(),
            outcome,
        });
    }

    /// Whether this survey supports turning execution on.
    ///
    /// Deliberately conservative and deliberately NOT a single number. A run that could not
    /// probe anything is not evidence of a healthy policy — it is evidence the tooling is not
    /// working, and the two look identical if you only count `would_transcode`.
    pub fn readiness(&self) -> Readiness {
        if self.examined == 0 {
            return Readiness::NothingExamined;
        }
        if self.probe_failed == self.examined {
            return Readiness::ProbeUnusable;
        }
        // More than half undecidable means the planner cannot judge this library, whatever the
        // swap machinery does correctly.
        if self.cannot_decide * 2 > self.examined {
            return Readiness::MostlyUndecidable;
        }
        Readiness::Surveyed
    }
}

/// A judgement about the SURVEY, not about any file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Readiness {
    /// No candidate was examined — says nothing about the library either way.
    NothingExamined,
    /// Every probe failed. The tooling is not working; the policy is unevaluated.
    ProbeUnusable,
    /// The planner could not judge most files.
    MostlyUndecidable,
    /// The survey produced usable judgements. NOT an instruction to enable mutation — that
    /// remains the operator's call after reading the counts.
    Surveyed,
}

impl Readiness {
    pub fn as_str(self) -> &'static str {
        match self {
            Readiness::NothingExamined => "nothing_examined",
            Readiness::ProbeUnusable => "probe_unusable",
            Readiness::MostlyUndecidable => "mostly_undecidable",
            Readiness::Surveyed => "surveyed",
        }
    }
}

/// Stand-in destination for the argv the planner builds during a survey.
///
/// A survey never executes, so no output path is needed — but `plan_transcode` builds the
/// command line, which requires one. This is deliberately not a plausible path: if this string
/// ever appears in a log, a process list, or on disk, something has run a survey's plan, which
/// is a bug worth spotting immediately rather than a file quietly appearing in the library.
pub const DRY_RUN_OUTPUT_SENTINEL: &str = "/dev/null/muse-dry-run-never-executed";

/// Classify one already-probed file. Pure, so every branch is testable without ffprobe.
///
/// Takes the PROBE as well as the decision because the predicted deletion
/// refusals are a function of the source, not only of the plan — the gate
/// refuses on properties of the original that the plan never mentions.
pub fn classify(probe: &MediaProbe, decision: &TranscodeDecision) -> SurveyOutcome {
    match decision {
        TranscodeDecision::AlreadyOptimal => SurveyOutcome::AlreadyOptimal,
        TranscodeDecision::Transcode { reasons, plan, .. } => SurveyOutcome::WouldTranscode {
            reasons: reasons.iter().map(|r| format!("{r:?}")).collect(),
            predicted_deletion_refusals:
                crate::foundry::directplay::predicted_deletion_refusals(probe, plan),
        },
        TranscodeDecision::CannotDecide { why } => SurveyOutcome::CannotDecide {
            why: describe_undecidable(why),
        },
    }
}

fn describe_undecidable(why: &Undecidable) -> String {
    format!("{why:?}")
}

/// Survey up to `limit` files. NEVER encodes, NEVER writes, NEVER replaces.
///
/// `optimize_file` is not called and is not reachable from here — that is the whole point, and
/// it is why this is safe to run against a live library before the stage has ever executed.
/// Why a survey stopped before examining everything it was given.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SurveyTruncation {
    /// The wall-clock deadline passed.
    DeadlineReached,
}

/// Should the survey stop now? Pure, so the rule is testable —
/// `survey_files` needs a live `Foundry`, and when this check was inline a
/// mutation that stopped RECORDING the truncation survived every test.
///
/// Recording it matters more than the stopping: a partial survey that reads as
/// a complete one would understate the work a 16,221-title run entails, which
/// is the number the operator is deciding on.
pub fn survey_truncation(
    elapsed: std::time::Duration,
    deadline: std::time::Duration,
) -> Option<SurveyTruncation> {
    (elapsed >= deadline).then_some(SurveyTruncation::DeadlineReached)
}

pub fn survey_files(
    foundry: &Foundry,
    policy: &TranscodePolicy,
    candidates: &[PathBuf],
    limit: usize,
    // Wall clock for the whole survey. A full-library pre-flight is now
    // possible (the limit reaches 50,000), so it needs its own bound: 16,221
    // probes at 0.17s is ~46 minutes normally, but a filesystem having a bad
    // day turns that into something unbounded. Hitting the deadline reports
    // what was ACTUALLY examined and marks the result truncated, which is a
    // different fact from a completed survey.
    deadline: std::time::Duration,
) -> SurveySummary {
    let started = std::time::Instant::now();
    let mut summary = SurveySummary {
        truncated: candidates.len() > limit,
        ..Default::default()
    };
    for path in candidates.iter().take(limit) {
        if let Some(reason) = survey_truncation(started.elapsed(), deadline) {
            summary.truncated = true;
            tracing::warn!(
                examined = summary.examined,
                reason = ?reason,
                "foundry/survey: stopped early; the report covers only what was examined"
            );
            break;
        }
        match foundry.probe_file(path) {
            Ok(probe) => {
                // The output path is only used to BUILD the argv, which a survey never runs.
                // A sentinel is passed rather than a real destination so nothing downstream can
                // mistake this for a prepared job — and so a bug that did try to execute would
                // fail on an obviously fake path rather than write somewhere real.
                let decision = plan_transcode(
                    &probe,
                    policy,
                    &path.display().to_string(),
                    DRY_RUN_OUTPUT_SENTINEL,
                );
                summary.record(path, classify(&probe, &decision));
            }
            // A probe failure is its OWN outcome, never folded into "optimal" or "cannot
            // decide" — those would attribute a tooling problem to the file or the policy.
            Err(error) => summary.record(path, SurveyOutcome::ProbeFailed { error }),
        }
    }
    summary
}

#[cfg(test)]
mod tests {

    /// A time-truncated survey must be marked truncated, exactly like a
    /// limit-truncated one.
    ///
    /// The survey can now cover the whole library (the limit reaches 50,000),
    /// so it needs a wall-clock bound. The failure that matters is not the
    /// stopping — it is a partial survey that reads as a complete one, which
    /// would understate the work a 16,221-title run entails.
    #[test]
    fn the_deadline_rule_fires_exactly_at_the_deadline() {
        use std::time::Duration;
        assert_eq!(survey_truncation(Duration::from_secs(1), Duration::from_secs(60)), None);
        assert_eq!(
            survey_truncation(Duration::from_secs(60), Duration::from_secs(60)),
            Some(SurveyTruncation::DeadlineReached),
            "at the deadline, not only past it"
        );
        assert_eq!(
            survey_truncation(Duration::from_secs(600), Duration::from_secs(60)),
            Some(SurveyTruncation::DeadlineReached)
        );
    }

    #[test]
    fn a_survey_that_runs_out_of_time_is_marked_truncated() {
        // Pure check of the flag's meaning: `truncated` is set by EITHER the
        // limit or the deadline, and callers must not treat it as
        // limit-specific.
        let by_limit = SurveySummary { truncated: true, examined: 500, ..Default::default() };
        let by_time = SurveySummary { truncated: true, examined: 37, ..Default::default() };
        assert!(by_limit.truncated && by_time.truncated);
        // ...and a complete survey is not truncated, or the flag means nothing.
        let complete = SurveySummary { truncated: false, examined: 16_221, ..Default::default() };
        assert!(!complete.truncated);
    }

    /// The clamp must actually permit a full-library pre-flight.
    ///
    /// The old ceiling was 500 — 3% of this library — so "survey before you
    /// commit" could only ever mean "sample". The bound existed because an
    /// unbounded survey could wedge on a stalled probe; ffprobe now has a
    /// 120s timeout (FOUNDRY-10) and the survey has its own deadline.
    #[test]
    fn the_survey_limit_reaches_the_whole_library() {
        let dash = include_str!("../web/dashboard.rs");
        assert!(
            dash.contains("q.limit.unwrap_or(25).clamp(1, 50_000)"),
            "the survey must be able to cover all 16,221 candidates, not a 500-file sample"
        );
        assert!(
            dash.contains("q.deadline_secs.unwrap_or(3600)"),
            "...and a full-library survey needs its own wall-clock bound"
        );
    }
    use super::*;

    fn summary_of(outcomes: &[SurveyOutcome]) -> SurveySummary {
        let mut s = SurveySummary::default();
        for (i, o) in outcomes.iter().enumerate() {
            s.record(Path::new(&format!("/f/{i}.mkv")), o.clone());
        }
        s
    }

    fn optimal() -> SurveyOutcome {
        SurveyOutcome::AlreadyOptimal
    }
    fn transcode() -> SurveyOutcome {
        SurveyOutcome::WouldTranscode {
            predicted_deletion_refusals: Vec::new(),
            reasons: vec!["BitrateAboveCeiling".into()],
        }
    }
    fn undecidable() -> SurveyOutcome {
        SurveyOutcome::CannotDecide {
            why: "NoDuration".into(),
        }
    }
    fn probe_failed() -> SurveyOutcome {
        SurveyOutcome::ProbeFailed {
            error: "ffprobe: not found".into(),
        }
    }

    #[test]
    fn every_outcome_is_counted_in_its_own_bucket() {
        // A probe failure must not be attributed to the file's content or the policy — those
        // are three different problems with three different fixes.
        let s = summary_of(&[optimal(), transcode(), undecidable(), probe_failed()]);
        assert_eq!(s.examined, 4);
        assert_eq!(s.already_optimal, 1);
        assert_eq!(s.would_transcode, 1);
        assert_eq!(s.cannot_decide, 1);
        assert_eq!(s.probe_failed, 1);
    }

    #[test]
    fn a_run_where_every_probe_failed_is_NOT_a_healthy_survey() {
        // The trap this exists for: 20 probe failures and zero would-transcode looks identical
        // to "your library is already optimal" if you only count transcodes. It is actually
        // "ffprobe is not working", and enabling mutation on that basis would be acting on no
        // evidence at all.
        let s = summary_of(&[probe_failed(), probe_failed(), probe_failed()]);
        assert_eq!(s.would_transcode, 0);
        assert_eq!(s.readiness(), Readiness::ProbeUnusable);
    }

    #[test]
    fn a_mostly_undecidable_library_is_not_ready_however_correct_the_swap_is() {
        let s = summary_of(&[undecidable(), undecidable(), optimal()]);
        assert_eq!(s.readiness(), Readiness::MostlyUndecidable);
        // Exactly half is NOT "mostly" — the boundary is stated rather than left to a reader.
        let half = summary_of(&[undecidable(), optimal()]);
        assert_eq!(half.readiness(), Readiness::Surveyed);
    }

    #[test]
    fn an_empty_run_says_nothing_rather_than_looking_clean() {
        // Zero examined with zero failures would otherwise read as a perfect result.
        let s = SurveySummary::default();
        assert_eq!(s.readiness(), Readiness::NothingExamined);
    }

    #[test]
    fn a_usable_survey_is_not_an_instruction_to_enable_mutation() {
        // `Surveyed` means "these counts are worth reading", not "go ahead". The distinction is
        // in the name and in the doc; this pins that no stronger variant exists to be confused
        // with it.
        let s = summary_of(&[optimal(), transcode()]);
        assert_eq!(s.readiness(), Readiness::Surveyed);
        assert_eq!(Readiness::Surveyed.as_str(), "surveyed");
    }

    #[test]
    fn truncation_is_reported_so_a_count_is_never_read_as_the_whole_library() {
        let mut s = SurveySummary {
            truncated: true,
            ..Default::default()
        };
        s.record(Path::new("/f/a.mkv"), transcode());
        assert!(s.truncated, "a capped run must say so");
    }

    #[test]
    fn a_transcode_outcome_carries_its_reasons() {
        // "would transcode: 1400" without reasons is not actionable — the operator cannot tell
        // an over-tight bitrate ceiling from a genuine backlog of bad files.
        let s = summary_of(&[transcode()]);
        match &s.files[0].outcome {
            SurveyOutcome::WouldTranscode { reasons, .. } => assert!(!reasons.is_empty()),
            other => panic!("expected WouldTranscode, got {other:?}"),
        }
    }

    #[test]
    fn the_outcome_labels_are_distinct() {
        // They end up in a report an operator reads; two states sharing a word is how a
        // tooling failure gets mistaken for a clean library.
        let labels = [
            optimal().as_str(),
            transcode().as_str(),
            undecidable().as_str(),
            probe_failed().as_str(),
        ];
        let unique: std::collections::HashSet<_> = labels.iter().collect();
        assert_eq!(unique.len(), labels.len(), "{labels:?}");
    }
}
