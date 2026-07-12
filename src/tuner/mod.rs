//! Linear tuner (MUSE-28, spec §4d-E) — exposes `mode='linear'` channels to
//! Plex Live TV as a custom HDHomeRun-emulation tuner (`/discover.json`,
//! `/lineup.json`, `/lineup_status.json`) and, as an M3U+XMLTV alternative,
//! `/muse.m3u` + `/xmltv.xml`. A rolling-window scheduler (`scheduler`)
//! keeps each linear channel's `channel_programs` grid (MUSE-23) topped up
//! 24-48h ahead; `xmltv` renders EPG programme data straight from that
//! grid.
//!
//! The actual continuous video stream (`/auto/vN`, ffmpeg concat + join-
//! mid-stream) is MUSE-29 — see [`crate::streaming`], which every URL
//! advertised here now points at.

pub mod hdhr;
pub mod m3u;
pub mod scheduler;
pub mod xmltv;

use crate::http::AppState;

/// Build the base URL Plex/players use to reach this instance, honoring
/// [`crate::config::Config::public_base_url`] and otherwise degrading to
/// `http://{bind_addr}` (only correct when `bind_addr` isn't a wildcard
/// address — see the field's doc comment).
pub fn base_url(state: &AppState) -> String {
    match &state.config.public_base_url {
        Some(url) => url.trim_end_matches('/').to_string(),
        None => format!("http://{}", state.config.bind_addr),
    }
}
