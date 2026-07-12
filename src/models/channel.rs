//! `channels` / `channel_runs` / `channel_programs` — the pseudo-TV director
//! schema (MUSE-23, spec §3.8/§4d). This module owns the DATA MODEL only:
//! - the channel *definition* (a preset/personal/theme/genre/era brief),
//! - a composed *run* instance (an on-demand play-queue Muse actually built
//!   and, optionally, played),
//! - and the linear *program grid* (the time-anchored EPG rows that back
//!   the future XMLTV/HDHomeRun guide).
//!
//! The agentic composer that fills a run's `schedule` / a channel's rolling
//! `channel_programs` window (MUSE-24), the Terminus/Lumina playback tools
//! (MUSE-25), the web lineup guide (MUSE-27), the HDHomeRun/XMLTV tuner
//! (MUSE-28), and the ffmpeg streaming engine (MUSE-29) are separate, later
//! items — none of that logic lives here.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value as Json;
use sqlx::FromRow;

// --- channels ---------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type)]
#[sqlx(type_name = "channel_kind", rename_all = "snake_case")]
#[serde(rename_all = "snake_case")]
pub enum ChannelKind {
    Personal,
    Theme,
    Genre,
    Era,
    Preset,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type)]
#[sqlx(type_name = "channel_mode", rename_all = "snake_case")]
#[serde(rename_all = "snake_case")]
pub enum ChannelMode {
    OnDemand,
    Linear,
}

#[derive(Debug, Clone, FromRow, Serialize, Deserialize)]
pub struct Channel {
    pub id: i64,
    /// Seam: `accounts` (spec §3.2) isn't built in this repo yet (it ships
    /// with MUSE-03's telemetry/taste migrations) — no FK until then.
    pub account_id: Option<i64>,
    pub name: String,
    pub kind: ChannelKind,
    pub mode: ChannelMode,
    pub channel_number: Option<f32>,
    pub target_client_id: Option<i64>,
    pub directive: Option<String>,
    pub rules: Json,
    pub is_preset: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct NewChannel {
    pub account_id: Option<i64>,
    pub name: String,
    pub kind: ChannelKind,
    pub mode: ChannelMode,
    pub channel_number: Option<f32>,
    pub target_client_id: Option<i64>,
    pub directive: Option<String>,
    pub rules: Json,
    pub is_preset: bool,
}

// --- channel_runs -------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type)]
#[sqlx(type_name = "channel_run_status", rename_all = "snake_case")]
#[serde(rename_all = "snake_case")]
pub enum ChannelRunStatus {
    Composed,
    Playing,
    Paused,
    Stopped,
    Completed,
}

#[derive(Debug, Clone, FromRow, Serialize, Deserialize)]
pub struct ChannelRun {
    pub id: i64,
    pub channel_id: Option<i64>,
    /// Seam: see `Channel::account_id`.
    pub account_id: Option<i64>,
    pub target_client_id: Option<i64>,
    pub plex_play_queue_id: Option<String>,
    pub schedule: Json,
    pub total_duration_ms: Option<i64>,
    pub composed_at: DateTime<Utc>,
    pub started_at: Option<DateTime<Utc>>,
    pub ended_at: Option<DateTime<Utc>>,
    pub status: ChannelRunStatus,
}

#[derive(Debug, Clone)]
pub struct NewChannelRun {
    pub channel_id: Option<i64>,
    pub account_id: Option<i64>,
    pub target_client_id: Option<i64>,
    pub plex_play_queue_id: Option<String>,
    pub schedule: Json,
    pub total_duration_ms: Option<i64>,
}

// --- channel_programs (the linear EPG grid) -----------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type)]
#[sqlx(type_name = "channel_program_item_type", rename_all = "snake_case")]
#[serde(rename_all = "snake_case")]
pub enum ChannelProgramItemType {
    Episode,
    Movie,
    Interstitial,
}

#[derive(Debug, Clone, FromRow, Serialize, Deserialize)]
pub struct ChannelProgram {
    pub id: i64,
    pub channel_id: i64,
    pub item_type: ChannelProgramItemType,
    pub media_item_id: Option<i64>,
    pub episode_id: Option<i64>,
    pub interstitial_id: Option<i64>,
    pub title: String,
    pub subtitle: Option<String>,
    pub description: Option<String>,
    pub artwork_url: Option<String>,
    pub start_at: DateTime<Utc>,
    pub end_at: DateTime<Utc>,
    pub duration_ms: i64,
    pub rationale: Option<String>,
    /// Seam: once a program plays, the taste loop (MUSE-25) logs it back
    /// into `play_events` (telemetry, MUSE-03 — in flight, not yet built in
    /// this repo). No FK until that table exists.
    pub play_event_id: Option<i64>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct NewChannelProgram {
    pub channel_id: i64,
    pub item_type: ChannelProgramItemType,
    pub media_item_id: Option<i64>,
    pub episode_id: Option<i64>,
    pub interstitial_id: Option<i64>,
    pub title: String,
    pub subtitle: Option<String>,
    pub description: Option<String>,
    pub artwork_url: Option<String>,
    pub start_at: DateTime<Utc>,
    pub end_at: DateTime<Utc>,
    pub duration_ms: i64,
    pub rationale: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn channel_mode_serde_round_trip() {
        let json = serde_json::to_string(&ChannelMode::Linear).unwrap();
        assert_eq!(json, "\"linear\"");
        let back: ChannelMode = serde_json::from_str(&json).unwrap();
        assert_eq!(back, ChannelMode::Linear);
    }

    #[test]
    fn channel_program_item_type_serde_round_trip() {
        let json = serde_json::to_string(&ChannelProgramItemType::Interstitial).unwrap();
        assert_eq!(json, "\"interstitial\"");
        let back: ChannelProgramItemType = serde_json::from_str(&json).unwrap();
        assert_eq!(back, ChannelProgramItemType::Interstitial);
    }
}
