//! MUSE-09: `POST /query/resolve` — the resolution-ladder handler + its
//! three tier implementations. See `super`'s module docs for the ladder
//! contract; this file is the real (I/O-performing) side of it, wired
//! through `super::run_ladder`.

use std::sync::Arc;

use axum::extract::State;
use axum::Json;

use crate::error::MuseResult;
use crate::http::AppState;
use crate::models::embedding::DEFAULT_EMBEDDING_MODEL;
use crate::repo;

use super::{clamp_limit, run_ladder, ResolveHit, ResolveRequest, ResolveResponse, ResolveTier};

pub async fn resolve_handler(
    State(state): State<Arc<AppState>>,
    Json(req): Json<ResolveRequest>,
) -> MuseResult<Json<ResolveResponse>> {
    let query = req.query.trim();
    if query.is_empty() {
        // An empty query can never resolve to anything meaningful; answer
        // cleanly rather than spending a round trip on every tier.
        return Ok(Json(ResolveResponse {
            tier: ResolveTier::None,
            results: Vec::new(),
        }));
    }

    let limit = clamp_limit(req.limit);
    let include_tmdb = req.include_tmdb;

    let (tier, results) = run_ladder(
        || vector_tier(&state, query, limit),
        || trigram_tier(&state, query, limit),
        include_tmdb,
        || tmdb_tier(&state, query, limit),
    )
    .await;

    Ok(Json(ResolveResponse { tier, results }))
}

/// Tier 1: library-vector-first ANN over the MUSE-08 embeddings.
///
/// Returns an empty `Vec` (never propagates an error to the ladder) when:
/// Ollama isn't configured (`state.embed.is_none()`), the embed call fails,
/// the nearest-neighbor query fails, or every candidate's cosine distance
/// exceeds `Config::recall_vector_max_distance` — none of those are hard
/// failures for `/query/resolve` as a whole, they just mean "try the next
/// rung."
async fn vector_tier(state: &Arc<AppState>, query: &str, limit: i64) -> Vec<ResolveHit> {
    let Some(client) = &state.embed else {
        return Vec::new();
    };

    let vector = match client.embed(DEFAULT_EMBEDDING_MODEL, query).await {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(error = %e, "MUSE-09: query embedding failed; degrading to next recall tier");
            return Vec::new();
        }
    };

    let matches = match crate::embed::nearest(&state.pool, vector, limit).await {
        Ok(m) => m,
        Err(e) => {
            tracing::warn!(error = %e, "MUSE-09: vector nearest-neighbor lookup failed; degrading to next recall tier");
            return Vec::new();
        }
    };

    let mut hits = Vec::with_capacity(matches.len());
    for m in matches {
        if m.distance > state.config.recall_vector_max_distance {
            // Ascending-distance order (see `repo::embedding::nearest`) means
            // every subsequent match is at least as far — nothing further
            // in the list can be confident either.
            break;
        }

        let media_item_id = m.entity_id;
        // A stale embedding row pointing at a since-deleted media_item is a
        // normal race (library reorganized after the last embed pass), not
        // a failure — skip it and keep going rather than failing the tier.
        let Ok(item) = repo::media_item::get(&state.pool, media_item_id).await else {
            continue;
        };
        let Ok(meta) = repo::media_metadata::get(&state.pool, item.media_metadata_id).await else {
            continue;
        };

        hits.push(ResolveHit::Vector {
            media_item_id,
            media_metadata_id: meta.id,
            title: meta.title,
            year: meta.year,
            distance: m.distance,
        });
    }

    hits
}

/// Tier 2: pg_trgm fuzzy title search, scoped to the library's own
/// `media_metadata`. Fires when the vector tier is unavailable or
/// unconfident.
async fn trigram_tier(state: &Arc<AppState>, query: &str, limit: i64) -> Vec<ResolveHit> {
    match repo::media_metadata::search_by_title(&state.pool, query, limit).await {
        Ok(rows) => rows
            .into_iter()
            .map(|m| ResolveHit::Trigram {
                media_metadata_id: m.id,
                title: m.title,
                year: m.year,
            })
            .collect(),
        Err(e) => {
            tracing::warn!(error = %e, "MUSE-09: pg_trgm title search failed; degrading to next recall tier");
            Vec::new()
        }
    }
}

/// Tier 3: TMDb lookup beyond the library. Only ever invoked by the ladder
/// when the caller opted in (`include_tmdb: true`); every hit is tagged
/// with an explicit "not in your library" note per spec.
async fn tmdb_tier(state: &Arc<AppState>, query: &str, limit: i64) -> Vec<ResolveHit> {
    let Some(client) = &state.tmdb else {
        return Vec::new();
    };

    let results = match client.search_multi(query).await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(error = %e, "MUSE-09: tmdb search_multi failed; recall ladder exhausted");
            return Vec::new();
        }
    };

    results
        .into_iter()
        .filter_map(|t| {
            let title = t.display_title()?.to_string();
            Some(ResolveHit::Tmdb {
                tmdb_id: t.id.to_string(),
                media_type: t.media_type.clone(),
                title,
                year: t.year(),
                note: "not in your library — found on TMDb".to_string(),
            })
        })
        .take(limit.max(0) as usize)
        .collect()
}
