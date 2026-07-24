//! S125 one-shot migration helpers the ORCHESTRATOR runs to move the
//! `embeddings` table (and the derived taste/persona centroids) from
//! nomic-embed-text(768) to qwen3-embedding(1024) through Chord.
//!
//! These are NOT called by any background worker — they are deliberate,
//! operator/orchestrator-driven steps that bracket the two schema
//! migrations. The exact ordered sequence is:
//!
//! 1. **migration `0106_embedding_1024.sql`** — adds `embeddings.embedding_1024`
//!    (`vector(1024)`, nullable) + `embeddings.model_1024` (text, nullable),
//!    and widens every derived centroid column to `vector(1024)` (their old
//!    768 values are discarded; they are recomputed in step 4, never
//!    re-embedded).
//! 2. **[`backfill_1024`]** (this module) — re-embeds every row's
//!    `source_text` through Chord and writes `embedding_1024`/`model_1024`.
//!    Rows with `source_text IS NULL` CANNOT be reproduced and are left
//!    untouched (see [`count_unbackfillable`] — the count to watch); the
//!    cutover migration deletes them.
//! 3. **migration `0107_embedding_1024_cutover.sql`** — deletes any row still
//!    missing `embedding_1024`, promotes `embedding_1024` → `embedding`,
//!    `model_1024` → `model`, sets `dim = 1024`, and rebuilds the HNSW
//!    index. Run ONLY after [`count_pending_backfill`] returns 0.
//! 4. **[`recompute_all_centroids`]** (this module) — now that `embedding`
//!    is the 1024 space, recompute per-account taste centroids
//!    (`taste_profile.overall_centroid` + `taste_context_centroids.centroid`)
//!    and re-derive persona centroids (`personas.centroid`). Guarded so it
//!    REFUSES to run on a table not fully cut over (see
//!    [`recompute_all_centroids`]'s guard) — never recompute centroids on a
//!    partially-backfilled/half-migrated table.
//!
//! `population_profile.mainstream_centroid` is widened by migration 1 but has
//! no recompute step here: no code in the crate ever populates it (MUSE-20's
//! mainstream-centroid math was never implemented), so it stays NULL exactly
//! as before, just at the new width.

use pgvector::Vector;
use sqlx::PgPool;

use crate::error::{MuseError, MuseResult};
use crate::models::embedding::DEFAULT_EMBEDDING_MODEL;
use crate::models::persona::{NewPersona, PERSONA_KIND_DERIVED};
use crate::repo;
use crate::taste_model::chord_client::ChordClient;

use super::chord::ChordEmbedClient;

/// How many `embeddings` rows to pull + re-embed per page. Kept modest so a
/// large library doesn't turn into one giant burst against the shared Chord
/// GPU serve (same VRAM-politeness spirit as the incremental embed pipeline).
const BACKFILL_PAGE_SIZE: i64 = 50;

/// Tally of one [`backfill_1024`] run.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BackfillSummary {
    /// Rows re-embedded through Chord and written to `embedding_1024`.
    pub embedded: usize,
    /// Rows whose Chord re-embed or write failed (logged, not fatal — the
    /// row keeps `embedding_1024 = NULL` and a later run retries it).
    pub failed: usize,
    /// Rows that can NEVER be backfilled because `source_text IS NULL` — the
    /// cutover migration drops these. Surfaced so the operator can eyeball
    /// how many 768 rows are about to be lost (they'll re-embed naturally on
    /// the next incremental pass once their `source_text` is recomposed).
    pub unbackfillable: i64,
}

/// Count rows that still need a 1024 backfill and CAN be backfilled
/// (`source_text` present). The cutover migration must not run until this
/// is 0.
pub async fn count_pending_backfill(pool: &PgPool) -> MuseResult<i64> {
    sqlx::query_scalar::<_, i64>(
        "SELECT count(*) FROM embeddings WHERE embedding_1024 IS NULL AND source_text IS NOT NULL",
    )
    .fetch_one(pool)
    .await
    .map_err(MuseError::Database)
}

