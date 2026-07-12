//! Background-worker harness.
//!
//! Registers the real background workers: the Plex session poller (MUSE-07,
//! spec §4-B) and the Prowlarr report-pull worker (MUSE-17). The embedder,
//! taste-recompute, and proactive-scheduler workers still land in later spec
//! items — this module remains the stable spawn point for them to grow from.

use std::sync::Arc;

use crate::http::AppState;
use crate::prowlarr::spawn_report_pull_worker;
use crate::tracker::poller;

/// Spawn all background workers for the given application state.
///
/// Each worker degrades gracefully rather than panicking when its upstream
/// dependency isn't configured. The report-pull worker is only spawned when
/// Prowlarr IS configured, so an unconfigured deployment doesn't run an idle
/// tokio task forever. The Plex poller likewise no-ops (a single log line)
/// when Plex isn't configured.
pub fn spawn_workers(state: Arc<AppState>) {
    tracing::info!("worker harness started");

    poller::spawn(state.clone());

    if state.prowlarr.is_some() {
        spawn_report_pull_worker(state.clone());
        tracing::info!("prowlarr report-pull worker spawned (MUSE-17)");
    } else {
        tracing::info!("prowlarr not configured; report-pull worker not started");
    }

    // MUSE-28: the linear-channel director — keeps every mode='linear'
    // channel's channel_programs grid topped off a rolling
    // channel_guide_window_hours ahead. Always spawned: a deployment with
    // no linear channels yet just ticks a no-op (empty channel list), same
    // graceful-degrade posture as an unreachable DB.
    crate::tuner::scheduler::spawn(state.clone());
    tracing::info!("linear tuner scheduler worker spawned (MUSE-28)");
}
