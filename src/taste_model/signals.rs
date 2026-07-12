//! Signal weighting/decay math + the derivation of `taste_signals` rows from
//! `watch_stats` + `ratings` + `watchlist` (spec §3.4, MUSE-10).
//!
//! See the module-level doc on `crate::taste_model` for the overall design.
//! This file holds: (1) the weight constants, (2) [`recency_weight`] — a
//! pure, DB-free function so the decay math is unit-testable in isolation,
//! and (3) [`derive_signals_for_account`]/[`replace_derived_signals`] — the
//! extraction + idempotent-replace pipeline.

use chrono::{DateTime, Utc};
use sqlx::PgPool;

use crate::error::MuseResult;
use crate::models::taste::{NewTasteSignal, TasteSignal};
use crate::models::watch_stats::WatchStats;
use crate::repo;

// --- signal_type constants -------------------------------------------------

pub const SIGNAL_FINISHED: &str = "finished";
pub const SIGNAL_ABANDONED: &str = "abandoned";
pub const SIGNAL_REWATCHED: &str = "rewatched";
pub const SIGNAL_RATED: &str = "rated";
pub const SIGNAL_WATCHLISTED: &str = "watchlisted";

/// Every signal type this module derives automatically from
/// `watch_stats`/`ratings`/`watchlist`. [`replace_derived_signals`] deletes
/// exactly these types (and only these) before re-inserting a fresh set —
/// a `curation_note` signal (spec §3.4: "free-text curation") is
/// human-authored and outside this list, so a recompute never touches it.
pub const DERIVED_SIGNAL_TYPES: [&str; 5] = [
    SIGNAL_FINISHED,
    SIGNAL_ABANDONED,
    SIGNAL_REWATCHED,
    SIGNAL_RATED,
    SIGNAL_WATCHLISTED,
];

// --- weight constants -------------------------------------------------------

/// Base weight for a title with at least one finish. `+`.
pub const WEIGHT_FINISH: f32 = 1.0;

/// Weight *per rewatch beyond the first finish* (spec: "rewatch = ++",
/// "VERY strong +" per the `watch_stats.rewatch_count` doc comment) — a
/// title rewatched 3 times contributes `3 * WEIGHT_REWATCH_PER` on top of
/// [`WEIGHT_FINISH`], so rewatch strength scales with how many times the
/// account has gone back to it, not a flat bonus.
pub const WEIGHT_REWATCH_PER: f32 = 2.5;

/// Base weight for a title ever abandoned early without a later finish
/// (spec: "abandon = -"). Negative.
pub const WEIGHT_ABANDON: f32 = -1.5;

/// Midpoint of Plex's 0-10 explicit rating scale — a rating exactly at the
/// midpoint contributes zero weight (neither a positive nor negative
/// signal); above/below it scales toward [`RATING_WEIGHT_SCALE`].
pub const RATING_MIDPOINT: f32 = 5.0;

/// Maximum magnitude an explicit rating can contribute once scaled (a
/// perfect 10/10 -> `+RATING_WEIGHT_SCALE`; a 0/10 -> `-RATING_WEIGHT_SCALE`).
pub const RATING_WEIGHT_SCALE: f32 = 2.0;

/// Mild positive weight for adding a title to the watchlist — intent, not
/// yet action (spec: "watchlist-add = mild +").
pub const WEIGHT_WATCHLIST_ADD: f32 = 0.3;

/// Additional weight once a watchlisted title is later actually watched
/// (spec §3.3 `watchlist.fulfilled`: "intent->action signal") — added on
/// top of [`WEIGHT_WATCHLIST_ADD`], never in place of it.
pub const WEIGHT_WATCHLIST_FULFILLED_BONUS: f32 = 0.3;

/// Default recency half-life, in days, for [`recency_weight`] (~6 months).
pub const DEFAULT_HALF_LIFE_DAYS: f64 = 180.0;

/// Exponential recency decay: a signal observed `now` has full weight
/// (`1.0`); one observed `half_life_days` ago has half weight; one observed
/// `2 * half_life_days` ago has a quarter; and so on. Never reaches exactly
/// zero — an old signal is outweighed by recent ones, not erased.
///
/// A signal observed in the *future* relative to `now` (clock skew, a
/// backfilled `observed_at` newer than the recompute's own clock read) is
/// clamped to zero days-since (full weight `1.0`) rather than producing a
/// decay factor greater than 1.0, which would over-weight it relative to a
/// signal observed exactly now.
pub fn recency_weight(observed_at: DateTime<Utc>, now: DateTime<Utc>, half_life_days: f64) -> f64 {
    if half_life_days <= 0.0 {
        return 1.0;
    }
    let days_since = (now - observed_at).num_seconds() as f64 / 86_400.0;
    let days_since = days_since.max(0.0);
    0.5_f64.powf(days_since / half_life_days)
}

