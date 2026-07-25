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

/// Deserialization helpers for Tautulli's loosely-typed JSON.
///
/// Tautulli emits numeric fields inconsistently: a real integer for
/// well-formed rows, but an **empty string** `""` (not `null`, not a number)
/// for fields that don't apply to a given item — e.g. `parent_rating_key`,
/// `grandparent_rating_key`, `media_index`, `parent_media_index` on a movie
/// (parentless), or `year` on items with no release year. A bare
/// `Option<i64>` does **not** save us here: serde maps JSON `null` → `None`,
/// but delegates the empty string to `i64`, which rejects it with
/// `invalid type: string "", expected i64`, and — because
/// `serde_json::from_slice` is all-or-nothing — a single such field fails the
/// entire `get_history` page (all 1872 rows, zero imported).
///
/// [`de::empty_string_as_none`] tolerates all three shapes: a JSON number, a
/// numeric string (`"123"`), or an empty string / whitespace / `null` → `None`.
mod de {
    use serde::{Deserialize, Deserializer};
    use std::fmt::Display;
    use std::str::FromStr;

    /// Deserialize a numeric field that Tautulli may send as a JSON number, a
    /// numeric string, or an empty string. Empty string / whitespace / `null`
    /// → `None`; a non-empty string is parsed via `FromStr`.
    ///
    /// Generic over the numeric target `T` (`i64`, `i32`, `f64`, `f32`, ...)
    /// so one helper covers every numeric field on every Tautulli struct.
    pub(super) fn empty_string_as_none<'de, D, T>(
        deserializer: D,
    ) -> Result<Option<T>, D::Error>
    where
        D: Deserializer<'de>,
        T: FromStr + Deserialize<'de>,
        <T as FromStr>::Err: Display,
    {
        // Untagged: a JSON number deserializes straight into `T`; anything
        // else (including a numeric string or `""`) falls through to `Str`.
        // `Option<_>` in front handles JSON `null` → `None` before the
        // untagged enum is consulted.
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum NumOrStr<T> {
            Num(T),
            Str(String),
        }

        match Option::<NumOrStr<T>>::deserialize(deserializer)? {
            None => Ok(None),
            Some(NumOrStr::Num(v)) => Ok(Some(v)),
            Some(NumOrStr::Str(s)) => {
                let trimmed = s.trim();
                if trimmed.is_empty() {
                    Ok(None)
                } else {
                    trimmed
                        .parse::<T>()
                        .map(Some)
                        .map_err(serde::de::Error::custom)
                }
            }
        }
    }
}

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
///
/// **Numeric-field robustness:** every integer/float field below carries
/// `#[serde(default, deserialize_with = "de::empty_string_as_none")]` because
/// Tautulli sends `""` (empty string) rather than `null`/a number for fields
/// that don't apply to a given item (movies are parentless, some items lack a
/// `year`, etc.). Without this, one `""` fails the whole `get_history` page.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct HistoryRow {
    /// Tautulli's own dedup key for a (possibly multi-part) playback —
    /// stored verbatim as `play_sessions.tautulli_ref_id`.
    #[serde(default, deserialize_with = "de::empty_string_as_none")]
    pub reference_id: Option<i64>,
    #[serde(default, deserialize_with = "de::empty_string_as_none")]
    pub id: Option<i64>,
    #[serde(default, deserialize_with = "de::empty_string_as_none")]
    pub row_id: Option<i64>,
    /// Unix seconds.
    #[serde(default, deserialize_with = "de::empty_string_as_none")]
    pub started: Option<i64>,
    /// Unix seconds.
    #[serde(default, deserialize_with = "de::empty_string_as_none")]
    pub stopped: Option<i64>,
    /// Seconds actually played (session length, not necessarily full media
    /// runtime — see [`crate::tautulli::backfill`] for how this is combined
    /// with `get_metadata`'s `duration` when enrichment is available).
    #[serde(default, deserialize_with = "de::empty_string_as_none")]
    pub duration: Option<i64>,
    #[serde(default, deserialize_with = "de::empty_string_as_none")]
    pub paused_counter: Option<i32>,
    #[serde(default, deserialize_with = "de::empty_string_as_none")]
    pub user_id: Option<i64>,
    pub user: Option<String>,
    pub friendly_name: Option<String>,
    pub player: Option<String>,
    pub platform: Option<String>,
    pub product: Option<String>,
    pub ip_address: Option<String>,
    pub session_key: Option<String>,
    #[serde(default, deserialize_with = "de::empty_string_as_none")]
    pub rating_key: Option<i64>,
    /// `""` on movies/parentless items (observed live: 169/1872 rows).
    #[serde(default, deserialize_with = "de::empty_string_as_none")]
    pub parent_rating_key: Option<i64>,
    /// `""` on movies/parentless items (observed live: 169/1872 rows).
    #[serde(default, deserialize_with = "de::empty_string_as_none")]
    pub grandparent_rating_key: Option<i64>,
    /// Episode number within a season (`""` on parentless items — 169 rows).
    #[serde(default, deserialize_with = "de::empty_string_as_none")]
    pub media_index: Option<i64>,
    /// Season number (`""` on parentless items — 169 rows).
    #[serde(default, deserialize_with = "de::empty_string_as_none")]
    pub parent_media_index: Option<i64>,
    /// Release year (`""` when Tautulli has no year — observed 72 rows).
    #[serde(default, deserialize_with = "de::empty_string_as_none")]
    pub year: Option<i64>,
    pub full_title: Option<String>,
    pub title: Option<String>,
    /// `'movie' | 'episode' | 'track' | 'clip' | ...`
    pub media_type: Option<String>,
    /// 0-100 (Tautulli reports a percentage, not a 0-1 fraction).
    #[serde(default, deserialize_with = "de::empty_string_as_none")]
    pub percent_complete: Option<f64>,
    /// Tautulli's own finished determination: `0` (not watched), `0.5`
    /// (partially watched), `1` (watched/scrobbled).
    #[serde(default, deserialize_with = "de::empty_string_as_none")]
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

