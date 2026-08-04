//! MPRB-08 (Plane MUSE #145): `GET /ops/probe/:id/why` — the operator-facing
//! explanation of what the probe found for ONE media file, and why the system
//! concluded what it concluded.
//!
//! This is where an operator looks when a title unexpectedly needs transcoding,
//! is marked unreadable, or refuses to direct-play — instead of reading logs.
//!
//! # The one rule this module is built around
//!
//! **It explains by CALLING the deciding code. It never restates it.**
//!
//! An explanation endpoint that computes its own answer is the single most
//! dangerous shape in this codebase: it agrees with the real decision in review
//! and diverges in production, and the operator then trusts the wrong one. This
//! repo has already paid for exactly that — `predicted_deletion_refusals`
//! restated the deletion gate instead of calling it and was wrong **by a factor
//! of twenty** (3,158 titles, not 160), caught only by a live end-to-end run,
//! because a restatement is internally consistent and reads correctly.
//!
//! So every line of the `explanation` array below is the *rendering* of a value
//! returned by the sole authority for that rule. Where each one comes from:
//!
//! | Reported rule | Function called | Ultimate authority |
//! |---|---|---|
//! | `probe_state` | [`MediaFile::probe_state_parsed`] | MPRB-05's persisted `StoredProbeState` |
//! | `media_info_document` | [`MediaFile::stored_media_info`] | MPRB-05's one reader |
//! | `direct_play` | [`crate::foundry::directplay::direct_play_blockers`] | itself |
//! | `dynamic_range` | [`crate::media::derive::dynamic_range`] | [`crate::foundry::hdr::classify_hdr`] |
//! | `dolby_vision` | [`crate::media::derive::dolby_vision`] | [`crate::foundry::hdr::classify_dolby_vision`] |
//! | `bit_depth` | [`crate::media::derive::is_10bit`] | [`crate::foundry::hdr::pixel_bit_depth`] |
//! | `resolution_class` | [`crate::media::derive::resolution_class`] | [`crate::foundry::validate::resolution_band`] |
//! | `preservation_worthy_audio` | [`crate::media::derive::has_preservation_worthy_audio`] | `foundry::ladder::PRESERVATION_WORTHY_AUDIO` |
//! | `image_subtitles` | [`crate::media::derive::has_image_subtitles`] and [`crate::media::probe::is_bitmap_subtitle_codec`] | `media::probe::BITMAP_SUBTITLE_CODECS` |
//! | `effective_bitrate` | [`crate::media::derive::effective_bitrate_bps`] | itself |
//! | `suspicion` | [`crate::media::derive::suspicion`] | itself |
//!
//! Every [`Finding::source`] names that function **by path**, in the payload, so
//! an operator reading a surprising answer can go straight to the code that
//! produced it rather than to this file. A `source` that named this module
//! would be the first symptom of the restatement failure, so a test asserts
//! every emitted `source` is a path this file actually calls.
//!
//! This is also the first production caller of MPRB-03's
//! [`crate::media::derive`] accessors, which until now had none — a
//! disconnected-helper risk flagged when they merged. They are wired here.
//!
//! # What it deliberately does NOT do
//!
//! - **It never probes.** It reports the persisted result. A `/why` that
//!   re-probes would answer a different question ("what would a probe say
//!   *now*") than the one the operator is asking ("why does the system believe
//!   what it believes"), and would put an ffprobe spawn behind a GET.
//! - **It never emits `original_file_path`.** No neighbouring endpoint in
//!   `src/web` or `src/http` exposes any file path today (verified by grep
//!   before writing this). `relative_path` — library-relative, the value the
//!   operator needs to find the title — is emitted; the absolute host path is
//!   not, and a test asserts it stays out of the payload.
//!
//! # Auth
//!
//! Registered inside `ops_routes()`, which `crate::http::router` nests under
//! the `protected` sub-router behind [`crate::http::auth::require_api_token`].
//! That is the existing convention for every operator trigger, and it is the
//! right one here: this payload discloses library structure and per-file paths.
//! The route is **not** given a bespoke gate — a second auth convention would be
//! its own hazard.
//!
//! # Route form: `:id`, not `{id}`
//!
//! axum is pinned at **0.7** (`Cargo.toml`). Brace-style path params are a 0.8
//! feature; on 0.7 a `{id}` route silently fails to match the way this repo has
//! already been bitten by (`muse_axum_brace_route_bug`). The pin was re-read
//! against the tree rather than inherited from the spec, which is 91 commits
//! stale and says so in its own banner.

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::Json;
use chrono::{DateTime, Utc};
use serde::Serialize;

use crate::error::MuseError;
use crate::foundry::hdr::HdrVerdict;
use crate::foundry::validate::ResolutionBand;
use crate::http::AppState;
use crate::media::doc::{StoredMediaInfo, StoredProbeState};
use crate::media::probe::MediaProbe;
use crate::models::media_file::MediaFile;

// --- The payload -----------------------------------------------------------

/// The whole answer for one file.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ProbeWhy {
    pub media_file_id: i64,
    pub media_item_id: i64,
    /// Library-relative. See the module doc for why `original_file_path` is
    /// absent.
    pub relative_path: String,
    pub probe: ProbeStatus,
    pub document: DocumentStatus,
    /// One entry per rule the system applied. **Empty** when there is no v1
    /// document to apply them to — an empty explanation is honest; a fabricated
    /// one over an absent probe is not.
    pub explanation: Vec<Finding>,
}

/// What the probe did, from the persisted columns MPRB-05 added.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ProbeStatus {
    /// The distinguishing field. See [`ProbeOutcome`].
    pub outcome: &'static str,
    /// The raw `media_files.probe_state` cell, verbatim — present even when
    /// this binary could not parse it, because "a value I do not understand"
    /// is only actionable if the operator can see the value.
    pub stored_state: Option<String>,
    pub probed_at: Option<DateTime<Utc>>,
    pub attempts: i32,
    /// `probe_error`: the `ProbeError` description for a failure, the suspicion
    /// description for a result that parsed but looks wrong.
    pub error: Option<String>,
    pub summary: String,
}

