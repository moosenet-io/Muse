//! MUSEX-11 (Plane TERM #387): the TWO-TIMESCALE adaptation loop — fast
//! reactive + slow durable — closing PROGRAM -> OBSERVE -> INTERPRET ->
//! ADAPT -> LEARN.
//!
//! [`crate::tracker::interpret`] (MUSEX-10) is OBSERVE+INTERPRET: it
//! disambiguates a session's play-state pattern into one
//! [`crate::tracker::interpret::InterpretedSignal`] — a `kind` +
//! `confidence` + human-readable `rationale`. This module is ADAPT: it
//! reacts to that signal on two different clocks.
//!
//! - [`fast_adapt`] — the FAST loop. Runs within-session, right after a
//!   signal is interpreted, and only ever touches the schedule's NEXT slot
//!   (never anything already played, never the durable model). It is
//!   confidence-GATED: below [`HIGH_CONFIDENCE_THRESHOLD`] it does nothing
//!   ([`FastAdaptationKind::NoAdaptation`]) — an ambiguous signal must never
//!   whipsaw the next pick. See that function's doc for the threshold's
//!   justification and the per-`SignalKind` mapping.
//! - [`slow_consolidate`] — the SLOW loop. Runs sleep-time, over MULTIPLE
//!   sessions' accumulated signals, and is the only path that may move
//!   durable taste (the `taste_signals` rows
//!   [`crate::taste_model::signals`] derives from behavior — this module
//!   adds a new, passive-signal-sourced category alongside those). It
//!   requires a SUSTAINED pattern — the same signal direction recurring
//!   across at least [`SUSTAINED_PATTERN_MIN_SESSIONS`] *distinct* sessions
//!   — before it moves anything. A single session, however strong its
//!   signals, can never satisfy that distinct-session count on its own: this
//!   is the "one bad night never warps the durable model" guarantee, and
//!   it's structural (a `HashSet` of session ids), not a fragile threshold
//!   tuned to look like it works.
//!
//! Both loops emit a [`crate::taste_review::trace::ReasoningTrace`] — the
//! SAME struct MUSEX-07 built for "why did this recommendation happen,"
//! reused verbatim here for "why did this adaptation happen." Nothing new is
//! invented for the audit surface; a reviewer who already knows how to read
//! a recommendation trace can read an adaptation trace.
//!
//! Both loops take an [`Aggressiveness`] knob (0.0-1.0) that scales HOW MUCH
//! a fast adaptation shifts the next slot, and how much a durable update
//! moves the taste weight. `0.0` degrades to "detect but barely nudge";
//! `1.0` is the largest shift either loop will ever apply. Aggressiveness
//! never changes WHETHER a loop fires (the confidence gate / sustained-
//! pattern requirement are separate, non-negotiable gates) — only the
//! magnitude once it does.

use std::collections::HashSet;

use crate::channels::director::SlotIntent;
use crate::curation::candidates::CandidateSource;
use crate::taste_review::trace::{ReasoningTrace, SignalContribution};
use crate::tracker::interpret::{InterpretedSignal, SignalKind};

// --- confidence gate (fast loop) --------------------------------------------

/// The fast loop only reacts to a signal at/above this confidence.
///
/// Justification, grounded in `tracker::interpret`'s own confidence scale
/// (see `interpret_play_state`'s doc and its tests):
/// - The ambiguous "no strong pattern matched" default is a HARD-CODED
///   `0.3`, and every rule-based interpretation (early abandon, fatigue,
///   interruption, binge) has a floor of `0.5` right at its threshold —
///   `interpret.rs`'s own tests assert the ambiguous default stays `<= 0.4`
///   (`ambiguous_midway_stop_defaults_to_low_confidence_negative`). A
///   threshold has to clear that `0.4` ceiling with real margin, or an
///   ambiguous default could occasionally leak through.
/// - `interpret.rs`'s tests label `early_abandon` (`>= 0.7`) and
///   `late_stop_late_night_fatigue` (`>= 0.6`) as its own "high confidence"
///   examples — the module's own vocabulary for "clean, well-evidenced" sits
///   in the 0.6-0.7 band.
/// - `0.65` sits inside that band: comfortably above the ambiguous ceiling
///   (`0.4`) with margin, while still requiring more than "a pattern just
///   barely cleared its rule's `0.5` floor" (a marginal binge/interruption
///   right at threshold does NOT count as high-confidence here, even though
///   it produced a concrete `SignalKind` rather than the ambiguous default —
///   correctly distinguishing "matched a rule" from "matched it
///   convincingly").
pub const HIGH_CONFIDENCE_THRESHOLD: f32 = 0.65;

// --- sustained-pattern gate (slow loop) -------------------------------------

/// The minimum confidence an individual session's signal must clear to even
/// be counted as evidence toward a sustained pattern. Set to the same `0.5`
/// floor `interpret.rs`'s rule-based confidences start at — i.e. "matched a
/// real rule," excluding the `0.3` ambiguous default. A session whose signal
/// never rose above ambiguous contributes nothing to consolidation, sustained
/// or not.
pub const MIN_EVIDENCE_CONFIDENCE: f32 = 0.5;

