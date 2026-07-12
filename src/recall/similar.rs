//! MUSE-09: `POST /query/similar` — "more like this" for a known
//! `media_item_id`.
//!
//! Prefers the seed's own stored MUSE-08 embedding (cosine nearest-neighbor
//! via `repo::embedding::nearest`, excluding the seed itself from its own
//! result list). When the seed has no embedding yet — not embedded, or
//! Ollama was never configured — falls back to a shared-genre/metadata
//! similarity ranking (`repo::media_metadata::similar_by_genre`) so the
//! endpoint degrades to something useful rather than erroring on a title
//! that simply hasn't been through the embedding pipeline.

use std::sync::Arc;

use axum::extract::State;
use axum::Json;

use crate::error::MuseResult;
use crate::http::AppState;
use crate::models::embedding::{EmbeddingEntityKind, DEFAULT_EMBEDDING_MODEL};
use crate::repo;

use super::{clamp_limit, SimilarHit, SimilarRequest, SimilarResponse, SimilarTier};

pub async fn similar_handler(
    State(state): State<Arc<AppState>>,
    Json(req): Json<SimilarRequest>,
) -> MuseResult<Json<SimilarResponse>> {
    let limit = clamp_limit(req.limit);

    // Real errors here (seed id doesn't exist / DB down) should propagate —
    // unlike `/query/resolve`'s free-text ladder, a caller-supplied
    // `media_item_id` that doesn't resolve is a genuine 404, not a tier to
    // gracefully fall through.
    let seed_item = repo::media_item::get(&state.pool, req.media_item_id).await?;
    let seed_meta = repo::media_metadata::get(&state.pool, seed_item.media_metadata_id).await?;

    if let Some(seed_embedding) = repo::embedding::get(
        &state.pool,
        EmbeddingEntityKind::MediaItem.as_str(),
        req.media_item_id,
        DEFAULT_EMBEDDING_MODEL,
    )
    .await?
    {
        // Over-fetch by one: the seed is its own closest neighbor
        // (distance 0) and gets filtered out below.
        let matches = repo::embedding::nearest(
            &state.pool,
            EmbeddingEntityKind::MediaItem.as_str(),
            DEFAULT_EMBEDDING_MODEL,
            &seed_embedding.embedding,
            limit + 1,
        )
        .await?;

        let mut hits = Vec::new();
        for m in matches {
            if m.entity_id == req.media_item_id {
                continue;
            }
            // A stale embedding pointing at a since-deleted media_item is a
            // normal race, not a failure — skip and keep going.
            let Ok(item) = repo::media_item::get(&state.pool, m.entity_id).await else {
                continue;
            };
            let Ok(meta) = repo::media_metadata::get(&state.pool, item.media_metadata_id).await else {
                continue;
            };

            hits.push(SimilarHit {
                media_item_id: Some(item.id),
                media_metadata_id: meta.id,
                title: meta.title,
                year: meta.year,
                distance: Some(m.distance),
            });

            if hits.len() as i64 >= limit {
                break;
            }
        }

        if !hits.is_empty() {
            return Ok(Json(SimilarResponse {
                tier: SimilarTier::Vector,
                results: hits,
            }));
        }
        // The seed had an embedding but no other neighbor resolved (e.g. a
        // library of one) — fall through to the genre fallback below
        // rather than answering an empty vector-tier result.
    }

    let fallback =
        repo::media_metadata::similar_by_genre(&state.pool, seed_meta.id, seed_meta.kind, limit).await?;

    if fallback.is_empty() {
        return Ok(Json(SimilarResponse {
            tier: SimilarTier::None,
            results: Vec::new(),
        }));
    }

    let hits = fallback
        .into_iter()
        .map(|meta| SimilarHit {
            media_item_id: None,
            media_metadata_id: meta.id,
            title: meta.title,
            year: meta.year,
            distance: None,
        })
        .collect();

    Ok(Json(SimilarResponse {
        tier: SimilarTier::Genre,
        results: hits,
    }))
}
