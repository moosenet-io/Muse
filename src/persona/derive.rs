//! Persona derivation: CLUSTERED watch signals -> persona taste vectors,
//! plus support for EXPLICIT, operator/user-declared persona definitions
//! over an arbitrary set of media items (MUSEX-02, Plane TERM #378).
//!
//! ## Why context-bucket clustering, not general k-means
//! `derive_context_cluster_personas` reuses
//! `taste_model::profile::context_key_for` — the exact weekend/weekday x
//! time-of-day bucketing `compute_context_centroids` already uses (spec
//! §3.4, MUSE-10) — as the "cluster" assignment rule, rather than a
//! general-purpose k-means over embedding space. Two reasons:
//! 1. It's a fixed, already-shipped, already-tested pure function
//!    (`context_key_for`) with no random initialization or
//!    convergence-order dependence — which matters directly for this
//!    item's DETERMINISTIC acceptance criterion. A k-means variant would
//!    need its own seeded-and-documented determinism story; the bucket
//!    function already has one.
//! 2. It produces exactly the kind of heterogeneous-taste split the AC's
//!    examples describe ("solo-2am", "with-kids") — a context bucket IS a
//!    named viewing situation, which is what a persona is supposed to
//!    represent, without inventing a second taxonomy.
//!
//! A future MUSEX iteration that wants genuine embedding-space k-means can
//! add it alongside this function without disturbing it — nothing here
//! assumes it's the only derivation strategy.
//!
//! ## Determinism (the AC, and how each step earns it)
//! - `context_key_for` is a pure function of `(hour, dow)` (already tested
//!   in `taste_model::profile`).
//! - Bucket assignment folds into a `BTreeMap<String, Vec<i64>>` (key-sorted
//!   iteration — never a `HashMap`), matching `compute_context_centroids`'s
//!   own posture.
//! - Each bucket's `media_item_ids` is sorted+deduped before being handed to
//!   `taste_model::profile::mean_embedding`, which itself sorts again (see
//!   its doc) — so the resulting centroid never depends on the order rows
//!   came back from Postgres (no `ORDER BY` on `list_finished_context_rows`
//!   is relied on for correctness here).
//! - `repo::persona::genre_counts_for_media_items` orders its result
//!   `ORDER BY count DESC, genre ASC` in SQL — ties break alphabetically,
//!   in the database, not on Rust-side hash iteration.
//! - `derive_explicit` sorts+dedups its caller-supplied media-item-id slice
//!   before computing anything, so passing the same *set* of ids in a
//!   different `Vec` order yields a bit-identical persona.
//!
//! See `crate::taste_model::profile`'s `mean_embedding_is_order_independent_and_bit_deterministic`
//! test for the negative-nondeterminism guard at the shared-primitive level
//! (same idiom as MUSET-05's `taste_mechanics_tests` HashMap-order
//! finding), and `crate::persona_mechanics_tests` for the DB-gated,
//! end-to-end "same inputs -> same persona vectors" determinism test this
//! module's derivation functions are exercised through.
//!
//! ## No live-system contact (S9)
//! Every function here takes an already-connected `PgPool` (the caller's
//! responsibility to obtain via the guarded snapshot path in tests, or the
//! service's normal configured pool at runtime) and never opens its own
//! connection or reads a live-fleet secret — same posture as
//! `taste_model::profile`.

use std::collections::{BTreeMap, HashMap};

use serde_json::{json, Value as Json};
use sqlx::PgPool;

use crate::error::MuseResult;
use crate::models::embedding::{Embedding, EmbeddingEntityKind, DEFAULT_EMBEDDING_MODEL};
use crate::repo;
use crate::taste_model::profile::{context_key_for, mean_embedding};

/// How many top genres [`defining_signals_json`] keeps in a persona's
/// `defining_signals.top_genres` — enough to be a useful "why this
/// persona" summary without the jsonb column growing unbounded on a
/// persona derived from a large watch history.
const TOP_GENRES_LIMIT: usize = 5;

/// The result of deriving one persona (from either path below): a name, its
/// computed taste centroid, the explainability payload
/// [`defining_signals_json`] built, and how many source media items it was
/// averaged over. Callers persist this via `repo::persona::upsert_for_account`
/// (single-account) or `insert_shared` + `add_member` (a persona spanning
/// several accounts).
#[derive(Debug, Clone)]
pub struct DerivedPersona {
    pub name: String,
    pub centroid: pgvector::Vector,
    pub defining_signals: Json,
    pub sample_size: i32,
}

