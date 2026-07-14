//! MUSEX-12 (Plane TERM #388): the loop's ACTIVE sense — a conversational
//! assistant, delivered in Lumina's voice, that asks a question ONLY when
//! genuinely useful and stays silent otherwise.
//!
//! ## Why this module exists
//! [`crate::tracker::interpret`] (MUSEX-10) is the PASSIVE sense: it
//! disambiguates play-state telemetry into an [`InterpretedSignal`] with a
//! `confidence`. [`crate::adaptation`] (MUSEX-11) is what the loop does with
//! a signal once it has one — the FAST loop (`fast_adapt`) reacts
//! immediately but is deliberately confidence-GATED at
//! [`crate::adaptation::HIGH_CONFIDENCE_THRESHOLD`]: below that threshold it
//! does nothing (`FastAdaptationKind::NoAdaptation`), because an ambiguous
//! signal must never whipsaw the next pick.
//!
//! That gate leaves a gap: what happens to a signal that's ambiguous
//! (low-confidence) but would, if resolved, genuinely change the next slot?
//! Left alone, it's simply dropped — the fast loop shrugs, the slow loop
//! (MUSEX-11) needs three sustained sessions before it moves anything. This
//! module is the third option: convert exactly THAT kind of ambiguity into
//! ground truth by asking, through Lumina, instead of guessing or ignoring.
//!
//! ## The gate ([`decide_ask`]) — load-bearing
//! A question fires ONLY when a fork is BOTH:
//! - **MATERIAL** — [`is_material_fork`]: the signal's `kind` is one that
//!   [`crate::adaptation::fast_adapt`] would actually act on if it were
//!   confident (`Negative`/`Fatigue`/`Engagement` — mirroring `fast_adapt`'s
//!   own match arms verbatim, not a second classification invented here).
//!   `Interruption` is never material: `fast_adapt` itself never adapts on
//!   it "even at maximum confidence" (see that function's doc/tests) because
//!   it's a real-world event, not a taste decision either way — asking "did
//!   you not like it?" after a phone call would be exactly the over-asking
//!   this module must avoid.
//! - **LOW-CONFIDENCE** — strictly below
//!   [`crate::adaptation::HIGH_CONFIDENCE_THRESHOLD`] (`0.65`), the SAME
//!   threshold `fast_adapt` gates on. At/above it, the signal is "clear":
//!   the fast loop already adapts silently, so asking would be redundant
//!   nagging over something the loop already handled.
//!
//! `AskFrequency::Never` (silent mode) short-circuits both checks: it always
//! returns [`AskDecision::Silent`], regardless of materiality or confidence
//! — "never ask, infer only."
//!
//! ## The three moments (AC)
//! - **Pre-session** ([`pre_session_ask_decision`] / [`program_from_intent`]):
//!   a single intent question ("what's the vibe tonight?") that PROGRAMS the
//!   session — who/energy/time-budget map to a persona name suggestion +
//!   real [`crate::channels::director::DirectorConstraints`] (the same type
//!   MUSEX-05's director consumes; this module invents no second "session
//!   plan" shape).
//! - **Mid-session** ([`decide_ask`]): fires only through the gate above.
//! - **Post-watch** ([`record_post_watch_reaction`]): one optional tap,
//!   NEVER a gate — `None` is always a valid, silent input.
//!
//! ## Answer -> signal ([`answer_to_signal`])
//! A user-stated answer is ground truth, not an inference — it becomes an
//! [`InterpretedSignal`] at [`ANSWERED_SIGNAL_CONFIDENCE`] (`1.0`, always
//! `>= HIGH_CONFIDENCE_THRESHOLD`), so it flows straight into
//! `crate::adaptation::fast_adapt`'s high-confidence path exactly like a
//! clear passive signal would — MUSEX-11's loop does not need to know
//! whether a signal came from inference or from asking.
//!
//! ## Lumina's voice
//! Question phrasing reuses the exact warm/concise Lumina persona
//! `crate::proactive::generators::build_message` already established (same
//! "You are Lumina..." register), never inventing a second assistant
//! persona. [`build_question_message`] mirrors `build_message`'s shape: a
//! deterministic template (always available, what every pure test exercises)
//! plus an optional Chord-phrased sentence that is never on the critical
//! path (Chord down/unset -> template, exactly like MUSE-12's proactive
//! nudges).

use chrono::{DateTime, Duration as ChronoDuration, Timelike, Utc};

