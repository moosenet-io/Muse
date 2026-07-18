//! A minimal, read-only-in-spirit client for Chord's OpenAI-compatible
//! `/v1/chat/completions` endpoint — MUSE-10's `model_notes` summary.
//!
//! Mirrors the shape of every other optional integration in this crate
//! (`OllamaEmbedClient`, `SearxngClient`, `NewsClient`): construction via
//! [`ChordClient::from_config`] returns `None` when `CHORD_URL` isn't
//! configured, and any transport/parse failure surfaces as a normal
//! [`crate::error::MuseError`] the caller is expected to treat as
//! best-effort (see `taste_model::recompute`'s "never fail the recompute on
//! a Chord problem" handling).

use std::time::Duration;

use base64::{engine::general_purpose::STANDARD as BASE64_STANDARD, Engine as _};
use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::error::{MuseError, MuseResult};

/// Generous timeout: `model_notes` is a background-worker convenience, not
/// a user-facing request path, and <host>'s GPU may be busy with the resident
/// `lemonade-coder` production serve.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

/// Interim reasoning model default (S96 §0.4: "interim default
/// `qwen3-coder:30b` (already resident) for reasoning until a Harmony
/// curation-model sweep picks a chat/instruct-tuned model"). A model NAME,
/// not an infrastructure value — routed through Chord, which owns the
/// actual endpoint/host.
pub const DEFAULT_MODEL: &str = "qwen3-coder:30b";

#[derive(Debug, Serialize)]
struct ChatMessage<'a> {
    role: &'a str,
    content: String,
}

#[derive(Debug, Serialize)]
struct ChatCompletionRequest<'a> {
    model: &'a str,
    messages: Vec<ChatMessage<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,
    temperature: f32,
}

#[derive(Debug, Deserialize)]
struct ChatCompletionResponse {
    #[serde(default)]
    choices: Vec<ChatChoice>,
}

#[derive(Debug, Deserialize)]
struct ChatChoice {
    message: ChatChoiceMessage,
}

#[derive(Debug, Deserialize)]
struct ChatChoiceMessage {
    #[serde(default)]
    content: String,
}

/// A typed client against Chord's OpenAI-compatible chat-completions
/// surface.
#[derive(Debug, Clone)]
pub struct ChordClient {
    http: reqwest::Client,
    base_url: String,
}

impl ChordClient {
    /// Build a client against a specific Chord base URL (e.g. an httpmock
    /// server in tests, or the fleet's `CHORD_URL`).
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

    /// Build a client from `Config` (`CHORD_URL` — `Config::chord_url`).
    /// Returns `None` when unset — `model_notes` generation simply becomes
    /// unavailable, exactly like every other optional integration in this
    /// crate.
    pub fn from_config(config: &Config) -> Option<Self> {
        let url = config.chord_url.clone()?;

        match Self::new(url) {
            Ok(client) => Some(client),
            Err(e) => {
                tracing::warn!(error = %e, "failed to construct Chord client; model_notes generation will degrade");
                None
            }
        }
    }

    /// `POST /v1/chat/completions` with a system + user message, returning
    /// the first choice's message content. Bounded to a fairly short
    /// summary (`max_tokens`) — `model_notes` is a couple of sentences, not
    /// an essay.
    pub async fn chat_completion(
        &self,
        model: &str,
        system_prompt: &str,
        user_prompt: &str,
    ) -> MuseResult<String> {
        let url = format!("{}/v1/chat/completions", self.base_url);

        let request = ChatCompletionRequest {
            model,
            messages: vec![
                ChatMessage { role: "system", content: system_prompt.to_string() },
                ChatMessage { role: "user", content: user_prompt.to_string() },
            ],
            max_tokens: Some(220),
            temperature: 0.4,
        };

        let resp = self.http.post(&url).json(&request).send().await?;

        let status = resp.status();
        let bytes = resp.bytes().await?;

        if !status.is_success() {
            let body = String::from_utf8_lossy(&bytes).to_string();
            return Err(MuseError::Upstream {
                status: status.as_u16(),
                message: format!("chord chat-completions request to {url} failed: {body}"),
            });
        }

        let parsed: ChatCompletionResponse = serde_json::from_slice(&bytes).map_err(|e| MuseError::Upstream {
            status: status.as_u16(),
            message: format!("failed to parse chord chat-completions response from {url}: {e}"),
        })?;

        let content = parsed
            .choices
            .into_iter()
            .next()
            .map(|c| c.message.content)
            .unwrap_or_default();

        let trimmed = content.trim();
        if trimmed.is_empty() {
            return Err(MuseError::upstream(format!(
                "chord returned an empty chat-completion for model {model}"
            )));
        }

        Ok(trimmed.to_string())
    }

