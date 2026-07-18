//! MUSEL-A2 — the provider-resolution + enrichment aggregator.
//!
//! [`resolve_and_merge`] is the fan-out-then-merge core: given a title's
//! already-known provider ids (from arr ingest, a filename parse, or an
//! existing `media_metadata` row) plus a set of *named*, configured
//! [`MetadataProvider`]s, it calls each provider's own
//! [`MetadataProvider::resolve_by_id`], merges what came back into one
//! [`ProviderMetadata`] with a documented, deterministic precedence, and
//! never fails just because a provider is absent, down, or has nothing for
//! this id. Persistence onto `media_metadata` is a separate concern (see
//! `repo::media_metadata::apply_enrichment`) — this module's output is pure
//! data, not a DB write.
//!
//! ## Precedence (ARR-BLUEPRINT §7.7)
//! - **Movies**: `tmdb` is primary (Radarr's own provider-id precedence).
//! - **TV/anime**: `tvdb` is primary (Sonarr's own provider-id precedence).
//! - A field the primary provider didn't populate (`None`/empty) is
//!   gap-filled from the next provider that has it, in fan-out order.
//! - `provider_ids` is a **union** across every provider that resolved,
//!   never truncated to one — anime titles alone can carry 5+ ids
//!   (tvdb/tmdb/imdb/tvmaze/mal/anilist per the blueprint) and every one of
//!   them is worth keeping regardless of which provider is "primary" for
//!   this `MediaKind`.
//! - A conflicting scalar field (primary and a secondary provider disagree)
//!   keeps the primary's value; the disagreement is logged at `debug` so
//!   it's visible without being noisy.
//!
//! ## Graceful degrade
//! - A provider absent from the `providers` slice at all (the caller
//!   already skips ones whose `from_config` returned `None`) simply
//!   doesn't contribute.
//! - A provider present but with no id known for it (e.g. no `tvdb_id` on
//!   this title) is skipped for the id-based pass.
//! - A provider that returns `Ok(None)` (well-formed "not found") or
//!   `Err(_)` (mid-fan-out failure) is skipped; the others still merge.
//!   An error is logged at `warn`, a `None` at `debug`.
//! - `providers` empty -> `Ok(None)`, not an error.
//!
//! ## The title-search fallback is NARROWLY scoped (review finding, S119b)
//! The lowest-confidence title-search fallback is **only** attempted when
//! `ids` carries **no known provider id at all**
//! (`ResolveIds::provider_ids.is_empty()`). If the caller *did* supply one
//! or more known ids and every one of them failed to resolve (each
//! provider returned `Ok(None)` or errored), [`resolve_and_merge`] returns
//! `Ok(None)` — it does **not** fall back to a title search in that case.
//! Guessing by title when a specific id was supplied but came up empty
//! would risk silently attaching an unrelated title's data to a real,
//! specific id (e.g. a stale/wrong `tmdb_id` on the row) — exactly the
//! "wrong-confident match" the spec's edge cases forbid. Only the true
//! "we have nothing to go on but a title" case gets the fallback.
//!
//! When the fallback IS taken, the result is wrapped in
//! [`ResolvedMetadata`] with [`MatchConfidence::TitleSearch`] rather than
//! [`MatchConfidence::Id`] — see that type's doc. Callers that persist a
//! [`ResolvedMetadata`] (see `repo::media_metadata::apply_enrichment`'s
//! caller in `maintenance::run_metadata_resolve_pass`) MUST check
//! `confidence` and treat `TitleSearch` as tentative, never as an
//! authoritative overwrite.

use std::collections::HashMap;

use crate::error::MuseResult;

use super::{MediaKind, MetadataProvider, ProviderMetadata};

