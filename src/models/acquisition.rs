//! Acquisition-domain models (MUSEM-01, Plane MUSE S119 Sprint 1):
//! monitoring ("wanted"), requests, the download queue, typed history, and
//! the blocklist. See `migrations/0104_acquisition_domain.sql` for the
//! schema this backs and why the pre-existing quality tables
//! (`src/models/quality.rs`) are reused rather than redefined here.
//!
//! Status-shaped columns (`media_requests.status`, `download_queue.status`,
//! `history_events.event_type`) are stored as plain `text`, not a Postgres
//! enum type — mirrors `src/models/proactive_item.rs::ProactiveItem::status`.
//! The enums below are the application-level validated view: a row's raw
//! string always decodes (never fails a `SELECT`), and converts to/from the
//! enum via `TryFrom`/`as_str`, so an unrecognized value is a decode error
//! at the point of *use*, not a failed fetch.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value as Json;
use sqlx::FromRow;

/// `monitored_items` — the "wanted" driver row (blueprint §7.9-adjacent:
/// monitoring is per-`(media_metadata, library)`, decoupled from whether a
/// `media_items` row/file exists yet).
#[derive(Debug, Clone, FromRow, Serialize, Deserialize)]
pub struct MonitoredItem {
    pub id: i64,
    pub media_metadata_id: i64,
    pub media_item_id: Option<i64>,
    pub library_id: i64,
    pub monitored: bool,
    pub quality_profile_id: Option<i64>,
    pub min_availability: Option<String>,
    pub last_search_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct NewMonitoredItem {
    pub media_metadata_id: i64,
    pub media_item_id: Option<i64>,
    pub library_id: i64,
    pub monitored: bool,
    pub quality_profile_id: Option<i64>,
    pub min_availability: Option<String>,
}

/// `media_requests` — a <media-service>-lifecycle request. `status` is raw text
/// on the row (see module doc); use [`RequestStatus::from_str`] to decode.
#[derive(Debug, Clone, FromRow, Serialize, Deserialize)]
pub struct MediaRequest {
    pub id: i64,
    pub provider_ids: Json,
    pub media_kind: String,
    pub title: String,
    pub requested_by: Option<String>,
    pub status: String,
    pub tier: Option<String>,
    pub quality_profile_id: Option<i64>,
    pub note: Option<String>,
    /// MUSEM-06 follow-up (migration `0105_media_requests_monitored_item`):
    /// the `monitored_items` row this request originated from, when
    /// applicable. `NULL` for every `POST /requests`/`approve`/
    /// `AcquisitionSink` request (MUSEM-05, no monitored item involved) —
    /// only the wanted worker (`crate::acquisition::worker`) sets this. See
    /// `repo::acquisition::has_open_worker_request_for_monitored_item`, the
    /// consumer this column exists for.
    pub monitored_item_id: Option<i64>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct NewMediaRequest {
    pub provider_ids: Json,
    pub media_kind: String,
    pub title: String,
    pub requested_by: Option<String>,
    pub tier: Option<String>,
    pub quality_profile_id: Option<i64>,
    pub note: Option<String>,
    pub monitored_item_id: Option<i64>,
}

/// `download_queue` — one row per in-flight/terminal grab. The DB enforces
/// `request_id IS NOT NULL OR monitored_item_id IS NOT NULL` via a CHECK
/// (`download_queue_has_source`); [`NewDownloadQueueEntry`] mirrors that at
/// the type level with an explicit two-variant source.
#[derive(Debug, Clone, FromRow, Serialize, Deserialize)]
pub struct DownloadQueueEntry {
    pub id: i64,
    pub request_id: Option<i64>,
    pub monitored_item_id: Option<i64>,
    pub release_guid: String,
    pub release_title: String,
    pub indexer: Option<String>,
    pub download_client: Option<String>,
    pub client_hash: Option<String>,
    pub protocol: Option<String>,
    pub status: String,
    pub size_bytes: Option<i64>,
    pub added_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// The source a queued download traces back to — mirrors
/// `download_queue_has_source`'s "at least one of request/monitored-item"
/// CHECK at the type level so an enqueue call site cannot construct a
/// row-shape the DB would reject.
#[derive(Debug, Clone, Copy)]
pub enum DownloadSource {
    Request(i64),
    MonitoredItem(i64),
    Both { request_id: i64, monitored_item_id: i64 },
}

#[derive(Debug, Clone)]
pub struct NewDownloadQueueEntry {
    pub source: DownloadSource,
    pub release_guid: String,
    pub release_title: String,
    pub indexer: Option<String>,
    pub download_client: Option<String>,
    pub client_hash: Option<String>,
    pub protocol: Option<String>,
    pub size_bytes: Option<i64>,
}

/// `history_events` — typed history (see module/migration doc for why this
/// is jsonb-typed-by-`event_type` rather than a loose bag).
#[derive(Debug, Clone, FromRow, Serialize, Deserialize)]
pub struct HistoryEvent {
    pub id: i64,
    pub event_type: String,
    pub media_metadata_id: Option<i64>,
    pub monitored_item_id: Option<i64>,
    pub download_id: Option<String>,
    pub source_title: Option<String>,
    pub quality: Option<Json>,
    pub data: Json,
    pub languages: Json,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct NewHistoryEvent {
    pub event_type: String,
    pub media_metadata_id: Option<i64>,
    pub monitored_item_id: Option<i64>,
    pub download_id: Option<String>,
    pub source_title: Option<String>,
    pub quality: Option<Json>,
    pub data: Json,
    pub languages: Json,
}

/// `blocklist` — releases/hashes the (future) decision engine must never
/// re-grab.
#[derive(Debug, Clone, FromRow, Serialize, Deserialize)]
pub struct BlocklistEntry {
    pub id: i64,
    pub source_title: String,
    pub torrent_hash: Option<String>,
    pub media_metadata_id: Option<i64>,
    pub indexer: Option<String>,
    pub message: Option<String>,
    pub size_bytes: Option<i64>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct NewBlocklistEntry {
    pub source_title: String,
    pub torrent_hash: Option<String>,
    pub media_metadata_id: Option<i64>,
    pub indexer: Option<String>,
    pub message: Option<String>,
    pub size_bytes: Option<i64>,
}

/// A row returned by `repo::acquisition::list_wanted` — the monitored item
/// plus enough context (title, current best quality sort_order if any) for
/// a caller to act without a second round-trip. Kept distinct from
/// [`MonitoredItem`] since it's a query-shaped projection, not a table row.
#[derive(Debug, Clone, FromRow, Serialize, Deserialize)]
pub struct WantedItem {
    pub monitored_item_id: i64,
    pub media_metadata_id: i64,
    pub library_id: i64,
    pub title: String,
    pub quality_profile_id: Option<i64>,
    pub has_file: bool,
    pub best_quality_sort_order: Option<i32>,
    pub cutoff_sort_order: Option<i32>,
}

/// `media_requests.status` — the <media-service>-style request lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RequestStatus {
    Requested,
    Approved,
    Denied,
    Searching,
    Grabbed,
    Available,
    Failed,
}

impl RequestStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            RequestStatus::Requested => "requested",
            RequestStatus::Approved => "approved",
            RequestStatus::Denied => "denied",
            RequestStatus::Searching => "searching",
            RequestStatus::Grabbed => "grabbed",
            RequestStatus::Available => "available",
            RequestStatus::Failed => "failed",
        }
    }
}

impl std::str::FromStr for RequestStatus {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "requested" => Ok(RequestStatus::Requested),
            "approved" => Ok(RequestStatus::Approved),
            "denied" => Ok(RequestStatus::Denied),
            "searching" => Ok(RequestStatus::Searching),
            "grabbed" => Ok(RequestStatus::Grabbed),
            "available" => Ok(RequestStatus::Available),
            "failed" => Ok(RequestStatus::Failed),
            other => Err(format!("unknown request status: {other}")),
        }
    }
}

