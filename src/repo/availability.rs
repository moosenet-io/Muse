//! Repo functions for `availability` — the per-title "grabbable now" rollup
//! (MUSE-16 §4b-D).

use sqlx::PgPool;

use crate::error::{MuseError, MuseResult};
use crate::models::availability::Availability;

pub async fn get(pool: &PgPool, media_metadata_id: i64) -> MuseResult<Availability> {
    sqlx::query_as::<_, Availability>("SELECT * FROM availability WHERE media_metadata_id = $1")
        .bind(media_metadata_id)
        .fetch_optional(pool)
        .await
        .map_err(MuseError::Database)?
        .ok_or_else(|| MuseError::NotFound(format!("availability for media_metadata {media_metadata_id} not found")))
}

/// Recompute the rollup for one title directly from its current `releases`
/// rows — the rollup worker's core statement (§4b-D). Safe to call
/// repeatedly/idempotently; a title with zero releases still gets a
/// (empty) rollup row so callers can distinguish "checked, nothing found"
/// from "never checked".
pub async fn recompute(pool: &PgPool, media_metadata_id: i64) -> MuseResult<Availability> {
    sqlx::query_as::<_, Availability>(
        r#"
        INSERT INTO availability (
            media_metadata_id, best_quality, best_seeders, release_count,
            has_freeleech, cheapest_size_bytes, newest_release_at, computed_at
        )
        SELECT
            $1,
            (SELECT quality FROM releases
                WHERE media_metadata_id = $1
                ORDER BY seeders DESC NULLS LAST, size_bytes DESC NULLS LAST
                LIMIT 1),
            (SELECT MAX(seeders) FROM releases WHERE media_metadata_id = $1),
            (SELECT COUNT(*) FROM releases WHERE media_metadata_id = $1),
            (SELECT COALESCE(BOOL_OR(freeleech), false) FROM releases WHERE media_metadata_id = $1),
            (SELECT MIN(size_bytes) FROM releases WHERE media_metadata_id = $1),
            (SELECT MAX(publish_date) FROM releases WHERE media_metadata_id = $1),
            now()
        ON CONFLICT (media_metadata_id) DO UPDATE SET
            best_quality = EXCLUDED.best_quality,
            best_seeders = EXCLUDED.best_seeders,
            release_count = EXCLUDED.release_count,
            has_freeleech = EXCLUDED.has_freeleech,
            cheapest_size_bytes = EXCLUDED.cheapest_size_bytes,
            newest_release_at = EXCLUDED.newest_release_at,
            computed_at = now()
        RETURNING *
        "#,
    )
    .bind(media_metadata_id)
    .fetch_one(pool)
    .await
    .map_err(MuseError::Database)
}
