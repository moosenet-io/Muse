//! On-demand targeted Prowlarr search (MUSEM-03, blueprint §4).
//!
//! The existing report-pull path (`worker.rs`) is a scheduled, no-query RSS
//! sweep across every enabled indexer -- this module is the other kind of
//! call: "search this specific title *now*" against `GET /api/v1/search`,
//! for a caller (MUSEM-04's decision/scoring engine) that needs fresh
//! candidate releases for one title on demand. It is deliberately
//! read/parse-only: no grab decision, no persistence, no write path -- same
//! "report-pull, never a grab" framing as the rest of `prowlarr::client`.
//!
//! This reuses, rather than duplicates:
//! - [`ProwlarrClient::targeted_search`] for the actual HTTP call, which
//!   already enforces the shared hourly rate-limit budget
//!   (`RateLimiter::gate_hourly_cap`) on the *same* `RateLimiter` instance
//!   the report-pull path's `rss_pull` uses -- an on-demand search and the
//!   scheduled report-pull worker draw from one client's one budget.
//! - [`parse_release_name`] for release-name parsing (quality/edition/
//!   revision/language/release-group), exactly as `worker.rs` does per
//!   report-pulled release.

use crate::config::Config;
use crate::error::MuseResult;

use super::client::ProwlarrClient;
use super::models::ProwlarrRelease;
use super::parse::{parse_release_name, ParsedRelease};

/// One parsed candidate release from an on-demand targeted search.
///
/// Carries both the raw Prowlarr report fields -- including the
/// Prowlarr-proxied `download_url` preserved verbatim for the eventual grab
/// step, and null-safe `seeders`/`leechers` (private-tracker results can
/// report neither; that's "unknown", not "zero" -- blueprint §4 edge case)
/// -- and Muse's own deterministic parse of the release title
/// (`parsed`), plus Prowlarr's own extracted `{imdb_id, tmdb_id, tvdb_id}`
/// guess. Both id sources are kept side by side rather than one replacing
/// the other, per the blueprint's explicit warning against relying solely
/// on Prowlarr's guess.
#[derive(Debug, Clone, PartialEq)]
pub struct SearchRelease {
    pub guid: String,
    pub title: String,
    pub indexer_id: i32,
    pub indexer: Option<String>,
    pub protocol: Option<String>,
    pub size: Option<i64>,
    pub publish_date: Option<chrono::DateTime<chrono::Utc>>,
    pub info_url: Option<String>,
    /// The Prowlarr-proxied download URL, preserved verbatim -- this is the
    /// opaque, apikey-bearing redirect link the eventual grab step needs,
    /// not the tracker's own URL (blueprint §4).
    pub download_url: Option<String>,
    pub info_hash: Option<String>,
    /// `None` means "not reported by this indexer" (common on private
    /// trackers), never coerced to `0` -- see `ProwlarrRelease`.
    pub seeders: Option<i32>,
    pub leechers: Option<i32>,
    pub grabs: Option<i32>,
    pub freeleech: bool,
    pub indexer_flags: Vec<String>,
    pub categories: Vec<i32>,
    /// Prowlarr's own extracted ids (blueprint §4: `0` = unknown,
    /// normalized to `None` by `ProwlarrRelease::{imdb,tmdb,tvdb}_id`).
    pub imdb_id: Option<i64>,
    pub tmdb_id: Option<i64>,
    pub tvdb_id: Option<i64>,
    /// Muse's own deterministic release-name parse
    /// (`parse::parse_release_name`) of `title`. An unparseable title is
    /// never dropped from the result set -- it simply carries a low/zero
    /// `parsed.confidence`, same posture as the report-pull path.
    pub parsed: ParsedRelease,
}

