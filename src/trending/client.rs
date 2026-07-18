//! Read-only TMDb (The Movie Database) HTTP client (MUSE-19).
//!
//! Mirrors `crate::plex::PlexClient`'s shape: a pure typed HTTP client that
//! persists nothing itself (the ingest routine in `trending::mod` owns
//! persistence), and constructs via [`TmdbClient::from_config`], which
//! returns `None` when `TMDB_API_KEY` isn't configured — trending features
//! degrade gracefully rather than blocking startup.

use std::time::Duration;

use async_trait::async_trait;
use reqwest::header::ACCEPT;
use serde::de::DeserializeOwned;

use crate::config::Config;
use crate::error::{MuseError, MuseResult};
use crate::metadata::{MediaKind as MetadataKind, MetadataProvider, ProviderImages, ProviderMetadata};

use super::models::{
    ResultsEnvelope, TmdbDetails, TmdbFindResults, TmdbTitle, WatchProvidersEnvelope,
};
pub use super::models::RegionProviders;

const DEFAULT_BASE_URL: &str = "https://api.themoviedb.org/3";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// TMDb's image CDN base — `poster_path`/`backdrop_path` come back as
/// bare relative paths (e.g. `/abc123.jpg`); this is prefixed to build a
/// usable URL. `w780` is a reasonable general-purpose size (poster and
/// backdrop alike) — not the largest TMDb offers, but far smaller than
/// `original` for a background enrichment pass that doesn't need
/// print-quality art.
const IMAGE_BASE_URL: &str = "https://image.tmdb.org/t/p/w780";

/// TMDb's `movie` vs `tv` media-type path segment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TmdbMediaType {
    Movie,
    Tv,
}

impl TmdbMediaType {
    fn as_path(&self) -> &'static str {
        match self {
            TmdbMediaType::Movie => "movie",
            TmdbMediaType::Tv => "tv",
        }
    }
}

/// TMDb's `/trending/{media_type}/{window}` day-vs-week axis.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrendingWindow {
    Day,
    Week,
}

impl TrendingWindow {
    pub fn as_str(&self) -> &'static str {
        match self {
            TrendingWindow::Day => "day",
            TrendingWindow::Week => "week",
        }
    }
}

/// A typed, read-only TMDb client.
#[derive(Debug, Clone)]
pub struct TmdbClient {
    http: reqwest::Client,
    base_url: String,
    api_key: String,
}

impl TmdbClient {
    /// Build a client against a specific TMDb-compatible base URL (e.g. the
    /// real `https://api.themoviedb.org/3`, or an httpmock server in tests)
    /// and API key.
    pub fn new(base_url: impl Into<String>, api_key: impl Into<String>) -> MuseResult<Self> {
        let http = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .map_err(MuseError::Http)?;

        Ok(Self {
            http,
            base_url: base_url.into().trim_end_matches('/').to_string(),
            api_key: api_key.into(),
        })
    }

    /// Build a client from `Config` (`TMDB_API_KEY`) against the real TMDb
    /// API. Returns `None` when unset/empty — callers (and the trending
    /// ingest routine) treat TMDb as an optional, gracefully-degrading
    /// dependency, same as `PlexClient::from_config`.
    pub fn from_config(config: &Config) -> Option<Self> {
        let api_key = config.tmdb_api_key.clone()?;

        match Self::new(DEFAULT_BASE_URL, api_key) {
            Ok(client) => Some(client),
            Err(e) => {
                tracing::warn!(error = %e, "failed to construct TMDb client; trending features will degrade");
                None
            }
        }
    }