/// Count rows that can NEVER be backfilled through Chord because they have no
/// `source_text` to re-embed — the rows the cutover migration will delete.
pub async fn count_unbackfillable(pool: &PgPool) -> MuseResult<i64> {
    sqlx::query_scalar::<_, i64>(
        "SELECT count(*) FROM embeddings WHERE embedding_1024 IS NULL AND source_text IS NULL",
    )
    .fetch_one(pool)
    .await
    .map_err(MuseError::Database)
}

#[derive(Debug, sqlx::FromRow)]
struct BackfillRow {
    id: i64,
    source_text: String,
}

/// Re-embed every backfillable `embeddings` row's `source_text` through Chord
/// (`qwen3-embedding`, 1024) and write the result into `embedding_1024` /
/// `model_1024`.
///
/// **Per-row resilient (never loses ground on a bad row):** pages by a
/// KEYSET cursor on ascending `id` (`... AND id > $last_id`), advancing the
/// cursor past every row it attempts — success OR failure. A row whose Chord
/// re-embed or write fails is logged, counted, and left `NULL` (so a later
/// run — which resets the cursor and re-selects any still-`NULL` row —
/// retries it); it does NOT abort the page and, crucially, does NOT block the
/// rows behind it (the old "always page from the top" approach let a cluster
/// of persistently-failing rows starve everything after them). Only truly
/// unbackfillable rows (`source_text IS NULL`) are expected to remain `NULL`
/// after a clean run.
///
/// Idempotent + resumable across calls. Runs BETWEEN migration `0106` (which
/// adds `embedding_1024`) and migration `0107` (the cutover) — see the module
/// docs. The `0107` cutover is guarded to REFUSE to run while any backfillable
/// row is still `NULL`, so a partial/failed backfill can never lead to data
/// loss.
pub async fn backfill_1024(pool: &PgPool, client: &ChordEmbedClient) -> MuseResult<BackfillSummary> {
    let mut summary = BackfillSummary::default();
    let mut last_id: i64 = 0;

    loop {
        let rows = sqlx::query_as::<_, BackfillRow>(
            r#"
            SELECT id, source_text
            FROM embeddings
            WHERE embedding_1024 IS NULL AND source_text IS NOT NULL AND id > $1
            ORDER BY id
            LIMIT $2
            "#,
        )
        .bind(last_id)
        .bind(BACKFILL_PAGE_SIZE)
        .fetch_all(pool)
        .await
        .map_err(MuseError::Database)?;

        if rows.is_empty() {
            break;
        }

        for row in &rows {
            // Advance the cursor for EVERY attempted row (success or failure)
            // so a failing row never blocks the rows after it, and a page can
            // never re-select rows we already tried this run.
            last_id = row.id;

            match client.embed(DEFAULT_EMBEDDING_MODEL, &row.source_text).await {
                Ok(vector) => match write_embedding_1024(pool, row.id, vector).await {
                    Ok(()) => summary.embedded += 1,
                    Err(e) => {
                        summary.failed += 1;
                        tracing::warn!(embedding_id = row.id, error = %e, "S125 backfill: failed to write embedding_1024; leaving NULL for retry, continuing");
                    }
                },
                Err(e) => {
                    summary.failed += 1;
                    tracing::warn!(embedding_id = row.id, error = %e, "S125 backfill: Chord re-embed failed; leaving NULL for retry, continuing");
                }
            }
        }
    }

    summary.unbackfillable = count_unbackfillable(pool).await?;
    tracing::info!(
        embedded = summary.embedded,
        failed = summary.failed,
        unbackfillable = summary.unbackfillable,
        "S125 backfill_1024 complete"
    );
    Ok(summary)
}

async fn write_embedding_1024(pool: &PgPool, id: i64, vector: Vec<f32>) -> MuseResult<()> {
    sqlx::query("UPDATE embeddings SET embedding_1024 = $1, model_1024 = $2 WHERE id = $3")
        .bind(Vector::from(vector))
        .bind(DEFAULT_EMBEDDING_MODEL)
        .bind(id)
        .execute(pool)
        .await
        .map_err(MuseError::Database)?;
    Ok(())
}

/// Tally of one [`recompute_all_centroids`] run.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CentroidRecomputeSummary {
    pub accounts_considered: usize,
    pub taste_recomputed: usize,
    pub taste_failed: usize,
    pub personas_upserted: usize,
    pub personas_failed: usize,
}

