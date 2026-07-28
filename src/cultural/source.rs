//! MUSEX-07 (Plane TERM #383): the [`TrendSource`] seam — trending +
//! "the talk" (comment/rating volume), read-only, sourced from TMDb (already
//! integrated, see `crate::trending::TmdbClient`) and, config-gated,
//! Trakt.
//!
//! ## No-PII-egress by construction
//! [`TrendQuery`]/[`TalkQuery`] are the ONLY inputs a [`TrendSource`] impl
//! ever sees. Both are plain, public trend params — a region code, a
//! day/week window, an optional media kind, or a set of TMDb ids (a public
//! catalog identifier, not user data) — never an account id, watch
//! history, persona vector, or any other account-scoped signal. This is a
//! type-level guarantee (the trait signature has no way to smuggle an
//! `account_id` through), reinforced by two runtime negative tests: this
//! module's own `tests::trend_query_serialization_never_contains_account_scoped_values`
//! (a struct-level property test), and the load-bearing, DB-gated,
//! end-to-end one —
//! `crate::cultural::live_tests::orchestration_never_forwards_account_data_to_the_trend_source`
//! — which seeds a real account/persona with a PII-shaped name, runs it
//! through the actual `crate::cultural::the_talk_surface`/
//! `cold_start_recommendations` orchestration against a [`MockTrendSource`]
//! that records every call, and asserts none of the recorded, serialized
//! requests contain the seeded PII.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::sync::Mutex;
use std::time::Duration;

use crate::config::Config;
use crate::error::{MuseError, MuseResult};
use crate::models::media_metadata::MediaKind;
use crate::trending::{TmdbClient, TmdbMediaType, TrendingWindow};

/// Public, non-account-scoped trending query params. See the module doc's
/// "No-PII-egress by construction" section — this struct's fields are the
/// ENTIRE set of information any [`TrendSource`] impl's `trending` may act
/// on.
#[derive(Debug, Clone, Serialize)]
pub struct TrendQuery {
    /// ISO 3166-1 region code (e.g. `"US"`) — a content-region setting, not
    /// user data.
    pub region: String,
    pub window: TrendingWindow,
    /// Restrict to one media kind, or `None` for both movie + tv.
    pub kind: Option<MediaKind>,
}

// `TrendingWindow`/`MediaKind` already derive what they need
// (`TrendingWindow` is a plain enum in `crate::trending::client`); give
// `TrendQuery` a `Serialize` impl for the no-PII-egress test's JSON
// round-trip without requiring every field type to already have one.
impl Serialize for TrendingWindow {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

/// Public, non-account-scoped "the talk" query params: which titles (by
/// public catalog id, e.g. TMDb id) to pull comment/rating volume for.
/// Never an account id, never a watched-titles list keyed to a specific
/// account — the caller passes only the external ids of titles already
/// known (from a `trending` pull) to be culturally live.
#[derive(Debug, Clone, Serialize)]
pub struct TalkQuery {
    pub external_ids: Vec<String>,
}

/// One trending title, as returned by a [`TrendSource`].
#[derive(Debug, Clone, PartialEq)]
pub struct TrendEntry {
    /// The source's public catalog id for this title (TMDb id for
    /// [`TmdbTrendSource`], Trakt id for [`TraktTrendSource`]).
    pub external_id: String,
    pub kind: MediaKind,
    pub title: String,
    pub year: Option<i32>,
    /// Raw popularity score from the source, not yet normalized —
    /// callers scale as needed (mirrors `Candidate::taste_fit`'s own
    /// "raw source signal, not a final score" posture).
    pub popularity: f64,
}

/// One title's "the talk" volume signal, as returned by a [`TrendSource`].
#[derive(Debug, Clone, PartialEq)]
pub struct TalkEntry {
    pub external_id: String,
    /// Normalized `[0.0, 1.0]` talk-volume signal strength — how much
    /// comment/rating activity, relative to the source's own scale.
    pub talk_score: f64,
    pub comment_count: Option<i64>,
    pub rating_count: Option<i64>,
}

/// The read-only, no-PII-egress trending + "the talk" seam. A real
/// TMDb/Trakt dispatch and a deterministic mock both implement this trait,
/// so `crate::cultural`'s orchestration code never knows or cares which one
/// it's talking to — the same posture as `taste_review::panel::ReasoningPanel`.
#[async_trait]
pub trait TrendSource: Send + Sync {
    async fn trending(&self, query: &TrendQuery) -> MuseResult<Vec<TrendEntry>>;
    async fn talk(&self, query: &TalkQuery) -> MuseResult<Vec<TalkEntry>>;
}

// --- TMDb -------------------------------------------------------------------

/// Trending via the existing, already-integrated `crate::trending::TmdbClient`
/// (MUSE-19) — this impl adds no new TMDb surface, it just adapts
/// `TmdbClient::trending`/`popular` into the [`TrendSource`] shape.
///
/// TMDb has no comment/rating-volume endpoint (that's Trakt's role, see
/// [`TraktTrendSource`]) — `talk` here always returns
/// `Err(MuseError::NotImplemented)`, same explicit-not-silent posture as
/// `trending::OptionalSource::fetch`.
pub struct TmdbTrendSource {
    client: TmdbClient,
}

impl TmdbTrendSource {
    pub fn new(client: TmdbClient) -> Self {
        Self { client }
    }