/// The minimum number of *distinct* sessions exhibiting the SAME dominant
/// signal kind before the slow loop will move durable taste at all. `3` is
/// deliberately more than "twice in a row" (which a single unlucky weekend
/// could produce) — it asks for a pattern that has repeated across at least
/// three separate sittings. Below this count, [`slow_consolidate`] always
/// returns a non-moving [`DurableTasteUpdate`] (`moved: false`,
/// `weight_delta: 0.0`), REGARDLESS of how confident any individual
/// session's signal was — this is what makes "one bad night never warps the
/// durable model" true by construction: a single session can supply at most
/// one distinct session id, which can never reach `3`.
pub const SUSTAINED_PATTERN_MIN_SESSIONS: usize = 3;

// --- aggressiveness (tunable) -----------------------------------------------

/// How aggressively either loop reacts, once its gate has already been
/// cleared. `0.0..=1.0`; values outside that range are clamped on
/// construction so a caller can never accidentally overshoot the largest
/// shift either loop is willing to make. This is the AC-required "tunable"
/// knob: a deployment (or a per-account preference) can dial it via
/// [`Aggressiveness::new`], or use one of the three named presets below.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Aggressiveness(f32);

impl Aggressiveness {
    /// Small, cautious shifts — for an account that dislikes the channel
    /// visibly reacting to one signal.
    pub const GENTLE: Aggressiveness = Aggressiveness(0.25);
    /// The default: a clearly-felt but not jarring shift.
    pub const STANDARD: Aggressiveness = Aggressiveness(0.5);
    /// The largest shift either loop will ever apply.
    pub const BOLD: Aggressiveness = Aggressiveness(1.0);

    pub fn new(value: f32) -> Self {
        Aggressiveness(value.clamp(0.0, 1.0))
    }

    pub fn value(self) -> f32 {
        self.0
    }
}

impl Default for Aggressiveness {
    fn default() -> Self {
        Aggressiveness::STANDARD
    }
}

// --- fast loop: next-slot adaptation ----------------------------------------

/// The next slot's mutable plan — the FAST loop's only write surface. A
/// small, self-contained shape (not the full `channels::director::Slot`,
/// which also carries a resolved candidate/title/rationale a caller
/// determines separately) covering exactly what a fast adaptation is allowed
/// to nudge: the arc `intent` ([`SlotIntent`], reused from MUSEX-05's
/// director rather than a second enum), a target runtime, and two steering
/// flags a caller's candidate-pool selection is expected to honor.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct NextSlotPlan {
    pub target_intent: SlotIntent,
    pub target_runtime_ms: i64,
    /// Steer the next pick away from whatever source/genre pool just
    /// produced a `Negative` signal (a real dislike, not the show that was
    /// interrupted or the account being sleepy).
    pub avoid_same_source_pool: bool,
    /// Steer the next pick toward "more of the same" — set on a confident
    /// `Engagement` signal.
    pub favor_more_like_last: bool,
}

/// A floor under [`fast_adapt`]'s Fatigue wind-down shrink, so an
/// aggressiveness of `1.0` never proposes an absurdly short (or negative)
/// next-slot runtime.
const MIN_WIND_DOWN_RUNTIME_MS: i64 = 10 * 60_000;

/// What kind of next-slot shift (if any) [`fast_adapt`] applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FastAdaptationKind {
    /// Fatigue: shift the next slot toward `WindDown` and shrink its
    /// runtime target — "good night, not bad show," so the channel eases
    /// off rather than pushing another full-length pick.
    WindDownShorter,
    /// Negative (confident, genuine dislike): steer the next pick away from
    /// the source/genre pool that produced it.
    DifferentGenre,
    /// Engagement (a binge streak): lean into "more of the same."
    MoreOfSame,
    /// The gate wasn't cleared — either the signal's confidence was below
    /// [`HIGH_CONFIDENCE_THRESHOLD`], or the signal kind (`Interruption`) is
    /// not a taste decision either way (see `tracker::interpret`'s own doc
    /// on that kind) and must never move the next slot regardless of
    /// confidence.
    NoAdaptation,
}

/// One fast-loop reaction to one [`InterpretedSignal`]. Deliberately does
/// NOT derive `PartialEq`: it holds a [`ReasoningTrace`] (which has no
/// `PartialEq`, and shouldn't grow one just for a test's convenience), and
/// nothing needs whole-struct equality — callers/tests compare the
/// meaningful fields (`kind`, `adjusted_plan`, `magnitude`) directly.
#[derive(Debug, Clone)]
pub struct FastAdaptation {
    pub kind: FastAdaptationKind,
    /// The resulting next-slot plan — identical to the input `current` when
    /// `kind == NoAdaptation` (the negative-test invariant: nothing changes
    /// when the gate isn't cleared).
    pub adjusted_plan: NextSlotPlan,
    /// `0.0` when `kind == NoAdaptation`; otherwise `aggressiveness.value()`
    /// — how strongly the shift was applied, surfaced directly so a caller
    /// (or a test) doesn't have to diff `adjusted_plan` against `current` to
    /// know a shift happened.
    pub magnitude: f32,
    pub trace: ReasoningTrace,
}