use crate::adaptation::HIGH_CONFIDENCE_THRESHOLD;
use crate::channels::director::{DirectorConstraints, TimeOfDay};
use crate::taste_model::chord_client::{ChordClient, DEFAULT_MODEL};
use crate::tracker::interpret::{InterpretedSignal, SignalKind};

// --- frequency (tunable, AC-required) ---------------------------------------

/// How often the assistant is willing to ask. Tunable per account/deployment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AskFrequency {
    /// The default: ask on every material + low-confidence fork.
    #[default]
    Standard,
    /// A stricter bar than [`Self::Standard`] — only the single most
    /// decision-changing fork kind (`Negative`, a real dislike) clears
    /// materiality; `Fatigue`/`Engagement` forks go silent even when
    /// low-confidence, deferring to the fast loop's next pass or the slow
    /// loop's sustained-pattern consolidation instead of asking about them.
    Reduced,
    /// "Never ask, infer only." [`decide_ask`] and [`pre_session_ask_decision`]
    /// always return [`AskDecision::Silent`] in this mode, unconditionally.
    Never,
}

// --- confidence a resolved answer carries -----------------------------------

/// The confidence [`answer_to_signal`] assigns a user-stated answer. Maximal
/// (`1.0`, not merely `>= HIGH_CONFIDENCE_THRESHOLD`) because an explicit
/// answer isn't evidence toward a guess — it IS the ground truth the rest of
/// the loop's confidence scale is trying to approximate.
pub const ANSWERED_SIGNAL_CONFIDENCE: f32 = 1.0;

// --- materiality (mirrors fast_adapt's own classification) -----------------

/// Would [`crate::adaptation::fast_adapt`] act on this signal `kind` if it
/// were confident? Mirrors that function's match arms exactly — `Negative`,
/// `Fatigue`, and `Engagement` each produce a real next-slot shift there;
/// `Interruption` never does, at any confidence (see `fast_adapt`'s own
/// doc/tests). A kind that the fast loop would never act on is, by the same
/// logic, never worth asking about — asking is only for forks that would
/// otherwise change what happens next.
pub fn is_material_fork(kind: SignalKind, frequency: AskFrequency) -> bool {
    match frequency {
        AskFrequency::Never => false,
        AskFrequency::Reduced => matches!(kind, SignalKind::Negative),
        AskFrequency::Standard => !matches!(kind, SignalKind::Interruption),
    }
}

// --- Lumina voice ------------------------------------------------------------

/// The system prompt for an optional Chord-phrased question — same warm,
/// concise, facts-only register `proactive::generators::build_message`
/// establishes for Lumina, adapted for a clarifying QUESTION rather than a
/// relayed nudge.
const LUMINA_QUESTION_SYSTEM: &str = "You are Lumina, a warm, concise personal assistant for Muse \
    (a private media companion). You are about to ask the account ONE short, natural-sounding \
    clarifying question. You MUST ask ONLY the question given below, rephrased for warmth — never \
    invent a new question, a plot detail, or any claim not present in the prompt. Do not add a \
    preamble or explanation, just the one question.";

/// What kind of moment produced a [`ConversationalQuestion`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AskQuestionKind {
    /// The single pre-session intent question.
    PreSessionIntent,
    /// A mid-session question resolving one ambiguous [`SignalKind`] fork.
    MidSessionFork(SignalKind),
}

/// One question the assistant may ask, already phrased in Lumina's voice
/// (the deterministic template — see the module doc for why the template,
/// not a live Chord call, is what every pure test exercises).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConversationalQuestion {
    pub kind: AskQuestionKind,
    /// The deterministic, always-available phrasing.
    pub prompt: String,
}

/// The optional Chord-phrased delivery, mirroring
/// `proactive::generators::build_message` exactly: falls back to
/// `question.prompt` verbatim whenever `chord` is `None` or the call fails —
/// never a blocking dependency, and never called by any test in this module
/// (the seam is mocked/absent, per the anti-hang contract).
pub async fn build_question_message(
    chord: Option<&ChordClient>,
    question: &ConversationalQuestion,
) -> String {
    let Some(client) = chord else {
        return question.prompt.clone();
    };

    let user = format!("Question to ask: {}", question.prompt);
    match client
        .chat_completion(DEFAULT_MODEL, LUMINA_QUESTION_SYSTEM, &user)
        .await
    {
        Ok(text) => text,
        Err(e) => {
            tracing::warn!(
                error = %e,
                "MUSEX-12: chord question phrasing failed; falling back to the templated question"
            );
            question.prompt.clone()
        }
    }
}

