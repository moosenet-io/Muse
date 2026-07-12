//! Read-only-in-spirit (no state on the Ollama side to mutate) HTTP client
//! for Ollama's embeddings endpoint.
//!
//! Construction is via [`OllamaEmbedClient::from_config`], which returns
//! `None` when `MUSE_OLLAMA_URL` isn't configured — callers treat local
//! embeddings as an optional, gracefully-degrading dependency exactly like
//! `PlexClient`/`ProwlarrClient`/`TmdbClient`.

use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::error::{MuseError, MuseResult};

/// Generous but bounded — nomic-embed-text is small, but <host>'s GPU is
/// shared with a permanent `lemonade-coder` (qwen3-coder:30b) production
/// serve that holds the card, so a request can occasionally queue behind
/// other work rather than failing outright.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Serialize)]
struct EmbeddingsRequest<'a> {
    model: &'a str,
    prompt: &'a str,
}

#[derive(Debug, Deserialize)]
struct EmbeddingsResponse {
    #[serde(default)]
    embedding: Vec<f32>,
}

/// A typed client for Ollama's `POST /api/embeddings` endpoint.
#[derive(Debug, Clone)]
pub struct OllamaEmbedClient {
    http: reqwest::Client,
    base_url: String,
}

impl OllamaEmbedClient {
    /// Build a client against a specific Ollama base URL (e.g.
    /// `http://192.168.0.x:11434`).
    pub fn new(base_url: impl Into<String>) -> MuseResult<Self> {
        let http = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .map_err(MuseError::Http)?;

        Ok(Self {
            http,
            base_url: base_url.into().trim_end_matches('/').to_string(),
        })
    }

    /// Build a client from `Config` (`MUSE_OLLAMA_URL`). Returns `None` when
    /// unset or when the client fails to construct — the embedding pipeline
    /// degrades to "nothing to do" rather than blocking startup or a caller.
    /// Never panics.
    pub fn from_config(config: &Config) -> Option<Self> {
        let url = config.ollama_url.clone()?;

        match Self::new(url) {
            Ok(client) => Some(client),
            Err(e) => {
                tracing::warn!(error = %e, "failed to construct Ollama embed client; local embeddings will degrade");
                None
            }
        }
    }

    /// Embed a single piece of text with the given model, returning the raw
    /// vector as reported by Ollama. Callers are responsible for validating
    /// the dimensionality matches what they intend to store (the `embeddings`
    /// table pins `vector(768)` for `nomic-embed-text`, per S96 §0.7).
    pub async fn embed(&self, model: &str, text: &str) -> MuseResult<Vec<f32>> {
        let url = format!("{}/api/embeddings", self.base_url);

        let resp = self
            .http
            .post(&url)
            .json(&EmbeddingsRequest { model, prompt: text })
            .send()
            .await?;

        let status = resp.status();
        let bytes = resp.bytes().await?;

        if !status.is_success() {
            let body = String::from_utf8_lossy(&bytes).to_string();
            return Err(MuseError::Upstream {
                status: status.as_u16(),
                message: format!("ollama embeddings request to {url} failed: {body}"),
            });
        }

        let parsed: EmbeddingsResponse = serde_json::from_slice(&bytes).map_err(|e| MuseError::Upstream {
            status: status.as_u16(),
            message: format!("failed to parse ollama embeddings response from {url}: {e}"),
        })?;

        if parsed.embedding.is_empty() {
            return Err(MuseError::upstream(format!(
                "ollama returned an empty embedding vector for model {model} (is it pulled on the target host?)"
            )));
        }

        Ok(parsed.embedding)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use httpmock::prelude::*;

    fn client_for(server: &MockServer) -> OllamaEmbedClient {
        OllamaEmbedClient::new(server.base_url()).expect("client should construct")
    }

    #[tokio::test]
    async fn embed_parses_vector_from_response() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(POST)
                .path("/api/embeddings")
                .json_body(serde_json::json!({"model": "nomic-embed-text", "prompt": "Arrival (2016)"}));
            then.status(200)
                .header("content-type", "application/json")
                .body(r#"{"embedding": [0.1, 0.2, 0.3]}"#);
        });

        let client = client_for(&server);
        let vector = client
            .embed("nomic-embed-text", "Arrival (2016)")
            .await
            .expect("embed should succeed");

        mock.assert();
        assert_eq!(vector, vec![0.1, 0.2, 0.3]);
    }

    #[tokio::test]
    async fn embed_surfaces_upstream_error_status() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(POST).path("/api/embeddings");
            then.status(500).body("model not found");
        });

        let client = client_for(&server);
        let result = client.embed("nomic-embed-text", "anything").await;

        assert!(result.is_err());
        match result.unwrap_err() {
            MuseError::Upstream { status, .. } => assert_eq!(status, 500),
            other => panic!("expected Upstream error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn embed_rejects_empty_vector_without_panicking() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(POST).path("/api/embeddings");
            then.status(200)
                .header("content-type", "application/json")
                .body(r#"{"embedding": []}"#);
        });

        let client = client_for(&server);
        let result = client.embed("nomic-embed-text", "anything").await;

        assert!(result.is_err());
    }

    #[tokio::test]
    async fn embed_malformed_json_does_not_panic() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(POST).path("/api/embeddings");
            then.status(200)
                .header("content-type", "application/json")
                .body("{not valid json");
        });

        let client = client_for(&server);
        let result = client.embed("nomic-embed-text", "anything").await;

        assert!(result.is_err());
    }

    #[test]
    fn from_config_returns_none_when_unconfigured() {
        let config = test_config(None);
        assert!(OllamaEmbedClient::from_config(&config).is_none());
    }

    #[test]
    fn from_config_builds_client_when_configured() {
        // RFC 5737 TEST-NET-1 address — never a real fleet host.
        let config = test_config(Some("http://192.0.2.10:11434".to_string()));
        assert!(OllamaEmbedClient::from_config(&config).is_some());
    }

    fn test_config(ollama_url: Option<String>) -> Config {
        Config {
            database_url: None,
            bind_addr: "0.0.0.0:8090".to_string(),
            log_level: "info".to_string(),
            plex_url: None,
            plex_token: None,
            tautulli_url: None,
            tautulli_api_key: None,
            radarr_url: None,
            radarr_api_key: None,
            sonarr_url: None,
            sonarr_api_key: None,
            prowlarr_url: None,
            prowlarr_api_key: None,
            tmdb_api_key: None,
            ollama_url,
            chord_url: None,
            arr_instances_json: None,
        }
    }
}
