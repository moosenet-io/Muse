//! Tautulli API v2 client + one-time watch-history backfill importer
//! (MUSE-06, spec §4-D).
//!
//! This module is the Tautulli half of the Tautulli-replacement subsystem
//! (§4): MUSE-07 (native Plex webhook/poller capture) is the *ongoing*
//! Tautulli replacement, and this module is the *one-time* migration of
//! Tautulli's pre-existing history onto the same `play_sessions` /
//! `play_session_media_info` schema, tagged `tautulli_ref_id` for
//! provenance and deduped against anything MUSE-07 already captured
//! natively. Muse never depends on Tautulli staying up — it only mines its
//! history once (see [`backfill::run`]).
//!
//! Read-only against Tautulli; makes no writes to Tautulli's own database.

pub mod backfill;
mod client;
mod models;

pub use client::{HistoryPage, TautulliClient, DEFAULT_PAGE_SIZE};
pub use models::{HistoryRow, MetadataInfo, StreamData};