    async fn get<T: DeserializeOwned>(&self, path: &str, query: &[(&str, &str)]) -> MuseResult<T> {
        let url = format!("{}{}", self.base_url, path);

        let mut all_query: Vec<(&str, &str)> = vec![("api_key", self.api_key.as_str())];
        all_query.extend_from_slice(query);

        let resp = self
            .http
            .get(&url)
            .header(ACCEPT, "application/json")
            .query(&all_query)
            .send()
            .await?;

        let status = resp.status();
        let bytes = resp.bytes().await?;

        if !status.is_success() {
            let body = String::from_utf8_lossy(&bytes).to_string();
            return Err(MuseError::Upstream {
                status: status.as_u16(),
                message: format!("tmdb request to {path} failed: {body}"),
            });
        }

        serde_json::from_slice::<T>(&bytes).map_err(|e| MuseError::Upstream {
            status: status.as_u16(),
            message: format!("failed to parse tmdb response from {path}: {e}"),
        })
    }

    /// `GET /trending/{media_type}/{window}` — the day-one trending source.
    pub async fn trending(
        &self,
        media_type: TmdbMediaType,
        window: TrendingWindow,
    ) -> MuseResult<Vec<TmdbTitle>> {
        let path = format!("/trending/{}/{}", media_type.as_path(), window.as_str());
        let envelope: ResultsEnvelope<TmdbTitle> = self.get(&path, &[]).await?;
        Ok(envelope.results)
    }

    /// `GET /movie|tv/popular` — region-configurable.
    pub async fn popular(
        &self,
        media_type: TmdbMediaType,
        region: Option<&str>,
    ) -> MuseResult<Vec<TmdbTitle>> {
        let path = format!("/{}/popular", media_type.as_path());
        let query: Vec<(&str, &str)> = match region {
            Some(r) => vec![("region", r)],
            None => vec![],
        };
        let envelope: ResultsEnvelope<TmdbTitle> = self.get(&path, &query).await?;
        Ok(envelope.results)
    }

    /// `GET /search/multi` — free-text title search across movies and tv in
    /// one call (TMDb's `media_type` field on each hit disambiguates).
    /// MUSE-09's resolution ladder uses this as its "beyond the library"
    /// tier: a query that neither the vector nor trigram tier could resolve
    /// against the local catalog gets one shot at a TMDb lookup so the
    /// caller can be told the honest answer ("not in your library, but here
    /// it is on TMDb") rather than a bare miss. Results are returned in
    /// TMDb's own relevance order; callers should not re-sort by
    /// popularity, which would defeat the relevance ranking for a
    /// free-text query.
    pub async fn search_multi(&self, query: &str) -> MuseResult<Vec<TmdbTitle>> {
        let envelope: ResultsEnvelope<TmdbTitle> = self.get("/search/multi", &[("query", query)]).await?;
        Ok(envelope.results)
    }

    /// `GET /movie|tv/{id}/watch/providers` — where a title streams, keyed
    /// by ISO 3166-1 region code.
    pub async fn watch_providers(
        &self,
        media_type: TmdbMediaType,
        tmdb_id: &str,
    ) -> MuseResult<std::collections::HashMap<String, RegionProviders>> {
        let path = format!("/{}/{}/watch/providers", media_type.as_path(), tmdb_id);
        let envelope: WatchProvidersEnvelope = self.get(&path, &[]).await?;
        Ok(envelope.results)
    }

    /// `GET /movie|tv/{id}?append_to_response=external_ids` — the full
    /// record `MetadataProvider::resolve_by_id` needs (MUSEL-A2): overview,
    /// genres, poster/backdrop, rating, and (via `external_ids`) the
    /// imdb id bridge for `provider_ids`. A 404 surfaces as
    /// `MuseError::Upstream { status: 404, .. }`, which
    /// [`MetadataProvider::resolve_by_id`]'s impl below maps to `Ok(None)`
    /// — a title simply absent from TMDb, not an error.
    async fn get_details(&self, media_type: TmdbMediaType, id: &str) -> MuseResult<TmdbDetails> {
        let path = format!("/{}/{}", media_type.as_path(), id);
        self.get(&path, &[("append_to_response", "external_ids")]).await
    }

