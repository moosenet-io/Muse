//! Repo functions for `media_metadata` — shared provider-keyed metadata.
//!
//! Two upsert entry points reflect the blueprint's provider-precedence
//! finding (§7.7): movies key primarily on `(kind, tmdb_id)` (Radarr), shows
//! key primarily on `(kind, tvdb_id)` (Sonarr).

use sqlx::PgPool;

use crate::error::{MuseError, MuseResult};
use crate::metadata::ProviderMetadata;
use crate::models::media_metadata::{MediaKind, MediaMetadata, NewMediaMetadata};

pub async fn upsert_by_tmdb(pool: &PgPool, new: &NewMediaMetadata) -> MuseResult<MediaMetadata> {
    let tmdb_id = new
        .tmdb_id
        .as_deref()
        .ok_or_else(|| MuseError::Conflict("upsert_by_tmdb requires tmdb_id".to_string()))?;

    sqlx::query_as::<_, MediaMetadata>(
        r#"
        INSERT INTO media_metadata (
            kind, tmdb_id, tvdb_id, imdb_id, provider_ids, title, sort_title,
            original_title, original_language, status, overview, studio,
            network, runtime_minutes, year, images
        )
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16)
        ON CONFLICT (kind, tmdb_id) DO UPDATE SET
            tvdb_id = EXCLUDED.tvdb_id,
            imdb_id = EXCLUDED.imdb_id,
            provider_ids = EXCLUDED.provider_ids,
            title = EXCLUDED.title,
            sort_title = EXCLUDED.sort_title,
            original_title = EXCLUDED.original_title,
            original_language = EXCLUDED.original_language,
            status = EXCLUDED.status,
            overview = EXCLUDED.overview,
            studio = EXCLUDED.studio,
            network = EXCLUDED.network,
            runtime_minutes = EXCLUDED.runtime_minutes,
            year = EXCLUDED.year,
            images = EXCLUDED.images,
            updated_at = now()
        RETURNING *
        "#,
    )
    .bind(new.kind)
    .bind(tmdb_id)
    .bind(&new.tvdb_id)
    .bind(&new.imdb_id)
    .bind(&new.provider_ids)
    .bind(&new.title)
    .bind(&new.sort_title)
    .bind(&new.original_title)
    .bind(&new.original_language)
    .bind(&new.status)
    .bind(&new.overview)
    .bind(&new.studio)
    .bind(&new.network)
    .bind(new.runtime_minutes)
    .bind(new.year)
    .bind(&new.images)
    .fetch_one(pool)
    .await
    .map_err(MuseError::Database)
}

pub async fn upsert_by_tvdb(pool: &PgPool, new: &NewMediaMetadata) -> MuseResult<MediaMetadata> {
    let tvdb_id = new
        .tvdb_id
        .as_deref()
        .ok_or_else(|| MuseError::Conflict("upsert_by_tvdb requires tvdb_id".to_string()))?;

    sqlx::query_as::<_, MediaMetadata>(
        r#"
        INSERT INTO media_metadata (
            kind, tmdb_id, tvdb_id, imdb_id, provider_ids, title, sort_title,
            original_title, original_language, status, overview, studio,
            network, runtime_minutes, year, images
        )
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16)
        ON CONFLICT (kind, tvdb_id) DO UPDATE SET
            tmdb_id = EXCLUDED.tmdb_id,
            imdb_id = EXCLUDED.imdb_id,
            provider_ids = EXCLUDED.provider_ids,
            title = EXCLUDED.title,
            sort_title = EXCLUDED.sort_title,
            original_title = EXCLUDED.original_title,
            original_language = EXCLUDED.original_language,
            status = EXCLUDED.status,
            overview = EXCLUDED.overview,
            studio = EXCLUDED.studio,
            network = EXCLUDED.network,
            runtime_minutes = EXCLUDED.runtime_minutes,
            year = EXCLUDED.year,
            images = EXCLUDED.images,
            updated_at = now()
        RETURNING *
        "#,
    )
    .bind(new.kind)
    .bind(&new.tmdb_id)
    .bind(tvdb_id)
    .bind(&new.imdb_id)
    .bind(&new.provider_ids)
    .bind(&new.title)
    .bind(&new.sort_title)
    .bind(&new.original_title)
    .bind(&new.original_language)
    .bind(&new.status)
    .bind(&new.overview)
    .bind(&new.studio)
    .bind(&new.network)
    .bind(new.runtime_minutes)
    .bind(new.year)
    .bind(&new.images)
    .fetch_one(pool)
    .await
    .map_err(MuseError::Database)
}

pub async fn get(pool: &PgPool, id: i64) -> MuseResult<MediaMetadata> {
    sqlx::query_as::<_, MediaMetadata>("SELECT * FROM media_metadata WHERE id = $1")
        .bind(id)
        .fetch_optional(pool)
        .await
        .map_err(MuseError::Database)?
        .ok_or_else(|| MuseError::NotFound(format!("media_metadata {id} not found")))
}