// --- the mid-session gate (load-bearing) ------------------------------------

/// One outcome of [`decide_ask`]: either ask a real question, or stay
/// silent with a documented reason (surfaced for audit/tests, never shown to
/// the account).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AskDecision {
    Ask {
        question: ConversationalQuestion,
        kind: AskQuestionKind,
    },
    Silent {
        reason: String,
    },
}

/// THE gate. Emits [`AskDecision::Ask`] only when `signal` is BOTH
/// [`is_material_fork`] AND strictly below
/// [`HIGH_CONFIDENCE_THRESHOLD`] — see the module doc for the full
/// justification. `subject_title` grounds the question in the actual title
/// the signal came from (never a generic "this show").
pub fn decide_ask(
    signal: &InterpretedSignal,
    subject_title: &str,
    frequency: AskFrequency,
) -> AskDecision {
    if frequency == AskFrequency::Never {
        return AskDecision::Silent {
            reason: "AskFrequency::Never (silent mode) — infer only, never ask".to_string(),
        };
    }

    if !is_material_fork(signal.kind, frequency) {
        return AskDecision::Silent {
            reason: format!(
                "{:?} is not a material decision fork under {frequency:?} — fast_adapt itself \
                 never acts on it regardless of confidence, so asking would be nagging, not \
                 resolving ambiguity",
                signal.kind
            ),
        };
    }

    if signal.confidence >= HIGH_CONFIDENCE_THRESHOLD {
        return AskDecision::Silent {
            reason: format!(
                "confidence {:.2} >= HIGH_CONFIDENCE_THRESHOLD ({HIGH_CONFIDENCE_THRESHOLD:.2}) — \
                 the fork is clear; fast_adapt already adapts silently, asking would be redundant",
                signal.confidence
            ),
        };
    }

    let question = mid_session_question(signal, subject_title);
    AskDecision::Ask {
        kind: question.kind,
        question,
    }
}

/// Build the mid-session question text for a material, low-confidence
/// signal. One template per [`SignalKind`] this gate can actually reach
/// (`Negative`/`Fatigue`/`Engagement` — `Interruption` never reaches here,
/// see [`is_material_fork`]), each phrased as a light, easy-to-answer
/// conversational check-in, never a blocking gate.
fn mid_session_question(signal: &InterpretedSignal, subject_title: &str) -> ConversationalQuestion {
    let prompt = match signal.kind {
        SignalKind::Negative => {
            format!("Not feeling \"{subject_title}\"? Say the word and I'll switch it up.")
        }
        SignalKind::Fatigue => {
            "Getting late — want me to wind things down, or keep the channel going?".to_string()
        }
        SignalKind::Engagement => {
            format!("You're clearly into \"{subject_title}\" — want more like this next?")
        }
        SignalKind::Interruption => {
            // Unreachable through decide_ask (is_material_fork excludes it),
            // but total rather than partial so this stays a safe function to
            // call directly in a test without panicking.
            "Everything okay? No rush either way.".to_string()
        }
    };
    ConversationalQuestion {
        kind: AskQuestionKind::MidSessionFork(signal.kind),
        prompt,
    }
}

// --- answer -> high-confidence signal ---------------------------------------

/// Turn a resolved answer into an [`InterpretedSignal`] the rest of the loop
/// (`crate::adaptation::fast_adapt`) can consume exactly like a passively
/// interpreted one — at [`ANSWERED_SIGNAL_CONFIDENCE`], always clearing
/// [`HIGH_CONFIDENCE_THRESHOLD`].
pub fn answer_to_signal(kind: SignalKind, note: impl Into<String>) -> InterpretedSignal {
    InterpretedSignal {
        kind,
        confidence: ANSWERED_SIGNAL_CONFIDENCE,
        rationale: format!("user-answered: {}", note.into()),
    }
}

// --- pre-session intent question ---------------------------------------------

/// Coarse energy level a pre-session answer selects — the "vibe" half of
/// "who/energy/time-budget."
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnergyLevel {
    Low,
    Medium,
    High,
}

/// A resolved pre-session intent answer — who's watching, their energy, and
/// how long they have. Deliberately plain fields (not yet another builder)
/// since this is the terminal shape a caller's UI/chat answer parses into.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IntentAnswer {
    pub who: String,
    pub energy: EnergyLevel,
    pub time_budget_minutes: i64,
}

