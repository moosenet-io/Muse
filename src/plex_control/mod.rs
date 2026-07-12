//! Plex playback-control client + player discovery (MUSE-22, spec §4d-A).
//!
//! This module is the *mechanism* piece of the Channels feature: discover
//! Plex Companion / client-control targets (`plex_clients`), build play
//! queues, and drive transport commands (`play`/`pause`/`skipNext`/`stop`/
//! timeline poll). These are all benign control actions — they start/steer
//! playback and never mutate the Plex library.
//!
//! Deliberately **out of scope** here:
//! - Confirm-gate / approval logic for issuing these commands — that's
//!   MUSE-25, layered on top of this mechanism.
//! - The channel composer (schedule generation) — MUSE-2x, consumes
//!   `create_play_queue`/`CastController` from here.
//! - Google Cast (raw Cast v2) for bare Chromecasts without a Plex
//!   receiver — `cast::GoogleCastController` reserves the seam only.

mod cast;
mod client;
mod models;
mod repo;

pub use cast::{CastController, GoogleCastController};
pub use client::PlexControlClient;
pub use models::{PlayQueue, PlayQueueRequest, PlexPlayer, TimelinePoll, TransportCommand};
pub use repo::upsert_players;
