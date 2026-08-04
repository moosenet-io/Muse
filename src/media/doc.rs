//! **The stored probe document** — `media_files.media_info`, versioned, with
//! exactly one typed reader.
//!
//! ## What this module is for
//! [`super::probe::MediaProbe`] is an *ephemeral observation*: what `ffprobe`
//! said about a file at one moment. [`MediaInfoDoc`] is the *library fact*: that
//! observation wrapped in an envelope carrying a schema version, plus the flat
//! keys the GUI already reads and the one fact the probe cannot know (the
//! filename's extension). Two types, deliberately — see the naming decision
//! recorded in [`super`].
//!
//! ## One reader, and why that is a rule rather than a style
//! `media_info` is `jsonb`. Every caller reads it through
//! [`crate::models::media_file::MediaFile::stored_media_info`] and nothing else;
//! a test in this file greps `src/` and fails the build on ad-hoc key access.
//! This repo has already paid for restated logic once, expensively:
//! `predicted_deletion_refusals` restated the deletion rule instead of calling
//! it and was wrong **by 20x**, caught only by a live run. A convention survives
//! until the next hurried change. A failing test does not.
//!
//! ## What this module deliberately does NOT do
//! It does not classify probe failures. [`super::probe::ProbeError::state`] and
//! [`super::probe::ProbeError::is_retryable`] shipped in MPRB-02 and are the
//! only classification of those errors that exists. [`StoredProbeState`] wraps
//! that enum rather than re-listing its cases, so the two cannot drift — see the
//! type's own docs.

use serde::{Deserialize, Serialize};
use serde_json::Value as Json;

use crate::foundry::policy::normalize_container;
use crate::media::probe::{MediaProbe, ProbeError, ProbeState};

/// The version stamped into every document this binary writes.
///
/// Bumping this is a real migration event, not a constant edit: the backfill
/// queue's partial index (`migrations/0113_media_files_probe.sql`) hard-codes
/// `media_info_version < 1`, because a partial-index predicate must be constant.
/// A bump therefore needs a new migration adding a `< 2` index, or the re-probe
/// sweep seq-scans the whole table. (The *query's* predicate is generated from
/// this constant by [`probe_queue_predicate`] and moves on its own; the migration
/// is the one artefact SQL forces you to write by hand, which is why
/// `the_queue_index_predicate_matches_the_current_schema_version` guards it.)
///
/// **Bump it whenever [`MediaProbe`]'s shape changes.** The document embeds that
/// type verbatim via serde, which is what keeps this module from restating its
/// field list — the cost of that choice is that a field added there changes what
/// a v1 document contains.
pub const MEDIA_INFO_SCHEMA_VERSION: u16 = 1;

/// **The one definition of "this row still needs a probe."**
///
/// It is stated as a function of the only fact both sides of the system can see:
/// the version the row *claims*. `None` means the row makes no version claim at
/// all — the cell is absent, or it holds the pre-S130 container-only shape.
///
/// # Why this exists, and why it is `Option<u16>` rather than a `StoredMediaInfo`
/// The rule had two implementations. [`StoredMediaInfo::needs_probe`] evaluated
/// it over a parsed document, and `repo::media_file::list_needing_probe` restated
/// it as a hand-written SQL literal (`media_info_version < 1`). The two agreed
/// only because the constant happened to be `1`: a v2 document was correctly left
/// alone by the Rust and equally excluded by `< 1` **by accident**, while a
/// version-0 claim was queued by the SQL and skipped by the Rust. One claim, two
/// mechanisms, kept aligned by a constant's current value — the exact class of
/// defect this epic keeps paying for.
///
/// The signature is `Option<u16>` precisely because that is what `media_info_version`
/// is: the column is a mirror of [`StoredMediaInfo::claimed_version`], written in
/// the same statement as the document (see `repo::media_file::set_probe_result`).
/// So a rule over the claim is a rule both a Rust `match` and a SQL `WHERE` can
/// evaluate over the *same* value, and [`probe_queue_predicate`] renders exactly
/// this function into SQL rather than restating it.
///
/// # The rule
/// - **No claim** → probe it. Never probed by S130, or legacy.
/// - **Strictly older than [`MEDIA_INFO_SCHEMA_VERSION`]** → probe it. Re-probing
///   UPGRADES the row; that is what a schema bump is for.
/// - **Equal or newer** → leave it. Re-probing a document a newer binary wrote
///   DOWNGRADES the row, which a rolling deploy must never do.
pub const fn version_needs_probe(claimed_version: Option<u16>) -> bool {
    match claimed_version {
        None => true,
        Some(version) => version < MEDIA_INFO_SCHEMA_VERSION,
    }
}

/// [`version_needs_probe`] rendered as a SQL boolean over `column` — the backfill
/// queue's predicate, **generated, never hand-written**.
///
/// The SQL side is the one that must scale: the queue is a keyset scan over
/// ~16,000 rows backed by a partial index, and a partial-index predicate must be
/// a constant expression. That is a real constraint, but it constrains the
/// *rendering*, not the *rule* — so the rule stays in Rust, where it can be
/// unit-tested without a database, and this function projects it. The literal
/// interpolated here is [`MEDIA_INFO_SCHEMA_VERSION`] itself, so a bump moves the
/// query and the Rust predicate in one edit and cannot move only one.
///
/// `column` is a caller-supplied identifier, never user input; every call site in
/// this crate passes the literal `"media_info_version"`.
pub fn probe_queue_predicate(column: &str) -> String {
    format!("({column} IS NULL OR {column} < {MEDIA_INFO_SCHEMA_VERSION})")
}

/// The longest string accepted as a file extension. Beyond this it is not an
/// extension, it is a filename with a dot in it (or an attack on a text column).
const MAX_FILE_EXTENSION_LEN: usize = 16;

// --- The persisted state taxonomy ------------------------------------------