    /// `GET /find/{imdb_id}?external_source=imdb_id` — the IMDb-id bridge.
    /// Returns thin hits (same shape as `search_multi`'s), split by media
    /// type; [`Self::resolve_by_imdb_id`] takes the first hit for the
    /// requested `media_type` and re-resolves it via [`Self::get_details`]
    /// for the full record.
    async fn find_by_imdb_id(&self, imdb_id: &str) -> MuseResult<TmdbFindResults> {
        let path = format!("/find/{imdb_id}");
        self.get(&path, &[("external_source", "imdb_id")]).await
    }

    /// Bridges an IMDb id to a full TMDb record: `/find` to get TMDb's own
    /// numeric id, then `/{media_type}/{id}` for the full details. Two
    /// round trips, but only taken when the caller has no native `tmdb_id`
    /// for this title (see `metadata::resolve::ResolveIds::id_for`).
    /// `Ok(None)` for a well-formed no-match at either step — never an
    /// error just because IMDb's id doesn't bridge to a TMDb entry.
    async fn resolve_by_imdb_id(
        &self,
        media_type: TmdbMediaType,
        imdb_id: &str,
    ) -> MuseResult<Option<ProviderMetadata>> {
        let found = self.find_by_imdb_id(imdb_id).await?;
        let candidate = match media_type {
            TmdbMediaType::Movie => found.movie_results.into_iter().next(),
            TmdbMediaType::Tv => found.tv_results.into_iter().next(),
        };
        let Some(candidate) = candidate else {
            return Ok(None);
        };

        let tmdb_id = candidate.id.to_string();
        match self.get_details(media_type, &tmdb_id).await {
            Ok(details) => Ok(Some(details_to_provider_metadata(details, &tmdb_id))),
            Err(MuseError::Upstream { status: 404, .. }) => Ok(None),
            Err(e) => Err(e),
        }
    }
}

/// Maps the crate-wide, provider-agnostic [`MetadataKind`] to TMDb's own
/// `movie`/`tv` path split.
fn to_tmdb_media_type(kind: MetadataKind) -> TmdbMediaType {
    match kind {
        MetadataKind::Movie => TmdbMediaType::Movie,
        MetadataKind::Series => TmdbMediaType::Tv,
    }
}

/// Normalizes a [`TmdbDetails`] response into the crate-wide
/// [`ProviderMetadata`] shape. `tmdb_id` is threaded through explicitly
/// (rather than trusting `details.id`) so the id recorded in
/// `provider_ids["tmdb"]` is always exactly the id the caller asked for,
/// even if a future TMDb response shape ever omits/renames `id`.
fn details_to_provider_metadata(details: TmdbDetails, tmdb_id: &str) -> ProviderMetadata {
    let mut provider_ids = std::collections::HashMap::new();
    provider_ids.insert("tmdb".to_string(), tmdb_id.to_string());
    if let Some(imdb_id) = details
        .external_ids
        .and_then(|ext| ext.imdb_id)
        .filter(|id| !id.is_empty())
    {
        provider_ids.insert("imdb".to_string(), imdb_id);
    }

    let first_aired = details.release_date.clone().or_else(|| details.first_air_date.clone());
    let year = first_aired
        .as_deref()
        .filter(|d| d.len() >= 4)
        .and_then(|d| d[0..4].parse::<i32>().ok());

    ProviderMetadata {
        provider_ids,
        title: details.title.or(details.name),
        overview: details.overview.filter(|s| !s.is_empty()),
        genres: details.genres.into_iter().map(|g| g.name).filter(|n| !n.is_empty()).collect(),
        images: ProviderImages {
            poster_url: details.poster_path.map(|p| format!("{IMAGE_BASE_URL}{p}")),
            backdrop_url: details.backdrop_path.map(|p| format!("{IMAGE_BASE_URL}{p}")),
        },
        rating: details.vote_average,
        first_aired,
        year,
        // TMDb's tv `networks` field exists but isn't fetched here — out
        // of scope for the v1 mapping (TVDB's `originalNetwork` already
        // covers the TV-primary case per the blueprint's precedence, and
        // movies have no network at all).
        network: None,
        keywords: Vec::new(),
        // MUSEL-C2 field: TMDb runtime isn't fetched in this v1 mapping.
        runtime_minutes: None,
    }
}