/// Resolve a provider `tmdb_id` (+ kind) to an existing `media_metadata.id`,
/// if the title is already known to the catalog. Used by the trending
/// ingest (MUSE-19) to link a trending entry to a library title — most
/// trending entries won't resolve and stay `None` (the caller falls back to
/// `external_ref`).
pub async fn find_by_tmdb_id(
    pool: &PgPool,
    kind: MediaKind,
    tmdb_id: &str,
) -> MuseResult<Option<i64>> {
    sqlx::query_scalar::<_, i64>("SELECT id FROM media_metadata WHERE kind = $1 AND tmdb_id = $2")
        .bind(kind)
        .bind(tmdb_id)
        .fetch_optional(pool)
        .await
        .map_err(MuseError::Database)
}

/// MUSEL-B1: resolve a provider `tvdb_id` (+ kind) to an existing
/// `media_metadata.id`, the TVDB-equivalent of [`find_by_tmdb_id`] above —
/// used by the library scanner to match a file whose path carries a
/// `{tvdb-NNNN}` id tag (the Sonarr/Radarr folder-naming convention) against
/// an already-cataloged row, without ever creating a new one.
pub async fn find_by_tvdb_id(pool: &PgPool, kind: MediaKind, tvdb_id: &str) -> MuseResult<Option<i64>> {
    sqlx::query_scalar::<_, i64>("SELECT id FROM media_metadata WHERE kind = $1 AND tvdb_id = $2")
        .bind(kind)
        .bind(tvdb_id)
        .fetch_optional(pool)
        .await
        .map_err(MuseError::Database)
}

/// BSEED-1: resolve a provider `imdb_id` (+ kind) to an existing
/// `media_metadata.id` — the IMDb-equivalent of [`find_by_tmdb_id`] /
/// [`find_by_tvdb_id`], used by the Tautulli session resolver to match a
/// history row's `imdb://tt…` GUID against an already-cataloged (arr-ingested)
/// row. `imdb_id` is stored verbatim (`tt…`), so this is an exact match on the
/// same string form the GUID parser yields.
///
/// **Resolves ONLY when the match is unambiguous** (same rule as
/// [`find_by_title_year`]): unlike `tmdb_id`/`tvdb_id`, `imdb_id` is NOT unique
/// in `media_metadata` (duplicate/inconsistent IMDb records happen), and imdb
/// is the last-resort GUID priority — so this fetches up to two candidates and
/// returns `Some` only when exactly one exists; zero or more-than-one → `None`
/// (leave the session unresolved rather than cross-link to an arbitrary
/// same-kind row).
pub async fn find_by_imdb_id(pool: &PgPool, kind: MediaKind, imdb_id: &str) -> MuseResult<Option<i64>> {
    let candidates: Vec<i64> = sqlx::query_scalar::<_, i64>(
        "SELECT id FROM media_metadata WHERE kind = $1 AND imdb_id = $2 LIMIT 2",
    )
    .bind(kind)
    .bind(imdb_id)
    .fetch_all(pool)
    .await
    .map_err(MuseError::Database)?;

    // Exactly one → unambiguous resolution; zero or ambiguous (≥2) → None.
    match candidates.as_slice() {
        [only] => Ok(Some(*only)),
        _ => Ok(None),
    }
}

/// Best-effort resolve a parsed release (title + optional year) to an
/// existing `media_metadata` row via an exact, case-insensitive title match
/// (+ year equality when a year was parsed). Used by the Prowlarr
/// report-pull worker (MUSE-17) to link a release to a title without ever
/// guessing at a fuzzy match -- a release that doesn't resolve stays
/// unmatched (`media_metadata_id = NULL`), which is preserved on purpose
/// (negative-space discovery, spec S4b-B) rather than silently dropped.
///
/// Deliberately NOT a fuzzy/similarity match (unlike `search_by_title`
/// above): a curation/availability signal is only as trustworthy as its
/// resolution, and a wrong match silently feeding curation is worse than a
/// visibly-unresolved release.
///
/// **Resolves ONLY when the match is unambiguous** (FIX B): `(kind,
/// lower(title), year)` is not a unique key, so two different titles sharing a
/// title+year would otherwise let a blind `LIMIT 1` cross-link to an arbitrary
/// one. Instead this fetches up to two candidates and returns `Some` only when
/// exactly one exists; zero or more-than-one → `None` (leave it unresolved
/// rather than guess). This is a last-resort fallback (only reached when a
/// session/release carries no tmdb/tvdb/imdb id at all), so correctness beats
/// coverage. The year-absent branch is even more ambiguous and follows the
/// same rule.
pub async fn find_by_title_year(
    pool: &PgPool,
    kind: MediaKind,
    title: &str,
    year: Option<i32>,
) -> MuseResult<Option<i64>> {
    let candidates: Vec<i64> = match year {
        Some(y) => sqlx::query_scalar::<_, i64>(
            "SELECT id FROM media_metadata WHERE kind = $1 AND lower(title) = lower($2) AND year = $3 LIMIT 2",
        )
        .bind(kind)
        .bind(title)
        .bind(y)
        .fetch_all(pool)
        .await
        .map_err(MuseError::Database)?,
        None => sqlx::query_scalar::<_, i64>(
            "SELECT id FROM media_metadata WHERE kind = $1 AND lower(title) = lower($2) LIMIT 2",
        )
        .bind(kind)
        .bind(title)
        .fetch_all(pool)
        .await
        .map_err(MuseError::Database)?,
    };

    // Exactly one → unambiguous resolution; zero or ambiguous (≥2) → None.
    match candidates.as_slice() {
        [only] => Ok(Some(*only)),
        _ => Ok(None),
    }
}

