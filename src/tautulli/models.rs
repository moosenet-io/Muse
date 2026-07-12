//! Typed (partial) models for the Tautulli API v2 JSON responses used by the
//! MUSE-06 backfill importer: `get_history`, `get_metadata`, `get_stream_data`.
//!
//! Every Tautulli API v2 response shares the same envelope —
//! `{"response": {"result": "success"|"error", "message": ..., "data": ...}}`
//! — regardless of `cmd`. Field names/shapes here follow the commonly
//! documented Tautulli API (tautulli.com/api docs + the widely-mirrored
//! community API reference); this has **not** been exercised against a live
//! Tautulli instance in this change (no reachable Tautulli in this build
//! environment), so — like `plex::models` before it — fields are
//! intentionally permissive (`Option`/`#[serde(default)]`) so an unexpected
//! or missing field degrades gracefully rather than breaking parsing.

use serde::Deserialize;

/// Top-level Tautulli response envelope, generic over the `data` payload
/// shape (varies per `cmd`).
#[derive(Debug, Deserialize)]
pub(crate) struct Envelope<T> {
    pub(crate) response: ResponseBody<T>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ResponseBody<T> {
    pub(crate) result: String,
    #[serde(default)]
    pub(crate) message: Option<String>,
    pub(crate) data: T,
}

/// `get_history` payload: a DataTables-shaped page of `HistoryRow`s plus the
/// total/filtered counts used to drive paging (`start`/`length` query
/// params on the next request).
#[derive(Debug, Default, Deserialize)]
pub(crate) struct HistoryData {
    #[serde(rename = "recordsFiltered", default)]
    pub(crate) records_filtered: i64,
    #[serde(rename = "recordsTotal", default)]
    pub(crate) records_total: i64,
    #[serde(default)]
    pub(crate) data: Vec<HistoryRow>,
}

/// One row of Tautulli watch history (`session_history` parity — see spec
/// §3.3/§4-D). `rating_key`/`parent_rating_key`/`grandparent_rating_key` are
/// numeric Plex ratingKeys in Tautulli's JSON; kept as `i64` here and
/// stringified at the call site to match `media_items.plex_rating_key`
/// /`episodes.plex_rating_key` (`text` columns, per MUSE-02).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct HistoryRow {
    /// Tautulli's own dedup key for a (possibly multi-part) playback —
    /// stored verbatim as `play_sessions.tautulli_ref_id`.
    pub reference_id: Option<i64>,
    pub id: Option<i64>,
    pub row_id: Option<i64>,
    /// Unix seconds.
    pub started: Option<i64>,
    /// Unix seconds.
    pub stopped: Option<i64>,
    /// Seconds actually played (session length, not necessarily full media
    /// runtime — see [`crate::tautulli::backfill`] for how this is combined
    /// with `get_metadata`'s `duration` when enrichment is available).
    pub duration: Option<i64>,
    pub paused_counter: Option<i32>,
    pub user_id: Option<i64>,
    pub user: Option<String>,
    pub friendly_name: Option<String>,
    pub player: Option<String>,
    pub platform: Option<String>,
    pub product: Option<String>,
    pub ip_address: Option<String>,
    pub session_key: Option<String>,
    pub rating_key: Option<i64>,
    pub parent_rating_key: Option<i64>,
    pub grandparent_rating_key: Option<i64>,
    pub full_title: Option<String>,
    pub title: Option<String>,
    /// `'movie' | 'episode' | 'track' | 'clip' | ...`
    pub media_type: Option<String>,
    /// 0-100 (Tautulli reports a percentage, not a 0-1 fraction).
    pub percent_complete: Option<f64>,
    /// Tautulli's own finished determination: `0` (not watched), `0.5`
    /// (partially watched), `1` (watched/scrobbled).
    pub watched_status: Option<f64>,
}

impl HistoryRow {
    pub fn rating_key_str(&self) -> Option<String> {
        self.rating_key.map(|k| k.to_string())
    }

    pub fn grandparent_rating_key_str(&self) -> Option<String> {
        self.grandparent_rating_key.map(|k| k.to_string())
    }
}

/// `get_metadata` payload — only the fields the backfill importer uses to
/// enrich a history row (true media runtime for a more accurate
/// `percent_complete`/`duration_ms` than the session-length-only `duration`
/// on [`HistoryRow`]).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct MetadataInfo {
    pub media_type: Option<String>,
    pub rating_key: Option<i64>,
    pub title: Option<String>,
    /// Full media runtime, milliseconds.
    pub duration: Option<i64>,
}

/// `get_stream_data` payload — session-level quality/transcode detail,
/// mapped onto `play_session_media_info` (Tautulli `session_history_media_info`
/// parity, per spec §3.3/§4-D). Field names follow the Tautulli API
/// convention of unprefixed `video_decision`/`audio_decision`/etc.;
/// UNVERIFIED against a live server (no reachable Tautulli in this build
/// environment) — the orchestrator should confirm on a real instance before
/// depending on this for anything beyond best-effort enrichment.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct StreamData {
    pub video_decision: Option<String>,
    pub audio_decision: Option<String>,
    pub transcode_decision: Option<String>,
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

/// Map a Tautulli decision string (`"direct play"`, `"direct stream"`,
/// `"transcode"`, `"copy"`, case-insensitive, space- or underscore-
/// separated) onto our [`crate::models::play_session::DecisionKind`].
/// Returns `None` for an empty/unrecognized value rather than erroring —
/// media-info enrichment is best-effort and must never fail the backfill
/// over one unfamiliar decision string.
pub fn parse_decision_kind(
    raw: Option<&str>,
) -> Option<crate::models::play_session::DecisionKind> {
    use crate::models::play_session::DecisionKind;

    let normalized = raw?.trim().to_ascii_lowercase().replace(['-', '_'], " ");
    match normalized.as_str() {
        "direct play" => Some(DecisionKind::DirectPlay),
        "direct stream" => Some(DecisionKind::DirectStream),
        "transcode" => Some(DecisionKind::Transcode),
        "copy" => Some(DecisionKind::Copy),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::play_session::DecisionKind;

    #[test]
    fn parse_decision_kind_handles_known_variants() {
        assert_eq!(parse_decision_kind(Some("direct play")), Some(DecisionKind::DirectPlay));
        assert_eq!(parse_decision_kind(Some("Direct Stream")), Some(DecisionKind::DirectStream));
        assert_eq!(parse_decision_kind(Some("TRANSCODE")), Some(DecisionKind::Transcode));
        assert_eq!(parse_decision_kind(Some("copy")), Some(DecisionKind::Copy));
        assert_eq!(parse_decision_kind(Some("direct_play")), Some(DecisionKind::DirectPlay));
    }

    #[test]
    fn parse_decision_kind_returns_none_for_unknown_or_missing() {
        assert_eq!(parse_decision_kind(Some("burst")), None);
        assert_eq!(parse_decision_kind(None), None);
        assert_eq!(parse_decision_kind(Some("")), None);
    }
}
