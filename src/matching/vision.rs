//! MUSEL-C2: a thin adapter over [`ChordClient`] for the frame-consistency
//! question -- "is this frame consistent with `<title>` (era/genre/
//! setting; not a slate/test-pattern/black frame)?" -- the strongest of
//! the three signals [`crate::matching::verify::verify_match`] combines.
//!
//! Mirrors [`ChordClient::from_config`]'s posture exactly:
//! [`ChordVisionVerifier::from_config`] returns `None` when Chord (or a
//! vision-capable model) isn't configured, and the caller simply skips
//! this signal -- it is never a hard dependency and never fails the
//! pipeline. All HTTP transport against Chord goes through
//! [`ChordClient::chat_completion_with_image`]; this module only builds
//! the prompt and parses the response -- it is not a second direct client
//! against a model URL (the spec's "no direct model URL" requirement).

use async_trait::async_trait;

use crate::config::Config;
use crate::error::MuseResult;
use crate::matching::stills::Still;
use crate::metadata::MediaKind;
use crate::taste_model::chord_client::ChordClient;

/// A vision-capable Chord model to ask the frame-consistency question.
/// A model NAME only -- Chord owns the actual endpoint/host, same posture
/// as `chord_client::DEFAULT_MODEL`. Overridable via `MUSE_VISION_MODEL`
/// so an operator can point this at whichever vision-capable model is
/// resident without a code change (this crate has no other env-var-direct
/// reads outside `config.rs` -- see that module's doc comment -- so this
/// override is read once, here, at construction time only, exactly like
/// how `Config` itself is built).
pub const DEFAULT_VISION_MODEL: &str = "qwen2.5-vl:7b";

/// One VLM answer to the frame-consistency question.
#[derive(Debug, Clone, PartialEq)]
pub struct VisionAnswer {
    pub consistent: bool,
    pub confidence: f32,
    pub explanation: String,
}

/// The seam [`verify_match`](crate::matching::verify::verify_match)
/// depends on -- a trait, not the concrete [`ChordVisionVerifier`], so
/// tests can supply a deterministic mock instead of a live Chord endpoint
/// (mirrors [`crate::metadata::MockMetadataProvider`]'s relationship to
/// [`crate::metadata::MetadataProvider`]).
#[async_trait]
pub trait VisionVerifier: Send + Sync {
    /// `Ok(None)` means the signal is unavailable for this call (e.g. the
    /// model replied but not in the expected format) -- a graceful skip,
    /// not an error the caller should surface. `Err` is a transport/
    /// upstream failure; `verify_match` treats both the same way (skip
    /// this signal, log the reason, fall back to the other signals) since
    /// a VLM problem must never fail the whole match check.
    async fn is_consistent(
        &self,
        still: &Still,
        title: &str,
        kind: MediaKind,
        year: Option<i32>,
    ) -> MuseResult<Option<VisionAnswer>>;
}

/// The real, Chord-backed [`VisionVerifier`].
pub struct ChordVisionVerifier {
    chord: ChordClient,
    model: String,
}

impl ChordVisionVerifier {
    /// Builds from [`Config`]; `None` when Chord isn't configured
    /// (`CHORD_URL` unset) -- same graceful-degrade posture as
    /// [`ChordClient::from_config`] itself. The model defaults to
    /// [`DEFAULT_VISION_MODEL`], overridable via `MUSE_VISION_MODEL`.
    pub fn from_config(config: &Config) -> Option<Self> {
        let chord = ChordClient::from_config(config)?;
        let model =
            std::env::var("MUSE_VISION_MODEL").unwrap_or_else(|_| DEFAULT_VISION_MODEL.to_string());
        Some(Self { chord, model })
    }
}

