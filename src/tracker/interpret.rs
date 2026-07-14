//! (D) Play-state telemetry INTERPRETATION — MUSEX-10 (Plane TERM #386).
//!
//! Ingests already-recorded play-state telemetry (the `play_events` stream
//! written by [`super::webhook`]/`super::poller`, folded by
//! [`super::reconstruct::fold_events`]) and DISAMBIGUATES what a stop/pause
//! pattern actually means: dislike vs fatigue vs interruption vs delight —
//! using pattern + TIME-OF-NIGHT, not just "did they finish it." An
//! 82%-stop at 11:40pm after two episodes is a good night, not a bad show;
//! this module is what tells those apart. This is the loop's *passive*
//! sense: [`InterpretedSignal`] is the input a future adaptation loop
//! (MUSEX-11) consumes alongside the explicit `taste_signals` derived in
//! `crate::taste_model::signals` — this module does not write to
//! `taste_signals` itself.
//!
//! ## Server-agnostic by construction
//!
//! [`PlayStateEvent`]/[`PlayStateEventKind`] is a NORMALIZED shape. An
//! adapter (today: [`PlayStateEventKind::from_plex_event_type`], mapping
//! the Plex webhook's `media.*` vocabulary; tomorrow: a Jellyfin webhook via
//! [`PlayStateEventKind::from_jellyfin_notification_type`], mapping the
//! Jellyfin webhook plugin's `Playback*` `NotificationType` vocabulary — see
//! `config.rs`'s `jellyfin_url`/`jellyfin_token`, the only other Jellyfin
//! footprint in this crate today per MUSEX-09) maps its own event names
//! into this ONE vocabulary at the boundary.
//!
//! The ingest entry point [`interpret_from_events`] normalizes EVERY incoming
//! event through this boundary FIRST (see [`normalize_to_fold_vocab`]),
//! dispatching on the event's `source`, so a Jellyfin `PlaybackStop` actually
//! flows into a stopped session end-to-end — not just in an isolated name
//! test. The shared, Plex-keyed fold ([`super::reconstruct::fold_events`],
//! also used by MUSET-08's shadow runner) is left untouched: normalization
//! re-expresses each kind as the Plex-vocabulary `event_type` the fold
//! already understands ([`PlayStateEventKind::to_plex_event_type`]).
//!
//! ## READ-ONLY, always
//!
//! This module reads `play_events` telemetry and produces an
//! [`InterpretedSignal`] value. It NEVER calls a playback-control API
//! (play/pause/seek/stop against a live server) — that surface lives in a
//! separate, dedicated cast-control module elsewhere in this crate; this
//! module doesn't reference it, import it, or link against any HTTP client.
//! The `no_playback_mutation_calls` test below source-scans this file's
//! non-test code to keep that true.

use chrono::{DateTime, Timelike, Utc};

use crate::channels::TimeOfDay;
use crate::models::play_event::PlayEvent;
use crate::tracker::reconstruct::{self, Fold};

// --- normalization boundary -------------------------------------------------

/// A normalized play-state event kind — SERVER-AGNOSTIC. Adapters map their
/// own vocabulary into this; nothing downstream branches on a
/// server-specific string again.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlayStateEventKind {
    Start,
    Pause,
    Resume,
    Seek,
    Stop,
    Complete,
}

impl PlayStateEventKind {
    /// Plex webhook `event` field (persisted verbatim as
    /// [`PlayEvent::event_type`] by `super::webhook`) → normalized kind.
    /// Plex has no distinct "seek" event; a seek shows up as a `media.play`
    /// with a jumped `viewOffset`, which is a pattern-level concern (see
    /// [`longest_single_pause_ms`]), not a normalization concern.
    pub fn from_plex_event_type(event_type: &str) -> Option<Self> {
        match event_type {
            "media.play" => Some(Self::Start),
            "media.resume" => Some(Self::Resume),
            "media.pause" => Some(Self::Pause),
            "media.stop" => Some(Self::Stop),
            "media.scrobble" => Some(Self::Complete),
            _ => None,
        }
    }

    /// Jellyfin webhook-plugin `NotificationType` → normalized kind. Muse
    /// has no Jellyfin play-state ingest endpoint yet (only the config-gated
    /// `JellyfinSyncPlay` watch-together stub, per MUSEX-09) — this mapping
    /// is defined at the adapter boundary ahead of that endpoint landing, so
    /// the interpreter is exercised as server-agnostic from day one rather
    /// than accreting Plex-only assumptions that a future Jellyfin adapter
    /// would have to unwind.
    pub fn from_jellyfin_notification_type(notification_type: &str) -> Option<Self> {
        match notification_type {
            "PlaybackStart" => Some(Self::Start),
            "PlaybackUnpause" => Some(Self::Resume),
            "PlaybackPause" => Some(Self::Pause),
            "PlaybackStop" => Some(Self::Stop),
            "PlaybackProgress" => Some(Self::Seek),
            _ => None,
        }
    }