fn adaptation_trace(label: &str, signal: &InterpretedSignal, path: String) -> ReasoningTrace {
    ReasoningTrace {
        media_metadata_id: 0,
        title: label.to_string(),
        // No `Candidate` is involved in a next-slot shift (this reacts to
        // passive telemetry, not a scored candidate) — `Taste` is the
        // closest real variant conceptually (this is a taste-driven
        // reaction) and is never read as "this came from the taste
        // candidate pool" by any consumer of a `ReasoningTrace` today.
        source: CandidateSource::Taste,
        score: signal.confidence as f64,
        taste_fit: signal.confidence as f64,
        source_weight: 1.0,
        signals: vec![SignalContribution {
            signal: format!("{:?}", signal.kind),
            weight: signal.confidence as f64,
            description: signal.rationale.clone(),
        }],
        path,
    }
}

/// The FAST loop: react to one interpreted signal by (maybe) adjusting the
/// NEXT slot. Confidence-gated — see [`HIGH_CONFIDENCE_THRESHOLD`]'s doc for
/// the threshold and its justification. Below it (or on an `Interruption`
/// signal, which is never a taste decision — see `tracker::interpret`'s own
/// doc on that kind), returns [`FastAdaptationKind::NoAdaptation`] with
/// `adjusted_plan` set to `current.clone()` UNCHANGED: this is the AC's
/// negative test made structural, not just documented.
pub fn fast_adapt(
    current: &NextSlotPlan,
    signal: &InterpretedSignal,
    aggressiveness: Aggressiveness,
) -> FastAdaptation {
    let no_adaptation = |reason: &str| FastAdaptation {
        kind: FastAdaptationKind::NoAdaptation,
        adjusted_plan: *current,
        magnitude: 0.0,
        trace: adaptation_trace(
            "fast_adapt: no adaptation",
            signal,
            format!(
                "fast_adapt: {reason} (confidence {:.2}, threshold {HIGH_CONFIDENCE_THRESHOLD:.2})",
                signal.confidence
            ),
        ),
    };

    if signal.confidence < HIGH_CONFIDENCE_THRESHOLD {
        return no_adaptation("signal confidence below HIGH_CONFIDENCE_THRESHOLD");
    }

    match signal.kind {
        SignalKind::Interruption => {
            no_adaptation("an interruption is a real-world event, not a taste decision either way")
        }
        SignalKind::Fatigue => {
            let shrink = (current.target_runtime_ms as f32 * 0.5 * aggressiveness.value()) as i64;
            let target_runtime_ms =
                (current.target_runtime_ms - shrink).max(MIN_WIND_DOWN_RUNTIME_MS);
            let adjusted_plan = NextSlotPlan {
                target_intent: SlotIntent::WindDown,
                target_runtime_ms,
                ..*current
            };
            FastAdaptation {
                kind: FastAdaptationKind::WindDownShorter,
                adjusted_plan,
                magnitude: aggressiveness.value(),
                trace: adaptation_trace(
                    "fast_adapt: wind-down shorter",
                    signal,
                    format!(
                        "fast_adapt(Fatigue): shift next slot to WindDown, shrink runtime {} -> {} ms (aggressiveness {:.2})",
                        current.target_runtime_ms, target_runtime_ms, aggressiveness.value()
                    ),
                ),
            }
        }
        SignalKind::Negative => {
            let adjusted_plan = NextSlotPlan {
                avoid_same_source_pool: true,
                ..*current
            };
            FastAdaptation {
                kind: FastAdaptationKind::DifferentGenre,
                adjusted_plan,
                magnitude: aggressiveness.value(),
                trace: adaptation_trace(
                    "fast_adapt: different genre",
                    signal,
                    format!(
                        "fast_adapt(Negative): steer next slot away from the same source/genre pool (aggressiveness {:.2})",
                        aggressiveness.value()
                    ),
                ),
            }
        }
        SignalKind::Engagement => {
            let adjusted_plan = NextSlotPlan {
                favor_more_like_last: true,
                ..*current
            };
            FastAdaptation {
                kind: FastAdaptationKind::MoreOfSame,
                adjusted_plan,
                magnitude: aggressiveness.value(),
                trace: adaptation_trace(
                    "fast_adapt: more of the same",
                    signal,
                    format!(
                        "fast_adapt(Engagement): steer next slot toward more of the same (aggressiveness {:.2})",
                        aggressiveness.value()
                    ),
                ),
            }
        }
    }
}

// --- slow loop: durable-taste consolidation ---------------------------------

/// One session's contribution to the slow loop's evidence pool — the
/// interpreted-signal summary of a single stopped session, tagged with which
/// session it came from so [`slow_consolidate`] can count DISTINCT sessions
/// rather than raw signal rows (a session that emits the same signal
/// multiple times must not count as multiple sessions' worth of evidence).
#[derive(Debug, Clone)]
pub struct SessionSignal {
    pub session_id: String,
    pub kind: SignalKind,
    pub confidence: f32,
}

