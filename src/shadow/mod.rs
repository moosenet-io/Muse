//! MUSET-08 (Plane TERM #373): a shadow-mode runner for the
//! Tautulli-replacement (spec §4 — see `crate::tautulli` and
//! `crate::tracker`).
//!
//! ## What "shadow" means here
//! Muse's *ongoing* Tautulli replacement is the native Plex webhook/poller
//! capture in `crate::tracker` (MUSE-07): raw `play_events` are folded
//! (`tracker::reconstruct::fold_events`) into `play_sessions`, which is what
//! Tautulli's own watch-history/play-count analytics correspond to. This
//! module runs that same analytics computation in **shadow** — it produces
//! Muse's version of the watch-data output for later parity comparison
//! (MUSET-09) against the snapshot's Tautulli-origin numbers, but it never
//! takes over the live function:
//!
//! - **Non-authoritative by construction.** [`run`] takes an
//!   already-connected `&PgPool` and only ever *reads* from it
//!   (`repo::play_event::list_all_with_session_key`,
//!   `repo::account::get_by_plex_account_id`,
//!   `tracker::reconstruct::resolve_rating_key` — all `SELECT`s). There is
//!   no `INSERT`/`UPDATE`/`UPSERT` anywhere in this module: it cannot write
//!   to `play_sessions` or `watch_stats` (the tables the live function
//!   owns), and there is no "promote this to authoritative" switch. The
//!   [`ShadowResult`] it returns is a plain in-memory value the caller may
//!   log, compare, or discard — this module never places it anywhere.
//! - **Reuses the real analytics, never reimplements them.** The actual
//!   "what does a finished/abandoned/watched session look like" logic is
//!   100% delegated to the same pure fold
//!   (`crate::tracker::reconstruct::fold_events`) and rating-key resolution
//!   (`crate::tracker::reconstruct::resolve_rating_key`) the live
//!   `reconstruct_and_persist` path uses — this module only adds the
//!   read-only grouping/aggregation glue to run that fold over an entire
//!   snapshot's play_events and roll the per-session folds up into
//!   `watch_stats`-shaped rows ([`ShadowWatchStat`], deliberately the same
//!   field set as `models::watch_stats::NewWatchStats` so MUSET-09 can
//!   diff them directly).
//! - **Runs against SNAPSHOT data only.** [`run`] never opens its own pool
//!   (matching `crate::fixtures`' posture) — the caller obtains one via
//!   `crate::snapshot::load::connect_snapshot_db_from_env`/
//!   `connect_snapshot_db`, the ONE guarded path (AC5) that refuses to
//!   connect to anything live-shaped. See `main.rs`'s `muse shadow-run`
//!   subcommand for the wiring.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use sqlx::PgPool;

use crate::error::MuseResult;
use crate::models::play_event::PlayEvent;
use crate::repo;
use crate::tracker::reconstruct::{self, Fold};

/// Which side of a (account, media) pair resolved. Kept alongside the
/// resolved id (when available) so a caller can tell "we know the local id"
/// from "we only ever saw the raw Plex identifier" — useful for MUSE-09
/// parity comparison against `watch_stats`, which is keyed on resolved ids.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct AggregateKey {
    account_id: Option<i64>,
    account_ref: Option<String>,
    media_item_id: Option<i64>,
    episode_id: Option<i64>,
    rating_key: Option<String>,
}

/// One shadow-computed, `watch_stats`-shaped aggregate for a (account,
/// media) pair — Muse's own analytics output, computed purely from folded
/// `play_events`, structured for a direct MUSET-09 parity diff against
/// either the real `watch_stats` table or the snapshot's Tautulli-origin
/// numbers.
#[derive(Debug, Clone, PartialEq)]
pub struct ShadowWatchStat {
    /// Resolved local account id, when the account_ref could be matched
    /// against an existing `accounts` row (read-only lookup — this module
    /// never creates one).
    pub account_id: Option<i64>,
    /// The raw Plex account identifier the events carried, always present
    /// when any event in the group had one (kept even when resolved, so a
    /// caller can audit resolution failures).
    pub account_ref: Option<String>,
    pub media_item_id: Option<i64>,
    pub episode_id: Option<i64>,
    /// The raw Plex `ratingKey`, kept for the same audit reason as
    /// `account_ref`.
    pub rating_key: Option<String>,
    /// Number of distinct `session_key`s folded into this aggregate — the
    /// shadow analog of `watch_stats.play_count`.
    pub play_count: i32,
    pub finished_count: i32,
    /// Sessions `fold_events` marked `is_abandoned` — `watch_stats` itself
    /// only carries a single current `abandoned` bool per row; this shadow
    /// aggregate carries the count across all folded sessions (the fuller
    /// signal available when reducing straight from `play_events`) plus a
    /// `currently_abandoned` derived field below for a like-for-like diff.
    pub abandoned_count: i32,
    pub total_watched_ms: i64,
    pub avg_percent: Option<f32>,
    pub first_started_at: Option<DateTime<Utc>>,
    pub last_started_at: Option<DateTime<Utc>>,
}

