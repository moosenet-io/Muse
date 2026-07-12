//! Repo functions for `external_enrichment` — the Terminus-tool-suite
//! enrichment cache (spec §3.5).

use chrono::{DateTime, Utc};
use sqlx::PgPool;

use crate::error::{MuseError, MuseResult};
use crate::models::external_enrichment::{ExternalEnrichment, NewExternalEnrichment};

/// Upsert keyed by `(media_item_id, kind, source)` — a fresh fetch replaces
/// the cached payload/confidence/ttl rather than accumulating history (this
/// is a cache, not an audit log; see `taste_signals`/`play_events` for the
/// auditable tables).
pub async fn upsert(pool: &PgPool, new: &NewExternalEnrichment) -> MuseResult<ExternalEnrichment> {
    sqlx::query_as::<_, ExternalEnrichment>(
        r#"
        INSERT INTO external_enrichment (media_item_id, kind, source, payload, confidence, ttl_seconds)
        VALUES ($1, $2, $3, $4, $5, $6)
        ON CONFLICT (media_item_id, kind, source) DO UPDATE SET
            payload = EXCLUDED.payload,
            confidence = EXCLUDED.confidence,
            ttl_seconds = EXCLUDED.ttl_seconds,
            fetched_at = now()
        RETURNING *
        "#,
    )
    .bind(new.media_item_id)
    .bind(&new.kind)
    .bind(&new.source)
    .bind(&new.payload)
    .bind(new.confidence)
    .bind(new.ttl_seconds)
    .fetch_one(pool)
    .await
    .map_err(MuseError::Database)
}

pub async fn list_for_media_item(pool: &PgPool, media_item_id: i64) -> MuseResult<Vec<ExternalEnrichment>> {
    sqlx::query_as::<_, ExternalEnrichment>(
        "SELECT * FROM external_enrichment WHERE media_item_id = $1 ORDER BY fetched_at DESC",
    )
    .bind(media_item_id)
    .fetch_all(pool)
    .await
    .map_err(MuseError::Database)
}

/// Cache entries whose `ttl_seconds` has elapsed since `fetched_at` as of
/// `now` — the refresh worker's "needs re-fetch" query.
pub async fn list_expired(pool: &PgPool, now: DateTime<Utc>) -> MuseResult<Vec<ExternalEnrichment>> {
    sqlx::query_as::<_, ExternalEnrichment>(
        "SELECT * FROM external_enrichment WHERE fetched_at + (ttl_seconds * interval '1 second') <= $1",
    )
    .bind(now)
    .fetch_all(pool)
    .await
    .map_err(MuseError::Database)
}