/// The four states a file can be in, kept **distinguishable**: "never probed"
/// is not "probe failed" is not "unreadable" is not "a state a newer binary
/// wrote". A missing file never reaches here at all — it is a `404` from
/// [`crate::repo::media_file::get`].
///
/// The three probed spellings are **not** re-spelled here: they come from
/// [`StoredProbeState::as_str`], which is MPRB-05's/MPRB-02's vocabulary. Only
/// the two states MPRB-05 cannot express — never probed, and a state this
/// binary does not know — are named in this file.
pub enum ProbeOutcome {}

impl ProbeOutcome {
    /// `probe_state IS NULL` — every row that predates migration 0113 is in
    /// exactly this state, and it means nothing has ever looked at the file.
    pub const NEVER_PROBED: &'static str = "never_probed";
    /// A `probe_state` cell this binary cannot parse: during a rolling deploy
    /// an older binary genuinely sees a value a newer one wrote. Reported as
    /// its own outcome rather than folded into `never_probed`, which would
    /// claim a probe never ran when one did.
    pub const UNRECOGNISED_STATE: &'static str = "unrecognised_state";
}

/// What the `media_info` cell holds, via MPRB-05's one reader.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct DocumentStatus {
    /// `absent` | `legacy` | `v1` | `unknown_version`.
    pub status: &'static str,
    pub schema_version: Option<u16>,
    /// Whether this row is still eligible for the backfill queue — by calling
    /// [`StoredMediaInfo::needs_probe`], not by re-deriving the predicate.
    pub needs_probe: bool,
    pub summary: String,
}

/// One rule, what it concluded, why, and **who decided**.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Finding {
    /// Stable machine code for the rule.
    pub rule: &'static str,
    /// The verdict, rendered.
    pub verdict: String,
    /// The operator-facing reason.
    pub because: String,
    /// The fully-qualified path of the function that produced `verdict`. This
    /// module renders it; it does not decide it.
    pub source: &'static str,
}

impl Finding {
    fn new(rule: &'static str, verdict: impl Into<String>, because: impl Into<String>, source: &'static str) -> Self {
        Self {
            rule,
            verdict: verdict.into(),
            because: because.into(),
            source,
        }
    }
}

// --- The handler -----------------------------------------------------------

/// `GET /ops/probe/:id/why`.
///
/// `404` for a media file id that does not exist (from
/// [`crate::repo::media_file::get`] — this handler does not invent a second
/// not-found path). Every other state is a `200` describing itself.
pub async fn probe_why(
    State(state): State<Arc<AppState>>,
    Path(id): Path<i64>,
) -> Result<Json<ProbeWhy>, MuseError> {
    let file = crate::repo::media_file::get(&state.pool, id).await?;
    Ok(Json(explain(&file)))
}

/// The whole endpoint, minus the row fetch. **Pure**, so every acceptance
/// criterion below the HTTP layer is testable without a database — which
/// matters here because `MUSE_TEST_DATABASE_URL` does not exist on this fleet
/// (#130) and a `db_gated` test would silently skip.
pub fn explain(file: &MediaFile) -> ProbeWhy {
    let stored = file.stored_media_info();
    ProbeWhy {
        media_file_id: file.id,
        media_item_id: file.media_item_id,
        relative_path: file.relative_path.clone(),
        probe: probe_status(file),
        document: document_status(&stored),
        explanation: match stored.as_v1() {
            Some(doc) => findings(&doc.probe),
            None => Vec::new(),
        },
    }
}

fn probe_status(file: &MediaFile) -> ProbeStatus {
    let (outcome, summary) = match (file.probe_state.as_deref(), file.probe_state_parsed()) {
        (None, _) => (
            ProbeOutcome::NEVER_PROBED,
            "no probe has ever run against this file — the row predates migration 0113, or the \
             backfill has not reached it. This is NOT a probe failure and says nothing about the \
             file."
                .to_string(),
        ),
        (Some(_), Some(state @ StoredProbeState::Ok)) => (
            state.as_str(),
            "the probe ran, parsed, and nothing about the result looks wrong.".to_string(),
        ),
        (Some(_), Some(state @ StoredProbeState::Suspicious)) => (
            state.as_str(),
            "the probe ran and PARSED — a document is stored and the rules below were applied to \
             it — but the result describes something implausible. See `probe.error` for which \
             suspicion fired."
                .to_string(),
        ),
        (Some(_), Some(state @ StoredProbeState::Failed(_))) => (
            state.as_str(),
            "the probe produced no usable answer. See `probe.error` for what ffprobe reported; \
             `unreadable` means we never got a verdict (retrying can help), `probe_failed` means \
             we got one and it was unusable (retrying will say the same thing)."
                .to_string(),
        ),
        (Some(_), None) => (
            ProbeOutcome::UNRECOGNISED_STATE,
            "the stored probe state is a value this binary does not know — during a rolling \
             deploy this is a state a NEWER binary wrote. It is reported verbatim in \
             `stored_state` rather than guessed at, and it is NOT the same as never having been \
             probed."
                .to_string(),
        ),
    };
    ProbeStatus {
        outcome,
        stored_state: file.probe_state.clone(),
        probed_at: file.probed_at,
        attempts: file.probe_attempts,
        error: file.probe_error.clone(),
        summary,
    }
}

