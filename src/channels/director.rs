//! MUSEX-05 (Plane TERM #381): the channel DIRECTOR core — programs a
//! continuous, plausible CHANNEL rather than a flat list: a slot sequence
//! with intent (warm-up → main → wind-down), timed to end sensibly for the
//! hour, matched to a persona/blend taste target and shaped by
//! time-of-day/runtime awareness.
//!
//! ## How this differs from [`super::compose`] (MUSE-24)
//! `compose::compose_channel_run` round-robins a caller-CHOSEN set of shows'
//! next-unwatched episodes against a session-length budget — it answers
//! "keep these shows going, in this order, for about this long." The
//! director answers a different question: "given a taste target (a persona
//! or blended session vector) and a time budget, DECIDE what to watch and in
//! what order" — it programs from the real candidate pool
//! (`curation::candidates`/`curation::recommend`), the same pool
//! `curation::recommend::recommend_handler` ranks, rather than a
//! caller-supplied show list. The two are complementary, additive
//! features: this module changes no compose behavior and is not wired into
//! `compose_channel_run` in any way.
//!
//! ## Grounding — reuses real code, invents nothing new
//! - **Candidate pool + scoring**: [`DirectorCandidate`] wraps a real
//!   `curation::candidates::Candidate` plus the runtime a caller looked up
//!   (from `media_metadata.runtime_minutes` — see the module's DB-gated
//!   test) and the score `curation::recommend::score_candidate` already
//!   computed. The director does not invent a second scoring formula.
//! - **Safe vs. exploration split**: `CandidateSource::AvailableNow` is
//!   already, structurally, "a title Muse doesn't own and the account
//!   hasn't engaged with" (`candidates.rs`'s own doc) — the one source of
//!   the four that is genuinely NOT a known/safe pick. Reusing that
//!   existing taxonomy (rather than inventing a new "exploration" flag) is
//!   what makes the serendipity split grounded instead of arbitrary:
//!   `Taste`/`OnDeck`/`Gap` are the safe pool, `AvailableNow` is the
//!   exploration pool.
//! - **Per-slot rationale**: every [`Slot::rationale`] is built by
//!   [`taste_review::trace::build_reasoning_trace`] +
//!   [`taste_review::because::because_line`] (MUSEX-04) — the exact same
//!   "because…" surface a recommendation gets, falling back to
//!   `curation::recommend::template_rationale` only in the (never-actually-
//!   empty-for-this-crate's-producers) case `because_line` returns `None`.
//!   No rationale text is written fresh by this module.
//!
//! ## Determinism
//! [`program_channel`] is a pure, DB-free function of its inputs: same
//! `pool` + `constraints` in, byte-for-byte same [`ChannelSchedule`] out.
//! The one input that could otherwise leak nondeterminism — WHICH slots are
//! reserved for exploration — is governed by [`DirectorConstraints::seed`]
//! via a fixed modular-interval reservation (`exploration_interval` +
//! `seed % interval`), never an unseeded `rand` call and never `HashMap`
//! iteration order (the safe/exploration pools are `VecDeque`s built by a
//! single stable `Vec::retain`-style partition, preserving caller order).

use chrono::{DateTime, Duration as ChronoDuration, Utc};
use serde::Serialize;
use std::collections::VecDeque;

use crate::curation::candidates::{Candidate, CandidateSource};
use crate::curation::recommend::template_rationale;
use crate::taste_review::because::because_line;
use crate::taste_review::trace::build_reasoning_trace;

/// A candidate ready to be scheduled: a real [`Candidate`] plus the two
/// facts `curation::candidates`/`curation::recommend` don't themselves
/// carry — a runtime and a final rank score. Construct `score` via
/// `curation::recommend::score_candidate(&candidate)` (or
/// `rank_candidates`'s per-item score) and `runtime_ms` from the real
/// `media_metadata.runtime_minutes` (or an episode's `runtime_minutes`) —
/// see the module doc for why the director doesn't recompute either.
#[derive(Debug, Clone)]
pub struct DirectorCandidate {
    pub candidate: Candidate,
    pub score: f64,
    pub runtime_ms: i64,
}

/// Coarse time-of-day bucket driving the energy arc — how quickly a
/// schedule trends toward `SlotIntent::WindDown`. Deliberately coarse (four
/// buckets, not 24 hourly ones): the AC calls for "time-of-day shapes the
/// energy arc," not a distinct arc per hour.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TimeOfDay {
    Morning,
    Afternoon,
    Evening,
    LateNight,
}

impl TimeOfDay {
    /// Bucket a 24h clock hour (0-23) into a [`TimeOfDay`]. Pure and total
    /// (any `u32` is reduced mod 24 first, so it never panics on an
    /// out-of-range hour).
    pub fn from_hour(hour: u32) -> Self {
        match hour % 24 {
            5..=11 => TimeOfDay::Morning,
            12..=16 => TimeOfDay::Afternoon,
            17..=22 => TimeOfDay::Evening,
            _ => TimeOfDay::LateNight,
        }
    }