    /// Build from `Config` (`TMDB_API_KEY`, reusing
    /// `TmdbClient::from_config`). Returns `None` when TMDb isn't
    /// configured — same graceful degrade as every other optional
    /// integration in this crate.
    ///
    /// **Also `None` in key-less proxy mode**, which is the subtle case.
    /// `TmdbClient::from_config` returns a client when `metadata_keyless` is
    /// set (the default) even with no API key — that client talks to the
    /// Radarr metadata proxy, which is genuinely useful for *enrichment*. But
    /// the proxy has no `/trending` endpoint, so `TmdbClient::trending`
    /// deliberately degrades to an empty vec in that mode (AMETA-2). Building
    /// a `TmdbTrendSource` on top of it therefore yields a trend source that
    /// can never, under any circumstances, return a trend.
    ///
    /// A source that is silently always-empty is worse than an absent one: an
    /// absent source lets the caller fall back to a working one (Trakt), while
    /// an always-empty source looks available and quietly contributes nothing.
    /// So proxy mode yields `None` here, even though the same client is still
    /// the right thing for metadata enrichment elsewhere.
    pub fn from_config(config: &Config) -> Option<Self> {
        let client = TmdbClient::from_config(config)?;
        if client.is_proxy_mode() {
            tracing::debug!(
                "cultural: TMDb client is in key-less proxy mode, which has no \
                 /trending endpoint — not registering a TMDb trend source"
            );
            return None;
        }
        Some(Self::new(client))
    }
}

#[async_trait]
impl TrendSource for TmdbTrendSource {
    async fn trending(&self, query: &TrendQuery) -> MuseResult<Vec<TrendEntry>> {
        let kinds: Vec<(TmdbMediaType, MediaKind)> = match query.kind {
            Some(MediaKind::Movie) => vec![(TmdbMediaType::Movie, MediaKind::Movie)],
            Some(MediaKind::Show) => vec![(TmdbMediaType::Tv, MediaKind::Show)],
            None => vec![
                (TmdbMediaType::Movie, MediaKind::Movie),
                (TmdbMediaType::Tv, MediaKind::Show),
            ],
        };

        let mut out = Vec::new();
        for (tmdb_kind, kind) in kinds {
            let titles = self.client.trending(tmdb_kind, query.window).await?;
            out.extend(titles.into_iter().map(|t| TrendEntry {
                external_id: t.id.to_string(),
                kind,
                title: t.display_title().unwrap_or_default().to_string(),
                year: t.year(),
                popularity: t.popularity.unwrap_or(0.0),
            }));
        }
        Ok(out)
    }

