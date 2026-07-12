//! MUSE-24 — the channel composer (the agentic director) + named presets.
//!
//! Given a `channels` row (MUSE-23) and a set of shows, this module composes
//! an ordered lineup: round-robin each show's next-unwatched (or
//! taste-ranked-priority) episode, interleaved with cadence-matched
//! interstitials picked from the `interstitials` pool, bounded by a target
//! session length — and persists it as a fresh `channel_runs` row plus its
//! ordered `channel_programs` rows forming a contiguous timeline
//! (`end_at[i] == start_at[i+1]`).
//!
//! The core algorithm is fully **deterministic** and works with **no LLM at
//! all** (`ComposeOptions::use_llm = false`, or an unconfigured/unreachable
//! Chord). An optional local-LLM pass — via Chord's `/v1/chat/completions`
//! endpoint — may re-order the shows' round-robin priority and produce a
//! human `rationale`; on ANY failure (Chord unconfigured, network error,
//! non-success status, malformed/invalid response) it falls back to the
//! deterministic show order plus a templated rationale. Composition never
//! fails because the LLM is unavailable.
//!
//! Re-composing a channel (`compose_channel_run` again, or the
//! `regenerate_channel_run` / `adjust_channel_run` helpers) always inserts a
//! **new** `channel_runs` row — it never mutates a prior run, so history is
//! preserved.
//!
//! Submodules:
//! - [`compose`] — the composer itself: `compose_channel_run`,
//!   `regenerate_channel_run`, `adjust_channel_run`, `ComposeOptions`,
//!   `EpisodeOrdering`.
//! - [`presets`] — the named presets (Saturday Morning / Prestige Drama
//!   Night / 90s Chaos / Comfort Rewatch / Discover / Household Movie
//!   Night) as data, each resolving to a [`compose::ComposeOptions`] overlay
//!   via [`presets::Preset::apply`].

pub mod compose;
pub mod presets;
pub mod routes;

pub use compose::{
    adjust_channel_run, compose_channel_run, regenerate_channel_run, ComposeOptions,
    EpisodeOrdering,
};
pub use routes::compose_handler;
pub use presets::{list_presets, resolve_preset, Preset, PresetName};
