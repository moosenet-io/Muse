//! MUSET-07: the adversarial reasoning-critique panel seam.
//!
//! [`ReasoningPanel`] is dispatched a [`crate::taste_review::trace::ReasoningTrace`]
//! plus a [`RecommendationSummary`] and asked the REASONING-CRITIQUE
//! question — [`build_critique_prompt`] is the one, shared place that
//! framing lives, so both the real dispatch and every test assert the exact
//! same wording. It explicitly does NOT ask "is this a good recommendation"
//! (that's what MUSE-11's rationale + golden-set regression already cover);
//! it asks whether the STATED REASON is a defensible driver or looks like
//! spurious correlation / single-genre overfit / a stale signal.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::time::Duration;

use crate::config::Config;
use crate::error::{MuseError, MuseResult};
use crate::taste_review::trace::ReasoningTrace;

/// The minimal recommendation-facing context the panel needs alongside the
/// trace: what was actually told to the user, so it can judge whether the
/// REASON matches the recommendation, not just the raw signal list.
#[derive(Debug, Clone, Serialize)]
pub struct RecommendationSummary {
    pub media_metadata_id: i64,
    pub title: String,
    pub rationale: String,
}

/// One panel member's critique of the reasoning (not the recommendation).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentCritique {
    /// Which panel member/model produced this critique (e.g. `"opus"`,
    /// `"diffusion-gemma"`) — informational, mirrors the dual-review
    /// discipline elsewhere in the fleet (never two of the same reviewer).
    pub agent: String,
    /// `true` when this agent judges the stated reasoning to be spurious /
    /// overfit / stale — i.e. NOT a defensible driver of the recommendation.
    pub spurious: bool,
    /// The agent's free-text critique explaining its `spurious` verdict.
    pub critique: String,
}

/// The panel's aggregate verdict on one trace's reasoning.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PanelVerdict {
    /// `true` only when every agent that voted agreed the reasoning is
    /// spurious (see [`aggregate_verdict`]).
    pub spurious: bool,
    /// `true` when every voting agent agreed with each other (all spurious,
    /// or all sound) — `false` on any split vote, which routes to
    /// escalate-to-human instead of either filing or silently passing.
    pub consensus: bool,
    pub per_agent: Vec<AgentCritique>,
}

/// Aggregate a set of per-agent critiques into a [`PanelVerdict`]. Shared by
/// both [`MockReasoningPanel`] and [`TerminusReasoningPanel`] so "what counts
/// as consensus" is defined exactly once. An empty panel (no agents
/// responded) is defined as no-consensus — never silently treated as sound,
/// since "nobody weighed in" is not the same as "everybody agreed it's fine."
pub fn aggregate_verdict(per_agent: Vec<AgentCritique>) -> PanelVerdict {
    if per_agent.is_empty() {
        return PanelVerdict {
            spurious: false,
            consensus: false,
            per_agent,
        };
    }

    let spurious_votes = per_agent.iter().filter(|a| a.spurious).count();
    let consensus = spurious_votes == 0 || spurious_votes == per_agent.len();
    let spurious = consensus && spurious_votes == per_agent.len();

    PanelVerdict {
        spurious,
        consensus,
        per_agent,
    }
}

