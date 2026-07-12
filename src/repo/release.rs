//! Repo functions for `releases`.

use sqlx::PgPool;

use crate::error::{MuseError, MuseResult};
use crate::models::release::{NewRelease, Release};

/// Upsert a release keyed by `(indexer_id, guid)` (MUSE-16 §4b-B/C: report
/// pull + targeted search both funnel here). A re-seen release refreshes its
/// health/parse columns and `last_seen_at`, but never overwrites
/// `first_seen_at`.
pub async fn upsert(pool: &PgPool, new: &NewRelease) -> MuseResult<Release> {
    sqlx::query_as::<_, Release>(
        r#"
        INSERT INTO releases (
            media_metadata_id, episode_id, indexer_id, guid, title, info_url,
            download_url, info_hash, size_bytes, publish_date, seeders,
            leechers, grabs, freeleech, freeleech_pct, categories,
            parsed_title, parsed_year, quality, resolution, source,
            video_codec, audio_codec, audio_channels, hdr, edition,
            release_group, proper_repack, languages, subtitles,
            parse_confidence, expires_at
        )
        VALUES (
            $1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15,
            $16, $17, $18, $19, $20, $21, $22, $23, $24, $25, $26, $27, $28,
            $29, $30, $31, $32
        )
        ON CONFLICT (indexer_id, guid) DO UPDATE SET
            media_metadata_id = COALESCE(EXCLUDED.media_metadata_id, releases.media_metadata_id),
            episode_id = COALESCE(EXCLUDED.episode_id, releases.episode_id),
            title = EXCLUDED.title,
            info_url = EXCLUDED.info_url,
            download_url = EXCLUDED.download_url,
            info_hash = EXCLUDED.info_hash,
            size_bytes = EXCLUDED.size_bytes,
            publish_date = EXCLUDED.publish_date,
            seeders = EXCLUDED.seeders,
            leechers = EXCLUDED.leechers,
            grabs = EXCLUDED.grabs,
            freeleech = EXCLUDED.freeleech,
            freeleech_pct = EXCLUDED.freeleech_pct,
            categories = EXCLUDED.categories,
            parsed_title = EXCLUDED.parsed_title,
            parsed_year = EXCLUDED.parsed_year,
            quality = EXCLUDED.quality,
            resolution = EXCLUDED.resolution,
            source = EXCLUDED.source,
            video_codec = EXCLUDED.video_codec,
            audio_codec = EXCLUDED.audio_codec,
            audio_channels = EXCLUDED.audio_channels,
            hdr = EXCLUDED.hdr,
            edition = EXCLUDED.edition,
            release_group = EXCLUDED.release_group,
            proper_repack = EXCLUDED.proper_repack,
            languages = EXCLUDED.languages,
            subtitles = EXCLUDED.subtitles,
            parse_confidence = EXCLUDED.parse_confidence,
            expires_at = EXCLUDED.expires_at,
            last_seen_at = now()
        RETURNING *
        "#,
    )
    .bind(new.media_metadata_id)
    .bind(new.episode_id)
    .bind(new.indexer_id)
    .bind(&new.guid)
    .bind(&new.title)
    .bind(&new.info_url)
    .bind(&new.download_url)
    .bind(&new.info_hash)
    .bind(new.size_bytes)
    .bind(new.publish_date)
    .bind(new.seeders)
    .bind(new.leechers)
    .bind(new.grabs)
    .bind(new.freeleech)
    .bind(new.freeleech_pct)
    .bind(&new.categories)
    .bind(&new.parsed_title)
    .bind(new.parsed_year)
    .bind(&new.quality)
    .bind(&new.resolution)
    .bind(&new.source)
    .bind(&new.video_codec)
    .bind(&new.audio_codec)
    .bind(new.audio_channels)
    .bind(&new.hdr)
    .bind(&new.edition)
    .bind(&new.release_group)
    .bind(new.proper_repack)
    .bind(&new.languages)
    .bind(&new.subtitles)
    .bind(new.parse_confidence)
    .bind(new.expires_at)
    .fetch_one(pool)
    .await
    .map_err(MuseError::Database)
}

pub async fn get(pool: &PgPool, id: i64) -> MuseResult<Release> {
    sqlx::query_as::<_, Release>("SELECT * FROM releases WHERE id = $1")
        .bind(id)
        .fetch_optional(pool)
        .await
        .map_err(MuseError::Database)?
        .ok_or_else(|| MuseError::NotFound(format!("release {id} not found")))
}

/// All currently-tracked releases resolved to a given title, freshest first
/// — the candidate corpus Phase-1 AI release-selection reasons over.
pub async fn list_by_media_metadata(pool: &PgPool, media_metadata_id: i64) -> MuseResult<Vec<Release>> {
    sqlx::query_as::<_, Release>(
        "SELECT * FROM releases WHERE media_metadata_id = $1 ORDER BY publish_date DESC NULLS LAST",
    )
    .bind(media_metadata_id)
    .fetch_all(pool)
    .await
    .map_err(MuseError::Database)
}

/// Releases seen by an indexer that haven't resolved to a title yet —
/// negative-space discovery + a target set for later re-matching.
pub async fn list_unresolved(pool: &PgPool, limit: i64) -> MuseResult<Vec<Release>> {
    sqlx::query_as::<_, Release>(
        r#"
        SELECT * FROM releases
        WHERE media_metadata_id IS NULL
        ORDER BY first_seen_at DESC
        LIMIT $1
        "#,
    )
    .bind(limit)
    .fetch_all(pool)
    .await
    .map_err(MuseError::Database)
}

/// Prune releases past their `expires_at` (rolling-snapshot hygiene per
/// §3.6: "expired rows are pruned"). Returns the number of rows removed.
pub async fn prune_expired(pool: &PgPool) -> MuseResult<u64> {
    let result = sqlx::query("DELETE FROM releases WHERE expires_at IS NOT NULL AND expires_at < now()")
        .execute(pool)
        .await
        .map_err(MuseError::Database)?;
    Ok(result.rows_affected())
}
