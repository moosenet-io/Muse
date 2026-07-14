//! MUSEX-13: the [`DiscordClient`] seam — mirrors
//! `crate::cultural::source::TrendSource` / `crate::watch_together::sync::ServerSyncPrimitive`:
//! a trait, a config-gated real implementation that is inert when
//! unconfigured, and a deterministic mock for tests. Muse has no Discord
//! integration prior to this item, so [`RealDiscordClient`] is a minimal,
//! **documented best-effort client** against Discord's REST API — like
//! `crate::cultural::source::TraktTrendSource`, it has never been exercised
//! against a live endpoint and should be re-verified before relying on it
//! in production. No test in this crate makes a live call (per the
//! anti-hang contract): every test exercises [`MockDiscordClient`].

use async_trait::async_trait;

use crate::config::Config;
use crate::error::{MuseError, MuseResult};

/// A server-agnostic rich embed — title, poster art URL, synopsis — that a
/// [`DiscordClient`] impl renders into whatever wire format its target
/// actually needs (Discord's embed JSON, for [`RealDiscordClient`]). Kept
/// deliberately free of any Discord-specific type so [`crate::discord::bot`]
/// never has to know about Discord's embed schema, only this struct.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RichEmbed {
    pub title: String,
    /// Same-origin artwork URL (`crate::web::artwork::art_handler`'s
    /// `/art/{kind}/{id}` proxy) when a public base URL is configured —
    /// never a raw upstream (Plex/TMDb) URL, and never carries a
    /// credential (see that handler's own doc: the Plex token stays
    /// server-side). `None` when no public base URL is configured, same
    /// graceful-degrade posture as every other optional field in this
    /// crate.
    pub poster_url: Option<String>,
    /// Grounded synopsis text — see `crate::discord::bot::build_rich_embed`
    /// for where this comes from (real `Candidate::facts`, never invented).
    pub synopsis: String,
}

/// The Discord API seam. `post_embed`/`reply` are the two shapes
/// `crate::discord::bot` needs: a rich-embed post (a recommendation) and a
/// plain reply (a generic acknowledgement, or the assistant's own
/// conversational voice via `crate::assistant::build_question_message`-style
/// phrasing — this trait doesn't care which).
#[async_trait]
pub trait DiscordClient: Send + Sync {
    async fn post_embed(&self, channel_id: &str, embed: RichEmbed) -> MuseResult<()>;
    async fn reply(&self, channel_id: &str, content: &str) -> MuseResult<()>;
}

// --- real (config-gated, best-effort, never called by this crate's own tests) ---

/// Discord's REST API base — public API surface, not a credential, same
/// posture as `crate::trending::client::TmdbClient`'s
/// `DEFAULT_BASE_URL`/`crate::cultural::source::TRAKT_DEFAULT_BASE_URL`
/// literals.
const DISCORD_API_BASE_URL: &str = "https://discord.com/api/v10";
const REQUEST_TIMEOUT_SECS: u64 = 10;

/// A minimal Discord bot REST client. **Documented best-effort, not
/// verified against a live Discord endpoint** (same caveat as
/// `crate::cultural::source::TraktTrendSource`) — Muse had no Discord
/// integration before this item. Inert (no live call, no startup impact)
/// unless `DISCORD_BOT_TOKEN` is configured.
pub struct RealDiscordClient {
    http: reqwest::Client,
    base_url: String,
    bot_token: String,
}

impl RealDiscordClient {
    pub fn new(bot_token: impl Into<String>) -> MuseResult<Self> {
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(REQUEST_TIMEOUT_SECS))
            .build()
            .map_err(MuseError::Http)?;

        Ok(Self {
            http,
            base_url: DISCORD_API_BASE_URL.to_string(),
            bot_token: bot_token.into(),
        })
    }

    /// Build from `Config` (`DISCORD_BOT_TOKEN`, via
    /// `vault::manager()`/<secret-manager>-materialized env at runtime — never a
    /// literal, S1/S7). Returns `None` when unset — the Discord surface
    /// simply doesn't run, same graceful-degrade posture as
    /// `TmdbClient::from_config`/`TraktTrendSource::from_config`.
    pub fn from_config(config: &Config) -> Option<Self> {
        let token = config.discord_bot_token.clone()?;
        match Self::new(token) {
            Ok(client) => Some(client),
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "MUSEX-13: failed to construct Discord client; the Discord bot surface will degrade"
                );
                None
            }
        }
    }

    fn discord_embed_json(embed: &RichEmbed) -> serde_json::Value {
        let mut json = serde_json::json!({
            "title": embed.title,
            "description": embed.synopsis,
        });
        if let Some(poster_url) = &embed.poster_url {
            json["image"] = serde_json::json!({ "url": poster_url });
        }
        json
    }
}