/// Build the reasoning-CRITIQUE prompt (system, user) sent to the panel.
/// This is asserted verbatim by tests — it is the one place the
/// "is W a defensible driver, or spurious correlation / single-genre
/// overfit / stale signal?" framing is authored, so it can never drift
/// between the real dispatch and what a test believes it says.
pub fn build_critique_prompt(
    trace: &ReasoningTrace,
    rec: &RecommendationSummary,
) -> (String, String) {
    let system = "You are an adversarial reasoning reviewer for Muse's taste engine. You do NOT judge whether a \
        recommendation is good — that question is out of scope for you. Your ONLY job is to interrogate the \
        REASONING that produced it: for each signal listed, decide whether it is a defensible driver of this \
        recommendation, or whether it looks like spurious correlation, single-genre overfit, or a stale signal \
        being given too much weight. A recommendation can be a perfectly fine pick for the WRONG reason -- flag \
        that as spurious reasoning even if you'd have recommended the same title yourself."
        .to_string();

    let signals_desc = trace
        .signals
        .iter()
        .map(|s| format!("- {} (weight {:.2}): {}", s.signal, s.weight, s.description))
        .collect::<Vec<_>>()
        .join("\n");

    let user = format!(
        "Recommendation: \"{title}\" (media_metadata_id {id})\nRationale shown to the user: {rationale}\n\n\
        Reasoning trace (source tier: {source:?}, path: {path}):\n{signals_desc}\n\n\
        REASONING-CRITIQUE question: you recommended \"{title}\" because of the signal(s) listed above -- is each \
        one a defensible driver of this recommendation, or is it spurious correlation / single-genre overfit / a \
        stale signal? Do not evaluate whether the pick itself is good; evaluate whether the STATED REASON holds up.",
        title = rec.title,
        id = rec.media_metadata_id,
        rationale = rec.rationale,
        source = trace.source,
        path = trace.path,
    );

    (system, user)
}

/// The adversarial reasoning-critique panel seam. The real dispatch target
/// (a live Terminus/Chord review panel) and tests (deterministic mocks) both
/// implement this trait, so orchestration code never knows or cares which
/// one it's talking to.
#[async_trait]
pub trait ReasoningPanel: Send + Sync {
    async fn critique(
        &self,
        trace: &ReasoningTrace,
        rec: &RecommendationSummary,
    ) -> MuseResult<PanelVerdict>;
}

/// A deterministic, network-free panel for tests: returns whatever
/// [`AgentCritique`]s it was constructed with, aggregated via
/// [`aggregate_verdict`] — exactly the same aggregation rule the real panel
/// uses, so a test exercises real orchestration logic against a fake
/// dispatch, not a fake aggregation rule too.
pub struct MockReasoningPanel {
    pub verdicts: Vec<AgentCritique>,
}

impl MockReasoningPanel {
    pub fn new(verdicts: Vec<AgentCritique>) -> Self {
        Self { verdicts }
    }
}

#[async_trait]
impl ReasoningPanel for MockReasoningPanel {
    async fn critique(
        &self,
        _trace: &ReasoningTrace,
        _rec: &RecommendationSummary,
    ) -> MuseResult<PanelVerdict> {
        Ok(aggregate_verdict(self.verdicts.clone()))
    }
}

/// Response shape this client expects from a configured reasoning-panel
/// endpoint: a flat list of per-agent critiques. **Documented best-effort
/// guess** — Muse has no live Terminus panel-dispatch integration to verify
/// this shape against yet (see the `taste_review` module doc's "what's real
/// vs stubbed" section). Wire the real shape up once Muse gains that client.
#[derive(Debug, Deserialize)]
struct PanelDispatchResponse {
    #[serde(default)]
    agents: Vec<AgentCritique>,
}

/// The real, config-gated dispatch to a Terminus-fronted adversarial
/// reasoning panel. Only constructible when `Config::reasoning_panel_url`
/// is set (`from_config` returns `None` otherwise) — inert by default, same
/// posture as every other optional HTTP integration in this crate
/// (`ChordClient`, `SearxngClient`, `NewsClient`).
pub struct TerminusReasoningPanel {
    http: reqwest::Client,
    base_url: String,
    api_key: Option<String>,
    model: Option<String>,
}

const PANEL_REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

impl TerminusReasoningPanel {
    pub fn new(
        base_url: impl Into<String>,
        api_key: Option<String>,
        model: Option<String>,
    ) -> MuseResult<Self> {
        let http = reqwest::Client::builder()
            .timeout(PANEL_REQUEST_TIMEOUT)
            .build()
            .map_err(MuseError::Http)?;

        Ok(Self {
            http,
            base_url: base_url.into().trim_end_matches('/').to_string(),
            api_key,
            model,
        })
    }

