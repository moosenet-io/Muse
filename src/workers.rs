//! Background-worker harness.
//!
//! Phase 0 ships no real workers yet — the session poller, embedder,
//! taste-recompute, and proactive-scheduler workers land in MUSE-05+. This
//! module exists so `main.rs` has a stable spawn point to grow from.

use std::sync::Arc;

use crate::http::AppState;

/// Spawn all background workers for the given application state.
///
/// Currently a no-op harness: it logs that no workers are registered yet and
/// returns immediately. Future workers should be spawned here via
/// `tokio::spawn` and their `JoinHandle`s tracked/returned as needed.
pub fn spawn_workers(_state: Arc<AppState>) {
    tracing::info!("worker harness started: no workers registered yet (see MUSE-05+)");
}
