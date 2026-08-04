//! **`probe_capture`** — turn a raw `ffprobe` document from the operator's
//! library into a fixture that is safe to commit and to mirror publicly
//! (S130-A `MPRB-04`).
//!
//! ## Why this exists at all
//! Every `ffprobe` fixture in [`crate::media::probe`] before this item was
//! either hand-written or a single hand-trimmed capture. Hand-written fixtures
//! prove the parser is self-consistent; they cannot prove it agrees with the
//! tool. MUSE #109 and #106 were both cases of a fixture agreeing with the code
//! and both disagreeing with `ffprobe`. The corpus under `tests/golden/probe/`
//! is the fix: real documents, from the real 16,221-title library, produced by
//! the real `ffprobe 5.1.9-0+deb12u1` the deployment host runs.
//!
//! Real documents carry the operator's absolute paths, episode and film titles,
//! release-group names and external database ids. This module is what stands
//! between those and a public git mirror.
//!
//! ## Why it is not a binary
//! `src/bin/` does not exist in this crate and this item deliberately does not
//! create it. A second binary target is a second thing that ships in the
//! deployment image, a second thing the OCI publish step has to know about
//! (see the `oci-publish` module-vs-binary trap), and — the point that decides
//! it — a target the test gate does not exercise. What needs to be *correct*
//! here is the scrubbing rule, not an argv wrapper around it. So the rule is an
//! ordinary module, compiled under `#[cfg(test)]` because minting fixtures is
//! the only thing that calls it, and it is driven by the ignored
//! `mint_fixtures_from_raw_captures` test in [`super::probe_golden`]. That test
//! is a real, runnable path (it is how the committed corpus was produced), not
//! a placeholder.
//!
//! ## The scrubbing rule is an ALLOWLIST, and that is the whole design
//! A denylist of "fields that carry PII" is wrong here for the reason
//! denylists are always wrong at a trust boundary: it is a claim about a set
//! nobody enumerated. `ffprobe` emits container tags verbatim, and a Matroska
//! file can carry *any* tag key a muxer chose to write — `Group`, `IMDB`,
//! `artist`, and `_STATISTICS_WRITING_APP` all appear in this library's real
//! documents, and none of them would have been on a list written in advance.
//!
//! So: every string value in the document is classified by its **path**, and a
//! path that is not on [`KEEP_PATHS`] or [`SYNTHESIZE_PATHS`] has its value
//! replaced with [`REDACTED`]. A field nobody thought about is redacted by
//! default, which is the failure direction that cannot leak.
//!
//! Keys are preserved even when values are redacted, so the fixture keeps the
//! *shape* of a real document — the parser sees the same key set `ffprobe`
//! emitted, which is most of what a shape fixture is for.
//!
//! ## And then a second, independent check
//! [`scrub_probe_document`] does not return a document it has not re-read.
//! After scrubbing it runs [`residual_pii`] over the rendered output and
//! returns `Err` on any hit. That is deliberately a *different* mechanism from
//! the allowlist — it matches on value shape (absolute paths, IPv4 literals,
//! `user@host`, known fleet hostnames) rather than on key path — so a mistake
//! in the allowlist has to coincide with a mistake in the scanner to produce a
//! leaked fixture.
//!
//! This is the same posture as the public-mirror PII gate, and it is here for
//! the same reason: the gate is a working mechanism, and the correct response
//! to it is to not need it.

use serde_json::Value;

/// What a redacted string value becomes.
///
/// A visible, greppable sentinel rather than an empty string: an empty string
/// is a value `ffprobe` itself emits (`tags.title` is empty on several real
/// files in this library), so using it here would make "we removed this" and
/// "the file said this" indistinguishable in a committed fixture.
pub const REDACTED: &str = "<redacted>";

