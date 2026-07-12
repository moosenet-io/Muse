//! Read-only Radarr/Sonarr `*arr` API v3 HTTP client.
//!
//! MUSE-05: mirrors `plex::PlexClient` — a *pure* typed HTTP client, no
//! persistence of its own (that's `ingest::run`). One [`ArrClient`] talks to
//! exactly one instance (Radarr or Sonarr both speak the same v3 envelope
//! shape); the multi-instance fan-out lives in `ingest`.

use std::time::Duration;

use serde::de::DeserializeOwned;

use crate::error::{MuseError, MuseResult};

use super::models::{RadarrMovie, SonarrEpisode, SonarrEpisodeFile, SonarrSeries};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

/// A typed, read-only *arr (Radarr/Sonarr) client for a single instance.
#[derive(Debug, Clone)]
pub struct ArrClient {
    http: reqwest::Client,
    base_url: String,
    api_key: String,
}

impl ArrClient {
    /// Build a client against a specific instance base URL (e.g.
    /// `http://192.0.2.10:7878`) and its own API key. Never shared across
    /// instances — each *arr instance has its own key (blueprint §8).
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

    /// Build a client from a configured instance entry.
    pub fn from_instance(instance: &super::config::ArrInstanceConfig) -> MuseResult<Self> {
        Self::new(instance.base_url.clone(), instance.api_key.clone())
    }

    async fn get<T: DeserializeOwned>(&self, path: &str, query: &[(&str, &str)]) -> MuseResult<T> {
        let url = format!("{}{}", self.base_url, path);

        let resp = self
            .http
            .get(&url)
            .header("X-Api-Key", &self.api_key)
            .query(query)
            .send()
            .await?;

        let status = resp.status();
        let bytes = resp.bytes().await?;

        if !status.is_success() {
            let body = String::from_utf8_lossy(&bytes).to_string();
            return Err(MuseError::Upstream {
                status: status.as_u16(),
                message: format!("arr request to {url} failed: {body}"),
            });
        }

        serde_json::from_slice::<T>(&bytes).map_err(|e| MuseError::Upstream {
            status: status.as_u16(),
            message: format!("failed to parse arr response from {url}: {e}"),
        })
    }

    /// `GET /api/v3/movie` — every movie on this Radarr instance
    /// (join-flattened `MovieMetadata` + `Movies` + `MovieFiles`, blueprint
    /// §5). Only meaningful against a Radarr instance.
    pub async fn movies(&self) -> MuseResult<Vec<RadarrMovie>> {
        self.get("/api/v3/movie", &[]).await
    }

    /// `GET /api/v3/series` — every series on this Sonarr instance
    /// (blueprint §5). Only meaningful against a Sonarr instance.
    pub async fn series(&self) -> MuseResult<Vec<SonarrSeries>> {
        self.get("/api/v3/series", &[]).await
    }

    /// `GET /api/v3/episode?seriesId=` — every episode for one series.
    pub async fn episodes(&self, series_id: i64) -> MuseResult<Vec<SonarrEpisode>> {
        let sid = series_id.to_string();
        self.get("/api/v3/episode", &[("seriesId", sid.as_str())])
            .await
    }