/// `get_metadata` payload — the fields the backfill importer uses to enrich a
/// history row: the item's true media runtime (for a more accurate
/// `percent_complete`/`duration_ms` than the session-length-only `duration`
/// on [`HistoryRow`]) and, for BSEED-1 GUID resolution, the provider `guids`
/// (`imdb://`/`tmdb://`/`tvdb://`) plus the show-level `grandparent_guid`.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct MetadataInfo {
    pub media_type: Option<String>,
    #[serde(default, deserialize_with = "de::empty_string_as_none")]
    pub rating_key: Option<i64>,
    pub title: Option<String>,
    /// Full media runtime, milliseconds.
    #[serde(default, deserialize_with = "de::empty_string_as_none")]
    pub duration: Option<i64>,
    /// Release year — used as the second key of the title+year resolution
    /// fallback (BSEED-1). `""`/absent → `None`, like every other numeric.
    #[serde(default, deserialize_with = "de::empty_string_as_none")]
    pub year: Option<i64>,
    /// Provider GUIDs for this item — Tautulli/Plex emit these either as an
    /// array of bare strings (`["imdb://tt1856101", "tmdb://335984"]`) or as
    /// an array of `{"id": "..."}` objects depending on version; both shapes
    /// are tolerated by [`GuidEntry`]. Use [`MetadataInfo::guids`] to read the
    /// flattened string list rather than this field directly.
    #[serde(default)]
    pub guids: Vec<GuidEntry>,
    /// The owning show's GUID for an episode (`grandparent_guid`) — how a TV
    /// session resolves onto an *arr-ingested show `media_metadata` row (which
    /// carries the show's tvdb/tmdb/imdb id) without a Plex library sync.
    pub grandparent_guid: Option<String>,
}

/// One entry of a Tautulli/Plex `guids` array — either a bare string
/// (`"imdb://tt1856101"`) or a `{"id": "imdb://tt1856101"}` object, depending
/// on the Tautulli/Plex-agent version. Deserialized permissively (untagged)
/// so an unexpected shape degrades to being ignored rather than failing the
/// whole `get_metadata` parse — same posture as the rest of this module.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum GuidEntry {
    Str(String),
    Obj { id: String },
}

impl GuidEntry {
    fn as_str(&self) -> &str {
        match self {
            GuidEntry::Str(s) => s,
            GuidEntry::Obj { id } => id,
        }
    }
}