/// Scale an explicit 0-10 rating around [`RATING_MIDPOINT`] into a weight in
/// `[-RATING_WEIGHT_SCALE, +RATING_WEIGHT_SCALE]`.
pub fn rating_weight(rating: f32) -> f32 {
    ((rating - RATING_MIDPOINT) / RATING_MIDPOINT * RATING_WEIGHT_SCALE)
        .clamp(-RATING_WEIGHT_SCALE, RATING_WEIGHT_SCALE)
}

/// Derive the automatic `taste_signals` this account's current
/// `watch_stats`/`ratings`/`watchlist` state implies, WITHOUT writing
/// anything — a pure(-ish; it reads the DB but never mutates it) function so
/// [`replace_derived_signals`]'s "delete then reinsert" split is testable
/// independently of the delete step.
///
/// One row per contributing fact per title: a finished, rewatched AND rated
/// title produces up to three separate signal rows (`finished`,
/// `rewatched`, `rated`) rather than one blended row — keeping each atom
/// separately auditable, per the spec's "these are the auditable atoms"
/// framing.
pub async fn derive_signals_for_account(pool: &PgPool, account_id: i64) -> MuseResult<Vec<NewTasteSignal>> {
    let mut signals = Vec::new();

    let stats = repo::watch_stats::list_watch_stats_for_account(pool, account_id).await?;
    for s in &stats {
        signals.extend(signals_from_watch_stats(account_id, s));
    }

    let ratings = repo::watch_stats::list_ratings_for_account(pool, account_id).await?;
    for r in ratings {
        let Some(rating) = r.rating else { continue };
        signals.push(NewTasteSignal {
            account_id,
            media_item_id: Some(r.media_item_id),
            signal_type: SIGNAL_RATED.to_string(),
            weight: rating_weight(rating),
            context_key: None,
            note: None,
        });
    }

    let watchlist = repo::watch_stats::list_watchlist_for_account(pool, account_id).await?;
    for w in watchlist {
        let weight = if w.fulfilled {
            WEIGHT_WATCHLIST_ADD + WEIGHT_WATCHLIST_FULFILLED_BONUS
        } else {
            WEIGHT_WATCHLIST_ADD
        };
        signals.push(NewTasteSignal {
            account_id,
            media_item_id: Some(w.media_item_id),
            signal_type: SIGNAL_WATCHLISTED.to_string(),
            weight,
            context_key: None,
            note: None,
        });
    }

    Ok(signals)
}

fn signals_from_watch_stats(account_id: i64, s: &WatchStats) -> Vec<NewTasteSignal> {
    let mut out = Vec::new();

    if s.finished_count > 0 {
        out.push(NewTasteSignal {
            account_id,
            media_item_id: Some(s.media_item_id),
            signal_type: SIGNAL_FINISHED.to_string(),
            weight: WEIGHT_FINISH,
            context_key: None,
            note: None,
        });
    }

    if s.rewatch_count > 0 {
        out.push(NewTasteSignal {
            account_id,
            media_item_id: Some(s.media_item_id),
            signal_type: SIGNAL_REWATCHED.to_string(),
            weight: WEIGHT_REWATCH_PER * s.rewatch_count as f32,
            context_key: None,
            note: None,
        });
    }

    if s.abandoned {
        out.push(NewTasteSignal {
            account_id,
            media_item_id: Some(s.media_item_id),
            signal_type: SIGNAL_ABANDONED.to_string(),
            weight: WEIGHT_ABANDON,
            context_key: None,
            note: None,
        });
    }

    out
}