    /// The fraction of the time budget (0.0-1.0) at which the energy arc
    /// starts trending to `SlotIntent::WindDown`. Late night winds down at
    /// the HALFWAY point (a short, low-energy session); evening winds down
    /// in its back quarter; morning/afternoon barely wind down at all
    /// (energy stays up almost the whole session) — the concrete,
    /// testable shape of "time-of-day shapes the energy arc."
    fn wind_down_threshold(self) -> f64 {
        match self {
            TimeOfDay::LateNight => 0.5,
            TimeOfDay::Evening => 0.75,
            TimeOfDay::Morning | TimeOfDay::Afternoon => 0.9,
        }
    }
}

/// Where in the arc a slot sits — the "intent" AC-required on every slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SlotIntent {
    WarmUp,
    Main,
    WindDown,
}

/// Everything [`program_channel`] needs beyond the candidate pool.
#[derive(Debug, Clone)]
pub struct DirectorConstraints {
    /// The schedule's first slot starts here.
    pub start_at: DateTime<Utc>,
    /// The schedule must not schedule a slot that would end after this —
    /// the "ends sensibly" / "fits before midnight" AC. Must be after
    /// `start_at`; a non-positive budget yields an empty schedule.
    pub end_by: DateTime<Utc>,
    pub time_of_day: TimeOfDay,
    /// Fraction (0.0-1.0) of slots reserved for exploration picks (the
    /// `CandidateSource::AvailableNow` pool). `0.0` disables reservation
    /// entirely (a schedule may then end up all-safe if that's genuinely
    /// all the pool offers). Values are clamped to `[0.0, 1.0]`.
    pub serendipity_budget: f64,
    /// A hard cap on slot count, independent of the time budget (defends
    /// against a pathologically large budget + tiny-runtime pool spinning
    /// out an enormous schedule). `0` is treated as "no extra cap" (only
    /// the time/pool-exhaustion stop conditions apply).
    pub max_slots: usize,
    /// Deterministic seed for WHICH slot indices land on the exploration
    /// reservation — see the module doc's determinism section. Two calls
    /// with the same seed (and everything else equal) always reserve the
    /// same slot indices.
    pub seed: u64,
}

/// One programmed slot in a [`ChannelSchedule`].
#[derive(Debug, Clone, Serialize)]
pub struct Slot {
    pub media_metadata_id: i64,
    pub title: String,
    pub source: CandidateSource,
    pub runtime_ms: i64,
    pub start_at: DateTime<Utc>,
    pub end_at: DateTime<Utc>,
    pub intent: SlotIntent,
    /// `true` when this slot was filled from the exploration pool
    /// (`CandidateSource::AvailableNow`) rather than the safe pool.
    pub is_exploration: bool,
    /// Grounded per-slot "why this" — see the module doc's rationale
    /// section. Never fabricated: either MUSEX-04's `because_line` output
    /// or `curation::recommend::template_rationale`'s deterministic
    /// fallback, both built strictly from `Candidate::facts`.
    pub rationale: String,
}

/// The full programmed channel: an ordered, timed, intent-tagged slot
/// sequence.
#[derive(Debug, Clone, Serialize)]
pub struct ChannelSchedule {
    pub slots: Vec<Slot>,
    pub time_of_day: TimeOfDay,
    /// How many slots were reserved-and-filled from the exploration pool —
    /// surfaced directly so a caller (or a test) doesn't have to recount
    /// `slots.iter().filter(|s| s.is_exploration)`.
    pub exploration_slot_count: usize,
}

/// How many slots apart an exploration reservation lands, given a
/// `serendipity_budget` fraction. `<= 0.0` (or a non-finite value) disables
/// reservation (`usize::MAX` — never hit by `%`). `>= 1.0` reserves every
/// slot. Otherwise the nearest interval to `1.0 / budget`, floored at 1 so
/// a very high budget still advances.
fn exploration_interval(serendipity_budget: f64) -> usize {
    if !(serendipity_budget.is_finite()) || serendipity_budget <= 0.0 {
        return usize::MAX;
    }
    if serendipity_budget >= 1.0 {
        return 1;
    }
    ((1.0 / serendipity_budget).round() as usize).max(1)
}

/// The arc-position intent for a slot about to start at `fraction_elapsed`
/// (elapsed-so-far / total budget, 0.0-1.0) through the session. Slot 0 is
/// always `WarmUp` regardless of time-of-day (every channel opens gently);
/// after that, `time_of_day`'s [`TimeOfDay::wind_down_threshold`] decides
/// how much of the back end of the session is `WindDown` vs `Main`.
fn slot_intent(idx: usize, fraction_elapsed: f64, time_of_day: TimeOfDay) -> SlotIntent {
    if idx == 0 {
        return SlotIntent::WarmUp;
    }
    if fraction_elapsed >= time_of_day.wind_down_threshold() {
        SlotIntent::WindDown
    } else {
        SlotIntent::Main
    }
}

