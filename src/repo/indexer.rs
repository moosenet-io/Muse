//! Repo functions for `indexers`.

use sqlx::PgPool;

use crate::error::{MuseError, MuseResult};
use crate::models::indexer::{Indexer, NewIndexer};

/// Upsert an indexer keyed by `prowlarr_id` (MUSE-16 §4b-A: indexer sync).
pub async fn upsert(pool: &PgPool, new: &NewIndexer) -> MuseResult<Indexer> {
    sqlx::query_as::<_, Indexer>(
        r#"
        INSERT INTO indexers (
            prowlarr_id, name, protocol, privacy, enabled, categories, polite_min_interval_secs
        )
        VALUES ($1, $2, $3, $4, $5, $6, $7)
        ON CONFLICT (prowlarr_id) DO UPDATE SET
            name = EXCLUDED.name,
            protocol = EXCLUDED.protocol,
            privacy = EXCLUDED.privacy,
            enabled = EXCLUDED.enabled,
            categories = EXCLUDED.categories,
            polite_min_interval_secs = EXCLUDED.polite_min_interval_secs,
            updated_at = now()
        RETURNING *
        "#,
    )
    .bind(new.prowlarr_id)
    .bind(&new.name)
    .bind(&new.protocol)
    .bind(&new.privacy)
    .bind(new.enabled)
    .bind(&new.categories)
    .bind(new.polite_min_interval_secs)
    .fetch_one(pool)
    .await
    .map_err(MuseError::Database)
}

pub async fn get(pool: &PgPool, id: i64) -> MuseResult<Indexer> {
    sqlx::query_as::<_, Indexer>("SELECT * FROM indexers WHERE id = $1")
        .bind(id)
        .fetch_optional(pool)
        .await
        .map_err(MuseError::Database)?
        .ok_or_else(|| MuseError::NotFound(format!("indexer {id} not found")))
}

pub async fn list_enabled(pool: &PgPool) -> MuseResult<Vec<Indexer>> {
    sqlx::query_as::<_, Indexer>("SELECT * FROM indexers WHERE enabled ORDER BY name")
        .fetch_all(pool)
        .await
        .map_err(MuseError::Database)
}

/// Record that a report-pull (§4b-B) just happened for this indexer, so the
/// scheduler's next-eligible-time math has a durable anchor independent of
/// the in-process rate limiter (which resets on restart).
pub async fn mark_rss_pulled(pool: &PgPool, id: i64) -> MuseResult<()> {
    sqlx::query("UPDATE indexers SET last_rss_pull_at = now(), updated_at = now() WHERE id = $1")
        .bind(id)
        .execute(pool)
        .await
        .map_err(MuseError::Database)?;
    Ok(())
}
