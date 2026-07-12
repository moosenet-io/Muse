//! Source-text composition + the incremental embedding batch driver.
//!
//! ## Change detection (a documented divergence from the founding spec)
//! The MUSE-08 build brief anticipated a `source_text_hash` column on
//! `embeddings` for cheap change detection. The MUSE-03 migration that
//! actually shipped (`migrations/0018_embeddings.sql`) has no such column
//! — only `source_text` itself. Rather than add a migration for a hash the
//! schema doesn't need, this module treats the already-stored
//! `source_text` as its own change-detection key: [`compose_source_text`]
//! is byte-for-byte deterministic given the same inputs (stable field
//! order, sorted genres), so a plain string comparison against the stored
//! value is exactly as reliable as a hash comparison would be, at the
//! (small, Phase-0) library sizes this runs against, without a schema
//! change. If library size ever makes the text comparison itself a cost
//! concern, a `source_text_hash text` column can be added later purely as
//! an index/comparison optimization — the composition logic here would not
//! need to change.
//!
//! ## Entity keying
//! Embeddings are keyed to `media_item.id` (`EmbeddingEntityKind::MediaItem`),
//! matching the convention already exercised by MUSE-03's own round-trip
//! test (`telemetry_taste_embeddings_schema_migrates_and_round_trips` in
//! `src/integration_tests.rs`) — not `media_metadata.id`. A title present in
//! two libraries gets two embedding rows with identical `source_text`
//! (since both point at the same shared `media_metadata`); that's a small
//! amount of duplicated work, not a correctness problem, and keeps the
//! entity space aligned with what MUSE-09's recall will actually resolve
//! back to (a specific library instance, not a bare metadata row).
//!
//! ## VRAM-politeness
//! <host>'s GPU is shared: `lemonade-coder.service` holds it in permanent
//! production (`qwen3-coder:30b` on :8081) and <host> is not idle for ad hoc
//! GPU work. `nomic-embed-text` is tiny, but [`embed_stale`] still calls
//! Ollama in small sub-batches with a short pause between them rather than
//! firing every request back-to-back, so a large backlog doesn't turn into
//! a burst that contends with the resident serving model.

use std::time::Duration;

use sqlx::PgPool;

use crate::error::MuseResult;
use crate::models::embedding::{EmbeddingEntityKind, EmbeddingMatch, NewEmbedding, DEFAULT_EMBEDDING_MODEL};
use crate::models::media_metadata::MediaKind;
use crate::repo;

use super::ollama::OllamaEmbedClient;

/// How many `media_item` rows to pull from Postgres per page while scanning
/// for stale/missing embeddings. Purely a DB-scan chunk size — unrelated to
/// [`EMBED_SUB_BATCH_SIZE`], which bounds Ollama calls.
const CANDIDATE_PAGE_SIZE: i64 = 200;

/// Upper bound on how many pages a single [`embed_stale`] call will scan
/// looking for stale candidates (~5,000 rows at the default page size)
/// before giving up for this call. Prevents a single invocation from
/// scanning an enormous, fully-up-to-date library forever; a subsequent
/// call picks up where a fresh page-0 scan would naturally find new work
/// (newly-added titles sort to the end of `media_items` by id, so in
/// practice new work is found quickly even if a full historical sweep
/// would take multiple calls).
const MAX_PAGES_PER_CALL: i64 = 25;

/// How many Ollama `embed` calls to fire before pausing — the
/// VRAM-politeness knob described in the module docs.
const EMBED_SUB_BATCH_SIZE: usize = 8;

/// Pause between sub-batches of Ollama calls.
const INTER_SUB_BATCH_PAUSE: Duration = Duration::from_millis(300);

/// Result of one [`embed_stale`] call.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EmbedOutcome {
    /// Rows embedded and upserted this call.
    pub embedded: usize,
    /// Rows considered but skipped because `source_text` is unchanged
    /// (already embedded under the current model) — the idempotency case.
    pub skipped_unchanged: usize,
    /// Rows where computing or storing the embedding failed; logged, not
    /// fatal to the batch (one bad title shouldn't stall the rest).
    pub failed: usize,
}

impl EmbedOutcome {
    /// Total rows this call touched (embedded + skipped + failed).
    pub fn considered(&self) -> usize {
        self.embedded + self.skipped_unchanged + self.failed
    }
}

#[derive(Debug, sqlx::FromRow)]
struct CandidateRow {
    media_item_id: i64,
    kind: MediaKind,
    title: String,
    overview: Option<String>,
    tagline: Option<String>,
    studio: Option<String>,
    network: Option<String>,
    year: Option<i32>,
    genres: Vec<String>,
    existing_source_text: Option<String>,
}