/// JSON paths whose string value is carried through **verbatim**.
///
/// Array indices are normalized to `[]`, so `/streams/3/codec_name` is matched
/// as `/streams/[]/codec_name`.
///
/// **Case:** matching is exact, and a `.../tags/<key>` path is first re-spelled
/// to the key's canonical spelling by [`canonicalize_tag_path`], which asks
/// [`crate::media::probe::canonical_tag_key`]. So each tag appears here ONCE,
/// in lowercase, and the set of accepted spellings is whatever the parser
/// actually reads — derived from the parser's table rather than restated here,
/// where the two could drift apart (`TAGCASE-01`). A spelling the parser does
/// not read is left untouched, matches nothing, and is redacted: the
/// fail-closed default is unchanged, and no blanket case-fold is applied.
///
/// Membership rule: a path is here only if it is (a) read by
/// [`crate::media::probe::parse_probe_json`], or (b) a codec/format descriptor
/// that is a fixed vocabulary rather than free text. Nothing that a muxer can
/// write arbitrary text into is on this list, which is why `tags/title` is on
/// [`SYNTHESIZE_PATHS`] instead of here even though the parser reads it.
pub const KEEP_PATHS: &[&str] = &[
    // --- container level -------------------------------------------------
    "/format/format_name",
    "/format/format_long_name",
    "/format/duration",
    "/format/size",
    "/format/bit_rate",
    "/format/start_time",
    "/format/nb_streams",
    "/format/probe_score",
    // --- per stream ------------------------------------------------------
    "/streams/[]/codec_name",
    "/streams/[]/codec_long_name",
    "/streams/[]/codec_type",
    "/streams/[]/codec_tag",
    "/streams/[]/codec_tag_string",
    "/streams/[]/profile",
    "/streams/[]/pix_fmt",
    "/streams/[]/sample_fmt",
    "/streams/[]/color_range",
    "/streams/[]/color_space",
    "/streams/[]/color_transfer",
    "/streams/[]/color_primaries",
    "/streams/[]/chroma_location",
    "/streams/[]/field_order",
    "/streams/[]/channel_layout",
    "/streams/[]/sample_rate",
    "/streams/[]/bit_rate",
    "/streams/[]/max_bit_rate",
    "/streams/[]/bits_per_raw_sample",
    "/streams/[]/nb_frames",
    "/streams/[]/duration",
    "/streams/[]/duration_ts",
    "/streams/[]/start_time",
    "/streams/[]/start_pts",
    "/streams/[]/time_base",
    "/streams/[]/r_frame_rate",
    "/streams/[]/avg_frame_rate",
    "/streams/[]/display_aspect_ratio",
    "/streams/[]/sample_aspect_ratio",
    "/streams/[]/is_avc",
    "/streams/[]/nal_length_size",
    "/streams/[]/id",
    "/streams/[]/mimetype",
    "/streams/[]/tags/mimetype",
    // The ISO-639 language tag. A three-letter code from a closed vocabulary,
    // read by the parser, and the thing several subtitle/audio selection rules
    // turn on — a fixture with its languages redacted could not exercise them.
    //
    // Listed ONCE, in its canonical spelling. The uppercase Matroska spelling
    // `LANGUAGE` is covered because [`canonicalize_tag_path`] re-spells a tag
    // key through `probe::canonical_tag_key` before matching — see the note on
    // [`KEEP_PATHS`] above. Do not add a second entry for it here.
    "/streams/[]/tags/language",
    // --- side data (Dolby Vision) ----------------------------------------
    "/streams/[]/side_data_list/[]/side_data_type",
    // --- chapters ---------------------------------------------------------
    "/chapters/[]/time_base",
    "/chapters/[]/start_time",
    "/chapters/[]/end_time",
];

/// JSON paths whose value is replaced with a **synthetic but shape-preserving**
/// stand-in rather than with [`REDACTED`].
///
/// These are paths the parser reads *and* that carry the operator's content,
/// so neither carrying them through nor blanking them is acceptable: a
/// redacted `format.filename` would stop looking like a filename, and a
/// redacted `tags.title` would still exercise the title path but would make
/// the golden output say `<redacted>` where a reader expects a title.
///
/// Canonical spellings only, for the reason given on [`KEEP_PATHS`]: the
/// uppercase `TITLE` spelling is covered by [`canonicalize_tag_path`].
pub const SYNTHESIZE_PATHS: &[&str] = &[
    "/format/filename",
    "/format/tags/title",
    "/streams/[]/tags/title",
    "/streams/[]/tags/filename",
    "/chapters/[]/tags/title",
];

/// A residual-PII finding: what was matched and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PiiHit {
    /// Which rule fired — the vocabulary, so a message names the class.
    pub rule: &'static str,
    /// The matched text, bounded so a hit cannot itself flood a log.
    pub sample: String,
}

/// Why a document could not be turned into a committable fixture.
#[derive(Debug, Clone, PartialEq)]
pub enum ScrubError {
    /// The raw capture was not JSON at all.
    NotJson { message: String },
    /// Scrubbing ran and the output STILL matched a PII rule.
    ///
    /// Fail-closed: no document is returned. The correct response is to widen
    /// the scrubber, never to publish the document and rely on the mirror gate.
    ResidualPii { hits: Vec<PiiHit> },
}

impl std::fmt::Display for ScrubError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotJson { message } => write!(f, "raw capture is not JSON: {message}"),
            Self::ResidualPii { hits } => write!(
                f,
                "refusing to emit a fixture: {} residual PII hit(s) survived scrubbing: {:?}",
                hits.len(),
                hits
            ),
        }
    }
}

/// Fleet-identifying words that must never reach a public mirror.
///
/// Lowercased, matched as substrings of a lowercased haystack. These are host
/// and mount names from this deployment, not a general-purpose list.
const FLEET_WORDS: &[&str] = &[
    "/srv/media",
    "/home/coder",
    "lumina-harmony",
    "moosenet",
    "<operator>",
    "qnap",
    "/mnt/muse-scratch",
];

/// Scan rendered text for anything that still looks identifying.
///
/// Shape-based and deliberately independent of [`KEEP_PATHS`] — see the module
/// docs for why the two mechanisms are not allowed to share a rule.
///
/// Over-matching is acceptable here and under-matching is not: a false positive
/// costs a fixture a manual look, a false negative costs a public leak.
pub fn residual_pii(text: &str) -> Vec<PiiHit> {
    let mut hits = Vec::new();
    let lower = text.to_ascii_lowercase();

    for word in FLEET_WORDS {
        if lower.contains(word) {
            hits.push(PiiHit {
                rule: "fleet_word",
                sample: (*word).to_string(),
            });
        }
    }

    for line in text.lines() {
        if let Some(sample) = first_unix_path(line) {
            hits.push(PiiHit {
                rule: "absolute_path",
                sample,
            });
        }
        if let Some(sample) = first_windows_path(line) {
            hits.push(PiiHit {
                rule: "windows_path",
                sample,
            });
        }
        if let Some(sample) = first_ipv4(line) {
            hits.push(PiiHit {
                rule: "ipv4",
                sample,
            });
        }
        if let Some(sample) = first_user_at_host(line) {
            hits.push(PiiHit {
                rule: "user_at_host",
                sample,
            });
        }
    }

    hits
}

