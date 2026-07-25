//! Read-only TMDb (The Movie Database) HTTP client (MUSE-19).
//!
//! Mirrors `crate::plex::PlexClient`'s shape: a pure typed HTTP client that
//! persists nothing itself (the ingest routine in `trending::mod` owns
//! persistence), and constructs via [`TmdbClient::from_config`]. When
//! `TMDB_API_KEY` is configured it talks to the real TMDb API; when it isn't
//! (and `metadata_keyless` is on, the default) it falls back to the key-less
//! Radarr public metadata proxy ([`TmdbMode::RadarrProxy`], AMETA-1) so
//! metadata enrichment works with zero operator key setup — trending/
//! watch-providers degrade gracefully in that mode rather than blocking.

use std::time::Duration;

use async_trait::async_trait;
use reqwest::header::ACCEPT;
use serde::de::DeserializeOwned;

use crate::config::Config;
use crate::error::{MuseError, MuseResult};
use crate::metadata::{
    MediaKind as MetadataKind, MetadataProvider, ProviderImages, ProviderMetadata,
};

pub use super::models::RegionProviders;
use super::models::{
    ResultsEnvelope, TmdbDetails, TmdbFindResults, TmdbTitle, WatchProvidersEnvelope,
};

const DEFAULT_BASE_URL: &str = "https://api.themoviedb.org/3";
/// AMETA-1: Radarr's public TMDb metadata proxy — the key-less default the
/// client points at when no `TMDB_API_KEY` is configured. This is exactly the
/// proxy Radarr itself uses so a user never registers a TMDb API key; no auth,
/// no `api_key` query param. Overridable via `MUSE_TMDB_METADATA_URL`.
pub const DEFAULT_RADARR_PROXY_URL: &str = "https://api.radarr.video/v1";
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

/// How a [`TmdbClient`] talks to its backend (AMETA-1).
///
/// - [`TmdbMode::Api`] — the real `api.themoviedb.org` v3 API: every GET
///   carries an `api_key` query param, `resolve_by_id`/`search` parse TMDb's
///   own JSON, and trending/popular/watch-providers all work.
/// - [`TmdbMode::RadarrProxy`] — the key-less public proxy at
///   `api.radarr.video`: no `api_key`, movie records only, and a different
///   response shape (genres as string arrays, `images[]` with `coverType`
///   instead of `poster_path`). The proxy has **no** `/trending` or
///   `/watch/providers`, so those degrade to empty in this mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TmdbMode {
    Api,
    RadarrProxy,
}

/// A typed, read-only TMDb client.
#[derive(Debug, Clone)]
pub struct TmdbClient {
    http: reqwest::Client,
    base_url: String,
    api_key: String,
    mode: TmdbMode,
}

impl TmdbClient {
    /// Build a client against a specific TMDb-compatible base URL (e.g. the
    /// real `https://api.themoviedb.org/3`, or an httpmock server in tests)
    /// and API key.
    pub fn new(base_url: impl Into<String>, api_key: impl Into<String>) -> MuseResult<Self> {
        Self::with_mode(base_url, api_key, TmdbMode::Api)
    }

    /// Build a key-less proxy-mode client against a Radarr-metadata-proxy-
    /// compatible base URL (the real `https://api.radarr.video/v1`, or an
    /// httpmock server in tests). No API key is carried; requests parse the
    /// Radarr proxy JSON shape rather than TMDb's own.
    pub fn new_proxy(base_url: impl Into<String>) -> MuseResult<Self> {
        Self::with_mode(base_url, String::new(), TmdbMode::RadarrProxy)
    }

    fn with_mode(
        base_url: impl Into<String>,
        api_key: impl Into<String>,
        mode: TmdbMode,
    ) -> MuseResult<Self> {
        let http = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .map_err(MuseError::Http)?;

        Ok(Self {
            http,
            base_url: base_url.into().trim_end_matches('/').to_string(),
            api_key: api_key.into(),
            mode,
        })
    }