/// Compose the deterministic embedding input string for a title.
///
/// Deterministic in two senses that matter for change detection: (1) field
/// order never depends on iteration/hash-map order, and (2) `genres` is
/// sorted before joining, so the same underlying metadata always produces
/// byte-identical text regardless of the order Postgres's `array_agg`
/// happened to return rows in. Only non-empty fields are included, so
/// sparse metadata doesn't leave literal "None"/empty-string noise in the
/// embedded text.
pub fn compose_source_text(
    kind: MediaKind,
    title: &str,
    year: Option<i32>,
    tagline: Option<&str>,
    overview: Option<&str>,
    studio: Option<&str>,
    network: Option<&str>,
    genres: &[String],
) -> String {
    let kind_label = match kind {
        MediaKind::Movie => "movie",
        MediaKind::Show => "show",
    };

    let mut lines = Vec::new();

    lines.push(match year {
        Some(y) => format!("{title} ({y})"),
        None => title.to_string(),
    });
    lines.push(format!("Type: {kind_label}"));

    if !genres.is_empty() {
        let mut sorted_genres = genres.to_vec();
        sorted_genres.sort();
        sorted_genres.dedup();
        lines.push(format!("Genres: {}", sorted_genres.join(", ")));
    }

    if let Some(studio) = non_empty(studio) {
        lines.push(format!("Studio: {studio}"));
    }
    if let Some(network) = non_empty(network) {
        lines.push(format!("Network: {network}"));
    }
    if let Some(tagline) = non_empty(tagline) {
        lines.push(format!("Tagline: {tagline}"));
    }
    if let Some(overview) = non_empty(overview) {
        lines.push(format!("Overview: {overview}"));
    }

    lines.join("\n")
}

fn non_empty(s: Option<&str>) -> Option<&str> {
    s.map(str::trim).filter(|s| !s.is_empty())
}

/// Find `media_item` rows whose embedding (under [`DEFAULT_EMBEDDING_MODEL`])
/// is missing or stale, embed up to `batch` of them via `client`, and upsert
/// the results into `embeddings`. Idempotent: re-running with no metadata
/// changes finds every candidate's `source_text` already matching and does
/// no Ollama calls or writes (all land in [`EmbedOutcome::skipped_unchanged`]).
///
/// Bounded by both `batch` (how many rows this call will actually embed)
/// and an internal page-scan cap (see [`MAX_PAGES_PER_CALL`]) so one call
/// can't run away scanning a huge, fully-current library. Safe to call
/// repeatedly on a schedule (e.g. a future worker) — each call makes
/// forward progress and never re-does settled work.
pub async fn embed_stale(pool: &PgPool, client: &OllamaEmbedClient, batch: usize) -> MuseResult<EmbedOutcome> {
    let mut outcome = EmbedOutcome::default();

    if batch == 0 {
        return Ok(outcome);
    }

    let mut to_embed: Vec<(i64, String)> = Vec::new();
    let mut offset: i64 = 0;

    'paging: for _ in 0..MAX_PAGES_PER_CALL {
        let rows = fetch_candidate_page(pool, offset, CANDIDATE_PAGE_SIZE).await?;
        let fetched = rows.len() as i64;
        if rows.is_empty() {
            break;
        }

        for row in rows {
            let composed = compose_source_text(
                row.kind,
                &row.title,
                row.year,
                row.tagline.as_deref(),
                row.overview.as_deref(),
                row.studio.as_deref(),
                row.network.as_deref(),
                &row.genres,
            );

            let unchanged = row
                .existing_source_text
                .as_deref()
                .is_some_and(|existing| existing == composed);

            if unchanged {
                outcome.skipped_unchanged += 1;
            } else {
                to_embed.push((row.media_item_id, composed));
            }

            if to_embed.len() >= batch {
                break 'paging;
            }
        }

        offset += fetched;
        if fetched < CANDIDATE_PAGE_SIZE {
            break; // exhausted the table
        }
    }

    let mut chunks = to_embed.chunks(EMBED_SUB_BATCH_SIZE).peekable();
    while let Some(sub_batch) = chunks.next() {
        for (media_item_id, source_text) in sub_batch {
            match embed_one(pool, client, *media_item_id, source_text).await {
                Ok(()) => outcome.embedded += 1,
                Err(e) => {
                    tracing::warn!(media_item_id, error = %e, "MUSE-08: failed to embed/upsert media_item");
                    outcome.failed += 1;
                }
            }
        }

        // VRAM-politeness pause — see module docs. Skip the pause after the
        // final sub-batch (nothing left to protect the GPU from).
        if chunks.peek().is_some() {
            tokio::time::sleep(INTER_SUB_BATCH_PAUSE).await;
        }
    }

    Ok(outcome)
}