impl ShadowWatchStat {
    /// `watch_stats.abandoned` is the state of the *most recent* session,
    /// not a lifetime count — the caller passes in whether the most
    /// recently-started fold was itself abandoned (easy for a caller
    /// iterating folds in order to track; this module's own aggregation
    /// doesn't need it, so it isn't tracked in [`Accumulator`]).
    pub fn currently_abandoned(&self, most_recent_was_abandoned: bool) -> bool {
        most_recent_was_abandoned && self.abandoned_count > 0
    }
}

/// The full shadow run output — Muse's computed analytics for later parity
/// comparison (MUSET-09), plus enough bookkeeping to sanity-check the run
/// itself (a shadow run over an empty/misconfigured snapshot should look
/// visibly empty, not silently plausible).
#[derive(Debug, Clone)]
pub struct ShadowResult {
    pub computed_at: DateTime<Utc>,
    /// Distinct `session_key`s seen in `play_events` before folding.
    pub session_keys_considered: usize,
    /// Sessions that actually produced a fold (non-empty event group —
    /// always equal to `session_keys_considered` in practice, since a
    /// group only exists because at least one event had that session_key;
    /// kept as its own field so a future caller can't accidentally conflate
    /// "considered" with "folded" if that ever changes).
    pub sessions_folded: usize,
    pub stats: Vec<ShadowWatchStat>,
}

struct Accumulator {
    key: AggregateKey,
    play_count: i32,
    finished_count: i32,
    abandoned_count: i32,
    total_watched_ms: i64,
    percent_sum: f32,
    percent_count: i32,
    first_started_at: Option<DateTime<Utc>>,
    last_started_at: Option<DateTime<Utc>>,
}

impl Accumulator {
    fn new(key: AggregateKey) -> Self {
        Self {
            key,
            play_count: 0,
            finished_count: 0,
            abandoned_count: 0,
            total_watched_ms: 0,
            percent_sum: 0.0,
            percent_count: 0,
            first_started_at: None,
            last_started_at: None,
        }
    }

    fn fold_in(&mut self, fold: &Fold) {
        self.play_count += 1;
        if fold.is_finished {
            self.finished_count += 1;
        }
        if fold.is_abandoned {
            self.abandoned_count += 1;
        }
        self.total_watched_ms += fold.watched_ms;
        if let Some(pct) = fold.percent_complete {
            self.percent_sum += pct;
            self.percent_count += 1;
        }
        self.first_started_at = Some(match self.first_started_at {
            Some(existing) => existing.min(fold.started_at),
            None => fold.started_at,
        });
        self.last_started_at = Some(match self.last_started_at {
            Some(existing) => existing.max(fold.started_at),
            None => fold.started_at,
        });
    }

    fn into_stat(self) -> ShadowWatchStat {
        ShadowWatchStat {
            account_id: self.key.account_id,
            account_ref: self.key.account_ref,
            media_item_id: self.key.media_item_id,
            episode_id: self.key.episode_id,
            rating_key: self.key.rating_key,
            play_count: self.play_count,
            finished_count: self.finished_count,
            abandoned_count: self.abandoned_count,
            total_watched_ms: self.total_watched_ms,
            avg_percent: if self.percent_count > 0 {
                Some(self.percent_sum / self.percent_count as f32)
            } else {
                None
            },
            first_started_at: self.first_started_at,
            last_started_at: self.last_started_at,
        }
    }
}

/// Group already-session-ordered `play_events` (as returned by
/// `repo::play_event::list_all_with_session_key`) into per-`session_key`
/// slices. Pure, read-only-in-spirit (no I/O — takes an owned `Vec` it
/// already has in memory).
fn group_by_session(events: Vec<PlayEvent>) -> Vec<Vec<PlayEvent>> {
    let mut groups: Vec<Vec<PlayEvent>> = Vec::new();
    for event in events {
        match groups.last_mut() {
            Some(current)
                if current.last().and_then(|e| e.session_key.as_deref())
                    == event.session_key.as_deref() =>
            {
                current.push(event);
            }
            _ => groups.push(vec![event]),
        }
    }
    groups
}