impl From<ProwlarrRelease> for SearchRelease {
    fn from(release: ProwlarrRelease) -> Self {
        let parsed = parse_release_name(&release.title);
        let freeleech = release.is_freeleech() || parsed.freeleech;
        let categories = release.category_ids();
        let imdb_id = release.imdb_id();
        let tmdb_id = release.tmdb_id();
        let tvdb_id = release.tvdb_id();

        Self {
            guid: release.guid,
            title: release.title,
            indexer_id: release.indexer_id,
            indexer: release.indexer,
            protocol: release.protocol,
            size: release.size,
            publish_date: release.publish_date,
            info_url: release.info_url,
            download_url: release.download_url,
            info_hash: release.info_hash,
            seeders: release.seeders,
            leechers: release.leechers,
            grabs: release.grabs,
            freeleech,
            indexer_flags: release.indexer_flags,
            categories,
            imdb_id,
            tmdb_id,
            tvdb_id,
            parsed,
        }
    }
}

/// Run an on-demand targeted search against Prowlarr `/api/v1/search`
/// (blueprint §4) and return parsed candidate releases.
///
/// `query`/`tmdb_id` follow `ProwlarrClient::targeted_search`'s own
/// preference (ID-based lookup over free text when both are given); at
/// least one of the two is required. `categories`/`indexer_ids` narrow the
/// search the same way report-pull does. The rolling hourly search budget
/// comes from `Config::prowlarr_search_max_per_hour` -- shared, via the
/// client's single `RateLimiter`, with the report-pull worker's own calls.
///
/// A rate-limit hit surfaces as `Err(MuseError::Conflict)` (the caller's
/// cue to back off / retry later per the existing limiter, not a hard
/// failure); an upstream non-2xx or malformed-JSON response surfaces as
/// `Err(MuseError::Upstream)`. Neither is ever silently swallowed into an
/// empty `Vec` -- a genuine "no releases found" result is a distinct `Ok`
/// with an empty `Vec` (blueprint §4 edge case), never conflated with a
/// failed search.
pub async fn search_releases(
    client: &ProwlarrClient,
    config: &Config,
    query: Option<&str>,
    tmdb_id: Option<&str>,
    categories: &[i32],
    indexer_ids: &[i32],
) -> MuseResult<Vec<SearchRelease>> {
    let max_per_hour = config.prowlarr_search_max_per_hour.max(1) as usize;

    let releases = client
        .targeted_search(query, tmdb_id, categories, indexer_ids, max_per_hour)
        .await?;

    if releases.is_empty() {
        tracing::debug!(?query, ?tmdb_id, "on-demand prowlarr search returned no releases");
    }

    Ok(releases.into_iter().map(SearchRelease::from).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::MuseError;
    use httpmock::prelude::*;

    fn client_for(server: &MockServer) -> ProwlarrClient {
        ProwlarrClient::new(server.base_url(), "test-key").expect("client should construct")
    }

    fn test_config() -> Config {
        Config {
            prowlarr_search_max_per_hour: 30,
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn search_releases_parses_a_mixed_result_set() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(GET)
                .path("/api/v1/search")
                .query_param("query", "The Matrix");
            then.status(200)
                .header("content-type", "application/json")
                .body(
                    r#"[
                        {
                            "guid": "public-1",
                            "title": "The.Matrix.1999.1080p.BluRay.x264-GRP",
                            "indexerId": 1, "indexer": "PublicTracker", "protocol": "torrent",
                            "size": 4300000000, "seeders": 42, "leechers": 3, "grabs": 10,
                            "downloadUrl": "http://example.invalid/dl/public-1",
                            "infoUrl": "http://example.invalid/info/public-1",
                            "indexerFlags": ["freeleech"],
                            "categories": [{"id": 2000, "name": "Movies"}],
                            "imdbId": 133093, "tmdbId": 603, "tvdbId": 0
                        },
                        {
                            "guid": "private-1",
                            "title": "The.Matrix.1999.2160p.REMUX-PRIV",
                            "indexerId": 2, "indexer": "PrivateTracker", "protocol": "torrent",
                            "size": 40000000000,
                            "seeders": null, "leechers": null, "infoHash": null,
                            "downloadUrl": "http://example.invalid/dl/private-1",
                            "categories": [{"id": 2000, "name": "Movies"}],
                            "imdbId": 0, "tmdbId": 0, "tvdbId": 0
                        }
                    ]"#,
                );
        });

        let client = client_for(&server);
        let config = test_config();

        let results = search_releases(&client, &config, Some("The Matrix"), None, &[2000], &[1, 2])
            .await
            .expect("search should succeed");

        mock.assert();
        assert_eq!(results.len(), 2);

        let public = results.iter().find(|r| r.guid == "public-1").unwrap();
        assert!(public.freeleech, "indexerFlags freeleech should be preserved");
        assert_eq!(public.seeders, Some(42));
        assert_eq!(public.download_url.as_deref(), Some("http://example.invalid/dl/public-1"));
        assert_eq!(public.imdb_id, Some(133093));
        assert_eq!(public.tmdb_id, Some(603));
        assert_eq!(public.tvdb_id, None, "tvdbId 0 should normalize to None, not Some(0)");
        assert_eq!(public.parsed.source.as_deref(), Some("BluRay"));

        let private = results.iter().find(|r| r.guid == "private-1").unwrap();
        assert_eq!(
            private.seeders, None,
            "a null-seeders private result should stay Option::None, not be coerced to 0"
        );
        assert_eq!(private.leechers, None);
        assert_eq!(
            private.download_url.as_deref(),
            Some("http://example.invalid/dl/private-1"),
            "downloadUrl must be preserved verbatim for the grab step"
        );
        assert_eq!(private.imdb_id, None);
    }

    #[tokio::test]
    async fn search_releases_upstream_5xx_is_a_typed_error_not_an_empty_vec() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/api/v1/search");
            then.status(503).body("indexer unavailable");
        });

        let client = client_for(&server);
        let config = test_config();

        let err = search_releases(&client, &config, Some("query"), None, &[2000], &[1])
            .await
            .expect_err("a 5xx from prowlarr must surface as an error, not Ok(vec![])");

        match err {
            MuseError::Upstream { status, .. } => assert_eq!(status, 503),
            other => panic!("expected Upstream error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn search_releases_keeps_an_unparseable_title_with_low_confidence() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/api/v1/search");
            then.status(200)
                .header("content-type", "application/json")
                .body(
                    r#"[{"guid": "weird-1", "title": "just_some_random_words", "indexerId": 1,
                         "categories": []}]"#,
                );
        });

        let client = client_for(&server);
        let config = test_config();

        let results = search_releases(&client, &config, Some("query"), None, &[], &[1])
            .await
            .expect("search should succeed even for an unparseable title");

        assert_eq!(results.len(), 1, "an unparseable title must not be silently dropped");
        assert!(results[0].parsed.confidence < 0.3);
    }

    #[tokio::test]
    async fn search_releases_empty_result_is_ok_not_an_error() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/api/v1/search");
            then.status(200)
                .header("content-type", "application/json")
                .body("[]");
        });

        let client = client_for(&server);
        let config = test_config();

        let results = search_releases(&client, &config, Some("nonexistent title"), None, &[2000], &[1])
            .await
            .expect("zero results is a legitimate Ok, not an error");
        assert!(results.is_empty());
    }

    #[tokio::test]
    async fn search_releases_shares_the_client_hourly_rate_limit() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/api/v1/search");
            then.status(200)
                .header("content-type", "application/json")
                .body("[]");
        });

        let client = client_for(&server);
        let config = Config {
            prowlarr_search_max_per_hour: 1,
            ..Default::default()
        };

        search_releases(&client, &config, Some("first"), None, &[2000], &[1])
            .await
            .expect("first search within budget should succeed");

        let err = search_releases(&client, &config, Some("second"), None, &[2000], &[1])
            .await
            .expect_err("second search should exceed the configured hourly cap of 1");
        match err {
            MuseError::Conflict(_) => {}
            other => panic!("expected Conflict, got {other:?}"),
        }
    }
}