fn document_status(stored: &StoredMediaInfo) -> DocumentStatus {
    let (status, schema_version, summary) = match stored {
        StoredMediaInfo::Absent => (
            "absent",
            None,
            "`media_info` is NULL. There is nothing to explain — no rule below has been applied."
                .to_string(),
        ),
        StoredMediaInfo::Legacy(_) => (
            "legacy",
            None,
            "`media_info` holds the pre-S130 `{\"container\": \"<ext>\"}` shape, written from the \
             FILENAME and never from the file's contents. It carries no stream data, so no \
             delivery rule can be applied to it."
                .to_string(),
        ),
        StoredMediaInfo::V1(doc) => (
            "v1",
            Some(doc.schema_version),
            "a probe document this binary understands. The rules below were applied to it."
                .to_string(),
        ),
        StoredMediaInfo::UnknownVersion { version } => (
            "unknown_version",
            Some(*version),
            "`media_info` holds a document this binary must not interpret — either written by a \
             NEWER binary, or structurally corrupt at a version we do know. It is deliberately \
             not partially parsed: half a document read under the wrong schema is a wrong answer \
             delivered confidently."
                .to_string(),
        ),
    };
    DocumentStatus {
        status,
        schema_version,
        needs_probe: stored.needs_probe(),
        summary,
    }
}

// --- The rules, each by delegation -----------------------------------------

const SRC_DIRECT_PLAY: &str = "foundry::directplay::direct_play_blockers";
const SRC_DYNAMIC_RANGE: &str = "media::derive::dynamic_range -> foundry::hdr::classify_hdr";
const SRC_DOLBY_VISION: &str = "media::derive::dolby_vision -> foundry::hdr::classify_dolby_vision";
const SRC_BIT_DEPTH: &str = "media::derive::is_10bit -> foundry::hdr::pixel_bit_depth";
const SRC_RESOLUTION: &str = "media::derive::resolution_class -> foundry::validate::resolution_band";
const SRC_AUDIO: &str = "media::derive::has_preservation_worthy_audio";
const SRC_IMAGE_SUBS: &str =
    "media::derive::has_image_subtitles -> media::probe::is_bitmap_subtitle_codec";
const SRC_BITRATE: &str = "media::derive::effective_bitrate_bps";
const SRC_SUSPICION: &str = "media::derive::suspicion";

/// Every `source` string this module may emit. The test
/// `every_emitted_source_names_a_function_this_file_calls` walks this list
/// against the file's own text, so a `source` can never drift into naming a
/// function that is no longer called — which is what a restatement would look
/// like from the outside.
const ALL_SOURCES: &[&str] = &[
    SRC_DIRECT_PLAY,
    SRC_DYNAMIC_RANGE,
    SRC_DOLBY_VISION,
    SRC_BIT_DEPTH,
    SRC_RESOLUTION,
    SRC_AUDIO,
    SRC_IMAGE_SUBS,
    SRC_BITRATE,
    SRC_SUSPICION,
];

fn findings(probe: &MediaProbe) -> Vec<Finding> {
    let mut out = Vec::new();
    out.extend(direct_play_findings(probe));
    out.extend(dynamic_range_findings(probe));
    out.push(resolution_finding(probe));
    out.push(audio_finding(probe));
    out.extend(subtitle_findings(probe));
    out.push(bitrate_finding(probe));
    out.push(suspicion_finding(probe));
    out
}

/// Why this file would or would not direct-play.
///
/// The list comes from [`crate::foundry::directplay::direct_play_blockers`] and
/// the prose from each blocker's own `Display` — so the text an operator reads
/// here is character-for-character the text the planner's own reporting uses.
/// There is no second wording and no second set of thresholds.
fn direct_play_findings(probe: &MediaProbe) -> Vec<Finding> {
    // PATH A's policy, not the default. `TranscodePolicy::default()` caps at
    // 1080p/12Mbps and would report blockers the production path does not have
    // — the exact mismatch `foundry::policy`'s
    // `the_production_endpoints_use_path_as_policy_not_the_default` test exists
    // to prevent. A guard test in this file holds this call site to the same
    // rule, because that test scans `dashboard.rs` only.
    let policy = crate::foundry::policy::TranscodePolicy::direct_play_normalization();
    let blockers = crate::foundry::directplay::direct_play_blockers(probe, &policy);
    if blockers.is_empty() {
        return vec![Finding::new(
            "direct_play",
            "no_blocker_found",
            "no blocker in the known set was found. This is NOT a promise of direct play: facts \
             we cannot observe (see foundry::hdr::undetectable_formats) are not in the set.",
            SRC_DIRECT_PLAY,
        )];
    }
    blockers
        .iter()
        .map(|b| {
            Finding::new(
                "direct_play",
                blocker_code(b),
                // The blocker's OWN Display. Not a paraphrase.
                b.to_string(),
                SRC_DIRECT_PLAY,
            )
        })
        .collect()
}

/// A stable machine token per blocker variant.
///
/// This is a rendering of a value that has already been decided — it applies no
/// threshold and reads no field that participates in the decision. The `match`
/// is wildcard-free so a new blocker variant is a compile error here rather
/// than silently arriving as some catch-all string.
fn blocker_code(b: &crate::foundry::directplay::DirectPlayBlocker) -> &'static str {
    use crate::foundry::directplay::DirectPlayBlocker as B;
    match b {
        B::VideoCodecNotWidelySupported { .. } => "video_codec_not_widely_supported",
        B::HighBitDepthH264 { .. } => "high_bit_depth_h264",
        B::ContainerNotStreamable { .. } => "container_not_streamable",
        B::AudioCodecNotWidelySupported { .. } => "audio_codec_not_widely_supported",
        B::AudioChannelsAboveClientCeiling { .. } => "audio_channels_above_client_ceiling",
        B::DefaultBitmapSubtitles { .. } => "default_bitmap_subtitles",
        B::ResolutionAboveCeiling { .. } => "resolution_above_ceiling",
    }
}