    /// Map a normalized kind BACK to the Plex-vocabulary `event_type` string
    /// that the shared fold ([`reconstruct::fold_events`]) and
    /// [`longest_single_pause_ms`] key on. This is the boundary shim that
    /// makes the ingest path server-agnostic end-to-end WITHOUT touching the
    /// shared, Plex-keyed fold (which MUSET-08's shadow runner also depends
    /// on): a Jellyfin `PlaybackStop` → [`Self::Stop`] → `"media.stop"` folds
    /// into a stopped session exactly like a native Plex `media.stop`. A
    /// `Seek`/progress tick maps to `"media.play"` — the fold treats it as a
    /// "still playing" snapshot that advances the view offset, which is
    /// exactly Plex's own model of a seek (Plex has no distinct seek event).
    pub fn to_plex_event_type(self) -> &'static str {
        match self {
            Self::Start => "media.play",
            Self::Resume => "media.resume",
            Self::Pause => "media.pause",
            Self::Seek => "media.play",
            Self::Stop => "media.stop",
            Self::Complete => "media.scrobble",
        }
    }
}

/// A normalized play-state event: the shape every interpreter function
/// consumes, regardless of originating server.
#[derive(Debug, Clone)]
pub struct PlayStateEvent {
    pub kind: PlayStateEventKind,
    pub at: DateTime<Utc>,
    /// 0.0-1.0 progress through the item's runtime at this event, when
    /// derivable (needs both `view_offset_ms` and the item's `duration_ms`).
    pub percent: Option<f32>,
    pub session_key: String,
    pub item_ref: Option<String>,
}

impl PlayStateEvent {
    /// Normalize a raw [`PlayEvent`] row into the server-agnostic shape.
    /// `duration_ms` is the item runtime (from the session's [`Fold`]) used
    /// to turn `view_offset_ms` into a percent; pass `None` when unknown.
    /// Returns `None` for an event type no known adapter recognizes (the
    /// row is still durable in `play_events` regardless — see
    /// `super::webhook::handle_payload`).
    pub fn from_play_event(ev: &PlayEvent, duration_ms: Option<i64>) -> Option<Self> {
        let kind = match ev.source.as_str() {
            "jellyfin_webhook" => {
                PlayStateEventKind::from_jellyfin_notification_type(&ev.event_type)?
            }
            _ => PlayStateEventKind::from_plex_event_type(&ev.event_type)?,
        };
        let percent = match (ev.view_offset_ms, duration_ms) {
            (Some(offset), Some(d)) if d > 0 => Some((offset as f32 / d as f32).clamp(0.0, 1.0)),
            _ => None,
        };
        Some(PlayStateEvent {
            kind,
            at: ev.received_at,
            percent,
            session_key: ev.session_key.clone().unwrap_or_default(),
            item_ref: ev.rating_key.clone(),
        })
    }
}

// --- disambiguation output ---------------------------------------------------

/// The four passive signal kinds this module disambiguates between.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignalKind {
    /// A real dislike/taste-mismatch signal (early abandon).
    Negative,
    /// Stopped near the end, late at night, after multiple episodes — "good
    /// night," not "bad show."
    Fatigue,
    /// A long pause mid-session — a real-world interruption, not a taste
    /// decision either way.
    Interruption,
    /// Multiple completions back to back — a delight/engagement signal.
    Engagement,
}

/// A disambiguated interpretation of one session's play-state pattern.
#[derive(Debug, Clone)]
pub struct InterpretedSignal {
    pub kind: SignalKind,
    /// 0.0-1.0 — how strongly the pattern matched the rule that fired.
    pub confidence: f32,
    /// Human-readable explanation of which rule fired and why (audit trail
    /// / debugging; not parsed by callers).
    pub rationale: String,
}

// --- thresholds (documented, deterministic) ---------------------------------

/// Below this fraction of runtime, a stop is a strong dislike signal.
/// Mirrors `reconstruct::ABANDON_THRESHOLD` — the same "abandoned" cutoff
/// this crate already uses for `play_sessions.is_abandoned`.
pub const EARLY_ABANDON_PCT_MAX: f32 = reconstruct::ABANDON_THRESHOLD;

/// At/above this fraction, a non-scrobbled stop is "close enough to done"
/// that a late-night stop reads as sleepiness rather than dislike.
pub const FATIGUE_PCT_MIN: f32 = 0.65;

