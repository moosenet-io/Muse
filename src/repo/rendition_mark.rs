//! FOUNDRY-26: persistence for Path B's marks.
//!
//! Every query here is scoped to LIVE marks (`revoked_at IS NULL`). There is
//! deliberately no function that returns "all media" or "all titles" — the
//! rendition run must be unable to obtain a candidate that nobody marked, and
//! that is enforced by this module simply not offering one.

use sqlx::PgPool;

use crate::error::{MuseError, MuseResult};
use crate::foundry::marks::{MarkScope, RenditionMark};
use crate::foundry::rendition::RenditionName;

/// Create or replace the live mark for a path.
///
/// Re-marking a path REPLACES rather than stacking: two live marks on one path
/// would each expand to the same files, so the run would encode everything
/// twice and the operator would see doubled counts with no explanation.
pub async fn upsert(
    pool: &PgPool,
    scope: MarkScope,
    path: &str,
    rungs: &[RenditionName],
    marked_by: Option<&str>,
) -> MuseResult<i64> {
    let rung_strs: Vec<String> = rungs.iter().map(|r| r.as_str().to_string()).collect();
    // Revoke first, then insert: the partial unique index allows any number of
    // REVOKED rows for a path but only one live one, so this is the shape that
    // keeps the history while replacing the live mark.
    sqlx::query("UPDATE rendition_marks SET revoked_at = now() WHERE path = $1 AND revoked_at IS NULL")
        .bind(path)
        .execute(pool)
        .await
        .map_err(MuseError::Database)?;
    let row: (i64,) = sqlx::query_as(
        "INSERT INTO rendition_marks (scope, path, rungs, marked_by) \
         VALUES ($1, $2, $3, $4) RETURNING id",
    )
    .bind(scope.as_str())
    .bind(path)
    .bind(&rung_strs)
    .bind(marked_by)
    .fetch_one(pool)
    .await
    .map_err(MuseError::Database)?;
    Ok(row.0)
}

/// Revoke the live mark for a path. Returns whether one was there.
pub async fn revoke(pool: &PgPool, path: &str) -> MuseResult<bool> {
    let r = sqlx::query(
        "UPDATE rendition_marks SET revoked_at = now() WHERE path = $1 AND revoked_at IS NULL",
    )
    .bind(path)
    .execute(pool)
    .await
    .map_err(MuseError::Database)?;
    Ok(r.rows_affected() > 0)
}

/// Every LIVE mark.
///
/// The ONLY way a rendition run obtains candidates. A row whose scope or rung
/// cannot be parsed is SKIPPED and reported by the caller rather than guessed
/// at — the database constrains both, so an unparseable row means the schema
/// and the code have diverged, which must not be papered over.
pub async fn live(pool: &PgPool) -> MuseResult<(Vec<RenditionMark>, Vec<String>)> {
    let rows: Vec<(i64, String, String, Vec<String>)> = sqlx::query_as(
        "SELECT id, scope, path, rungs FROM rendition_marks \
         WHERE revoked_at IS NULL ORDER BY created_at",
    )
    .fetch_all(pool)
    .await
    .map_err(MuseError::Database)?;

    let mut out = Vec::new();
    let mut unparseable = Vec::new();
    for (id, scope, path, rungs) in rows {
        let Some(scope) = MarkScope::parse(&scope) else {
            unparseable.push(format!("mark {id} has scope `{scope}`, which this build does not know"));
            continue;
        };
        let parsed: Vec<RenditionName> = rungs.iter().filter_map(|r| RenditionName::parse(r)).collect();
        if parsed.len() != rungs.len() {
            unparseable.push(format!(
                "mark {id} names rungs {rungs:?}, some of which this build does not know"
            ));
            continue;
        }
        out.push(RenditionMark { id, scope, path, rungs: parsed });
    }
    Ok((out, unparseable))
}