    async fn talk(&self, _query: &TalkQuery) -> MuseResult<Vec<TalkEntry>> {
        Err(MuseError::NotImplemented)
    }
}

// --- Trakt --------------------------------------------------------------

/// Minimal, config-gated Trakt client behind the [`TrendSource`] seam.
/// **Documented best-effort guess, not verified against a live Trakt
/// endpoint** — Muse had no Trakt integration before this item (see the
/// crate-wide grep this item's build note ran: `trakt` only existed as a
/// name in `trending::OptionalSource`, never a client). The request/response
/// shapes below mirror Trakt's public API docs (`/movies|shows/trending`,
/// `/movies|shows/{id}/comments`) but, like
/// `taste_review::panel::TerminusReasoningPanel`, should be re-verified
/// against a live sandbox before this is relied on in production. Inert
/// (no live call, no startup impact) unless `TRAKT_CLIENT_ID` is set.
pub struct TraktTrendSource {
    http: reqwest::Client,
    base_url: String,
    client_id: String,
    api_key: Option<String>,
}

/// Trakt's public API host — the default `from_config` uses when
/// `Config::trakt_base_url` (`MUSE_TRAKT_BASE_URL`) is unset. Public (like
/// `TmdbClient`'s own default base URL is a fine literal) since it's an
/// API endpoint, not a credential; `from_config` still lets it be
/// overridden for tests/proxying.
pub const TRAKT_DEFAULT_BASE_URL: &str = "https://api.trakt.tv";
const TRAKT_REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

impl TraktTrendSource {
    pub fn new(
        base_url: impl Into<String>,
        client_id: impl Into<String>,
        api_key: Option<String>,
    ) -> MuseResult<Self> {
        let http = reqwest::Client::builder()
            .timeout(TRAKT_REQUEST_TIMEOUT)
            .build()
            .map_err(MuseError::Http)?;

        Ok(Self {
            http,
            base_url: base_url.into().trim_end_matches('/').to_string(),
            client_id: client_id.into(),
            api_key,
        })
    }

    /// Build from `Config` (`TRAKT_CLIENT_ID` + `TRAKT_API_KEY` +
    /// `MUSE_TRAKT_BASE_URL`). Returns `None` when `trakt_client_id` isn't
    /// set — the Trakt half of the cultural layer simply doesn't run, same
    /// graceful-degrade posture as `TmdbClient::from_config`.
    ///
    /// The base URL is `Config::trakt_base_url` when set (a test's httpmock
    /// server, or an on-prem Trakt proxy), otherwise [`TRAKT_DEFAULT_BASE_URL`]
    /// — the same overridable-default seam `TmdbClient::new(base_url, ..)`
    /// provides for TMDb.
    pub fn from_config(config: &Config) -> Option<Self> {
        let client_id = config.trakt_client_id.clone()?;
        let base_url = config
            .trakt_base_url
            .clone()
            .unwrap_or_else(|| TRAKT_DEFAULT_BASE_URL.to_string());
        match Self::new(base_url, client_id, config.trakt_api_key.clone()) {
            Ok(client) => Some(client),
            Err(e) => {
                tracing::warn!(error = %e, "MUSEX-07: failed to construct Trakt client; talk/trending via Trakt will degrade");
                None
            }
        }
    }

    fn kind_path(kind: MediaKind) -> &'static str {
        match kind {
            MediaKind::Movie => "movies",
            MediaKind::Show => "shows",
        }
    }

    async fn get(&self, path: &str) -> MuseResult<Vec<u8>> {
        let url = format!("{}{}", self.base_url, path);
        let mut req = self
            .http
            .get(&url)
            .header("trakt-api-version", "2")
            .header("trakt-api-key", &self.client_id);
        if let Some(key) = &self.api_key {
            req = req.bearer_auth(key);
        }

        let resp = req.send().await?;
        let status = resp.status();
        let bytes = resp.bytes().await?;

        if !status.is_success() {
            let body = String::from_utf8_lossy(&bytes).to_string();
            return Err(MuseError::Upstream {
                status: status.as_u16(),
                message: format!("trakt request to {path} failed: {body}"),
            });
        }
        Ok(bytes.to_vec())
    }
}

#[derive(Debug, Deserialize)]
struct TraktTrendingEntry {
    #[serde(default)]
    watchers: Option<i64>,
    #[serde(default)]
    movie: Option<TraktTitle>,
    #[serde(default)]
    show: Option<TraktTitle>,
}

