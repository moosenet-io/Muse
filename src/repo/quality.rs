//! Repo functions for quality definitions/profiles and the custom-format
//! scorer seam (blueprint §2/§6/§7.4/§7.5).

use sqlx::PgPool;

use crate::error::{MuseError, MuseResult};
use crate::models::quality::{
    CustomFormat, NewCustomFormat, NewQualityDefinition, NewQualityProfile, QualityDefinition,
    QualityProfile, QualityProfileFormat,
};

pub async fn create_definition(
    pool: &PgPool,
    new: &NewQualityDefinition,
) -> MuseResult<QualityDefinition> {
    sqlx::query_as::<_, QualityDefinition>(
        r#"
        INSERT INTO quality_definitions (quality_key, title, source, resolution, modifier, sort_order)
        VALUES ($1, $2, $3, $4, $5, $6)
        ON CONFLICT (quality_key) DO UPDATE SET
            title = EXCLUDED.title,
            source = EXCLUDED.source,
            resolution = EXCLUDED.resolution,
            modifier = EXCLUDED.modifier,
            sort_order = EXCLUDED.sort_order
        RETURNING *
        "#,
    )
    .bind(&new.quality_key)
    .bind(&new.title)
    .bind(&new.source)
    .bind(&new.resolution)
    .bind(&new.modifier)
    .bind(new.sort_order)
    .fetch_one(pool)
    .await
    .map_err(MuseError::Database)
}

pub async fn get_definition(pool: &PgPool, id: i64) -> MuseResult<QualityDefinition> {
    sqlx::query_as::<_, QualityDefinition>("SELECT * FROM quality_definitions WHERE id = $1")
        .bind(id)
        .fetch_optional(pool)
        .await
        .map_err(MuseError::Database)?
        .ok_or_else(|| MuseError::NotFound(format!("quality_definition {id} not found")))
}

pub async fn list_definitions(pool: &PgPool) -> MuseResult<Vec<QualityDefinition>> {
    sqlx::query_as::<_, QualityDefinition>("SELECT * FROM quality_definitions ORDER BY sort_order")
        .fetch_all(pool)
        .await
        .map_err(MuseError::Database)
}

pub async fn create_profile(pool: &PgPool, new: &NewQualityProfile) -> MuseResult<QualityProfile> {
    sqlx::query_as::<_, QualityProfile>(
        r#"
        INSERT INTO quality_profiles (name, cutoff_quality_id, items, upgrade_allowed, natural_language_intent)
        VALUES ($1, $2, $3, $4, $5)
        ON CONFLICT (name) DO UPDATE SET
            cutoff_quality_id = EXCLUDED.cutoff_quality_id,
            items = EXCLUDED.items,
            upgrade_allowed = EXCLUDED.upgrade_allowed,
            natural_language_intent = EXCLUDED.natural_language_intent,
            updated_at = now()
        RETURNING *
        "#,
    )
    .bind(&new.name)
    .bind(new.cutoff_quality_id)
    .bind(&new.items)
    .bind(new.upgrade_allowed)
    .bind(&new.natural_language_intent)
    .fetch_one(pool)
    .await
    .map_err(MuseError::Database)
}

pub async fn get_profile(pool: &PgPool, id: i64) -> MuseResult<QualityProfile> {
    sqlx::query_as::<_, QualityProfile>("SELECT * FROM quality_profiles WHERE id = $1")
        .bind(id)
        .fetch_optional(pool)
        .await
        .map_err(MuseError::Database)?
        .ok_or_else(|| MuseError::NotFound(format!("quality_profile {id} not found")))
}

pub async fn list_profiles(pool: &PgPool) -> MuseResult<Vec<QualityProfile>> {
    sqlx::query_as::<_, QualityProfile>("SELECT * FROM quality_profiles ORDER BY name")
        .fetch_all(pool)
        .await
        .map_err(MuseError::Database)
}

pub async fn create_custom_format(pool: &PgPool, new: &NewCustomFormat) -> MuseResult<CustomFormat> {
    sqlx::query_as::<_, CustomFormat>(
        r#"
        INSERT INTO custom_formats (name, specifications, include_when_renaming)
        VALUES ($1, $2, $3)
        ON CONFLICT (name) DO UPDATE SET
            specifications = EXCLUDED.specifications,
            include_when_renaming = EXCLUDED.include_when_renaming,
            updated_at = now()
        RETURNING *
        "#,
    )
    .bind(&new.name)
    .bind(&new.specifications)
    .bind(new.include_when_renaming)
    .fetch_one(pool)
    .await
    .map_err(MuseError::Database)
}

pub async fn list_custom_formats(pool: &PgPool) -> MuseResult<Vec<CustomFormat>> {
    sqlx::query_as::<_, CustomFormat>("SELECT * FROM custom_formats ORDER BY name")
        .fetch_all(pool)
        .await
        .map_err(MuseError::Database)
}

/// Set (upsert) a custom format's score within a quality profile's
/// FormatItems-equivalent table. Score evaluation itself (matching a parsed
/// release against `custom_formats.specifications` and summing scores) is
/// out of scope for MUSE-02 — this only persists the score table.
pub async fn set_profile_format_score(
    pool: &PgPool,
    quality_profile_id: i64,
    custom_format_id: i64,
    score: i32,
) -> MuseResult<QualityProfileFormat> {
    sqlx::query_as::<_, QualityProfileFormat>(
        r#"
        INSERT INTO quality_profile_formats (quality_profile_id, custom_format_id, score)
        VALUES ($1, $2, $3)
        ON CONFLICT (quality_profile_id, custom_format_id) DO UPDATE SET score = EXCLUDED.score
        RETURNING *
        "#,
    )
    .bind(quality_profile_id)
    .bind(custom_format_id)
    .bind(score)
    .fetch_one(pool)
    .await
    .map_err(MuseError::Database)
}

pub async fn list_profile_format_scores(
    pool: &PgPool,
    quality_profile_id: i64,
) -> MuseResult<Vec<QualityProfileFormat>> {
    sqlx::query_as::<_, QualityProfileFormat>(
        "SELECT * FROM quality_profile_formats WHERE quality_profile_id = $1",
    )
    .bind(quality_profile_id)
    .fetch_all(pool)
    .await
    .map_err(MuseError::Database)
}