/// MUSE-09 "more like this" fallback for a seed with no stored embedding:
/// rank other `media_metadata` rows of the same `kind` by shared-genre
/// overlap with `seed_id`, most genres in common first (ties broken by
/// `popularity` so the fallback still surfaces something reasonable, not an
/// arbitrary id order). Excludes the seed itself. Returns an empty vec
/// (never an error) when the seed has no genres recorded — that's a normal
/// sparse-metadata case, not a failure.
pub async fn similar_by_genre(
    pool: &PgPool,
    seed_id: i64,
    kind: MediaKind,
    limit: i64,
) -> MuseResult<Vec<MediaMetadata>> {
    sqlx::query_as::<_, MediaMetadata>(
        r#"
        SELECT mm.*
        FROM media_metadata mm
        JOIN media_metadata_genres mmg ON mmg.media_metadata_id = mm.id
        WHERE mm.kind = $1
          AND mm.id <> $2
          AND mmg.genre_id IN (
              SELECT genre_id FROM media_metadata_genres WHERE media_metadata_id = $2
          )
        GROUP BY mm.id
        ORDER BY COUNT(*) DESC, mm.popularity DESC NULLS LAST, mm.id
        LIMIT $3
        "#,
    )
    .bind(kind)
    .bind(seed_id)
    .bind(limit)
    .fetch_all(pool)
    .await
    .map_err(MuseError::Database)
}