/// What the probe left this file in, as persisted on `media_files.probe_state`.
///
/// # Why the failure side is a wrapper, not four flat variants
/// MPRB-02 already shipped [`ProbeState`] — the classification of *why a probe
/// produced no usable answer* — together with the exhaustive, wildcard-free
/// `match` in [`ProbeError::state`] that assigns it. Writing
/// `enum StoredProbeState { Ok, Suspicious, Unreadable, ProbeFailed }` here
/// would have been a **second classification of the same errors**: internally
/// consistent, readable, and free to drift the moment a `ProbeError` variant is
/// added. That is the exact shape this epic keeps paying for.
///
/// So the failure side is `Failed(ProbeState)` — a value of MPRB-02's type, with
/// its `as_str()` supplying the wire/DB spelling. This module adds only the two
/// states MPRB-02 could not have: the ones that describe a probe that
/// *succeeded*.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StoredProbeState {
    /// Probed, parsed, nothing looks wrong.
    Ok,
    /// Probed and parsed, but the result looks wrong (zero-duration, no video in
    /// a movie file, …). **Still stored, and stored labelled**: it parsed, and
    /// partial data serves the `/why` endpoint better than a null does.
    ///
    /// This module does not decide what is suspicious — the caller passes the
    /// description in. The suspicion rule belongs with the probe extensions
    /// (MPRB-03), and there must be exactly one of it.
    Suspicious,
    /// The probe produced no usable answer. Carries MPRB-02's classification
    /// verbatim.
    Failed(ProbeState),
}

impl StoredProbeState {
    /// The wire/DB spelling. Must stay in lockstep with the `CHECK` constraint in
    /// `migrations/0113_media_files_probe.sql` — a test in this file asserts it,
    /// by reading the migration.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Suspicious => "suspicious",
            // NOT re-spelled here. MPRB-02 owns these two strings.
            Self::Failed(state) => state.as_str(),
        }
    }

    /// The state a failed probe leaves the file in — by *delegation*, never by a
    /// second `match` over [`ProbeError`].
    pub fn from_error(error: &ProbeError) -> Self {
        Self::Failed(error.state())
    }

    /// Whether this state means a usable document was stored. `suspicious`
    /// counts as probed *for completion* while still counting as needing
    /// attention *for the report*; conflating those two questions is what makes
    /// a backfill look finished when it is not.
    pub fn is_probed(&self) -> bool {
        matches!(self, Self::Ok | Self::Suspicious)
    }

    /// Parse the DB spelling back. Returns `None` for anything else — including
    /// a value a NEWER binary wrote, which must not be guessed at.
    ///
    /// The failure spellings are matched against `ProbeState::as_str()` rather
    /// than against string literals, for the same reason [`Self::as_str`] does
    /// not restate them.
    pub fn parse(s: &str) -> Option<Self> {
        if s == Self::Ok.as_str() {
            return Some(Self::Ok);
        }
        if s == Self::Suspicious.as_str() {
            return Some(Self::Suspicious);
        }
        for state in [ProbeState::Unreadable, ProbeState::ProbeFailed] {
            if s == state.as_str() {
                return Some(Self::Failed(state));
            }
        }
        None
    }
}

// --- The flat compatibility projection -------------------------------------

/// The six flat, top-level keys `constellation-web`'s `MediaDetailPanel` already
/// renders — **a deliverable, not a convenience**.
///
/// That panel does
/// `['container','video_codec','audio_codec','resolution','width','height'].map(k => info[k] …)`
/// over `media_info` and shows whichever are present. Only `container` has ever
/// been populated (and from the *filename*), so the panel has permanent dead
/// pixels today. Emitting these keys lights it up with **zero constellation-web
/// change and no `dist/` rebuild** — which is why the keys are flat and
/// top-level rather than nested somewhere tidier.
///
/// **Derived, never independently sourced.** [`FlatProjection::derive`] is the
/// only constructor, so the projection cannot disagree with the probe it came
/// from. A field is omitted, never emitted as `"unknown"` or `0` — the panel
/// renders only present keys, and an invented value is worse than a gap.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct FlatProjection {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub container: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub video_codec: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio_codec: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolution: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
}

impl FlatProjection {
    /// Derive the projection from the probe (plus the extension hint, for the one
    /// case ffmpeg's shared demuxer makes undecidable).
    pub fn derive(probe: &MediaProbe, file_extension: Option<&str>) -> Self {
        let video = probe.primary_video();
        let (width, height) = match video {
            Some(v) => (v.width, v.height),
            None => (None, None),
        };
        Self {
            container: flat_container(&probe.container, file_extension),
            video_codec: video.map(|v| v.codec.clone()),
            // First audio stream, NOT a language/disposition-aware "default".
            // If MPRB-03 lands a `MediaProbe::default_audio()`, switch to it —
            // do NOT grow a second stream-selection rule in this module.
            audio_codec: probe.audio.first().map(|a| a.codec.clone()),
            // Absent when either dimension is unknown — never "0x0", which is a
            // claim about the file rather than an absence of one.
            resolution: match (width, height) {
                (Some(w), Some(h)) => Some(format!("{w}x{h}")),
                _ => None,
            },
            width,
            height,
        }
    }
}

/// The flat `container` key: the legacy KEY and the legacy SHAPE (a bare string
/// like `"mkv"`), so a v1 document is a strict superset of the pre-S130 one and
/// the panel keeps working throughout the deploy→backfill window.
///
/// `MediaProbe::container` is the RAW `format.format_name` — the literal string
/// `"matroska,webm"` for a `.mkv`. Rendering that where a normalised value used
/// to appear is a GUI regression wearing a schema change's clothes, so it goes
/// through [`normalize_container`], the crate's one container-normalisation rule.
///
/// # The one place the extension is consulted, and why that is not "believing
/// the filename"
/// ffmpeg uses a **shared demuxer** for Matroska and WebM: `format_name` is
/// `"matroska,webm"` for both, so normalisation *cannot* tell them apart — it is
/// not that inference is overridden, it is that there is no inference to
/// override. The extension is used only to resolve that provably-undecidable
/// case, and only when it says `webm`. Everywhere else the bytes win: a `.avi`
/// full of HEVC still reports whatever the demuxer says.
fn flat_container(format_name: &str, file_extension: Option<&str>) -> Option<String> {
    match normalize_container(format_name) {
        Some(container) => {
            let normalised = container.extension();
            if normalised == "mkv" && file_extension == Some("webm") {
                return Some("webm".to_string());
            }
            Some(normalised.to_string())
        }
        // A container this crate has no policy for. Falling back to the
        // extension keeps the panel's oldest behaviour (it showed the extension)
        // rather than blanking a field that used to render.
        None => file_extension.map(|e| e.to_string()),
    }
}