impl SessionSignal {
    pub fn from_interpreted(session_id: impl Into<String>, signal: &InterpretedSignal) -> Self {
        SessionSignal {
            session_id: session_id.into(),
            kind: signal.kind,
            confidence: signal.confidence,
        }
    }
}

/// `taste_signals.signal_type` this module writes on a genuine durable
/// negative consolidation — a new category alongside
/// `taste_model::signals::DERIVED_SIGNAL_TYPES` (those are derived from
/// `watch_stats`/`ratings`/`watchlist`; this one is derived from a SUSTAINED
/// pattern of passive `tracker::interpret` signals instead).
pub const DURABLE_SIGNAL_SUSTAINED_NEGATIVE: &str = "adaptation_sustained_negative";
/// `taste_signals.signal_type` on a genuine durable engagement consolidation.
pub const DURABLE_SIGNAL_SUSTAINED_ENGAGEMENT: &str = "adaptation_sustained_engagement";

/// The largest magnitude [`slow_consolidate`] will ever move a durable
/// weight by in one consolidation, at `aggressiveness == 1.0` — deliberately
/// smaller than `taste_model::signals::WEIGHT_FINISH`/`WEIGHT_ABANDON`
/// (`1.0`/`-1.5`): a passive, inferred pattern is corroborating evidence
/// alongside the account's explicit finish/abandon/rating behavior, not a
/// replacement for it, so it should never out-weigh a single explicit
/// signal.
const MAX_DURABLE_WEIGHT_DELTA: f32 = 0.8;

/// The outcome of one slow-loop consolidation pass.
#[derive(Debug, Clone)]
pub struct DurableTasteUpdate {
    /// `false` when no sustained pattern was found (or the dominant pattern
    /// was `Fatigue`/`Interruption` — see below) — the durable model must be
    /// treated as UNCHANGED in that case.
    pub moved: bool,
    /// `None` when `moved == false`.
    pub signal_type: Option<&'static str>,
    /// `0.0` when `moved == false`.
    pub weight_delta: f32,
    /// How many distinct sessions were considered overall (evidence-eligible
    /// or not) — surfaced for audit, not itself a gate.
    pub sessions_considered: usize,
    /// How many DISTINCT sessions exhibited the dominant kind at/above
    /// [`MIN_EVIDENCE_CONFIDENCE`] — the count [`SUSTAINED_PATTERN_MIN_SESSIONS`]
    /// is compared against.
    pub dominant_session_count: usize,
    pub trace: ReasoningTrace,
}

