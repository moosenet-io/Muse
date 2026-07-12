//! "On now" resolution (MUSE-29) — the join-mid-stream math. Pure functions
//! over an already-fetched slice of [`ChannelProgram`] rows (the caller
//! fetches those via `repo::channel::list_programs_in_window`); no I/O here,
//! so this is fully unit-testable without a live DB.
//!
//! By the MUSE-28 scheduler's own invariant (see `tuner::scheduler`'s module
//! doc), a channel's grid is gap-free and non-overlapping — at most one
//! program covers any given instant. [`resolve_on_now`] doesn't *assume*
//! that invariant holds (a channel that fell behind, or a fixture in a
//! test, may violate it), it just resolves deterministically when it
//! doesn't: ties break toward the latest `start_at`.

use chrono::{DateTime, Utc};

use crate::models::channel::ChannelProgram;

/// What's airing "now" on a linear channel, plus the join-mid-stream seek
/// offset and the ordered rest of the grid.
///
/// No `PartialEq`/`Eq` derive: [`ChannelProgram`] itself doesn't derive
/// them (it's a `FromRow` model with `DateTime<Utc>` fields elsewhere in
/// the crate, compared field-by-field rather than structurally), so tests
/// assert on individual fields (`.current.id`, `.seek_ms`, ...) instead of
/// whole-struct equality.
#[derive(Debug, Clone)]
pub struct OnNow {
    /// The program covering `now`.
    pub current: ChannelProgram,
    /// How far into `current` a viewer tuning in at `now` should seek,
    /// in milliseconds — `now - current.start_at`, clamped to
    /// `[0, current.duration_ms]` so a clock skew or an off-by-a-tick
    /// boundary never produces a negative or out-of-range seek.
    pub seek_ms: i64,
    /// Every program starting after `current`, ordered by `start_at`
    /// ascending (now/next/later, minus "now").
    pub upcoming: Vec<ChannelProgram>,
}

/// Resolve what's on now (plus the join offset and upcoming order) from an
/// unordered slice of `channel_programs` rows. Returns `None` when no
/// program in `programs` covers `now` (an empty/exhausted grid, or a
/// channel that has fallen behind and not yet been topped off — the
/// caller's job is to run the MUSE-28 scheduler first, then re-check).
///
/// `programs` need not be pre-sorted or pre-filtered to a window; this
/// function does both. Rows with `end_at <= start_at` (which the scheduler
/// itself never produces, but a fixture might) are treated as not covering
/// any instant and are simply never selected as "current" — they may still
/// appear in `upcoming` if their `start_at` is in the future, since a
/// zero/negative-length row starting later is still meaningful ordering
/// information for the caller.
pub fn resolve_on_now(programs: &[ChannelProgram], now: DateTime<Utc>) -> Option<OnNow> {
    let current = programs
        .iter()
        .filter(|p| p.start_at <= now && p.end_at > now)
        // Tie-break toward the latest start_at, matching
        // `repo::channel::current_program`'s `ORDER BY start_at DESC LIMIT 1`
        // so the pure resolver and the SQL-side helper agree.
        .max_by_key(|p| p.start_at)?
        .clone();

    let seek_ms = (now - current.start_at)
        .num_milliseconds()
        .clamp(0, current.duration_ms.max(0));

    let mut upcoming: Vec<ChannelProgram> = programs
        .iter()
        .filter(|p| p.start_at > current.start_at)
        .cloned()
        .collect();
    upcoming.sort_by_key(|p| p.start_at);

    Some(OnNow {
        current,
        seek_ms,
        upcoming,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::channel::ChannelProgramItemType;
    use chrono::Duration;

    fn program(id: i64, start_offset_min: i64, duration_min: i64) -> ChannelProgram {
        let base = DateTime::parse_from_rfc3339("2026-07-12T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let start_at = base + Duration::minutes(start_offset_min);
        let end_at = start_at + Duration::minutes(duration_min);
        ChannelProgram {
            id,
            channel_id: 1,
            item_type: ChannelProgramItemType::Episode,
            media_item_id: None,
            episode_id: Some(id),
            interstitial_id: None,
            title: format!("Program {id}"),
            subtitle: None,
            description: None,
            artwork_url: None,
            start_at,
            end_at,
            duration_ms: duration_min * 60_000,
            rationale: None,
            play_event_id: None,
            created_at: base,
        }
    }

    #[test]
    fn tune_in_mid_program_computes_correct_seek_offset() {
        // program 1: [0, 30) min, program 2: [30, 60) min.
        let programs = vec![program(1, 0, 30), program(2, 30, 30)];
        let base = programs[0].start_at;
        let now = base + Duration::minutes(17); // 17 min into program 1

        let on_now = resolve_on_now(&programs, now).expect("a program covers `now`");
        assert_eq!(on_now.current.id, 1);
        assert_eq!(on_now.seek_ms, 17 * 60_000);
        assert_eq!(on_now.upcoming.iter().map(|p| p.id).collect::<Vec<_>>(), vec![2]);
    }

    #[test]
    fn tune_in_at_exact_start_seeks_to_zero() {
        let programs = vec![program(1, 0, 30)];
        let now = programs[0].start_at;

        let on_now = resolve_on_now(&programs, now).unwrap();
        assert_eq!(on_now.seek_ms, 0);
    }

    #[test]
    fn tune_in_one_tick_before_end_seeks_to_almost_full_duration() {
        let programs = vec![program(1, 0, 30)];
        // end_at is exclusive; one millisecond before it must still resolve
        // to program 1, not "nothing playing".
        let now = programs[0].end_at - Duration::milliseconds(1);

        let on_now = resolve_on_now(&programs, now).unwrap();
        assert_eq!(on_now.current.id, 1);
        assert_eq!(on_now.seek_ms, 30 * 60_000 - 1);
    }

    #[test]
    fn tune_in_with_no_covering_program_returns_none() {
        let programs = vec![program(1, 0, 30), program(2, 60, 30)];
        // a 30-minute gap between the two programs (not a real scheduler
        // output, but the resolver must not panic or pick a wrong program).
        let now = programs[0].end_at + Duration::minutes(5);

        assert!(resolve_on_now(&programs, now).is_none());
    }

    #[test]
    fn tune_in_on_empty_grid_returns_none() {
        assert!(resolve_on_now(&[], Utc::now()).is_none());
    }

    #[test]
    fn upcoming_is_sorted_and_excludes_past_and_current() {
        let mut programs = vec![program(1, 0, 30), program(3, 90, 30), program(2, 60, 30)];
        // Deliberately out of order in the input slice.
        programs.swap(1, 2);
        let now = programs[0].start_at + Duration::minutes(5);

        let on_now = resolve_on_now(&programs, now).unwrap();
        assert_eq!(on_now.current.id, 1);
        assert_eq!(
            on_now.upcoming.iter().map(|p| p.id).collect::<Vec<_>>(),
            vec![2, 3],
            "upcoming must be start_at-ascending regardless of input order"
        );
    }

    #[test]
    fn overlapping_rows_break_ties_toward_the_latest_start_at() {
        // Not a real scheduler output (the grid is supposed to be
        // non-overlapping), but the resolver must still be deterministic
        // rather than panicking or picking arbitrarily.
        let programs = vec![program(1, 0, 60), program(2, 10, 60)];
        let now = programs[0].start_at + Duration::minutes(20);

        let on_now = resolve_on_now(&programs, now).unwrap();
        assert_eq!(on_now.current.id, 2, "later start_at wins the tie, matching current_program's SQL ORDER BY");
    }
}