// --- The document ----------------------------------------------------------

/// The stored `media_info` document, version 1.
///
/// ```text
/// { "schema_version": 1,
///   "probe": { …the whole MediaProbe, verbatim… },
///   "container": "mkv", "video_codec": "hevc", "audio_codec": "eac3",
///   "resolution": "1920x1080", "width": 1920, "height": 1080,
///   "file_extension": "mkv" }
/// ```
///
/// `probe` embeds [`MediaProbe`] through serde rather than re-declaring its
/// fields. That is the point: MPRB-03 is extending that type in parallel, and a
/// hand-copied field list would be stale before it merged. The cost is recorded
/// on [`MEDIA_INFO_SCHEMA_VERSION`] — a shape change there is a version bump
/// here.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MediaInfoDoc {
    pub schema_version: u16,
    pub probe: MediaProbe,
    /// **The one fact the probe cannot know.** [`MediaProbe`] carries no path and
    /// no filename — correctly, since it describes a file's *contents*, and a
    /// pure function of a JSON document must not acquire a filename input. But
    /// ffmpeg's shared Matroska/WebM demuxer makes `format_name` identical for a
    /// `.mkv` and a `.webm`, and downstream needs to tell them apart for one
    /// narrow case. So it is persisted **here**, at the layer that holds the
    /// path.
    ///
    /// **It is a hint, and the constraint is load-bearing**: it may only resolve
    /// what inference over the stream lists left *unproven*, and **may never
    /// override inference that succeeded**. A scene release is a filename, not
    /// an authority — a `.mkv` extension on a file whose streams say otherwise is
    /// a mislabelled file, and believing the label over the bytes is how a
    /// playback decision goes wrong in the one case this field exists to help.
    /// Recording what the filename claims is not the same as believing it; the
    /// never-override rule is what makes storing it safe.
    ///
    /// Absent, empty, or longer than 16 characters ⇒ `None`. Downstream then
    /// simply has no tiebreaker and answers "cannot decide" more often, so this
    /// must never become a dependency.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_extension: Option<String>,
    /// The flat GUI keys, flattened to the top level of the document.
    ///
    /// On the way OUT this is always [`FlatProjection::derive`]'s output. On the
    /// way IN it is whatever the row holds, which is why readers should prefer
    /// [`Self::probe`]; [`Self::derived_projection`] recomputes it, and a test
    /// asserts the two agree for every fixture.
    #[serde(flatten)]
    pub projection: FlatProjection,
}

impl MediaInfoDoc {
    /// Build a v1 document from a probe and the `media_files.relative_path` the
    /// caller is already updating.
    pub fn new(probe: MediaProbe, relative_path: &str) -> Self {
        let file_extension = file_extension_of(relative_path);
        let projection = FlatProjection::derive(&probe, file_extension.as_deref());
        Self {
            schema_version: MEDIA_INFO_SCHEMA_VERSION,
            probe,
            file_extension,
            projection,
        }
    }

    /// Recompute the projection from the embedded probe. Equal to
    /// [`Self::projection`] for any document this binary wrote.
    pub fn derived_projection(&self) -> FlatProjection {
        FlatProjection::derive(&self.probe, self.file_extension.as_deref())
    }

    /// Serialise for storage. Fails only if the probe contains a non-finite
    /// float, which `parse_probe_json` cannot produce.
    pub fn to_json(&self) -> Result<Json, serde_json::Error> {
        serde_json::to_value(self)
    }
}

/// Lowercased final extension of a relative path, or `None`.
///
/// Deliberately string-level rather than `Path::extension()`: the value stored in
/// `media_files.relative_path` is a DB string that may have been written on
/// another platform, and `Path` semantics are host-dependent. A trailing-slash
/// (directory-style) path, a dotfile with no extension, and a bare filename all
/// yield `None`.
fn file_extension_of(relative_path: &str) -> Option<String> {
    let name = relative_path
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(relative_path);
    // `.env` is a dotfile, not an `env`-extensioned file: require something
    // before the dot.
    let (stem, ext) = name.rsplit_once('.')?;
    if stem.is_empty() || ext.is_empty() || ext.len() > MAX_FILE_EXTENSION_LEN {
        return None;
    }
    Some(ext.to_ascii_lowercase())
}

// --- The one reader --------------------------------------------------------

/// The pre-S130 document: `{"container": "mkv"}`, written by `src/library/scan.rs`
/// from the file EXTENSION, never from the file's contents.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct LegacyMediaInfo {
    pub container: Option<String>,
}

/// What a `media_files.media_info` cell actually holds. **The only typed view of
/// that column.**
#[derive(Debug, Clone, PartialEq)]
pub enum StoredMediaInfo {
    /// `NULL`, or JSON `null`.
    Absent,
    /// No `schema_version` ⇒ pre-S130. Still eligible for backfill.
    Legacy(LegacyMediaInfo),
    /// A document this binary understands.
    V1(Box<MediaInfoDoc>),
    /// A document this binary must not interpret: either written by a NEWER
    /// binary (during a rolling deploy an older binary genuinely sees newer
    /// documents), or structurally corrupt at a version we do know.
    ///
    /// Both degrade here rather than erroring, and that is the point: **a bad row
    /// must not break a list endpoint.** A newer version is never *partially*
    /// parsed — half a document read under the wrong schema is a wrong answer
    /// delivered confidently.
    UnknownVersion { version: u16 },
}