/// Run the shadow analytics pass over every `play_events` row in `pool`.
///
/// `pool` MUST already be a guard-validated snapshot/test connection (see
/// the module doc) — this function performs no DSN validation of its own
/// and, structurally, cannot mutate anything it reads: every call it makes
/// is a `SELECT`-backed repo/tracker read function, never an insert/upsert.
///
/// Read-only account/media resolution is best-effort: an unresolvable
/// `account_ref`/`rating_key` (no matching row yet — e.g. *arr ingest
/// hasn't seen the item in this snapshot) still contributes a
/// [`ShadowWatchStat`] keyed on the raw Plex identifiers, exactly like the
/// live `reconstruct_and_persist` path leaves an unresolved session as raw
/// `play_events` rather than erroring.
pub async fn run(pool: &PgPool) -> MuseResult<ShadowResult> {
    let events = repo::play_event::list_all_with_session_key(pool).await?;
    let session_keys_considered = events
        .iter()
        .filter_map(|e| e.session_key.as_deref())
        .collect::<std::collections::BTreeSet<_>>()
        .len();

    let groups = group_by_session(events);
    let mut sessions_folded = 0usize;
    let mut accumulators: BTreeMap<AggregateKey, Accumulator> = BTreeMap::new();

    for group in groups {
        // Reused verbatim from the live path — this is the one place the
        // actual "what counts as finished/abandoned/watched" analytics
        // logic lives; the shadow runner never reimplements it.
        let Some(fold) = reconstruct::fold_events(&group) else {
            continue;
        };
        sessions_folded += 1;

        // Read-only resolution (SELECTs only — see resolve_rating_key and
        // get_by_plex_account_id, neither of which writes).
        let account_id = match &fold.account_ref {
            Some(account_ref) => repo::account::get_by_plex_account_id(pool, account_ref)
                .await?
                .map(|a| a.id),
            None => None,
        };
        let (media_item_id, episode_id) = match &fold.rating_key {
            Some(rating_key) => reconstruct::resolve_rating_key(pool, rating_key).await?,
            None => (None, None),
        };

        let key = AggregateKey {
            account_id,
            account_ref: fold.account_ref.clone(),
            media_item_id,
            episode_id,
            rating_key: fold.rating_key.clone(),
        };

        accumulators
            .entry(key.clone())
            .or_insert_with(|| Accumulator::new(key))
            .fold_in(&fold);
    }

    let stats = accumulators
        .into_values()
        .map(Accumulator::into_stat)
        .collect();

    Ok(ShadowResult {
        computed_at: Utc::now(),
        session_keys_considered,
        sessions_folded,
        stats,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn group_by_session_splits_on_session_key_change() {
        fn ev(session_key: Option<&str>, offset_ms: i64) -> PlayEvent {
            PlayEvent {
                id: offset_ms,
                received_at: Utc::now(),
                source: "test".to_string(),
                event_type: "media.play".to_string(),
                account_ref: None,
                session_key: session_key.map(str::to_string),
                rating_key: None,
                view_offset_ms: Some(offset_ms),
                player: None,
                platform: None,
                product: None,
                device: None,
                ip_address: None,
                raw: serde_json::json!({}),
            }
        }

        let events = vec![
            ev(Some("s1"), 0),
            ev(Some("s1"), 1000),
            ev(Some("s2"), 0),
            ev(Some("s1"), 2000), // out-of-adjacency re-appearance of s1 -> new group
        ];
        let groups = group_by_session(events);
        assert_eq!(
            groups.len(),
            3,
            "adjacent-run grouping should yield 3 groups"
        );
        assert_eq!(groups[0].len(), 2);
        assert_eq!(groups[1].len(), 1);
        assert_eq!(groups[2].len(), 1);
    }

    #[test]
    fn shadow_watch_stat_currently_abandoned_requires_both_signals() {
        let stat = ShadowWatchStat {
            account_id: None,
            account_ref: None,
            media_item_id: None,
            episode_id: None,
            rating_key: None,
            play_count: 2,
            finished_count: 0,
            abandoned_count: 1,
            total_watched_ms: 0,
            avg_percent: None,
            first_started_at: None,
            last_started_at: None,
        };
        assert!(stat.currently_abandoned(true));
        assert!(!stat.currently_abandoned(false));

        let never_abandoned = ShadowWatchStat {
            abandoned_count: 0,
            ..stat
        };
        assert!(!never_abandoned.currently_abandoned(true));
    }

    // ===================================================================
    // DB-gated: the real shadow run against the guarded snapshot/test
    // Postgres, plus the load-bearing negative test proving shadow mode is
    // non-authoritative.
    //
    // Gated exactly like crate::snapshot::db_gated / crate::fixtures::loader
    // tests — skips cleanly when no MUSE_SNAPSHOT_DATABASE_URL /
    // MUSE_TEST_DATABASE_URL is configured, never touches anything live.
    // ===================================================================
    mod db_gated {
        use uuid::Uuid;

        use super::*;
        use crate::models::play_event::NewPlayEvent;
        use crate::snapshot::load;

        async fn snapshot_pool_or_skip(test_name: &str) -> Option<PgPool> {
            let Some(database_url) = load::snapshot_database_url_from_env() else {
                eprintln!(
                    "{} / {} not set -- skipping {test_name} (expected in the \
                     default test run; the shadow runner does not require a \
                     live DB)",
                    load::SNAPSHOT_DATABASE_URL_VAR,
                    load::TEST_DATABASE_URL_VAR,
                );
                return None;
            };
            let pool = load::connect_snapshot_db(&database_url)
                .await
                .expect("connect to the configured snapshot/test DSN (guard-checked)");
            load::migrate_snapshot_db(&pool)
                .await
                .expect("migrations should apply cleanly");
            Some(pool)
        }

        const WATCHED_TABLES: &[&str] =
            &["play_events", "play_sessions", "watch_stats", "accounts"];

        async fn snapshot_row_counts(pool: &PgPool) -> Vec<(&'static str, i64)> {
            let mut counts = Vec::with_capacity(WATCHED_TABLES.len());
            for table in WATCHED_TABLES {
                let count: i64 = sqlx::query_scalar(&format!("SELECT COUNT(*) FROM {table}"))
                    .fetch_one(pool)
                    .await
                    .unwrap_or(0);
                counts.push((*table, count));
            }
            counts
        }

        /// Seed a handful of raw `play_events` shaped like a snapshot's
        /// ingested play data (tagged `source = "snapshot:tautulli"`, same
        /// convention `snapshot::normalize::load_tautulli_history_snapshot`
        /// uses) -- one finished session, one abandoned session, both under
        /// the same synthetic account/rating-key pair distinguished by a
        /// fresh UUID suffix so repeated runs never collide.
        async fn seed_two_sessions(pool: &PgPool, suffix: &str) -> (String, String) {
            let account_ref = format!("muset08-account-{suffix}");
            let rating_key = format!("muset08-rk-{suffix}");
            let session_finished = format!("muset08-session-finished-{suffix}");
            let session_abandoned = format!("muset08-session-abandoned-{suffix}");

            // Finished session: play then a scrobble.
            repo::play_event::insert(
                pool,
                &NewPlayEvent {
                    source: "snapshot:tautulli".to_string(),
                    event_type: "media.play".to_string(),
                    account_ref: Some(account_ref.clone()),
                    session_key: Some(session_finished.clone()),
                    rating_key: Some(rating_key.clone()),
                    view_offset_ms: Some(0),
                    player: None,
                    platform: None,
                    product: None,
                    device: None,
                    ip_address: None,
                    raw: serde_json::json!({"duration": 100_000}),
                },
            )
            .await
            .expect("insert finished-session play event");
            repo::play_event::insert(
                pool,
                &NewPlayEvent {
                    source: "snapshot:tautulli".to_string(),
                    event_type: "media.scrobble".to_string(),
                    account_ref: Some(account_ref.clone()),
                    session_key: Some(session_finished.clone()),
                    rating_key: Some(rating_key.clone()),
                    view_offset_ms: Some(95_000),
                    player: None,
                    platform: None,
                    product: None,
                    device: None,
                    ip_address: None,
                    raw: serde_json::json!({"duration": 100_000}),
                },
            )
            .await
            .expect("insert scrobble event");

            // Abandoned session: play then an early stop (well under
            // reconstruct::ABANDON_THRESHOLD).
            repo::play_event::insert(
                pool,
                &NewPlayEvent {
                    source: "snapshot:tautulli".to_string(),
                    event_type: "media.play".to_string(),
                    account_ref: Some(account_ref.clone()),
                    session_key: Some(session_abandoned.clone()),
                    rating_key: Some(rating_key.clone()),
                    view_offset_ms: Some(0),
                    player: None,
                    platform: None,
                    product: None,
                    device: None,
                    ip_address: None,
                    raw: serde_json::json!({"duration": 100_000}),
                },
            )
            .await
            .expect("insert abandoned-session play event");
            repo::play_event::insert(
                pool,
                &NewPlayEvent {
                    source: "snapshot:tautulli".to_string(),
                    event_type: "media.stop".to_string(),
                    account_ref: Some(account_ref.clone()),
                    session_key: Some(session_abandoned.clone()),
                    rating_key: Some(rating_key.clone()),
                    view_offset_ms: Some(5_000),
                    player: None,
                    platform: None,
                    product: None,
                    device: None,
                    ip_address: None,
                    raw: serde_json::json!({"duration": 100_000}),
                },
            )
            .await
            .expect("insert stop event");

            (account_ref, rating_key)
        }

        /// The core MUSET-08 test: a shadow run over synthetic snapshot-shaped
        /// `play_events` produces its own (non-persisted) analytics output --
        /// reusing the real `fold_events` analytics, computing a finished and
        /// an abandoned session correctly -- entirely via the guarded snapshot
        /// connection path.
        #[tokio::test]
        async fn shadow_run_computes_analytics_from_snapshot_play_events() {
            let Some(pool) =
                snapshot_pool_or_skip("shadow_run_computes_analytics_from_snapshot_play_events")
                    .await
            else {
                return;
            };

            let suffix = Uuid::new_v4().simple().to_string();
            let (account_ref, rating_key) = seed_two_sessions(&pool, &suffix).await;

            let result = run(&pool).await.expect("shadow run should succeed");

            let stat = result
                .stats
                .iter()
                .find(|s| {
                    s.account_ref.as_deref() == Some(account_ref.as_str())
                        && s.rating_key.as_deref() == Some(rating_key.as_str())
                })
                .expect(
                    "the seeded (account_ref, rating_key) pair should appear in the shadow output",
                );

            assert_eq!(stat.play_count, 2, "two distinct session_keys were seeded");
            assert_eq!(
                stat.finished_count, 1,
                "exactly one session reached the scrobble"
            );
            assert_eq!(
                stat.abandoned_count, 1,
                "exactly one session stopped well under the abandon threshold"
            );
            assert!(stat.total_watched_ms > 0);

            // Cleanup -- leave the scratch DB as clean as we found it (same
            // posture as crate::snapshot::db_gated).
            sqlx::query("DELETE FROM play_events WHERE account_ref = $1")
                .bind(&account_ref)
                .execute(&pool)
                .await
                .ok();
        }

        /// The load-bearing negative test (AC): shadow mode is
        /// **non-authoritative**. Snapshot the watch-data-of-record tables
        /// (`play_sessions`, `watch_stats`) plus `play_events`/`accounts`
        /// before and after a shadow run over freshly-seeded snapshot data,
        /// and assert every one of them is byte-for-byte unchanged except
        /// for the seed's own inserts (which the shadow run itself performs
        /// none of). This proves by observation -- not by code inspection --
        /// that `shadow::run` never writes to, mutates, or becomes the
        /// source-of-truth for the live watch-data function: it only reads
        /// and returns an in-memory [`ShadowResult`].
        #[tokio::test]
        async fn shadow_run_never_mutates_the_watch_data_of_record() {
            let Some(pool) =
                snapshot_pool_or_skip("shadow_run_never_mutates_the_watch_data_of_record").await
            else {
                return;
            };

            let suffix = Uuid::new_v4().simple().to_string();
            let (account_ref, _rating_key) = seed_two_sessions(&pool, &suffix).await;

            // Snapshot state AFTER seeding (the seed step is an explicit,
            // out-of-band `play_events` insert simulating snapshot-loaded
            // data -- not part of the shadow runner under test) but BEFORE
            // the shadow run.
            let before = snapshot_row_counts(&pool).await;

            let result = run(&pool).await.expect("shadow run should succeed");
            assert!(
                !result.stats.is_empty(),
                "sanity: the shadow run must have actually computed something \
                 over the seeded data, not silently no-op'd"
            );

            let after = snapshot_row_counts(&pool).await;

            assert_eq!(
                before, after,
                "a shadow run must never mutate play_events/play_sessions/watch_stats/accounts \
                 -- shadow mode is non-authoritative by construction"
            );

            // `play_sessions`/`watch_stats` specifically must show zero rows
            // for the seeded account -- proving the shadow run didn't
            // silently become the live reconstruct/recompute path under a
            // different name.
            let session_rows: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM play_sessions ps JOIN accounts a ON a.id = ps.account_id \
                 WHERE a.plex_account_id = $1",
            )
            .bind(&account_ref)
            .fetch_one(&pool)
            .await
            .unwrap_or(-1);
            assert_eq!(
                session_rows, 0,
                "no play_sessions row should exist for the shadow-only seeded account \
                 (that account was never created by this test, and shadow::run never creates one)"
            );

            sqlx::query("DELETE FROM play_events WHERE account_ref = $1")
                .bind(&account_ref)
                .execute(&pool)
                .await
                .ok();
        }
    }
}