/// `/a/b` — two or more slash-separated path segments, the shape of an
/// absolute filesystem path.
///
/// A single `/` is not enough: `matroska,webm` never contains one but
/// `application/x-truetype-font` and `24000/1001` both do, and those are real
/// values this corpus must keep. Requiring a LEADING slash plus a second
/// separator is what distinguishes `/srv/media/Movies/...` from `image/jpeg`.
///
/// And a segment may not START with a space, which is not a nicety: the first
/// draft of this rule allowed it, and `ffprobe`'s real `codec_long_name`
/// — `"H.264 / AVC / MPEG-4 AVC / MPEG-4 part 10"` — was read as a
/// four-segment path, which made the scrubber refuse an entirely clean
/// document. Spaces INSIDE a segment are still allowed, because this
/// library's directories are full of them (`/srv/media/TV Shows/...`) and a
/// rule that missed those would miss the paths that actually occur.
fn first_unix_path(line: &str) -> Option<String> {
    let chars: Vec<char> = line.chars().collect();
    for (i, c) in chars.iter().enumerate() {
        if *c != '/' {
            continue;
        }
        if !chars.get(i + 1).copied().is_some_and(is_segment_start) {
            continue;
        }
        let mut j = i + 1;
        let mut segments = 0usize;
        let mut seg_len = 0usize;
        while j < chars.len() {
            let ch = chars[j];
            if ch == '/' {
                // A second separator only continues the path if what follows
                // it is another segment.
                if seg_len == 0 || !chars.get(j + 1).copied().is_some_and(is_segment_start) {
                    break;
                }
                segments += 1;
                seg_len = 0;
            } else if is_path_char(ch) {
                seg_len += 1;
            } else {
                break;
            }
            j += 1;
        }
        if segments >= 1 && seg_len > 0 {
            let end = j.min(chars.len());
            return Some(bound(&chars[i..end].iter().collect::<String>()));
        }
    }
    None
}

/// A path segment's FIRST character. Deliberately narrower than
/// [`is_path_char`]: no space, and no opening bracket.
fn is_segment_start(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')
}

fn is_path_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | ' ' | '\'' | '(' | ')' | '[' | ']')
}

/// `C:\Users\...` — a drive letter, a colon and a backslash.
fn first_windows_path(line: &str) -> Option<String> {
    let chars: Vec<char> = line.chars().collect();
    for i in 0..chars.len().saturating_sub(2) {
        if chars[i].is_ascii_alphabetic()
            && chars[i + 1] == ':'
            && (chars[i + 2] == '\\' || (chars[i + 2] == '\\' && chars.get(i + 3) == Some(&'\\')))
        {
            let end = (i + 24).min(chars.len());
            return Some(bound(&chars[i..end].iter().collect::<String>()));
        }
    }
    None
}

/// A dotted quad whose four parts are all in `0..=255`.
///
/// Version strings are the reason for the range check *and* for requiring
/// exactly four parts: `libebml v1.4.5` has three, and `Lavf51.10.0` has three.
fn first_ipv4(line: &str) -> Option<String> {
    for token in line.split(|c: char| !(c.is_ascii_digit() || c == '.')) {
        let parts: Vec<&str> = token.split('.').collect();
        if parts.len() != 4 {
            continue;
        }
        if parts
            .iter()
            .all(|p| !p.is_empty() && p.len() <= 3 && p.parse::<u16>().map(|n| n <= 255) == Ok(true))
        {
            return Some(token.to_string());
        }
    }
    None
}

/// `<email>` — an address shape.
fn first_user_at_host(line: &str) -> Option<String> {
    let chars: Vec<char> = line.chars().collect();
    for (i, c) in chars.iter().enumerate() {
        if *c != '@' || i == 0 {
            continue;
        }
        let before = chars[i - 1];
        if !(before.is_ascii_alphanumeric() || before == '.' || before == '_' || before == '-') {
            continue;
        }
        // The host part must contain a dot, so a bare `user@host` in prose
        // does not fire but a real address does.
        let after: String = chars[i + 1..]
            .iter()
            .take_while(|c| c.is_ascii_alphanumeric() || **c == '.' || **c == '-')
            .collect();
        if after.contains('.') && !after.ends_with('.') && after.len() >= 4 {
            let start = i.saturating_sub(16);
            let end = (i + 1 + after.len()).min(chars.len());
            return Some(bound(&chars[start..end].iter().collect::<String>()));
        }
    }
    None
}

/// Bound a reported sample so a finding cannot itself be a flood — and so a
/// hit reported into a log does not re-leak the whole path it found.
fn bound(s: &str) -> String {
    const MAX: usize = 40;
    if s.chars().count() <= MAX {
        return s.to_string();
    }
    s.chars().take(MAX).collect::<String>() + "…"
}

/// Normalize a JSON path for allowlist matching: every array index becomes
/// `[]`, so one entry covers every stream.
fn normalize_path(path: &[PathSeg]) -> String {
    let mut out = String::new();
    for seg in path {
        out.push('/');
        match seg {
            PathSeg::Key(k) => out.push_str(k),
            PathSeg::Index => out.push_str("[]"),
        }
    }
    out
}