    /// Build a client from `Config`. Precedence (AMETA-1/2):
    /// 1. `TMDB_API_KEY` set → real TMDb API (today's behavior, full
    ///    trending + metadata).
    /// 2. else `metadata_keyless` (default true) → **key-less proxy mode**
    ///    against `MUSE_TMDB_METADATA_URL` (default `api.radarr.video`), so a
    ///    fresh deploy gets poster/genre/overview enrichment with zero
    ///    operator key setup. Trending/watch-providers degrade to empty in
    ///    this mode (the proxy has no such endpoints).
    /// 3. else (`metadata_keyless=false`, no key) → `None`, the old
    ///    graceful-degrade posture.
    pub fn from_config(config: &Config) -> Option<Self> {
        if let Some(api_key) = config.tmdb_api_key.clone() {
            return match Self::new(DEFAULT_BASE_URL, api_key) {
                Ok(client) => Some(client),
                Err(e) => {
                    tracing::warn!(error = %e, "failed to construct TMDb client; trending features will degrade");
                    None
                }
            };
        }

        if !config.metadata_keyless {
            return None;
        }

        let base_url = config
            .tmdb_metadata_base_url
            .clone()
            .unwrap_or_else(|| DEFAULT_RADARR_PROXY_URL.to_string());
        match Self::new_proxy(base_url) {
            Ok(client) => {
                tracing::info!(
                    base_url = %client.base_url,
                    "AMETA-1: TMDB_API_KEY unset — using key-less Radarr metadata proxy (movie enrichment only; trending/watch-providers disabled)"
                );
                Some(client)
            }
            Err(e) => {
                tracing::warn!(error = %e, "failed to construct key-less TMDb proxy client; metadata enrichment will degrade");
                None
            }
        }
    }