/// "After multiple episodes" — at least this many episodes watched in the
/// same sitting before this stop, for the fatigue rule.
pub const FATIGUE_MIN_EPISODE_STREAK: u32 = 2;

/// A single continuous pause at/above this length signals a real-world
/// interruption (phone call, doorbell, etc.) rather than a taste decision.
pub const LONG_PAUSE_MS_THRESHOLD: i64 = 20 * 60 * 1000; // 20 minutes

/// This many (or more) consecutive completed episodes in one sitting is a
/// binge / engagement signal.
pub const BINGE_STREAK_MIN: u32 = 3;

// --- session pattern (the interpreter's real input) --------------------------

/// The aggregated pattern [`interpret_play_state`] disambiguates over. Built
/// from a real [`Fold`] (this crate's one fold/reconstruction algorithm —
/// see `reconstruct::fold_events`) plus two things the fold doesn't compute:
/// the longest single pause, and the caller-supplied episode streak (cross-
/// session history the fold can't see from one session's events alone).
#[derive(Debug, Clone)]
pub struct SessionPattern {
    /// 0.0-1.0 (from `Fold::percent_complete`, `0.0` if unknown — an
    /// unknown percent never accidentally reads as "finished").
    pub percent_complete: f32,
    pub stopped_at: DateTime<Utc>,
    pub is_finished: bool,
    pub longest_pause_ms: i64,
    /// Consecutive episodes completed in this watch sitting, ending at (and
    /// including, if this one finished) the current session.
    pub episode_streak: u32,
}

impl SessionPattern {
    /// Build from a real `Fold`. Returns `None` when the session hasn't
    /// stopped yet (`Fold::stopped_at` is `None`) — there is nothing to
    /// disambiguate about a still-playing session.
    pub fn from_fold(fold: &Fold, longest_pause_ms: i64, episode_streak: u32) -> Option<Self> {
        let stopped_at = fold.stopped_at?;
        Some(SessionPattern {
            percent_complete: fold.percent_complete.unwrap_or(0.0),
            stopped_at,
            is_finished: fold.is_finished,
            longest_pause_ms,
            episode_streak,
        })
    }
}

/// The longest single continuous pause span, in ms, across a session's raw
/// events — a small, separate aggregation from `reconstruct::fold_events`
/// (which tracks pause *count* and *total* paused_ms, not the longest single
/// span). Pure; sorts internally by `(received_at, id)` exactly like
/// `fold_events`, so it's equally order-independent/late-event-tolerant.
pub fn longest_single_pause_ms(events: &[PlayEvent]) -> i64 {
    let mut sorted: Vec<&PlayEvent> = events.iter().collect();
    sorted.sort_by(|a, b| a.received_at.cmp(&b.received_at).then(a.id.cmp(&b.id)));

    let mut longest = 0i64;
    let mut pause_started_at: Option<DateTime<Utc>> = None;
    for ev in sorted {
        match ev.event_type.as_str() {
            "media.pause" => {
                if pause_started_at.is_none() {
                    pause_started_at = Some(ev.received_at);
                }
            }
            "media.play" | "media.resume" | "media.stop" => {
                if let Some(start) = pause_started_at.take() {
                    longest = longest.max((ev.received_at - start).num_milliseconds().max(0));
                }
            }
            _ => {}
        }
    }
    longest
}

/// The SERVER-AGNOSTIC ingest boundary. Normalize a raw event stream into
/// the shared Plex-vocabulary the fold keys on, dispatching each event on its
/// `source` (Plex vs Jellyfin). This is what makes ingest server-agnostic
/// *end to end* rather than only in the isolated name helpers: a Jellyfin
/// `PlaybackStop` (source `jellyfin_webhook`) is normalized to
/// [`PlayStateEventKind::Stop`] then re-expressed as `"media.stop"`, so the
/// shared, Plex-keyed [`reconstruct::fold_events`]/[`longest_single_pause_ms`]
/// fold it into a stopped session exactly like a native Plex event. Without
/// this, those raw Jellyfin strings fall through the fold's Plex-string match
/// and no stop/pause is ever detected.
///
/// Preserves native Plex behavior byte-for-byte: a recognized Plex event
/// normalizes to its own identical `event_type` (identity rewrite), and an
/// UNRECOGNIZED event (Plex or otherwise — e.g. `media.rate`, a future event
/// type) is passed through UNCHANGED rather than dropped, so the fold's own
/// `advance()`/`_ => {}` handling of it is identical to feeding the raw
/// stream. `reconstruct::fold_events` is never modified.
fn normalize_to_fold_vocab(events: &[PlayEvent]) -> Vec<PlayEvent> {
    events
        .iter()
        .map(|ev| {
            let kind = if ev.source.contains("jellyfin") {
                PlayStateEventKind::from_jellyfin_notification_type(&ev.event_type)
            } else {
                PlayStateEventKind::from_plex_event_type(&ev.event_type)
            };
            match kind {
                Some(k) => {
                    let mut normalized = ev.clone();
                    normalized.event_type = k.to_plex_event_type().to_string();
                    normalized
                }
                // Unrecognized: pass through untouched so the shared fold's
                // existing handling is unchanged (Plex behavior preserved).
                None => ev.clone(),
            }
        })
        .collect()
}