#[async_trait]
impl VisionVerifier for ChordVisionVerifier {
    async fn is_consistent(
        &self,
        still: &Still,
        title: &str,
        kind: MediaKind,
        year: Option<i32>,
    ) -> MuseResult<Option<VisionAnswer>> {
        let kind_label = match kind {
            MediaKind::Movie => "movie",
            MediaKind::Series => "TV series",
        };
        let year_label = year.map(|y| format!(" ({y})")).unwrap_or_default();

        let system_prompt = "You are verifying whether a single video frame really belongs to a \
            specific title. Answer strictly in this exact three-line format, nothing else:\n\
            CONSISTENT: yes|no\n\
            CONFIDENCE: <a number between 0.0 and 1.0>\n\
            REASON: <one short sentence>\n\
            Answer \"no\" if the frame is a black/blank frame, a test pattern, a slate, static/\
            noise, or is clearly the wrong era, genre, or setting for the title.";

        let user_prompt = format!(
            "Title: {title}{year_label} ({kind_label}). Is this frame consistent with that title?"
        );

        let content = self
            .chord
            .chat_completion_with_image(&self.model, system_prompt, &user_prompt, &still.bytes, "image/jpeg")
            .await?;

        Ok(parse_vision_answer(&content))
    }
}

/// Parse the model's `CONSISTENT:`/`CONFIDENCE:`/`REASON:` reply.
/// `None` when the reply doesn't contain a parseable `CONSISTENT:` line --
/// a graceful "the model didn't answer in the expected shape" rather than
/// a panic or a fabricated guess either way.
fn parse_vision_answer(content: &str) -> Option<VisionAnswer> {
    let mut consistent = None;
    let mut confidence = None;
    let mut explanation = String::new();

    for line in content.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("CONSISTENT:") {
            consistent = Some(rest.trim().to_ascii_lowercase().starts_with("yes"));
        } else if let Some(rest) = line.strip_prefix("CONFIDENCE:") {
            confidence = rest.trim().parse::<f32>().ok();
        } else if let Some(rest) = line.strip_prefix("REASON:") {
            explanation = rest.trim().to_string();
        }
    }

    let consistent = consistent?;
    let confidence = confidence.unwrap_or(0.5).clamp(0.0, 1.0);
    Some(VisionAnswer { consistent, confidence, explanation })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_vision_answer_parses_well_formed_yes() {
        let content = "CONSISTENT: yes\nCONFIDENCE: 0.9\nREASON: matches the setting and era.";
        let answer = parse_vision_answer(content).expect("should parse");
        assert!(answer.consistent);
        assert!((answer.confidence - 0.9).abs() < 1e-6);
        assert_eq!(answer.explanation, "matches the setting and era.");
    }

    #[test]
    fn parse_vision_answer_parses_well_formed_no() {
        let content = "CONSISTENT: no\nCONFIDENCE: 0.8\nREASON: black frame, no content.";
        let answer = parse_vision_answer(content).expect("should parse");
        assert!(!answer.consistent);
        assert!((answer.confidence - 0.8).abs() < 1e-6);
    }

    #[test]
    fn parse_vision_answer_tolerates_extra_whitespace_and_case() {
        let content = "consistent: YES \n confidence: 0.42\nreason: looks right";
        // Lowercased prefixes aren't matched by `strip_prefix` (case
        // sensitive) -- this documents that the parser expects the
        // uppercase field names the system prompt asks for, and degrades
        // to `None` rather than guessing when a model doesn't comply.
        assert!(parse_vision_answer(content).is_none());
    }

    #[test]
    fn parse_vision_answer_missing_confidence_defaults_to_midpoint() {
        let content = "CONSISTENT: yes\nREASON: no confidence line given.";
        let answer = parse_vision_answer(content).expect("should parse");
        assert!((answer.confidence - 0.5).abs() < 1e-6);
    }

    #[test]
    fn parse_vision_answer_returns_none_when_unparseable() {
        assert!(parse_vision_answer("I am not sure what to say").is_none());
    }

    #[test]
    fn parse_vision_answer_clamps_out_of_range_confidence() {
        let content = "CONSISTENT: yes\nCONFIDENCE: 5.0\nREASON: overconfident model.";
        let answer = parse_vision_answer(content).expect("should parse");
        assert!((answer.confidence - 1.0).abs() < 1e-6);
    }
}