/// MUSEL-A2: candidate rows for the resolver — titles with a known
/// provider id (so `resolve_and_merge` has something to fan out with) but
/// no prior enrichment sync, oldest-first (`last_info_sync IS NULL` first
/// via `NULLS FIRST`, then least-recently-synced). Bounded by `limit`,
/// same posture as `curation::candidates::gather_gap_candidates` feeding
/// `maintenance::run_maintenance_pass`'s bounded enrichment step — a
/// background pass that could otherwise scan the whole catalog every tick.
pub async fn find_needing_enrichment(
    pool: &PgPool,
    kind: MediaKind,
    limit: i64,
) -> MuseResult<Vec<MediaMetadata>> {
    sqlx::query_as::<_, MediaMetadata>(
        r#"
        SELECT * FROM media_metadata mm
        WHERE mm.kind = $1
          AND (mm.tmdb_id IS NOT NULL OR mm.tvdb_id IS NOT NULL OR mm.imdb_id IS NOT NULL)
          AND (
                -- MUSE #114: `overview IS NULL` alone was the entire eligibility test, and it
                -- silently disabled enrichment for the whole library. `arr::ingest` populates
                -- `overview` for every title it upserts (1649 movies + 240 series here), so
                -- every row failed this predicate and the pass reported
                -- `metadata_resolve_ran=true metadata_resolved=0` every hour for months.
                --
                -- Overview and genres come from DIFFERENT sources: overview from *arr, genres
                -- only from the provider enrichment this pass performs. So a title can have an
                -- overview and no genres for ever, which is exactly why `genres` and
                -- `media_metadata_genres` are both 0 rows — and why taste's genre_lean and the
                -- Discover taste category have nothing to work with (MUSE #90).
                --
                -- The predicate now asks what enrichment actually SUPPLIES, rather than using
                -- one field as a proxy for all of them.
                mm.overview IS NULL
                OR NOT EXISTS (
                    SELECT 1 FROM media_metadata_genres g WHERE g.media_metadata_id = mm.id
                )
          )
        ORDER BY mm.last_info_sync ASC NULLS FIRST, mm.id
        LIMIT $2
        "#,
    )
    .bind(kind)
    .bind(limit)
    .fetch_all(pool)
    .await
    .map_err(MuseError::Database)
}

/// MUSEL-A2: persist a `metadata::resolve_and_merge` result onto an
/// *existing* `media_metadata` row (`media_metadata_id`) — this never
/// creates a row; row creation stays `arr::ingest`'s job. Muse's DB only:
/// nothing here calls out to a provider or writes to the library, matching
/// `MetadataProvider`'s read-only contract.
///
/// Truly additive/fill-only, never a blind overwrite (review finding 3,
/// S119b codex REQUEST_CHANGES) — an existing Muse DB value may be
/// curated or a previous, deliberately-chosen enrichment, and a fresh
/// resolver pass must never clobber it:
/// - `overview`/`year`/`tmdb_id`/`tvdb_id`/`imdb_id` only fill in when the
///   row's own value is currently NULL — the row's existing value always
///   wins over whatever this pass resolved, even if the merge produced a
///   different one.
/// - `provider_ids` is a union with what the row already has (existing
///   keys win on overlap), never replaced wholesale.
/// - `images`/`keywords` are ADD-ONLY: a new `coverType`/keyword not
///   already present is appended; an existing `coverType` entry's
///   URL is left untouched even if the merge produced a different URL for
///   the same `coverType` — this is intentionally NOT a "refresh art from
///   the provider" operation (that would be a different, explicit item).
/// - `genres` are additively linked (`ensure_genres_linked`) — never
///   unlinked, so a possibly-wrong single-provider genre never silently
///   removes a curator's or another provider's tag.
pub async fn apply_enrichment(
    pool: &PgPool,
    media_metadata_id: i64,
    enrichment: &ProviderMetadata,
) -> MuseResult<MediaMetadata> {
    let current = get(pool, media_metadata_id).await?;

    let mut provider_ids = current.provider_ids.as_object().cloned().unwrap_or_default();
    for (k, v) in &enrichment.provider_ids {
        provider_ids
            .entry(k.clone())
            .or_insert_with(|| serde_json::Value::String(v.clone()));
    }

    let mut images: Vec<serde_json::Value> = current.images.as_array().cloned().unwrap_or_default();
    if let Some(poster) = &enrichment.images.poster_url {
        add_image_entry_if_absent(&mut images, "poster", poster);
    }
    if let Some(backdrop) = &enrichment.images.backdrop_url {
        add_image_entry_if_absent(&mut images, "fanart", backdrop);
    }

    let mut keywords: Vec<serde_json::Value> = current.keywords.as_array().cloned().unwrap_or_default();
    for keyword in &enrichment.keywords {
        if keyword.trim().is_empty() {
            continue;
        }
        let value = serde_json::Value::String(keyword.clone());
        if !keywords.contains(&value) {
            keywords.push(value);
        }
    }

    let mut ratings = current.ratings.as_object().cloned().unwrap_or_default();
    if let Some(rating) = enrichment.rating {
        // Coarse v1 shape: `ProviderMetadata::rating` is already a single
        // merged value by the time it reaches here (see
        // `metadata::resolve::merge_metadata`'s precedence), so there's no
        // per-provider breakdown left to key this by. Documented in
        // README.md; a future item could thread per-provider ratings
        // through `ProviderMetadata` if that granularity turns out to
        // matter for the UI. Fill-only, same as every other scalar below:
        // an already-recorded "resolved" rating is left alone.
        ratings
            .entry("resolved".to_string())
            .or_insert_with(|| serde_json::json!({ "value": rating }));
    }

    let tmdb_id = current.tmdb_id.clone().or_else(|| enrichment.provider_ids.get("tmdb").cloned());
    let tvdb_id = current.tvdb_id.clone().or_else(|| enrichment.provider_ids.get("tvdb").cloned());
    let imdb_id = current.imdb_id.clone().or_else(|| enrichment.provider_ids.get("imdb").cloned());
    // Fill-only: the row's existing overview/year always win (review
    // finding 3) — a resolver re-run must never clobber a previously
    // recorded (possibly curated) value.
    let overview = current.overview.clone().or_else(|| enrichment.overview.clone());
    let year = current.year.or(enrichment.year);

    let updated = sqlx::query_as::<_, MediaMetadata>(
        r#"
        UPDATE media_metadata SET
            tmdb_id = $2,
            tvdb_id = $3,
            imdb_id = $4,
            provider_ids = $5,
            overview = $6,
            images = $7,
            ratings = $8,
            year = $9,
            keywords = $10,
            last_info_sync = now(),
            updated_at = now()
        WHERE id = $1
        RETURNING *
        "#,
    )
    .bind(media_metadata_id)
    .bind(&tmdb_id)
    .bind(&tvdb_id)
    .bind(&imdb_id)
    .bind(serde_json::Value::Object(provider_ids))
    .bind(&overview)
    .bind(serde_json::Value::Array(images))
    .bind(serde_json::Value::Object(ratings))
    .bind(year)
    .bind(serde_json::Value::Array(keywords))
    .fetch_optional(pool)
    .await
    .map_err(MuseError::Database)?
    .ok_or_else(|| MuseError::NotFound(format!("media_metadata {media_metadata_id} not found")))?;

    if !enrichment.genres.is_empty() {
        ensure_genres_linked(pool, media_metadata_id, &enrichment.genres).await?;
    }

    Ok(updated)
}

/// Appends one `images` array entry for `cover_type` ONLY if no entry for
/// that `coverType` already exists — add-only, never replaces an existing
/// entry's URL (review finding 3: an existing poster/backdrop may be a
/// deliberately-chosen or previously-fetched one; a resolver re-run must
/// not silently swap it for a different provider's URL). Matches the
/// shape documented on `media_metadata.images` in migration 0005
/// (`[{coverType,url,remoteUrl}]`).
fn add_image_entry_if_absent(images: &mut Vec<serde_json::Value>, cover_type: &str, url: &str) {
    let already_present = images
        .iter()
        .any(|e| e.get("coverType").and_then(|c| c.as_str()) == Some(cover_type));
    if !already_present {
        images.push(serde_json::json!({ "coverType": cover_type, "url": url, "remoteUrl": url }));
    }
}

/// Find-or-create each genre by name, then link it to `media_metadata_id`
/// if not already linked. Never unlinks an existing genre a prior
/// enrichment pass (or a different provider) already attached — see
/// [`apply_enrichment`]'s doc for why this is additive-only.
async fn ensure_genres_linked(pool: &PgPool, media_metadata_id: i64, genre_names: &[String]) -> MuseResult<()> {
    for name in genre_names {
        if name.trim().is_empty() {
            continue;
        }

        // `DO UPDATE SET name = EXCLUDED.name` (a no-op write) rather than
        // `DO NOTHING` so `RETURNING id` still fires on a name that
        // already exists — `DO NOTHING` skips `RETURNING` entirely on a
        // conflict, which would leave `genre_id` unresolved.
        let genre_id: i64 = sqlx::query_scalar::<_, i64>(
            r#"
            INSERT INTO genres (name) VALUES ($1)
            ON CONFLICT (name) DO UPDATE SET name = EXCLUDED.name
            RETURNING id
            "#,
        )
        .bind(name)
        .fetch_one(pool)
        .await
        .map_err(MuseError::Database)?;

        sqlx::query(
            "INSERT INTO media_metadata_genres (media_metadata_id, genre_id) VALUES ($1, $2) \
             ON CONFLICT DO NOTHING",
        )
        .bind(media_metadata_id)
        .bind(genre_id)
        .execute(pool)
        .await
        .map_err(MuseError::Database)?;
    }
    Ok(())
}

pub async fn search_by_title(pool: &PgPool, query: &str, limit: i64) -> MuseResult<Vec<MediaMetadata>> {
    sqlx::query_as::<_, MediaMetadata>(
        r#"
        SELECT * FROM media_metadata
        WHERE title ILIKE '%' || $1 || '%'
        ORDER BY similarity(title, $1) DESC
        LIMIT $2
        "#,
    )
    .bind(query)
    .bind(limit)
    .fetch_all(pool)
    .await
    .map_err(MuseError::Database)
}

#[cfg(test)]
mod tests {
    /// A unique-enough suffix so repeated runs do not collide on (kind, tmdb_id).
    fn uuid_like() -> String {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos().to_string())
            .unwrap_or_else(|_| "0".to_string())
    }

    use super::*;

    /// MUSEL-A2 persistence round-trip: seeds a `media_metadata` row (as
    /// arr ingest would), applies a `ProviderMetadata` enrichment, and
    /// asserts the merged fields landed — overview/images/ratings/year/
    /// provider_ids onto the row, genres linked via
    /// `media_metadata_genres`. Gated on `MUSE_TEST_DATABASE_URL`: skips
    /// cleanly (never fails) when unset, matching every other live-DB test
    /// in this crate (see `maintenance::tests` for the same pattern).
    #[tokio::test]
    async fn a_title_with_an_overview_but_NO_GENRES_is_still_eligible_for_enrichment() {
        // MUSE #114, the whole bug. The predicate was `overview IS NULL` alone, and
        // `arr::ingest` fills `overview` for every title it upserts — so the entire library
        // was permanently ineligible and the pass logged
        // `metadata_resolve_ran=true metadata_resolved=0` every hour while `genres` and
        // `media_metadata_genres` stayed at 0 rows.
        //
        // Overview and genres arrive from DIFFERENT sources, so one is not a proxy for the
        // other. This asserts the case that was excluded: a fully-described title that has
        // never had a genre linked.
        let Ok(url) = std::env::var("MUSE_TEST_DATABASE_URL") else {
            eprintln!("MUSE_TEST_DATABASE_URL not set — skipping");
            return;
        };
        let pool = PgPool::connect(&url).await.expect("connect");

        let row = sqlx::query_as::<_, MediaMetadata>(
            r#"INSERT INTO media_metadata (kind, tmdb_id, title, overview)
               VALUES ('movie', $1, 'Has Overview No Genres', 'a full overview')
               RETURNING *"#,
        )
        .bind(format!("test-{}", uuid_like()))
        .fetch_one(&pool)
        .await
        .expect("insert");

        let found = find_needing_enrichment(&pool, MediaKind::Movie, 500)
            .await
            .expect("query");
        assert!(
            found.iter().any(|m| m.id == row.id),
            "a title with an overview but no linked genres MUST be eligible — it is the case \
             that kept the whole library out of enrichment",
        );

        // And once it HAS a genre, it drops out again (so the pass converges instead of
        // re-processing the library for ever).
        ensure_genres_linked(&pool, row.id, &["Drama".to_string()])
            .await
            .expect("link");
        let after = find_needing_enrichment(&pool, MediaKind::Movie, 500)
            .await
            .expect("query");
        assert!(
            !after.iter().any(|m| m.id == row.id),
            "an enriched title must stop being a candidate, or the pass never converges",
        );

        sqlx::query("DELETE FROM media_metadata WHERE id = $1")
            .bind(row.id)
            .execute(&pool)
            .await
            .ok();
    }

    #[tokio::test]
    async fn apply_enrichment_persists_merged_fields_onto_existing_row() {
        let Ok(database_url) = std::env::var("MUSE_TEST_DATABASE_URL") else {
            eprintln!(
                "MUSE_TEST_DATABASE_URL not set — skipping \
                 apply_enrichment_persists_merged_fields_onto_existing_row \
                 (expected in the default test run; the crate does not require a live DB)"
            );
            return;
        };

        use sqlx::postgres::PgPoolOptions;
        use uuid::Uuid;

        let pool = PgPoolOptions::new()
            .max_connections(5)
            .connect(&database_url)
            .await
            .expect("connect to MUSE_TEST_DATABASE_URL");
        sqlx::migrate!("./migrations")
            .run(&pool)
            .await
            .expect("migrations should apply cleanly");

        let suffix = Uuid::new_v4().simple().to_string();

        let seeded = upsert_by_tmdb(
            &pool,
            &NewMediaMetadata {
                kind: MediaKind::Movie,
                tmdb_id: Some(format!("musela2-tmdb-{suffix}")),
                tvdb_id: None,
                imdb_id: None,
                provider_ids: serde_json::json!({}),
                title: format!("MUSEL-A2 Test Movie {suffix}"),
                sort_title: None,
                original_title: None,
                original_language: None,
                status: None,
                overview: None,
                studio: None,
                network: None,
                runtime_minutes: None,
                year: None,
                images: serde_json::json!([]),
            },
        )
        .await
        .expect("seed media_metadata row");

        assert!(seeded.overview.is_none(), "precondition: no overview yet");

        let enrichment = ProviderMetadata {
            provider_ids: [("imdb".to_string(), format!("tt{suffix}"))].into_iter().collect(),
            title: Some(format!("MUSEL-A2 Test Movie {suffix} (Resolved)")),
            overview: Some("A resolved overview from the merge.".to_string()),
            genres: vec![format!("musela2-genre-{suffix}")],
            images: crate::metadata::ProviderImages {
                poster_url: Some("https://image.tmdb.org/t/p/w780/poster.jpg".to_string()),
                backdrop_url: None,
            },
            rating: Some(8.4),
            first_aired: Some("2021-01-01".to_string()),
            year: Some(2021),
            network: None,
            keywords: vec![format!("musela2-keyword-{suffix}")],
            runtime_minutes: None,
        };

        let updated = apply_enrichment(&pool, seeded.id, &enrichment)
            .await
            .expect("apply_enrichment should succeed");

        assert_eq!(updated.overview, enrichment.overview);
        assert_eq!(updated.year, Some(2021));
        assert_eq!(updated.imdb_id, Some(format!("tt{suffix}")));
        // tmdb_id already present on the row -> untouched (COALESCE precedence).
        assert_eq!(updated.tmdb_id, Some(format!("musela2-tmdb-{suffix}")));

        let images = updated.images.as_array().expect("images should be an array");
        assert!(images
            .iter()
            .any(|img| img.get("coverType").and_then(|c| c.as_str()) == Some("poster")
                && img.get("url").and_then(|u| u.as_str()) == Some("https://image.tmdb.org/t/p/w780/poster.jpg")));

        let ratings = updated.ratings.as_object().expect("ratings should be an object");
        assert_eq!(
            ratings.get("resolved").and_then(|r| r.get("value")).and_then(|v| v.as_f64()),
            Some(8.4)
        );

        let keywords = updated.keywords.as_array().expect("keywords should be an array");
        assert!(keywords
            .iter()
            .any(|k| k.as_str() == Some(format!("musela2-keyword-{suffix}").as_str())));

        let genre_linked: i64 = sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM media_metadata_genres mmg \
             JOIN genres g ON g.id = mmg.genre_id \
             WHERE mmg.media_metadata_id = $1 AND g.name = $2",
        )
        .bind(seeded.id)
        .bind(format!("musela2-genre-{suffix}"))
        .fetch_one(&pool)
        .await
        .expect("genre-link count query");
        assert_eq!(genre_linked, 1);

        // A second apply_enrichment call (re-running the resolver) with the
        // same genre/keyword is idempotent -- still exactly one link row
        // and no duplicate keyword entry.
        let updated_again = apply_enrichment(&pool, seeded.id, &enrichment)
            .await
            .expect("second apply_enrichment should succeed");
        let genre_linked_again: i64 = sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM media_metadata_genres mmg \
             JOIN genres g ON g.id = mmg.genre_id \
             WHERE mmg.media_metadata_id = $1 AND g.name = $2",
        )
        .bind(seeded.id)
        .bind(format!("musela2-genre-{suffix}"))
        .fetch_one(&pool)
        .await
        .expect("genre-link count query");
        assert_eq!(genre_linked_again, 1);

        let keywords_again = updated_again.keywords.as_array().expect("keywords should be an array");
        let keyword_count = keywords_again
            .iter()
            .filter(|k| k.as_str() == Some(format!("musela2-keyword-{suffix}").as_str()))
            .count();
        assert_eq!(keyword_count, 1, "re-running enrichment must not duplicate an already-recorded keyword");
    }

    /// Review finding 3 (S119b codex REQUEST_CHANGES): `apply_enrichment`
    /// must be fill-only for `overview`/images, never clobbering a value
    /// the row already carries (curated, or from an earlier enrichment
    /// pass) with whatever a fresh resolve happened to produce. Seeds a
    /// row that ALREADY has an overview and a poster image, then applies
    /// an enrichment carrying DIFFERENT values for both, and asserts
    /// neither changed.
    #[tokio::test]
    async fn apply_enrichment_never_clobbers_a_preexisting_overview_or_image() {
        let Ok(database_url) = std::env::var("MUSE_TEST_DATABASE_URL") else {
            eprintln!(
                "MUSE_TEST_DATABASE_URL not set — skipping \
                 apply_enrichment_never_clobbers_a_preexisting_overview_or_image \
                 (expected in the default test run; the crate does not require a live DB)"
            );
            return;
        };

        use sqlx::postgres::PgPoolOptions;
        use uuid::Uuid;

        let pool = PgPoolOptions::new()
            .max_connections(5)
            .connect(&database_url)
            .await
            .expect("connect to MUSE_TEST_DATABASE_URL");
        sqlx::migrate!("./migrations")
            .run(&pool)
            .await
            .expect("migrations should apply cleanly");

        let suffix = Uuid::new_v4().simple().to_string();
        let curated_overview = format!("Curated overview {suffix} — do not overwrite");
        let curated_poster = format!("https://curated.example/{suffix}/poster.jpg");

        let seeded = upsert_by_tmdb(
            &pool,
            &NewMediaMetadata {
                kind: MediaKind::Movie,
                tmdb_id: Some(format!("musela2-clobber-tmdb-{suffix}")),
                tvdb_id: None,
                imdb_id: None,
                provider_ids: serde_json::json!({}),
                title: format!("MUSEL-A2 Clobber Test Movie {suffix}"),
                sort_title: None,
                original_title: None,
                original_language: None,
                status: None,
                overview: Some(curated_overview.clone()),
                studio: None,
                network: None,
                runtime_minutes: None,
                year: Some(1999),
                images: serde_json::json!([{"coverType": "poster", "url": curated_poster, "remoteUrl": curated_poster}]),
            },
        )
        .await
        .expect("seed media_metadata row with a pre-existing overview + poster");

        let enrichment = ProviderMetadata {
            title: Some("A Different Title Entirely".to_string()),
            overview: Some("A DIFFERENT overview from a fresh resolve.".to_string()),
            images: crate::metadata::ProviderImages {
                poster_url: Some("https://fresh-provider.example/different-poster.jpg".to_string()),
                backdrop_url: None,
            },
            year: Some(2005),
            ..Default::default()
        };

        let updated = apply_enrichment(&pool, seeded.id, &enrichment)
            .await
            .expect("apply_enrichment should succeed");

        assert_eq!(
            updated.overview,
            Some(curated_overview),
            "a pre-existing overview must never be replaced by a fresh resolve"
        );
        assert_eq!(updated.year, Some(1999), "a pre-existing year must never be replaced");

        let images = updated.images.as_array().expect("images should be an array");
        let poster_entries: Vec<_> = images
            .iter()
            .filter(|img| img.get("coverType").and_then(|c| c.as_str()) == Some("poster"))
            .collect();
        assert_eq!(poster_entries.len(), 1, "must not duplicate the poster coverType entry");
        assert_eq!(
            poster_entries[0].get("url").and_then(|u| u.as_str()),
            Some(curated_poster.as_str()),
            "the pre-existing poster URL must be left untouched, not replaced by the fresh provider's URL"
        );
    }

    /// FIX B (live-DB, gated): `find_by_title_year` resolves ONLY when the
    /// title+year match is unambiguous. A unique title+year returns that id;
    /// two different movies sharing the exact title+year return `None` (never a
    /// blind pick). The year-absent branch follows the same rule.
    #[tokio::test]
    async fn find_by_title_year_resolves_only_when_unambiguous() {
        let Ok(database_url) = std::env::var("MUSE_TEST_DATABASE_URL") else {
            eprintln!(
                "MUSE_TEST_DATABASE_URL not set — skipping find_by_title_year_resolves_only_when_unambiguous"
            );
            return;
        };

        use sqlx::postgres::PgPoolOptions;
        use uuid::Uuid;

        let pool = PgPoolOptions::new()
            .max_connections(5)
            .connect(&database_url)
            .await
            .expect("connect");
        sqlx::migrate!("./migrations").run(&pool).await.expect("migrate");

        let suffix = Uuid::new_v4().simple().to_string();

        // Unique title+year -> resolves to exactly that row.
        let unique_title = format!("FIXB Unique {suffix}");
        let unique = upsert_by_tmdb(
            &pool,
            &NewMediaMetadata {
                kind: MediaKind::Movie,
                tmdb_id: Some(format!("fixB-unique-{suffix}")),
                tvdb_id: None,
                imdb_id: None,
                provider_ids: serde_json::json!({}),
                title: unique_title.clone(),
                sort_title: None,
                original_title: None,
                original_language: None,
                status: None,
                overview: None,
                studio: None,
                network: None,
                runtime_minutes: None,
                year: Some(1990),
                images: serde_json::json!([]),
            },
        )
        .await
        .expect("seed unique");

        assert_eq!(
            find_by_title_year(&pool, MediaKind::Movie, &unique_title, Some(1990))
                .await
                .expect("query"),
            Some(unique.id),
            "a unique title+year must resolve"
        );
        // Year-absent, still unique title -> resolves.
        assert_eq!(
            find_by_title_year(&pool, MediaKind::Movie, &unique_title, None)
                .await
                .expect("query"),
            Some(unique.id),
            "a unique title (year absent) must resolve"
        );

        // Two different movies sharing the SAME title+year -> ambiguous -> None.
        let dup_title = format!("FIXB Ambiguous {suffix}");
        for n in 0..2 {
            upsert_by_tmdb(
                &pool,
                &NewMediaMetadata {
                    kind: MediaKind::Movie,
                    tmdb_id: Some(format!("fixB-dup-{n}-{suffix}")),
                    tvdb_id: None,
                    imdb_id: None,
                    provider_ids: serde_json::json!({}),
                    title: dup_title.clone(),
                    sort_title: None,
                    original_title: None,
                    original_language: None,
                    status: None,
                    overview: None,
                    studio: None,
                    network: None,
                    runtime_minutes: None,
                    year: Some(1985),
                    images: serde_json::json!([]),
                },
            )
            .await
            .expect("seed dup");
        }

        assert_eq!(
            find_by_title_year(&pool, MediaKind::Movie, &dup_title, Some(1985))
                .await
                .expect("query"),
            None,
            "an ambiguous title+year (2 rows) must NOT be resolved — correctness over coverage"
        );
        assert_eq!(
            find_by_title_year(&pool, MediaKind::Movie, &dup_title, None)
                .await
                .expect("query"),
            None,
            "the year-absent branch must also refuse an ambiguous match"
        );
    }

    /// FINAL FIX (live-DB, gated): `find_by_imdb_id` resolves ONLY when the
    /// imdb match is unambiguous. `imdb_id` is not unique in `media_metadata`,
    /// so a unique imdb returns that id, but two same-kind rows sharing the
    /// exact `imdb_id` return `None` (never a blind cross-link).
    #[tokio::test]
    async fn find_by_imdb_id_resolves_only_when_unambiguous() {
        let Ok(database_url) = std::env::var("MUSE_TEST_DATABASE_URL") else {
            eprintln!(
                "MUSE_TEST_DATABASE_URL not set — skipping find_by_imdb_id_resolves_only_when_unambiguous"
            );
            return;
        };

        use sqlx::postgres::PgPoolOptions;
        use uuid::Uuid;

        let pool = PgPoolOptions::new()
            .max_connections(5)
            .connect(&database_url)
            .await
            .expect("connect");
        sqlx::migrate!("./migrations").run(&pool).await.expect("migrate");

        let suffix = Uuid::new_v4().simple().to_string();

        // Unique imdb_id -> resolves to exactly that row.
        let unique_imdb = format!("tt-fiximdb-unique-{suffix}");
        let unique = upsert_by_tmdb(
            &pool,
            &NewMediaMetadata {
                kind: MediaKind::Movie,
                tmdb_id: Some(format!("fiximdb-unique-{suffix}")),
                tvdb_id: None,
                imdb_id: Some(unique_imdb.clone()),
                provider_ids: serde_json::json!({}),
                title: format!("FIXIMDB Unique {suffix}"),
                sort_title: None,
                original_title: None,
                original_language: None,
                status: None,
                overview: None,
                studio: None,
                network: None,
                runtime_minutes: None,
                year: Some(1995),
                images: serde_json::json!([]),
            },
        )
        .await
        .expect("seed unique imdb");

        assert_eq!(
            find_by_imdb_id(&pool, MediaKind::Movie, &unique_imdb)
                .await
                .expect("query"),
            Some(unique.id),
            "a unique imdb_id must resolve"
        );

        // Two DIFFERENT same-kind rows sharing the SAME imdb_id -> ambiguous ->
        // None. (imdb_id is non-unique, so keyed on distinct tmdb_ids.)
        let dup_imdb = format!("tt-fiximdb-dup-{suffix}");
        for n in 0..2 {
            upsert_by_tmdb(
                &pool,
                &NewMediaMetadata {
                    kind: MediaKind::Movie,
                    tmdb_id: Some(format!("fiximdb-dup-{n}-{suffix}")),
                    tvdb_id: None,
                    imdb_id: Some(dup_imdb.clone()),
                    provider_ids: serde_json::json!({}),
                    title: format!("FIXIMDB Dup {n} {suffix}"),
                    sort_title: None,
                    original_title: None,
                    original_language: None,
                    status: None,
                    overview: None,
                    studio: None,
                    network: None,
                    runtime_minutes: None,
                    year: Some(1986),
                    images: serde_json::json!([]),
                },
            )
            .await
            .expect("seed dup imdb");
        }

        assert_eq!(
            find_by_imdb_id(&pool, MediaKind::Movie, &dup_imdb)
                .await
                .expect("query"),
            None,
            "an ambiguous imdb_id (2 same-kind rows) must NOT be resolved — correctness over coverage"
        );
    }
}
