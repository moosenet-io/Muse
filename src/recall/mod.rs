//! MUSE-09: vector recall + search API/tools (spec §S96, MUSE-09).
//!
//! Two axum-facing surfaces, both assistant-speed (private, single-account
//! Phase-0 lookups, not a public search engine):
//!
//! - [`resolve::resolve_handler`] — `POST /query/resolve`: "that space
//!   linguist movie" → the right `media_item`/`media_metadata`, via a
//!   resolution ladder that degrades gracefully rung by rung:
//!   1. **vector** — library-vector-first ANN over the MUSE-08 embeddings
//!      (`embed::nearest`), confidence-gated by
//!      `Config::recall_vector_max_distance`.
//!   2. **trigram** — `repo::media_metadata::search_by_title` (pg_trgm),
//!      when the vector tier is unavailable (no Ollama configured) or
//!      didn't produce a confident match.
//!   3. **tmdb** — only when the caller opts in (`include_tmdb: true`), a
//!      TMDb lookup beyond the library, clearly marked as "not in your
//!      library" in the response.
//! - [`similar::similar_handler`] — `POST /query/similar`: "more like this"
//!   for a known `media_item_id`, preferring its own stored embedding
//!   (excluding itself from its own neighbor list) and falling back to
//!   shared-genre/metadata similarity when the seed has no embedding yet.
//!
//! ## Graceful degradation (mandatory, per spec)
//! No tier here ever turns an unavailable dependency into a 500. An
//! unconfigured Ollama client, an unconfigured TMDb key, or a transient
//! upstream failure all collapse to "this tier found nothing," which the
//! ladder ([`run_ladder`]) treats identically to a tier that legitimately
//! found no confident match — the next rung fires instead. The only errors
//! that propagate to the HTTP layer are ones intrinsic to the request
//! itself (e.g. `/query/similar` given a `media_item_id` that doesn't
//! exist) or genuine Postgres failures, via the usual [`crate::error::MuseError`]
//! `IntoResponse` mapping.

#[cfg(test)]
mod live_tests;
pub mod resolve;
pub mod similar;

pub use resolve::resolve_handler;
pub use similar::similar_handler;

use serde::{Deserialize, Serialize};

/// Default `limit` when a caller doesn't specify one.
pub const DEFAULT_RESOLVE_LIMIT: i64 = 10;
/// Hard ceiling on `limit`, regardless of what the caller asks for —
/// assistant-speed lookups, not a paginated browse API.
pub const MAX_RESOLVE_LIMIT: i64 = 50;

/// Which rung of the `/query/resolve` ladder produced the returned results
/// (or that none did).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResolveTier {
    Vector,
    Trigram,
    Tmdb,
    None,
}

/// One `/query/resolve` hit. The `source` tag tells the caller (and the
/// spec's "clearly marked as not in your library" requirement for the TMDb
/// tier) which rung produced it.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "source", rename_all = "snake_case")]
pub enum ResolveHit {
    Vector {
        media_item_id: i64,
        media_metadata_id: i64,
        title: String,
        year: Option<i32>,
        /// pgvector cosine distance (`embedding <=> query`); lower is
        /// closer. Included so a caller with its own confidence policy can
        /// re-filter without a second round trip.
        distance: f64,
    },
    Trigram {
        media_metadata_id: i64,
        title: String,
        year: Option<i32>,
    },
    Tmdb {
        tmdb_id: String,
        media_type: Option<String>,
        title: String,
        year: Option<i32>,
        note: String,
    },
}

#[derive(Debug, Deserialize)]
pub struct ResolveRequest {
    pub query: String,
    #[serde(default)]
    pub limit: Option<i64>,
    /// Opt-in beyond-the-library lookup: the ladder only ever reaches the
    /// TMDb tier when this is `true` (default `false`) — per spec, that
    /// tier fires "only if the caller opts in."
    #[serde(default)]
    pub include_tmdb: bool,
}

#[derive(Debug, Serialize)]
pub struct ResolveResponse {
    pub tier: ResolveTier,
    pub results: Vec<ResolveHit>,
}

#[derive(Debug, Deserialize)]
pub struct SimilarRequest {
    pub media_item_id: i64,
    #[serde(default)]
    pub limit: Option<i64>,
}

/// Which strategy produced a `/query/similar` result set.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SimilarTier {
    Vector,
    Genre,
    None,
}

#[derive(Debug, Clone, Serialize)]
pub struct SimilarHit {
    /// `None` for a genre-fallback hit that hasn't (yet) been added to a
    /// library instance — it's a `media_metadata`-only match.
    pub media_item_id: Option<i64>,
    pub media_metadata_id: i64,
    pub title: String,
    pub year: Option<i32>,
    /// `Some` only for vector-tier hits; the genre fallback has no distance
    /// metric to report.
    pub distance: Option<f64>,
}

#[derive(Debug, Serialize)]
pub struct SimilarResponse {
    pub tier: SimilarTier,
    pub results: Vec<SimilarHit>,
}

/// Clamp a caller-supplied `limit` into `[1, MAX_RESOLVE_LIMIT]`, defaulting
/// to [`DEFAULT_RESOLVE_LIMIT`] when unset. Never panics, never returns 0
/// (a 0-limit request would look identical to "found nothing" downstream).
pub(crate) fn clamp_limit(requested: Option<i64>) -> i64 {
    requested
        .unwrap_or(DEFAULT_RESOLVE_LIMIT)
        .clamp(1, MAX_RESOLVE_LIMIT)
}