/// The single pre-session intent question, in Lumina's voice.
pub fn pre_session_intent_question() -> ConversationalQuestion {
    ConversationalQuestion {
        kind: AskQuestionKind::PreSessionIntent,
        prompt: "What's the vibe tonight? Who's watching, how much energy do you have, and how \
                 much time do we have?"
            .to_string(),
    }
}

/// Whether to ask the pre-session intent question at all, gated only by
/// [`AskFrequency`] (there's no signal/confidence yet at session start — the
/// only reason to skip is silent mode).
pub fn pre_session_ask_decision(frequency: AskFrequency) -> AskDecision {
    if frequency == AskFrequency::Never {
        return AskDecision::Silent {
            reason: "AskFrequency::Never (silent mode) — infer only, never ask".to_string(),
        };
    }
    let question = pre_session_intent_question();
    AskDecision::Ask {
        kind: question.kind,
        question,
    }
}

/// Map an [`EnergyLevel`] to a persona-name suggestion. A deliberately small,
/// stable vocabulary (not a DB lookup) — the caller is expected to resolve
/// this name via the real addressability seam
/// (`crate::repo::persona::get_by_name_for_account`), falling back to a
/// derived/explicit persona of that name if one doesn't exist yet; this
/// module only decides WHICH name a "high energy" vs "winding down" answer
/// points at, never touches the database itself.
pub fn suggested_persona_name(energy: EnergyLevel) -> &'static str {
    match energy {
        EnergyLevel::Low => "wind-down",
        EnergyLevel::Medium => "evening",
        EnergyLevel::High => "high-energy",
    }
}

/// Program the session from a resolved [`IntentAnswer`]: a persona-name
/// suggestion (see [`suggested_persona_name`]) plus real
/// [`DirectorConstraints`] — the SAME type MUSEX-05's director consumes, so
/// a caller can hand this straight to `channels::director::program_channel`
/// without a second "session plan" shape in between. `time_of_day` is
/// derived from `now` via `TimeOfDay::from_hour` (the director's own
/// bucketing), never guessed independently.
pub fn program_from_intent(
    answer: &IntentAnswer,
    now: DateTime<Utc>,
) -> (&'static str, DirectorConstraints) {
    let persona_name = suggested_persona_name(answer.energy);
    let end_by = now + ChronoDuration::minutes(answer.time_budget_minutes.max(0));
    let constraints = DirectorConstraints {
        start_at: now,
        end_by,
        time_of_day: TimeOfDay::from_hour(now.hour()),
        // A gentle default: the pre-session flow programs a plausible
        // channel, not an exploration-heavy one — same STANDARD-equivalent
        // posture `adaptation::Aggressiveness::default()` takes for the
        // adaptation loops, kept local since `DirectorConstraints` has no
        // shared default constant of its own.
        serendipity_budget: 0.2,
        max_slots: 0,
        seed: now.timestamp() as u64,
    };
    (persona_name, constraints)
}

// --- post-watch reaction (optional, NEVER a gate) ---------------------------

/// A frictionless post-watch reaction — one tap, nothing more. Optional by
/// construction: [`record_post_watch_reaction`] takes `Option<Self>` and a
/// `None` is always a completely valid, silent input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PostWatchReaction {
    ThumbsUp,
    ThumbsDown,
}