impl StoredMediaInfo {
    /// Parse a `media_info` cell. Total — never errors, never panics.
    pub fn from_json(value: Option<&Json>) -> Self {
        let Some(value) = value else {
            return Self::Absent;
        };
        if value.is_null() {
            return Self::Absent;
        }
        // A JSON array or scalar is not a document. It is `Legacy` with
        // everything `None` rather than `UnknownVersion`, because it carries no
        // version claim at all.
        let Some(object) = value.as_object() else {
            return Self::Legacy(LegacyMediaInfo::default());
        };

        let Some(version) = object.get("schema_version") else {
            return Self::Legacy(LegacyMediaInfo {
                container: object
                    .get("container")
                    .and_then(Json::as_str)
                    .map(str::to_string),
            });
        };

        // A `schema_version` that is not a small integer is a claim we cannot
        // evaluate; treat it as unreadable at version 0 rather than falling back
        // to `Legacy`, which would silently reinterpret a versioned document.
        let version = version
            .as_u64()
            .and_then(|v| u16::try_from(v).ok())
            .unwrap_or(0);
        if version != MEDIA_INFO_SCHEMA_VERSION {
            return Self::UnknownVersion { version };
        }

        match serde_json::from_value::<MediaInfoDoc>(value.clone()) {
            Ok(doc) => Self::V1(Box::new(doc)),
            // Corrupt at a version we know. Degrade, do not error.
            Err(_) => Self::UnknownVersion { version },
        }
    }

    /// The v1 document, if this row holds one.
    pub fn as_v1(&self) -> Option<&MediaInfoDoc> {
        match self {
            Self::V1(doc) => Some(doc),
            _ => None,
        }
    }

    /// The version this row claims, or `None` when it claims none.
    ///
    /// This is the Rust-side value of the `media_info_version` column: they are
    /// written in one statement (`repo::media_file::set_probe_result`) and so
    /// cannot diverge. Everything that reasons about "how old is this document"
    /// — here and in SQL — reasons about this one value.
    pub fn claimed_version(&self) -> Option<u16> {
        match self {
            // No `schema_version` key at all: never probed, or pre-S130.
            Self::Absent | Self::Legacy(_) => None,
            Self::V1(doc) => Some(doc.schema_version),
            Self::UnknownVersion { version } => Some(*version),
        }
    }

