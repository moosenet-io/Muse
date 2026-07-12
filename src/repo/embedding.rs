//! Repo functions for `embeddings` — pgvector recall (spec §3.4).

use pgvector::Vector;
use sqlx::PgPool;

use crate::error::{MuseError, MuseResult};
use crate::models::embedding::{Embedding, EmbeddingMatch, NewEmbedding};

/// Upsert keyed by `(entity_kind, entity_id, model)` — a re-embed with the
/// same model replaces the vector in place; a different model gets its own
/// row (so re-embeds/model migrations are detectable and both are queryable
/// during a transition, per the MUSE-03 build note).
pub async fn upsert(pool: &PgPool, new: &NewEmbedding) -> MuseResult<Embedding> {
    sqlx::query_as::<_, Embedding>(
        r#"
        INSERT INTO embeddings (entity_kind, entity_id, model, dim, embedding, source_text)
        VALUES ($1, $2, $3, $4, $5, $6)
        ON CONFLICT (entity_kind, entity_id, model) DO UPDATE SET
            dim = EXCLUDED.dim,
            embedding = EXCLUDED.embedding,
            source_text = EXCLUDED.source_text,
            embedded_at = now()
        RETURNING *
        "#,
    )
    .bind(new.entity_kind.as_str())
    .bind(new.entity_id)
    .bind(&new.model)
    .bind(new.dim)
    .bind(&new.embedding)
    .bind(&new.source_text)
    .fetch_one(pool)
    .await
    .map_err(MuseError::Database)
}

pub async fn get(pool: &PgPool, entity_kind: &str, entity_id: i64, model: &str) -> MuseResult<Option<Embedding>> {
    sqlx::query_as::<_, Embedding>(
        "SELECT * FROM embeddings WHERE entity_kind = $1 AND entity_id = $2 AND model = $3",
    )
    .bind(entity_kind)
    .bind(entity_id)
    .bind(model)
    .fetch_optional(pool)
    .await
    .map_err(MuseError::Database)
}

/// Batch fetch of embeddings for a set of entity ids under one model — the
/// MUSE-10 taste-centroid computation's lookup primitive (avoids an N+1 of
/// [`get`] calls when averaging over dozens/hundreds of finished titles). An
/// id with no stored embedding simply doesn't appear in the result — callers
/// that need to "skip cleanly" for a missing embedding (per the MUSE-10
/// build brief) do so by checking which requested ids are absent.
pub async fn get_many(
    pool: &PgPool,
    entity_kind: &str,
    model: &str,
    entity_ids: &[i64],
) -> MuseResult<Vec<Embedding>> {
    sqlx::query_as::<_, Embedding>(
        "SELECT * FROM embeddings WHERE entity_kind = $1 AND model = $2 AND entity_id = ANY($3)",
    )
    .bind(entity_kind)
    .bind(model)
    .bind(entity_ids)
    .fetch_all(pool)
    .await
    .map_err(MuseError::Database)
}

/// Cosine-distance nearest neighbors within an entity kind, for a given
/// model (never mixes vectors from different embedding models/dims).
pub async fn nearest(
    pool: &PgPool,
    entity_kind: &str,
    model: &str,
    query: &Vector,
    limit: i64,
) -> MuseResult<Vec<EmbeddingMatch>> {
    sqlx::query_as::<_, EmbeddingMatch>(
        r#"
        SELECT entity_kind, entity_id, (embedding <=> $3) AS distance
        FROM embeddings
        WHERE entity_kind = $1 AND model = $2
        ORDER BY embedding <=> $3
        LIMIT $4
        "#,
    )
    .bind(entity_kind)
    .bind(model)
    .bind(query.clone())
    .bind(limit)
    .fetch_all(pool)
    .await
    .map_err(MuseError::Database)
}

pub async fn delete(pool: &PgPool, entity_kind: &str, entity_id: i64, model: &str) -> MuseResult<()> {
    sqlx::query("DELETE FROM embeddings WHERE entity_kind = $1 AND entity_id = $2 AND model = $3")
        .bind(entity_kind)
        .bind(entity_id)
        .bind(model)
        .execute(pool)
        .await
        .map_err(MuseError::Database)?;
    Ok(())
}