/// How [`resolve_and_merge`] arrived at a [`ResolvedMetadata`] result.
/// Exists specifically so a persistence caller can tell an authoritative
/// id-based match apart from a lowest-confidence title-search guess —
/// `ProviderMetadata` itself carries no such marker (it's the same
/// normalized shape either way), so this lives on the wrapper instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatchConfidence {
    /// At least one configured provider resolved this title via its own
    /// id lookup (`MetadataProvider::resolve_by_id`). Safe to persist as
    /// an authoritative enrichment.
    Id,
    /// No known id resolved anything (or none was supplied); this is a
    /// first-hit, free-text title search result. NEVER an authoritative
    /// match — a caller persisting this should treat it as tentative at
    /// best, or skip persisting it entirely (see
    /// `maintenance::run_metadata_resolve_pass`, which skips it).
    TitleSearch,
}

/// [`resolve_and_merge`]'s output: the merged metadata plus how confident
/// the match is. `None` from `resolve_and_merge` itself (not a variant
/// here) covers "nothing resolved at all" — see that function's doc.
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedMetadata {
    pub metadata: ProviderMetadata,
    pub confidence: MatchConfidence,
}

/// Provider name for TMDb entries in [`ProviderMetadata::provider_ids`] /
/// the `providers` slice passed to [`resolve_and_merge`]. Matches the keys
/// `TvdbClient::into_metadata`'s remote-id bridge already uses.
pub const TMDB: &str = "tmdb";
/// Provider name for TheTVDB. See [`TMDB`].
pub const TVDB: &str = "tvdb";
/// Provider name for a bare IMDb id — never itself a fan-out target (no
/// `MetadataProvider` speaks "imdb" natively), but a recognized key in
/// [`ResolveIds::provider_ids`] / merged `provider_ids` output, and the id
/// namespace the MUSEL-A2 TMDb adapter (`trending::client`) bridges via
/// `/find`.
pub const IMDB: &str = "imdb";

/// The known provider ids for a title going into [`resolve_and_merge`],
/// plus a `title` for the no-known-ids fallback. Keyed by provider name
/// (see [`TMDB`]/[`TVDB`]/[`IMDB`]) so it generalizes to future providers
/// without another named-field addition — a title's TVMaze/MAL/AniList ids
/// (anime, per the blueprint) flow through the same map even though no
/// `MetadataProvider` for them exists yet.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ResolveIds {
    pub provider_ids: HashMap<String, String>,
    /// Fallback query for the "no known ids" path. Never used when any
    /// provider produces an id-based hit.
    pub title: Option<String>,
}

impl ResolveIds {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_id(mut self, provider: impl Into<String>, id: impl Into<String>) -> Self {
        self.provider_ids.insert(provider.into(), id.into());
        self
    }

    pub fn with_title(mut self, title: impl Into<String>) -> Self {
        self.title = Some(title.into());
        self
    }

    /// The id [`resolve_and_merge`] should try for a provider named
    /// `provider_name`: that provider's own id if known, else the `imdb`
    /// id as a bridge candidate (only the MUSEL-A2 TMDb adapter currently
    /// understands an IMDb-shaped ("tt...") id passed to
    /// [`MetadataProvider::resolve_by_id`] — TheTVDB has no analogous
    /// lookup-by-imdb-id call, so passing it there would just be a
    /// guaranteed miss, which is still graceful: `TvdbClient::resolve_by_id`
    /// returns `Ok(None)` for an id it doesn't recognize, never an error).
    fn id_for(&self, provider_name: &str) -> Option<&str> {
        self.provider_ids
            .get(provider_name)
            .or_else(|| self.provider_ids.get(IMDB))
            .map(String::as_str)
    }
}

/// One configured provider, named so [`resolve_and_merge`] can apply the
/// precedence rule (which name is "primary" depends on `MediaKind`) and
/// pick the right id out of [`ResolveIds`] for it.
pub struct NamedProvider<'a> {
    pub name: &'static str,
    pub provider: &'a dyn MetadataProvider,
}

impl<'a> NamedProvider<'a> {
    pub fn new(name: &'static str, provider: &'a dyn MetadataProvider) -> Self {
        Self { name, provider }
    }
}

fn primary_provider_name(kind: MediaKind) -> &'static str {
    match kind {
        MediaKind::Movie => TMDB,
        MediaKind::Series => TVDB,
    }
}