    /// `POST /v1/chat/completions` with a single image attached to the user
    /// turn (the OpenAI-compatible multimodal `content: [{"type":"text",...},
    /// {"type":"image_url",...}]` shape) — MUSEL-C2's frame-consistency
    /// question (`matching::vision::ChordVisionVerifier`). Same
    /// posture/timeout/error-mapping as [`ChordClient::chat_completion`];
    /// this is the ONE Chord HTTP transport implementation in this crate —
    /// `matching::vision` is a thin prompt/parse adapter over this method,
    /// never a second direct client against Chord's endpoint.
    pub async fn chat_completion_with_image(
        &self,
        model: &str,
        system_prompt: &str,
        user_prompt: &str,
        image_bytes: &[u8],
        image_mime: &str,
    ) -> MuseResult<String> {
        let url = format!("{}/v1/chat/completions", self.base_url);
        let encoded = BASE64_STANDARD.encode(image_bytes);
        let data_url = format!("data:{image_mime};base64,{encoded}");

        let request = serde_json::json!({
            "model": model,
            "temperature": 0.2,
            "max_tokens": 150,
            "messages": [
                {"role": "system", "content": system_prompt},
                {"role": "user", "content": [
                    {"type": "text", "text": user_prompt},
                    {"type": "image_url", "image_url": {"url": data_url}},
                ]},
            ],
        });

        let resp = self.http.post(&url).json(&request).send().await?;

        let status = resp.status();
        let bytes = resp.bytes().await?;

        if !status.is_success() {
            let body = String::from_utf8_lossy(&bytes).to_string();
            return Err(MuseError::Upstream {
                status: status.as_u16(),
                message: format!("chord vision chat-completions request to {url} failed: {body}"),
            });
        }

        let parsed: ChatCompletionResponse = serde_json::from_slice(&bytes).map_err(|e| MuseError::Upstream {
            status: status.as_u16(),
            message: format!("failed to parse chord vision chat-completions response from {url}: {e}"),
        })?;

        let content = parsed
            .choices
            .into_iter()
            .next()
            .map(|c| c.message.content)
            .unwrap_or_default();

        let trimmed = content.trim();
        if trimmed.is_empty() {
            return Err(MuseError::upstream(format!(
                "chord returned an empty vision chat-completion for model {model}"
            )));
        }

        Ok(trimmed.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use httpmock::prelude::*;

    fn client_for(server: &MockServer) -> ChordClient {
        ChordClient::new(server.base_url()).expect("client should construct")
    }

    #[tokio::test]
    async fn chat_completion_parses_first_choice_content() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(POST).path("/v1/chat/completions");
            then.status(200)
                .header("content-type", "application/json")
                .body(
                    r#"{"choices": [{"message": {"role": "assistant", "content": "  You love cerebral, slow-burn sci-fi.  "}}]}"#,
                );
        });

        let client = client_for(&server);
        let notes = client
            .chat_completion(DEFAULT_MODEL, "system prompt", "user prompt")
            .await
            .expect("chat_completion should succeed");

        mock.assert();
        assert_eq!(notes, "You love cerebral, slow-burn sci-fi.");
    }

    #[tokio::test]
    async fn chat_completion_surfaces_upstream_error_status() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(POST).path("/v1/chat/completions");
            then.status(500).body("model not loaded");
        });

        let client = client_for(&server);
        let result = client.chat_completion(DEFAULT_MODEL, "sys", "user").await;

        assert!(result.is_err());
        match result.unwrap_err() {
            MuseError::Upstream { status, .. } => assert_eq!(status, 500),
            other => panic!("expected Upstream error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn chat_completion_rejects_empty_content_without_panicking() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(POST).path("/v1/chat/completions");
            then.status(200)
                .header("content-type", "application/json")
                .body(r#"{"choices": [{"message": {"role": "assistant", "content": ""}}]}"#);
        });

        let client = client_for(&server);
        let result = client.chat_completion(DEFAULT_MODEL, "sys", "user").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn chat_completion_malformed_json_does_not_panic() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(POST).path("/v1/chat/completions");
            then.status(200)
                .header("content-type", "application/json")
                .body("{not valid json");
        });

        let client = client_for(&server);
        assert!(client.chat_completion(DEFAULT_MODEL, "sys", "user").await.is_err());
    }

    #[tokio::test]
    async fn chat_completion_with_image_sends_data_url_and_parses_response() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(POST)
                .path("/v1/chat/completions")
                .body_contains("data:image/jpeg;base64,")
                .body_contains("image_url");
            then.status(200)
                .header("content-type", "application/json")
                .body(r#"{"choices": [{"message": {"role": "assistant", "content": "CONSISTENT: yes\nCONFIDENCE: 0.9\nREASON: matches."}}]}"#);
        });

        let client = client_for(&server);
        let content = client
            .chat_completion_with_image("vision-model", "sys", "user", &[0xFF, 0xD8, 0xFF, 0x01], "image/jpeg")
            .await
            .expect("chat_completion_with_image should succeed");

        mock.assert();
        assert!(content.contains("CONSISTENT: yes"));
    }

    #[tokio::test]
    async fn chat_completion_with_image_surfaces_upstream_error_status() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(POST).path("/v1/chat/completions");
            then.status(503).body("vision model not loaded");
        });

        let client = client_for(&server);
        let result = client
            .chat_completion_with_image("vision-model", "sys", "user", &[1, 2, 3], "image/jpeg")
            .await;

        assert!(result.is_err());
        match result.unwrap_err() {
            MuseError::Upstream { status, .. } => assert_eq!(status, 503),
            other => panic!("expected Upstream error, got {other:?}"),
        }
    }

    #[test]
    fn from_config_returns_none_when_unconfigured() {
        let config = Config::default();
        assert!(ChordClient::from_config(&config).is_none());
    }

    #[test]
    fn from_config_builds_client_when_configured() {
        // RFC 5737 TEST-NET-1 address — never a real fleet host.
        let mut config = Config::default();
        config.chord_url = Some("http://192.0.2.20:8099".to_string());
        assert!(ChordClient::from_config(&config).is_some());
    }
}
