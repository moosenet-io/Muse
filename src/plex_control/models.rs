//! Typed request/response shapes for the Plex client-control (Companion) API
//! and the DB-facing `PlexPlayer` shape.
//!
//! Raw wire shapes (`Raw*`/`*Envelope`) mirror Plex's JSON responses (which
//! wrap everything in a top-level `MediaContainer`) and are kept private —
//! callers only see the typed, Muse-shaped structs below.

use serde::{Deserialize, Serialize};

/// A discovered Plex player / cast target, as stored in `plex_clients`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PlexPlayer {
    /// Plex client id — the control target for every playback command.
    pub machine_identifier: String,
    pub name: Option<String>,
    pub product: Option<String>,
    /// Plex's `deviceClass` (e.g. "stb", "phone", "tv").
    pub device: Option<String>,
    pub platform: Option<String>,
    pub address: Option<String>,
    pub port: Option<u16>,
    /// e.g. "playback", "timeline", "navigation" — from `protocolCapabilities`.
    pub protocol_caps: Vec<String>,
    /// Best-effort heuristic (product/device name match) — see
    /// `RawPlexClient::into_player`. plex.tv `/resources` (which carries an
    /// explicit `provides` field including "player"/"controller") would give
    /// a more reliable signal for remote/Chromecast targets; not implemented
    /// here (`GET /clients` from the local PMS only) — noted as a follow-up.
    pub is_cast_target: bool,
}

/// Envelope Plex wraps every JSON response in.
#[derive(Debug, Deserialize)]
pub(super) struct MediaContainerEnvelope<T> {
    #[serde(rename = "MediaContainer")]
    pub media_container: T,
}

#[derive(Debug, Deserialize)]
pub(super) struct ClientsMediaContainer {
    #[serde(rename = "Server", default)]
    pub server: Vec<RawPlexClient>,
}

/// `GET /clients` list entry. Field names/casing per Plex's legacy
/// "Server" naming for registered client-control targets.
#[derive(Debug, Deserialize)]
pub(super) struct RawPlexClient {
    pub name: Option<String>,
    pub address: Option<String>,
    pub port: Option<u16>,
    #[serde(rename = "machineIdentifier")]
    pub machine_identifier: String,
    pub product: Option<String>,
    #[serde(rename = "deviceClass")]
    pub device_class: Option<String>,
    pub platform: Option<String>,
    #[serde(rename = "protocolCapabilities")]
    pub protocol_capabilities: Option<String>,
}

impl RawPlexClient {
    pub fn into_player(self) -> PlexPlayer {
        let protocol_caps: Vec<String> = self
            .protocol_capabilities
            .unwrap_or_default()
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();

        let haystack = format!(
            "{} {}",
            self.product.as_deref().unwrap_or_default(),
            self.device_class.as_deref().unwrap_or_default()
        )
        .to_lowercase();
        let is_cast_target = haystack.contains("chromecast") || haystack.contains("cast");

        PlexPlayer {
            machine_identifier: self.machine_identifier,
            name: self.name,
            product: self.product,
            device: self.device_class,
            platform: self.platform,
            address: self.address,
            port: self.port,
            protocol_caps,
            is_cast_target,
        }
    }
}

/// Plex server identity (`GET /identity`) — needed to build the
/// `server://{machineIdentifier}/...` URI play queues require.
#[derive(Debug, Deserialize)]
pub(super) struct IdentityMediaContainer {
    #[serde(rename = "machineIdentifier")]
    pub machine_identifier: String,
}

/// Request to build a Plex play queue from an ordered list of ratingKeys.
#[derive(Debug, Clone)]
pub struct PlayQueueRequest {
    /// Ordered `ratingKey`s (Plex library item ids), in play order.
    pub rating_keys: Vec<String>,
    pub shuffle: bool,
    pub continuous: bool,
}

impl PlayQueueRequest {
    pub fn new(rating_keys: Vec<String>) -> Self {
        Self {
            rating_keys,
            shuffle: false,
            continuous: false,
        }
    }
}

/// A created Plex play queue (`POST /playQueues` response).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PlayQueue {
    pub play_queue_id: i64,
    pub play_queue_selected_item_id: Option<i64>,
    pub size: i64,
}

#[derive(Debug, Deserialize)]
pub(super) struct PlayQueueMediaContainer {
    #[serde(rename = "playQueueID")]
    pub play_queue_id: i64,
    #[serde(rename = "playQueueSelectedItemID")]
    pub play_queue_selected_item_id: Option<i64>,
    #[serde(default, rename = "size")]
    pub size: i64,
}

/// Transport commands understood by `/player/playback/*`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportCommand {
    Play,
    Pause,
    Stop,
    SkipNext,
}

impl TransportCommand {
    pub(super) fn endpoint(self) -> &'static str {
        match self {
            TransportCommand::Play => "play",
            TransportCommand::Pause => "pause",
            TransportCommand::Stop => "stop",
            TransportCommand::SkipNext => "skipNext",
        }
    }
}

/// Parsed `/player/timeline/poll` state. `raw` retains the full decoded body
/// since the timeline payload has many optional/version-dependent fields we
/// don't need to model exhaustively yet.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TimelinePoll {
    pub state: Option<String>,
    pub rating_key: Option<String>,
    pub time_ms: Option<i64>,
    pub duration_ms: Option<i64>,
    pub raw: serde_json::Value,
}
