//! Matching-verification support (MUSEL-C1/C2): proving an identified media
//! file really is the title Muse thinks it is, rather than trusting a
//! provider-ID match blindly.
//!
//! - [`stills`] — MUSEL-C1: the ffmpeg sample-still extraction primitive.
//!   Pure timestamp-spread math + one impure per-timestamp ffmpeg spawn,
//!   mirroring the split in [`crate::streaming`] (`ffmpeg` module = pure
//!   arg-building, `mod.rs` = the one impure spawn/read layer).
//!
//! MUSEL-C2 (the `verify_match`/`vision`/`liveness` verdict logic that
//! consumes these stills) is a separate spec item and lands in a later
//! module here.

pub mod stills;

pub use stills::{extract_sample_stills, Still};