/// Dynamic range, Dolby Vision and bit depth — three independent axes, reported
/// as three findings because collapsing them is how a 10-bit SDR file gets
/// tone-mapped.
fn dynamic_range_findings(probe: &MediaProbe) -> Vec<Finding> {
    let mut out = Vec::new();

    match crate::media::derive::dynamic_range(probe) {
        None => out.push(Finding::new(
            "dynamic_range",
            "no_video_stream",
            "there is no non-cover-art video stream, so there is no dynamic range to classify.",
            SRC_DYNAMIC_RANGE,
        )),
        Some(HdrVerdict::Sdr) => out.push(Finding::new(
            "dynamic_range",
            "sdr",
            "proven standard dynamic range — no tone-map is needed or wanted.",
            SRC_DYNAMIC_RANGE,
        )),
        Some(HdrVerdict::Hdr { transfer }) => out.push(Finding::new(
            "dynamic_range",
            format!("hdr:{}", transfer.as_str()),
            format!(
                "proven high dynamic range (transfer `{}`, nominal peak {} nits) — an \
                 SDR-targeted rendition must tone-map.",
                transfer.as_str(),
                transfer.nominal_peak_nits()
            ),
            SRC_DYNAMIC_RANGE,
        )),
        // The `why` is the authority's own Display — this endpoint does not
        // paraphrase why something could not be established.
        Some(HdrVerdict::Unknown { why }) => out.push(Finding::new(
            "dynamic_range",
            "unknown",
            why.to_string(),
            SRC_DYNAMIC_RANGE,
        )),
    }

    match crate::media::derive::dolby_vision(probe) {
        None => out.push(Finding::new(
            "dolby_vision",
            "no_video_stream",
            "there is no non-cover-art video stream to inspect for a Dolby Vision signal.",
            SRC_DOLBY_VISION,
        )),
        Some(v) => out.push(Finding::new(
            "dolby_vision",
            if v.is_present() { "present" } else { "not_detected" },
            v.to_string(),
            SRC_DOLBY_VISION,
        )),
    }

    match crate::media::derive::is_10bit(probe) {
        None => out.push(Finding::new(
            "bit_depth",
            "undecided",
            "the bit depth could not be established from either `bits_per_raw_sample` or the \
             pixel format. A client-capability check must treat this as undecided, never as \
             8-bit.",
            SRC_BIT_DEPTH,
        )),
        Some(true) => out.push(Finding::new(
            "bit_depth",
            "above_8_bit",
            "the primary video stream carries more than 8 bits per component. Independent of the \
             HDR axis above — 10-bit SDR is common.",
            SRC_BIT_DEPTH,
        )),
        Some(false) => out.push(Finding::new(
            "bit_depth",
            "8_bit",
            "the primary video stream carries 8 bits per component.",
            SRC_BIT_DEPTH,
        )),
    }

    out
}

fn resolution_finding(probe: &MediaProbe) -> Finding {
    let band = crate::media::derive::resolution_class(probe);
    let because = match band {
        ResolutionBand::Unknown =>
            "ffprobe gave no usable dimensions (absent, or literally zero), or there is no video \
             stream. Its own band — the planner will refuse a file it cannot measure.",
        _ => "banded from the primary video stream's WIDTH, against boundaries drawn around this \
              library's measured sample (1918-wide scope releases land as 1080p, not 720p).",
    };
    Finding::new("resolution_class", resolution_code(band), because, SRC_RESOLUTION)
}

/// Wildcard-free rendering of an already-decided band.
fn resolution_code(band: ResolutionBand) -> &'static str {
    match band {
        ResolutionBand::Tiny => "tiny",
        ResolutionBand::Sd => "sd",
        ResolutionBand::Hd720 => "hd720",
        ResolutionBand::Hd1080 => "hd1080",
        ResolutionBand::Uhd => "uhd",
        ResolutionBand::Unknown => "unknown",
    }
}

fn audio_finding(probe: &MediaProbe) -> Finding {
    if crate::media::derive::has_preservation_worthy_audio(probe) {
        Finding::new(
            "preservation_worthy_audio",
            "present",
            "at least one audio track is lossless or object-bearing (foundry::ladder's \
             PRESERVATION_WORTHY_AUDIO list — which includes `dts`, so this is NOT the same claim \
             as `lossless`). Re-encoding it is a lossy, one-way change.",
            SRC_AUDIO,
        )
    } else {
        Finding::new(
            "preservation_worthy_audio",
            "absent",
            "no audio track is on foundry::ladder's preservation-worthy list.",
            SRC_AUDIO,
        )
    }
}

/// Bitmap subtitles: the whole-file answer, plus the per-stream detail that
/// makes the direct-play blocker above legible.
///
/// The per-stream loop calls [`crate::media::probe::is_bitmap_subtitle_codec`]
/// — the single home of that rule since SUBCODEC-01 — and reports the
/// `default`/`forced` dispositions as observed. It does **not** re-derive which
/// combination forces a burn-in; that is the direct-play blocker's job and it
/// is already reported by [`direct_play_findings`].
fn subtitle_findings(probe: &MediaProbe) -> Vec<Finding> {
    let mut out = vec![if crate::media::derive::has_image_subtitles(probe) {
        Finding::new(
            "image_subtitles",
            "present",
            "at least one subtitle track is bitmap-based (PGS/VobSub/DVB). Note: an UNKNOWN \
             subtitle codec reports as NOT bitmap — the predicate does not fail closed, and this \
             endpoint does not override it.",
            SRC_IMAGE_SUBS,
        )
    } else {
        Finding::new(
            "image_subtitles",
            "absent",
            "no subtitle track matched the bitmap codec list. An unrecognised codec also lands \
             here.",
            SRC_IMAGE_SUBS,
        )
    }];

    for s in &probe.subtitles {
        if crate::media::probe::is_bitmap_subtitle_codec(&s.codec) {
            out.push(Finding::new(
                "image_subtitles",
                format!("stream_{}", s.index),
                format!(
                    "subtitle stream {} is bitmap-based (`{}`), default={}, forced={}.",
                    s.index, s.codec, s.default, s.forced
                ),
                SRC_IMAGE_SUBS,
            ));
        }
    }
    out
}