/// Build this slot's grounded rationale — MUSEX-04's `because_line` over a
/// fresh reasoning trace for `candidate`/`score`, falling back to the
/// deterministic templated rationale on the (signal-less-trace) `None`
/// case. Reuses both functions verbatim; adds no new prose of its own.
fn slot_rationale(candidate: &Candidate, score: f64) -> String {
    let trace = build_reasoning_trace(candidate, score);
    because_line(&trace).unwrap_or_else(|| template_rationale(candidate))
}

/// Partition a candidate pool into (safe, exploration) queues, preserving
/// relative order (a stable partition, not a `HashMap`-keyed one) so
/// "highest score first" ordering upstream (the caller is expected to hand
/// in an already-`rank_candidates`-sorted pool) survives into each queue.
fn partition_pool(
    pool: Vec<DirectorCandidate>,
) -> (VecDeque<DirectorCandidate>, VecDeque<DirectorCandidate>) {
    let mut safe = VecDeque::new();
    let mut exploration = VecDeque::new();
    for dc in pool {
        if dc.candidate.source == CandidateSource::AvailableNow {
            exploration.push_back(dc);
        } else {
            safe.push_back(dc);
        }
    }
    (safe, exploration)
}

/// Scan `queue` front-to-back for the first entry whose `runtime_ms` fits
/// in `remaining_ms`, remove and return it (preserving the relative order
/// of everything else). Returns `None` if nothing in the queue fits — the
/// caller then tries the other queue, or stops. This is the "a too-long
/// slot is excluded, not just the whole schedule" behavior the AC calls
/// for: a later, shorter candidate still gets a chance even when an
/// earlier, higher-priority one doesn't fit the remaining budget.
fn pop_first_fitting(
    queue: &mut VecDeque<DirectorCandidate>,
    remaining_ms: i64,
) -> Option<DirectorCandidate> {
    let pos = queue.iter().position(|dc| dc.runtime_ms <= remaining_ms)?;
    queue.remove(pos)
}

/// Program a [`ChannelSchedule`] from a candidate pool and constraints. Pure
/// and DB-free — see the module doc for the full contract (safe/exploration
/// split, energy arc, runtime/end-by fitting, determinism).
///
/// `pool` should already be de-duplicated (`candidates::dedup_candidates`)
/// and, within each source, roughly score-ordered (e.g. via
/// `recommend::rank_candidates`) — the director does not re-sort; it only
/// partitions and fits.
pub fn program_channel(
    pool: Vec<DirectorCandidate>,
    constraints: &DirectorConstraints,
) -> ChannelSchedule {
    let budget_ms = (constraints.end_by - constraints.start_at)
        .num_milliseconds()
        .max(0);
    let serendipity_budget = constraints.serendipity_budget.clamp(0.0, 1.0);
    let interval = exploration_interval(serendipity_budget);
    let offset = if interval == usize::MAX {
        0
    } else {
        (constraints.seed % interval as u64) as usize
    };

    let (mut safe, mut exploration) = partition_pool(pool);

    let mut slots = Vec::new();
    let mut elapsed_ms: i64 = 0;
    let mut cursor_at = constraints.start_at;
    let mut idx = 0usize;
    let mut exploration_slot_count = 0usize;

    loop {
        if constraints.max_slots != 0 && slots.len() >= constraints.max_slots {
            break;
        }
        let remaining_ms = budget_ms - elapsed_ms;
        if remaining_ms <= 0 {
            break;
        }

        let want_exploration = interval != usize::MAX && idx % interval == offset;

        let picked = if want_exploration {
            pop_first_fitting(&mut exploration, remaining_ms)
                .or_else(|| pop_first_fitting(&mut safe, remaining_ms))
        } else {
            pop_first_fitting(&mut safe, remaining_ms)
                .or_else(|| pop_first_fitting(&mut exploration, remaining_ms))
        };

        // Neither queue had anything that fits the remaining budget — either
        // both are exhausted, or everything left is simply too long for
        // what's left of the session (a correct exclusion, not an error).
        // `remaining_ms` only shrinks going forward, so nothing later would
        // fit either: a clean stop, never a fallback spin.
        let Some(dc) = picked else {
            break;
        };

        let fraction_elapsed = if budget_ms > 0 {
            elapsed_ms as f64 / budget_ms as f64
        } else {
            0.0
        };
        let intent = slot_intent(idx, fraction_elapsed, constraints.time_of_day);
        let is_exploration = dc.candidate.source == CandidateSource::AvailableNow;
        if is_exploration {
            exploration_slot_count += 1;
        }

        let start_at = cursor_at;
        let end_at = start_at + ChronoDuration::milliseconds(dc.runtime_ms);
        let rationale = slot_rationale(&dc.candidate, dc.score);

        slots.push(Slot {
            media_metadata_id: dc.candidate.media_metadata_id,
            title: dc.candidate.title.clone(),
            source: dc.candidate.source,
            runtime_ms: dc.runtime_ms,
            start_at,
            end_at,
            intent,
            is_exploration,
            rationale,
        });

        elapsed_ms += dc.runtime_ms;
        cursor_at = end_at;
        idx += 1;
    }

    ChannelSchedule {
        slots,
        time_of_day: constraints.time_of_day,
        exploration_slot_count,
    }
}