    /// Whether this row still needs a probe.
    ///
    /// **This does not state the rule — it evaluates [`version_needs_probe`]**,
    /// the same function [`probe_queue_predicate`] renders into the backfill
    /// queue's SQL. The scan (which calls this) and the backfill worker (which
    /// drains that queue) therefore cannot disagree about what needs a probe,
    /// including across a schema bump. See `version_needs_probe` for why that is
    /// worth a seam.
    pub fn needs_probe(&self) -> bool {
        version_needs_probe(self.claimed_version())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::probe::{AudioStream, VideoStream};

    fn probe(container: &str) -> MediaProbe {
        MediaProbe {
            container: container.to_string(),
            duration_secs: Some(3600.0),
            format_bitrate_bps: Some(8_000_000),
            size_bytes: Some(4_000_000_000),
            video: vec![VideoStream {
                index: 0,
                codec: "hevc".to_string(),
                width: Some(1920),
                height: Some(1080),
                ..Default::default()
            }],
            audio: vec![AudioStream {
                index: 1,
                codec: "eac3".to_string(),
                channels: Some(6),
                language: Some("eng".to_string()),
                bitrate_bps: Some(640_000),
                ..Default::default()
            }],
            subtitles: vec![],
            attachments: vec![],
            data_stream_count: 0,
            unindexed_stream_count: 0,
            chapter_count: 12,
            title: Some("A Film".to_string()),
            other_stream_count: 0,
            notes: Vec::new(),
        }
    }

    // --- state taxonomy ---

    #[test]
    fn failure_spellings_come_from_mprb_02_not_from_a_second_list() {
        // The assertion is delegation, not equality-of-two-literals: if
        // ProbeState::as_str ever changes, this module changes with it for free.
        assert_eq!(
            StoredProbeState::Failed(ProbeState::Unreadable).as_str(),
            ProbeState::Unreadable.as_str()
        );
        assert_eq!(
            StoredProbeState::Failed(ProbeState::ProbeFailed).as_str(),
            ProbeState::ProbeFailed.as_str()
        );
    }

    #[test]
    fn from_error_delegates_to_probe_error_state() {
        let cases = [
            ProbeError::ToolMissing {
                binary: "ffprobe".into(),
            },
            ProbeError::Spawn {
                binary: "ffprobe".into(),
                message: "eperm".into(),
            },
            ProbeError::Timeout { secs: 30 },
            ProbeError::ExitFailure {
                code: Some(1),
                stderr: "boom".into(),
            },
            ProbeError::MalformedOutput {
                message: "eof".into(),
            },
            ProbeError::NoStreams,
            ProbeError::OutputTooLarge { cap: 1024 },
        ];
        for error in cases {
            assert_eq!(
                StoredProbeState::from_error(&error),
                StoredProbeState::Failed(error.state()),
                "{error:?}"
            );
        }
    }

    #[test]
    fn every_state_round_trips_through_its_db_spelling() {
        for state in [
            StoredProbeState::Ok,
            StoredProbeState::Suspicious,
            StoredProbeState::Failed(ProbeState::Unreadable),
            StoredProbeState::Failed(ProbeState::ProbeFailed),
        ] {
            assert_eq!(StoredProbeState::parse(state.as_str()), Some(state));
        }
        assert_eq!(StoredProbeState::parse("teleported"), None);
        assert_eq!(StoredProbeState::parse(""), None);
    }

    /// The SQL `CHECK` and the Rust enum are two statements of one vocabulary, in
    /// two languages, and nothing else forces them to agree. This reads the
    /// migration and makes them agree.
    #[test]
    fn the_check_constraint_lists_exactly_the_states_the_code_can_write() {
        let sql = include_str!("../../migrations/0113_media_files_probe.sql");
        let clause = sql
            .split("media_files_probe_state_values CHECK (")
            .nth(1)
            .expect("the CHECK constraint should be present");
        let in_list = clause
            .split_once("IN (")
            .expect("an IN list")
            .1
            .split_once(')')
            .expect("a closed IN list")
            .0;
        let listed: Vec<String> = in_list
            .split(',')
            .map(|s| s.trim().trim_matches('\'').to_string())
            .filter(|s| !s.is_empty())
            .collect();

        let mut writable: Vec<&str> = [
            StoredProbeState::Ok,
            StoredProbeState::Suspicious,
            StoredProbeState::Failed(ProbeState::Unreadable),
            StoredProbeState::Failed(ProbeState::ProbeFailed),
        ]
        .iter()
        .map(|s| s.as_str())
        .collect();
        writable.sort_unstable();

        let mut listed_sorted: Vec<&str> = listed.iter().map(String::as_str).collect();
        listed_sorted.sort_unstable();

        assert_eq!(
            listed_sorted, writable,
            "the CHECK constraint and StoredProbeState have diverged — a state the \
             code can write that the constraint rejects is a runtime 23514, and a \
             state the constraint allows that the code cannot write is dead SQL"
        );
    }

    /// The backfill queue's partial index hard-codes the version literal, because
    /// a partial-index predicate must be constant. So bumping
    /// [`MEDIA_INFO_SCHEMA_VERSION`] without adding a matching index silently
    /// leaves the re-probe sweep seq-scanning the table — a performance cliff
    /// that reads as "the backfill is slow", not as "someone edited a constant".
    /// This makes the constant and the migration one decision.
    #[test]
    fn the_queue_index_predicate_matches_the_current_schema_version() {
        // Every migration, not just 0113: a version bump adds a NEW index file,
        // and this must keep passing when it does.
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("migrations");
        let expected = format!("media_info_version < {MEDIA_INFO_SCHEMA_VERSION}");
        let indexed = std::fs::read_dir(&dir)
            .expect("read migrations/")
            .filter_map(|e| e.ok())
            .any(|e| {
                std::fs::read_to_string(e.path())
                    .map(|sql| sql.contains(&expected))
                    .unwrap_or(false)
            });
        assert!(
            indexed,
            "MEDIA_INFO_SCHEMA_VERSION is {MEDIA_INFO_SCHEMA_VERSION} but no migration \
             indexes `{expected}` — add the new partial index before bumping the constant, \
             or the re-probe sweep seq-scans the whole table"
        );
    }

    // --- one rule, two renderings ------------------------------------------

    /// A deliberately minimal interpreter for the ONE predicate shape
    /// [`probe_queue_predicate`] emits: `(<col> IS NULL OR <col> < <n>)`.
    ///
    /// **This is the weaker half of the pair, and is labelled as such.** It reads
    /// the generated string; it does not execute Postgres — there is no
    /// `MUSE_TEST_DATABASE_URL` in this environment (MUSE #130), so the queue
    /// query itself is SKIPPED, never passed. What this *does* establish is that
    /// the exact string `list_needing_probe` runs classifies a row the same way
    /// `needs_probe()` classifies the document behind it, at every version. It
    /// asserts the shape rather than pattern-matching leniently, so a change to
    /// the emitted SQL fails here loudly instead of being silently reinterpreted.
    fn evaluate_predicate(sql: &str, column_value: Option<u16>) -> bool {
        const COLUMN: &str = "media_info_version";
        let body = sql
            .strip_prefix('(')
            .and_then(|s| s.strip_suffix(')'))
            .unwrap_or_else(|| panic!("predicate is not parenthesised: {sql}"));
        let (null_arm, less_arm) = body
            .split_once(" OR ")
            .unwrap_or_else(|| panic!("predicate is not a two-armed OR: {sql}"));
        assert_eq!(
            null_arm,
            format!("{COLUMN} IS NULL"),
            "the null arm changed shape: {sql}"
        );
        let bound: u16 = less_arm
            .strip_prefix(&format!("{COLUMN} < "))
            .unwrap_or_else(|| panic!("the ordering arm changed shape: {sql}"))
            .parse()
            .unwrap_or_else(|e| panic!("the version bound is not a u16 ({e}): {sql}"));
        match column_value {
            None => true,
            Some(version) => version < bound,
        }
    }

    /// A stored document claiming exactly `version`. At the current version this
    /// is a real, parsed `V1`; at any other it is `UnknownVersion` — which is the
    /// point, since both must answer `claimed_version()` the same way.
    fn stored_at_version(version: u16) -> StoredMediaInfo {
        let doc = MediaInfoDoc::new(probe("matroska,webm"), "Movies/A Film.mkv");
        let mut value = doc.to_json().unwrap();
        value.as_object_mut().unwrap()["schema_version"] = Json::from(version);
        StoredMediaInfo::from_json(Some(&value))
    }

    /// **The divergence test.** `needs_probe()` (what MPRB-06's scan calls) and the
    /// generated queue predicate (what MPRB-07's backfill worker drains) must
    /// classify the same set of rows — at every version, not just at the one that
    /// happens to be current.
    ///
    /// Before PROBEVER-01 these were two independent statements of one rule: the
    /// Rust said `Absent | Legacy`, the SQL said `< 1`. They agreed only by
    /// accident of the constant's value. This test fails if anyone re-splits them,
    /// and it is why a schema bump can no longer make the scan and the backfill
    /// disagree about what "needs a probe" means.
    #[test]
    fn needs_probe_and_the_generated_queue_predicate_agree_at_every_version() {
        let sql = probe_queue_predicate("media_info_version");

        // Rows that make no version claim: the column is NULL for both.
        let legacy = StoredMediaInfo::from_json(Some(&serde_json::json!({ "container": "mkv" })));
        for stored in [StoredMediaInfo::Absent, legacy] {
            assert_eq!(
                stored.claimed_version(),
                None,
                "fixture precondition: {stored:?} must claim no version, or this \
                 test is comparing the two sides over different inputs"
            );
            assert!(
                stored.needs_probe(),
                "an unversioned row must always be queued: {stored:?}"
            );
            assert_eq!(
                stored.needs_probe(),
                evaluate_predicate(&sql, None),
                "the scan and the backfill queue disagree about an unversioned row"
            );
        }

        // Every version from 0 to two beyond the current constant. `+ 2` so that
        // "newer than this binary" is covered on both sides of a future bump.
        for version in 0..=(MEDIA_INFO_SCHEMA_VERSION + 2) {
            let stored = stored_at_version(version);
            assert_eq!(
                stored.claimed_version(),
                Some(version),
                "fixture precondition: the document must claim version {version}; \
                 if `from_json` rejected it for some OTHER reason, this loop would \
                 be asserting agreement over an input the parser never saw"
            );
            assert_eq!(
                stored.needs_probe(),
                evaluate_predicate(&sql, Some(version)),
                "at schema_version {version} the scan says needs_probe={} but the \
                 backfill queue's predicate `{sql}` says {} — one rule, two \
                 mechanisms, drifted",
                stored.needs_probe(),
                evaluate_predicate(&sql, Some(version))
            );
        }
    }

    /// The rule's *direction*, stated once so the seam cannot be "corrected" into
    /// something symmetric. Older is upgraded; equal and newer are left alone.
    #[test]
    fn the_rule_re_probes_older_documents_and_never_touches_newer_ones() {
        assert!(
            version_needs_probe(None),
            "a row with no version claim has never been probed"
        );
        for older in 0..MEDIA_INFO_SCHEMA_VERSION {
            assert!(
                version_needs_probe(Some(older)),
                "version {older} is older than {MEDIA_INFO_SCHEMA_VERSION} and must be re-probed"
            );
        }
        assert!(
            !version_needs_probe(Some(MEDIA_INFO_SCHEMA_VERSION)),
            "the current version is up to date"
        );
        assert!(
            !version_needs_probe(Some(MEDIA_INFO_SCHEMA_VERSION + 1)),
            "re-probing a newer binary's document DOWNGRADES the row"
        );
    }

    /// The predicate carries the constant, not a copy of its current value.
    #[test]
    fn the_queue_predicate_is_generated_from_the_schema_version_constant() {
        assert_eq!(
            probe_queue_predicate("media_info_version"),
            format!(
                "(media_info_version IS NULL OR media_info_version < {MEDIA_INFO_SCHEMA_VERSION})"
            )
        );
    }

    #[test]
    fn suspicious_counts_as_probed_and_failures_do_not() {
        assert!(StoredProbeState::Ok.is_probed());
        assert!(StoredProbeState::Suspicious.is_probed());
        assert!(!StoredProbeState::Failed(ProbeState::Unreadable).is_probed());
        assert!(!StoredProbeState::Failed(ProbeState::ProbeFailed).is_probed());
    }

    // --- the projection ---

    #[test]
    fn the_projection_equals_the_derivation_for_every_fixture() {
        for (path, container) in [
            ("Movies/A Film (2020)/A Film.mkv", "matroska,webm"),
            ("Movies/A Film (2020)/A Film.mp4", "mov,mp4,m4a,3gp,3g2,mj2"),
            ("Shows/S01/E01.avi", "avi"),
            ("Shows/S01/E01.ts", "mpegts"),
        ] {
            let doc = MediaInfoDoc::new(probe(container), path);
            assert_eq!(doc.projection, doc.derived_projection(), "{path}");
        }
    }

    #[test]
    fn the_document_carries_the_six_flat_gui_keys_at_the_top_level() {
        let doc = MediaInfoDoc::new(probe("matroska,webm"), "Movies/A Film.mkv");
        let json = doc.to_json().unwrap();
        let object = json.as_object().unwrap();
        // Exactly the keys constellation-web's MediaDetailPanel maps.
        for key in [
            "container",
            "video_codec",
            "audio_codec",
            "resolution",
            "width",
            "height",
        ] {
            assert!(object.contains_key(key), "missing flat key {key}");
        }
        assert_eq!(object["container"], Json::from("mkv"));
        assert_eq!(object["video_codec"], Json::from("hevc"));
        assert_eq!(object["audio_codec"], Json::from("eac3"));
        assert_eq!(object["resolution"], Json::from("1920x1080"));
        assert_eq!(object["width"], Json::from(1920));
        assert_eq!(object["height"], Json::from(1080));
    }

    #[test]
    fn a_v1_document_is_a_strict_superset_of_the_legacy_one() {
        // The legacy document was exactly {"container": "<ext>"} — a bare string.
        // The panel must keep rendering throughout the deploy→backfill window.
        let doc = MediaInfoDoc::new(probe("matroska,webm"), "Movies/A Film.mkv");
        let json = doc.to_json().unwrap();
        assert!(json["container"].is_string());
        assert_eq!(json["container"].as_str(), Some("mkv"));
    }

    #[test]
    fn resolution_is_absent_rather_than_zero_by_zero_when_a_dimension_is_unknown() {
        let mut p = probe("matroska,webm");
        p.video[0].height = None;
        let doc = MediaInfoDoc::new(p, "Movies/A Film.mkv");
        assert_eq!(doc.projection.resolution, None);
        let json = doc.to_json().unwrap();
        assert!(!json.as_object().unwrap().contains_key("resolution"));
        assert!(!json.as_object().unwrap().contains_key("height"));
        assert_eq!(json["width"], Json::from(1920));
    }

    #[test]
    fn an_audio_only_file_projects_no_video_keys_and_no_resolution() {
        let mut p = probe("matroska,webm");
        p.video.clear();
        let doc = MediaInfoDoc::new(p, "Music/track.mka");
        assert_eq!(doc.projection.video_codec, None);
        assert_eq!(doc.projection.resolution, None);
        assert_eq!(doc.projection.audio_codec.as_deref(), Some("eac3"));
    }

    #[test]
    fn the_flat_container_is_normalised_not_the_raw_format_name() {
        let doc = MediaInfoDoc::new(probe("mov,mp4,m4a,3gp,3g2,mj2"), "a.mp4");
        assert_eq!(doc.projection.container.as_deref(), Some("mp4"));
        // and the RAW value survives, untouched, on the probe.
        assert_eq!(doc.probe.container, "mov,mp4,m4a,3gp,3g2,mj2");
    }

    #[test]
    fn a_container_with_no_policy_falls_back_to_the_extension_rather_than_blanking() {
        let doc = MediaInfoDoc::new(probe("ogg"), "a.ogv");
        assert_eq!(doc.projection.container.as_deref(), Some("ogv"));
    }

    // --- file_extension ---

    #[test]
    fn an_mkv_and_a_webm_with_the_identical_format_name_are_distinguishable() {
        let mkv = MediaInfoDoc::new(probe("matroska,webm"), "Movies/A Film.MKV");
        let webm = MediaInfoDoc::new(probe("matroska,webm"), "Movies/A Film.webm");
        assert_eq!(mkv.probe.container, webm.probe.container);
        assert_eq!(mkv.file_extension.as_deref(), Some("mkv")); // lowercased
        assert_eq!(webm.file_extension.as_deref(), Some("webm"));
        assert_ne!(mkv.file_extension, webm.file_extension);
        // …and the flat key resolves the demuxer's genuine ambiguity.
        assert_eq!(mkv.projection.container.as_deref(), Some("mkv"));
        assert_eq!(webm.projection.container.as_deref(), Some("webm"));
    }

    #[test]
    fn an_extension_that_disagrees_with_the_streams_is_stored_verbatim_not_believed() {
        // A .avi full of HEVC. The extension is recorded; the container is what
        // the demuxer said. Recording a claim is not believing it.
        let doc = MediaInfoDoc::new(probe("matroska,webm"), "Movies/mislabelled.avi");
        assert_eq!(doc.file_extension.as_deref(), Some("avi"));
        assert_eq!(doc.projection.container.as_deref(), Some("mkv"));
        assert_eq!(doc.projection.video_codec.as_deref(), Some("hevc"));
    }

    #[test]
    fn absent_empty_dotfile_directory_and_overlong_extensions_all_store_none() {
        for path in [
            "Movies/no_extension",
            "Movies/trailing.",
            "Movies/",
            ".hidden",
            "Movies/a.thisextensionisfartoolong",
            "",
        ] {
            assert_eq!(file_extension_of(path), None, "{path}");
        }
        assert_eq!(file_extension_of("a.mkv"), Some("mkv".to_string()));
        // Exactly at the bound is accepted; one past it is not.
        let at_bound = "a.".to_string() + &"x".repeat(MAX_FILE_EXTENSION_LEN);
        assert!(file_extension_of(&at_bound).is_some());
        let past_bound = "a.".to_string() + &"x".repeat(MAX_FILE_EXTENSION_LEN + 1);
        assert_eq!(file_extension_of(&past_bound), None);
    }

    #[test]
    fn the_probe_type_itself_never_acquires_a_path_or_a_filename() {
        // Negative test with teeth: `parse_probe_json` is a pure function of a
        // JSON document, and the extension is a fact only the persistence layer
        // holds. If someone adds a path input to the parser, this stops
        // compiling — which is the point.
        let parse: fn(&str) -> Result<MediaProbe, ProbeError> =
            crate::media::probe::parse_probe_json;
        let _ = parse;
        // And the stored probe carries no trace of the path anywhere in its JSON.
        // The token is deliberately unlike any probe field VALUE — `title` is a
        // legitimate container tag, so a fixture whose title matches its filename
        // would make this assertion pass or fail for the wrong reason.
        let doc = MediaInfoDoc::new(probe("matroska,webm"), "Movies/pathtoken9271/x.mkv");
        assert_eq!(doc.file_extension.as_deref(), Some("mkv"));
        let text = doc.to_json().unwrap()["probe"].to_string();
        assert!(!text.contains("pathtoken9271"), "{text}");
        assert!(!text.contains("file_extension"), "{text}");
    }

    // --- the reader ---

    #[test]
    fn a_null_or_missing_cell_reads_as_absent() {
        assert_eq!(StoredMediaInfo::from_json(None), StoredMediaInfo::Absent);
        assert_eq!(
            StoredMediaInfo::from_json(Some(&Json::Null)),
            StoredMediaInfo::Absent
        );
    }

    #[test]
    fn a_pre_s130_container_only_row_reads_as_legacy_and_stays_eligible_for_backfill() {
        let value = serde_json::json!({ "container": "mkv" });
        let stored = StoredMediaInfo::from_json(Some(&value));
        assert_eq!(
            stored,
            StoredMediaInfo::Legacy(LegacyMediaInfo {
                container: Some("mkv".to_string())
            })
        );
        assert!(stored.needs_probe());
    }

    #[test]
    fn an_array_or_scalar_cell_reads_as_legacy_with_everything_none() {
        for value in [
            serde_json::json!([1, 2, 3]),
            serde_json::json!("mkv"),
            serde_json::json!(7),
        ] {
            assert_eq!(
                StoredMediaInfo::from_json(Some(&value)),
                StoredMediaInfo::Legacy(LegacyMediaInfo::default()),
                "{value}"
            );
        }
    }

    #[test]
    fn a_v1_document_round_trips_the_probe_unchanged() {
        let original = probe("matroska,webm");
        let doc = MediaInfoDoc::new(original.clone(), "Movies/A Film.mkv");
        let json = doc.to_json().unwrap();
        match StoredMediaInfo::from_json(Some(&json)) {
            StoredMediaInfo::V1(back) => {
                assert_eq!(back.probe, original);
                assert_eq!(back.schema_version, MEDIA_INFO_SCHEMA_VERSION);
                assert_eq!(back.file_extension.as_deref(), Some("mkv"));
                assert_eq!(back.projection, back.derived_projection());
            }
            other => panic!("expected V1, got {other:?}"),
        }
        assert!(!StoredMediaInfo::from_json(Some(&json)).needs_probe());
    }

    #[test]
    fn a_newer_document_is_opaque_and_never_partially_parsed() {
        let value = serde_json::json!({
            "schema_version": 99,
            "probe": { "container": "matroska,webm" },
            "container": "mkv",
            "some_field_from_the_future": true,
        });
        let stored = StoredMediaInfo::from_json(Some(&value));
        assert_eq!(stored, StoredMediaInfo::UnknownVersion { version: 99 });
        assert_eq!(stored.as_v1(), None);
        // A rolling deploy must NOT re-probe (and thus downgrade) a row a newer
        // binary wrote.
        assert!(!stored.needs_probe());
    }

    /// The version check must be a **gate**, not a side effect of the newer
    /// document happening to be unparseable.
    ///
    /// This test exists because the first version of the one above did not
    /// distinguish the two: its v99 fixture was also structurally invalid, so
    /// deleting the version guard entirely left the suite green — the guard's own
    /// mutation SURVIVED. Here the body is a byte-for-byte valid v1 document
    /// wearing a newer version, which is exactly the rolling-deploy case: a newer
    /// binary's document is usually a superset, and *that* is when partial
    /// parsing silently delivers a wrong answer.
    #[test]
    fn a_newer_document_that_would_parse_as_v1_is_still_refused() {
        let doc = MediaInfoDoc::new(probe("matroska,webm"), "Movies/A Film.mkv");
        let mut value = doc.to_json().unwrap();
        let object = value.as_object_mut().unwrap();
        object.insert("schema_version".into(), Json::from(2));
        object.insert("a_field_v2_added".into(), Json::from("meaningful"));

        // Structurally this IS a v1 document; only the version says otherwise.
        let mut as_v1 = value.clone();
        as_v1.as_object_mut().unwrap()["schema_version"] = Json::from(1);
        assert!(matches!(
            StoredMediaInfo::from_json(Some(&as_v1)),
            StoredMediaInfo::V1(_)
        ));

        assert_eq!(
            StoredMediaInfo::from_json(Some(&value)),
            StoredMediaInfo::UnknownVersion { version: 2 },
            "a v2 document must be opaque even when it would deserialize as v1 — \
             half a document read under the wrong schema is a wrong answer \
             delivered confidently"
        );
    }

    #[test]
    fn a_structurally_corrupt_v1_document_degrades_instead_of_erroring() {
        let value = serde_json::json!({
            "schema_version": 1,
            "probe": { "container": 17, "this": "is not a MediaProbe" },
        });
        // The whole point: a list endpoint that renders this row must not fail.
        assert_eq!(
            StoredMediaInfo::from_json(Some(&value)),
            StoredMediaInfo::UnknownVersion { version: 1 }
        );
    }

    #[test]
    fn a_nonsense_schema_version_is_unknown_rather_than_reinterpreted_as_legacy() {
        for value in [
            serde_json::json!({ "schema_version": "one", "container": "mkv" }),
            serde_json::json!({ "schema_version": -1, "container": "mkv" }),
            serde_json::json!({ "schema_version": 999999999999u64, "container": "mkv" }),
        ] {
            assert_eq!(
                StoredMediaInfo::from_json(Some(&value)),
                StoredMediaInfo::UnknownVersion { version: 0 },
                "{value}"
            );
        }
    }

    // --- the grep guard ---

    /// `media_info` is jsonb, and jsonb invites ad-hoc key access. This walks
    /// `src/` and fails on any reach into it outside the two files that own the
    /// document. Same enforcement idea as constellation-web's single-`fetch`-site
    /// rule; it exists because "never ad-hoc key access" is otherwise a
    /// convention that survives until the next hurried change.
    #[test]
    fn nothing_outside_the_document_layer_reaches_into_the_media_info_jsonb() {
        // Split so this test's own source does not match itself.
        let needles = [
            concat!("media_info", "[\""),
            concat!("media_info", ".get(\""),
            concat!("media_info", " ->>"),
            concat!("media_info", "->>"),
        ];
        const OWNERS: &[&str] = &["src/media/doc.rs", "src/models/media_file.rs"];

        let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut offenders = Vec::new();
        let mut stack = vec![src.clone()];
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir).expect("read src/") {
                let path = entry.expect("dir entry").path();
                if path.is_dir() {
                    stack.push(path);
                    continue;
                }
                if path.extension().and_then(|e| e.to_str()) != Some("rs") {
                    continue;
                }
                let rel = path
                    .strip_prefix(src.parent().unwrap())
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/");
                if OWNERS.contains(&rel.as_str()) {
                    continue;
                }
                let text = std::fs::read_to_string(&path).expect("read source file");
                for (n, line) in text.lines().enumerate() {
                    if needles.iter().any(|needle| line.contains(needle)) {
                        offenders.push(format!("{rel}:{}", n + 1));
                    }
                }
            }
        }
        assert!(
            offenders.is_empty(),
            "ad-hoc media_info jsonb access outside the document layer: {offenders:?} — \
             read it through MediaFile::stored_media_info() instead"
        );
    }

    #[test]
    fn the_grep_guard_would_actually_catch_an_offender() {
        // The guard above can only pass; this proves its needles match the shape
        // it claims to defend against (a guard that cannot fire is not a guard).
        let needles = [
            concat!("media_info", "[\""),
            concat!("media_info", ".get(\""),
            concat!("media_info", " ->>"),
            concat!("media_info", "->>"),
        ];
        let offending_lines = [
            "    let c = row.media_info[\"container\"].clone();",
            "    let c = file.media_info.get(\"container\");",
            "    \"SELECT media_info ->> 'container' FROM media_files\"",
            "    \"SELECT media_info->>'container' FROM media_files\"",
        ];
        for line in offending_lines {
            assert!(
                needles.iter().any(|needle| line.contains(needle)),
                "the guard would not have caught: {line}"
            );
        }
        let innocent = [
            "    media_info: Option<Json>,",
            "    .bind(&media_info)",
            "    media_info = COALESCE($3, media_info),",
        ];
        for line in innocent {
            assert!(
                !needles.iter().any(|needle| line.contains(needle)),
                "the guard falsely flags: {line}"
            );
        }
    }
}
