//! Background-worker harness.
//!
//! Registers the real background workers: the Plex session poller (MUSE-07,
//! spec §4-B) and the Prowlarr report-pull worker (MUSE-17). The embedder,
//! taste-recompute, and proactive-scheduler workers still land in later spec
//! items — this module remains the stable spawn point for them to grow from.

use std::sync::Arc;

use crate::http::AppState;
use crate::maintenance::{spawn_maintenance_worker, spawn_trending_worker};
use crate::proactive::scheduler as proactive_scheduler;
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

    // MUSE-12: the proactive-content generator worker — runs the five
    // event-driven generators for every account on a configurable cadence
    // and upserts cooldown/dedup-filtered results into `proactive_items`.
    // Always spawned (same posture as the tuner scheduler above): a
    // deployment with zero accounts yet just ticks a harmless no-op.
    proactive_scheduler::spawn(state.clone());
    tracing::info!("proactive content generator worker spawned (MUSE-12)");

    // MUSE-31: the background maintenance pipeline -- arr ingest ->
    // embed_stale -> per-account taste/divergence recompute -> bounded
    // enrichment, in dependency order. Always spawned (same posture as the
    // tuner scheduler / proactive generator above): with nothing configured
    // yet, each tick is a harmless no-op pass. This is what makes a freshly
    // deployed Muse self-populate embeddings/taste_profile/taste_divergence
    // -- previously nothing ever called those routines on a schedule.
    spawn_maintenance_worker(state.clone());
    tracing::info!("background maintenance worker spawned (MUSE-31)");

    // MUSE-31: the daily trending/population worker -- snapshot_trending +
    // compute_population_distributions, only when TMDb is configured.
    // Separate cadence from the maintenance pass above (coarser, TMDb-
    // specific); always spawned, no-ops cleanly without state.tmdb.
    spawn_trending_worker(state.clone());
    tracing::info!("trending/population worker spawned (MUSE-31)");
}