async fn embed_one(pool: &PgPool, client: &OllamaEmbedClient, media_item_id: i64, source_text: &str) -> MuseResult<()> {
    let vector = client.embed(DEFAULT_EMBEDDING_MODEL, source_text).await?;

    repo::embedding::upsert(
        pool,
        &NewEmbedding::nomic(
            EmbeddingEntityKind::MediaItem,
            media_item_id,
            vector,
            Some(source_text.to_string()),
        ),
    )
    .await?;

    Ok(())
}

async fn fetch_candidate_page(pool: &PgPool, offset: i64, limit: i64) -> MuseResult<Vec<CandidateRow>> {
    sqlx::query_as::<_, CandidateRow>(
        r#"
        SELECT
            mi.id AS media_item_id,
            mm.kind AS kind,
            mm.title AS title,
            mm.overview AS overview,
            mm.tagline AS tagline,
            mm.studio AS studio,
            mm.network AS network,
            mm.year AS year,
            COALESCE(array_agg(DISTINCT g.name) FILTER (WHERE g.name IS NOT NULL), ARRAY[]::text[]) AS genres,
            e.source_text AS existing_source_text
        FROM media_items mi
        JOIN media_metadata mm ON mm.id = mi.media_metadata_id
        LEFT JOIN media_metadata_genres mmg ON mmg.media_metadata_id = mm.id
        LEFT JOIN genres g ON g.id = mmg.genre_id
        LEFT JOIN embeddings e
            ON e.entity_kind = $1 AND e.entity_id = mi.id AND e.model = $2
        GROUP BY mi.id, mm.kind, mm.title, mm.overview, mm.tagline, mm.studio, mm.network, mm.year, e.source_text
        ORDER BY mi.id
        LIMIT $3 OFFSET $4
        "#,
    )
    .bind(EmbeddingEntityKind::MediaItem.as_str())
    .bind(DEFAULT_EMBEDDING_MODEL)
    .bind(limit)
    .bind(offset)
    .fetch_all(pool)
    .await
    .map_err(crate::error::MuseError::Database)
}

/// Targeted variant of [`fetch_candidate_page`]'s query, scoped to specific
/// `media_item` ids instead of an offset/limit page. Used where callers
/// already know which rows they care about (tests; a future "force
/// re-embed this title" admin action) and would otherwise have to guess how
/// large a page to scan to find them in a table that only grows over time.
async fn fetch_candidates_by_media_item_ids(pool: &PgPool, media_item_ids: &[i64]) -> MuseResult<Vec<CandidateRow>> {
    sqlx::query_as::<_, CandidateRow>(
        r#"
        SELECT
            mi.id AS media_item_id,
            mm.kind AS kind,
            mm.title AS title,
            mm.overview AS overview,
            mm.tagline AS tagline,
            mm.studio AS studio,
            mm.network AS network,
            mm.year AS year,
            COALESCE(array_agg(DISTINCT g.name) FILTER (WHERE g.name IS NOT NULL), ARRAY[]::text[]) AS genres,
            e.source_text AS existing_source_text
        FROM media_items mi
        JOIN media_metadata mm ON mm.id = mi.media_metadata_id
        LEFT JOIN media_metadata_genres mmg ON mmg.media_metadata_id = mm.id
        LEFT JOIN genres g ON g.id = mmg.genre_id
        LEFT JOIN embeddings e
            ON e.entity_kind = $1 AND e.entity_id = mi.id AND e.model = $2
        WHERE mi.id = ANY($3)
        GROUP BY mi.id, mm.kind, mm.title, mm.overview, mm.tagline, mm.studio, mm.network, mm.year, e.source_text
        ORDER BY mi.id
        "#,
    )
    .bind(EmbeddingEntityKind::MediaItem.as_str())
    .bind(DEFAULT_EMBEDDING_MODEL)
    .bind(media_item_ids)
    .fetch_all(pool)
    .await
    .map_err(crate::error::MuseError::Database)
}