/// Re-spell a `<prefix>/tags/<key>` path with the key's **canonical** spelling,
/// so [`KEEP_PATHS`] and [`SYNTHESIZE_PATHS`] list each tag once and the set of
/// accepted spellings is the one the parser reads.
///
/// This is the whole of the derivation (`TAGCASE-01`): the answer to "is
/// `LANGUAGE` kept?" is `probe::canonical_tag_key`'s answer, which is the same
/// table `probe::RawTags::get` resolves against, so parser and scrubber cannot
/// disagree about case without failing
/// [`tests::the_scrubber_keeps_exactly_the_spellings_the_parser_reads`].
///
/// A key the parser does not read is returned unchanged — it matches no
/// allowlist entry and is redacted. This is NOT a case-insensitive match:
/// `Language` is not read and is not kept.
fn canonicalize_tag_path(path: &str) -> String {
    let Some((prefix, key)) = path.rsplit_once('/') else {
        return path.to_string();
    };
    if !prefix.ends_with("/tags") {
        return path.to_string();
    }
    match crate::media::probe::canonical_tag_key(key) {
        Some(canonical) => format!("{prefix}/{canonical}"),
        None => path.to_string(),
    }
}

#[derive(Debug, Clone)]
enum PathSeg {
    Key(String),
    Index,
}

/// Turn a raw `ffprobe` stdout document into a committable fixture.
///
/// `label` names the fixture and is what synthesized values are derived from,
/// so two fixtures never collide and a synthetic filename is traceable back to
/// the fixture it belongs to — without being traceable to a real file.
///
/// Output is pretty-printed with sorted keys (this crate does not enable
/// `serde_json/preserve_order`, so `Map` is a `BTreeMap` and the order is
/// reproducible by construction), and ends in a newline.
pub fn scrub_probe_document(raw: &str, label: &str) -> Result<String, ScrubError> {
    let mut value: Value =
        serde_json::from_str(raw).map_err(|e| ScrubError::NotJson {
            message: e.to_string(),
        })?;

    let mut path = Vec::new();
    scrub_value(&mut value, &mut path, label);

    let rendered = serde_json::to_string_pretty(&value)
        .expect("a Value that was just parsed must re-serialize")
        + "\n";

    let hits = residual_pii(&rendered);
    if !hits.is_empty() {
        return Err(ScrubError::ResidualPii { hits });
    }
    Ok(rendered)
}

/// Scrub an `ffprobe` **stderr** capture.
///
/// Separate from the document because stderr is not JSON and because it has a
/// second problem the document does not: `ffprobe` prefixes each diagnostic
/// with the demuxer's heap address (`[matroska,webm @ 0x5c4d2c7bf180]`), which
/// changes on every run. Committing it would produce a fixture that cannot be
/// re-derived, so the address is replaced with a fixed placeholder — a
/// determinism fix, not a privacy one, and named as such.
///
/// The path `ffprobe` echoes back is the privacy part, and it is removed the
/// same fail-closed way: the scrubbed text is re-scanned and an `Err` is
/// returned rather than a document with a surviving path.
pub fn scrub_stderr(raw: &str, label: &str) -> Result<String, ScrubError> {
    let mut out = String::new();
    for line in raw.lines() {
        let line = replace_hex_addresses(line);
        // ffprobe's per-file error line is `<path>: <reason>`. Keep the reason
        // — it is the entire value of these fixtures — and replace the path.
        let rewritten = match line.find(": ") {
            Some(i) if first_unix_path(&line[..i]).is_some() => {
                format!("{}: {}", synthetic_filename(label), &line[i + 2..])
            }
            _ => line,
        };
        out.push_str(&rewritten);
        out.push('\n');
    }

    let hits = residual_pii(&out);
    if !hits.is_empty() {
        return Err(ScrubError::ResidualPii { hits });
    }
    Ok(out)
}