    async fn get<T: DeserializeOwned>(&self, path: &str, query: &[(&str, &str)]) -> MuseResult<T> {
        let url = format!("{}{}", self.base_url, path);

        // Proxy mode is key-less — never append an `api_key` param.
        let mut all_query: Vec<(&str, &str)> = match self.mode {
            TmdbMode::Api => vec![("api_key", self.api_key.as_str())],
            TmdbMode::RadarrProxy => Vec::new(),
        };
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

    /// Whether this client is in key-less Radarr-proxy mode (AMETA-1). The
    /// proxy serves only movie lookup/search — trending, popular and
    /// watch-providers have no proxy endpoint and degrade to empty.
    pub fn is_proxy_mode(&self) -> bool {
        self.mode == TmdbMode::RadarrProxy
    }

    /// `GET /trending/{media_type}/{window}` — the day-one trending source.
    ///
    /// **Proxy-mode caveat (AMETA-2):** `api.radarr.video` has no `/trending`
    /// endpoint, so in [`TmdbMode::RadarrProxy`] this degrades to an empty
    /// list (logged once) rather than erroring — trending remains a
    /// real-key-only feature; only metadata enrichment works key-less.
    pub async fn trending(
        &self,
        media_type: TmdbMediaType,
        window: TrendingWindow,
    ) -> MuseResult<Vec<TmdbTitle>> {
        if self.mode == TmdbMode::RadarrProxy {
            tracing::debug!(
                "AMETA-2: trending unavailable in key-less proxy mode; returning empty"
            );
            return Ok(Vec::new());
        }
        let path = format!("/trending/{}/{}", media_type.as_path(), window.as_str());
        let envelope: ResultsEnvelope<TmdbTitle> = self.get(&path, &[]).await?;
        Ok(envelope.results)
    }

    /// `GET /movie|tv/popular` — region-configurable. Degrades to empty in
    /// key-less proxy mode (no proxy endpoint), same as [`Self::trending`].
    pub async fn popular(
        &self,
        media_type: TmdbMediaType,
        region: Option<&str>,
    ) -> MuseResult<Vec<TmdbTitle>> {
        if self.mode == TmdbMode::RadarrProxy {
            tracing::debug!("AMETA-2: popular unavailable in key-less proxy mode; returning empty");
            return Ok(Vec::new());
        }
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
        let envelope: ResultsEnvelope<TmdbTitle> =
            self.get("/search/multi", &[("query", query)]).await?;
        Ok(envelope.results)
    }

    /// `GET /movie|tv/{id}/watch/providers` — where a title streams, keyed
    /// by ISO 3166-1 region code.
    pub async fn watch_providers(
        &self,
        media_type: TmdbMediaType,
        tmdb_id: &str,
    ) -> MuseResult<std::collections::HashMap<String, RegionProviders>> {
        if self.mode == TmdbMode::RadarrProxy {
            tracing::debug!(
                "AMETA-2: watch_providers unavailable in key-less proxy mode; returning empty"
            );
            return Ok(std::collections::HashMap::new());
        }
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
        self.get(&path, &[("append_to_response", "external_ids")])
            .await
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

    // --- AMETA-2: key-less Radarr-proxy (api.radarr.video) resolve/search ---

    /// Resolve a single movie via the Radarr metadata proxy:
    /// `GET /movie/{tmdbId}`, or `GET /movie/imdb/{imdbId}` when `provider_id`
    /// is an IMDb id (the `tt`-prefixed bridge, replacing TMDb's `/find`).
    /// The proxy is movie-only, so a `Series` kind resolves to `Ok(None)`
    /// (series go through the Skyhook path in `TvdbClient`). A 404 is a
    /// well-formed "not on the proxy", mapped to `Ok(None)`.
    async fn radarr_resolve(
        &self,
        media_type: TmdbMediaType,
        provider_id: &str,
    ) -> MuseResult<Option<ProviderMetadata>> {
        if media_type == TmdbMediaType::Tv {
            // api.radarr.video is a TMDb-*movie* proxy; series are Skyhook's job.
            return Ok(None);
        }

        let path = if provider_id.starts_with("tt") {
            format!("/movie/imdb/{provider_id}")
        } else {
            format!("/movie/{provider_id}")
        };

        match self.get::<RadarrMovie>(&path, &[]).await {
            Ok(movie) => Ok(Some(radarr_proxy_to_provider_metadata(movie))),
            Err(MuseError::Upstream { status: 404, .. }) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Free-text movie search via the Radarr proxy: `GET /search?q={term}`.
    /// Returns full records (the proxy embeds the same shape as
    /// `/movie/{id}`), mapped to `ProviderMetadata`. Movie-only — a `Series`
    /// kind yields an empty list.
    async fn radarr_search(
        &self,
        query: &str,
        media_type: TmdbMediaType,
    ) -> MuseResult<Vec<ProviderMetadata>> {
        if media_type == TmdbMediaType::Tv {
            return Ok(Vec::new());
        }
        let movies: Vec<RadarrMovie> = self.get("/search", &[("q", query)]).await?;
        Ok(movies
            .into_iter()
            .map(radarr_proxy_to_provider_metadata)
            .collect())
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

    let first_aired = details
        .release_date
        .clone()
        .or_else(|| details.first_air_date.clone());
    let year = first_aired
        .as_deref()
        .filter(|d| d.len() >= 4)
        .and_then(|d| d[0..4].parse::<i32>().ok());

    ProviderMetadata {
        provider_ids,
        title: details.title.or(details.name),
        overview: details.overview.filter(|s| !s.is_empty()),
        genres: details
            .genres
            .into_iter()
            .map(|g| g.name)
            .filter(|n| !n.is_empty())
            .collect(),
        images: ProviderImages {
            poster_url: details.poster_path.map(|p| format!("{IMAGE_BASE_URL}{p}")),
            backdrop_url: details
                .backdrop_path
                .map(|p| format!("{IMAGE_BASE_URL}{p}")),
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

// --- AMETA-2: Radarr public-proxy (api.radarr.video) response shapes -------
//
// The proxy JSON differs from TMDb's own: `genres` is a plain string array
// (not `[{id,name}]`), art comes back as an `images[]` array tagged by
// `coverType` (not `poster_path`/`backdrop_path` relative paths, and already
// as absolute `remoteUrl`s), and `runtime`/`year` are inline. Permissive
// (`Option`/`#[serde(default)]`) like `TmdbTitle`, since the proxy omits
// fields per title.

/// One entry of a Radarr-proxy `images[]` array, e.g.
/// `{"coverType":"poster","url":"/...","remoteUrl":"https://image.tmdb.org/..."}`.
#[derive(Debug, Clone, Deserialize)]
struct RadarrImage {
    #[serde(rename = "coverType", default)]
    cover_type: Option<String>,
    #[serde(default)]
    url: Option<String>,
    #[serde(rename = "remoteUrl", default)]
    remote_url: Option<String>,
}

impl RadarrImage {
    /// Prefer the absolute `remoteUrl` (the TMDb CDN URL); fall back to the
    /// proxy-relative `url` if that's all the record carries.
    fn best_url(&self) -> Option<String> {
        self.remote_url.clone().or_else(|| self.url.clone())
    }
}

/// Tolerant Radarr `ratings` shape. Newer proxies nest per-source
/// (`{"tmdb":{"value":8.2},"imdb":{"value":8.7}}`); older/flat responses may
/// send `{"value":8.2}` directly. Either yields a single best-effort rating.
#[derive(Debug, Clone, Default, Deserialize)]
struct RadarrRatings {
    #[serde(default)]
    tmdb: Option<RadarrRatingValue>,
    #[serde(default)]
    imdb: Option<RadarrRatingValue>,
    #[serde(default)]
    value: Option<f64>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct RadarrRatingValue {
    #[serde(default)]
    value: Option<f64>,
}

impl RadarrRatings {
    fn best(&self) -> Option<f64> {
        self.tmdb
            .as_ref()
            .and_then(|r| r.value)
            .or_else(|| self.imdb.as_ref().and_then(|r| r.value))
            .or(self.value)
    }
}

/// A Radarr-proxy movie record (`GET /movie/{tmdbId}` /
/// `GET /movie/imdb/{imdbId}` / `GET /search` element).
#[derive(Debug, Clone, Deserialize)]
struct RadarrMovie {
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    overview: Option<String>,
    #[serde(default)]
    year: Option<i32>,
    /// Runtime in minutes — fills `ProviderMetadata::runtime_minutes`
    /// (MUSEL-C2), which the real-API v1 mapping leaves `None`.
    #[serde(default)]
    runtime: Option<i32>,
    #[serde(default)]
    genres: Vec<String>,
    #[serde(default)]
    images: Vec<RadarrImage>,
    #[serde(default)]
    ratings: Option<RadarrRatings>,
    #[serde(rename = "imdbId", default)]
    imdb_id: Option<String>,
    /// TMDb id as a JSON number on the proxy — captured as a string for
    /// `provider_ids` parity with the rest of the crate.
    #[serde(rename = "tmdbId", default)]
    tmdb_id: Option<i64>,
    /// The proxy also echoes `inCinemas`/`physicalRelease`; `inCinemas` is
    /// the closest analogue to TMDb's `release_date` for `first_aired`.
    #[serde(rename = "inCinemas", default)]
    in_cinemas: Option<String>,
}

/// Maps a Radarr-proxy [`RadarrMovie`] into the crate-wide
/// [`ProviderMetadata`] (AMETA-2). Poster ← `images[coverType=="poster"]`,
/// backdrop ← `images[coverType=="fanart"]`, genres pass through as-is
/// (already strings), and `runtime` fills `runtime_minutes`.
fn radarr_proxy_to_provider_metadata(movie: RadarrMovie) -> ProviderMetadata {
    let mut provider_ids = std::collections::HashMap::new();
    if let Some(tmdb_id) = movie.tmdb_id {
        provider_ids.insert("tmdb".to_string(), tmdb_id.to_string());
    }
    if let Some(imdb_id) = movie.imdb_id.filter(|s| !s.is_empty()) {
        provider_ids.insert("imdb".to_string(), imdb_id);
    }

    let find_image = |want: &str| -> Option<String> {
        movie
            .images
            .iter()
            .find(|img| img.cover_type.as_deref() == Some(want))
            .and_then(RadarrImage::best_url)
    };

    let first_aired = movie.in_cinemas.clone().filter(|s| !s.is_empty());
    let year = movie.year.or_else(|| {
        first_aired
            .as_deref()
            .filter(|d| d.len() >= 4)
            .and_then(|d| d[0..4].parse::<i32>().ok())
    });

    ProviderMetadata {
        provider_ids,
        title: movie.title.filter(|s| !s.is_empty()),
        overview: movie.overview.filter(|s| !s.is_empty()),
        genres: movie.genres.into_iter().filter(|g| !g.is_empty()).collect(),
        images: ProviderImages {
            poster_url: find_image("poster"),
            backdrop_url: find_image("fanart"),
        },
        rating: movie.ratings.and_then(|r| r.best()),
        first_aired,
        year,
        network: None,
        keywords: Vec::new(),
        // MUSEL-C2: the proxy carries runtime — populate it (real-API v1
        // mapping still leaves this None).
        runtime_minutes: movie.runtime.filter(|m| *m > 0),
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

        // AMETA-2: key-less proxy mode uses the Radarr-proxy shapes/endpoints.
        if self.mode == TmdbMode::RadarrProxy {
            return self.radarr_resolve(media_type, provider_id).await;
        }

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
        // AMETA-2: proxy mode searches api.radarr.video (movie-only).
        if self.mode == TmdbMode::RadarrProxy {
            return self.radarr_search(query, to_tmdb_media_type(kind)).await;
        }

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
        let result = client
            .trending(TmdbMediaType::Movie, TrendingWindow::Day)
            .await;

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
        let result = client
            .trending(TmdbMediaType::Movie, TrendingWindow::Day)
            .await;

        assert!(result.is_err());
    }

    #[test]
    fn from_config_returns_none_when_keyless_disabled_and_no_key() {
        // AMETA-1: with the key-less switch explicitly off and no raw key,
        // the client is unavailable — the old graceful-degrade posture.
        let config = Config {
            metadata_keyless: false,
            ..Default::default()
        };
        assert!(TmdbClient::from_config(&config).is_none());
    }

    #[test]
    fn from_config_builds_real_api_client_when_key_set() {
        // Raw TMDB_API_KEY always wins → real-API mode, even with keyless on.
        let config = Config {
            tmdb_api_key: Some("abc123".to_string()),
            metadata_keyless: true,
            ..Default::default()
        };
        let client = TmdbClient::from_config(&config).expect("key ⇒ Some");
        assert!(
            !client.is_proxy_mode(),
            "a raw key must select real-API mode"
        );
    }

    #[test]
    fn from_config_builds_keyless_proxy_when_no_key() {
        // AMETA-1: the headline behavior — no key + keyless default true ⇒
        // Some(proxy-mode) instead of None, pointed at the Radarr proxy.
        let config = Config {
            metadata_keyless: true,
            ..Default::default()
        };
        let client = TmdbClient::from_config(&config).expect("keyless ⇒ Some");
        assert!(
            client.is_proxy_mode(),
            "no key must select Radarr-proxy mode"
        );
        assert_eq!(client.base_url, DEFAULT_RADARR_PROXY_URL);
    }

    #[test]
    fn from_config_honors_proxy_base_url_override() {
        let config = Config {
            metadata_keyless: true,
            tmdb_metadata_base_url: Some("http://radarr-proxy.test.invalid/v1".to_string()),
            ..Default::default()
        };
        let client = TmdbClient::from_config(&config).expect("keyless ⇒ Some");
        assert!(client.is_proxy_mode());
        assert_eq!(client.base_url, "http://radarr-proxy.test.invalid/v1");
    }

    // --- AMETA-2: Radarr key-less proxy mode --------------------------------

    fn proxy_client_for(server: &MockServer) -> TmdbClient {
        TmdbClient::new_proxy(server.base_url()).expect("proxy client should construct")
    }

    #[tokio::test]
    async fn proxy_resolve_by_id_parses_radarr_movie_shape() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(GET).path("/movie/603");
            // NOTE: no api_key query param asserted — proxy mode is key-less.
            then.status(200)
                .header("content-type", "application/json")
                .body(
                    r#"{
                        "title": "The Matrix",
                        "overview": "A hacker discovers reality is a simulation.",
                        "year": 1999,
                        "runtime": 136,
                        "genres": ["Action", "Science Fiction"],
                        "images": [
                            {"coverType": "poster", "url": "/p.jpg", "remoteUrl": "https://image.tmdb.org/t/p/original/p.jpg"},
                            {"coverType": "fanart", "remoteUrl": "https://image.tmdb.org/t/p/original/b.jpg"}
                        ],
                        "ratings": {"tmdb": {"value": 8.2}},
                        "imdbId": "tt0133093",
                        "tmdbId": 603,
                        "inCinemas": "1999-03-30"
                    }"#,
                );
        });

        let client = proxy_client_for(&server);
        let result = MetadataProvider::resolve_by_id(&client, MetadataKind::Movie, "603")
            .await
            .expect("resolve should not error")
            .expect("603 should resolve");

        mock.assert();
        assert_eq!(result.title.as_deref(), Some("The Matrix"));
        assert_eq!(result.year, Some(1999));
        assert_eq!(result.runtime_minutes, Some(136));
        assert_eq!(
            result.genres,
            vec!["Action".to_string(), "Science Fiction".to_string()]
        );
        assert_eq!(result.provider_ids.get("tmdb"), Some(&"603".to_string()));
        assert_eq!(
            result.provider_ids.get("imdb"),
            Some(&"tt0133093".to_string())
        );
        assert_eq!(
            result.images.poster_url.as_deref(),
            Some("https://image.tmdb.org/t/p/original/p.jpg")
        );
        assert_eq!(
            result.images.backdrop_url.as_deref(),
            Some("https://image.tmdb.org/t/p/original/b.jpg")
        );
        assert_eq!(result.rating, Some(8.2));
    }

    #[tokio::test]
    async fn proxy_resolve_bridges_imdb_id_via_movie_imdb_path() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(GET).path("/movie/imdb/tt0133093");
            then.status(200)
                .header("content-type", "application/json")
                .body(r#"{"title": "The Matrix", "tmdbId": 603, "imdbId": "tt0133093"}"#);
        });

        let client = proxy_client_for(&server);
        let result = MetadataProvider::resolve_by_id(&client, MetadataKind::Movie, "tt0133093")
            .await
            .expect("resolve should not error")
            .expect("imdb bridge should resolve");

        mock.assert();
        assert_eq!(result.title.as_deref(), Some("The Matrix"));
        assert_eq!(result.provider_ids.get("tmdb"), Some(&"603".to_string()));
    }

    #[tokio::test]
    async fn proxy_resolve_series_kind_is_none_movie_only_proxy() {
        // api.radarr.video is movie-only; a Series lookup no-ops to None
        // (series go through the Skyhook path) — and makes no HTTP call.
        let server = MockServer::start();
        let client = proxy_client_for(&server);
        let result = MetadataProvider::resolve_by_id(&client, MetadataKind::Series, "121361")
            .await
            .expect("series-on-movie-proxy should not error");
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn proxy_resolve_returns_none_for_404() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/movie/999999");
            then.status(404).body("not found");
        });

        let client = proxy_client_for(&server);
        let result = MetadataProvider::resolve_by_id(&client, MetadataKind::Movie, "999999")
            .await
            .expect("a 404 should not be an error");
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn proxy_search_parses_movie_hits() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(GET).path("/search").query_param("q", "matrix");
            then.status(200)
                .header("content-type", "application/json")
                .body(r#"[{"title": "The Matrix", "tmdbId": 603, "genres": ["Action"]}]"#);
        });

        let client = proxy_client_for(&server);
        let hits = MetadataProvider::search(&client, "matrix", MetadataKind::Movie)
            .await
            .expect("search should parse");

        mock.assert();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].title.as_deref(), Some("The Matrix"));
        assert_eq!(hits[0].provider_ids.get("tmdb"), Some(&"603".to_string()));
    }

    #[tokio::test]
    async fn proxy_trending_and_watch_providers_degrade_to_empty_not_error() {
        // AMETA-2 caveat: the proxy has no /trending or /watch/providers, so
        // these degrade to empty WITHOUT hitting the network or erroring.
        let server = MockServer::start();
        let client = proxy_client_for(&server);

        let trending = client
            .trending(TmdbMediaType::Movie, TrendingWindow::Day)
            .await
            .expect("trending must degrade, not error");
        assert!(trending.is_empty());

        let popular = client
            .popular(TmdbMediaType::Movie, None)
            .await
            .expect("popular must degrade, not error");
        assert!(popular.is_empty());

        let providers = client
            .watch_providers(TmdbMediaType::Movie, "603")
            .await
            .expect("watch_providers must degrade, not error");
        assert!(providers.is_empty());
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
        assert_eq!(
            result.genres,
            vec!["Action".to_string(), "Science Fiction".to_string()]
        );
        assert_eq!(result.provider_ids.get("tmdb"), Some(&"603".to_string()));
        assert_eq!(
            result.provider_ids.get("imdb"),
            Some(&"tt0133093".to_string())
        );
        assert_eq!(
            result.images.poster_url,
            Some(format!("{IMAGE_BASE_URL}/poster.jpg"))
        );
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
                .body(
                    r#"{"movie_results": [{"id": 603, "title": "The Matrix"}], "tv_results": []}"#,
                );
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
        assert_eq!(
            results[0].provider_ids.get("tmdb"),
            Some(&"329865".to_string())
        );
    }
}