/// Ingest a session's raw events (from EITHER server) and interpret the
/// result. Normalizes the stream through the server-agnostic boundary
/// ([`normalize_to_fold_vocab`]) FIRST, then reuses `reconstruct::fold_events`
/// — the one real fold algorithm, not reinvented here — for the
/// finished/percent/stopped judgment. `episode_streak` is caller-supplied
/// (cross-session context this crate's caller — e.g. a future ingest hook —
/// is expected to track). Returns `None` only when there's nothing to fold
/// (`events` empty) or the session hasn't stopped yet.
pub fn interpret_from_events(
    events: &[PlayEvent],
    episode_streak: u32,
) -> Option<InterpretedSignal> {
    let normalized = normalize_to_fold_vocab(events);
    let fold = reconstruct::fold_events(&normalized)?;
    let pause_ms = longest_single_pause_ms(&normalized);
    let pattern = SessionPattern::from_fold(&fold, pause_ms, episode_streak)?;
    Some(interpret_play_state(&pattern))
}

// --- the disambiguation rules -------------------------------------------------

/// Deterministic, rules-based disambiguation over a session's pattern.
/// Rule precedence (each rule below is checked in order; the first match
/// wins — this makes the function pure, deterministic, and independent of
/// hash-map iteration order or randomness):
///
/// 1. **Binge → Engagement** — finished, with a streak of
///    [`BINGE_STREAK_MIN`]+ consecutive completed episodes this sitting.
///    Checked first so a long finishing binge isn't mistaken for late-night
///    fatigue just because it's also late.
/// 2. **Long pause → Interruption** — a single continuous pause at/above
///    [`LONG_PAUSE_MS_THRESHOLD`]. Checked next: a real-world interruption
///    is independent of where in the runtime it happened, and shouldn't be
///    swallowed by the fatigue/abandon rules below.
/// 3. **Late-stop-late-night → Fatigue** — stopped at/above
///    [`FATIGUE_PCT_MIN`], during [`TimeOfDay::LateNight`] (reusing
///    MUSEX-05's `TimeOfDay::from_hour` bucket — the same "late night"
///    boundary this crate already uses to shape a channel's energy arc),
///    after at least [`FATIGUE_MIN_EPISODE_STREAK`] episodes this sitting.
/// 4. **Early abandon → Negative** — stopped at/below
///    [`EARLY_ABANDON_PCT_MAX`] (mirrors `reconstruct::ABANDON_THRESHOLD`).
/// 5. **Default → low-confidence Negative** — an ambiguous mid-way stop that
///    matched none of the above. It didn't complete and no mitigating
///    pattern applied, but a middling stop is a much weaker dislike signal
///    than an early abandon, hence the low (0.3) confidence floor rather
///    than a confident verdict.
pub fn interpret_play_state(pattern: &SessionPattern) -> InterpretedSignal {
    // 1. Binge.
    if pattern.is_finished && pattern.episode_streak >= BINGE_STREAK_MIN {
        let confidence =
            confidence_from_excess(pattern.episode_streak as f32, BINGE_STREAK_MIN as f32, 5.0);
        return InterpretedSignal {
            kind: SignalKind::Engagement,
            confidence,
            rationale: format!(
                "finished with a {}-episode streak this sitting (binge threshold {})",
                pattern.episode_streak, BINGE_STREAK_MIN
            ),
        };
    }

    // 2. Long pause.
    if pattern.longest_pause_ms >= LONG_PAUSE_MS_THRESHOLD {
        let confidence = confidence_from_excess(
            pattern.longest_pause_ms as f32,
            LONG_PAUSE_MS_THRESHOLD as f32,
            LONG_PAUSE_MS_THRESHOLD as f32 * 3.0,
        );
        return InterpretedSignal {
            kind: SignalKind::Interruption,
            confidence,
            rationale: format!(
                "paused for {}m (>= {}m interruption threshold)",
                pattern.longest_pause_ms / 60_000,
                LONG_PAUSE_MS_THRESHOLD / 60_000
            ),
        };
    }

    // 3. Late-stop-late-night → fatigue.
    let late_night = TimeOfDay::from_hour(pattern.stopped_at.hour()) == TimeOfDay::LateNight;
    if pattern.percent_complete >= FATIGUE_PCT_MIN
        && late_night
        && pattern.episode_streak >= FATIGUE_MIN_EPISODE_STREAK
    {
        let pct_margin = confidence_from_excess(
            pattern.percent_complete,
            FATIGUE_PCT_MIN,
            1.0 - FATIGUE_PCT_MIN,
        );
        let streak_margin = confidence_from_excess(
            pattern.episode_streak as f32,
            FATIGUE_MIN_EPISODE_STREAK as f32,
            3.0,
        );
        let confidence = ((pct_margin + streak_margin) / 2.0).clamp(0.5, 0.97);
        return InterpretedSignal {
            kind: SignalKind::Fatigue,
            confidence,
            rationale: format!(
                "stopped at {:.0}% around {:02}:00 after {} episodes this sitting — reads as sleepiness, not dislike",
                pattern.percent_complete * 100.0,
                pattern.stopped_at.hour(),
                pattern.episode_streak
            ),
        };
    }

    // 4. Early abandon → negative.
    if pattern.percent_complete <= EARLY_ABANDON_PCT_MAX {
        let confidence = confidence_from_excess(
            EARLY_ABANDON_PCT_MAX - pattern.percent_complete,
            0.0,
            EARLY_ABANDON_PCT_MAX,
        )
        .clamp(0.55, 0.97);
        return InterpretedSignal {
            kind: SignalKind::Negative,
            confidence,
            rationale: format!(
                "stopped at {:.0}% (<= {:.0}% early-abandon threshold)",
                pattern.percent_complete * 100.0,
                EARLY_ABANDON_PCT_MAX * 100.0
            ),
        };
    }

    // 5. Default: ambiguous mid-way stop, low-confidence negative.
    InterpretedSignal {
        kind: SignalKind::Negative,
        confidence: 0.3,
        rationale: format!(
            "stopped at {:.0}% with no strong disambiguating pattern (binge/interruption/fatigue thresholds unmet); \
             treated as a low-confidence negative default",
            pattern.percent_complete * 100.0
        ),
    }
}

