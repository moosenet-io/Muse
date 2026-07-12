//! MUSE-12: the proactive-content background worker — runs the generators
//! (`super::generators`) for every account on a configurable cadence and
//! upserts their output into `proactive_items`.
//!
//! Mirrors the spawn/degrade posture of every other worker in
//! `crate::workers` (`tracker::poller`, `prowlarr::spawn_report_pull_worker`,
//! `tuner::scheduler`): a `tokio::time::interval` loop that never panics —
//! a failure fetching the account list, or a single account's generator
//! pass failing, is logged and the loop just ticks again next interval.

use std::sync::Arc;
use std::time::Duration as StdDuration;

use crate::http::AppState;
use crate::taste_model::chord_client::ChordClient;

/// Spawn the proactive-content worker. Always spawned (same posture as the
/// linear-tuner scheduler): a deployment with zero accounts yet just ticks a
/// no-op, which is a cheap, harmless idle loop rather than special-cased
/// away.
pub fn spawn(state: Arc<AppState>) {
    let tick = StdDuration::from_secs(state.config.proactive_tick_interval_secs);
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(tick);
        // The first tick fires immediately; skip it so a hot-reload/restart
        // doesn't hammer every account's generators before the process has
        // even finished booting other workers.
        interval.tick().await;
        loop {
            interval.tick().await;
            run_once(&state).await;
        }
    });
}

/// One full pass: every account, every generator, cooldown-filtered and
/// persisted. Broken out from [`spawn`] so it's directly callable from a
/// live-DB test without paying for the interval-loop machinery.
pub async fn run_once(state: &Arc<AppState>) {
    let chord = ChordClient::from_config(&state.config);

    let accounts = match crate::repo::account::list(&state.pool).await {
        Ok(accounts) => accounts,
        Err(e) => {
            tracing::warn!(error = %e, "MUSE-12: proactive worker could not list accounts this pass; skipping");
            return;
        }
    };

    for account in accounts {
        match super::generators::generate_for_account(&state.pool, chord.as_ref(), account.id).await {
            Ok(created) if !created.is_empty() => {
                tracing::info!(
                    account_id = account.id,
                    created = created.len(),
                    "MUSE-12: proactive generator pass created new item(s)"
                );
            }
            Ok(_) => {}
            Err(e) => tracing::warn!(
                error = %e,
                account_id = account.id,
                "MUSE-12: proactive generator pass failed for this account; skipping (graceful degrade)"
            ),
        }
    }
}