#[derive(Debug, Deserialize)]
struct TraktTitle {
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    year: Option<i32>,
    ids: TraktIds,
}

#[derive(Debug, Deserialize)]
struct TraktIds {
    #[serde(default)]
    tmdb: Option<i64>,
    #[serde(default)]
    trakt: Option<i64>,
}

/// Only the comment COUNT matters to `talk` (see below) — this
/// intentionally deserializes no fields (serde ignores unrecognized JSON
/// object fields by default), just enough shape to count array elements.
#[derive(Debug, Deserialize)]
struct TraktComment {}

#[async_trait]
impl TrendSource for TraktTrendSource {
    async fn trending(&self, query: &TrendQuery) -> MuseResult<Vec<TrendEntry>> {
        let kinds: Vec<MediaKind> = match query.kind {
            Some(k) => vec![k],
            None => vec![MediaKind::Movie, MediaKind::Show],
        };

        let mut out = Vec::new();
        for kind in kinds {
            let path = format!("/{}/trending", Self::kind_path(kind));
            let bytes = self.get(&path).await?;
            let entries: Vec<TraktTrendingEntry> =
                serde_json::from_slice(&bytes).map_err(|e| MuseError::Upstream {
                    status: 200,
                    message: format!("failed to parse trakt trending response from {path}: {e}"),
                })?;

            for entry in entries {
                let title = entry.movie.or(entry.show);
                let Some(title) = title else { continue };
                // Prefer the TMDb id when Trakt provides one, so this
                // source's `external_id` lines up with the same catalog id
                // `TmdbTrendSource`/`media_metadata.tmdb_id` use — falls
                // back to the Trakt id only when Trakt didn't map one.
                let external_id = title.ids.tmdb.or(title.ids.trakt).map(|id| id.to_string());
                let Some(external_id) = external_id else {
                    continue;
                };

                out.push(TrendEntry {
                    external_id,
                    kind,
                    title: title.title.unwrap_or_default(),
                    year: title.year,
                    popularity: entry.watchers.unwrap_or(0) as f64,
                });
            }
        }
        Ok(out)
    }

    async fn talk(&self, query: &TalkQuery) -> MuseResult<Vec<TalkEntry>> {
        let mut out = Vec::with_capacity(query.external_ids.len());
        for external_id in &query.external_ids {
            // Best-effort: Trakt's comments endpoint is keyed by Trakt's own
            // slug/id, not a raw TMDb id — a real deployment would need a
            // TMDb->Trakt id resolution step this module doesn't have yet
            // (no Trakt search client). Treat a lookup failure as "no talk
            // signal for this title" (skip), never a hard error for the
            // whole batch — one bad id shouldn't blank the entire surface.
            let path = format!("/movies/{external_id}/comments?extended=full");
            let Ok(bytes) = self.get(&path).await else {
                continue;
            };
            let Ok(comments) = serde_json::from_slice::<Vec<TraktComment>>(&bytes) else {
                continue;
            };

            let comment_count = comments.len() as i64;
            // Normalize into [0.0, 1.0] against a documented reference
            // volume (50 comments = "very talked about") rather than an
            // unbounded raw count — matches `Candidate::taste_fit`'s
            // normalized-signal convention.
            const REFERENCE_COMMENT_VOLUME: f64 = 50.0;
            let talk_score = (comment_count as f64 / REFERENCE_COMMENT_VOLUME).clamp(0.0, 1.0);

            out.push(TalkEntry {
                external_id: external_id.clone(),
                talk_score,
                comment_count: Some(comment_count),
                rating_count: None,
            });
        }
        Ok(out)
    }
}

// --- Mock (tests only) -------------------------------------------------

/// A deterministic, network-free [`TrendSource`] for tests. Records every
/// query it receives (the seam the no-PII-egress negative test and the
/// trend-cache rate-limit test both inspect) and returns whatever canned
/// data it was constructed with.
#[derive(Default)]
pub struct MockTrendSource {
    trending_results: Vec<TrendEntry>,
    talk_results: Vec<TalkEntry>,
    /// Every [`TrendQuery`] this mock's `trending` was called with, in call
    /// order — the no-PII-egress test serializes these and asserts no
    /// seeded PII value appears; the trend-cache test asserts `.len()`.
    pub trending_calls: Mutex<Vec<TrendQuery>>,
    /// Every [`TalkQuery`] this mock's `talk` was called with, in call
    /// order — same purpose as `trending_calls`.
    pub talk_calls: Mutex<Vec<TalkQuery>>,
}