/// `download_queue.status` — the grab lifecycle from enqueue through
/// import.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QueueStatus {
    Queued,
    Downloading,
    Completed,
    Importing,
    Imported,
    Failed,
    Removed,
}

impl QueueStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            QueueStatus::Queued => "queued",
            QueueStatus::Downloading => "downloading",
            QueueStatus::Completed => "completed",
            QueueStatus::Importing => "importing",
            QueueStatus::Imported => "imported",
            QueueStatus::Failed => "failed",
            QueueStatus::Removed => "removed",
        }
    }
}

impl std::str::FromStr for QueueStatus {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "queued" => Ok(QueueStatus::Queued),
            "downloading" => Ok(QueueStatus::Downloading),
            "completed" => Ok(QueueStatus::Completed),
            "importing" => Ok(QueueStatus::Importing),
            "imported" => Ok(QueueStatus::Imported),
            "failed" => Ok(QueueStatus::Failed),
            "removed" => Ok(QueueStatus::Removed),
            other => Err(format!("unknown queue status: {other}")),
        }
    }
}

/// `history_events.event_type`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HistoryEventType {
    Requested,
    Grabbed,
    DownloadImported,
    DownloadFailed,
    Blocklisted,
    Deleted,
}

impl HistoryEventType {
    pub fn as_str(self) -> &'static str {
        match self {
            HistoryEventType::Requested => "requested",
            HistoryEventType::Grabbed => "grabbed",
            HistoryEventType::DownloadImported => "download_imported",
            HistoryEventType::DownloadFailed => "download_failed",
            HistoryEventType::Blocklisted => "blocklisted",
            HistoryEventType::Deleted => "deleted",
        }
    }
}

