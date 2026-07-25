//! MUSE-31: on-demand ops routes — the manual trigger for the same
//! routines the background workers in [`crate::maintenance`] run on a
//! schedule. Mainly useful to prime a freshly-deployed Muse (rather than
//! waiting out the first maintenance-tick interval) and for operator
//! debugging.
//!
//! Every handler here follows the same degrade posture as the rest of the
//! crate: a required upstream that isn't configured is a clean `503`
//! ([`MuseError::ServiceUnavailable`]), never a `500` — see the module docs
//! on `crate::maintenance` for why each step is optional in the first
//! place.

use std::sync::Arc;

use axum::extract::State;
use axum::Json;
use serde_json::{json, Value};

use crate::error::{MuseError, MuseResult};
use crate::http::AppState;

/// `POST /ops/ingest/arr` — run [`crate::arr::ingest::run`] once, right now,
/// over every configured `*arr` instance. `503` when `MUSE_ARR_INSTANCES`
/// isn't configured (nothing to ingest).
pub async fn ingest_arr(State(state): State<Arc<AppState>>) -> MuseResult<Json<Value>> {
    if state.arr_instances.is_empty() {
        return Err(MuseError::ServiceUnavailable(
            "no arr instances configured (MUSE_ARR_INSTANCES)".to_string(),
        ));
    }

    let summary = crate::arr::ingest::run(&state.pool, &state.arr_instances).await;

    Ok(Json(json!({
        "instances_ok": summary.instances_ok,
        "instances_skipped": summary.instances_skipped.iter().map(|(name, err)| json!({
            "instance": name,
            "error": err,
        })).collect::<Vec<_>>(),
        "movies_upserted": summary.movies_upserted,
        "series_upserted": summary.series_upserted,
        "episodes_upserted": summary.episodes_upserted,
        "files_upserted": summary.files_upserted,
    })))
}

/// `POST /ops/ingest/tautulli` — run the one-time [`crate::tautulli::backfill::run`]
/// history import, right now, with default [`crate::tautulli::backfill::BackfillOptions`].
/// `503` when `TAUTULLI_URL`/`TAUTULLI_API_KEY` aren't configured.
///
/// Unlike the maintenance pass's steps, this is intentionally NOT a
/// scheduled worker (see the module doc on `crate::tautulli::backfill`: a
/// one-time import, safe to re-run, but not something that should run every
/// 30 minutes against a Tautulli instance) — this route is its only caller.
pub async fn ingest_tautulli(State(state): State<Arc<AppState>>) -> MuseResult<Json<Value>> {
    let Some(client) = crate::tautulli::TautulliClient::from_config(&state.config) else {
        return Err(MuseError::ServiceUnavailable(
            "tautulli not configured (TAUTULLI_URL/TAUTULLI_API_KEY)".to_string(),
        ));
    };

    let summary = crate::tautulli::backfill::run(
        &state.pool,
        &client,
        &crate::tautulli::backfill::BackfillOptions::default(),
    )
    .await?;

    Ok(Json(json!({
        "pages_fetched": summary.pages_fetched,
        "rows_seen": summary.rows_seen,
        "imported": summary.imported,
        "skipped_already_imported": summary.skipped_already_imported,
        "skipped_native_overlap": summary.skipped_native_overlap,
        "skipped_missing_reference_id": summary.skipped_missing_reference_id,
        "resolved_media": summary.resolved_media,
        "unresolved_media": summary.unresolved_media,
    })))
}

/// Upper bound on how many unresolved sessions the on-demand
/// `POST /ops/library/resolve` re-resolution pass processes in one call —
/// generous (the pre-existing Tautulli backfill is ~1.5k rows), so a single
/// operator-triggered run drains the whole backlog.
const OPS_RESOLVE_LIMIT: i64 = 100_000;

/// `POST /ops/library/resolve` — BSEED-2: re-resolve previously-imported
/// Tautulli sessions that never matched a library item (`media_item_id IS
/// NULL`) against the now-populated catalog (arr ingest). This is the door
/// that turns the pre-existing imported watch history into taste input once
/// `media_items` carry matchable ids.
///
/// Supplies a Tautulli client when configured so sessions imported *before*
/// migration 0108 (no stored Plex keys — the pre-existing ~1.5k) can be
/// rehydrated from `get_history`/`get_metadata` and unblocked; sessions
/// imported after 0108 re-resolve fully offline regardless. Always `200`: the
/// pass is fully error-isolated (per-session failures are logged and skipped),
/// and an upstream Tautulli/paging failure degrades to "resolved what it could
/// offline" rather than erroring.
pub async fn resolve_library(State(state): State<Arc<AppState>>) -> Json<Value> {
    let tautulli = crate::tautulli::TautulliClient::from_config(&state.config);
    let summary = match crate::tautulli::backfill::resolve_existing_unresolved(
        &state.pool,
        tautulli.as_ref(),
        &crate::tautulli::backfill::BackfillOptions::default(),
        OPS_RESOLVE_LIMIT,
    )
    .await
    {
        Ok(summary) => summary,
        Err(e) => {
            tracing::warn!(error = %e, "POST /ops/library/resolve — re-resolution pass failed; returning empty summary");
            crate::tautulli::backfill::ResolveSummary::default()
        }
    };

    Json(json!({
        "sessions_considered": summary.sessions_considered,
        "resolved": summary.resolved,
        "deduped_conflicts": summary.deduped_conflicts,
        "still_unresolved": summary.still_unresolved,
        "tautulli_used": summary.tautulli_used,
    }))
}