/// `0x5c4d2c7bf180` → `0xADDR`. Run-to-run nondeterminism, not PII.
fn replace_hex_addresses(line: &str) -> String {
    let mut out = String::new();
    let chars: Vec<char> = line.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '0' && chars.get(i + 1) == Some(&'x') {
            let mut j = i + 2;
            while j < chars.len() && chars[j].is_ascii_hexdigit() {
                j += 1;
            }
            // Only long runs — `0x0000` is a real `codec_tag` value and must
            // survive untouched.
            if j - (i + 2) >= 8 {
                out.push_str("0xADDR");
                i = j;
                continue;
            }
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

/// The synthetic filename a fixture's container is given.
fn synthetic_filename(label: &str) -> String {
    format!("fixture-{label}.bin")
}

fn scrub_value(value: &mut Value, path: &mut Vec<PathSeg>, label: &str) {
    match value {
        Value::Object(map) => {
            for (k, v) in map.iter_mut() {
                path.push(PathSeg::Key(k.clone()));
                scrub_value(v, path, label);
                path.pop();
            }
        }
        Value::Array(items) => {
            for v in items.iter_mut() {
                path.push(PathSeg::Index);
                scrub_value(v, path, label);
                path.pop();
            }
        }
        Value::String(s) => {
            let p = canonicalize_tag_path(&normalize_path(path));
            if KEEP_PATHS.contains(&p.as_str()) {
                return;
            }
            if SYNTHESIZE_PATHS.contains(&p.as_str()) {
                *s = synthesize(&p, s, label);
                return;
            }
            *s = REDACTED.to_string();
        }
        // Numbers and booleans cannot carry a name. `ffprobe` renders every
        // free-text field as a string, so nothing identifying reaches here.
        _ => {}
    }
}

/// A stand-in that keeps the value's SHAPE.
///
/// An empty string stays empty: `tags.title: ""` occurs in this library's real
/// documents, and the parser's `filter(|t| !t.is_empty())` is a branch the
/// corpus should exercise. Replacing it with a non-empty synthetic value would
/// quietly delete that coverage.
fn synthesize(path: &str, original: &str, label: &str) -> String {
    if original.trim().is_empty() {
        return original.to_string();
    }
    match path {
        "/format/filename" => synthetic_filename(label),
        // An attachment filename is a font name (`Arial.ttf`). The parser
        // carries it and Foundry's argv maps it, so the EXTENSION is the part
        // that matters; the stem is replaced.
        "/streams/[]/tags/filename" => {
            let ext = original.rsplit_once('.').map(|(_, e)| e).unwrap_or("bin");
            format!("attachment.{}", ext.to_ascii_lowercase())
        }
        _ => "Fixture Title".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- the allowlist is fail-closed --------------------------------------

    /// The property the whole design rests on: a key nobody anticipated is
    /// redacted, not carried.
    ///
    /// Written with a tag key that really does appear in this library
    /// (`Group`, a release-group name) plus an invented one, so it is not a
    /// hypothetical.
    #[test]
    fn a_tag_key_nobody_listed_is_redacted_rather_than_carried() {
        let raw = r#"{
            "streams": [{"index": 0, "codec_type": "video", "codec_name": "h264",
                         "tags": {"Group": "DUSKLiGHT", "SOME_FUTURE_TAG": "Jane Doe"}}],
            "format": {"format_name": "matroska,webm"}
        }"#;
        let out = scrub_probe_document(raw, "t").expect("must scrub");
        assert!(!out.contains("DUSKLiGHT"), "{out}");
        assert!(!out.contains("Jane Doe"), "{out}");
        // ...and the KEYS survive, so the fixture still has a real document's
        // shape.
        assert!(out.contains("\"Group\""), "{out}");
        assert!(out.contains("\"SOME_FUTURE_TAG\""), "{out}");
        assert_eq!(out.matches(REDACTED).count(), 2, "{out}");
    }

    /// Everything the parser reads must survive, or the corpus would pin a
    /// parser running on blanks.
    #[test]
    fn every_field_the_parser_reads_survives_scrubbing() {
        let raw = r#"{
            "streams": [
              {"index": 0, "codec_type": "video", "codec_name": "hevc", "profile": "Main 10",
               "codec_tag_string": "[0][0][0][0]", "width": 3832, "height": 2068,
               "pix_fmt": "yuv420p10le", "level": 150, "color_range": "tv",
               "color_space": "bt2020nc", "color_transfer": "smpte2084",
               "color_primaries": "bt2020", "bits_per_raw_sample": "10",
               "r_frame_rate": "24000/1001", "avg_frame_rate": "24000/1001",
               "bit_rate": "25979106",
               "side_data_list": [{"side_data_type": "DOVI configuration record",
                                   "dv_profile": 8, "rpu_present_flag": 1}],
               "disposition": {"attached_pic": 0}},
              {"index": 1, "codec_type": "audio", "codec_name": "eac3", "profile": "LC",
               "channels": 6, "sample_rate": "48000", "channel_layout": "5.1(side)",
               "disposition": {"default": 1, "forced": 0}, "tags": {"language": "eng"}}
            ],
            "chapters": [{"time_base": "1/1000000000", "start_time": "0.000000"}],
            "format": {"format_name": "matroska,webm", "duration": "7735.000000",
                       "size": "24150599646", "bit_rate": "24975106",
                       "filename": "/srv/media/Movies/Real Title/Real Title.mkv",
                       "tags": {"title": "A Real Film (2023)"}}
        }"#;
        let scrubbed = scrub_probe_document(raw, "dv").expect("must scrub");

        let before = crate::media::probe::parse_probe_json(raw).expect("raw parses");
        let after = crate::media::probe::parse_probe_json(&scrubbed).expect("scrubbed parses");

        // The title is deliberately synthesized, so compare everything else by
        // clearing it on both sides — and assert the synthesis actually
        // happened, so this normalization cannot hide a leak.
        assert_eq!(before.title.as_deref(), Some("A Real Film (2023)"));
        assert_eq!(after.title.as_deref(), Some("Fixture Title"));
        let (mut before, mut after) = (before, after);
        before.title = None;
        after.title = None;
        assert_eq!(
            before, after,
            "scrubbing must change nothing the parser reads except the title"
        );
    }

    /// The path shape has to be replaced with something still shaped like a
    /// path, and it must not be the real one.
    #[test]
    fn the_container_filename_is_synthesized_not_blanked() {
        let raw = r#"{"streams":[{"index":0,"codec_type":"video","codec_name":"h264"}],
                      "format":{"format_name":"avi",
                                "filename":"/srv/media/MUSIC/Artist/clip.avi"}}"#;
        let out = scrub_probe_document(raw, "legacy_avi").expect("must scrub");
        assert!(out.contains("fixture-legacy_avi.bin"), "{out}");
        assert!(!out.contains("Artist"), "{out}");
        assert!(!out.contains(REDACTED), "a filename must not be blanked: {out}");
    }

    /// An attachment filename keeps its extension, because that is the part
    /// the parser and the transcode argv act on.
    #[test]
    fn an_attachment_filename_keeps_its_extension_and_loses_its_stem() {
        let raw = r#"{"streams":[{"index":0,"codec_type":"attachment","codec_name":"ttf",
                                   "tags":{"filename":"SomeoneCustomFont-Bold.TTF"}}],
                      "format":{"format_name":"matroska,webm"}}"#;
        let out = scrub_probe_document(raw, "fonts").expect("must scrub");
        assert!(out.contains("attachment.ttf"), "{out}");
        assert!(!out.contains("SomeoneCustomFont"), "{out}");
    }

    /// An empty title stays empty — the parser's `is_empty` branch is real
    /// coverage and synthesizing over it would delete it silently.
    #[test]
    fn an_empty_title_is_not_synthesized_into_a_present_one() {
        let raw = r#"{"streams":[{"index":0,"codec_type":"video","codec_name":"h264"}],
                      "format":{"format_name":"avi","tags":{"title":""}}}"#;
        let out = scrub_probe_document(raw, "t").expect("must scrub");
        let p = crate::media::probe::parse_probe_json(&out).expect("parses");
        assert_eq!(p.title, None, "an empty title must stay absent: {out}");
        assert!(!out.contains("Fixture Title"), "{out}");
    }

    /// Scrubbing an already-scrubbed document changes nothing.
    ///
    /// This is what makes the CI corpus check meaningful: the committed
    /// fixtures are fixed points, so a difference on re-scrub is a real change
    /// in the rule rather than churn.
    #[test]
    fn scrubbing_is_idempotent() {
        let raw = r#"{"streams":[{"index":0,"codec_type":"video","codec_name":"h264",
                                   "tags":{"Group":"X"}}],
                      "format":{"format_name":"avi","filename":"/srv/media/a/b.avi",
                                "tags":{"title":"Something"}}}"#;
        let once = scrub_probe_document(raw, "t").expect("first");
        let twice = scrub_probe_document(&once, "t").expect("second");
        assert_eq!(once, twice);
    }

    // --- the residual scanner ----------------------------------------------

    /// Every rule fires on the shape it names. Written as a table of
    /// (input, expected rule) so a rule that stopped working fails HERE rather
    /// than silently widening what the scrubber will emit.
    #[test]
    fn each_residual_pii_rule_fires_on_its_own_shape() {
        let cases: &[(&str, &str)] = &[
            ("\"/srv/media/Movies/x.mkv\"", "fleet_word"),
            ("\"/var/lib/private/thing\"", "absolute_path"),
            ("\"C:\\\\Users\\\\someone\"", "windows_path"),
            // TEST-NET-3 (RFC 5737), deliberately: a real fleet address
            // here would be the very thing this rule exists to catch, and the
            // repository's own pre-push PII gate rejects one.
            ("\"connect 203.0.113.9 now\"", "ipv4"),
            ("\"mail <operator>@example.com\"", "user_at_host"),
        ];
        for (input, rule) in cases {
            let hits = residual_pii(input);
            assert!(
                hits.iter().any(|h| h.rule == *rule),
                "{input:?} must trip {rule}, got {hits:?}"
            );
        }
    }

    /// ...and the values this corpus MUST keep do not trip it. A scanner that
    /// fires on `image/jpeg` or `24000/1001` would make the corpus impossible
    /// to build, and the tempting fix would be to weaken it.
    #[test]
    fn real_ffprobe_values_do_not_trip_the_residual_scanner() {
        for ok in [
            "\"matroska,webm\"",
            "\"application/x-truetype-font\"",
            "\"image/jpeg\"",
            "\"24000/1001\"",
            "\"1/1000000000\"",
            "\"5.1(side)\"",
            "\"libebml v1.4.5 + libmatroska v1.7.1\"",
            "\"Lavf51.10.0\"",
            "\"0x0000\"",
            "\"H.265 / HEVC (High Efficiency Video Coding)\"",
            "\"ATSC A/52A (AC-3)\"",
            // The one that actually caught this out. Four space-separated
            // `/`s; an earlier rule read it as a four-segment path and refused
            // a clean document.
            "\"H.264 / AVC / MPEG-4 AVC / MPEG-4 part 10\"",
            "\"MPEG-1/2 / MPEG-2 video\"",
        ] {
            assert_eq!(residual_pii(ok), Vec::new(), "false positive on {ok:?}");
        }
    }

    /// The path rule, stated as its own table — because the false-positive
    /// test above would still pass if the rule matched NOTHING at all.
    #[test]
    fn the_path_rule_separates_real_paths_from_slash_separated_prose() {
        for hit in [
            "/srv/media/Movies/x.mkv",
            "<path>/repos/Muse",
            "/var/lib/private/thing",
            // A directory with a space in it, which this library is full of.
            "opened /srv/media/TV Shows/Foundation/ep.mkv ok",
        ] {
            assert!(first_unix_path(hit).is_some(), "must match {hit:?}");
        }
        for miss in [
            "H.264 / AVC / MPEG-4 AVC / MPEG-4 part 10",
            "application/x-truetype-font",
            "24000/1001",
            "ATSC A/52A (AC-3)",
            "a / b / c",
            // These two are here because a mutation survived without them.
            // Deleting the LEADING segment-start guard leaves the inner one,
            // which is enough for every case above — but not for a leading
            // `/ ` followed by a genuine-looking path. Without this row the
            // guard could be removed with the suite still green.
            "/ foo/bar",
            "H.264 / AVC/MPEG-4",
        ] {
            assert_eq!(first_unix_path(miss), None, "must not match {miss:?}");
        }
    }

    /// The version-string case specifically, because it is the one an IPv4
    /// rule written without a part-count check gets wrong.
    #[test]
    fn a_three_part_version_is_not_an_ip_address() {
        assert_eq!(first_ipv4("libebml v1.3.0 + libmatroska v1.4.1"), None);
        assert_eq!(first_ipv4("Lavf51.10.0"), None);
        // Four parts that are all in range IS an address, even inside prose.
        assert_eq!(first_ipv4("host <internal-ip> responded"), Some("<internal-ip>".into()));
        // ...and four parts out of range is not.
        assert_eq!(first_ipv4("build 999.999.999.999"), None);
    }

    /// The address rule needs a host part with a dot, or `ffprobe`'s own
    /// `[matroska,webm @ 0xADDR]` prefix — which every failure fixture's
    /// stderr contains — reads as an email address and makes the scrubber
    /// refuse a document it has already cleaned.
    #[test]
    fn an_at_sign_without_a_dotted_host_is_not_an_address() {
        assert_eq!(first_user_at_host("[matroska,webm @ 0xADDR]"), None);
        assert_eq!(first_user_at_host("built @ home"), None);
        assert_eq!(first_user_at_host("v1.2 @ localhost"), None);
        // The three above are all rejected by the character BEFORE the `@`
        // being a space, so on their own they say nothing about the dotted-host
        // rule — a mutation that dropped it survived them all. These two have a
        // name character immediately before the `@`, so only the dotted-host
        // requirement can reject them.
        assert_eq!(first_user_at_host("root@ct327"), None, "no dot in the host");
        assert_eq!(first_user_at_host("a@b"), None);
        assert!(first_user_at_host("<email>").is_some());
    }

    /// The scrubber must not be able to return a document that trips the
    /// scanner. This is the fail-closed contract, and it is asserted by
    /// feeding a path through a key the allowlist KEEPS, which is the only way
    /// a leak could get past the first mechanism.
    #[test]
    fn a_leak_through_an_allowlisted_key_is_still_refused() {
        // `codec_name` is on KEEP_PATHS, so the allowlist alone would carry
        // this through. The second mechanism is what stops it.
        let raw = r#"{"streams":[{"index":0,"codec_type":"video",
                                   "codec_name":"/srv/media/Movies/leak.mkv"}],
                      "format":{"format_name":"matroska,webm"}}"#;
        match scrub_probe_document(raw, "t") {
            Err(ScrubError::ResidualPii { hits }) => {
                assert!(hits.iter().any(|h| h.rule == "fleet_word"), "{hits:?}");
            }
            other => panic!("a residual leak must be refused, got {other:?}"),
        }
    }

    #[test]
    fn a_capture_that_is_not_json_is_refused_by_name() {
        match scrub_probe_document("not json at all", "t") {
            Err(ScrubError::NotJson { .. }) => {}
            other => panic!("expected NotJson, got {other:?}"),
        }
    }

    // --- stderr -------------------------------------------------------------

    /// The diagnostic is the whole point of a failure fixture, so it must
    /// survive while the path and the heap address do not.
    #[test]
    fn stderr_keeps_the_diagnostic_and_loses_the_path_and_the_address() {
        let raw = "[matroska,webm @ 0x5c4d2c7bf180] EBML header parsing failed\n\
                   /srv/media/Movies/Thing/Thing.mkv: Invalid data found when processing input\n";
        let out = scrub_stderr(raw, "fail_thing").expect("must scrub");
        assert!(out.contains("EBML header parsing failed"), "{out}");
        assert!(out.contains("Invalid data found when processing input"), "{out}");
        assert!(out.contains("0xADDR"), "{out}");
        assert!(!out.contains("0x5c4d2c7bf180"), "{out}");
        assert!(!out.contains("/srv/media"), "{out}");
        assert!(out.contains("fixture-fail_thing.bin"), "{out}");
    }

    /// A short hex value is a real `codec_tag`, not an address.
    #[test]
    fn a_short_hex_value_is_not_mistaken_for_a_heap_address() {
        assert_eq!(replace_hex_addresses("codec_tag 0x0000 and 0x1bf"), "codec_tag 0x0000 and 0x1bf");
        assert_eq!(replace_hex_addresses("[x @ 0x5c4d2c7bf180]"), "[x @ 0xADDR]");
    }

    /// A stderr line the path-rewriter did not recognize is still refused if
    /// it carries a path — the rewriter is not trusted to be exhaustive.
    #[test]
    fn an_unrecognized_stderr_shape_carrying_a_path_is_refused() {
        // No `": "` separator, so the rewrite branch does not fire.
        let raw = "could not open /srv/media/Movies/x.mkv\n";
        match scrub_stderr(raw, "t") {
            Err(ScrubError::ResidualPii { .. }) => {}
            other => panic!("expected refusal, got {other:?}"),
        }
    }

    // --- path normalization -------------------------------------------------

    /// One allowlist entry has to cover every stream, or the list would have
    /// to be as long as the longest document.
    #[test]
    fn array_indices_normalize_to_a_single_allowlist_entry() {
        let path = vec![
            PathSeg::Key("streams".into()),
            PathSeg::Index,
            PathSeg::Key("tags".into()),
            PathSeg::Key("language".into()),
        ];
        assert_eq!(normalize_path(&path), "/streams/[]/tags/language");
        assert!(KEEP_PATHS.contains(&"/streams/[]/tags/language"));
    }

    // --- S130-A TAGCASE-01: tag-key case ------------------------------------

    /// The uppercase Matroska spelling of an allowlisted tag is carried
    /// verbatim — the case the committed corpus does not contain (every one of
    /// its 37 fixtures spells it `language`), so nothing else covers it.
    ///
    /// The value is a plain ISO-639 code, so the ONLY rule that can decide this
    /// assertion is the path rule under test: `fra` trips no residual-PII
    /// scanner and no length bound.
    #[test]
    fn the_uppercase_spelling_of_an_allowlisted_tag_is_kept_verbatim() {
        let raw = r#"{"streams":[{"index":0,"codec_type":"audio","codec_name":"eac3",
                                   "tags":{"LANGUAGE":"fra"}}],
                      "format":{"format_name":"matroska,webm"}}"#;
        let out = scrub_probe_document(raw, "t").expect("must scrub");
        assert!(out.contains("\"LANGUAGE\": \"fra\""), "{out}");
        assert!(!out.contains(REDACTED), "a read tag must not be redacted: {out}");
        // …and the parser really does read it back out of the fixture, so this
        // is not just a string surviving in a file nobody parses.
        let p = crate::media::probe::parse_probe_json(&out).expect("parses");
        assert_eq!(p.audio[0].language.as_deref(), Some("fra"), "{out}");
    }

    /// The uppercase spelling of a SYNTHESIZED tag is synthesized, not blanked
    /// and not carried — an operator's real film title in caps must not reach a
    /// public mirror through a spelling the list forgot.
    #[test]
    fn the_uppercase_spelling_of_a_synthesized_tag_is_synthesized() {
        let raw = r#"{"streams":[{"index":0,"codec_type":"video","codec_name":"h264"}],
                      "format":{"format_name":"matroska,webm",
                                "tags":{"TITLE":"A Real Film (2023)"}}}"#;
        let out = scrub_probe_document(raw, "t").expect("must scrub");
        assert!(!out.contains("A Real Film"), "{out}");
        assert!(out.contains("Fixture Title"), "{out}");
        assert!(!out.contains(REDACTED), "{out}");
    }

    /// Fail-closed is unchanged: this is an ENUMERATED alias set, not a
    /// case-insensitive match. A casing the parser does not read is redacted
    /// even though its lowercase form is on an allowlist.
    ///
    /// `fra`/`A Film` are benign values, so redaction here can only be the path
    /// rule's doing — if this test ever passed because the residual scanner
    /// rejected the document instead, the `expect` would fail and say so.
    #[test]
    fn a_tag_casing_the_parser_does_not_read_is_redacted_not_kept() {
        let raw = r#"{"streams":[{"index":0,"codec_type":"audio","codec_name":"eac3",
                                   "tags":{"Language":"fra","Title":"A Film"}}],
                      "format":{"format_name":"matroska,webm"}}"#;
        let out = scrub_probe_document(raw, "t").expect("must scrub");
        assert!(!out.contains("fra"), "an unread casing must not be carried: {out}");
        assert!(!out.contains("A Film"), "{out}");
        assert!(!out.contains("Fixture Title"), "an unread casing is not synthesized: {out}");
        assert_eq!(out.matches(REDACTED).count(), 2, "{out}");
    }

    /// The derivation itself: for every spelling the parser reads, the
    /// scrubber's decision is the SAME as for that tag's canonical spelling.
    ///
    /// This is what replaces the old arrangement, where `KEEP_PATHS` and
    /// `SYNTHESIZE_PATHS` restated the parser's alias set as extra entries.
    /// Restatement is what this repo has repeatedly paid for; the point is that
    /// adding a spelling in `probe.rs` cannot leave the scrubber behind.
    #[test]
    fn the_scrubber_keeps_exactly_the_spellings_the_parser_reads() {
        use crate::media::probe::canonical_tag_key;
        for list_path in KEEP_PATHS.iter().chain(SYNTHESIZE_PATHS.iter()) {
            let Some((prefix, key)) = list_path.rsplit_once('/') else {
                continue;
            };
            if !prefix.ends_with("/tags") {
                continue;
            }
            // Whatever the canonical key resolves to, every listed spelling of
            // it must normalize onto this same entry…
            if let Some(canonical) = canonical_tag_key(key) {
                assert_eq!(canonical, key, "{list_path} must be listed canonically");
                for spelling in ["language", "LANGUAGE", "title", "TITLE", "filename"] {
                    if canonical_tag_key(spelling) != Some(canonical) {
                        continue;
                    }
                    assert_eq!(
                        canonicalize_tag_path(&format!("{prefix}/{spelling}")),
                        *list_path,
                        "spelling {spelling} must resolve onto {list_path}"
                    );
                }
            }
            // …and a casing outside the parser's table must NOT.
            let odd = format!("{prefix}/Xx{key}");
            assert_eq!(canonicalize_tag_path(&odd), odd, "unknown keys are left alone");
        }
        // The lists must not have re-grown a second entry for a spelling that
        // canonicalization already covers.
        for p in KEEP_PATHS.iter().chain(SYNTHESIZE_PATHS.iter()) {
            assert_eq!(
                canonicalize_tag_path(p),
                **p,
                "{p} is a non-canonical restatement — remove it, the case rule covers it"
            );
        }
    }

    /// Canonicalization applies to tag keys ONLY. A non-tag field that happens
    /// to share a name is matched exactly, as before.
    #[test]
    fn canonicalization_is_confined_to_tag_keys() {
        assert_eq!(canonicalize_tag_path("/streams/[]/LANGUAGE"), "/streams/[]/LANGUAGE");
        assert_eq!(canonicalize_tag_path("/streams/[]/tags/LANGUAGE"), "/streams/[]/tags/language");
        assert_eq!(canonicalize_tag_path("/format/tags/TITLE"), "/format/tags/title");
        assert_eq!(canonicalize_tag_path("/streams/[]/codec_name"), "/streams/[]/codec_name");
    }

    /// The two lists must not overlap: a path in both would have its policy
    /// decided by whichever check runs first, which is exactly the kind of
    /// silent precedence that makes a rule untrustworthy.
    #[test]
    fn the_keep_and_synthesize_lists_are_disjoint() {
        for k in KEEP_PATHS {
            assert!(!SYNTHESIZE_PATHS.contains(k), "{k} is on both lists");
        }
    }
}