impl std::str::FromStr for HistoryEventType {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "requested" => Ok(HistoryEventType::Requested),
            "grabbed" => Ok(HistoryEventType::Grabbed),
            "download_imported" => Ok(HistoryEventType::DownloadImported),
            "download_failed" => Ok(HistoryEventType::DownloadFailed),
            "blocklisted" => Ok(HistoryEventType::Blocklisted),
            "deleted" => Ok(HistoryEventType::Deleted),
            other => Err(format!("unknown history event type: {other}")),
        }
    }
}

/// The `revision` half of the compound quality value (blueprint §2/§7.4) —
/// same field shape as `src/models/media_file.rs::Revision` (kept as a
/// distinct type here since this one is the acquisition-domain's own
/// serde-round-trippable compound, not a decode of `media_files`'
/// flattened `revision_*` columns).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct QualityRevision {
    pub version: i32,
    pub real: i32,
    pub is_repack: bool,
}

/// The compound quality value stamped onto a grab/import history event —
/// `{quality, revision}` (blueprint §2/§7.4: "quality is a compound value,
/// not a flat enum column"). `quality` is renamed (not `tier_id`) so the
/// serialized JSON matches the blueprint's `{"quality": <id>, "revision":
/// {...}}` shape exactly.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct QualityStamp {
    #[serde(rename = "quality")]
    pub tier_id: i64,
    pub revision: QualityRevision,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    #[test]
    fn quality_stamp_round_trips_quality_revision_shape() {
        let stamp = QualityStamp {
            tier_id: 7,
            revision: QualityRevision {
                version: 2,
                real: 0,
                is_repack: true,
            },
        };
        let json = serde_json::to_value(stamp).unwrap();
        assert_eq!(json["quality"], 7);
        assert_eq!(json["revision"]["version"], 2);
        assert_eq!(json["revision"]["real"], 0);
        assert_eq!(json["revision"]["is_repack"], true);

        let back: QualityStamp = serde_json::from_value(json).unwrap();
        assert_eq!(back, stamp);
    }

    #[test]
    fn quality_stamp_deserializes_from_raw_arr_shaped_json() {
        // A literal JSON document in the exact `{quality, revision}` shape
        // ARR-BLUEPRINT.md documents, not round-tripped through our own
        // serializer — proves the `#[serde(rename = "quality")]` mapping
        // actually matches the external shape, not just itself.
        let raw = serde_json::json!({
            "quality": 3,
            "revision": {"version": 1, "real": 1, "is_repack": false}
        });
        let stamp: QualityStamp = serde_json::from_value(raw).unwrap();
        assert_eq!(stamp.tier_id, 3);
        assert_eq!(stamp.revision.version, 1);
        assert_eq!(stamp.revision.real, 1);
        assert!(!stamp.revision.is_repack);
    }

    #[test]
    fn request_status_round_trips_through_str() {
        for status in [
            RequestStatus::Requested,
            RequestStatus::Approved,
            RequestStatus::Denied,
            RequestStatus::Searching,
            RequestStatus::Grabbed,
            RequestStatus::Available,
            RequestStatus::Failed,
        ] {
            let s = status.as_str();
            assert_eq!(RequestStatus::from_str(s).unwrap(), status);
        }
    }

    #[test]
    fn request_status_unknown_string_errors_not_panics() {
        assert!(RequestStatus::from_str("not-a-real-status").is_err());
    }

    #[test]
    fn queue_status_round_trips_through_str() {
        for status in [
            QueueStatus::Queued,
            QueueStatus::Downloading,
            QueueStatus::Completed,
            QueueStatus::Importing,
            QueueStatus::Imported,
            QueueStatus::Failed,
            QueueStatus::Removed,
        ] {
            let s = status.as_str();
            assert_eq!(QueueStatus::from_str(s).unwrap(), status);
        }
    }

    #[test]
    fn queue_status_unknown_string_errors_not_panics() {
        assert!(QueueStatus::from_str("not-a-real-status").is_err());
    }

    #[test]
    fn history_event_type_round_trips_through_str() {
        for kind in [
            HistoryEventType::Requested,
            HistoryEventType::Grabbed,
            HistoryEventType::DownloadImported,
            HistoryEventType::DownloadFailed,
            HistoryEventType::Blocklisted,
            HistoryEventType::Deleted,
        ] {
            let s = kind.as_str();
            assert_eq!(HistoryEventType::from_str(s).unwrap(), kind);
        }
    }
}
