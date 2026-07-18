//! Matching-verification support (MUSEL-C1/C2): proving an identified media
//! file really is the title Muse thinks it is, rather than trusting a
//! provider-ID match blindly.
//!
//! - [`stills`] — MUSEL-C1: the ffmpeg sample-still extraction primitive.
//!   Pure timestamp-spread math + one impure per-timestamp ffmpeg spawn,
//!   mirroring the split in [`crate::streaming`] (`ffmpeg` module = pure
//!   arg-building, `mod.rs` = the one impure spawn/read layer).
//! - [`liveness`] — MUSEL-C2: cheap byte-level heuristics on the stills'
//!   raw JPEG bytes, to catch dead/blank/stuck content.
//! - [`vision`] — MUSEL-C2: the optional VLM-via-Chord signal, the
//!   strongest discriminator when configured.
//! - [`verify`] — MUSEL-C2: `verify_match`, which combines liveness +
//!   vision + metadata consistency into a single [`verify::MatchVerdict`].
//!   Verdict-only — see that module's doc comment.

pub mod liveness;
pub mod stills;
pub mod verify;
pub mod vision;

pub use stills::{extract_sample_stills, Still};
pub use verify::{verify_match, FileObservation, MatchVerdict, VerdictOutcome};
pub use vision::{ChordVisionVerifier, VisionAnswer, VisionVerifier};
