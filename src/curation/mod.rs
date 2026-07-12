//! MUSE-11: the curation / recommend engine v0 (spec §S96, MUSE-11).
//!
//! Local-LLM reasoning over `taste_profile` + library + availability →
//! ranked suggestions with a rationale that cites real, computed signals —
//! never an invented detail. Read-only, strictly per-account.
//!
//! - [`candidates`] — the four candidate sources (taste / on-deck / gap /
//!   availability-aware not-in-library) + de-dup.
//! - [`recommend`] — ranking, rationale (Chord-LLM with a deterministic,
//!   fact-grounded template fallback), and the `POST /recommend` /
//!   `GET /recommend/on_deck` / `GET /recommend/gaps` axum handlers.

pub mod candidates;
#[cfg(test)]
mod live_tests;
pub mod recommend;

pub use recommend::{gaps_handler, on_deck_handler, recommend_handler};
