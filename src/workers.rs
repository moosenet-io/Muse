//! Background-worker harness.
//!
//! MUSE-07 registers the first real worker: the Plex session poller (spec
//! §4-B). The embedder, taste-recompute, and proactive-scheduler workers
//! still land in later spec items — this module remains the stable spawn
//! point for them to grow from.

use std::sync::Arc;

use crate::http::AppState;
use crate::tracker::poller;

/// Spawn all background workers for the given application state.
///
/// Future workers should be spawned here via `tokio::spawn` and their
/// `JoinHandle`s tracked/returned as needed.
pub fn spawn_workers(state: Arc<AppState>) {
    tracing::info!("worker harness started");
    poller::spawn(state);
}