/// Derive one persona PER CONTEXT BUCKET an account has finished viewing in
/// (e.g. `"weekend_evening"`, `"weekday_late_night"`) — the CLUSTERED half
/// of the AC. A bucket with no embeddable finished titles produces no
/// persona (skipped cleanly, matching `compute_context_centroids`'s own
/// "no zero-sample row" posture) rather than a persona with a meaningless
/// zero vector.
pub async fn derive_context_cluster_personas(
    pool: &PgPool,
    account_id: i64,
) -> MuseResult<Vec<DerivedPersona>> {
    let rows = repo::play_session::list_finished_context_rows(pool, account_id).await?;

    let mut by_bucket: BTreeMap<String, Vec<i64>> = BTreeMap::new();
    for row in &rows {
        if let Some(key) = context_key_for(row.started_hour, row.started_dow) {
            by_bucket.entry(key).or_default().push(row.media_item_id);
        }
    }

    let all_ids: Vec<i64> = by_bucket.values().flatten().copied().collect();
    if all_ids.is_empty() {
        return Ok(Vec::new());
    }

    let embeddings = repo::embedding::get_many(
        pool,
        EmbeddingEntityKind::MediaItem.as_str(),
        DEFAULT_EMBEDDING_MODEL,
        &all_ids,
    )
    .await?;
    let by_id: HashMap<i64, &Embedding> = embeddings.iter().map(|e| (e.entity_id, e)).collect();

    let mut personas = Vec::new();
    for (context_key, mut media_item_ids) in by_bucket {
        media_item_ids.sort_unstable();
        media_item_ids.dedup();

        let Some(centroid) = mean_embedding(&media_item_ids, &by_id) else {
            continue; // no embeddable titles in this bucket -- no persona to derive
        };

        let top_genres = repo::persona::genre_counts_for_media_items(pool, &media_item_ids).await?;
        let defining_signals =
            defining_signals_json(Some(&context_key), &top_genres, &media_item_ids);

        personas.push(DerivedPersona {
            name: context_key,
            centroid,
            defining_signals,
            sample_size: media_item_ids.len() as i32,
        });
    }
    Ok(personas)
}

/// Derive an EXPLICIT persona over a caller-chosen set of media items — an
/// operator/user declares "these titles define this persona" (e.g. a
/// curated "prestige drama" set distinct from an account's everyday
/// comfort-watch cluster) rather than it being clustered from behavioral
/// signals. Sorts+dedups `media_item_ids` before computing anything, so the
/// same *set* of ids in any `Vec` order yields a bit-identical result.
/// Returns `Ok(None)` when the id set is empty or none of the ids have a
/// stored embedding yet — never an error (matches the "skip cleanly on a
/// missing embedding" posture `taste_model::profile` uses throughout).
pub async fn derive_explicit(
    pool: &PgPool,
    name: &str,
    media_item_ids: &[i64],
) -> MuseResult<Option<DerivedPersona>> {
    let mut ids = media_item_ids.to_vec();
    ids.sort_unstable();
    ids.dedup();
    if ids.is_empty() {
        return Ok(None);
    }

    let embeddings = repo::embedding::get_many(
        pool,
        EmbeddingEntityKind::MediaItem.as_str(),
        DEFAULT_EMBEDDING_MODEL,
        &ids,
    )
    .await?;
    let by_id: HashMap<i64, &Embedding> = embeddings.iter().map(|e| (e.entity_id, e)).collect();

    let Some(centroid) = mean_embedding(&ids, &by_id) else {
        return Ok(None); // none of the declared titles have an embedding yet
    };

    let top_genres = repo::persona::genre_counts_for_media_items(pool, &ids).await?;
    let defining_signals = defining_signals_json(None, &top_genres, &ids);

    Ok(Some(DerivedPersona {
        name: name.to_string(),
        centroid,
        defining_signals,
        sample_size: ids.len() as i32,
    }))
}

/// Build the `defining_signals` jsonb payload [`Persona::explain`]
/// (`crate::persona`) reads back — `{"context_key":.., "top_genres":[...],
/// "source_media_item_ids":[...]}`. `top_genres` is truncated to
/// [`TOP_GENRES_LIMIT`], preserving the deterministic
/// count-desc/name-asc order `repo::persona::genre_counts_for_media_items`
/// already produced in SQL.
fn defining_signals_json(
    context_key: Option<&str>,
    top_genres: &[repo::persona::GenreCount],
    source_media_item_ids: &[i64],
) -> Json {
    let genres_json: Vec<Json> = top_genres
        .iter()
        .take(TOP_GENRES_LIMIT)
        .map(|g| json!({"genre": g.genre, "count": g.count}))
        .collect();
    json!({
        "context_key": context_key,
        "top_genres": genres_json,
        "source_media_item_ids": source_media_item_ids,
    })
}