/// [`MetadataProvider`] adapter for the existing TMDb client (MUSEL-A2) —
/// lets `metadata::resolve::resolve_and_merge` fan out to TMDb the same
/// way it does to `TvdbClient`. An id starting with `tt` (IMDb's own
/// prefix) is bridged via [`TmdbClient::resolve_by_imdb_id`] rather than
/// treated as TMDb's own (numeric) id — see
/// `metadata::resolve::ResolveIds::id_for`'s doc for when that happens.
#[async_trait]
impl MetadataProvider for TmdbClient {
    async fn resolve_by_id(
        &self,
        kind: MetadataKind,
        provider_id: &str,
    ) -> MuseResult<Option<ProviderMetadata>> {
        let media_type = to_tmdb_media_type(kind);

        if provider_id.starts_with("tt") {
            return self.resolve_by_imdb_id(media_type, provider_id).await;
        }

        match self.get_details(media_type, provider_id).await {
            Ok(details) => Ok(Some(details_to_provider_metadata(details, provider_id))),
            Err(MuseError::Upstream { status: 404, .. }) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// `search_multi` already covers both media types in one call; results
    /// are filtered down to the requested `kind` via TMDb's own
    /// `media_type` field on each hit. Thin results (no overview/genres/
    /// images) — matches [`MetadataProvider::search`]'s "discovery only"
    /// contract (see `TvdbClient::search`'s identical posture); a caller
    /// that lands on one of these via `resolve_and_merge`'s fallback path
    /// gets a lowest-confidence title/year/rating match, not a full record.
    async fn search(&self, query: &str, kind: MetadataKind) -> MuseResult<Vec<ProviderMetadata>> {
        let want = match kind {
            MetadataKind::Movie => "movie",
            MetadataKind::Series => "tv",
        };

        let titles = self.search_multi(query).await?;
        Ok(titles
            .into_iter()
            .filter(|t| t.media_type.as_deref() == Some(want))
            .map(|t| {
                let mut provider_ids = std::collections::HashMap::new();
                provider_ids.insert("tmdb".to_string(), t.id.to_string());
                ProviderMetadata {
                    provider_ids,
                    title: t.display_title().map(str::to_string),
                    year: t.year(),
                    rating: t.vote_average,
                    ..Default::default()
                }
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use httpmock::prelude::*;

    fn client_for(server: &MockServer) -> TmdbClient {
        TmdbClient::new(server.base_url(), "test-key").expect("client should construct")
    }

    #[tokio::test]
    async fn trending_parses_results_and_sends_api_key() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(GET)
                .path("/trending/movie/day")
                .query_param("api_key", "test-key");
            then.status(200)
                .header("content-type", "application/json")
                .body(
                    r#"{
                        "page": 1,
                        "results": [
                            {"id": 27205, "title": "Inception", "release_date": "2010-07-15", "popularity": 80.5},
                            {"id": 603, "title": "The Matrix", "release_date": "1999-03-30", "popularity": 60.1}
                        ],
                        "total_pages": 1,
                        "total_results": 2
                    }"#,
                );
        });

        let client = client_for(&server);
        let titles = client
            .trending(TmdbMediaType::Movie, TrendingWindow::Day)
            .await
            .expect("trending should parse");

        mock.assert();
        assert_eq!(titles.len(), 2);
        assert_eq!(titles[0].display_title(), Some("Inception"));
        assert_eq!(titles[0].year(), Some(2010));
    }

    #[tokio::test]
    async fn popular_sends_region_query_param() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(GET)
                .path("/tv/popular")
                .query_param("region", "US");
            then.status(200)
                .header("content-type", "application/json")
                .body(r#"{"results": [{"id": 1399, "name": "Game of Thrones", "first_air_date": "2011-04-17"}]}"#);
        });

        let client = client_for(&server);
        let titles = client
            .popular(TmdbMediaType::Tv, Some("US"))
            .await
            .expect("popular should parse");

        mock.assert();
        assert_eq!(titles.len(), 1);
        assert_eq!(titles[0].display_title(), Some("Game of Thrones"));
    }