/// The resolution ladder's tier-SELECTION logic (MUSE-09), deliberately
/// decoupled from I/O so it's unit-testable with canned/faked tier
/// closures — the "which tier fires when" contract the spec calls out.
///
/// Runs each tier in order and stops at the first that returns a non-empty
/// `Vec`: vector, then trigram, then (only when `tmdb_enabled`) tmdb. A
/// closure returning an empty `Vec` means "skip me," for any reason
/// (dependency unconfigured, no confident match, empty library) — the
/// ladder doesn't need to know why, only whether to try the next rung.
/// Each closure is invoked at most once, and only if the ladder actually
/// reaches it, so a real caller's TMDb network call never fires when the
/// vector or trigram tier already answered.
pub async fn run_ladder<FutV, FutT, FutM>(
    vector: impl FnOnce() -> FutV,
    trigram: impl FnOnce() -> FutT,
    tmdb_enabled: bool,
    tmdb: impl FnOnce() -> FutM,
) -> (ResolveTier, Vec<ResolveHit>)
where
    FutV: std::future::Future<Output = Vec<ResolveHit>>,
    FutT: std::future::Future<Output = Vec<ResolveHit>>,
    FutM: std::future::Future<Output = Vec<ResolveHit>>,
{
    let hits = vector().await;
    if !hits.is_empty() {
        return (ResolveTier::Vector, hits);
    }

    let hits = trigram().await;
    if !hits.is_empty() {
        return (ResolveTier::Trigram, hits);
    }

    if tmdb_enabled {
        let hits = tmdb().await;
        if !hits.is_empty() {
            return (ResolveTier::Tmdb, hits);
        }
    }

    (ResolveTier::None, Vec::new())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn hit(n: i64) -> ResolveHit {
        ResolveHit::Trigram {
            media_metadata_id: n,
            title: format!("Item {n}"),
            year: None,
        }
    }

    #[tokio::test]
    async fn vector_tier_wins_when_it_has_confident_hits() {
        let trigram_calls = AtomicUsize::new(0);
        let tmdb_calls = AtomicUsize::new(0);

        let (tier, hits) = run_ladder(
            || async { vec![hit(1)] },
            || {
                trigram_calls.fetch_add(1, Ordering::SeqCst);
                async { vec![hit(2)] }
            },
            true,
            || {
                tmdb_calls.fetch_add(1, Ordering::SeqCst);
                async { vec![hit(3)] }
            },
        )
        .await;

        assert_eq!(tier, ResolveTier::Vector);
        assert_eq!(hits.len(), 1);
        assert_eq!(
            trigram_calls.load(Ordering::SeqCst),
            0,
            "trigram tier must not run once the vector tier answers confidently"
        );
        assert_eq!(
            tmdb_calls.load(Ordering::SeqCst),
            0,
            "tmdb tier must not run once the vector tier answers confidently"
        );
    }

    #[tokio::test]
    async fn falls_through_to_trigram_when_vector_is_empty() {
        let tmdb_calls = AtomicUsize::new(0);

        let (tier, hits) = run_ladder(
            || async { Vec::new() }, // e.g. no Ollama configured, or below the confidence bar
            || async { vec![hit(2)] },
            true,
            || {
                tmdb_calls.fetch_add(1, Ordering::SeqCst);
                async { vec![hit(3)] }
            },
        )
        .await;

        assert_eq!(tier, ResolveTier::Trigram);
        assert_eq!(hits.len(), 1);
        assert_eq!(
            tmdb_calls.load(Ordering::SeqCst),
            0,
            "tmdb tier must not run once the trigram tier answers"
        );
    }

    #[tokio::test]
    async fn falls_through_to_tmdb_when_vector_and_trigram_are_empty_and_opted_in() {
        let (tier, hits) = run_ladder(
            || async { Vec::new() },
            || async { Vec::new() },
            true,
            || async { vec![hit(3)] },
        )
        .await;

        assert_eq!(tier, ResolveTier::Tmdb);
        assert_eq!(hits.len(), 1);
    }

    #[tokio::test]
    async fn never_reaches_tmdb_when_caller_did_not_opt_in() {
        let tmdb_calls = AtomicUsize::new(0);

        let (tier, hits) = run_ladder(
            || async { Vec::new() },
            || async { Vec::new() },
            false, // include_tmdb: false
            || {
                tmdb_calls.fetch_add(1, Ordering::SeqCst);
                async { vec![hit(3)] }
            },
        )
        .await;

        assert_eq!(tier, ResolveTier::None);
        assert!(hits.is_empty());
        assert_eq!(
            tmdb_calls.load(Ordering::SeqCst),
            0,
            "the tmdb closure must never even run when the caller didn't opt in"
        );
    }

    #[tokio::test]
    async fn degrades_to_none_when_every_tier_is_empty() {
        let (tier, hits) = run_ladder(
            || async { Vec::new() },
            || async { Vec::new() },
            true,
            || async { Vec::new() },
        )
        .await;

        assert_eq!(tier, ResolveTier::None);
        assert!(hits.is_empty());
    }

    #[test]
    fn clamp_limit_defaults_and_bounds() {
        assert_eq!(clamp_limit(None), DEFAULT_RESOLVE_LIMIT);
        assert_eq!(clamp_limit(Some(0)), 1);
        assert_eq!(clamp_limit(Some(-5)), 1);
        assert_eq!(clamp_limit(Some(1000)), MAX_RESOLVE_LIMIT);
        assert_eq!(clamp_limit(Some(5)), 5);
    }
}