/// Record an optional post-watch reaction as a high-confidence signal.
/// `None` -> `None`: there is no gate here, nothing gets blocked or nagged
/// for waiting on a reaction that never comes. `Some(reaction)` becomes a
/// ground-truth [`InterpretedSignal`] via [`answer_to_signal`], exactly like
/// a resolved mid-session/pre-session answer.
pub fn record_post_watch_reaction(
    reaction: Option<PostWatchReaction>,
) -> Option<InterpretedSignal> {
    let reaction = reaction?;
    let (kind, note) = match reaction {
        PostWatchReaction::ThumbsUp => (SignalKind::Engagement, "post-watch thumbs up"),
        PostWatchReaction::ThumbsDown => (SignalKind::Negative, "post-watch thumbs down"),
    };
    Some(answer_to_signal(kind, note))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn signal(kind: SignalKind, confidence: f32) -> InterpretedSignal {
        InterpretedSignal {
            kind,
            confidence,
            rationale: format!("test signal: {kind:?} @ {confidence}"),
        }
    }

    // --- negative tests: no over-asking (load-bearing) ----------------------

    #[test]
    fn does_not_ask_on_a_clear_high_confidence_signal() {
        // A confident Negative is exactly the case fast_adapt already
        // handles silently (DifferentGenre) — asking would be redundant.
        let sig = signal(SignalKind::Negative, 0.9);
        let decision = decide_ask(&sig, "Some Show", AskFrequency::Standard);
        match decision {
            AskDecision::Silent { reason } => {
                assert!(reason.contains("HIGH_CONFIDENCE_THRESHOLD"));
            }
            AskDecision::Ask { .. } => panic!("must not ask on a clear, high-confidence signal"),
        }
    }

    #[test]
    fn does_not_nag_on_trivial_or_immaterial_signals() {
        // Interruption: even at very low confidence, and even at maximum
        // confidence, this is a routine real-world event, never a taste
        // decision fork worth interrupting the account over.
        for confidence in [0.1, 0.5, 0.99] {
            let sig = signal(SignalKind::Interruption, confidence);
            let decision = decide_ask(&sig, "Some Show", AskFrequency::Standard);
            match decision {
                AskDecision::Silent { reason } => {
                    assert!(reason.contains("not a material decision fork"));
                }
                AskDecision::Ask { .. } => {
                    panic!("must not nag on an immaterial Interruption signal (conf {confidence})")
                }
            }
        }
    }

    #[test]
    fn silent_mode_never_asks() {
        // A textbook material + low-confidence fork — would Ask under
        // Standard/Reduced — but AskFrequency::Never always wins.
        let sig = signal(SignalKind::Negative, 0.4);
        let decision = decide_ask(&sig, "Some Show", AskFrequency::Never);
        match decision {
            AskDecision::Silent { reason } => assert!(reason.contains("AskFrequency::Never")),
            AskDecision::Ask { .. } => panic!("AskFrequency::Never must always be silent"),
        }
    }

    #[test]
    fn reduced_frequency_narrows_materiality_to_negative_only() {
        // Fatigue is material under Standard but not under Reduced.
        let sig = signal(SignalKind::Fatigue, 0.4);
        assert!(matches!(
            decide_ask(&sig, "Some Show", AskFrequency::Standard),
            AskDecision::Ask { .. }
        ));
        assert!(matches!(
            decide_ask(&sig, "Some Show", AskFrequency::Reduced),
            AskDecision::Silent { .. }
        ));
    }

    // --- positive: material + low-confidence -> Ask -------------------------

    #[test]
    fn material_and_low_confidence_negative_fork_asks_a_real_question() {
        let sig = signal(SignalKind::Negative, 0.3);
        let decision = decide_ask(&sig, "Some Show", AskFrequency::Standard);
        match decision {
            AskDecision::Ask { question, kind } => {
                assert_eq!(kind, AskQuestionKind::MidSessionFork(SignalKind::Negative));
                assert!(question.prompt.contains("Some Show"));
            }
            AskDecision::Silent { reason } => {
                panic!("expected an Ask for a material, low-confidence fork, got Silent({reason})")
            }
        }
    }

    #[test]
    fn material_and_low_confidence_fatigue_and_engagement_also_ask() {
        for kind in [SignalKind::Fatigue, SignalKind::Engagement] {
            let sig = signal(kind, 0.5);
            let decision = decide_ask(&sig, "Some Show", AskFrequency::Standard);
            assert!(
                matches!(decision, AskDecision::Ask { .. }),
                "{kind:?} at low confidence should ask under Standard frequency"
            );
        }
    }

    #[test]
    fn confidence_exactly_at_threshold_does_not_ask() {
        // decide_ask's clear boundary matches fast_adapt's: >= threshold is
        // clear, not ambiguous.
        let sig = signal(SignalKind::Negative, HIGH_CONFIDENCE_THRESHOLD);
        let decision = decide_ask(&sig, "Some Show", AskFrequency::Standard);
        assert!(matches!(decision, AskDecision::Silent { .. }));
    }

    #[test]
    fn confidence_just_below_threshold_asks() {
        let sig = signal(SignalKind::Negative, HIGH_CONFIDENCE_THRESHOLD - 0.01);
        let decision = decide_ask(&sig, "Some Show", AskFrequency::Standard);
        assert!(matches!(decision, AskDecision::Ask { .. }));
    }

    // --- answer -> high-confidence signal ------------------------------------

    #[test]
    fn answer_to_signal_is_always_high_confidence() {
        let resolved = answer_to_signal(SignalKind::Negative, "didn't like it");
        assert_eq!(resolved.kind, SignalKind::Negative);
        assert!(resolved.confidence >= HIGH_CONFIDENCE_THRESHOLD);
        assert_eq!(resolved.confidence, ANSWERED_SIGNAL_CONFIDENCE);
        assert!(resolved.rationale.contains("didn't like it"));
    }

    #[test]
    fn a_resolved_answer_would_clear_fast_adapts_own_gate() {
        // The whole point of answer_to_signal: what it produces must be
        // usable directly by fast_adapt's real confidence gate.
        let resolved = answer_to_signal(SignalKind::Negative, "nope");
        assert!(resolved.confidence >= crate::adaptation::HIGH_CONFIDENCE_THRESHOLD);
    }

    // --- pre-session intent -------------------------------------------------

    #[test]
    fn pre_session_ask_decision_asks_by_default() {
        let decision = pre_session_ask_decision(AskFrequency::Standard);
        assert!(matches!(
            decision,
            AskDecision::Ask {
                kind: AskQuestionKind::PreSessionIntent,
                ..
            }
        ));
    }

    #[test]
    fn pre_session_ask_decision_is_silent_in_never_mode() {
        let decision = pre_session_ask_decision(AskFrequency::Never);
        assert!(matches!(decision, AskDecision::Silent { .. }));
    }

    #[test]
    fn program_from_intent_builds_real_director_constraints() {
        let now = Utc::now();
        let answer = IntentAnswer {
            who: "just me".to_string(),
            energy: EnergyLevel::Low,
            time_budget_minutes: 90,
        };
        let (persona_name, constraints) = program_from_intent(&answer, now);

        assert_eq!(persona_name, "wind-down");
        assert_eq!(constraints.start_at, now);
        assert_eq!(constraints.end_by, now + ChronoDuration::minutes(90));
        assert_eq!(constraints.time_of_day, TimeOfDay::from_hour(now.hour()));
    }

    #[test]
    fn program_from_intent_never_produces_a_negative_time_budget() {
        let now = Utc::now();
        let answer = IntentAnswer {
            who: "just me".to_string(),
            energy: EnergyLevel::Medium,
            time_budget_minutes: -30,
        };
        let (_, constraints) = program_from_intent(&answer, now);
        assert!(constraints.end_by >= constraints.start_at);
    }

    #[test]
    fn energy_levels_map_to_distinct_persona_names() {
        assert_eq!(suggested_persona_name(EnergyLevel::Low), "wind-down");
        assert_eq!(suggested_persona_name(EnergyLevel::Medium), "evening");
        assert_eq!(suggested_persona_name(EnergyLevel::High), "high-energy");
    }

    // --- post-watch reaction: optional, never a gate -------------------------

    #[test]
    fn post_watch_reaction_none_is_always_valid_and_silent() {
        // `InterpretedSignal` (the shared MUSEX-10 type) deliberately has no
        // `PartialEq`, so assert on the Option's emptiness, not `== None`.
        assert!(record_post_watch_reaction(None).is_none());
    }

    #[test]
    fn post_watch_thumbs_up_becomes_a_high_confidence_engagement_signal() {
        let resolved = record_post_watch_reaction(Some(PostWatchReaction::ThumbsUp)).unwrap();
        assert_eq!(resolved.kind, SignalKind::Engagement);
        assert!(resolved.confidence >= HIGH_CONFIDENCE_THRESHOLD);
    }

    #[test]
    fn post_watch_thumbs_down_becomes_a_high_confidence_negative_signal() {
        let resolved = record_post_watch_reaction(Some(PostWatchReaction::ThumbsDown)).unwrap();
        assert_eq!(resolved.kind, SignalKind::Negative);
        assert!(resolved.confidence >= HIGH_CONFIDENCE_THRESHOLD);
    }

    // --- Lumina voice: template is always available, no live LLM ------------

    #[tokio::test]
    async fn build_question_message_falls_back_to_the_template_with_no_chord_client() {
        let question = pre_session_intent_question();
        let message = build_question_message(None, &question).await;
        assert_eq!(message, question.prompt);
    }

    // --- determinism ----------------------------------------------------------

    #[test]
    fn decide_ask_is_deterministic_for_identical_inputs() {
        let sig = signal(SignalKind::Negative, 0.3);
        let a = decide_ask(&sig, "Some Show", AskFrequency::Standard);
        let b = decide_ask(&sig, "Some Show", AskFrequency::Standard);
        assert_eq!(a, b);
    }

    #[test]
    fn mid_session_question_text_is_deterministic() {
        let sig = signal(SignalKind::Fatigue, 0.4);
        let q1 = mid_session_question(&sig, "Some Show");
        let q2 = mid_session_question(&sig, "Some Show");
        assert_eq!(q1, q2);
    }
}
