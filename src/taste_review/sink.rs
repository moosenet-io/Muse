//! MUSET-07: the taste-quality finding-filing seam.
//!
//! On a consensus-spurious [`crate::taste_review::panel::PanelVerdict`],
//! [`orchestrate::review_recommendation`](crate::taste_review::orchestrate::review_recommendation)
//! files a [`TasteQualityFinding`] via [`FindingSink`]. The real
//! implementation ([`TerminusPlaneFindingSink`]) is the ONE sanctioned Plane
//! door (S9): a Plane issue is never filed via a raw Plane API call from
//! this crate, only through a configured Terminus-fronted endpoint —
//! mirroring `enrichment::client`'s existing "Muse doesn't call Terminus MCP
//! tools in-process, it calls the configured Terminus-tool-suite-shaped HTTP
//! surface" posture. It is config-gated and inert by default.

use async_trait::async_trait;
use serde::Serialize;
use std::time::Duration;

use crate::config::Config;
use crate::error::{MuseError, MuseResult};
use crate::taste_review::panel::PanelVerdict;

/// A taste-quality finding: the panel reached consensus that the reasoning
/// behind a recommendation is spurious/overfit/stale. Distinct from "the
/// recommendation was bad" — this is specifically a *reasoning* defect.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TasteQualityFinding {
    pub media_metadata_id: i64,
    pub title: String,
    /// The reasoning trace's `path` description, so the filed finding
    /// points at exactly which rule/formula produced the spurious reasoning.
    pub trace_path: String,
    pub verdict: PanelVerdict,
    /// Human-readable summary combining every agent's critique — this is
    /// what actually gets filed as the Plane issue body/comment.
    pub summary: String,
}

/// The finding-filing seam. The real impl files a Plane issue via the
/// sanctioned Terminus path; the mock records findings in-process for tests.
#[async_trait]
pub trait FindingSink: Send + Sync {
    async fn file(&self, finding: &TasteQualityFinding) -> MuseResult<()>;
}

/// A network-free sink for tests: appends every filed finding to an
/// in-memory `Vec` a test can inspect afterward.
#[derive(Debug, Default)]
pub struct MockFindingSink {
    pub filed: std::sync::Mutex<Vec<TasteQualityFinding>>,
}

impl MockFindingSink {
    pub fn new() -> Self {
        Self::default()
    }

    /// Convenience read-back for tests: a clone of everything filed so far.
    pub fn filed_findings(&self) -> Vec<TasteQualityFinding> {
        self.filed.lock().expect("mock sink mutex poisoned").clone()
    }
}

#[async_trait]
impl FindingSink for MockFindingSink {
    async fn file(&self, finding: &TasteQualityFinding) -> MuseResult<()> {
        self.filed
            .lock()
            .expect("mock sink mutex poisoned")
            .push(finding.clone());
        Ok(())
    }
}

const SINK_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// The real, config-gated finding-filing client: POSTs to a configured
/// Terminus-fronted Plane-filing endpoint — the ONE sanctioned Plane door
/// (S9), never a raw Plane API call from this crate. Only constructible
/// when `Config::taste_finding_sink_url` is set (`from_config` returns
/// `None` otherwise); inert by default, same posture as every other
/// optional HTTP integration in this crate.
///
/// **Stub, config-gated, minimal**: the request shape below
/// (`{"project", "title", "description", "labels"}`) is a documented
/// best-effort guess -- Muse has no live Terminus/Plane client integration
/// to verify this against (see the `taste_review` module doc's "what's real
/// vs stubbed" section, and `enrichment::client`'s "Muse does not call
/// Terminus MCP tools in-process" doc). Wire the real shape up once Muse
/// gains that client; until then this only ever runs against whatever URL
/// an operator explicitly configures.
pub struct TerminusPlaneFindingSink {
    http: reqwest::Client,
    base_url: String,
    api_key: Option<String>,
    /// Plane project identifier findings are tagged with -- sourced from
    /// `Config::taste_finding_plane_project`, never a literal (S1): which
    /// Plane project owns Muse taste-quality findings is an operator/deploy
    /// decision.
    project: Option<String>,
}

impl TerminusPlaneFindingSink {
    pub fn new(
        base_url: impl Into<String>,
        api_key: Option<String>,
        project: Option<String>,
    ) -> MuseResult<Self> {
        let http = reqwest::Client::builder()
            .timeout(SINK_REQUEST_TIMEOUT)
            .build()
            .map_err(MuseError::Http)?;

        Ok(Self {
            http,
            base_url: base_url.into().trim_end_matches('/').to_string(),
            api_key,
            project,
        })
    }