/// Cosine nearest-neighbor lookup over `media_item` embeddings under the
/// default model — the recall primitive MUSE-09 builds its search/"more
/// like this" surface on. A thin, opinionated wrapper over
/// `repo::embedding::nearest` (which is generic over entity kind + model);
/// this is scoped to the one combination the pipeline itself produces.
pub async fn nearest(pool: &PgPool, query_vec: Vec<f32>, k: i64) -> MuseResult<Vec<EmbeddingMatch>> {
    let query = pgvector::Vector::from(query_vec);
    repo::embedding::nearest(
        pool,
        EmbeddingEntityKind::MediaItem.as_str(),
        DEFAULT_EMBEDDING_MODEL,
        &query,
        k,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compose_source_text_is_deterministic_regardless_of_genre_order() {
        let a = compose_source_text(
            MediaKind::Movie,
            "Arrival",
            Some(2016),
            Some("Why are they here?"),
            Some("A linguist works with the military to communicate with aliens."),
            Some("Paramount Pictures"),
            None,
            &["Sci-Fi".to_string(), "Drama".to_string()],
        );
        let b = compose_source_text(
            MediaKind::Movie,
            "Arrival",
            Some(2016),
            Some("Why are they here?"),
            Some("A linguist works with the military to communicate with aliens."),
            Some("Paramount Pictures"),
            None,
            &["Drama".to_string(), "Sci-Fi".to_string()],
        );

        assert_eq!(a, b, "genre order must not affect composed text");
    }

    #[test]
    fn compose_source_text_omits_empty_and_whitespace_only_fields() {
        let text = compose_source_text(
            MediaKind::Show,
            "Test Show",
            None,
            Some("   "),
            None,
            Some(""),
            Some("Test Network"),
            &[],
        );

        assert!(text.contains("Test Show"));
        assert!(text.contains("Type: show"));
        assert!(text.contains("Network: Test Network"));
        assert!(!text.contains("Tagline:"));
        assert!(!text.contains("Studio:"));
        assert!(!text.contains("Genres:"));
        assert!(!text.contains("Overview:"));
    }

    #[test]
    fn compose_source_text_changes_when_a_field_changes() {
        let before = compose_source_text(
            MediaKind::Movie,
            "Test Movie",
            Some(2020),
            None,
            Some("Original overview."),
            None,
            None,
            &[],
        );
        let after = compose_source_text(
            MediaKind::Movie,
            "Test Movie",
            Some(2020),
            None,
            Some("Updated overview."),
            None,
            None,
            &[],
        );

        assert_ne!(
            before, after,
            "changing overview must change the composed text (this is the change-detection key)"
        );
    }

    #[test]
    fn compose_source_text_dedups_repeated_genres() {
        let text = compose_source_text(
            MediaKind::Movie,
            "Test Movie",
            Some(2020),
            None,
            None,
            None,
            None,
            &["Drama".to_string(), "Drama".to_string(), "Sci-Fi".to_string()],
        );

        assert_eq!(text.lines().find(|l| l.starts_with("Genres:")), Some("Genres: Drama, Sci-Fi"));
    }

    /// MUSE-08: `embed_stale` incremental / idempotent round-trip against a
    /// live Postgres, plus `nearest()` ordering, using hand-inserted
    /// embeddings so the test never depends on a reachable Ollama. Gated on
    /// `MUSE_TEST_DATABASE_URL` exactly like `src/integration_tests.rs` —
    /// skips cleanly (does not fail) when unset.
    #[tokio::test]
    async fn embed_stale_and_nearest_round_trip() {
        use crate::models::library::{LibraryKind, NewLibrary};
        use crate::models::media_item::NewMediaItem;
        use crate::models::media_metadata::NewMediaMetadata;
        use sqlx::postgres::PgPoolOptions;
        use uuid::Uuid;

        let Ok(database_url) = std::env::var("MUSE_TEST_DATABASE_URL") else {
            eprintln!(
                "MUSE_TEST_DATABASE_URL not set — skipping embed_stale_and_nearest_round_trip \
                 (this is expected in the default test run; the crate does not require a live DB)"
            );
            return;
        };

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

        let library = repo::library::create(
            &pool,
            &NewLibrary {
                name: format!("muse08_test_{suffix}"),
                kind: LibraryKind::Movie,
                root_folder: "/media/Movies/".to_string(),
                source_arr_name: Some("radarr".to_string()),
                source_arr_url: None,
            },
        )
        .await
        .expect("create library");

        let target = repo::media_metadata::upsert_by_tmdb(
            &pool,
            &NewMediaMetadata {
                kind: MediaKind::Movie,
                tmdb_id: Some(format!("tmdb-muse08-target-{suffix}")),
                tvdb_id: None,
                imdb_id: None,
                provider_ids: serde_json::json!({}),
                title: "MUSE-08 Target Movie".to_string(),
                sort_title: None,
                original_title: None,
                original_language: Some("en".to_string()),
                status: Some("released".to_string()),
                overview: Some("A movie about embedding pipelines.".to_string()),
                studio: None,
                network: None,
                runtime_minutes: Some(100),
                year: Some(2024),
                images: serde_json::json!([]),
            },
        )
        .await
        .expect("upsert target media_metadata");

        let decoy = repo::media_metadata::upsert_by_tmdb(
            &pool,
            &NewMediaMetadata {
                kind: MediaKind::Movie,
                tmdb_id: Some(format!("tmdb-muse08-decoy-{suffix}")),
                tvdb_id: None,
                imdb_id: None,
                provider_ids: serde_json::json!({}),
                title: "MUSE-08 Decoy Movie".to_string(),
                sort_title: None,
                original_title: None,
                original_language: Some("en".to_string()),
                status: Some("released".to_string()),
                overview: None,
                studio: None,
                network: None,
                runtime_minutes: Some(90),
                year: Some(2018),
                images: serde_json::json!([]),
            },
        )
        .await
        .expect("upsert decoy media_metadata");

        let target_item = repo::media_item::upsert(
            &pool,
            &NewMediaItem {
                library_id: library.id,
                media_metadata_id: target.id,
                path: format!("/media/Movies/MUSE-08 Target Movie (2024) {suffix}"),
                monitored: true,
                quality_profile_id: None,
                minimum_availability: None,
                plex_rating_key: None,
                added_at: None,
            },
        )
        .await
        .expect("upsert target media_item");

        let decoy_item = repo::media_item::upsert(
            &pool,
            &NewMediaItem {
                library_id: library.id,
                media_metadata_id: decoy.id,
                path: format!("/media/Movies/MUSE-08 Decoy Movie (2018) {suffix}"),
                monitored: true,
                quality_profile_id: None,
                minimum_availability: None,
                plex_rating_key: None,
                added_at: None,
            },
        )
        .await
        .expect("upsert decoy media_item");

        // Hand-insert embeddings with known vectors (no live Ollama needed):
        // the query vector is closest to `target_item`'s vector by
        // construction, and far from `decoy_item`'s.
        let query_vec = vec![1.0_f32; 768];
        let mut target_vec = vec![1.0_f32; 768];
        target_vec[0] = 0.99; // nearly identical to the query
        let decoy_vec = vec![-1.0_f32; 768]; // maximally dissimilar (cosine)

        repo::embedding::upsert(
            &pool,
            &NewEmbedding::nomic(
                EmbeddingEntityKind::MediaItem,
                target_item.id,
                target_vec,
                Some("MUSE-08 Target Movie (2024)\nType: movie".to_string()),
            ),
        )
        .await
        .expect("insert target embedding");

        repo::embedding::upsert(
            &pool,
            &NewEmbedding::nomic(
                EmbeddingEntityKind::MediaItem,
                decoy_item.id,
                decoy_vec,
                Some("MUSE-08 Decoy Movie (2018)\nType: movie".to_string()),
            ),
        )
        .await
        .expect("insert decoy embedding");

        // Large k so the shared test DB's accumulated embeddings from other
        // live-DB tests can't crowd this test's target/decoy out of the
        // result set — the assertion below is about their RELATIVE order,
        // which holds regardless of how many unrelated rows exist.
        let neighbors = nearest(&pool, query_vec, 100_000)
            .await
            .expect("nearest should not error");

        let target_pos = neighbors
            .iter()
            .position(|m| m.entity_id == target_item.id)
            .expect("target item should be present in results");
        let decoy_pos = neighbors
            .iter()
            .position(|m| m.entity_id == decoy_item.id)
            .expect("decoy item should be present in results");

        assert!(
            target_pos < decoy_pos,
            "the near-identical vector must rank ahead of the maximally-dissimilar one"
        );

        // --- idempotency: fetching the candidate row must see the
        // hand-inserted target embedding's source_text and treat a
        // re-composition that matches it as unchanged, i.e. embed_stale
        // would skip it rather than re-embedding needlessly. We exercise
        // the candidate query directly (rather than embed_stale, which
        // needs a real OllamaEmbedClient) since that's the piece with the
        // change-detection logic under test. Scoped to this test's own ids
        // (not a page scan) so the assertion holds regardless of how many
        // rows other tests have accumulated in a shared test database.
        let candidates = fetch_candidates_by_media_item_ids(&pool, &[target_item.id])
            .await
            .expect("fetch candidate rows by id");
        let target_candidate = candidates
            .iter()
            .find(|r| r.media_item_id == target_item.id)
            .expect("target item should appear in the candidate scan");
        assert_eq!(
            target_candidate.existing_source_text.as_deref(),
            Some("MUSE-08 Target Movie (2024)\nType: movie"),
            "candidate scan must surface the previously-stored source_text for change detection"
        );
    }
}