/// Fan out to every provider in `providers`, resolve/merge into one
/// [`ResolvedMetadata`]. See the module doc for the full precedence,
/// graceful-degrade, and title-search-scoping rules. Never returns `Err`
/// for a provider-side failure — only a truly unexpected internal error
/// would surface as `Err` here, and no such path currently exists (kept
/// `MuseResult` for forward compatibility with a future provider whose
/// `resolve_by_id` contract might need to short-circuit the whole
/// resolve).
///
/// Returns `Ok(None)` when nothing resolved: no providers configured, no
/// known ids AND no title, or — critically — known ids WERE supplied but
/// every one of them came back empty/erroring (see the module doc's
/// "title-search fallback is NARROWLY scoped" section for why that case
/// does NOT fall back to a title guess).
pub async fn resolve_and_merge(
    ids: &ResolveIds,
    kind: MediaKind,
    providers: &[NamedProvider<'_>],
) -> MuseResult<Option<ResolvedMetadata>> {
    if providers.is_empty() {
        tracing::debug!("resolve_and_merge: no providers configured; clean no-op");
        return Ok(None);
    }

    let have_known_ids = !ids.provider_ids.is_empty();

    if have_known_ids {
        let mut hits: Vec<(&str, ProviderMetadata)> = Vec::new();

        for np in providers {
            let Some(id) = ids.id_for(np.name) else {
                tracing::debug!(provider = np.name, "resolve_and_merge: no known id for this provider; skipping");
                continue;
            };

            match np.provider.resolve_by_id(kind, id).await {
                Ok(Some(meta)) => hits.push((np.name, meta)),
                Ok(None) => {
                    tracing::debug!(provider = np.name, id, "resolve_and_merge: provider has no record for this id");
                }
                Err(e) => {
                    tracing::warn!(
                        provider = np.name,
                        id,
                        error = %e,
                        "resolve_and_merge: provider errored mid-fan-out; skipping (graceful degrade)"
                    );
                }
            }
        }

        if hits.is_empty() {
            // Known ids were supplied but NONE of them resolved — do NOT
            // fall back to a title search here (see module doc): that
            // would risk attaching an unrelated title's data to a row
            // that carries a specific (if currently unresolvable) id.
            // Unresolved, not an error, not a guess.
            tracing::debug!(
                "resolve_and_merge: known ids were supplied but none resolved; returning unresolved \
                 (not falling back to a title guess against a specific-id row)"
            );
            return Ok(None);
        }

        return Ok(Some(ResolvedMetadata {
            metadata: merge_metadata(kind, hits),
            confidence: MatchConfidence::Id,
        }));
    }

    // No known ids at all — the only case the title-search fallback is
    // allowed to run in.
    let Some(title) = ids.title.as_deref().filter(|t| !t.trim().is_empty()) else {
        tracing::debug!("resolve_and_merge: no known ids and no title to fall back to; clean no-op");
        return Ok(None);
    };

    let mut hits: Vec<(&str, ProviderMetadata)> = Vec::new();
    for np in providers {
        match np.provider.search(title, kind).await {
            Ok(results) => {
                if let Some(first) = results.into_iter().next() {
                    tracing::warn!(
                        provider = np.name,
                        title,
                        "resolve_and_merge: no known id for any provider; falling back to lowest-confidence \
                         title search — flagged MatchConfidence::TitleSearch, never treated as a confident \
                         id-based match by a persistence caller"
                    );
                    hits.push((np.name, first));
                }
            }
            Err(e) => tracing::warn!(
                provider = np.name,
                title,
                error = %e,
                "resolve_and_merge: fallback title search failed; skipping"
            ),
        }
    }

    if hits.is_empty() {
        return Ok(None);
    }

    Ok(Some(ResolvedMetadata {
        metadata: merge_metadata(kind, hits),
        confidence: MatchConfidence::TitleSearch,
    }))
}

/// The pure merge step: primary-provider-wins-ties, gap-fill from the
/// rest, union `provider_ids`. `hits` is in fan-out order (whatever order
/// `providers` was given in) — reordered internally so the primary
/// provider for `kind` (if it produced a hit) is applied first.
fn merge_metadata(kind: MediaKind, hits: Vec<(&str, ProviderMetadata)>) -> ProviderMetadata {
    if hits.is_empty() {
        return ProviderMetadata::default();
    }

    let primary_name = primary_provider_name(kind);
    let mut ordered = hits;
    if let Some(pos) = ordered.iter().position(|(name, _)| *name == primary_name) {
        let primary = ordered.remove(pos);
        ordered.insert(0, primary);
    }

    let mut merged = ProviderMetadata::default();

    for (name, meta) in ordered {
        for (k, v) in &meta.provider_ids {
            merged.provider_ids.entry(k.clone()).or_insert_with(|| v.clone());
        }

        set_or_log_conflict(&mut merged.title, meta.title, name, "title");
        set_or_log_conflict(&mut merged.overview, meta.overview, name, "overview");
        set_or_log_conflict(&mut merged.rating, meta.rating, name, "rating");
        set_or_log_conflict(&mut merged.first_aired, meta.first_aired, name, "first_aired");
        set_or_log_conflict(&mut merged.year, meta.year, name, "year");
        set_or_log_conflict(&mut merged.network, meta.network, name, "network");
        set_or_log_conflict(&mut merged.images.poster_url, meta.images.poster_url, name, "poster_url");
        set_or_log_conflict(
            &mut merged.images.backdrop_url,
            meta.images.backdrop_url,
            name,
            "backdrop_url",
        );

        if merged.genres.is_empty() && !meta.genres.is_empty() {
            merged.genres = meta.genres;
        }
        if merged.keywords.is_empty() && !meta.keywords.is_empty() {
            merged.keywords = meta.keywords;
        }
    }

    merged
}

/// Fills `slot` from `incoming` only if `slot` is currently unset
/// (gap-fill); if both are set and disagree, keeps `slot` (the
/// higher-precedence provider already applied, since `merge_metadata`
/// visits providers in primary-first order) and logs the disagreement at
/// `debug`.
fn set_or_log_conflict<T: PartialEq + std::fmt::Debug>(
    slot: &mut Option<T>,
    incoming: Option<T>,
    provider: &str,
    field: &'static str,
) {
    match (&slot, incoming) {
        (Some(existing), Some(new)) if *existing != new => {
            tracing::debug!(
                provider,
                field,
                kept = ?existing,
                discarded = ?new,
                "resolve_and_merge: field conflict across providers — precedence kept the higher-priority value"
            );
        }
        (None, Some(new)) => *slot = Some(new),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metadata::{MockMetadataProvider, ProviderImages};

    fn meta(title: &str) -> ProviderMetadata {
        ProviderMetadata {
            title: Some(title.to_string()),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn no_providers_is_a_clean_no_op() {
        let ids = ResolveIds::new().with_id(TMDB, "603");
        let result = resolve_and_merge(&ids, MediaKind::Movie, &[]).await.unwrap();
        assert_eq!(result, None);
    }

    #[tokio::test]
    async fn no_known_ids_and_no_title_is_a_clean_no_op() {
        let tmdb = MockMetadataProvider::new();
        let providers = [NamedProvider::new(TMDB, &tmdb)];
        let result = resolve_and_merge(&ResolveIds::new(), MediaKind::Movie, &providers)
            .await
            .unwrap();
        assert_eq!(result, None);
    }

    #[tokio::test]
    async fn single_provider_resolves() {
        let tmdb = MockMetadataProvider::new().with_id("603", meta("The Matrix"));
        let providers = [NamedProvider::new(TMDB, &tmdb)];
        let ids = ResolveIds::new().with_id(TMDB, "603");

        let result = resolve_and_merge(&ids, MediaKind::Movie, &providers).await.unwrap().unwrap();
        assert_eq!(result.metadata.title, Some("The Matrix".to_string()));
        assert_eq!(result.confidence, MatchConfidence::Id);
    }

    #[tokio::test]
    async fn movies_prefer_tmdb_on_conflicting_title() {
        let tmdb = MockMetadataProvider::new().with_id("603", meta("The Matrix (TMDb)"));
        let tvdb = MockMetadataProvider::new().with_id("81", meta("The Matrix (TVDB)"));
        // Fan-out order deliberately lists tvdb first — precedence must
        // still pick tmdb for a movie regardless of fan-out order.
        let providers = [NamedProvider::new(TVDB, &tvdb), NamedProvider::new(TMDB, &tmdb)];
        let ids = ResolveIds::new().with_id(TMDB, "603").with_id(TVDB, "81");

        let result = resolve_and_merge(&ids, MediaKind::Movie, &providers).await.unwrap().unwrap();
        assert_eq!(result.metadata.title, Some("The Matrix (TMDb)".to_string()));
    }

    #[tokio::test]
    async fn tv_prefers_tvdb_on_conflicting_title() {
        let tmdb = MockMetadataProvider::new().with_id("1399", meta("GoT (TMDb)"));
        let tvdb = MockMetadataProvider::new().with_id("121361", meta("GoT (TVDB)"));
        let providers = [NamedProvider::new(TMDB, &tmdb), NamedProvider::new(TVDB, &tvdb)];
        let ids = ResolveIds::new().with_id(TMDB, "1399").with_id(TVDB, "121361");

        let result = resolve_and_merge(&ids, MediaKind::Series, &providers).await.unwrap().unwrap();
        assert_eq!(result.metadata.title, Some("GoT (TVDB)".to_string()));
    }

    #[tokio::test]
    async fn gap_fill_from_secondary_when_primary_missing_a_field() {
        let tmdb_meta = ProviderMetadata {
            title: Some("The Matrix".to_string()),
            overview: None, // primary doesn't have it
            ..Default::default()
        };
        let tvdb_meta = ProviderMetadata {
            title: Some("The Matrix (TVDB)".to_string()),
            overview: Some("A hacker discovers reality is a simulation.".to_string()),
            ..Default::default()
        };
        let tmdb = MockMetadataProvider::new().with_id("603", tmdb_meta);
        let tvdb = MockMetadataProvider::new().with_id("81", tvdb_meta);
        let providers = [NamedProvider::new(TMDB, &tmdb), NamedProvider::new(TVDB, &tvdb)];
        let ids = ResolveIds::new().with_id(TMDB, "603").with_id(TVDB, "81");

        let result = resolve_and_merge(&ids, MediaKind::Movie, &providers).await.unwrap().unwrap();
        // primary (tmdb) wins the title...
        assert_eq!(result.metadata.title, Some("The Matrix".to_string()));
        // ...but overview gap-fills from tvdb since tmdb didn't have one.
        assert_eq!(
            result.metadata.overview,
            Some("A hacker discovers reality is a simulation.".to_string())
        );
    }

    #[tokio::test]
    async fn provider_ids_are_unioned_across_providers() {
        let mut tmdb_meta = meta("The Matrix");
        tmdb_meta.provider_ids.insert(TMDB.to_string(), "603".to_string());
        let mut tvdb_meta = meta("The Matrix (TVDB)");
        tvdb_meta.provider_ids.insert(TVDB.to_string(), "81".to_string());
        tvdb_meta.provider_ids.insert(IMDB.to_string(), "tt0133093".to_string());

        let tmdb = MockMetadataProvider::new().with_id("603", tmdb_meta);
        let tvdb = MockMetadataProvider::new().with_id("81", tvdb_meta);
        let providers = [NamedProvider::new(TMDB, &tmdb), NamedProvider::new(TVDB, &tvdb)];
        let ids = ResolveIds::new().with_id(TMDB, "603").with_id(TVDB, "81");

        let result = resolve_and_merge(&ids, MediaKind::Movie, &providers).await.unwrap().unwrap();
        assert_eq!(result.metadata.provider_ids.get(TMDB), Some(&"603".to_string()));
        assert_eq!(result.metadata.provider_ids.get(TVDB), Some(&"81".to_string()));
        assert_eq!(result.metadata.provider_ids.get(IMDB), Some(&"tt0133093".to_string()));
    }

    #[tokio::test]
    async fn down_provider_is_skipped_others_still_merge() {
        struct AlwaysErrorsProvider;
        #[async_trait::async_trait]
        impl MetadataProvider for AlwaysErrorsProvider {
            async fn resolve_by_id(
                &self,
                _kind: MediaKind,
                _provider_id: &str,
            ) -> MuseResult<Option<ProviderMetadata>> {
                Err(crate::error::MuseError::Upstream {
                    status: 503,
                    message: "simulated tvdb outage".to_string(),
                })
            }
            async fn search(&self, _query: &str, _kind: MediaKind) -> MuseResult<Vec<ProviderMetadata>> {
                Ok(vec![])
            }
        }

        let down_tvdb = AlwaysErrorsProvider;
        let tmdb = MockMetadataProvider::new().with_id("603", meta("The Matrix"));
        let providers = [NamedProvider::new(TVDB, &down_tvdb), NamedProvider::new(TMDB, &tmdb)];
        let ids = ResolveIds::new().with_id(TMDB, "603").with_id(TVDB, "81");

        let result = resolve_and_merge(&ids, MediaKind::Movie, &providers).await.unwrap().unwrap();
        assert_eq!(result.metadata.title, Some("The Matrix".to_string()));
        assert_eq!(result.confidence, MatchConfidence::Id);
    }

    #[tokio::test]
    async fn no_known_id_falls_back_to_title_search_and_is_flagged_low_confidence() {
        let tmdb = MockMetadataProvider::new().with_search_results(vec![meta("Arrival")]);
        let providers = [NamedProvider::new(TMDB, &tmdb)];
        let ids = ResolveIds::new().with_title("arrival");

        let result = resolve_and_merge(&ids, MediaKind::Movie, &providers).await.unwrap().unwrap();
        assert_eq!(result.metadata.title, Some("Arrival".to_string()));
        assert_eq!(result.confidence, MatchConfidence::TitleSearch);
    }

    /// The review-finding regression test (S119b codex REQUEST_CHANGES,
    /// finding 1): known ids were supplied, but every provider failed to
    /// resolve any of them — this must NOT fall back to a title search,
    /// even though a `title` is also present on `ids` and would otherwise
    /// have plenty to search with. A wrong-confident match against the
    /// wrong title is worse than an honest "unresolved".
    #[tokio::test]
    async fn known_ids_all_failing_does_not_fall_back_to_title_search() {
        let tmdb = MockMetadataProvider::new().with_search_results(vec![meta("Some Unrelated Movie")]);
        let providers = [NamedProvider::new(TMDB, &tmdb)];
        // "603" is NOT registered in the mock's `by_id` map -> resolve_by_id
        // returns Ok(None). A title IS also present, but must be ignored.
        let ids = ResolveIds::new().with_id(TMDB, "603").with_title("The Matrix");

        let result = resolve_and_merge(&ids, MediaKind::Movie, &providers).await.unwrap();
        assert_eq!(result, None, "known-but-unresolvable ids must return None, never a title-search guess");
    }

    #[tokio::test]
    async fn absent_provider_from_the_slice_simply_does_not_contribute() {
        // Simulates `TvdbClient::from_config` returning `None` (unconfigured):
        // the caller never puts it in `providers` at all.
        let tmdb = MockMetadataProvider::new().with_id("603", meta("The Matrix"));
        let providers = [NamedProvider::new(TMDB, &tmdb)];
        let ids = ResolveIds::new().with_id(TMDB, "603").with_id(TVDB, "81");

        let result = resolve_and_merge(&ids, MediaKind::Movie, &providers).await.unwrap().unwrap();
        assert_eq!(result.metadata.title, Some("The Matrix".to_string()));
        assert!(!result.metadata.provider_ids.contains_key(TVDB));
    }

    #[test]
    fn images_default_is_unused_placeholder_guard() {
        // Guards the `ProviderImages::default()` shape hasn't silently
        // changed under us (would otherwise show up as a confusing failure
        // in the gap-fill test above rather than here).
        assert_eq!(ProviderImages::default(), ProviderImages { poster_url: None, backdrop_url: None });
    }
}