    /// Build a client from `Config` (`MUSE_TASTE_FINDING_SINK_URL` +
    /// `MUSE_TASTE_FINDING_SINK_API_KEY` + `MUSE_TASTE_FINDING_PLANE_PROJECT`).
    /// Returns `None` when the URL is unset -- finding-filing simply becomes
    /// unavailable (the orchestration layer still runs; it just can't file),
    /// same graceful-degrade posture as every other optional integration.
    pub fn from_config(config: &Config) -> Option<Self> {
        let url = config.taste_finding_sink_url.clone()?;

        match Self::new(
            url,
            config.taste_finding_sink_api_key.clone(),
            config.taste_finding_plane_project.clone(),
        ) {
            Ok(client) => Some(client),
            Err(e) => {
                tracing::warn!(error = %e, "MUSET-07: failed to construct taste-finding sink client; consensus-spurious findings will not be filed");
                None
            }
        }
    }
}

#[async_trait]
impl FindingSink for TerminusPlaneFindingSink {
    async fn file(&self, finding: &TasteQualityFinding) -> MuseResult<()> {
        let url = format!("{}/v1/plane/taste_quality_finding", self.base_url);

        let body = serde_json::json!({
            "project": self.project,
            "title": format!("Taste-quality finding: spurious reasoning for \"{}\"", finding.title),
            "description": finding.summary,
            "labels": ["taste-quality", "adversarial-reasoning-review"],
            "media_metadata_id": finding.media_metadata_id,
            "trace_path": finding.trace_path,
        });

        let mut req = self.http.post(&url).json(&body);
        if let Some(key) = &self.api_key {
            req = req.bearer_auth(key);
        }

        let resp = req.send().await?;
        let status = resp.status();

        if !status.is_success() {
            let bytes = resp.bytes().await.unwrap_or_default();
            let text = String::from_utf8_lossy(&bytes).to_string();
            return Err(MuseError::Upstream {
                status: status.as_u16(),
                message: format!("taste-quality finding filing to {url} failed: {text}"),
            });
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::taste_review::panel::AgentCritique;

    fn verdict() -> PanelVerdict {
        PanelVerdict {
            spurious: true,
            consensus: true,
            per_agent: vec![AgentCritique {
                agent: "opus".to_string(),
                spurious: true,
                critique: "single-genre overfit".to_string(),
            }],
        }
    }

    fn finding() -> TasteQualityFinding {
        TasteQualityFinding {
            media_metadata_id: 42,
            title: "Arrival".to_string(),
            trace_path: "Taste tier: score = source_weight(Taste) * taste_fit".to_string(),
            verdict: verdict(),
            summary: "Adversarial panel reached consensus that the reasoning is spurious."
                .to_string(),
        }
    }

    #[tokio::test]
    async fn mock_sink_records_filed_findings() {
        let sink = MockFindingSink::new();
        sink.file(&finding()).await.expect("mock never fails");

        let filed = sink.filed_findings();
        assert_eq!(filed.len(), 1);
        assert_eq!(filed[0].media_metadata_id, 42);
        assert_eq!(filed[0].title, "Arrival");
    }

    #[test]
    fn from_config_returns_none_when_unconfigured() {
        let config = Config::default();
        assert!(TerminusPlaneFindingSink::from_config(&config).is_none());
    }

    #[test]
    fn from_config_builds_client_when_configured() {
        let mut config = Config::default();
        // RFC 5737 TEST-NET-1 address -- never a real fleet host.
        config.taste_finding_sink_url = Some("http://192.0.2.50:8310".to_string());
        assert!(TerminusPlaneFindingSink::from_config(&config).is_some());
    }

    #[tokio::test]
    async fn terminus_sink_files_the_finding_via_the_configured_endpoint() {
        use httpmock::prelude::*;

        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(POST).path("/v1/plane/taste_quality_finding");
            then.status(200)
                .header("content-type", "application/json")
                .body(r#"{"issue_id": "TEST-1"}"#);
        });

        let client =
            TerminusPlaneFindingSink::new(server.base_url(), None, Some("TESTPROJ".to_string()))
                .expect("client should construct");
        client
            .file(&finding())
            .await
            .expect("filing should succeed");

        mock.assert();
    }

    #[tokio::test]
    async fn terminus_sink_surfaces_upstream_error_status() {
        use httpmock::prelude::*;

        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(POST).path("/v1/plane/taste_quality_finding");
            then.status(503).body("plane unreachable");
        });

        let client = TerminusPlaneFindingSink::new(server.base_url(), None, None)
            .expect("client should construct");
        let result = client.file(&finding()).await;

        assert!(result.is_err());
        match result.unwrap_err() {
            MuseError::Upstream { status, .. } => assert_eq!(status, 503),
            other => panic!("expected Upstream error, got {other:?}"),
        }
    }
}
