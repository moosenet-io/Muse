//! HDHomeRun-emulation discovery: `/discover.json`, `/lineup_status.json`,
//! `/lineup.json` — the exact shape Plex's built-in HDHomeRun tuner
//! integration expects (see the real HDHomeRun `/discover.json`/
//! `/lineup.json` API this mimics).

use std::sync::Arc;

use axum::extract::State;
use axum::response::IntoResponse;
use axum::Json;
use serde::Serialize;
use serde_json::json;

use crate::error::MuseResult;
use crate::http::AppState;
use crate::repo;

use super::base_url;

/// A single `/lineup.json` entry — GuideNumber must be a *string* per the
/// HDHomeRun protocol even though it's numeric-shaped.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct LineupEntry {
    #[serde(rename = "GuideNumber")]
    pub guide_number: String,
    #[serde(rename = "GuideName")]
    pub guide_name: String,
    #[serde(rename = "URL")]
    pub url: String,
}

/// `GuideNumber` for a channel: its configured `channel_number` (spec
/// §3.8's guide number, e.g. `101.1`) when set, else a stable fallback
/// derived from the channel id so every linear channel is still tunable.
pub fn guide_number(channel: &crate::models::Channel) -> String {
    match channel.channel_number {
        Some(n) if n.fract() == 0.0 => format!("{}", n as i64),
        Some(n) => format!("{n}"),
        None => format!("9{:03}", channel.id % 1000),
    }
}

/// `tvg-id`/XMLTV channel id: a stable, URL/XML-safe identifier for a
/// channel, independent of its (mutable) `channel_number`/`name`.
pub fn channel_ref(channel_id: i64) -> String {
    format!("muse-{channel_id}")
}

/// Build the list of tunable lineup entries for every linear channel,
/// shared by `/lineup.json`, `/muse.m3u`, and `/xmltv.xml` so all three
/// stay in agreement.
pub async fn lineup_entries(state: &AppState) -> MuseResult<Vec<LineupEntry>> {
    let base = base_url(state);
    let channels = repo::channel::list_linear_channels(&state.pool).await?;
    Ok(channels
        .into_iter()
        .map(|c| LineupEntry {
            guide_number: guide_number(&c),
            guide_name: c.name.clone(),
            url: format!("{base}/auto/v{}", c.id),
        })
        .collect())
}

/// `GET /discover.json` — HDHomeRun device metadata.
pub async fn discover_json(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let base = base_url(&state);
    Json(json!({
        "FriendlyName": "Muse TV",
        "Manufacturer": "Muse",
        "ManufacturerURL": format!("{base}/"),
        "ModelNumber": "MUSE-TUNER-1",
        "FirmwareName": "muse-tuner",
        "FirmwareVersion": env!("CARGO_PKG_VERSION"),
        "DeviceID": state.config.hdhr_device_id.clone(),
        "DeviceAuth": "muse",
        "BaseURL": base.clone(),
        "LineupURL": format!("{base}/lineup.json"),
        "TunerCount": 4,
    }))
}

/// `GET /lineup_status.json` — Plex polls this while (not) scanning.
pub async fn lineup_status_json() -> impl IntoResponse {
    Json(json!({
        "ScanInProgress": 0,
        "ScanPossible": 1,
        "Source": "Cable",
        "SourceList": ["Cable"],
    }))
}

/// `GET /lineup.json` — the bare JSON array HDHomeRun clients expect, one
/// entry per enabled (`mode='linear'`) channel.
pub async fn lineup_json(State(state): State<Arc<AppState>>) -> crate::error::MuseResult<Json<Vec<LineupEntry>>> {
    Ok(Json(lineup_entries(&state).await?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{Channel, ChannelKind, ChannelMode};
    use chrono::Utc;

    fn fixture_channel(id: i64, number: Option<f32>) -> Channel {
        Channel {
            id,
            account_id: None,
            name: format!("Test Channel {id}"),
            kind: ChannelKind::Preset,
            mode: ChannelMode::Linear,
            channel_number: number,
            target_client_id: None,
            directive: None,
            rules: serde_json::json!({}),
            is_preset: true,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    #[test]
    fn guide_number_uses_channel_number_when_set() {
        assert_eq!(guide_number(&fixture_channel(1, Some(101.0))), "101");
        assert_eq!(guide_number(&fixture_channel(1, Some(101.5))), "101.5");
    }

    #[test]
    fn guide_number_falls_back_to_id_when_unset() {
        assert_eq!(guide_number(&fixture_channel(7, None)), "9007");
    }

    #[test]
    fn channel_ref_is_stable_and_prefixed() {
        assert_eq!(channel_ref(42), "muse-42");
    }
}