impl MetadataInfo {
    /// The flattened provider-GUID strings for this item, regardless of which
    /// wire shape Tautulli sent (see [`GuidEntry`]).
    pub fn guids(&self) -> Vec<String> {
        self.guids.iter().map(|g| g.as_str().to_string()).collect()
    }
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
    #[serde(default, deserialize_with = "de::empty_string_as_none")]
    pub audio_channels: Option<f32>,
    pub video_resolution: Option<String>,
    #[serde(default, deserialize_with = "de::empty_string_as_none")]
    pub bitrate: Option<i32>,
    #[serde(default, deserialize_with = "de::empty_string_as_none")]
    pub width: Option<i32>,
    #[serde(default, deserialize_with = "de::empty_string_as_none")]
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

    /// Regression: a movie (parentless) row from a live Tautulli `get_history`
    /// page sends `""` for `parent_rating_key`, `grandparent_rating_key`,
    /// `media_index`, `parent_media_index`, and `year`. Before the tolerant
    /// deserializer this failed the whole page with
    /// `invalid type: string "", expected i64`. It must now parse those to
    /// `None` while keeping the well-formed numeric fields intact.
    #[test]
    fn movie_row_with_empty_string_numerics_parses_to_none() {
        let json = r#"{
            "reference_id": 4321,
            "id": 4321,
            "row_id": 4321,
            "started": 1700000000,
            "stopped": 1700006300,
            "duration": 6300,
            "paused_counter": 0,
            "user_id": 55,
            "rating_key": 12345,
            "parent_rating_key": "",
            "grandparent_rating_key": "",
            "media_index": "",
            "parent_media_index": "",
            "year": "",
            "media_type": "movie",
            "title": "Blade Runner 2049",
            "percent_complete": 97,
            "watched_status": 1
        }"#;

        let row: HistoryRow = serde_json::from_str(json).expect("movie row must parse");

        // Well-formed numerics survive.
        assert_eq!(row.reference_id, Some(4321));
        assert_eq!(row.rating_key, Some(12345));
        assert_eq!(row.user_id, Some(55));
        assert_eq!(row.duration, Some(6300));
        assert_eq!(row.percent_complete, Some(97.0));
        assert_eq!(row.watched_status, Some(1.0));