#[async_trait]
impl DiscordClient for RealDiscordClient {
    async fn post_embed(&self, channel_id: &str, embed: RichEmbed) -> MuseResult<()> {
        let url = format!("{}/channels/{}/messages", self.base_url, channel_id);
        let body = serde_json::json!({ "embeds": [Self::discord_embed_json(&embed)] });

        let resp = self
            .http
            .post(&url)
            .header("Authorization", format!("Bot {}", self.bot_token))
            .json(&body)
            .send()
            .await?;

        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(MuseError::Upstream {
                status: status.as_u16(),
                message: format!("discord post_embed to channel {channel_id} failed: {text}"),
            });
        }
        Ok(())
    }

    async fn reply(&self, channel_id: &str, content: &str) -> MuseResult<()> {
        let url = format!("{}/channels/{}/messages", self.base_url, channel_id);
        let body = serde_json::json!({ "content": content });

        let resp = self
            .http
            .post(&url)
            .header("Authorization", format!("Bot {}", self.bot_token))
            .json(&body)
            .send()
            .await?;

        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(MuseError::Upstream {
                status: status.as_u16(),
                message: format!("discord reply to channel {channel_id} failed: {text}"),
            });
        }
        Ok(())
    }
}

// --- mock (tests only) -------------------------------------------------

/// A deterministic, network-free [`DiscordClient`] for tests. Records every
/// call it receives — the seam the privacy negative test inspects to prove
/// nothing taste/watch-data-shaped ever reached "Discord."
#[derive(Debug, Default)]
pub struct MockDiscordClient {
    pub embed_calls: std::sync::Mutex<Vec<(String, RichEmbed)>>,
    pub reply_calls: std::sync::Mutex<Vec<(String, String)>>,
}

impl MockDiscordClient {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn embed_call_count(&self) -> usize {
        self.embed_calls.lock().unwrap().len()
    }

    pub fn reply_call_count(&self) -> usize {
        self.reply_calls.lock().unwrap().len()
    }
}

#[async_trait]
impl DiscordClient for MockDiscordClient {
    async fn post_embed(&self, channel_id: &str, embed: RichEmbed) -> MuseResult<()> {
        self.embed_calls
            .lock()
            .unwrap()
            .push((channel_id.to_string(), embed));
        Ok(())
    }

    async fn reply(&self, channel_id: &str, content: &str) -> MuseResult<()> {
        self.reply_calls
            .lock()
            .unwrap()
            .push((channel_id.to_string(), content.to_string()));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn embed() -> RichEmbed {
        RichEmbed {
            title: "Severance".to_string(),
            poster_url: Some("http://example.invalid/art/show/1".to_string()),
            synopsis: "you're 40% through it".to_string(),
        }
    }

    #[test]
    fn real_discord_client_from_config_returns_none_when_unconfigured() {
        let config = Config::default();
        assert!(RealDiscordClient::from_config(&config).is_none());
    }

    #[test]
    fn real_discord_client_from_config_builds_when_configured() {
        let config = Config {
            discord_bot_token: Some("test-bot-token".to_string()),
            ..Default::default()
        };
        assert!(RealDiscordClient::from_config(&config).is_some());
    }

    #[test]
    fn real_discord_client_never_logs_or_echoes_the_bot_token() {
        // SecretString/Display discipline (S1/S7 §"Logging secrets"):
        // RealDiscordClient's own Debug isn't derived (no Debug impl at
        // all), so there is no accidental `{:?}` leak path. This test
        // documents that constraint — if a future edit adds
        // `#[derive(Debug)]` to `RealDiscordClient`, it must not derive it
        // over `bot_token` without redaction.
        let config = Config {
            discord_bot_token: Some("<REDACTED-SECRET>".to_string()),
            ..Default::default()
        };
        let client = RealDiscordClient::from_config(&config).expect("configured");
        // No Debug/Display impl exists to format `client` through — this
        // line intentionally does not attempt `format!("{:?}", client)`,
        // since that would fail to compile, which IS the guarantee.
        let _ = client;
    }

    #[tokio::test]
    async fn mock_discord_client_records_embed_calls() {
        let mock = MockDiscordClient::new();
        mock.post_embed("channel-1", embed()).await.unwrap();
        assert_eq!(mock.embed_call_count(), 1);
        let calls = mock.embed_calls.lock().unwrap();
        assert_eq!(calls[0].0, "channel-1");
        assert_eq!(calls[0].1, embed());
    }

    #[tokio::test]
    async fn mock_discord_client_records_reply_calls() {
        let mock = MockDiscordClient::new();
        mock.reply("channel-1", "hey there").await.unwrap();
        assert_eq!(mock.reply_call_count(), 1);
        let calls = mock.reply_calls.lock().unwrap();
        assert_eq!(calls[0], ("channel-1".to_string(), "hey there".to_string()));
    }

    #[tokio::test]
    async fn mock_discord_client_starts_with_no_calls() {
        let mock = MockDiscordClient::new();
        assert_eq!(mock.embed_call_count(), 0);
        assert_eq!(mock.reply_call_count(), 0);
    }
}