    /// Build a client from `Config` (`MUSE_REASONING_PANEL_URL` +
    /// `MUSE_REASONING_PANEL_API_KEY` + `MUSE_REASONING_PANEL_MODEL`).
    /// Returns `None` when the URL is unset — the adversarial reasoning
    /// review feature simply doesn't run, same graceful-degrade posture as
    /// every other optional integration in this crate.
    pub fn from_config(config: &Config) -> Option<Self> {
        let url = config.reasoning_panel_url.clone()?;

        match Self::new(
            url,
            config.reasoning_panel_api_key.clone(),
            config.reasoning_panel_model.clone(),
        ) {
            Ok(client) => Some(client),
            Err(e) => {
                tracing::warn!(error = %e, "MUSET-07: failed to construct reasoning panel client; adversarial review will degrade");
                None
            }
        }
    }
}

#[async_trait]
impl ReasoningPanel for TerminusReasoningPanel {
    /// POST the critique prompt to the configured panel endpoint and
    /// aggregate its per-agent response. **Stub, config-gated, minimal**:
    /// the request/response shape below (`{"system", "user", "model",
    /// "agents": [...]}`) is a documented best-effort guess mirroring this
    /// crate's other JSON HTTP integrations (`ChordClient`) — it has not
    /// been verified against a live Terminus reasoning-panel endpoint,
    /// because none currently exists for Muse to call. Any transport/parse
    /// failure surfaces as a normal `MuseError::Upstream`; this method
    /// never panics on a malformed/unexpected response.
    async fn critique(
        &self,
        trace: &ReasoningTrace,
        rec: &RecommendationSummary,
    ) -> MuseResult<PanelVerdict> {
        let (system, user) = build_critique_prompt(trace, rec);
        let url = format!("{}/v1/taste_review/critique", self.base_url);

        let body = serde_json::json!({
            "system": system,
            "user": user,
            "model": self.model,
        });

        let mut req = self.http.post(&url).json(&body);
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
                message: format!("reasoning panel dispatch to {url} failed: {body}"),
            });
        }

        let parsed: PanelDispatchResponse =
            serde_json::from_slice(&bytes).map_err(|e| MuseError::Upstream {
                status: status.as_u16(),
                message: format!("failed to parse reasoning panel response from {url}: {e}"),
            })?;

        Ok(aggregate_verdict(parsed.agents))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::curation::candidates::CandidateSource;
    use crate::taste_review::trace::SignalContribution;

    fn trace() -> ReasoningTrace {
        ReasoningTrace {
            media_metadata_id: 42,
            title: "Arrival".to_string(),
            source: CandidateSource::Taste,
            score: 0.644,
            taste_fit: 0.92,
            source_weight: 0.7,
            signals: vec![SignalContribution {
                signal: "taste_profile_cosine_similarity".to_string(),
                weight: 0.92,
                description: "it's a 92% match to your overall taste profile".to_string(),
            }],
            path: "Taste tier: score = source_weight(Taste) [0.70] * taste_fit [0.92]".to_string(),
        }
    }

    fn rec() -> RecommendationSummary {
        RecommendationSummary {
            media_metadata_id: 42,
            title: "Arrival".to_string(),
            rationale:
                "\"Arrival\" is recommended because it's a 92% match to your overall taste profile."
                    .to_string(),
        }
    }

    #[test]
    fn critique_prompt_asks_the_reasoning_critique_question_not_is_this_good() {
        let (system, user) = build_critique_prompt(&trace(), &rec());

        assert!(
            system.contains("You do NOT judge whether a recommendation is good"),
            "system prompt must explicitly rule out the good-rec question: {system}"
        );
        assert!(
            system.contains("spurious correlation")
                && system.contains("single-genre overfit")
                && system.contains("stale signal"),
            "system prompt must name all three critique framings: {system}"
        );
        assert!(
            user.contains("REASONING-CRITIQUE question"),
            "user prompt must be explicitly framed as the reasoning-critique question: {user}"
        );
        assert!(
            user.contains("is each") && user.contains("defensible driver"),
            "user prompt must ask whether the signal is a defensible driver: {user}"
        );
        assert!(
            !user.to_lowercase().contains("is this a good rec"),
            "user prompt must never ask the good-rec question: {user}"
        );
        // Grounded in the real trace/rec, not invented.
        assert!(user.contains("Arrival"));
        assert!(user.contains("92% match to your overall taste profile"));
    }

    #[tokio::test]
    async fn mock_panel_reports_consensus_spurious_when_every_agent_agrees() {
        let panel = MockReasoningPanel::new(vec![
            AgentCritique {
                agent: "opus".to_string(),
                spurious: true,
                critique: "single-genre overfit".to_string(),
            },
            AgentCritique {
                agent: "diffusion-gemma".to_string(),
                spurious: true,
                critique: "stale signal".to_string(),
            },
        ]);

        let verdict = panel
            .critique(&trace(), &rec())
            .await
            .expect("mock never fails");
        assert!(verdict.consensus);
        assert!(verdict.spurious);
    }

    #[tokio::test]
    async fn mock_panel_reports_consensus_sound_when_every_agent_agrees_it_is_fine() {
        let panel = MockReasoningPanel::new(vec![
            AgentCritique {
                agent: "opus".to_string(),
                spurious: false,
                critique: "defensible".to_string(),
            },
            AgentCritique {
                agent: "diffusion-gemma".to_string(),
                spurious: false,
                critique: "defensible".to_string(),
            },
        ]);

        let verdict = panel
            .critique(&trace(), &rec())
            .await
            .expect("mock never fails");
        assert!(verdict.consensus);
        assert!(!verdict.spurious);
    }

    #[tokio::test]
    async fn mock_panel_reports_no_consensus_on_a_split_vote() {
        let panel = MockReasoningPanel::new(vec![
            AgentCritique {
                agent: "opus".to_string(),
                spurious: true,
                critique: "overfit".to_string(),
            },
            AgentCritique {
                agent: "diffusion-gemma".to_string(),
                spurious: false,
                critique: "fine".to_string(),
            },
        ]);

        let verdict = panel
            .critique(&trace(), &rec())
            .await
            .expect("mock never fails");
        assert!(!verdict.consensus);
    }

    #[test]
    fn aggregate_verdict_treats_empty_panel_as_no_consensus_not_sound() {
        let verdict = aggregate_verdict(vec![]);
        assert!(!verdict.consensus);
        assert!(!verdict.spurious);
    }

    #[test]
    fn from_config_returns_none_when_unconfigured() {
        let config = Config::default();
        assert!(TerminusReasoningPanel::from_config(&config).is_none());
    }

    #[test]
    fn from_config_builds_client_when_configured() {
        let mut config = Config::default();
        // RFC 5737 TEST-NET-1 address -- never a real fleet host.
        config.reasoning_panel_url = Some("http://192.0.2.40:8300".to_string());
        assert!(TerminusReasoningPanel::from_config(&config).is_some());
    }

    #[tokio::test]
    async fn terminus_panel_dispatches_the_critique_prompt_and_parses_agents() {
        use httpmock::prelude::*;

        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(POST).path("/v1/taste_review/critique");
            then.status(200)
                .header("content-type", "application/json")
                .body(
                    r#"{"agents": [
                        {"agent": "opus", "spurious": true, "critique": "single-genre overfit"},
                        {"agent": "diffusion-gemma", "spurious": true, "critique": "stale signal"}
                    ]}"#,
                );
        });

        let client = TerminusReasoningPanel::new(server.base_url(), None, None)
            .expect("client should construct");
        let verdict = client
            .critique(&trace(), &rec())
            .await
            .expect("dispatch should succeed");

        mock.assert();
        assert!(verdict.consensus);
        assert!(verdict.spurious);
        assert_eq!(verdict.per_agent.len(), 2);
    }

    #[tokio::test]
    async fn terminus_panel_surfaces_upstream_error_status() {
        use httpmock::prelude::*;

        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(POST).path("/v1/taste_review/critique");
            then.status(500).body("panel unavailable");
        });

        let client = TerminusReasoningPanel::new(server.base_url(), None, None)
            .expect("client should construct");
        let result = client.critique(&trace(), &rec()).await;

        assert!(result.is_err());
        match result.unwrap_err() {
            MuseError::Upstream { status, .. } => assert_eq!(status, 500),
            other => panic!("expected Upstream error, got {other:?}"),
        }
    }
}