impl MockTrendSource {
    pub fn new(trending_results: Vec<TrendEntry>, talk_results: Vec<TalkEntry>) -> Self {
        Self {
            trending_results,
            talk_results,
            trending_calls: Mutex::new(Vec::new()),
            talk_calls: Mutex::new(Vec::new()),
        }
    }

    pub fn trending_call_count(&self) -> usize {
        self.trending_calls.lock().unwrap().len()
    }

    pub fn talk_call_count(&self) -> usize {
        self.talk_calls.lock().unwrap().len()
    }
}

#[async_trait]
impl TrendSource for MockTrendSource {
    async fn trending(&self, query: &TrendQuery) -> MuseResult<Vec<TrendEntry>> {
        self.trending_calls.lock().unwrap().push(query.clone());
        Ok(self.trending_results.clone())
    }

    async fn talk(&self, query: &TalkQuery) -> MuseResult<Vec<TalkEntry>> {
        self.talk_calls.lock().unwrap().push(query.clone());
        Ok(self.talk_results.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(external_id: &str) -> TrendEntry {
        TrendEntry {
            external_id: external_id.to_string(),
            kind: MediaKind::Show,
            title: "Severance".to_string(),
            year: Some(2022),
            popularity: 99.0,
        }
    }

    #[tokio::test]
    async fn mock_trend_source_records_calls_and_returns_canned_data() {
        let mock = MockTrendSource::new(vec![entry("12345")], vec![]);
        let query = TrendQuery {
            region: "US".to_string(),
            window: TrendingWindow::Day,
            kind: None,
        };

        let result = mock
            .trending(&query)
            .await
            .expect("trending should succeed");
        assert_eq!(result, vec![entry("12345")]);
        assert_eq!(mock.trending_call_count(), 1);
    }

    /// Load-bearing sovereignty test: a [`TrendQuery`] built the way
    /// `crate::cultural`'s orchestration builds it (region/window/kind
    /// only) never serializes to include ANY of the account-scoped values
    /// a caller might have in scope at the call site (account id, a watched
    /// title, a persona name). This is the type-level guarantee from the
    /// module doc, made into a runtime assertion.
    #[test]
    fn trend_query_serialization_never_contains_account_scoped_values() {
        // Values that WOULD be PII/account-scoped if they leaked into the
        // outbound request -- none of them are constructor params of
        // `TrendQuery`, so this also documents what "no PII egress" means
        // concretely for this struct.
        let account_id = 424242_i64;
        let watched_title = "MySecretWatchedShow_Do_Not_Leak";
        let persona_name = "solo-2am-account-424242";

        let query = TrendQuery {
            region: "US".to_string(),
            window: TrendingWindow::Week,
            kind: Some(MediaKind::Show),
        };
        let json = serde_json::to_string(&query).expect("TrendQuery should serialize");

        assert!(!json.contains(&account_id.to_string()));
        assert!(!json.contains(watched_title));
        assert!(!json.contains(persona_name));
        assert!(!json.contains("account"));
        assert!(!json.contains("persona"));
        assert!(!json.contains("watch"));
    }

    #[test]
    fn talk_query_serialization_carries_only_public_catalog_ids() {
        let query = TalkQuery {
            external_ids: vec!["27205".to_string(), "603".to_string()],
        };
        let json = serde_json::to_string(&query).expect("TalkQuery should serialize");
        assert_eq!(json, r#"{"external_ids":["27205","603"]}"#);
    }

    #[test]
    fn tmdb_trend_source_from_config_returns_none_when_unconfigured() {
        // A default Config has no TMDB_API_KEY. Note this passes for a reason
        // that changed under it: `metadata_keyless` defaults true, so
        // `TmdbClient::from_config` DOES hand back a (proxy-mode) client here.
        // What makes this None is the proxy-mode check in
        // `TmdbTrendSource::from_config` — see the regression test below.
        let config = Config::default();
        assert!(TmdbTrendSource::from_config(&config).is_none());
    }

    #[test]
    fn tmdb_trend_source_is_none_in_keyless_proxy_mode_even_though_the_client_builds() {
        // The regression this fixes. The AMETA key-less metadata proxy made
        // `TmdbClient::from_config` return Some without an API key, which
        // silently turned this into an always-empty trend source (the proxy
        // has no /trending endpoint, so `trending()` returns an empty vec by
        // design). The test above had asserted None since before AMETA and
        // began failing on main; relaxing it would have locked in the bad
        // behaviour, so the source is gated on proxy mode instead.
        let config = Config {
            tmdb_api_key: None,
            metadata_keyless: true,
            ..Default::default()
        };

        // The client itself is available and useful — for enrichment.
        let client = crate::trending::client::TmdbClient::from_config(&config)
            .expect("key-less proxy client should still build for enrichment");
        assert!(client.is_proxy_mode(), "precondition: this is proxy mode");

        // ...but it must not be offered as a TREND source.
        assert!(
            TmdbTrendSource::from_config(&config).is_none(),
            "a trend source that can never return a trend must not be registered"
        );
    }

    #[test]
    fn tmdb_trend_source_is_none_when_keyless_is_disabled_and_no_key_is_set() {
        // The third precedence branch: no key and keyless off means the client
        // itself is None, so the trend source is too.
        let config = Config {
            tmdb_api_key: None,
            metadata_keyless: false,
            ..Default::default()
        };
        assert!(TmdbTrendSource::from_config(&config).is_none());
    }

    #[test]
    fn tmdb_trend_source_from_config_builds_when_configured() {
        let config = Config {
            tmdb_api_key: Some("abc123".to_string()),
            ..Default::default()
        };
        assert!(TmdbTrendSource::from_config(&config).is_some());
    }

    #[test]
    fn trakt_trend_source_from_config_returns_none_when_unconfigured() {
        let config = Config::default();
        assert!(TraktTrendSource::from_config(&config).is_none());
    }

    #[test]
    fn trakt_trend_source_from_config_builds_when_configured() {
        let config = Config {
            trakt_client_id: Some("client-id".to_string()),
            ..Default::default()
        };
        assert!(TraktTrendSource::from_config(&config).is_some());
    }

    /// The base-URL override seam (matching `TmdbClient`'s httpmock test):
    /// `from_config` must honor `Config::trakt_base_url`, so a test server
    /// (here httpmock) — never the live `api.trakt.tv` — receives the
    /// request. Also asserts the required `trakt-api-key` header carries
    /// the configured client id and NO account-scoped param rides along
    /// (the query is a bare public trending endpoint).
    #[tokio::test]
    async fn trakt_from_config_honors_base_url_override_and_sends_client_id_header() {
        use httpmock::prelude::*;

        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(GET)
                .path("/shows/trending")
                .header("trakt-api-key", "test-client-id");
            then.status(200).header("content-type", "application/json").body(
                r#"[
                    {"watchers": 4200, "show": {"title": "Severance", "year": 2022, "ids": {"tmdb": 95396, "trakt": 152041}}}
                ]"#,
            );
        });

        // Build the way production does — through `from_config` — but with
        // the base URL pointed at the mock server via the new override.
        let config = Config {
            trakt_client_id: Some("test-client-id".to_string()),
            trakt_base_url: Some(server.base_url()),
            ..Default::default()
        };
        let source =
            TraktTrendSource::from_config(&config).expect("configured Trakt source should build");

        let entries = source
            .trending(&TrendQuery {
                region: "US".to_string(),
                window: TrendingWindow::Day,
                kind: Some(MediaKind::Show),
            })
            .await
            .expect("trakt trending should parse against the mock server");

        mock.assert(); // proves the mock base URL was honored, not api.trakt.tv
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].external_id, "95396"); // TMDb id preferred over trakt id
        assert_eq!(entries[0].title, "Severance");
        assert_eq!(entries[0].popularity, 4200.0);
    }
}