    #[tokio::test]
    async fn search_multi_parses_mixed_movie_and_tv_results() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(GET)
                .path("/search/multi")
                .query_param("query", "arrival");
            then.status(200)
                .header("content-type", "application/json")
                .body(
                    r#"{
                        "results": [
                            {"id": 329865, "title": "Arrival", "release_date": "2016-11-10", "media_type": "movie"},
                            {"id": 999, "name": "Arrival (TV series)", "first_air_date": "2020-01-01", "media_type": "tv"}
                        ]
                    }"#,
                );
        });

        let client = client_for(&server);
        let hits = client
            .search_multi("arrival")
            .await
            .expect("search_multi should parse");

        mock.assert();
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].display_title(), Some("Arrival"));
        assert_eq!(hits[0].media_type.as_deref(), Some("movie"));
        assert_eq!(hits[1].media_type.as_deref(), Some("tv"));
    }

    #[tokio::test]
    async fn watch_providers_parses_region_map() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/movie/603/watch/providers");
            then.status(200)
                .header("content-type", "application/json")
                .body(
                    r#"{
                        "id": 603,
                        "results": {
                            "US": {
                                "link": "https://www.themoviedb.org/movie/603/watch",
                                "flatrate": [{"provider_id": 8, "provider_name": "Netflix"}]
                            }
                        }
                    }"#,
                );
        });

        let client = client_for(&server);
        let regions = client
            .watch_providers(TmdbMediaType::Movie, "603")
            .await
            .expect("watch_providers should parse");

        let us = regions.get("US").expect("US region present");
        assert_eq!(us.flatrate[0].provider_name, "Netflix");
    }

    #[tokio::test]
    async fn upstream_error_status_is_surfaced_not_panicked() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/trending/movie/day");
            then.status(401).body("invalid api key");
        });

        let client = client_for(&server);
        let result = client.trending(TmdbMediaType::Movie, TrendingWindow::Day).await;

        assert!(result.is_err());
        match result.unwrap_err() {
            MuseError::Upstream { status, .. } => assert_eq!(status, 401),
            other => panic!("expected Upstream error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn malformed_json_does_not_panic() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/trending/movie/day");
            then.status(200)
                .header("content-type", "application/json")
                .body("{not valid json");
        });

        let client = client_for(&server);
        let result = client.trending(TmdbMediaType::Movie, TrendingWindow::Day).await;

        assert!(result.is_err());
    }

    #[test]
    fn from_config_returns_none_when_unconfigured() {
        let config = Config {
            ..Default::default()
        };
        assert!(TmdbClient::from_config(&config).is_none());
    }

    #[test]
    fn from_config_builds_client_when_configured() {
        let config = Config {
            tmdb_api_key: Some("abc123".to_string()),
            ..Default::default()
        };
        assert!(TmdbClient::from_config(&config).is_some());
    }

    // --- MUSEL-A2: MetadataProvider adapter ---------------------------

    #[tokio::test]
    async fn resolve_by_id_parses_movie_details_with_imdb_bridge() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(GET)
                .path("/movie/603")
                .query_param("append_to_response", "external_ids");
            then.status(200)
                .header("content-type", "application/json")
                .body(
                    r#"{
                        "id": 603,
                        "title": "The Matrix",
                        "overview": "A hacker discovers reality is a simulation.",
                        "release_date": "1999-03-30",
                        "vote_average": 8.2,
                        "poster_path": "/poster.jpg",
                        "backdrop_path": "/backdrop.jpg",
                        "genres": [{"id": 28, "name": "Action"}, {"id": 878, "name": "Science Fiction"}],
                        "external_ids": {"imdb_id": "tt0133093"}
                    }"#,
                );
        });

        let client = client_for(&server);
        let result = MetadataProvider::resolve_by_id(&client, MetadataKind::Movie, "603")
            .await
            .expect("resolve_by_id should not error")
            .expect("603 should resolve");

        mock.assert();
        assert_eq!(result.title, Some("The Matrix".to_string()));
        assert_eq!(result.year, Some(1999));
        assert_eq!(result.genres, vec!["Action".to_string(), "Science Fiction".to_string()]);
        assert_eq!(result.provider_ids.get("tmdb"), Some(&"603".to_string()));
        assert_eq!(result.provider_ids.get("imdb"), Some(&"tt0133093".to_string()));
        assert_eq!(result.images.poster_url, Some(format!("{IMAGE_BASE_URL}/poster.jpg")));
    }

    #[tokio::test]
    async fn resolve_by_id_returns_none_for_404() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(GET).path("/movie/999999");
            then.status(404).body(r#"{"status_message": "not found"}"#);
        });

        let client = client_for(&server);
        let result = MetadataProvider::resolve_by_id(&client, MetadataKind::Movie, "999999")
            .await
            .expect("a 404 should not be an error");

        mock.assert();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn resolve_by_id_bridges_imdb_id_via_find_then_details() {
        let server = MockServer::start();
        let find_mock = server.mock(|when, then| {
            when.method(GET)
                .path("/find/tt0133093")
                .query_param("external_source", "imdb_id");
            then.status(200)
                .header("content-type", "application/json")
                .body(r#"{"movie_results": [{"id": 603, "title": "The Matrix"}], "tv_results": []}"#);
        });
        let details_mock = server.mock(|when, then| {
            when.method(GET).path("/movie/603");
            then.status(200)
                .header("content-type", "application/json")
                .body(r#"{"id": 603, "title": "The Matrix", "release_date": "1999-03-30"}"#);
        });

        let client = client_for(&server);
        let result = MetadataProvider::resolve_by_id(&client, MetadataKind::Movie, "tt0133093")
            .await
            .expect("bridge should not error")
            .expect("bridge should resolve");

        find_mock.assert();
        details_mock.assert();
        assert_eq!(result.title, Some("The Matrix".to_string()));
        assert_eq!(result.provider_ids.get("tmdb"), Some(&"603".to_string()));
    }

    #[tokio::test]
    async fn resolve_by_id_imdb_bridge_returns_none_when_find_has_no_hit() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(GET).path("/find/tt9999999");
            then.status(200)
                .header("content-type", "application/json")
                .body(r#"{"movie_results": [], "tv_results": []}"#);
        });

        let client = client_for(&server);
        let result = MetadataProvider::resolve_by_id(&client, MetadataKind::Movie, "tt9999999")
            .await
            .expect("no bridge hit should not be an error");

        mock.assert();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn search_filters_to_requested_media_kind() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(GET).path("/search/multi").query_param("query", "arrival");
            then.status(200)
                .header("content-type", "application/json")
                .body(
                    r#"{
                        "results": [
                            {"id": 329865, "title": "Arrival", "release_date": "2016-11-10", "media_type": "movie"},
                            {"id": 999, "name": "Arrival (TV series)", "first_air_date": "2020-01-01", "media_type": "tv"}
                        ]
                    }"#,
                );
        });

        let client = client_for(&server);
        let results = MetadataProvider::search(&client, "arrival", MetadataKind::Movie)
            .await
            .expect("search should not error");

        mock.assert();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].title, Some("Arrival".to_string()));
        assert_eq!(results[0].provider_ids.get("tmdb"), Some(&"329865".to_string()));
    }
}
