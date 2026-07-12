//! Repo functions for `interstitials` (MUSE-23).

use sqlx::PgPool;

use crate::error::{MuseError, MuseResult};
use crate::models::interstitial::{Interstitial, InterstitialKind, NewInterstitial};

pub async fn upsert(pool: &PgPool, new: &NewInterstitial) -> MuseResult<Interstitial> {
    sqlx::query_as::<_, Interstitial>(
        r#"
        INSERT INTO interstitials (
            plex_rating_key, kind, title, decade, theme, genre, mood, duration_ms, tags, source
        )
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
        ON CONFLICT (plex_rating_key) DO UPDATE SET
            kind = EXCLUDED.kind,
            title = EXCLUDED.title,
            decade = EXCLUDED.decade,
            theme = EXCLUDED.theme,
            genre = EXCLUDED.genre,
            mood = EXCLUDED.mood,
            duration_ms = EXCLUDED.duration_ms,
            tags = EXCLUDED.tags,
            source = EXCLUDED.source,
            updated_at = now()
        RETURNING *
        "#,
    )
    .bind(&new.plex_rating_key)
    .bind(new.kind)
    .bind(&new.title)
    .bind(new.decade)
    .bind(&new.theme)
    .bind(&new.genre)
    .bind(&new.mood)
    .bind(new.duration_ms)
    .bind(&new.tags)
    .bind(&new.source)
    .fetch_one(pool)
    .await
    .map_err(MuseError::Database)
}

pub async fn get(pool: &PgPool, id: i64) -> MuseResult<Interstitial> {
    sqlx::query_as::<_, Interstitial>("SELECT * FROM interstitials WHERE id = $1")
        .bind(id)
        .fetch_optional(pool)
        .await
        .map_err(MuseError::Database)?
        .ok_or_else(|| MuseError::NotFound(format!("interstitial {id} not found")))
}

/// Themed selection for the composer: filter by kind/decade/theme, any of
/// which may be omitted (`None` matches any value for that dimension).
pub async fn list_by_kind_decade_theme(
    pool: &PgPool,
    kind: Option<InterstitialKind>,
    decade: Option<i32>,
    theme: Option<&str>,
) -> MuseResult<Vec<Interstitial>> {
    sqlx::query_as::<_, Interstitial>(
        r#"
        SELECT * FROM interstitials
        WHERE ($1::interstitial_kind IS NULL OR kind = $1)
          AND ($2::int IS NULL OR decade = $2)
          AND ($3::text IS NULL OR theme = $3)
        ORDER BY id
        "#,
    )
    .bind(kind)
    .bind(decade)
    .bind(theme)
    .fetch_all(pool)
    .await
    .map_err(MuseError::Database)
}

/// MUSE-29: resolve an interstitial to its on-disk `file_path`
/// (migrations/0098), if one has been populated. Selects the single column
/// directly (rather than reusing [`get`]'s `SELECT *`/`Interstitial` model)
/// so callers on the streaming hot path never pull the taxonomy columns
/// they don't need. Returns `Ok(None)` both when the interstitial doesn't
/// exist and when it exists but has no `file_path` yet — either way the
/// caller's only correct move is to skip that program, so the two cases are
/// deliberately not distinguished here.
pub async fn get_file_path(pool: &PgPool, id: i64) -> MuseResult<Option<String>> {
    let row: Option<(Option<String>,)> =
        sqlx::query_as("SELECT file_path FROM interstitials WHERE id = $1")
            .bind(id)
            .fetch_optional(pool)
            .await
            .map_err(MuseError::Database)?;
    Ok(row.and_then(|(path,)| path))
}

pub async fn list_by_tag(pool: &PgPool, tag: &str) -> MuseResult<Vec<Interstitial>> {
    sqlx::query_as::<_, Interstitial>(
        "SELECT * FROM interstitials WHERE $1 = ANY(tags) ORDER BY id",
    )
    .bind(tag)
    .fetch_all(pool)
    .await
    .map_err(MuseError::Database)
}