/// Idempotent replace: delete every existing *automatically-derived*
/// `taste_signals` row for `account_id` ([`DERIVED_SIGNAL_TYPES`] — never a
/// human `curation_note`), derive a fresh set from current
/// `watch_stats`/`ratings`/`watchlist` ([`derive_signals_for_account`]), and
/// insert it. Re-running with unchanged upstream data produces the same set
/// of rows (different `id`s/`observed_at`s, since they're freshly inserted,
/// but the same `(media_item_id, signal_type, weight)` triples) — the
/// re-derivation is deterministic given the same inputs.
pub async fn replace_derived_signals(pool: &PgPool, account_id: i64) -> MuseResult<Vec<TasteSignal>> {
    let signal_type_refs: Vec<&str> = DERIVED_SIGNAL_TYPES.to_vec();
    repo::taste::delete_signals_by_types(pool, account_id, &signal_type_refs).await?;

    let derived = derive_signals_for_account(pool, account_id).await?;
    let mut inserted = Vec::with_capacity(derived.len());
    for new_signal in &derived {
        inserted.push(repo::taste::record_signal(pool, new_signal).await?);
    }
    Ok(inserted)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    #[test]
    fn recency_weight_is_one_at_zero_days() {
        let now = Utc::now();
        assert!((recency_weight(now, now, DEFAULT_HALF_LIFE_DAYS) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn recency_weight_halves_at_the_half_life() {
        let now = Utc::now();
        let observed = now - Duration::days(180);
        let w = recency_weight(observed, now, 180.0);
        assert!((w - 0.5).abs() < 1e-6, "expected ~0.5, got {w}");
    }

    #[test]
    fn recency_weight_quarters_at_two_half_lives() {
        let now = Utc::now();
        let observed = now - Duration::days(360);
        let w = recency_weight(observed, now, 180.0);
        assert!((w - 0.25).abs() < 1e-6, "expected ~0.25, got {w}");
    }

    #[test]
    fn recency_weight_never_exceeds_one_for_future_timestamps() {
        let now = Utc::now();
        let observed = now + Duration::days(10); // clock skew / late backfill
        let w = recency_weight(observed, now, 180.0);
        assert!((w - 1.0).abs() < 1e-9, "future observation should clamp to full weight, got {w}");
    }

    #[test]
    fn recency_weight_zero_half_life_is_always_full_weight() {
        let now = Utc::now();
        let observed = now - Duration::days(9999);
        assert_eq!(recency_weight(observed, now, 0.0), 1.0);
    }

    #[test]
    fn rating_weight_scales_around_midpoint() {
        assert!((rating_weight(10.0) - RATING_WEIGHT_SCALE).abs() < 1e-6);
        assert!((rating_weight(0.0) + RATING_WEIGHT_SCALE).abs() < 1e-6);
        assert!((rating_weight(5.0)).abs() < 1e-6, "midpoint rating should be ~neutral");
    }

    #[test]
    fn rating_weight_clamps_out_of_range_input() {
        assert_eq!(rating_weight(20.0), RATING_WEIGHT_SCALE);
        assert_eq!(rating_weight(-20.0), -RATING_WEIGHT_SCALE);
    }

    fn stats_row(media_item_id: i64, finished_count: i32, rewatch_count: i32, abandoned: bool) -> WatchStats {
        WatchStats {
            account_id: 1,
            media_item_id,
            play_count: finished_count.max(1),
            finished_count,
            rewatch_count,
            total_watched_ms: 0,
            avg_percent: None,
            last_watched_at: None,
            abandoned,
            first_watched_at: None,
        }
    }

    #[test]
    fn signals_from_watch_stats_emits_finish_and_rewatch_signals() {
        let s = stats_row(42, 3, 2, false);
        let signals = signals_from_watch_stats(1, &s);

        let finished = signals.iter().find(|sig| sig.signal_type == SIGNAL_FINISHED);
        assert!(finished.is_some());
        assert_eq!(finished.unwrap().weight, WEIGHT_FINISH);

        let rewatched = signals.iter().find(|sig| sig.signal_type == SIGNAL_REWATCHED);
        assert!(rewatched.is_some(), "rewatch_count > 0 should emit a rewatched signal");
        assert_eq!(rewatched.unwrap().weight, WEIGHT_REWATCH_PER * 2.0);

        assert!(
            signals.iter().all(|sig| sig.signal_type != SIGNAL_ABANDONED),
            "not abandoned, should not emit an abandoned signal"
        );
    }

    #[test]
    fn signals_from_watch_stats_emits_abandoned_signal_and_no_finish_when_never_finished() {
        let s = stats_row(7, 0, 0, true);
        let signals = signals_from_watch_stats(1, &s);

        assert_eq!(signals.len(), 1, "only the abandoned signal should be emitted");
        assert_eq!(signals[0].signal_type, SIGNAL_ABANDONED);
        assert_eq!(signals[0].weight, WEIGHT_ABANDON);
    }

    #[test]
    fn rewatch_weight_dominates_a_single_abandonment() {
        // A title finished once, abandoned once on a later rewatch attempt,
        // but rewatched 3 times overall — net signal weight should still be
        // strongly positive (rewatch dominates), matching the MUSE-10 test
        // plan's "rewatch dominates" requirement.
        let s = stats_row(99, 1, 3, true);
        let signals = signals_from_watch_stats(1, &s);
        let total: f32 = signals.iter().map(|sig| sig.weight).sum();
        assert!(total > 0.0, "rewatch should dominate a single abandonment, got total {total}");
        assert!(
            total > WEIGHT_FINISH,
            "combined weight should exceed a bare finish, got {total}"
        );
    }
}