    /// `GET /api/v3/episodefile?seriesId=` — every episode file for one
    /// series (season-pack files appear once here, satisfying N episodes —
    /// see `ingest::ingest_sonarr_instance`).
    pub async fn episode_files(&self, series_id: i64) -> MuseResult<Vec<SonarrEpisodeFile>> {
        let sid = series_id.to_string();
        self.get("/api/v3/episodefile", &[("seriesId", sid.as_str())])
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use httpmock::prelude::*;

    fn client_for(server: &MockServer) -> ArrClient {
        ArrClient::new(server.base_url(), "test-api-key").expect("client should construct")
    }

    #[tokio::test]
    async fn movies_parses_radarr_join_flattened_shape() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(GET)
                .path("/api/v3/movie")
                .header("X-Api-Key", "test-api-key");
            then.status(200)
                .header("content-type", "application/json")
                .body(
                    r#"[
                        {
                            "id": 1,
                            "title": "Arrival",
                            "originalTitle": "Arrival",
                            "sortTitle": "arrival",
                            "status": "released",
                            "overview": "A linguist deciphers alien language.",
                            "year": 2016,
                            "path": "/media/Movies/Arrival (2016)",
                            "hasFile": true,
                            "monitored": true,
                            "minimumAvailability": "released",
                            "tmdbId": 329865,
                            "imdbId": "tt2543164",
                            "runtime": 116,
                            "studio": "Paramount",
                            "originalLanguage": {"id": 1, "name": "English"},
                            "images": [{"coverType": "poster", "url": "/poster.jpg"}],
                            "added": "2020-01-01T00:00:00Z",
                            "movieFile": {
                                "id": 10,
                                "relativePath": "Arrival (2016)/Arrival.2016.1080p.BluRay.Remux-FGT.mkv",
                                "size": 30000000000,
                                "releaseGroup": "FGT",
                                "languages": [{"id": 1, "name": "English"}],
                                "quality": {
                                    "quality": {"id": 30, "name": "Bluray-1080p Remux", "source": "bluray", "resolution": 1080},
                                    "revision": {"version": 1, "real": 0, "isRepack": false}
                                }
                            }
                        }
                    ]"#,
                );
        });

        let client = client_for(&server);
        let movies = client.movies().await.expect("movies should parse");

        mock.assert();
        assert_eq!(movies.len(), 1);
        let movie = &movies[0];
        assert_eq!(movie.title, "Arrival");
        assert_eq!(movie.tmdb_id, 329865);
        assert_eq!(movie.imdb_id.as_deref(), Some("tt2543164"));
        assert!(movie.has_file);
        let file = movie.movie_file.as_ref().expect("movie file should parse");
        assert_eq!(file.release_group.as_deref(), Some("FGT"));
        let quality = file.quality.as_ref().expect("quality should parse");
        assert_eq!(quality.quality.id, 30);
        assert_eq!(quality.quality.resolution, Some(1080));
        assert_eq!(quality.revision.version, 1);
        assert!(!quality.revision.is_repack);
    }

    #[tokio::test]
    async fn series_parses_sonarr_embedded_seasons() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(GET).path("/api/v3/series");
            then.status(200)
                .header("content-type", "application/json")
                .body(
                    r#"[
                        {
                            "id": 5,
                            "title": "Test Show",
                            "sortTitle": "test show",
                            "status": "continuing",
                            "overview": "A show.",
                            "network": "Test Network",
                            "path": "/media/TV/Test Show",
                            "monitored": true,
                            "tvdbId": 99999,
                            "tvRageId": 111,
                            "tvMazeId": 222,
                            "tmdbId": 0,
                            "imdbId": "tt0000000",
                            "malIds": [],
                            "aniListIds": [],
                            "year": 2021,
                            "runtime": 30,
                            "images": [],
                            "seasons": [
                                {"seasonNumber": 0, "monitored": false},
                                {"seasonNumber": 1, "monitored": true}
                            ],
                            "added": "2021-01-01T00:00:00Z"
                        }
                    ]"#,
                );
        });

        let client = client_for(&server);
        let series = client.series().await.expect("series should parse");

        mock.assert();
        assert_eq!(series.len(), 1);
        let show = &series[0];
        assert_eq!(show.tvdb_id, 99999);
        assert_eq!(show.tmdb_id_opt(), None);
        assert_eq!(show.seasons.len(), 2);
        assert_eq!(show.seasons[1].season_number, 1);
        assert!(show.seasons[1].monitored);
    }

    #[tokio::test]
    async fn episodes_and_episode_files_parse_and_correlate_by_query_param() {
        let server = MockServer::start();
        let episodes_mock = server.mock(|when, then| {
            when.method(GET)
                .path("/api/v3/episode")
                .query_param("seriesId", "5");
            then.status(200)
                .header("content-type", "application/json")
                .body(
                    r#"[
                        {"id": 100, "seasonNumber": 1, "episodeNumber": 1, "title": "Pilot", "monitored": true, "hasFile": true, "episodeFileId": 500},
                        {"id": 101, "seasonNumber": 1, "episodeNumber": 2, "title": "Ep 2", "monitored": true, "hasFile": true, "episodeFileId": 500}
                    ]"#,
                );
        });
        let files_mock = server.mock(|when, then| {
            when.method(GET)
                .path("/api/v3/episodefile")
                .query_param("seriesId", "5");
            then.status(200)
                .header("content-type", "application/json")
                .body(
                    r#"[
                        {
                            "id": 500,
                            "seasonNumber": 1,
                            "relativePath": "Test Show/Season 01/Test.Show.S01.1080p.WEB-DL.mkv",
                            "size": 8000000000,
                            "releaseGroup": "NTb",
                            "languages": [{"id": 1, "name": "English"}],
                            "quality": {
                                "quality": {"id": 3, "name": "WEBDL-1080p", "source": "webdl", "resolution": 1080},
                                "revision": {"version": 1, "real": 0, "isRepack": false}
                            },
                            "releaseType": "seasonPack"
                        }
                    ]"#,
                );
        });

        let client = client_for(&server);
        let episodes = client.episodes(5).await.expect("episodes should parse");
        let files = client
            .episode_files(5)
            .await
            .expect("episode files should parse");

        episodes_mock.assert();
        files_mock.assert();

        assert_eq!(episodes.len(), 2);
        assert_eq!(episodes[0].episode_file_id_opt(), Some(500));
        assert_eq!(episodes[1].episode_file_id_opt(), Some(500));

        assert_eq!(files.len(), 1);
        assert_eq!(files[0].release_type.as_deref(), Some("seasonPack"));
    }

    #[tokio::test]
    async fn upstream_error_status_is_surfaced_not_panicked() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/api/v3/movie");
            then.status(401).body("unauthorized");
        });

        let client = client_for(&server);
        let result = client.movies().await;

        assert!(result.is_err());
        match result.unwrap_err() {
            MuseError::Upstream { status, .. } => assert_eq!(status, 401),
            other => panic!("expected Upstream error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn connection_failure_is_surfaced_not_panicked() {
        // A port nothing listens on, to simulate the offline `radarr_animated`
        // instance without depending on network access being blocked.
        let client = ArrClient::new("http://127.0.0.1:1", "test-api-key")
            .expect("client should construct even for an unreachable host");

        let result = client.movies().await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn malformed_json_does_not_panic() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/api/v3/movie");
            then.status(200)
                .header("content-type", "application/json")
                .body("{not valid json");
        });

        let client = client_for(&server);
        let result = client.movies().await;

        assert!(result.is_err());
    }
}
