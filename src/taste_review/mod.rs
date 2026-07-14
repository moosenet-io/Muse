//! MUSET-07 (Plane TERM #372): adversarial reasoning review — the layer
//! that catches "right answer, wrong reason."
//!
//! ## Why this exists
//! MUSE-11's rationale (`curation::recommend::build_rationale`) is grounded
//! in real facts and is a fine *user-facing* explanation, but nothing in
//! the pipeline ever asks "is the REASON this was recommended actually a
//! defensible driver, or did the ranking latch onto a spurious correlation
//! / single-genre overfit / a stale signal?" A recommendation can be a
//! perfectly fine pick for the wrong reason (e.g. a single old horror-movie
//! rating dominating the affinity math because nothing since has diluted
//! it) and today nothing catches that. This module is that catch.
//!
//! ## Pipeline
//! 1. [`trace::build_reasoning_trace`] turns a real, already-computed
//!    [`crate::curation::candidates::Candidate`] + its
//!    [`crate::curation::recommend::score_candidate`] score into an
//!    INTERROGABLE [`trace::ReasoningTrace`] — every signal in it traces to
//!    a real fact/weight the recommend pipeline already computed, nothing
//!    invented (same grounding discipline as `candidates::Candidate::facts`
//!    and `recommend::template_rationale`).
//! 2. [`panel::ReasoningPanel`] dispatches that trace (+ the recommendation)
//!    to an adversarial panel, prompted with the REASONING-CRITIQUE
//!    question via [`panel::build_critique_prompt`] — never "is this a good
//!    rec," always "is the stated reason defensible."
//! 3. [`orchestrate::review_recommendation`] interprets the panel's
//!    [`panel::PanelVerdict`]: consensus-spurious files a
//!    [`sink::TasteQualityFinding`] via [`sink::FindingSink`] (the real impl
//!    is config-gated behind the ONE sanctioned Terminus Plane door, S9);
//!    no-consensus escalates to a human instead of silently dropping or
//!    guessing; consensus-sound produces no finding.
//!
//! ## What's real vs. stubbed
//! [`panel::MockReasoningPanel`] and [`sink::MockFindingSink`] are fully
//! real, in-process, network-free — they're what the tests in this module
//! (and any future caller) exercise. [`panel::TerminusReasoningPanel`] and
//! [`sink::TerminusPlaneFindingSink`] are real HTTP clients (same shape as
//! `taste_model::chord_client::ChordClient` /
//! `enrichment::client::SearxngClient`), but Muse currently has **no live
//! Terminus client integration** anywhere in this crate (see
//! `enrichment::client`'s own module doc: "Muse is a standalone service —
//! it does not call Terminus MCP tools in-process"), so their exact request
//! shape against a live Terminus reasoning-panel / Plane-filing HTTP
//! surface is a documented best-effort guess, not verified against a real
//! endpoint. Both are config-gated (`Config::reasoning_panel_url` /
//! `Config::taste_finding_sink_url`, both `None` by default) so they are
//! entirely inert — never constructed, never called — on any deployment
//! that hasn't explicitly set them. Wire the real request/response shape up
//! when Muse gains an actual Terminus client.
//!
//! ## "Why this" narration ([`because`], MUSEX-04)
//! [`because::because_line`] is a separate, user-facing consumer of
//! [`trace::ReasoningTrace`]: a short "because…" line, in Lumina's warm
//! concise voice, naming the real top signal(s) behind a rec — see
//! `because`'s module doc for the exact grounding contract (verbatim reuse
//! of `SignalContribution::description`, no LLM rephrase, no invented
//! words). It is the trust-facing sibling of this module's adversarial
//! reasoning-critique pipeline above: the panel asks "is the reason
//! defensible," `because_line` tells the human what the reason IS.

pub mod because;
pub mod orchestrate;
pub mod panel;
pub mod sink;
pub mod trace;
