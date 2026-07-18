//! Normalized metadata-provider seam (MUSEL-A1).
//!
//! Muse's existing `trending::client::TmdbClient` is a single, hardwired
//! source of TMDb data. This module defines a provider-agnostic
//! [`MetadataProvider`] trait plus a normalized [`ProviderMetadata`] shape
//! any concrete provider (TMDb, TheTVDB, …) can resolve into, so a future
//! resolver/aggregator (MUSEL-A2) can fan out across several configured
//! providers rather than being TMDb-specific. Read-only: a `MetadataProvider`
//! only ever *looks up* metadata, never writes to the provider.

use async_trait::async_trait;
use std::collections::HashMap;

use crate::error::MuseResult;

pub mod config;
pub mod resolve;
pub mod tvdb;

/// Which kind of title a lookup/search is for. Mirrors
/// `trending::client::TmdbMediaType`'s movie-vs-tv split, generalized across
/// providers (TheTVDB calls this "series" vs "movies").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaKind {
    Movie,
    Series,
}

/// Poster/backdrop art URLs a provider returned for a title. Every field is
/// independently nullable — not every provider (or every title on a given
/// provider) populates both.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ProviderImages {
    pub poster_url: Option<String>,
    pub backdrop_url: Option<String>,
}

/// A normalized metadata record, merged/producible from any configured
/// [`MetadataProvider`]. Every field beyond `provider_ids` is nullable —
/// providers disagree on coverage per title (MUSEL-A1 spec EDGE CASES: "a
/// title present in TVDB but missing some fields -> nullable, not an
/// error"). `provider_ids` carries every id known for this title across
/// providers (tvdb/tmdb/imdb/… — a title may accumulate more as MUSEL-A2's
/// resolver merges results from several providers).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ProviderMetadata {
    /// Provider name -> that provider's id for this title, e.g.
    /// `{"tvdb": "121361", "imdb": "tt0944947"}`.
    pub provider_ids: HashMap<String, String>,
    pub title: Option<String>,
    pub overview: Option<String>,
    pub genres: Vec<String>,
    pub images: ProviderImages,
    /// Provider rating on that provider's own scale (e.g. TVDB's 0-10
    /// average), left unnormalized here — a merge step is MUSEL-A2's concern.
    pub rating: Option<f64>,
    /// Best-effort first-aired/release date, `YYYY-MM-DD` where the
    /// provider gives one, else whatever precision it offers.
    pub first_aired: Option<String>,
    pub year: Option<i32>,
    pub network: Option<String>,
    /// TMDb-style free-text keywords (e.g. "time travel", "based on a
    /// novel"). Added post-A1 (MUSEL-A2) as an additive, `Default`-backed
    /// field. No concrete provider populates it yet — it exists so
    /// `resolve_and_merge`'s merge and `repo::media_metadata::apply_enrichment`'s
    /// persistence path are already wired for it.
    pub keywords: Vec<String>,
    /// Runtime in minutes, where the provider states one. `None` both when
    /// the provider has no runtime for this title and when the concrete
    /// [`MetadataProvider`] implementation doesn't parse it yet (as of
    /// MUSEL-C2, TheTVDB v4's shapes this crate deserializes don't carry
    /// `averageRuntime`/`runtime`). Added for MUSEL-C2's `verify_match`
    /// runtime-consistency signal (`matching::verify`).
    pub runtime_minutes: Option<i32>,
}

/// A provider of normalized title metadata. Read-only: no method on this
/// trait ever writes to the provider or to Muse's own DB — persistence is a
/// caller's concern (see MUSEL-A2's `repo::media_metadata` enrichment
/// upsert).
#[async_trait]
pub trait MetadataProvider: Send + Sync {
    /// Resolve a single title by this provider's own id (e.g. a TVDB series
    /// id). Returns `Ok(None)` for a well-formed "not found" (never an
    /// error) — a title simply absent from this provider is not a failure.
    async fn resolve_by_id(
        &self,
        kind: MediaKind,
        provider_id: &str,
    ) -> MuseResult<Option<ProviderMetadata>>;

    /// Free-text search, in the provider's own relevance order.
    async fn search(&self, query: &str, kind: MediaKind) -> MuseResult<Vec<ProviderMetadata>>;
}

/// A trivial in-memory [`MetadataProvider`] for tests (MUSEL-A2 and beyond
/// will want to fan out to several providers without hitting real HTTP).
/// Not `#[cfg(test)]`-gated: it's a small, dependency-free type other
/// crates/tests in this workspace can also use as a stand-in provider, same
/// posture as e.g. `download::DownloadClient` mocks elsewhere in this crate.
#[derive(Debug, Clone, Default)]
pub struct MockMetadataProvider {
    pub by_id: HashMap<String, ProviderMetadata>,
    pub search_results: Vec<ProviderMetadata>,
}

impl MockMetadataProvider {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_id(mut self, provider_id: impl Into<String>, metadata: ProviderMetadata) -> Self {
        self.by_id.insert(provider_id.into(), metadata);
        self
    }

    pub fn with_search_results(mut self, results: Vec<ProviderMetadata>) -> Self {
        self.search_results = results;
        self
    }
}

#[async_trait]
impl MetadataProvider for MockMetadataProvider {
    async fn resolve_by_id(
        &self,
        _kind: MediaKind,
        provider_id: &str,
    ) -> MuseResult<Option<ProviderMetadata>> {
        Ok(self.by_id.get(provider_id).cloned())
    }

    async fn search(&self, _query: &str, _kind: MediaKind) -> MuseResult<Vec<ProviderMetadata>> {
        Ok(self.search_results.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn mock_provider_resolves_by_id() {
        let metadata = ProviderMetadata {
            title: Some("Test Show".to_string()),
            ..Default::default()
        };
        let provider = MockMetadataProvider::new().with_id("121361", metadata.clone());

        let resolved = provider
            .resolve_by_id(MediaKind::Series, "121361")
            .await
            .expect("resolve should not error");

        assert_eq!(resolved, Some(metadata));
    }

    #[tokio::test]
    async fn mock_provider_resolve_by_id_none_for_unknown_id() {
        let provider = MockMetadataProvider::new();
        let resolved = provider
            .resolve_by_id(MediaKind::Movie, "nope")
            .await
            .expect("resolve should not error");
        assert!(resolved.is_none());
    }

    #[tokio::test]
    async fn mock_provider_search_returns_configured_results() {
        let metadata = ProviderMetadata {
            title: Some("Arrival".to_string()),
            ..Default::default()
        };
        let provider = MockMetadataProvider::new().with_search_results(vec![metadata.clone()]);

        let results = provider
            .search("arrival", MediaKind::Movie)
            .await
            .expect("search should not error");

        assert_eq!(results, vec![metadata]);
    }
}
