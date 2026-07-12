//! Linear tuner (MUSE-28, spec §4d-E) — exposes `mode='linear'` channels to
//! Plex Live TV as a custom HDHomeRun-emulation tuner (`/discover.json`,
//! `/lineup.json`, `/lineup_status.json`) and, as an M3U+XMLTV alternative,
//! `/muse.m3u` + `/xmltv.xml`. A rolling-window scheduler (`scheduler`)
//! keeps each linear channel's `channel_programs` grid (MUSE-23) topped up
//! 24-48h ahead; `xmltv` renders EPG programme data straight from that
//! grid.
//!
//! The actual continuous video stream (`/auto/vN`, ffmpeg concat + join-
//! mid-stream) is MUSE-29, a separate later item — [`stream_stub`] below is
//! the documented placeholder every advertised URL here points at until
//! that lands.

pub mod hdhr;
pub mod m3u;
pub mod scheduler;
pub mod xmltv;

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::response::IntoResponse;

use crate::error::MuseError;
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

/// The MUSE-29 stream stub every `/discover.json`/`lineup.json`/`muse.m3u`
/// URL currently points at (`/auto/v{channel_id}`, the exact path shape the
/// real HDHomeRun protocol uses). Returns `501 Not Implemented` — the
/// ffmpeg concat/join-mid-stream engine is a separate, later spec item.
pub async fn stream_stub(
    State(_state): State<Arc<AppState>>,
    Path(channel_id): Path<i64>,
) -> impl IntoResponse {
    tracing::debug!(channel_id, "tuner stream stub hit — MUSE-29 not built yet");
    MuseError::NotImplemented
}