/// `POST /ops/maintenance` — run one full [`crate::maintenance::run_maintenance_pass`]
/// pass, right now. Never fails (every step inside the pass is already
/// error-isolated — see that module's docs); always `200`, useful to prime
/// a fresh deploy's `embeddings`/`taste_profile`/`taste_divergence` rather
/// than waiting out the first scheduled tick.
pub async fn run_maintenance_now(State(state): State<Arc<AppState>>) -> Json<Value> {
    let summary = crate::maintenance::run_maintenance_pass(&state).await;

    Json(json!({
        "arr_ran": summary.arr_ran,
        "arr_instances_ok": summary.arr_instances_ok,
        "arr_instances_skipped": summary.arr_instances_skipped,
        "arr_movies_upserted": summary.arr_movies_upserted,
        "arr_series_upserted": summary.arr_series_upserted,
        "resolve_ran": summary.resolve_ran,
        "sessions_resolved": summary.sessions_resolved,
        "sessions_deduped": summary.sessions_deduped,
        "watch_stats_rebuilt": summary.watch_stats_rebuilt,
        "watch_stats_rebuild_failed": summary.watch_stats_rebuild_failed,
        "embed_ran": summary.embed_ran,
        "embedded": summary.embedded,
        "embed_skipped_unchanged": summary.embed_skipped_unchanged,
        "embed_failed": summary.embed_failed,
        "accounts_considered": summary.accounts_considered,
        "taste_recomputed": summary.taste_recomputed,
        "taste_failed": summary.taste_failed,
        "divergence_recomputed": summary.divergence_recomputed,
        "divergence_failed": summary.divergence_failed,
        "enrichment_ran": summary.enrichment_ran,
        "enrichment_attempted": summary.enrichment_attempted,
        "enrichment_failed": summary.enrichment_failed,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A pool built via `connect_lazy` never actually dials Postgres until
    /// first use — safe to construct in a unit test with no live DB, as
    /// long as the code path under test never touches it (both degrade-path
    /// handlers below short-circuit before any query).
    fn lazy_test_pool() -> sqlx::PgPool {
        sqlx::postgres::PgPoolOptions::new()
            .connect_lazy("postgres://user:pass@127.0.0.1:1/muse_test_lazy")
            .expect("connect_lazy should never fail synchronously")
    }

    fn state_with(arr_instances: Vec<crate::arr::ArrInstanceConfig>, config: crate::config::Config) -> AppState {
        AppState {
            pool: lazy_test_pool(),
            enrichment: crate::enrichment::EnrichmentService::from_config(&config),
            config,
            plex: None,
            prowlarr: None,
            arr_instances,
            tmdb: None,
            embed: None,
            download: None,
        }
    }

    #[tokio::test]
    async fn ingest_arr_degrades_to_service_unavailable_when_unconfigured() {
        let state = Arc::new(state_with(Vec::new(), crate::config::Config::default()));
        let result = ingest_arr(State(state)).await;
        assert!(
            matches!(result, Err(MuseError::ServiceUnavailable(_))),
            "no arr instances configured should degrade to a clean 503, not run/panic"
        );
    }

    #[tokio::test]
    async fn ingest_tautulli_degrades_to_service_unavailable_when_unconfigured() {
        let state = Arc::new(state_with(Vec::new(), crate::config::Config::default()));
        let result = ingest_tautulli(State(state)).await;
        assert!(
            matches!(result, Err(MuseError::ServiceUnavailable(_))),
            "no tautulli config should degrade to a clean 503, not attempt a connection"
        );
    }

    // --- live-DB happy-path route tests -----------------------------------
    //
    // Gated on MUSE_TEST_DATABASE_URL, same posture as every other live-DB
    // test in this crate: skips cleanly (does not fail) when unset.
    #[tokio::test]
    async fn run_maintenance_now_always_returns_200_even_with_nothing_configured() {
        let Ok(database_url) = std::env::var("MUSE_TEST_DATABASE_URL") else {
            eprintln!(
                "MUSE_TEST_DATABASE_URL not set — skipping \
                 run_maintenance_now_always_returns_200_even_with_nothing_configured"
            );
            return;
        };

        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
            .connect(&database_url)
            .await
            .expect("connect to MUSE_TEST_DATABASE_URL");
        sqlx::migrate!("./migrations")
            .run(&pool)
            .await
            .expect("migrations should apply cleanly");

        let config = crate::config::Config::default();
        let state = Arc::new(AppState {
            pool,
            enrichment: crate::enrichment::EnrichmentService::from_config(&config),
            config,
            plex: None,
            prowlarr: None,
            arr_instances: Vec::new(),
            tmdb: None,
            embed: None,
            download: None,
        });

        // The handler itself has no fallible path (it always returns
        // `Json<Value>`, never a `MuseResult`) -- this asserts it completes
        // and that the response shape carries every field the maintenance
        // pass reports, proving the route is wired to the real pass and not
        // a stub.
        let Json(body) = run_maintenance_now(State(state)).await;
        assert_eq!(body["arr_ran"], json!(false));
        assert_eq!(body["embed_ran"], json!(false));
        assert!(body["accounts_considered"].is_number());
    }
}