        // Empty-string numerics degrade to None instead of failing the page.
        assert_eq!(row.parent_rating_key, None);
        assert_eq!(row.grandparent_rating_key, None);
        assert_eq!(row.media_index, None);
        assert_eq!(row.parent_media_index, None);
        assert_eq!(row.year, None);
    }

    /// A normal episode row (all parent keys populated) still parses every
    /// numeric field to `Some(n)` — the fix must not regress well-formed rows.
    #[test]
    fn episode_row_with_populated_numerics_parses_to_some() {
        let json = r#"{
            "reference_id": 9001,
            "row_id": 9001,
            "started": 1700100000,
            "stopped": 1700105700,
            "duration": 2700,
            "user_id": 55,
            "rating_key": 88888,
            "parent_rating_key": 88800,
            "grandparent_rating_key": 88000,
            "media_index": 4,
            "parent_media_index": 2,
            "year": 2019,
            "media_type": "episode",
            "title": "The Long Night",
            "percent_complete": 88,
            "watched_status": 1
        }"#;

        let row: HistoryRow = serde_json::from_str(json).expect("episode row must parse");

        assert_eq!(row.rating_key, Some(88888));
        assert_eq!(row.parent_rating_key, Some(88800));
        assert_eq!(row.grandparent_rating_key, Some(88000));
        assert_eq!(row.media_index, Some(4));
        assert_eq!(row.parent_media_index, Some(2));
        assert_eq!(row.year, Some(2019));
    }

    /// Tautulli is also known to stringify otherwise-numeric values (e.g.
    /// `"rating_key": "12345"`) and to send `null`. Both must be accepted:
    /// a numeric string parses, `null` and `""` both become `None`.
    #[test]
    fn numeric_strings_and_null_are_tolerated() {
        let json = r#"{
            "reference_id": "7777",
            "rating_key": "12345",
            "parent_rating_key": null,
            "year": "  ",
            "percent_complete": "50.5"
        }"#;

        let row: HistoryRow = serde_json::from_str(json).expect("mixed row must parse");

        assert_eq!(row.reference_id, Some(7777));
        assert_eq!(row.rating_key, Some(12345));
        assert_eq!(row.parent_rating_key, None); // JSON null
        assert_eq!(row.year, None); // whitespace-only string
        assert_eq!(row.percent_complete, Some(50.5)); // numeric string → f64
    }

    /// BSEED-1: `get_metadata` for a movie carries a bare-string `guids`
    /// array (`imdb://`/`tmdb://`) and a real `year`; both must parse and be
    /// readable via [`MetadataInfo::guids`].
    #[test]
    fn metadata_info_parses_bare_string_guids_and_year() {
        let json = r#"{
            "media_type": "movie",
            "rating_key": 335984,
            "title": "Blade Runner 2049",
            "duration": 9840000,
            "year": 2017,
            "guids": ["imdb://tt1856101", "tmdb://335984"]
        }"#;

        let meta: MetadataInfo = serde_json::from_str(json).expect("movie metadata must parse");
        assert_eq!(meta.year, Some(2017));
        assert_eq!(meta.guids(), vec!["imdb://tt1856101".to_string(), "tmdb://335984".to_string()]);
        assert!(meta.grandparent_guid.is_none());
    }

    /// BSEED-1: some Tautulli/Plex-agent versions emit `guids` as an array of
    /// `{"id": ...}` objects, and an episode carries a `grandparent_guid` for
    /// its owning show. Both shapes must be tolerated.
    #[test]
    fn metadata_info_parses_object_guids_and_grandparent_guid() {
        let json = r#"{
            "media_type": "episode",
            "rating_key": 990001,
            "title": "The Long Night",
            "guids": [{"id": "imdb://tt6027908"}, {"id": "tvdb://7366144"}],
            "grandparent_guid": "tvdb://121361"
        }"#;

        let meta: MetadataInfo = serde_json::from_str(json).expect("episode metadata must parse");
        assert_eq!(meta.guids(), vec!["imdb://tt6027908".to_string(), "tvdb://7366144".to_string()]);
        assert_eq!(meta.grandparent_guid.as_deref(), Some("tvdb://121361"));
    }

    /// An item with no `guids`/`grandparent_guid`/`year` at all still parses
    /// (every field defaulted) — the permissive posture the rest of the
    /// module relies on.
    #[test]
    fn metadata_info_without_guids_parses_to_empty() {
        let meta: MetadataInfo =
            serde_json::from_str(r#"{"media_type": "movie", "duration": 6300000}"#).expect("must parse");
        assert!(meta.guids().is_empty());
        assert!(meta.grandparent_guid.is_none());
        assert!(meta.year.is_none());
    }

    /// A full `get_history` page (envelope + `HistoryData`) containing a
    /// mix of movie and episode rows must deserialize as a whole — this is
    /// the all-or-nothing path that previously imported zero of 1872 rows.
    #[test]
    fn history_page_with_mixed_rows_parses_whole() {
        let json = r#"{
            "response": {
                "result": "success",
                "message": null,
                "data": {
                    "recordsFiltered": 2,
                    "recordsTotal": 2,
                    "data": [
                        {
                            "reference_id": 1,
                            "rating_key": 100,
                            "parent_rating_key": "",
                            "grandparent_rating_key": "",
                            "media_index": "",
                            "year": "",
                            "media_type": "movie"
                        },
                        {
                            "reference_id": 2,
                            "rating_key": 200,
                            "parent_rating_key": 190,
                            "grandparent_rating_key": 180,
                            "media_index": 3,
                            "year": 2021,
                            "media_type": "episode"
                        }
                    ]
                }
            }
        }"#;

        let env: Envelope<HistoryData> =
            serde_json::from_str(json).expect("full history page must parse");
        assert_eq!(env.response.result, "success");
        assert_eq!(env.response.data.records_total, 2);
        assert_eq!(env.response.data.data.len(), 2);
        assert_eq!(env.response.data.data[0].parent_rating_key, None);
        assert_eq!(env.response.data.data[0].year, None);
        assert_eq!(env.response.data.data[1].parent_rating_key, Some(190));
        assert_eq!(env.response.data.data[1].year, Some(2021));
    }
}