fn bitrate_finding(probe: &MediaProbe) -> Finding {
    use crate::media::derive::BitrateSource;
    match crate::media::derive::effective_bitrate_bps(probe) {
        None => Finding::new(
            "effective_bitrate",
            "undecided",
            "no container bitrate, no complete set of per-stream bitrates, and no usable \
             size/duration pair — the file's overall bitrate could not be established.",
            SRC_BITRATE,
        ),
        Some(b) => Finding::new(
            "effective_bitrate",
            format!("{}", b.bps),
            match b.source {
                BitrateSource::Container => "from the container's own `format.bit_rate`.",
                BitrateSource::SumOfStreams => {
                    "summed from the per-stream bitrates — every modelled stream reported one. \
                     Omits any stream we do not model."
                }
                BitrateSource::SizeOverDuration => {
                    "computed as size*8/duration — an average over the whole file INCLUDING \
                     container overhead, so it reads slightly high. A ceiling applied to this as \
                     if it were the container figure rejects files that are inside it."
                }
            },
            SRC_BITRATE,
        ),
    }
}

/// The live suspicion verdict, recomputed from the stored document.
///
/// Deliberately reported alongside the PERSISTED `probe.outcome` rather than
/// instead of it. They can legitimately disagree — the persisted value was
/// written when the file was probed, and the rule may have changed since — and
/// that disagreement is exactly the kind of thing an operator opens this
/// endpoint to see. Neither value is computed here; one is read from the row,
/// the other from [`crate::media::derive::suspicion`].
fn suspicion_finding(probe: &MediaProbe) -> Finding {
    match crate::media::derive::suspicion(probe) {
        None => Finding::new(
            "suspicion",
            "none",
            "re-applying the suspicion rule to the stored document finds nothing implausible \
             about it. If `probe.outcome` says `suspicious`, the rule has changed since this file \
             was probed.",
            SRC_SUSPICION,
        ),
        Some(s) => Finding::new(
            "suspicion",
            s.as_str(),
            "re-applying the suspicion rule to the stored document flags it. This is a statement \
             about a SUCCESSFUL parse — ffprobe answered and the answer does not hang together — \
             never about a probe failure.",
            SRC_SUSPICION,
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::doc::MediaInfoDoc;
    use crate::media::probe::{AudioStream, SubtitleStream, VideoStream};
    use crate::models::media_file::ReleaseTypeKind;

    // --- fixtures ----------------------------------------------------------

    fn base_probe() -> MediaProbe {
        MediaProbe {
            container: "matroska,webm".to_string(),
            duration_secs: Some(3600.0),
            format_bitrate_bps: Some(8_000_000),
            size_bytes: Some(4_000_000_000),
            video: vec![VideoStream {
                index: 0,
                codec: "hevc".to_string(),
                width: Some(1920),
                height: Some(1080),
                pix_fmt: Some("yuv420p".to_string()),
                color_transfer: Some("bt709".to_string()),
                ..Default::default()
            }],
            audio: vec![AudioStream {
                index: 1,
                codec: "eac3".to_string(),
                channels: Some(6),
                ..Default::default()
            }],
            // `MediaProbe` has no `Default` — every field is named here on
            // purpose, so a field added to it is a compile error in this
            // fixture rather than a silent zero that could make a test vacuous.
            subtitles: vec![],
            attachments: vec![],
            data_stream_count: 0,
            unindexed_stream_count: 0,
            chapter_count: 0,
            title: None,
            other_stream_count: 0,
            notes: vec![],
        }
    }

    fn file_with(media_info: Option<serde_json::Value>) -> MediaFile {
        MediaFile {
            id: 7,
            media_item_id: 42,
            relative_path: "Movies/Some Film (1999)/Some Film (1999).mkv".to_string(),
            original_file_path: Some("/mnt/qnap/Media/Movies/Some Film (1999).mkv".to_string()),
            size_bytes: Some(4_000_000_000),
            date_added: None,
            scene_name: None,
            media_info,
            media_info_version: None,
            probed_at: None,
            probe_state: None,
            probe_error: None,
            probe_attempts: 0,
            release_group: None,
            edition: None,
            languages: vec![],
            subtitles: vec![],
            indexer_flags: 0,
            release_type: ReleaseTypeKind::Single,
            quality_tier_id: None,
            revision_version: 1,
            revision_real: 0,
            revision_is_repack: false,
            created_at: Utc::now(),
        }
    }

    fn file_with_probe(probe: MediaProbe) -> MediaFile {
        let doc = MediaInfoDoc::new(probe, "Movies/Some Film (1999)/Some Film (1999).mkv");
        let mut f = file_with(Some(doc.to_json().unwrap()));
        f.media_info_version = Some(1);
        f.probe_state = Some("ok".to_string());
        f.probed_at = Some(Utc::now());
        f
    }

    fn finding<'a>(why: &'a ProbeWhy, rule: &str) -> &'a Finding {
        why.explanation
            .iter()
            .find(|f| f.rule == rule)
            .unwrap_or_else(|| panic!("no `{rule}` finding in {:?}", why.explanation))
    }

    fn findings_for<'a>(why: &'a ProbeWhy, rule: &str) -> Vec<&'a Finding> {
        why.explanation.iter().filter(|f| f.rule == rule).collect()
    }

    // --- the state taxonomy stays distinguishable --------------------------

    /// "never probed" is not "probe failed" is not "unreadable" is not
    /// "suspicious" is not "a state a newer binary wrote". Five inputs, five
    /// distinct outcomes — asserted as a SET, so collapsing any two fails.
    #[test]
    fn every_probe_state_gets_its_own_distinguishable_outcome() {
        let cases = [
            (None, ProbeOutcome::NEVER_PROBED),
            (Some("ok"), "ok"),
            (Some("suspicious"), "suspicious"),
            (Some("unreadable"), "unreadable"),
            (Some("probe_failed"), "probe_failed"),
            (Some("quarantined_by_a_newer_binary"), ProbeOutcome::UNRECOGNISED_STATE),
        ];
        let mut seen = std::collections::BTreeSet::new();
        for (stored, expected) in cases {
            let mut f = file_with(None);
            f.probe_state = stored.map(str::to_string);
            let why = explain(&f);
            assert_eq!(
                why.probe.outcome, expected,
                "stored probe_state {stored:?} must report outcome `{expected}`"
            );
            assert!(
                seen.insert(why.probe.outcome),
                "outcome `{}` was produced by two different stored states — the taxonomy has \
                 collapsed",
                why.probe.outcome
            );
            // The summaries must differ too: an operator reads prose, not codes.
            assert!(!why.probe.summary.is_empty());
        }
        assert_eq!(seen.len(), 6);
    }

    /// The unparseable state is reported VERBATIM, not swallowed. An operator
    /// cannot act on "a value I do not understand" unless they can see it.
    #[test]
    fn an_unrecognised_state_is_echoed_verbatim() {
        let mut f = file_with(None);
        f.probe_state = Some("quarantined_by_a_newer_binary".to_string());
        let why = explain(&f);
        assert_eq!(why.probe.outcome, ProbeOutcome::UNRECOGNISED_STATE);
        assert_eq!(
            why.probe.stored_state.as_deref(),
            Some("quarantined_by_a_newer_binary")
        );
    }

    /// Never-probed and legacy/absent rows produce NO findings. A fabricated
    /// explanation over an absent probe is the failure this endpoint exists to
    /// prevent.
    #[test]
    fn no_document_means_no_findings_rather_than_invented_ones() {
        for media_info in [
            None,
            Some(serde_json::json!({ "container": "mkv" })),
            Some(serde_json::json!({ "schema_version": 99 })),
        ] {
            let why = explain(&file_with(media_info));
            assert!(
                why.explanation.is_empty(),
                "an explanation was produced without a v1 document: {:?}",
                why.explanation
            );
        }
    }

    #[test]
    fn document_status_distinguishes_all_four_cell_shapes() {
        let cases: [(Option<serde_json::Value>, &str, bool); 3] = [
            (None, "absent", true),
            (Some(serde_json::json!({ "container": "mkv" })), "legacy", true),
            (Some(serde_json::json!({ "schema_version": 99 })), "unknown_version", false),
        ];
        for (media_info, status, needs_probe) in cases {
            let why = explain(&file_with(media_info));
            assert_eq!(why.document.status, status);
            assert_eq!(why.document.needs_probe, needs_probe);
        }
        let why = explain(&file_with_probe(base_probe()));
        assert_eq!(why.document.status, "v1");
        assert_eq!(why.document.schema_version, Some(1));
        assert!(!why.document.needs_probe);
    }

    // --- the endpoint AGREES with the deciding code -------------------------

    /// The load-bearing test. For a file with real blockers, the reported
    /// `direct_play` findings must equal — one for one, and in the deciding
    /// code's own words — what `direct_play_blockers` returns under PATH A's
    /// policy. A restatement would pass a hand-written expectation and fail
    /// this one.
    #[test]
    fn direct_play_findings_are_exactly_what_the_authority_returns() {
        let mut p = base_probe();
        p.video[0].codec = "vc1".to_string();
        p.audio[0].codec = "truehd".to_string();
        p.audio[0].channels = Some(8);

        let policy = crate::foundry::policy::TranscodePolicy::direct_play_normalization();
        let expected = crate::foundry::directplay::direct_play_blockers(&p, &policy);
        assert!(
            expected.len() >= 2,
            "fixture must actually produce blockers or this test is vacuous; got {expected:?}"
        );

        let why = explain(&file_with_probe(p));
        let got = findings_for(&why, "direct_play");
        assert_eq!(got.len(), expected.len());
        for (f, b) in got.iter().zip(expected.iter()) {
            assert_eq!(f.because, b.to_string(), "prose must be the blocker's OWN Display");
            assert_eq!(f.source, SRC_DIRECT_PLAY);
        }
    }

    /// The no-blocker case must not be reported as a promise of direct play.
    #[test]
    fn a_clean_file_reports_no_blocker_found_and_refuses_to_promise() {
        let mut p = base_probe();
        p.container = "matroska,webm".to_string();
        p.video[0].codec = "h264".to_string();
        p.audio[0].codec = "aac".to_string();
        p.audio[0].channels = Some(2);
        p.subtitles.clear();

        let policy = crate::foundry::policy::TranscodePolicy::direct_play_normalization();
        assert!(
            crate::foundry::directplay::direct_play_blockers(&p, &policy).is_empty(),
            "fixture is not actually clean, so this test proves nothing"
        );

        let why = explain(&file_with_probe(p));
        let f = finding(&why, "direct_play");
        assert_eq!(f.verdict, "no_blocker_found");
        assert!(
            f.because.contains("NOT a promise"),
            "an empty blocker list must not read as a guarantee: {}",
            f.because
        );
    }

    /// PATH A's policy, not the default. `TranscodePolicy::default()` caps at
    /// 1080p; Path A's at 4K. A 4K file must therefore NOT report a
    /// resolution-ceiling blocker — which is a behavioural check on the policy
    /// choice, not a text scan.
    #[test]
    fn the_endpoint_uses_path_as_policy_not_the_default() {
        let mut p = base_probe();
        p.video[0].width = Some(3840);
        p.video[0].height = Some(2160);

        // Prove the two policies actually disagree here, or the test is
        // guarding a distinction without a difference.
        let d = crate::foundry::policy::TranscodePolicy::default();
        assert!(
            !crate::foundry::directplay::direct_play_blockers(&p, &d).is_empty(),
            "the default policy must reject this fixture, or the assertion below is vacuous"
        );

        let why = explain(&file_with_probe(p));
        assert!(
            !findings_for(&why, "direct_play")
                .iter()
                .any(|f| f.verdict == "resolution_above_ceiling"),
            "4K must not be reported as above the ceiling — that is the DEFAULT policy's answer, \
             not the production path's"
        );
    }

    /// HDR must never be reported as SDR when it could not be established, and
    /// the reason must be the authority's own words.
    #[test]
    fn an_unestablished_dynamic_range_reports_unknown_not_sdr() {
        let mut p = base_probe();
        p.video[0].color_transfer = None;
        p.video[0].pix_fmt = None;
        p.video[0].bits_per_raw_sample = None;

        // Ground truth from the authority.
        let verdict = crate::media::derive::dynamic_range(&p).unwrap();
        assert!(
            matches!(verdict, HdrVerdict::Unknown { .. }),
            "fixture must actually be undecidable; got {verdict:?}"
        );

        let why = explain(&file_with_probe(p));
        let f = finding(&why, "dynamic_range");
        assert_eq!(f.verdict, "unknown");
        assert_ne!(f.verdict, "sdr");
        assert_eq!(f.source, SRC_DYNAMIC_RANGE);
        let HdrVerdict::Unknown { why: reason } = verdict else {
            unreachable!()
        };
        assert_eq!(f.because, reason.to_string());
    }

    #[test]
    fn a_proven_hdr_file_reports_its_transfer() {
        let mut p = base_probe();
        p.video[0].color_transfer = Some("smpte2084".to_string());
        p.video[0].pix_fmt = Some("yuv420p10le".to_string());

        assert!(
            matches!(
                crate::media::derive::dynamic_range(&p),
                Some(HdrVerdict::Hdr { .. })
            ),
            "fixture must actually be HDR"
        );

        let why = explain(&file_with_probe(p));
        assert_eq!(finding(&why, "dynamic_range").verdict, "hdr:pq");
        assert_eq!(finding(&why, "bit_depth").verdict, "above_8_bit");
    }

    /// Dolby Vision prose is the verdict's own `Display` — the same sentence
    /// the deletion gate's reporting shows.
    #[test]
    fn dolby_vision_prose_comes_from_the_verdict_itself() {
        let mut p = base_probe();
        p.video[0].codec_tag = Some("dvhe".to_string());

        let verdict = crate::media::derive::dolby_vision(&p).unwrap();
        assert!(
            verdict.is_present(),
            "fixture must actually signal Dolby Vision; got {verdict:?}"
        );

        let why = explain(&file_with_probe(p));
        let f = finding(&why, "dolby_vision");
        assert_eq!(f.verdict, "present");
        assert_eq!(f.because, verdict.to_string());
    }

    #[test]
    fn bit_depth_is_undecided_rather_than_eight_when_it_cannot_be_established() {
        let mut p = base_probe();
        p.video[0].pix_fmt = None;
        p.video[0].bits_per_raw_sample = None;
        assert_eq!(crate::media::derive::is_10bit(&p), None);

        let why = explain(&file_with_probe(p));
        let f = finding(&why, "bit_depth");
        assert_eq!(f.verdict, "undecided");
        assert_ne!(f.verdict, "8_bit");
    }

    /// The resolution band reported must be the band `resolution_class`
    /// returns, for every band — including `unknown`, which must stay its own
    /// band rather than being folded into a real one.
    #[test]
    fn resolution_band_matches_the_authority_for_every_band() {
        let cases = [
            (Some(320u32), Some(240u32), "tiny"),
            (Some(720), Some(480), "sd"),
            (Some(1280), Some(720), "hd720"),
            (Some(1918), Some(802), "hd1080"),
            (Some(3840), Some(2160), "uhd"),
            (Some(0), Some(0), "unknown"),
        ];
        for (w, h, expected) in cases {
            let mut p = base_probe();
            p.video[0].width = w;
            p.video[0].height = h;
            assert_eq!(
                resolution_code(crate::media::derive::resolution_class(&p)),
                expected,
                "the authority disagrees with the fixture's expectation for {w:?}x{h:?}"
            );
            let why = explain(&file_with_probe(p));
            assert_eq!(finding(&why, "resolution_class").verdict, expected);
        }
    }

    #[test]
    fn preservation_worthy_audio_tracks_the_authority_both_ways() {
        for (codec, expected) in [("truehd", "present"), ("aac", "absent")] {
            let mut p = base_probe();
            p.audio[0].codec = codec.to_string();
            assert_eq!(
                crate::media::derive::has_preservation_worthy_audio(&p),
                expected == "present",
                "the authority disagrees with the fixture's expectation for `{codec}`"
            );
            let why = explain(&file_with_probe(p));
            assert_eq!(finding(&why, "preservation_worthy_audio").verdict, expected);
        }
    }

    /// A bitmap subtitle track is listed per-stream with its dispositions, and
    /// the whole-file answer agrees with `has_image_subtitles`.
    #[test]
    fn bitmap_subtitles_are_listed_per_stream_and_agree_with_the_authority() {
        let mut p = base_probe();
        p.subtitles = vec![
            SubtitleStream {
                index: 2,
                codec: "hdmv_pgs_subtitle".to_string(),
                language: Some("eng".to_string()),
                forced: false,
                default: true,
                hearing_impaired: false,
            },
            SubtitleStream {
                index: 3,
                codec: "subrip".to_string(),
                language: Some("eng".to_string()),
                forced: false,
                default: false,
                hearing_impaired: false,
            },
        ];
        assert!(crate::media::probe::is_bitmap_subtitle_codec("hdmv_pgs_subtitle"));
        assert!(!crate::media::probe::is_bitmap_subtitle_codec("subrip"));

        let why = explain(&file_with_probe(p));
        let subs = findings_for(&why, "image_subtitles");
        assert_eq!(subs[0].verdict, "present");
        // Exactly one per-stream entry: the PGS track, not the subrip one.
        let per_stream: Vec<&str> = subs[1..].iter().map(|f| f.verdict.as_str()).collect();
        assert_eq!(per_stream, vec!["stream_2"]);
        assert!(subs[1].because.contains("default=true"));
    }

    /// An unknown subtitle codec reports as NOT bitmap, and the prose SAYS SO.
    /// The predicate does not fail closed and this endpoint must not pretend it
    /// does — describing the behaviour as safer than it is would be its own
    /// claim-outruns-mechanism defect.
    #[test]
    fn an_unknown_subtitle_codec_is_reported_absent_and_the_caveat_is_stated() {
        let mut p = base_probe();
        p.subtitles = vec![SubtitleStream {
            index: 2,
            codec: "some_future_bitmap_format".to_string(),
            language: None,
            forced: false,
            default: true,
            hearing_impaired: false,
        }];
        assert!(!crate::media::derive::has_image_subtitles(&p));

        let why = explain(&file_with_probe(p));
        let f = finding(&why, "image_subtitles");
        assert_eq!(f.verdict, "absent");
        assert!(f.because.contains("unrecognised"), "{}", f.because);
    }

    /// The bitrate finding must name its SOURCE tier, because the three are not
    /// equally trustworthy.
    #[test]
    fn the_bitrate_finding_names_which_tier_produced_it() {
        let mut p = base_probe();
        let why = explain(&file_with_probe(p.clone()));
        assert_eq!(finding(&why, "effective_bitrate").verdict, "8000000");
        assert!(finding(&why, "effective_bitrate")
            .because
            .contains("format.bit_rate"));

        // Drop the container figure: falls through to size/duration, whose
        // prose must warn that it reads high.
        p.format_bitrate_bps = None;
        let why = explain(&file_with_probe(p.clone()));
        let f = finding(&why, "effective_bitrate");
        assert!(f.because.contains("size*8/duration"), "{}", f.because);

        // Nothing to compute from at all.
        p.duration_secs = None;
        p.size_bytes = None;
        assert_eq!(crate::media::derive::effective_bitrate_bps(&p), None);
        let why = explain(&file_with_probe(p));
        assert_eq!(finding(&why, "effective_bitrate").verdict, "undecided");
    }

    /// A stored `ok` state and a live `suspicion` hit are reported SIDE BY
    /// SIDE, not reconciled. Silently overwriting either would hide the drift
    /// an operator opened this endpoint to find.
    #[test]
    fn a_live_suspicion_is_reported_alongside_the_persisted_state_not_instead_of_it() {
        let mut p = base_probe();
        p.duration_secs = Some(0.0);
        assert_eq!(
            crate::media::derive::suspicion(&p),
            Some(crate::media::derive::Suspicion::ZeroDuration)
        );

        let mut f = file_with_probe(p);
        f.probe_state = Some("ok".to_string());
        let why = explain(&f);
        assert_eq!(why.probe.outcome, "ok", "the PERSISTED state must survive");
        assert_eq!(finding(&why, "suspicion").verdict, "zero_duration");
    }

    #[test]
    fn a_clean_file_reports_no_suspicion() {
        let p = base_probe();
        assert_eq!(crate::media::derive::suspicion(&p), None);
        let why = explain(&file_with_probe(p));
        assert_eq!(finding(&why, "suspicion").verdict, "none");
    }

    // --- structural guards --------------------------------------------------

    /// The absolute host path must never reach the payload. The fixture carries
    /// one, so adding an `original_file_path` field (or serialising the whole
    /// `MediaFile`) fails here.
    #[test]
    fn the_absolute_host_path_never_reaches_the_payload() {
        let why = explain(&file_with_probe(base_probe()));
        let json = serde_json::to_string(&why).unwrap();
        assert!(
            !json.contains("/mnt/qnap"),
            "the absolute host path leaked into the payload: {json}"
        );
        assert!(
            json.contains("Movies/Some Film (1999)"),
            "the library-relative path is what the operator needs and must be present"
        );
    }

    /// Every `source` string this module emits must name a function this file
    /// actually CALLS. A `source` naming a function that is no longer called is
    /// what a restatement looks like from the outside.
    #[test]
    fn every_emitted_source_names_a_function_this_file_calls() {
        let me = include_str!("probe_why.rs");
        for source in ALL_SOURCES {
            // `a::b::c -> d::e::f` — the FIRST path is the one this file calls;
            // the second is that function's own authority.
            let called = source.split(" -> ").next().unwrap();
            assert!(
                me.contains(&format!("crate::{called}(")),
                "`{source}` is emitted but `crate::{called}(...)` is not called in this file — \
                 the endpoint would be restating the rule rather than calling it"
            );
        }
    }

    /// The `ALL_SOURCES` list must cover every source actually emitted — a
    /// source outside the list would escape the guard above.
    #[test]
    fn all_sources_covers_every_source_a_finding_can_carry() {
        let mut p = base_probe();
        p.video[0].codec = "vc1".to_string();
        p.subtitles = vec![SubtitleStream {
            index: 2,
            codec: "dvd_subtitle".to_string(),
            language: None,
            forced: true,
            default: false,
            hearing_impaired: false,
        }];
        let why = explain(&file_with_probe(p));
        assert!(why.explanation.len() >= 8, "{:?}", why.explanation);
        for f in &why.explanation {
            assert!(
                ALL_SOURCES.contains(&f.source),
                "finding `{}` carries source `{}`, which is not in ALL_SOURCES",
                f.rule,
                f.source
            );
        }
    }

    /// The route must be registered with axum 0.7's `:id` form, inside
    /// `ops_routes()` — which `router()` nests under the auth-gated `protected`
    /// sub-router. A `{id}` route would silently fail to match on 0.7, and a
    /// route registered outside `ops_routes` would be unauthenticated.
    #[test]
    fn the_route_is_colon_form_and_registered_inside_the_auth_gated_ops_router() {
        assert!(
            include_str!("../../Cargo.toml").contains("axum = { version = \"0.7\""),
            "the axum pin moved — re-check the path-parameter syntax before trusting this test"
        );

        let m = include_str!("mod.rs");
        assert!(
            m.contains("\"/probe/:id/why\""),
            "the route must use axum 0.7's `:id` form"
        );
        assert!(
            !m.contains("/probe/{id}/why"),
            "brace-style path params are axum 0.8; on 0.7 they silently do not match"
        );

        let ops = m
            .split_once("fn ops_routes()")
            .expect("ops_routes not found")
            .1;
        let ops = ops.split_once("\nfn ").map(|(a, _)| a).unwrap_or(ops);
        assert!(
            ops.contains("\"/probe/:id/why\""),
            "the route must live in ops_routes(), which router() nests under the auth-gated \
             `protected` router — this payload discloses library structure"
        );
        assert!(
            m.contains(".nest(\"/ops\", ops_routes())"),
            "ops_routes() must still be nested under /ops on the protected router"
        );
    }
}
