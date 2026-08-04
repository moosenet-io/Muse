//! The persistence edge shared by every probe consumer: what a finished probe
//! should be written as, and the two MPRB-05 writers that write it.
//!
//! # Why this is a module and not two copies
//!
//! MPRB-06 built [`ProbeWrite`], [`probe_write`], [`ProbeSink`] and
//! [`DbProbeSink`] inside `crate::library::scan`, where the scanner was the only
//! consumer. MPRB-07 adds the second one — the backfill worker — and a second
//! consumer is exactly the moment the choice is made between one definition and
//! two. Two would have meant two places deciding which MPRB-05 writer to call
//! and two places labelling a suspicious result, free to drift the first time a
//! `ProbeError` variant is added.
//!
//! So this is a **move, not a rewrite**: the bodies below are MPRB-06's,
//! verbatim, with visibility widened to `pub(crate)`. `crate::library::scan`
//! imports them and behaves identically; `crate::media::backfill` imports the
//! same ones.
//!
//! # There is exactly one `match` over `ProbeError` in the probe path, and it
//! is not here
//!
//! [`ProbeWrite::Failure`] carries the error **verbatim**. Classification is
//! `StoredProbeState::from_error` → `ProbeError::state` (MPRB-02), and nowhere
//! else — neither this module, nor the scanner, nor the backfill worker matches
//! on a `ProbeError` variant.
//!
//! # Why the DB edge is a trait
//!
//! With no `MUSE_TEST_DATABASE_URL` on the build host (MUSE #130) anything
//! expressed behind a pool cannot execute in a test. Keeping the pool behind a
//! two-arm dispatch means every rule *above* it — what the probe said, what gets
//! counted, what is suspicious, what is retryable — runs for real against a
//! recording fake in both consumers' tests.

use async_trait::async_trait;
use sqlx::PgPool;

use crate::error::MuseResult;
use crate::media::probe::{MediaProbe, ProbeError};
use crate::repo;

/// What a finished probe should have written to the row.
///
/// A borrowed view, not a copy: it exists so the decision of *which* MPRB-05
/// writer to call, and *what* to label the result, can be made and tested
/// without a database connection.
#[derive(Debug)]
pub(crate) enum ProbeWrite<'a> {
    /// It parsed. `suspicion` is [`crate::media::derive::suspicion`]'s verdict —
    /// `None` for a file with nothing wrong with it. A suspicious result is
    /// still stored, and stored labelled; MPRB-05's `set_probe_result` owns that
    /// rule and this type only carries the description to it.
    Document {
        probe: &'a MediaProbe,
        suspicion: Option<&'static str>,
    },
    /// It did not. The `ProbeError` is passed through **verbatim**;
    /// classification happens in `StoredProbeState::from_error` →
    /// `ProbeError::state` (MPRB-02), and nowhere else. This module contains no
    /// `match` over `ProbeError` — deliberately, because a second one would be
    /// free to drift from the first the moment a variant is added.
    Failure { error: &'a ProbeError },
}

/// Decide what to write, from what the probe returned.
pub(crate) fn probe_write(result: &Result<MediaProbe, ProbeError>) -> ProbeWrite<'_> {
    match result {
        Ok(probe) => ProbeWrite::Document {
            probe,
            // Called, not restated. `suspicion` is MPRB-03's rule and there is
            // exactly one of it.
            suspicion: crate::media::derive::suspicion(probe).map(|s| s.as_str()),
        },
        Err(error) => ProbeWrite::Failure { error },
    }
}

/// The database edge of the probe step — **the only part of it that needs a
/// pool**, and deliberately the only part that is not exercised without one.
///
/// Its whole body is a two-arm dispatch onto MPRB-05's writers. Everything that
/// decides anything — whether to probe, what the probe said, how a failure is
/// classified, what gets counted — sits above this trait and runs for real in
/// its consumers' tests against a recording fake. Without this split the scan
/// integration and the backfill worker would both be verifiable only where
/// `MUSE_TEST_DATABASE_URL` is set, which is not where they are built
/// (MUSE #130).
///
/// `Send + Sync` is required at the use site rather than left to inference: both
/// consumers run inside an axum handler or a spawned task, and a `&dyn Trait`
/// held across an `await` is what silently makes that future non-`Send`.
#[async_trait]
pub(crate) trait ProbeSink {
    async fn record(
        &self,
        media_file_id: i64,
        relative_path: &str,
        write: &ProbeWrite<'_>,
    ) -> MuseResult<()>;
}

/// The production sink: MPRB-05's two writers, and nothing else.
pub(crate) struct DbProbeSink<'a>(pub(crate) &'a PgPool);

#[async_trait]
impl ProbeSink for DbProbeSink<'_> {
    async fn record(
        &self,
        media_file_id: i64,
        relative_path: &str,
        write: &ProbeWrite<'_>,
    ) -> MuseResult<()> {
        match write {
            ProbeWrite::Document { probe, suspicion } => {
                repo::media_file::set_probe_result(
                    self.0,
                    media_file_id,
                    relative_path,
                    probe,
                    *suspicion,
                )
                .await
            }
            ProbeWrite::Failure { error } => {
                repo::media_file::set_probe_error(self.0, media_file_id, error).await
            }
        }
    }
}