/// Recompute every DERIVED centroid off the now-1024 `embeddings` space,
/// AFTER the cutover migration. Covers the two centroid families that code
/// actually populates:
/// - `taste_profile.overall_centroid` + `taste_context_centroids.centroid`
///   (via [`crate::taste_model::recompute_taste`], per account), and
/// - `personas.centroid` (re-derive context-cluster personas per account and
///   upsert them).
///
/// **Guard (never recompute on a partially-backfilled table):** refuses to
/// run while ANY `embeddings` row is not yet at `dim = 1024` — i.e. the
/// cutover migration `0107` hasn't completed. Recomputing before then would
/// average old 768 vectors and try to store them in the widened 1024 centroid
/// columns, a dimension-mismatch error at best and silent corruption at
/// worst.
///
/// `chord` is the chat client used only for the optional `model_notes` prose
/// summary inside `recompute_taste`; `None` is fine (only the prose degrades).
/// Per-account failures are logged and skipped, never fatal to the run.
pub async fn recompute_all_centroids(
    pool: &PgPool,
    chord: Option<&ChordClient>,
) -> MuseResult<CentroidRecomputeSummary> {
    let stale = sqlx::query_scalar::<_, i64>("SELECT count(*) FROM embeddings WHERE dim <> 1024")
        .fetch_one(pool)
        .await
        .map_err(MuseError::Database)?;
    if stale > 0 {
        return Err(MuseError::Config(format!(
            "S125 recompute_all_centroids refused: {stale} embeddings row(s) are not dim=1024 yet — \
             run the 0107 cutover migration (and finish the backfill) before recomputing centroids"
        )));
    }

    let mut summary = CentroidRecomputeSummary::default();

    let accounts = repo::account::list(pool).await?;
    summary.accounts_considered = accounts.len();

    for account in &accounts {
        // Taste centroids (overall + per-context) — recompute_taste is the
        // single idempotent per-account entry point.
        match crate::taste_model::recompute_taste(pool, chord, account.id).await {
            Ok(_) => summary.taste_recomputed += 1,
            Err(e) => {
                summary.taste_failed += 1;
                tracing::warn!(account_id = account.id, error = %e, "S125 recompute: recompute_taste failed; continuing");
            }
        }

        // Persona centroids — re-derive the context-cluster personas and
        // upsert them (idempotent by (account_id, name, kind)). Explicit,
        // operator-declared personas (if any exist) are NOT re-derived here;
        // their source media ids live in `defining_signals` for a manual
        // re-derivation, but no crate code path creates them in production.
        match crate::persona::derive::derive_context_cluster_personas(pool, account.id).await {
            Ok(personas) => {
                for derived in personas {
                    let new_persona = NewPersona {
                        account_id: Some(account.id),
                        name: derived.name,
                        kind: PERSONA_KIND_DERIVED.to_string(),
                        centroid: derived.centroid,
                        defining_signals: derived.defining_signals,
                        metadata: serde_json::json!({}),
                        sample_size: derived.sample_size,
                    };
                    match repo::persona::upsert_for_account(pool, &new_persona).await {
                        Ok(_) => summary.personas_upserted += 1,
                        Err(e) => {
                            summary.personas_failed += 1;
                            tracing::warn!(account_id = account.id, error = %e, "S125 recompute: persona upsert failed; continuing");
                        }
                    }
                }
            }
            Err(e) => {
                summary.personas_failed += 1;
                tracing::warn!(account_id = account.id, error = %e, "S125 recompute: persona derivation failed; continuing");
            }
        }
    }

    tracing::info!(
        accounts_considered = summary.accounts_considered,
        taste_recomputed = summary.taste_recomputed,
        personas_upserted = summary.personas_upserted,
        "S125 recompute_all_centroids complete"
    );
    Ok(summary)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backfill_summary_default_is_all_zero() {
        let s = BackfillSummary::default();
        assert_eq!(s.embedded, 0);
        assert_eq!(s.failed, 0);
        assert_eq!(s.unbackfillable, 0);
    }

    #[test]
    fn centroid_summary_default_is_all_zero() {
        let s = CentroidRecomputeSummary::default();
        assert_eq!(s.accounts_considered, 0);
        assert_eq!(s.taste_recomputed, 0);
        assert_eq!(s.personas_upserted, 0);
    }
}