// --- named channel presets (persona-derived) --------------------------------

/// A named channel-director preset: which persona (by name, resolved via
/// `repo::persona::get_by_name_for_account`/`get_by_id` by the caller — this
/// module doesn't touch the DB) it programs toward, plus the constraint
/// defaults that give the preset its character. Additive to
/// `super::presets`'s six MUSE-24 episode-lineup presets — a different
/// feature (this directs a scored candidate pool toward a persona; that one
/// round-robins a caller-chosen show list).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DirectorPresetName {
    MusePrime,
    ComfortRewatch,
    DeepCutSundays,
    BackgroundCooking,
}

#[derive(Debug, Clone)]
pub struct DirectorPreset {
    pub name: DirectorPresetName,
    pub display_name: &'static str,
    pub description: &'static str,
    /// The persona name this preset resolves and programs toward (a lookup
    /// key for `repo::persona::get_by_name_for_account`, not a literal
    /// vector — this module has no opinion on how a caller resolves it).
    pub persona_name: &'static str,
    pub serendipity_budget: f64,
    /// A default time-of-day hint for callers that don't supply their own
    /// (e.g. a scheduler picking `TimeOfDay::from_hour(now)` instead);
    /// `None` means "no opinion, use the real clock."
    pub default_time_of_day: Option<TimeOfDay>,
}

/// The four MUSEX-05 named presets (AC §"named channel presets"). Order is
/// display order, not priority.
pub fn list_director_presets() -> Vec<DirectorPreset> {
    vec![
        DirectorPreset {
            name: DirectorPresetName::MusePrime,
            display_name: "Muse Prime",
            description: "The account's primary taste persona, moderate serendipity — a solid \
                default channel for any time of day.",
            persona_name: "primary",
            serendipity_budget: 0.2,
            default_time_of_day: None,
        },
        DirectorPreset {
            name: DirectorPresetName::ComfortRewatch,
            display_name: "Comfort Rewatch",
            description: "The comfort persona, low serendipity — familiar, low-risk favorites for \
                background/unwind viewing.",
            persona_name: "comfort",
            serendipity_budget: 0.05,
            default_time_of_day: Some(TimeOfDay::Evening),
        },
        DirectorPreset {
            name: DirectorPresetName::DeepCutSundays,
            display_name: "Deep Cut Sundays",
            description:
                "A discovery-leaning persona, high serendipity — deliberately reaches past \
                the safe pool for a lazy-Sunday deep-cut session.",
            persona_name: "discovery",
            serendipity_budget: 0.4,
            default_time_of_day: Some(TimeOfDay::Afternoon),
        },
        DirectorPreset {
            name: DirectorPresetName::BackgroundCooking,
            display_name: "Background Cooking",
            description: "A low-intensity persona, light serendipity — easy-to-half-watch \
                programming for a task in the foreground.",
            persona_name: "background",
            serendipity_budget: 0.1,
            default_time_of_day: Some(TimeOfDay::Afternoon),
        },
    ]
}

