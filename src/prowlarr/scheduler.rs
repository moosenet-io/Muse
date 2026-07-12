//! Per-indexer report-pull scheduling decision (MUSE-17, spec S4b-B: "on a
//! per-indexer interval ... never poll faster than this").
//!
//! [`is_due`] is the durable scheduling source of truth: it's driven off
//! `indexers.last_rss_pull_at` (persisted in Postgres, survives a worker
//! restart) rather than the in-process [`super::rate_limit::RateLimiter`]
//! (which resets on restart -- see `repo::indexer::mark_rss_pulled`'s doc
//! comment). The `RateLimiter` inside [`super::client::ProwlarrClient`]
//! still runs underneath as a second, defense-in-depth guard against a
//! scheduling bug ever hammering a tracker; `is_due` is the *primary*
//! decision the worker loop makes every tick.
//!
//! Deliberately a pure function (`now` is a parameter, not read from a live
//! clock) so the interval decision is testable without any async runtime or
//! real time passing -- see the tests below and `worker.rs`'s
//! `#[tokio::test(start_paused = true)]` test, which additionally exercises
//! the real `RateLimiter` via `tokio::time::advance`, reusing the MUSE-16
//! `rate_limit` test pattern for the layer that *does* depend on elapsed
//! time.

use chrono::{DateTime, Duration, TimeZone, Utc};

/// Whether an indexer with `last_pull` (its `last_rss_pull_at`, `None` if
/// never pulled) and `min_interval_secs` (its `polite_min_interval_secs`) is
/// due for a report-pull at `now`. A negative or zero `min_interval_secs` is
/// treated as "always due" rather than panicking or dividing by zero -- a
/// misconfigured interval should degrade to "poll every tick" (still gated
/// by the client's own `RateLimiter`), never crash the scheduler.
pub fn is_due(last_pull: Option<DateTime<Utc>>, min_interval_secs: i32, now: DateTime<Utc>) -> bool {
    let Some(last) = last_pull else {
        return true;
    };
    let min_interval = Duration::seconds(min_interval_secs.max(0) as i64);
    now.signed_duration_since(last) >= min_interval
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(offset_secs: i64) -> DateTime<Utc> {
        Utc.timestamp_opt(offset_secs, 0).unwrap()
    }

    #[test]
    fn never_pulled_is_always_due() {
        assert!(is_due(None, 900, t(0)));
        assert!(is_due(None, 0, t(0)));
    }

    #[test]
    fn not_due_before_the_interval_elapses() {
        let last = t(1000);
        let now = t(1899); // 899s later, interval is 900s
        assert!(!is_due(Some(last), 900, now));
    }

    #[test]
    fn due_exactly_at_the_interval_boundary() {
        let last = t(1000);
        let now = t(1900); // exactly 900s later
        assert!(is_due(Some(last), 900, now));
    }

    #[test]
    fn due_well_after_the_interval() {
        let last = t(1000);
        let now = t(10_000);
        assert!(is_due(Some(last), 900, now));
    }

    #[test]
    fn zero_or_negative_interval_is_always_due() {
        let last = t(1000);
        assert!(is_due(Some(last), 0, t(1000)));
        assert!(is_due(Some(last), -5, t(1000)));
    }

    #[test]
    fn independent_indexers_are_evaluated_independently() {
        // last_pull/min_interval are per-call, not shared state -- this is
        // really just documenting that `is_due` has no hidden global state
        // (unlike `RateLimiter`, which is keyed internally).
        let now = t(2000);
        assert!(is_due(Some(t(1000)), 500, now)); // due (1000s elapsed >= 500s)
        assert!(!is_due(Some(t(1999)), 500, now)); // not due (1s elapsed < 500s)
    }
}
