//! Background-worker harness.
//!
//! The session poller, embedder, taste-recompute, and proactive-scheduler
//! workers still land in MUSE-05+; the Prowlarr report-pull worker (MUSE-17)
//! is the first real background worker to register here.

use std::sync::Arc;

use crate::http::AppState;
use crate::prowlarr::spawn_report_pull_worker;

/// Spawn all background workers for the given application state.
///
/// Each worker is expected to degrade gracefully rather than panic when its
/// upstream dependency isn't configured (see `prowlarr::worker::run_tick`,
/// which no-ops per tick when `state.prowlarr` is `None`) -- but the report
/// -pull worker is only spawned at all when Prowlarr IS configured, so an
/// unconfigured deployment doesn't run an idle tokio task forever.
pub fn spawn_workers(state: Arc<AppState>) {
    tracing::info!("worker harness started");

    if state.prowlarr.is_some() {
        spawn_report_pull_worker(state.clone());
        tracing::info!("prowlarr report-pull worker spawned (MUSE-17)");
    } else {
        tracing::info!("prowlarr not configured; report-pull worker not started");
    }
}