/// The SLOW loop: fold MULTIPLE sessions' interpreted signals into a durable
/// taste update. Requires a SUSTAINED pattern — the same [`SignalKind`]
/// appearing at/above [`MIN_EVIDENCE_CONFIDENCE`] in at least
/// [`SUSTAINED_PATTERN_MIN_SESSIONS`] *distinct* `session_id`s — before it
/// will move anything; otherwise returns a non-moving [`DurableTasteUpdate`]
/// (`moved: false`, `weight_delta: 0.0`). A single session can supply at
/// most one distinct session id, so it can NEVER alone satisfy that count —
/// this is the "one bad night never warps the durable model" guarantee,
/// structural rather than a tuned-to-pass threshold.
///
/// Only `Negative`/`Engagement` are genuine taste directions; a sustained
/// `Fatigue` or `Interruption` pattern (even across many sessions) still
/// does not move durable taste — neither is a taste signal (see
/// `tracker::interpret`'s own doc: fatigue is "good night, not bad show,"
/// interruption is "a real-world interruption, not a taste decision either
/// way") — consolidating either into "the account likes/dislikes this" would
/// be exactly the kind of unfounded inference this crate's `Candidate::facts`
/// discipline forbids.
pub fn slow_consolidate(
    sessions: &[SessionSignal],
    aggressiveness: Aggressiveness,
) -> DurableTasteUpdate {
    let distinct_sessions: HashSet<&str> = sessions.iter().map(|s| s.session_id.as_str()).collect();
    let sessions_considered = distinct_sessions.len();

    let no_move = |dominant_session_count: usize, reason: String| DurableTasteUpdate {
        moved: false,
        signal_type: None,
        weight_delta: 0.0,
        sessions_considered,
        dominant_session_count,
        trace: ReasoningTrace {
            media_metadata_id: 0,
            title: "slow_consolidate: no durable move".to_string(),
            source: CandidateSource::Taste,
            score: 0.0,
            taste_fit: 0.0,
            source_weight: 1.0,
            signals: vec![],
            path: reason,
        },
    };

    // Evidence-eligible signals only (>= MIN_EVIDENCE_CONFIDENCE); group by
    // kind, tracking DISTINCT session ids per kind (a HashSet, not a count),
    // so repeated signals within one session never inflate its evidence
    // beyond "one session's worth."
    let mut by_kind: std::collections::HashMap<SignalKind, HashSet<&str>> =
        std::collections::HashMap::new();
    for s in sessions {
        if s.confidence >= MIN_EVIDENCE_CONFIDENCE {
            by_kind
                .entry(s.kind)
                .or_default()
                .insert(s.session_id.as_str());
        }
    }

    // The dominant kind is whichever has the most distinct sessions behind
    // it. Ties broken by a fixed `SignalKind` order (declaration order via
    // discriminant) so this stays deterministic rather than hash-order
    // dependent.
    let kind_priority = |k: &SignalKind| match k {
        SignalKind::Negative => 0,
        SignalKind::Fatigue => 1,
        SignalKind::Interruption => 2,
        SignalKind::Engagement => 3,
    };
    let dominant = by_kind
        .iter()
        .max_by(|a, b| {
            a.1.len()
                .cmp(&b.1.len())
                .then_with(|| kind_priority(b.0).cmp(&kind_priority(a.0)))
        })
        .map(|(k, set)| (*k, set.len()));

    let Some((dominant_kind, dominant_session_count)) = dominant else {
        return no_move(0, "no session cleared MIN_EVIDENCE_CONFIDENCE".to_string());
    };

    if dominant_session_count < SUSTAINED_PATTERN_MIN_SESSIONS {
        return no_move(
            dominant_session_count,
            format!(
                "dominant kind {dominant_kind:?} appeared in only {dominant_session_count} \
                 distinct session(s), below SUSTAINED_PATTERN_MIN_SESSIONS \
                 ({SUSTAINED_PATTERN_MIN_SESSIONS}) — a single (or near-single) session must never \
                 warp the durable model"
            ),
        );
    }

    let (signal_type, sign) = match dominant_kind {
        SignalKind::Negative => (DURABLE_SIGNAL_SUSTAINED_NEGATIVE, -1.0),
        SignalKind::Engagement => (DURABLE_SIGNAL_SUSTAINED_ENGAGEMENT, 1.0),
        SignalKind::Fatigue | SignalKind::Interruption => {
            return no_move(
                dominant_session_count,
                format!(
                    "dominant kind {dominant_kind:?} recurred in {dominant_session_count} \
                     distinct sessions, but neither Fatigue nor Interruption is a taste signal \
                     (per tracker::interpret's own doc) — it never moves durable taste, no matter \
                     how often it recurs"
                ),
            );
        }
    };

    // Average confidence across the dominant kind's qualifying sessions,
    // scaled by aggressiveness and capped at MAX_DURABLE_WEIGHT_DELTA.
    let confidences: Vec<f32> = sessions
        .iter()
        .filter(|s| s.kind == dominant_kind && s.confidence >= MIN_EVIDENCE_CONFIDENCE)
        .map(|s| s.confidence)
        .collect();
    let avg_confidence = confidences.iter().sum::<f32>() / confidences.len().max(1) as f32;
    let weight_delta = sign * avg_confidence * aggressiveness.value() * MAX_DURABLE_WEIGHT_DELTA;

    DurableTasteUpdate {
        moved: true,
        signal_type: Some(signal_type),
        weight_delta,
        sessions_considered,
        dominant_session_count,
        trace: ReasoningTrace {
            media_metadata_id: 0,
            title: "slow_consolidate: durable move".to_string(),
            source: CandidateSource::Taste,
            score: avg_confidence as f64,
            taste_fit: avg_confidence as f64,
            source_weight: 1.0,
            signals: confidences
                .iter()
                .enumerate()
                .map(|(i, c)| SignalContribution {
                    signal: format!("{dominant_kind:?}"),
                    weight: *c as f64,
                    description: format!(
                        "session #{i} of {dominant_session_count} distinct sessions exhibiting {dominant_kind:?}"
                    ),
                })
                .collect(),
            path: format!(
                "slow_consolidate: {dominant_kind:?} sustained across {dominant_session_count} \
                 distinct sessions (>= {SUSTAINED_PATTERN_MIN_SESSIONS}), avg confidence \
                 {avg_confidence:.2}, aggressiveness {:.2} -> weight_delta {weight_delta:.3} on \
                 `{signal_type}`",
                aggressiveness.value()
            ),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tracker::interpret::SignalKind;

    fn signal(kind: SignalKind, confidence: f32) -> InterpretedSignal {
        InterpretedSignal {
            kind,
            confidence,
            rationale: format!("test signal: {kind:?} @ {confidence}"),
        }
    }

    fn base_plan() -> NextSlotPlan {
        NextSlotPlan {
            target_intent: SlotIntent::Main,
            target_runtime_ms: 45 * 60_000,
            avoid_same_source_pool: false,
            favor_more_like_last: false,
        }
    }

    // --- fast loop: confidence gate (load-bearing negative test) -----------

    #[test]
    fn low_confidence_signal_does_not_trigger_fast_adaptation() {
        let current = base_plan();
        // Below HIGH_CONFIDENCE_THRESHOLD (0.65) — the interpreter's own
        // "ambiguous default" range.
        let sig = signal(SignalKind::Negative, 0.3);
        let result = fast_adapt(&current, &sig, Aggressiveness::BOLD);

        assert_eq!(result.kind, FastAdaptationKind::NoAdaptation);
        assert_eq!(
            result.adjusted_plan, current,
            "an ambiguous/low-confidence signal must leave the next-slot plan byte-for-byte unchanged"
        );
        assert_eq!(result.magnitude, 0.0);
    }

    #[test]
    fn confidence_exactly_at_threshold_does_adapt() {
        // The gate is `>=`, not `>` — right at the threshold clears it.
        let current = base_plan();
        let sig = signal(SignalKind::Negative, HIGH_CONFIDENCE_THRESHOLD);
        let result = fast_adapt(&current, &sig, Aggressiveness::STANDARD);
        assert_ne!(result.kind, FastAdaptationKind::NoAdaptation);
    }

    #[test]
    fn confidence_just_below_threshold_does_not_adapt() {
        let current = base_plan();
        let sig = signal(SignalKind::Fatigue, HIGH_CONFIDENCE_THRESHOLD - 0.01);
        let result = fast_adapt(&current, &sig, Aggressiveness::STANDARD);
        assert_eq!(result.kind, FastAdaptationKind::NoAdaptation);
        assert_eq!(result.adjusted_plan, current);
    }

    // --- fast loop: positive per-kind mapping -------------------------------

    #[test]
    fn high_confidence_fatigue_shifts_next_slot_to_wind_down_and_shrinks_runtime() {
        let current = base_plan();
        let sig = signal(SignalKind::Fatigue, 0.9);
        let result = fast_adapt(&current, &sig, Aggressiveness::STANDARD);

        assert_eq!(result.kind, FastAdaptationKind::WindDownShorter);
        assert_eq!(result.adjusted_plan.target_intent, SlotIntent::WindDown);
        assert!(
            result.adjusted_plan.target_runtime_ms < current.target_runtime_ms,
            "fatigue must shrink the next-slot runtime target"
        );
        assert!(result.magnitude > 0.0);
    }

    #[test]
    fn high_confidence_negative_steers_away_from_same_source_pool() {
        let current = base_plan();
        let sig = signal(SignalKind::Negative, 0.9);
        let result = fast_adapt(&current, &sig, Aggressiveness::STANDARD);

        assert_eq!(result.kind, FastAdaptationKind::DifferentGenre);
        assert!(result.adjusted_plan.avoid_same_source_pool);
    }

    #[test]
    fn high_confidence_engagement_favors_more_of_the_same() {
        let current = base_plan();
        let sig = signal(SignalKind::Engagement, 0.9);
        let result = fast_adapt(&current, &sig, Aggressiveness::STANDARD);

        assert_eq!(result.kind, FastAdaptationKind::MoreOfSame);
        assert!(result.adjusted_plan.favor_more_like_last);
    }

    #[test]
    fn high_confidence_interruption_never_adapts_the_next_slot() {
        // Interruption is a real-world event, not a taste decision — even at
        // maximum confidence it must not move the next slot.
        let current = base_plan();
        let sig = signal(SignalKind::Interruption, 0.99);
        let result = fast_adapt(&current, &sig, Aggressiveness::BOLD);

        assert_eq!(result.kind, FastAdaptationKind::NoAdaptation);
        assert_eq!(result.adjusted_plan, current);
    }

    // --- fast loop: aggressiveness scales magnitude, not whether -----------

    #[test]
    fn aggressiveness_scales_fatigue_shrink_magnitude() {
        let current = base_plan();
        let sig = signal(SignalKind::Fatigue, 0.9);

        let gentle = fast_adapt(&current, &sig, Aggressiveness::GENTLE);
        let bold = fast_adapt(&current, &sig, Aggressiveness::BOLD);

        assert!(
            bold.adjusted_plan.target_runtime_ms < gentle.adjusted_plan.target_runtime_ms,
            "a bolder aggressiveness must shrink the runtime target more: gentle={} bold={}",
            gentle.adjusted_plan.target_runtime_ms,
            bold.adjusted_plan.target_runtime_ms
        );
        assert!(bold.magnitude > gentle.magnitude);
    }

    #[test]
    fn aggressiveness_never_changes_whether_the_gate_fires() {
        let current = base_plan();
        let low_conf = signal(SignalKind::Negative, 0.2);
        // Even BOLD aggressiveness cannot rescue a low-confidence signal.
        let result = fast_adapt(&current, &low_conf, Aggressiveness::BOLD);
        assert_eq!(result.kind, FastAdaptationKind::NoAdaptation);
    }

    #[test]
    fn aggressiveness_clamps_out_of_range_construction() {
        assert_eq!(Aggressiveness::new(5.0).value(), 1.0);
        assert_eq!(Aggressiveness::new(-5.0).value(), 0.0);
    }

    // --- fast loop: reasoning trace is grounded, not fabricated -------------

    #[test]
    fn fast_adaptation_trace_carries_the_real_signal_rationale() {
        let current = base_plan();
        let sig = signal(SignalKind::Fatigue, 0.9);
        let result = fast_adapt(&current, &sig, Aggressiveness::STANDARD);

        assert_eq!(result.trace.signals.len(), 1);
        assert_eq!(result.trace.signals[0].description, sig.rationale);
        assert!((result.trace.signals[0].weight - sig.confidence as f64).abs() < 1e-6);
    }

    #[test]
    fn no_adaptation_trace_explains_the_gate() {
        let current = base_plan();
        let sig = signal(SignalKind::Negative, 0.1);
        let result = fast_adapt(&current, &sig, Aggressiveness::STANDARD);
        assert!(result
            .trace
            .path
            .contains("below HIGH_CONFIDENCE_THRESHOLD"));
    }

    // --- slow loop: one-bad-night guard (load-bearing negative test) -------

    #[test]
    fn a_single_session_never_warps_the_durable_model() {
        // One session, but MANY strong negative signals within it — all
        // sharing one session_id.
        let sessions = vec![
            SessionSignal {
                session_id: "sess-1".to_string(),
                kind: SignalKind::Negative,
                confidence: 0.95,
            },
            SessionSignal {
                session_id: "sess-1".to_string(),
                kind: SignalKind::Negative,
                confidence: 0.9,
            },
            SessionSignal {
                session_id: "sess-1".to_string(),
                kind: SignalKind::Negative,
                confidence: 0.99,
            },
        ];
        let update = slow_consolidate(&sessions, Aggressiveness::BOLD);

        assert!(
            !update.moved,
            "one session's signals, however numerous or strong, must never move durable taste"
        );
        assert_eq!(update.weight_delta, 0.0);
        assert_eq!(update.signal_type, None);
        assert_eq!(
            update.dominant_session_count, 1,
            "all three signals came from the same session_id — one distinct session, not three"
        );
    }

    #[test]
    fn two_sessions_are_still_not_enough_to_move_durable_taste() {
        let sessions = vec![
            SessionSignal {
                session_id: "sess-1".to_string(),
                kind: SignalKind::Negative,
                confidence: 0.9,
            },
            SessionSignal {
                session_id: "sess-2".to_string(),
                kind: SignalKind::Negative,
                confidence: 0.9,
            },
        ];
        let update = slow_consolidate(&sessions, Aggressiveness::BOLD);
        assert!(!update.moved);
        assert_eq!(update.dominant_session_count, 2);
    }

    // --- slow loop: sustained pattern DOES consolidate ----------------------

    #[test]
    fn sustained_negative_pattern_across_three_sessions_moves_durable_taste_negative() {
        let sessions = vec![
            SessionSignal {
                session_id: "sess-1".to_string(),
                kind: SignalKind::Negative,
                confidence: 0.8,
            },
            SessionSignal {
                session_id: "sess-2".to_string(),
                kind: SignalKind::Negative,
                confidence: 0.85,
            },
            SessionSignal {
                session_id: "sess-3".to_string(),
                kind: SignalKind::Negative,
                confidence: 0.9,
            },
        ];
        let update = slow_consolidate(&sessions, Aggressiveness::STANDARD);

        assert!(update.moved);
        assert_eq!(update.signal_type, Some(DURABLE_SIGNAL_SUSTAINED_NEGATIVE));
        assert!(
            update.weight_delta < 0.0,
            "a sustained negative pattern must move durable taste negative, got {}",
            update.weight_delta
        );
        assert_eq!(update.dominant_session_count, 3);
    }

    #[test]
    fn sustained_engagement_pattern_moves_durable_taste_positive() {
        let sessions: Vec<SessionSignal> = (1..=4)
            .map(|i| SessionSignal {
                session_id: format!("sess-{i}"),
                kind: SignalKind::Engagement,
                confidence: 0.85,
            })
            .collect();
        let update = slow_consolidate(&sessions, Aggressiveness::STANDARD);

        assert!(update.moved);
        assert_eq!(
            update.signal_type,
            Some(DURABLE_SIGNAL_SUSTAINED_ENGAGEMENT)
        );
        assert!(update.weight_delta > 0.0);
        assert_eq!(update.dominant_session_count, 4);
    }

    #[test]
    fn sustained_fatigue_or_interruption_pattern_never_moves_durable_taste() {
        // Fatigue is "good night, not bad show"; Interruption is a
        // real-world event. Neither is a taste signal, no matter how many
        // distinct sessions exhibit it.
        for kind in [SignalKind::Fatigue, SignalKind::Interruption] {
            let sessions: Vec<SessionSignal> = (1..=5)
                .map(|i| SessionSignal {
                    session_id: format!("sess-{i}"),
                    kind,
                    confidence: 0.9,
                })
                .collect();
            let update = slow_consolidate(&sessions, Aggressiveness::BOLD);
            assert!(
                !update.moved,
                "{kind:?} sustained across 5 sessions must still never move durable taste"
            );
            assert_eq!(update.weight_delta, 0.0);
        }
    }

    #[test]
    fn low_confidence_sessions_never_count_toward_the_sustained_pattern() {
        // Five distinct sessions, but every signal is below
        // MIN_EVIDENCE_CONFIDENCE — none should count as evidence.
        let sessions: Vec<SessionSignal> = (1..=5)
            .map(|i| SessionSignal {
                session_id: format!("sess-{i}"),
                kind: SignalKind::Negative,
                confidence: 0.3,
            })
            .collect();
        let update = slow_consolidate(&sessions, Aggressiveness::BOLD);
        assert!(!update.moved);
        assert_eq!(update.dominant_session_count, 0);
    }

    #[test]
    fn empty_session_list_never_moves_durable_taste() {
        let update = slow_consolidate(&[], Aggressiveness::BOLD);
        assert!(!update.moved);
        assert_eq!(update.sessions_considered, 0);
    }

    // --- slow loop: aggressiveness scales magnitude, not whether -----------

    #[test]
    fn slow_loop_aggressiveness_scales_weight_delta_magnitude_not_whether_it_moves() {
        let sessions: Vec<SessionSignal> = (1..=3)
            .map(|i| SessionSignal {
                session_id: format!("sess-{i}"),
                kind: SignalKind::Negative,
                confidence: 0.9,
            })
            .collect();

        let gentle = slow_consolidate(&sessions, Aggressiveness::GENTLE);
        let bold = slow_consolidate(&sessions, Aggressiveness::BOLD);

        assert!(gentle.moved && bold.moved);
        assert!(
            bold.weight_delta.abs() > gentle.weight_delta.abs(),
            "bolder aggressiveness must move durable taste further per consolidation: gentle={} bold={}",
            gentle.weight_delta,
            bold.weight_delta
        );
    }

    #[test]
    fn durable_weight_delta_never_exceeds_a_single_explicit_finish_signal() {
        // A passive, inferred pattern must never out-weigh
        // taste_model::signals::WEIGHT_FINISH (1.0) — corroborating
        // evidence, not a replacement for explicit behavior.
        let sessions: Vec<SessionSignal> = (1..=6)
            .map(|i| SessionSignal {
                session_id: format!("sess-{i}"),
                kind: SignalKind::Engagement,
                confidence: 0.99,
            })
            .collect();
        let update = slow_consolidate(&sessions, Aggressiveness::BOLD);
        assert!(update.weight_delta.abs() < crate::taste_model::signals::WEIGHT_FINISH);
    }

    // --- slow loop: trace is grounded --------------------------------------

    #[test]
    fn durable_update_trace_documents_the_sustained_pattern() {
        let sessions = vec![
            SessionSignal {
                session_id: "sess-1".to_string(),
                kind: SignalKind::Negative,
                confidence: 0.8,
            },
            SessionSignal {
                session_id: "sess-2".to_string(),
                kind: SignalKind::Negative,
                confidence: 0.8,
            },
            SessionSignal {
                session_id: "sess-3".to_string(),
                kind: SignalKind::Negative,
                confidence: 0.8,
            },
        ];
        let update = slow_consolidate(&sessions, Aggressiveness::STANDARD);
        assert!(update.trace.path.contains("3 distinct sessions"));
        assert_eq!(update.trace.signals.len(), 3);
    }

    #[test]
    fn no_move_trace_explains_the_one_bad_night_guard() {
        let sessions = vec![SessionSignal {
            session_id: "sess-1".to_string(),
            kind: SignalKind::Negative,
            confidence: 0.9,
        }];
        let update = slow_consolidate(&sessions, Aggressiveness::STANDARD);
        assert!(update
            .trace
            .path
            .contains("must never warp the durable model"));
    }

    // --- SessionSignal::from_interpreted reuses the real interpreter output -

    #[test]
    fn session_signal_from_interpreted_carries_the_real_kind_and_confidence() {
        let sig = signal(SignalKind::Engagement, 0.77);
        let s = SessionSignal::from_interpreted("sess-42", &sig);
        assert_eq!(s.session_id, "sess-42");
        assert_eq!(s.kind, SignalKind::Engagement);
        assert!((s.confidence - 0.77).abs() < f32::EPSILON);
    }

    // --- determinism ----------------------------------------------------------

    #[test]
    fn slow_consolidate_is_deterministic_for_identical_inputs() {
        let sessions: Vec<SessionSignal> = (1..=4)
            .map(|i| SessionSignal {
                session_id: format!("sess-{i}"),
                kind: if i % 2 == 0 {
                    SignalKind::Negative
                } else {
                    SignalKind::Engagement
                },
                confidence: 0.8,
            })
            .collect();

        let a = slow_consolidate(&sessions, Aggressiveness::STANDARD);
        let b = slow_consolidate(&sessions, Aggressiveness::STANDARD);
        assert_eq!(a.moved, b.moved);
        assert_eq!(a.signal_type, b.signal_type);
        assert!((a.weight_delta - b.weight_delta).abs() < f32::EPSILON);
    }
}
