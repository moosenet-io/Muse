//! `play_sessions` + `play_session_media_info` — reconstructed watch
//! sessions (spec §3.3, Tautulli `session_history`/`session_history_media_info`
//! parity). See `migrations/0015_play_sessions.sql` for the media_item_id /
//! episode_id divergence from the spec's flat media_items/media_children.

use chrono::{DateTime, Utc};
use ipnetwork::IpNetwork;
use serde::{Deserialize, Serialize};
use sqlx::FromRow;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type)]
#[sqlx(type_name = "decision_kind", rename_all = "snake_case")]
#[serde(rename_all = "snake_case")]
pub enum DecisionKind {
    DirectPlay,
    DirectStream,
    Transcode,
    Copy,
}

#[derive(Debug, Clone, FromRow, Serialize, Deserialize)]
pub struct PlaySession {
    pub id: i64,
    pub account_id: Option<i64>,
    pub media_item_id: Option<i64>,
    pub episode_id: Option<i64>,
    pub session_key: Option<String>,
    pub tautulli_ref_id: Option<i64>,
    pub started_at: DateTime<Utc>,
    pub stopped_at: Option<DateTime<Utc>>,
    pub duration_ms: Option<i64>,
    pub watched_ms: Option<i64>,
    pub view_offset_ms: Option<i64>,
    pub percent_complete: Option<f32>,
    pub paused_counter: i32,
    pub paused_ms: i64,
    pub is_finished: bool,
    pub is_abandoned: bool,
    pub player: Option<String>,
    pub platform: Option<String>,
    pub product: Option<String>,
    pub device: Option<String>,
    pub ip_address: Option<IpNetwork>,
    pub started_hour: Option<i32>,
    pub started_dow: Option<i32>,
    pub is_cinema_context: Option<bool>,
    pub created_at: DateTime<Utc>,
}

/// Fields accepted when upserting a session (keyed by the table's
/// `(account_id, media_item_id, episode_id, started_at)` UNIQUE — see
/// `repo::play_session::upsert`).
#[derive(Debug, Clone)]
pub struct NewPlaySession {
    pub account_id: Option<i64>,
    pub media_item_id: Option<i64>,
    pub episode_id: Option<i64>,
    pub session_key: Option<String>,
    pub tautulli_ref_id: Option<i64>,
    pub started_at: DateTime<Utc>,
    pub stopped_at: Option<DateTime<Utc>>,
    pub duration_ms: Option<i64>,
    pub watched_ms: Option<i64>,
    pub view_offset_ms: Option<i64>,
    pub percent_complete: Option<f32>,
    pub paused_counter: i32,
    pub paused_ms: i64,
    pub is_finished: bool,
    pub is_abandoned: bool,
    pub player: Option<String>,
    pub platform: Option<String>,
    pub product: Option<String>,
    pub device: Option<String>,
    pub ip_address: Option<IpNetwork>,
    pub started_hour: Option<i32>,
    pub started_dow: Option<i32>,
    pub is_cinema_context: Option<bool>,
}

#[derive(Debug, Clone, FromRow, Serialize, Deserialize)]
pub struct PlaySessionMediaInfo {
    pub play_session_id: i64,
    pub video_decision: Option<DecisionKind>,
    pub audio_decision: Option<DecisionKind>,
    pub transcode_decision: Option<DecisionKind>,
    pub container: Option<String>,
    pub video_codec: Option<String>,
    pub audio_codec: Option<String>,
    pub audio_channels: Option<f32>,
    pub video_resolution: Option<String>,
    pub bitrate: Option<i32>,
    pub width: Option<i32>,
    pub height: Option<i32>,
    pub transcode_reason: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct NewPlaySessionMediaInfo {
    pub video_decision: Option<DecisionKind>,
    pub audio_decision: Option<DecisionKind>,
    pub transcode_decision: Option<DecisionKind>,
    pub container: Option<String>,
    pub video_codec: Option<String>,
    pub audio_codec: Option<String>,
    pub audio_channels: Option<f32>,
    pub video_resolution: Option<String>,
    pub bitrate: Option<i32>,
    pub width: Option<i32>,
    pub height: Option<i32>,
    pub transcode_reason: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decision_kind_serde_round_trip() {
        for kind in [
            DecisionKind::DirectPlay,
            DecisionKind::DirectStream,
            DecisionKind::Transcode,
            DecisionKind::Copy,
        ] {
            let json = serde_json::to_string(&kind).unwrap();
            let back: DecisionKind = serde_json::from_str(&json).unwrap();
            assert_eq!(kind, back);
        }
        assert_eq!(
            serde_json::to_string(&DecisionKind::DirectPlay).unwrap(),
            "\"direct_play\""
        );
    }
}