/// A small, pure confidence-from-margin helper: `0.5` right at `threshold`,
/// scaling up toward (but never reaching) `1.0` as `value` exceeds
/// `threshold` by up to `scale`, capped at `0.99`. Deterministic, no
/// randomness/hash-order dependence.
fn confidence_from_excess(value: f32, threshold: f32, scale: f32) -> f32 {
    if scale <= 0.0 {
        return 0.5;
    }
    let excess = ((value - threshold) / scale).max(0.0);
    (0.5 + excess * 0.5).min(0.99)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn t(hour: u32, minute: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 7, 14, hour, minute, 0).unwrap()
    }

    fn pattern(
        percent: f32,
        stopped_at: DateTime<Utc>,
        finished: bool,
        pause_ms: i64,
        streak: u32,
    ) -> SessionPattern {
        SessionPattern {
            percent_complete: percent,
            stopped_at,
            is_finished: finished,
            longest_pause_ms: pause_ms,
            episode_streak: streak,
        }
    }

    // --- per-case disambiguation assertions (load-bearing) -------------------

    #[test]
    fn early_abandon_is_negative() {
        // Clean 5%-stop at 8pm (evening, not late night, no other pattern).
        let p = pattern(0.05, t(20, 0), false, 0, 1);
        let sig = interpret_play_state(&p);
        assert_eq!(sig.kind, SignalKind::Negative);
        assert!(
            sig.confidence >= 0.7,
            "a clean early abandon should be high-confidence: {}",
            sig.confidence
        );
    }

    #[test]
    fn late_stop_late_night_after_two_episodes_is_fatigue_not_dislike() {
        // The exact motivating case: 82% stop at 11:40pm after two episodes.
        let p = pattern(0.82, t(23, 40), false, 0, 2);
        let sig = interpret_play_state(&p);
        assert_eq!(
            sig.kind,
            SignalKind::Fatigue,
            "82%@23:40 after 2 episodes must read as fatigue, not dislike"
        );
        assert!(
            sig.confidence >= 0.6,
            "a clean 82%@11:40pm fatigue case should be high-confidence: {}",
            sig.confidence
        );
    }

    #[test]
    fn same_percent_stop_during_the_day_is_not_fatigue() {
        // Same 82% stop, but at 2pm — no late-night context, so it must NOT
        // be read as fatigue (proves fatigue genuinely depends on the hour,
        // not just the percent+streak).
        let p = pattern(0.82, t(14, 0), false, 0, 2);
        let sig = interpret_play_state(&p);
        assert_ne!(
            sig.kind,
            SignalKind::Fatigue,
            "a high-percent daytime stop is not fatigue"
        );
    }

    #[test]
    fn long_pause_is_interruption() {
        // Paused 25 minutes mid-session (well past the finish/abandon
        // thresholds either way), then stopped at 50%.
        let p = pattern(0.50, t(19, 0), false, 25 * 60 * 1000, 1);
        let sig = interpret_play_state(&p);
        assert_eq!(sig.kind, SignalKind::Interruption);
        assert!(sig.confidence >= 0.5);
    }

    #[test]
    fn binge_is_engagement() {
        // Finished, third episode in a row this sitting.
        let p = pattern(0.95, t(21, 0), true, 0, 3);
        let sig = interpret_play_state(&p);
        assert_eq!(sig.kind, SignalKind::Engagement);
        assert!(sig.confidence >= 0.5);
    }

    #[test]
    fn binge_beats_late_night_fatigue_when_both_patterns_present() {
        // Finished, 4-episode streak, AND late at night: binge (the
        // stronger, unambiguous positive signal) must win over fatigue.
        let p = pattern(0.98, t(23, 50), true, 0, 4);
        let sig = interpret_play_state(&p);
        assert_eq!(sig.kind, SignalKind::Engagement);
    }

    #[test]
    fn ambiguous_midway_stop_defaults_to_low_confidence_negative() {
        // 50% stop, daytime, single episode, no long pause: no strong
        // pattern matches any rule.
        let p = pattern(0.50, t(14, 0), false, 0, 1);
        let sig = interpret_play_state(&p);
        assert_eq!(sig.kind, SignalKind::Negative);
        assert!(
            sig.confidence <= 0.4,
            "an ambiguous default must be LOW confidence: {}",
            sig.confidence
        );
    }

    #[test]
    fn confidence_is_always_in_unit_range() {
        for pct in [0.0_f32, 0.1, 0.5, 0.82, 0.9, 1.0] {
            for hour in [2, 8, 14, 20, 23] {
                for streak in [0u32, 1, 2, 3, 5] {
                    let p = pattern(pct, t(hour, 0), pct >= 0.9, 0, streak);
                    let sig = interpret_play_state(&p);
                    assert!(
                        (0.0..=1.0).contains(&sig.confidence),
                        "confidence out of range for pct={pct} hour={hour} streak={streak}: {}",
                        sig.confidence
                    );
                }
            }
        }
    }

    // --- normalization / server-agnostic boundary -----------------------------

    #[test]
    fn plex_and_jellyfin_event_names_normalize_to_the_same_kind() {
        assert_eq!(
            PlayStateEventKind::from_plex_event_type("media.play"),
            Some(PlayStateEventKind::Start)
        );
        assert_eq!(
            PlayStateEventKind::from_jellyfin_notification_type("PlaybackStart"),
            Some(PlayStateEventKind::Start)
        );
        assert_eq!(
            PlayStateEventKind::from_plex_event_type("media.stop"),
            Some(PlayStateEventKind::Stop)
        );
        assert_eq!(
            PlayStateEventKind::from_jellyfin_notification_type("PlaybackStop"),
            Some(PlayStateEventKind::Stop)
        );
    }

    #[test]
    fn unrecognized_event_type_normalizes_to_none() {
        assert_eq!(PlayStateEventKind::from_plex_event_type("media.rate"), None);
        assert_eq!(
            PlayStateEventKind::from_jellyfin_notification_type("UserDataSaved"),
            None
        );
    }

    fn raw_event(
        id: i64,
        at: DateTime<Utc>,
        event_type: &str,
        source: &str,
        offset: Option<i64>,
    ) -> PlayEvent {
        PlayEvent {
            id,
            received_at: at,
            source: source.to_string(),
            event_type: event_type.to_string(),
            account_ref: Some("1".to_string()),
            session_key: Some("sess-x".to_string()),
            rating_key: Some("rk-x".to_string()),
            view_offset_ms: offset,
            player: None,
            platform: None,
            product: None,
            device: None,
            ip_address: None,
            raw: serde_json::json!({}),
        }
    }

    #[test]
    fn from_play_event_derives_percent_from_offset_and_duration() {
        let ev = raw_event(1, t(20, 0), "media.play", "plex_webhook", Some(50_000));
        let normalized =
            PlayStateEvent::from_play_event(&ev, Some(100_000)).expect("recognized event type");
        assert_eq!(normalized.kind, PlayStateEventKind::Start);
        assert_eq!(normalized.percent, Some(0.5));
    }

    #[test]
    fn from_play_event_source_selects_the_adapter() {
        let plex = raw_event(1, t(20, 0), "media.stop", "plex_webhook", None);
        let jelly = raw_event(2, t(20, 0), "PlaybackStop", "jellyfin_webhook", None);
        assert_eq!(
            PlayStateEvent::from_play_event(&plex, None).unwrap().kind,
            PlayStateEventKind::Stop
        );
        assert_eq!(
            PlayStateEvent::from_play_event(&jelly, None).unwrap().kind,
            PlayStateEventKind::Stop
        );
    }

    // --- reuse of fold_events (don't reinvent event parsing) ------------------

    #[test]
    fn interpret_from_events_reuses_fold_events_for_the_finished_judgment() {
        let t0 = t(23, 20);
        let events = vec![raw_event(1, t0, "media.play", "plex_webhook", Some(0)), {
            let mut e = raw_event(
                2,
                t0 + chrono::Duration::minutes(20),
                "media.stop",
                "plex_webhook",
                Some(82_000),
            );
            e.raw = serde_json::json!({"duration": 100_000});
            e
        }];
        // First event also carries duration so fold_events can compute
        // percent_complete from the very first row (extract_duration_ms
        // takes the max seen across the stream).
        let mut events = events;
        events[0].raw = serde_json::json!({"duration": 100_000});

        let sig = interpret_from_events(&events, 2).expect("a stopped session should interpret");
        assert_eq!(
            sig.kind,
            SignalKind::Fatigue,
            "82%@23:20 after 2 episodes via real fold_events output"
        );
    }

    #[test]
    fn jellyfin_stream_flows_end_to_end_and_matches_plex() {
        // The fatigue case — 82% stop at 23:40 after 2 episodes — but
        // delivered as JELLYFIN `PlaybackStart`/`PlaybackStop` events (source
        // `jellyfin_webhook`), NOT Plex `media.*` strings. Proves the whole
        // ingest -> interpret path is server-agnostic: without the
        // normalization shim, the raw-Plex-keyed fold would ignore these and
        // never produce a stopped session at all.
        let t0 = t(23, 20);
        let dur = serde_json::json!({ "duration": 100_000 });

        let mut j_start = raw_event(1, t0, "PlaybackStart", "jellyfin_webhook", Some(0));
        j_start.raw = dur.clone();
        let mut j_stop = raw_event(
            2,
            t0 + chrono::Duration::minutes(20),
            "PlaybackStop",
            "jellyfin_webhook",
            Some(82_000),
        );
        j_stop.raw = dur.clone();
        let jellyfin = vec![j_start, j_stop];

        let jelly_sig = interpret_from_events(&jellyfin, 2)
            .expect("a JELLYFIN stop must produce a stopped, interpretable session end-to-end");
        assert_eq!(
            jelly_sig.kind,
            SignalKind::Fatigue,
            "82%@23:40 after 2 episodes over Jellyfin must read as fatigue"
        );

        // The identical pattern delivered as a PLEX stream must interpret the
        // same way — server-agnostic, not Plex-only.
        let mut p_start = raw_event(1, t0, "media.play", "plex_webhook", Some(0));
        p_start.raw = dur.clone();
        let mut p_stop = raw_event(
            2,
            t0 + chrono::Duration::minutes(20),
            "media.stop",
            "plex_webhook",
            Some(82_000),
        );
        p_stop.raw = dur.clone();
        let plex = vec![p_start, p_stop];

        let plex_sig = interpret_from_events(&plex, 2).expect("plex stream should interpret");
        assert_eq!(
            jelly_sig.kind, plex_sig.kind,
            "a Jellyfin and a Plex stream of the same pattern must interpret identically"
        );
        assert!(
            (jelly_sig.confidence - plex_sig.confidence).abs() < 1e-6,
            "identical patterns from different servers must also yield identical confidence"
        );
    }

    #[test]
    fn raw_jellyfin_stop_is_ignored_by_the_fold_but_flows_after_normalization() {
        // Documents WHY the shim is load-bearing (this is exactly the gap the
        // review caught): feeding the raw Jellyfin vocabulary straight to
        // `fold_events` (as the pre-shim code did) yields NO stopped session,
        // because the fold keys on Plex strings.
        let t0 = t(23, 20);
        let mut j_stop = raw_event(
            2,
            t0 + chrono::Duration::minutes(20),
            "PlaybackStop",
            "jellyfin_webhook",
            Some(82_000),
        );
        j_stop.raw = serde_json::json!({ "duration": 100_000 });
        let raw = vec![
            raw_event(1, t0, "PlaybackStart", "jellyfin_webhook", Some(0)),
            j_stop,
        ];

        let raw_fold = reconstruct::fold_events(&raw).expect("fold produces a struct");
        assert!(
            raw_fold.stopped_at.is_none(),
            "raw Jellyfin PlaybackStop is not a Plex media.stop; the fold ignores it (the bug the shim fixes)"
        );

        // Through the normalization boundary it DOES fold into a stop.
        let normalized = normalize_to_fold_vocab(&raw);
        let norm_fold = reconstruct::fold_events(&normalized).expect("fold");
        assert!(
            norm_fold.stopped_at.is_some(),
            "a normalized Jellyfin stop must fold into a stopped session"
        );
    }

    #[test]
    fn normalization_preserves_native_plex_fold_output_exactly() {
        // A recognized Plex event normalizes to its own identical event_type,
        // and an unrecognized one is passed through untouched — so folding the
        // normalized Plex stream is byte-identical to folding the raw one
        // (native Plex behavior preserved; reconstruct.rs untouched).
        let t0 = t(21, 0);
        let raw = vec![
            {
                let mut e = raw_event(1, t0, "media.play", "plex_webhook", Some(0));
                e.raw = serde_json::json!({ "duration": 200_000 });
                e
            },
            raw_event(
                2,
                t0 + chrono::Duration::seconds(30),
                "media.rate",
                "plex_webhook",
                Some(30_000),
            ),
            raw_event(
                3,
                t0 + chrono::Duration::seconds(60),
                "media.stop",
                "plex_webhook",
                Some(60_000),
            ),
        ];
        let normalized = normalize_to_fold_vocab(&raw);
        assert_eq!(
            reconstruct::fold_events(&raw),
            reconstruct::fold_events(&normalized),
            "normalizing a Plex stream must not change the fold's output"
        );
    }

    #[test]
    fn interpret_from_events_empty_is_none() {
        assert!(interpret_from_events(&[], 0).is_none());
    }

    #[test]
    fn interpret_from_events_still_playing_session_is_none() {
        let events = vec![raw_event(
            1,
            t(20, 0),
            "media.play",
            "plex_webhook",
            Some(0),
        )];
        assert!(
            interpret_from_events(&events, 0).is_none(),
            "a session with no stop event has nothing to interpret yet"
        );
    }

    #[test]
    fn longest_single_pause_ms_finds_the_max_span_not_the_total() {
        let t0 = t(19, 0);
        let events = vec![
            raw_event(1, t0, "media.play", "plex_webhook", Some(0)),
            raw_event(
                2,
                t0 + chrono::Duration::minutes(5),
                "media.pause",
                "plex_webhook",
                Some(300_000),
            ),
            raw_event(
                3,
                t0 + chrono::Duration::minutes(10),
                "media.resume",
                "plex_webhook",
                Some(300_000),
            ),
            raw_event(
                4,
                t0 + chrono::Duration::minutes(15),
                "media.pause",
                "plex_webhook",
                Some(600_000),
            ),
            raw_event(
                5,
                t0 + chrono::Duration::minutes(45),
                "media.resume",
                "plex_webhook",
                Some(600_000),
            ),
            raw_event(
                6,
                t0 + chrono::Duration::minutes(50),
                "media.stop",
                "plex_webhook",
                Some(900_000),
            ),
        ];
        // Two pauses: 5min and 30min. Longest must be 30min, not the 35min
        // total (proves it's a max, not a sum, unlike Fold::paused_ms).
        assert_eq!(longest_single_pause_ms(&events), 30 * 60 * 1000);
    }

    // --- read-only / no-playback-mutation guarantee (negative test) -----------

    /// Source-scans THIS FILE's non-test code for any pattern that would
    /// indicate a playback-control call (the real control surface lives in
    /// `crate::plex_control::cast::CastController` — `play`/`pause` methods
    /// against a live server, an HTTP client, etc.). This module must never
    /// reference any of them: it is read-only telemetry interpretation,
    /// never a mutator of playback. A future accidental import of
    /// `plex_control` or a network client into this file should fail this
    /// test.
    #[test]
    fn no_playback_mutation_calls() {
        let source = include_str!("interpret.rs");
        let non_test_source = source.split("#[cfg(test)]").next().unwrap_or(source);

        const FORBIDDEN: &[&str] = &[
            "plex_control",
            "CastController",
            "PlexControlClient",
            "GoogleCastController",
            "reqwest",
            "http_client",
        ];
        for pattern in FORBIDDEN {
            assert!(
                !non_test_source.contains(pattern),
                "interpret.rs's non-test code must never reference `{pattern}` — telemetry \
                 interpretation is read-only and must not touch the live-server control surface \
                 or any HTTP client"
            );
        }
    }
}