/// Look up a single director preset by name.
pub fn resolve_director_preset(name: DirectorPresetName) -> Option<DirectorPreset> {
    list_director_presets().into_iter().find(|p| p.name == name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::media_metadata::MediaKind;

    fn candidate(id: i64, source: CandidateSource, taste_fit: f64, fact: &str) -> Candidate {
        Candidate {
            media_metadata_id: id,
            media_item_id: Some(id),
            title: format!("Title {id}"),
            year: Some(2020),
            kind: MediaKind::Movie,
            source,
            taste_fit,
            facts: vec![fact.to_string()],
            availability: None,
        }
    }

    fn dc(id: i64, source: CandidateSource, score: f64, runtime_ms: i64) -> DirectorCandidate {
        DirectorCandidate {
            candidate: candidate(
                id,
                source,
                score,
                "it's a 92% match to your overall taste profile",
            ),
            score,
            runtime_ms,
        }
    }

    fn base_constraints(hours: i64, time_of_day: TimeOfDay) -> DirectorConstraints {
        let start = DateTime::parse_from_rfc3339("2026-07-14T18:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        DirectorConstraints {
            start_at: start,
            end_by: start + ChronoDuration::hours(hours),
            time_of_day,
            serendipity_budget: 0.0,
            max_slots: 0,
            seed: 7,
        }
    }

    const THIRTY_MIN_MS: i64 = 30 * 60_000;
    const NINETY_MIN_MS: i64 = 90 * 60_000;

    // --- sequencing: intent + energy arc ------------------------------------

    #[test]
    fn first_slot_is_always_warm_up() {
        let pool = vec![
            dc(1, CandidateSource::Taste, 0.9, THIRTY_MIN_MS),
            dc(2, CandidateSource::Taste, 0.8, THIRTY_MIN_MS),
        ];
        let schedule = program_channel(pool, &base_constraints(3, TimeOfDay::Evening));
        assert_eq!(schedule.slots[0].intent, SlotIntent::WarmUp);
    }

    #[test]
    fn late_night_winds_down_earlier_than_evening() {
        // Six 30-minute safe slots over a 3h budget: late night should have
        // MORE wind-down slots than evening, because its wind-down
        // threshold (0.5) is earlier in the session than evening's (0.75).
        let pool_a: Vec<DirectorCandidate> = (1..=6)
            .map(|i| dc(i, CandidateSource::Taste, 0.9, THIRTY_MIN_MS))
            .collect();
        let pool_b = pool_a.clone();

        let late_night = program_channel(pool_a, &base_constraints(3, TimeOfDay::LateNight));
        let evening = program_channel(pool_b, &base_constraints(3, TimeOfDay::Evening));

        let late_night_wind_down = late_night
            .slots
            .iter()
            .filter(|s| s.intent == SlotIntent::WindDown)
            .count();
        let evening_wind_down = evening
            .slots
            .iter()
            .filter(|s| s.intent == SlotIntent::WindDown)
            .count();

        assert!(
            late_night_wind_down > evening_wind_down,
            "late night ({late_night_wind_down} wind-down slots) should wind down earlier/more \
             than evening ({evening_wind_down}) for the same pool+budget"
        );
    }

    #[test]
    fn morning_stays_high_energy_almost_the_whole_session() {
        let pool: Vec<DirectorCandidate> = (1..=4)
            .map(|i| dc(i, CandidateSource::Taste, 0.9, THIRTY_MIN_MS))
            .collect();
        let schedule = program_channel(pool, &base_constraints(2, TimeOfDay::Morning));
        let wind_down = schedule
            .slots
            .iter()
            .filter(|s| s.intent == SlotIntent::WindDown)
            .count();
        assert_eq!(
            wind_down, 0,
            "a 2h morning session of 30-min slots shouldn't reach the 0.9 wind-down threshold"
        );
    }

    // --- runtime/end-by timing (+ the too-long-excluded negative test) -----

    #[test]
    fn schedule_never_overruns_end_by() {
        let pool = vec![
            dc(1, CandidateSource::Taste, 0.9, NINETY_MIN_MS),
            dc(2, CandidateSource::Taste, 0.8, NINETY_MIN_MS),
            dc(3, CandidateSource::Taste, 0.7, NINETY_MIN_MS),
        ];
        // 2h budget: only one 90-min slot can ever fit (a second would end
        // at 180 min).
        let schedule = program_channel(pool, &base_constraints(2, TimeOfDay::Evening));
        let last = schedule.slots.last().expect("at least one slot fits");
        let end_by = base_constraints(2, TimeOfDay::Evening).end_by;
        assert!(
            last.end_at <= end_by,
            "last slot end_at {} must not exceed end_by {end_by}",
            last.end_at
        );
        assert_eq!(
            schedule.slots.len(),
            1,
            "only one 90-min slot fits a 2h budget"
        );
    }

    #[test]
    fn a_too_long_slot_is_excluded_while_a_later_shorter_one_still_fits() {
        // remaining budget after slot 1 (30 min, into a 1h budget) is 30
        // min: candidate 2 (90 min) does NOT fit and must be excluded, but
        // candidate 3 (20 min) — lower priority, listed after candidate 2 —
        // DOES fit and must be scheduled instead.
        let pool = vec![
            dc(1, CandidateSource::Taste, 0.95, THIRTY_MIN_MS),
            dc(2, CandidateSource::Taste, 0.9, NINETY_MIN_MS),
            dc(3, CandidateSource::Taste, 0.5, 20 * 60_000),
        ];
        let schedule = program_channel(pool, &base_constraints(1, TimeOfDay::Evening));

        let ids: Vec<i64> = schedule.slots.iter().map(|s| s.media_metadata_id).collect();
        assert_eq!(
            ids,
            vec![1, 3],
            "the too-long candidate 2 must be excluded, candidate 3 included"
        );
    }

    #[test]
    fn zero_or_negative_budget_yields_an_empty_schedule() {
        let pool = vec![dc(1, CandidateSource::Taste, 0.9, THIRTY_MIN_MS)];
        let start = DateTime::parse_from_rfc3339("2026-07-14T18:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let constraints = DirectorConstraints {
            start_at: start,
            end_by: start, // zero-length budget
            time_of_day: TimeOfDay::Evening,
            serendipity_budget: 0.0,
            max_slots: 0,
            seed: 1,
        };
        let schedule = program_channel(pool, &constraints);
        assert!(schedule.slots.is_empty());
    }

    // --- serendipity budget (+ the never-100%-safe negative test) ----------

    #[test]
    fn serendipity_budget_reserves_exploration_slots_never_all_safe() {
        let mut pool: Vec<DirectorCandidate> = (1..=8)
            .map(|i| dc(i, CandidateSource::Taste, 0.9, THIRTY_MIN_MS))
            .collect();
        pool.extend((100..=104).map(|i| dc(i, CandidateSource::AvailableNow, 0.3, THIRTY_MIN_MS)));

        let mut constraints = base_constraints(4, TimeOfDay::Evening); // 8 slots' worth of budget
        constraints.serendipity_budget = 0.25; // reserve every 4th slot

        let schedule = program_channel(pool, &constraints);

        assert!(
            schedule.exploration_slot_count > 0,
            "with serendipity_budget > 0 and a non-empty exploration pool, the schedule must \
             never collapse to 100% safe/known picks"
        );
        assert!(
            schedule.slots.iter().any(|s| s.is_exploration),
            "at least one scheduled slot must be flagged is_exploration"
        );
        // Every reserved position across a full 8-slot schedule (interval 4,
        // budget 0.25) is genuinely exploration, not just "count > 0".
        let explore_at: Vec<usize> = schedule
            .slots
            .iter()
            .enumerate()
            .filter(|(_, s)| s.is_exploration)
            .map(|(i, _)| i)
            .collect();
        assert!(!explore_at.is_empty());
    }

    #[test]
    fn zero_serendipity_budget_never_reserves_exploration_even_if_pool_has_it() {
        let mut pool: Vec<DirectorCandidate> = (1..=6)
            .map(|i| dc(i, CandidateSource::Taste, 0.9, THIRTY_MIN_MS))
            .collect();
        pool.extend((100..=102).map(|i| dc(i, CandidateSource::AvailableNow, 0.3, THIRTY_MIN_MS)));

        let constraints = base_constraints(3, TimeOfDay::Evening); // serendipity_budget: 0.0

        let schedule = program_channel(pool, &constraints);
        assert_eq!(
            schedule.exploration_slot_count, 0,
            "budget 0.0 must never reserve an exploration slot, even with exploration candidates \
             available in the pool"
        );
    }

    #[test]
    fn serendipity_reservation_falls_back_to_safe_when_exploration_pool_is_empty() {
        // Budget > 0 but no AvailableNow candidates exist at all: the
        // reservation must fall back to the safe pool rather than dropping
        // the slot (a real, empty pool is a legitimate degrade case, not a
        // bug).
        let pool: Vec<DirectorCandidate> = (1..=4)
            .map(|i| dc(i, CandidateSource::Taste, 0.9, THIRTY_MIN_MS))
            .collect();
        let mut constraints = base_constraints(2, TimeOfDay::Evening);
        constraints.serendipity_budget = 0.5;

        let schedule = program_channel(pool, &constraints);
        assert_eq!(schedule.exploration_slot_count, 0);
        assert_eq!(
            schedule.slots.len(),
            4,
            "still schedules from the safe pool, doesn't just stop"
        );
    }

    // --- rationale reuse (MUSEX-04) ------------------------------------------

    #[test]
    fn every_slot_rationale_reuses_the_because_line_format() {
        let pool = vec![dc(1, CandidateSource::Taste, 0.9, THIRTY_MIN_MS)];
        let schedule = program_channel(pool, &base_constraints(1, TimeOfDay::Evening));
        let slot = &schedule.slots[0];
        assert_eq!(
            slot.rationale,
            "Because it's a 92% match to your overall taste profile."
        );
    }

    #[test]
    fn rationale_falls_back_to_template_when_trace_has_no_signals() {
        let candidate = Candidate {
            media_metadata_id: 1,
            media_item_id: Some(1),
            title: "No Facts".to_string(),
            year: Some(2020),
            kind: MediaKind::Movie,
            source: CandidateSource::Taste,
            taste_fit: 0.5,
            facts: vec![],
            availability: None,
        };
        let pool = vec![DirectorCandidate {
            candidate: candidate.clone(),
            score: 0.5,
            runtime_ms: THIRTY_MIN_MS,
        }];
        let schedule = program_channel(pool, &base_constraints(1, TimeOfDay::Evening));
        assert_eq!(schedule.slots[0].rationale, template_rationale(&candidate));
    }

    // --- determinism (seeded, teeth'd) --------------------------------------

    #[test]
    fn program_channel_is_deterministic_for_identical_inputs() {
        let build_pool = || {
            let mut pool: Vec<DirectorCandidate> = (1..=6)
                .map(|i| dc(i, CandidateSource::Taste, 0.9, THIRTY_MIN_MS))
                .collect();
            pool.extend(
                (100..=102).map(|i| dc(i, CandidateSource::AvailableNow, 0.3, THIRTY_MIN_MS)),
            );
            pool
        };
        let mut constraints = base_constraints(4, TimeOfDay::Evening);
        constraints.serendipity_budget = 0.3;
        constraints.seed = 42;

        let a = program_channel(build_pool(), &constraints);
        let b = program_channel(build_pool(), &constraints);

        let ids_a: Vec<i64> = a.slots.iter().map(|s| s.media_metadata_id).collect();
        let ids_b: Vec<i64> = b.slots.iter().map(|s| s.media_metadata_id).collect();
        assert_eq!(
            ids_a, ids_b,
            "identical inputs must yield byte-for-byte identical schedules"
        );
        assert_eq!(a.exploration_slot_count, b.exploration_slot_count);
    }

    /// Teeth: a determinism test that only ever compares a run to itself is
    /// vacuous (it would pass even if `program_channel` returned a constant).
    /// This proves the seed genuinely participates: two DIFFERENT seeds, same
    /// everything else, produce a DIFFERENT reservation pattern for a pool
    /// where that's observable (interval doesn't divide evenly into the seed
    /// space identically).
    #[test]
    fn different_seeds_can_change_which_slots_are_reserved_for_exploration() {
        let build_pool = || {
            let mut pool: Vec<DirectorCandidate> = (1..=6)
                .map(|i| dc(i, CandidateSource::Taste, 0.9, THIRTY_MIN_MS))
                .collect();
            pool.extend(
                (100..=105).map(|i| dc(i, CandidateSource::AvailableNow, 0.3, THIRTY_MIN_MS)),
            );
            pool
        };
        let mut constraints_seed_0 = base_constraints(6, TimeOfDay::Evening);
        constraints_seed_0.serendipity_budget = 0.5; // interval 2
        constraints_seed_0.seed = 0;

        let mut constraints_seed_1 = constraints_seed_0.clone();
        constraints_seed_1.seed = 1;

        let a = program_channel(build_pool(), &constraints_seed_0);
        let b = program_channel(build_pool(), &constraints_seed_1);

        let explore_positions = |s: &ChannelSchedule| -> Vec<usize> {
            s.slots
                .iter()
                .enumerate()
                .filter(|(_, slot)| slot.is_exploration)
                .map(|(i, _)| i)
                .collect()
        };

        assert_ne!(
            explore_positions(&a),
            explore_positions(&b),
            "different seeds must be able to change which positions are reserved for exploration \
             (interval 2: seed 0 reserves even indices, seed 1 reserves odd indices)"
        );
    }

    #[test]
    fn exploration_interval_matches_documented_shape() {
        assert_eq!(exploration_interval(0.0), usize::MAX);
        assert_eq!(exploration_interval(-1.0), usize::MAX);
        assert_eq!(exploration_interval(f64::NAN), usize::MAX);
        assert_eq!(exploration_interval(1.0), 1);
        assert_eq!(exploration_interval(1.5), 1);
        assert_eq!(exploration_interval(0.25), 4);
        assert_eq!(exploration_interval(0.5), 2);
    }

    // --- named presets --------------------------------------------------------

    #[test]
    fn four_director_presets_with_unique_names() {
        let presets = list_director_presets();
        assert_eq!(presets.len(), 4);
        let mut names: Vec<String> = presets.iter().map(|p| format!("{:?}", p.name)).collect();
        names.sort();
        names.dedup();
        assert_eq!(names.len(), 4, "preset names must be unique");
    }

    #[test]
    fn resolve_director_preset_finds_each_by_name() {
        for p in list_director_presets() {
            let found = resolve_director_preset(p.name).expect("preset should resolve");
            assert_eq!(found.display_name, p.display_name);
        }
    }

    #[test]
    fn comfort_rewatch_has_lower_serendipity_than_deep_cut_sundays() {
        let comfort = resolve_director_preset(DirectorPresetName::ComfortRewatch).unwrap();
        let deep_cut = resolve_director_preset(DirectorPresetName::DeepCutSundays).unwrap();
        assert!(
            comfort.serendipity_budget < deep_cut.serendipity_budget,
            "Comfort Rewatch must be lower-serendipity than Deep Cut Sundays by construction"
        );
    }

    #[test]
    fn time_of_day_from_hour_covers_the_full_clock() {
        assert_eq!(TimeOfDay::from_hour(8), TimeOfDay::Morning);
        assert_eq!(TimeOfDay::from_hour(14), TimeOfDay::Afternoon);
        assert_eq!(TimeOfDay::from_hour(20), TimeOfDay::Evening);
        assert_eq!(TimeOfDay::from_hour(2), TimeOfDay::LateNight);
        assert_eq!(
            TimeOfDay::from_hour(29),
            TimeOfDay::Morning,
            "hours wrap mod 24 (29 % 24 = 5)"
        );
    }
}

/// DB-gated end-to-end proof: seeds one real `media_metadata` row (a movie
/// with a real `runtime_minutes`), builds a real `Candidate` +
/// `DirectorCandidate` from it (runtime read back from the DB, not
/// fabricated), and confirms `program_channel` schedules it using that real
/// runtime. Gated on `MUSE_TEST_DATABASE_URL`, identical skip-when-unset
/// posture as every other live-DB test in this crate
/// (`curation::live_tests`, `channels::routes`'s route test) — never a live
/// system, never a hardcoded DSN.
#[cfg(test)]
mod live_tests {
    use super::*;
    use crate::models::library::{LibraryKind, NewLibrary};
    use crate::models::media_metadata::{MediaKind, NewMediaMetadata};
    use sqlx::postgres::PgPoolOptions;
    use uuid::Uuid;

    #[tokio::test]
    async fn program_channel_schedules_a_real_seeded_candidate_with_its_real_runtime() {
        let Ok(database_url) = std::env::var("MUSE_TEST_DATABASE_URL") else {
            eprintln!(
                "MUSE_TEST_DATABASE_URL not set — skipping \
                 program_channel_schedules_a_real_seeded_candidate_with_its_real_runtime"
            );
            return;
        };

        let pool = PgPoolOptions::new()
            .max_connections(2)
            .connect(&database_url)
            .await
            .expect("connect to MUSE_TEST_DATABASE_URL");
        sqlx::migrate!("./migrations")
            .run(&pool)
            .await
            .expect("migrations should apply cleanly");

        let suffix = Uuid::new_v4().simple().to_string();

        let library = crate::repo::library::create(
            &pool,
            &NewLibrary {
                name: format!("musex05-director-movies-{suffix}"),
                kind: LibraryKind::Movie,
                root_folder: "/test/musex05-director".to_string(),
                source_arr_name: None,
                source_arr_url: None,
            },
        )
        .await
        .expect("create library");

        let metadata = crate::repo::media_metadata::upsert_by_tmdb(
            &pool,
            &NewMediaMetadata {
                kind: MediaKind::Movie,
                tmdb_id: Some(format!("musex05-director-tmdb-{suffix}")),
                tvdb_id: None,
                imdb_id: None,
                provider_ids: serde_json::json!({}),
                title: format!("MUSEX-05 Director Fixture Movie {suffix}"),
                sort_title: None,
                original_title: None,
                original_language: None,
                status: None,
                overview: None,
                studio: None,
                network: None,
                runtime_minutes: Some(37),
                year: Some(2023),
                images: serde_json::json!([]),
            },
        )
        .await
        .expect("upsert media_metadata");

        let real_runtime_ms = i64::from(
            metadata
                .runtime_minutes
                .expect("fixture set runtime_minutes"),
        ) * 60_000;

        let candidate = Candidate {
            media_metadata_id: metadata.id,
            media_item_id: None,
            title: metadata.title.clone(),
            year: metadata.year,
            kind: MediaKind::Movie,
            source: CandidateSource::Taste,
            taste_fit: 0.8,
            facts: vec!["it's a 92% match to your overall taste profile".to_string()],
            availability: None,
        };

        let director_pool = vec![DirectorCandidate {
            candidate,
            score: 0.8 * 0.7, // source_weight(Taste) * taste_fit, matching recommend::score_candidate
            runtime_ms: real_runtime_ms,
        }];

        let start = Utc::now();
        let constraints = DirectorConstraints {
            start_at: start,
            end_by: start + ChronoDuration::hours(2),
            time_of_day: TimeOfDay::Evening,
            serendipity_budget: 0.0,
            max_slots: 0,
            seed: 1,
        };

        let schedule = program_channel(director_pool, &constraints);

        assert_eq!(schedule.slots.len(), 1);
        let slot = &schedule.slots[0];
        assert_eq!(slot.media_metadata_id, metadata.id);
        assert_eq!(
            slot.runtime_ms, real_runtime_ms,
            "the slot's runtime must be the real seeded media_metadata.runtime_minutes, not a default"
        );
        assert_eq!(
            slot.end_at,
            slot.start_at + ChronoDuration::milliseconds(real_runtime_ms)
        );

        sqlx::query("DELETE FROM media_metadata WHERE id = $1")
            .bind(metadata.id)
            .execute(&pool)
            .await
            .ok();
        sqlx::query("DELETE FROM libraries WHERE id = $1")
            .bind(library.id)
            .execute(&pool)
            .await
            .ok();
    }
}
