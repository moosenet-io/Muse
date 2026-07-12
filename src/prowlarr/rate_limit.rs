//! Tracker etiquette guard (MUSE-16 §4b: "respecting tracker rules protects
//! your account standing — treated as a first-class constraint, tested").
//!
//! Two independent constraints, both enforced here:
//! - a per-key **minimum interval** between calls (report-pull cadence per
//!   indexer — never poll a given indexer faster than its configured
//!   `polite_min_interval_secs`);
//! - a **rolling hourly cap** on targeted searches (the "hard cap on
//!   searches/hour" the spec calls for, since targeted search is meant to be
//!   used "sparingly").
//!
//! This never blocks/sleeps the caller — it's a yes/no gate. A caller that
//! gets `Err` should skip or reschedule, not spin-wait; that keeps a
//! misbehaving worker from turning into the very hammering this guards
//! against.

use std::collections::HashMap;
use std::time::Duration;

use tokio::sync::Mutex;
use tokio::time::Instant;

use crate::error::{MuseError, MuseResult};

#[derive(Debug)]
pub struct RateLimiter {
    last_call: Mutex<HashMap<String, Instant>>,
    hourly_calls: Mutex<Vec<Instant>>,
}

impl RateLimiter {
    pub fn new() -> Self {
        Self {
            last_call: Mutex::new(HashMap::new()),
            hourly_calls: Mutex::new(Vec::new()),
        }
    }

    /// Enforce a minimum interval between calls keyed by `key` (typically
    /// `"indexer:{prowlarr_id}"`). Records `now` and returns `Ok(())` if
    /// enough time has passed since the last call for this key (or this is
    /// the first call); returns `Err(MuseError::Conflict)` otherwise.
    pub async fn gate_min_interval(&self, key: &str, min_interval: Duration) -> MuseResult<()> {
        let now = Instant::now();
        let mut guard = self.last_call.lock().await;

        if let Some(&last) = guard.get(key) {
            let elapsed = now.saturating_duration_since(last);
            if elapsed < min_interval {
                return Err(MuseError::Conflict(format!(
                    "rate limited: '{key}' was polled {elapsed:?} ago; minimum polite interval is {min_interval:?}"
                )));
            }
        }

        guard.insert(key.to_string(), now);
        Ok(())
    }

    /// Enforce a rolling hourly cap on calls through this limiter (the
    /// targeted-search budget). Returns `Err(MuseError::Conflict)` once
    /// `max_per_hour` calls have been recorded within the trailing hour.
    pub async fn gate_hourly_cap(&self, max_per_hour: usize) -> MuseResult<()> {
        let now = Instant::now();
        let window = Duration::from_secs(3600);
        let mut calls = self.hourly_calls.lock().await;

        calls.retain(|&t| now.saturating_duration_since(t) < window);

        if calls.len() >= max_per_hour {
            return Err(MuseError::Conflict(format!(
                "rate limited: hourly targeted-search cap of {max_per_hour} reached"
            )));
        }

        calls.push(now);
        Ok(())
    }
}

impl Default for RateLimiter {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn min_interval_blocks_immediate_repeat_and_allows_after_elapsed() {
        let limiter = RateLimiter::new();
        let interval = Duration::from_secs(900);

        limiter
            .gate_min_interval("indexer:1", interval)
            .await
            .expect("first call should be allowed");

        let err = limiter
            .gate_min_interval("indexer:1", interval)
            .await
            .expect_err("immediate repeat should be rate limited");
        match err {
            MuseError::Conflict(_) => {}
            other => panic!("expected Conflict, got {other:?}"),
        }

        tokio::time::advance(interval).await;

        limiter
            .gate_min_interval("indexer:1", interval)
            .await
            .expect("call after the interval has elapsed should be allowed");
    }

    #[tokio::test(start_paused = true)]
    async fn min_interval_is_independent_per_key() {
        let limiter = RateLimiter::new();
        let interval = Duration::from_secs(900);

        limiter.gate_min_interval("indexer:1", interval).await.unwrap();

        // A different indexer is unaffected by indexer:1's cooldown.
        limiter
            .gate_min_interval("indexer:2", interval)
            .await
            .expect("a different key should not be rate limited by another key's call");
    }

    #[tokio::test(start_paused = true)]
    async fn hourly_cap_blocks_once_reached_and_resets_after_window() {
        let limiter = RateLimiter::new();

        for _ in 0..3 {
            limiter
                .gate_hourly_cap(3)
                .await
                .expect("calls under the cap should be allowed");
        }

        let err = limiter
            .gate_hourly_cap(3)
            .await
            .expect_err("the 4th call within the hour should be rejected");
        match err {
            MuseError::Conflict(_) => {}
            other => panic!("expected Conflict, got {other:?}"),
        }

        tokio::time::advance(Duration::from_secs(3601)).await;

        limiter
            .gate_hourly_cap(3)
            .await
            .expect("budget should refresh once the oldest call falls outside the window");
    }
}
