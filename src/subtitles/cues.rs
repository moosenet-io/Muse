//! SUBS-01 — pure subtitle cue-timing parsing and offset arithmetic.
//!
//! Everything in this module is a **pure function over text**. No filesystem,
//! no process, no clock, no network. That is deliberate: applying an offset to
//! a subtitle is the one operation in this feature that changes what the
//! operator sees on screen, and it must be exhaustively testable without a
//! media file, an ffmpeg, or a database.
//!
//! ## Why a timestamp REWRITE and not a parse-and-render round trip
//!
//! The obvious design is: parse the file into a `Vec<Cue>`, shift each cue,
//! render it back out. That design silently destroys data. SRT carries
//! optional position coordinates on the timing line (`X1:.. X2:.. Y1:.. Y2:..`);
//! WebVTT carries cue settings (`align:start position:10%`), `NOTE` blocks,
//! `STYLE` blocks and a `REGION` preamble; ASS/SSA carries `[Script Info]`,
//! `[V4+ Styles]`, per-event styling, layers, effects, and karaoke `\k` tags
//! inside the text. A renderer that does not model all of that emits a file
//! that is *valid* but *different* — restyled, repositioned, or stripped of
//! the very fonts `foundry` works so hard to carry through a transcode.
//!
//! So this module rewrites **only the timestamps**, in place, and copies every
//! other byte through unchanged. Line endings, BOM, encoding, ordering,
//! blank-line conventions and unknown directives all survive untouched. The
//! cost is that the parser must recognise a timing line precisely; the benefit
//! is that a subtitle Muse has shifted is otherwise byte-identical to the one
//! the operator chose.
//!
//! ## Fail closed
//!
//! A timestamp that does not parse is [`CueError::MalformedTimestamp`], never
//! a silent zero and never a skipped line. A file with no recognisable timing
//! lines at all is [`CueError::NoCues`], never `Ok(0 cues)`. Both rules exist
//! because the failure mode they prevent — an "adjusted" subtitle that was
//! quietly not adjusted, or a "read" subtitle that quietly had no content — is
//! indistinguishable from success at the call site.

use std::fmt;

/// The subtitle text formats Muse can read cue timings from and shift.
///
/// Image-based subtitle formats (PGS/`.sup`, VOBSUB/`.idx`+`.sub`) are
/// deliberately absent. Their timings live in a binary container, not in text,
/// so none of this module applies to them; see [`SubtitleFormat::from_extension`]
/// returning `None` and [`crate::subtitles::AvailableSubtitle::is_shiftable`]
/// for how that limitation is surfaced to the operator rather than hidden.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubtitleFormat {
    /// SubRip. `00:00:20,000 --> 00:00:24,400`, comma decimal separator,
    /// millisecond precision, hours mandatory.
    SubRip,
    /// WebVTT. `00:00:20.000 --> 00:00:24.400`, period separator, millisecond
    /// precision, hours OPTIONAL (`00:20.000 --> 00:24.400` is legal).
    WebVtt,
    /// Advanced SubStation Alpha (and its SSA predecessor). Timings live in
    /// fields 2 and 3 of a `Dialogue:`/`Comment:` line as `H:MM:SS.CC` —
    /// one-digit hours, CENTISECOND precision.
    AdvancedSubStation,
}

impl SubtitleFormat {
    /// Map a filename extension (case-insensitive, no leading dot) to a
    /// format. `None` for anything this module cannot shift — including the
    /// image-based `sub`/`idx`/`sup` formats, which are legitimate subtitles
    /// Muse will happily list and activate but cannot re-time.
    pub fn from_extension(ext: &str) -> Option<Self> {
        match ext.trim().trim_start_matches('.').to_ascii_lowercase().as_str() {
            "srt" => Some(Self::SubRip),
            "vtt" | "webvtt" => Some(Self::WebVtt),
            "ass" | "ssa" => Some(Self::AdvancedSubStation),
            _ => None,
        }
    }

    /// The conventional file extension for this format.
    pub fn extension(&self) -> &'static str {
        match self {
            Self::SubRip => "srt",
            Self::WebVtt => "vtt",
            Self::AdvancedSubStation => "ass",
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::SubRip => "srt",
            Self::WebVtt => "vtt",
            Self::AdvancedSubStation => "ass",
        }
    }
}

/// One cue's on-screen span, in milliseconds from the start of the programme.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CueSpan {
    pub start_ms: i64,
    pub end_ms: i64,
}

impl CueSpan {
    pub fn duration_ms(&self) -> i64 {
        self.end_ms - self.start_ms
    }
}

/// Why a subtitle's timings could not be read or rewritten.
///
/// Every variant carries enough to tell the operator which line to look at.
/// None of them is recoverable by substituting a default — that is the point.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CueError {
    /// A line was structurally a timing line (it carried the format's cue
    /// marker) but a timestamp in it did not parse. Hard error: a subtitle
    /// file we only partly understand must never be written back out as if we
    /// understood all of it.
    MalformedTimestamp {
        line_number: usize,
        /// The offending line, truncated. Subtitle text is not secret, but it
        /// is arbitrary user content, so it is length-bounded before it can
        /// reach a log or an HTTP error body.
        line: String,
        reason: &'static str,
    },
    /// The text parsed cleanly but contained no cue timing lines at all.
    ///
    /// NOT `Ok(vec![])`. An empty subtitle and an unparseable one look
    /// identical to a caller that treats "no cues" as success, and the
    /// downstream consequence — reporting "subtitle applied" for a file that
    /// shows nothing — is exactly the false-success class this codebase
    /// refuses to ship.
    NoCues { format: SubtitleFormat },
    /// A cue's end precedes its start in the SOURCE file, before any offset
    /// was applied. Left as an error rather than repaired: a file whose cues
    /// run backwards was produced by something broken, and shifting it would
    /// produce a confidently-wrong result from a confidently-wrong input.
    InvertedCue {
        line_number: usize,
        start_ms: i64,
        end_ms: i64,
    },
}

impl fmt::Display for CueError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MalformedTimestamp {
                line_number,
                line,
                reason,
            } => write!(
                f,
                "subtitle timing line {line_number} is malformed ({reason}): {line:?} — \
                 refusing to guess at the intended time"
            ),
            Self::NoCues { format } => write!(
                f,
                "no {} cue timings were found in this subtitle — it is empty, truncated, \
                 or not actually {} (this is an error, not an empty subtitle)",
                format.as_str(),
                format.as_str()
            ),
            Self::InvertedCue {
                line_number,
                start_ms,
                end_ms,
            } => write!(
                f,
                "subtitle cue at line {line_number} ends ({end_ms}ms) before it starts \
                 ({start_ms}ms) in the source file — refusing to shift a subtitle that is \
                 already inconsistent"
            ),
        }
    }
}

impl std::error::Error for CueError {}

/// Longest slice of an offending line carried in a [`CueError`].
const MAX_ERROR_LINE_LEN: usize = 120;

fn truncate_line(line: &str) -> String {
    // Char-boundary safe: `char_indices` never splits a UTF-8 sequence, which
    // a naive `&line[..N]` would panic on for a subtitle in any non-ASCII
    // language — i.e. most of the ones a subtitle feature exists for.
    match line.char_indices().nth(MAX_ERROR_LINE_LEN) {
        Some((idx, _)) => format!("{}…", &line[..idx]),
        None => line.to_string(),
    }
}

/// What [`apply_offset`] did, beyond producing the text.
///
/// The counts are reported, not swallowed. `clamped_at_zero > 0` means the
/// operator asked for a negative shift large enough to push cues before the
/// start of the programme; those cues are pinned to zero rather than dropped
/// or given negative times, and the operator is told how many.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OffsetApplied {
    /// The rewritten subtitle text. Byte-identical to the input except for
    /// the timestamps themselves.
    pub text: String,
    /// How many cue timing lines were rewritten.
    pub cues_shifted: usize,
    /// How many cue boundaries were clamped to zero because the offset would
    /// have moved them before the start of the programme.
    pub clamped_at_zero: usize,
    /// The offset that was applied, echoed back so a caller persisting the
    /// result records the same number that is actually in the file.
    pub offset_ms: i64,
}

/// Parse every cue span out of `text`, in file order.
///
/// This is the read half — used by the sync detector to build a subtitle
/// activity signal (see [`crate::subtitles::sync`]) — and shares its timestamp
/// parsing with [`apply_offset`] so the two can never disagree about what a
/// given file says.
pub fn parse_cue_spans(text: &str, format: SubtitleFormat) -> Result<Vec<CueSpan>, CueError> {
    let mut spans = Vec::new();

    for (idx, line) in text.lines().enumerate() {
        let line_number = idx + 1;
        let Some(found) = find_timing(line, format, line_number)? else {
            continue;
        };
        if found.end_ms < found.start_ms {
            return Err(CueError::InvertedCue {
                line_number,
                start_ms: found.start_ms,
                end_ms: found.end_ms,
            });
        }
        spans.push(CueSpan {
            start_ms: found.start_ms,
            end_ms: found.end_ms,
        });
    }

    if spans.is_empty() {
        return Err(CueError::NoCues { format });
    }
    Ok(spans)
}

/// Shift every cue in `text` by `offset_ms` (positive = later, negative =
/// earlier) and return the rewritten text.
///
/// **This never writes a file.** It returns a `String`; persisting it as a
/// SEPARATE file, never over the original, is the caller's job and is enforced
/// in [`crate::subtitles::adjust`].
///
/// Clamping rule, stated once here because it is the only place a value is
/// invented rather than computed:
/// - `new_start = max(0, start + offset)`
/// - `new_end   = max(new_start, end + offset)`
///
/// A cue shifted entirely before zero therefore collapses to a zero-length cue
/// at zero rather than being dropped or given a negative timestamp. Dropping
/// it would lose dialogue; a negative timestamp is not representable in any of
/// these formats and players handle it inconsistently. Both boundaries that
/// were clamped are counted in [`OffsetApplied::clamped_at_zero`] so the
/// operator can see that a shift this large is probably not what they meant.
///
/// An `offset_ms` of exactly zero is still a full parse-and-validate pass, not
/// a shortcut return of the input: a caller asking to apply a zero offset is
/// entitled to learn that the file is malformed.
pub fn apply_offset(text: &str, format: SubtitleFormat, offset_ms: i64) -> Result<OffsetApplied, CueError> {
    let mut out = String::with_capacity(text.len() + 16);
    let mut cues_shifted = 0usize;
    let mut clamped_at_zero = 0usize;

    // Reproduce the input's line structure exactly, including whether it ended
    // with a trailing newline. `str::lines` discards that information, so the
    // split is done manually: a subtitle that gained or lost a trailing
    // newline is a diff the operator would have to explain.
    let ends_with_newline = text.ends_with('\n');
    let mut line_number = 0usize;
    let total_lines = text.split('\n').count();

    for (idx, raw_line) in text.split('\n').enumerate() {
        let is_last_fragment = idx + 1 == total_lines;
        // The final fragment of a text ending in '\n' is the empty string
        // after that newline — it is not a line of the file. Everything else
        // is emitted verbatim, and the `push('\n')` below (which fires for
        // every fragment except the last) is what restores the terminator.
        // Counting that phantom fragment as a line would put every error's
        // reported line number one past the end of the file.
        if !(is_last_fragment && ends_with_newline && raw_line.is_empty()) {
            line_number += 1;
        }

        // CRLF files: '\r' rides at the end of the fragment. Strip it for
        // parsing, restore it verbatim on output.
        let (line, cr) = match raw_line.strip_suffix('\r') {
            Some(stripped) => (stripped, "\r"),
            None => (raw_line, ""),
        };

        match find_timing(line, format, line_number)? {
            Some(found) => {
                if found.end_ms < found.start_ms {
                    return Err(CueError::InvertedCue {
                        line_number,
                        start_ms: found.start_ms,
                        end_ms: found.end_ms,
                    });
                }

                let new_start = (found.start_ms + offset_ms).max(0);
                if found.start_ms + offset_ms < 0 {
                    clamped_at_zero += 1;
                }
                let shifted_end = found.end_ms + offset_ms;
                let new_end = shifted_end.max(new_start);
                if shifted_end < new_start {
                    clamped_at_zero += 1;
                }

                // Splice: everything before the start stamp, the new start
                // stamp, everything between the stamps (the arrow, any SRT
                // position coords / VTT settings / ASS field commas), the new
                // end stamp, everything after. Only the two stamp slices move.
                out.push_str(&line[..found.start_range.0]);
                out.push_str(&format_timestamp(new_start, format));
                out.push_str(&line[found.start_range.1..found.end_range.0]);
                out.push_str(&format_timestamp(new_end, format));
                out.push_str(&line[found.end_range.1..]);

                cues_shifted += 1;
            }
            None => out.push_str(line),
        }

        out.push_str(cr);
        if !is_last_fragment {
            out.push('\n');
        }
    }

    if cues_shifted == 0 {
        return Err(CueError::NoCues { format });
    }

    Ok(OffsetApplied {
        text: out,
        cues_shifted,
        clamped_at_zero,
        offset_ms,
    })
}

/// A timing line located within one source line: both parsed times, plus the
/// exact byte ranges the two timestamps occupy so the caller can splice
/// without re-finding them.
struct FoundTiming {
    start_ms: i64,
    end_ms: i64,
    start_range: (usize, usize),
    end_range: (usize, usize),
}

/// The cue-time arrow used by both SubRip and WebVTT.
const ARROW: &str = "-->";

/// Locate and parse the timing on one line, or `Ok(None)` if this line is not
/// a timing line at all (dialogue text, a cue index, a header, a blank).
///
/// The distinction between "not a timing line" and "a malformed timing line"
/// is the whole safety property of this function. A line is judged to be a
/// timing line by a structural marker that cannot appear in ordinary subtitle
/// text at that position — the `-->` arrow for SRT/VTT, the `Dialogue:`/
/// `Comment:` prefix for ASS. Once a line is judged a timing line, a failure
/// to parse its timestamps is an ERROR, never a skip. Skipping would let a
/// file with a corrupt timestamp be "successfully" shifted with that one cue
/// left at its original time.
fn find_timing(line: &str, format: SubtitleFormat, line_number: usize) -> Result<Option<FoundTiming>, CueError> {
    match format {
        SubtitleFormat::SubRip | SubtitleFormat::WebVtt => find_arrow_timing(line, format, line_number),
        SubtitleFormat::AdvancedSubStation => find_ass_timing(line, line_number),
    }
}

fn find_arrow_timing(
    line: &str,
    format: SubtitleFormat,
    line_number: usize,
) -> Result<Option<FoundTiming>, CueError> {
    let Some(arrow_at) = line.find(ARROW) else {
        return Ok(None);
    };

    let before = &line[..arrow_at];
    let after = &line[arrow_at + ARROW.len()..];

    // The start stamp is the LAST whitespace-delimited token before the arrow;
    // the end stamp is the FIRST after it. Taking the token rather than the
    // whole slice is what lets SRT position coordinates (`X1:0 X2:0 ...`,
    // which follow the end stamp) and WebVTT cue settings (`align:start`,
    // likewise) survive untouched.
    let (start_tok, start_range) = last_token(before, 0).ok_or_else(|| CueError::MalformedTimestamp {
        line_number,
        line: truncate_line(line),
        reason: "no timestamp before the `-->` arrow",
    })?;
    let after_base = arrow_at + ARROW.len();
    let (end_tok, end_range) = first_token(after, after_base).ok_or_else(|| CueError::MalformedTimestamp {
        line_number,
        line: truncate_line(line),
        reason: "no timestamp after the `-->` arrow",
    })?;

    let start_ms = parse_timestamp(start_tok, format).ok_or_else(|| CueError::MalformedTimestamp {
        line_number,
        line: truncate_line(line),
        reason: "the start timestamp is not a valid time",
    })?;
    let end_ms = parse_timestamp(end_tok, format).ok_or_else(|| CueError::MalformedTimestamp {
        line_number,
        line: truncate_line(line),
        reason: "the end timestamp is not a valid time",
    })?;

    Ok(Some(FoundTiming {
        start_ms,
        end_ms,
        start_range,
        end_range,
    }))
}

/// ASS/SSA: `Dialogue: <Layer>,<Start>,<End>,<Style>,<Name>,...`
///
/// `Comment:` lines share the field layout and are shifted too — a commented
/// event is still positioned relative to the video, and a fansub that
/// re-enables one after Muse shifted the file would otherwise find it at the
/// old time.
fn find_ass_timing(line: &str, line_number: usize) -> Result<Option<FoundTiming>, CueError> {
    let trimmed_start = line.len() - line.trim_start().len();
    let body = line.trim_start();
    let is_event = body.starts_with("Dialogue:") || body.starts_with("Comment:");
    if !is_event {
        return Ok(None);
    }

    // Field 0 is everything after the `Dialogue:`/`Comment:` prefix up to the
    // first comma (Layer); fields 1 and 2 are Start and End. `Text` is the
    // last field and may itself contain commas, which is exactly why only the
    // first three commas are used and the remainder of the line is copied
    // through verbatim.
    let after_prefix = body.find(':').map(|i| i + 1).unwrap_or(0);
    let fields_base = trimmed_start + after_prefix;
    let fields = &line[fields_base..];

    let mut comma_positions = fields.match_indices(',').map(|(i, _)| i);
    let (Some(c0), Some(c1), Some(c2)) = (comma_positions.next(), comma_positions.next(), comma_positions.next())
    else {
        return Err(CueError::MalformedTimestamp {
            line_number,
            line: truncate_line(line),
            reason: "an ASS event line needs at least Layer,Start,End fields",
        });
    };

    let start_slice = &fields[c0 + 1..c1];
    let end_slice = &fields[c1 + 1..c2];

    let (start_tok, start_range) =
        trimmed_span(start_slice, fields_base + c0 + 1).ok_or_else(|| CueError::MalformedTimestamp {
            line_number,
            line: truncate_line(line),
            reason: "the ASS Start field is empty",
        })?;
    let (end_tok, end_range) =
        trimmed_span(end_slice, fields_base + c1 + 1).ok_or_else(|| CueError::MalformedTimestamp {
            line_number,
            line: truncate_line(line),
            reason: "the ASS End field is empty",
        })?;

    let start_ms =
        parse_timestamp(start_tok, SubtitleFormat::AdvancedSubStation).ok_or_else(|| CueError::MalformedTimestamp {
            line_number,
            line: truncate_line(line),
            reason: "the ASS Start field is not a valid time",
        })?;
    let end_ms =
        parse_timestamp(end_tok, SubtitleFormat::AdvancedSubStation).ok_or_else(|| CueError::MalformedTimestamp {
            line_number,
            line: truncate_line(line),
            reason: "the ASS End field is not a valid time",
        })?;

    Ok(Some(FoundTiming {
        start_ms,
        end_ms,
        start_range,
        end_range,
    }))
}

/// The last whitespace-delimited token in `s`, with its absolute byte range
/// (offset by `base`).
fn last_token(s: &str, base: usize) -> Option<(&str, (usize, usize))> {
    let end = s.trim_end().len();
    if end == 0 {
        return None;
    }
    let start = s[..end].rfind(char::is_whitespace).map(|i| i + 1).unwrap_or(0);
    Some((&s[start..end], (base + start, base + end)))
}

/// The first whitespace-delimited token in `s`, with its absolute byte range.
fn first_token(s: &str, base: usize) -> Option<(&str, (usize, usize))> {
    let start = s.len() - s.trim_start().len();
    if start == s.len() {
        return None;
    }
    let rel_end = s[start..].find(char::is_whitespace).map(|i| start + i).unwrap_or(s.len());
    Some((&s[start..rel_end], (base + start, base + rel_end)))
}

/// The non-whitespace core of `s`, with its absolute byte range.
fn trimmed_span(s: &str, base: usize) -> Option<(&str, (usize, usize))> {
    let start = s.len() - s.trim_start().len();
    let end = s.trim_end().len();
    if start >= end {
        return None;
    }
    Some((&s[start..end], (base + start, base + end)))
}

/// Parse one timestamp token into milliseconds.
///
/// Returns `None` — never a substituted zero — for anything that does not
/// parse. Every caller turns that `None` into a hard [`CueError`].
///
/// Accepted shapes, by format:
/// - SubRip: `HH:MM:SS,mmm` (comma). A period is also accepted, because
///   real-world `.srt` files in the wild use both and rejecting the period
///   variant would refuse files every player handles. Output is always
///   re-rendered with the canonical comma.
/// - WebVTT: `HH:MM:SS.mmm` or `MM:SS.mmm` (hours optional per the spec).
/// - ASS: `H:MM:SS.CC` (centiseconds).
///
/// Rejected in every format: a negative sign (times are unsigned in all three
/// grammars — a leading `-` is corruption, not a valid early cue), minutes or
/// seconds ≥ 60, and any non-digit in a numeric field.
pub fn parse_timestamp(token: &str, format: SubtitleFormat) -> Option<i64> {
    let token = token.trim();
    if token.is_empty() || token.starts_with('-') || token.starts_with('+') {
        return None;
    }

    // Split off the sub-second fraction. SRT canonically uses ',' and VTT/ASS
    // use '.'; both separators are accepted on input for robustness against
    // real-world files, and the FORMAT decides what is written back out.
    let (time_part, frac_part) = match token.rfind([',', '.']) {
        Some(idx) => (&token[..idx], Some(&token[idx + 1..])),
        None => (token, None),
    };

    let components: Vec<&str> = time_part.split(':').collect();
    let (hours, minutes, seconds) = match (components.as_slice(), format) {
        ([h, m, s], _) => (parse_u_field(h)?, parse_u_field(m)?, parse_u_field(s)?),
        // WebVTT alone permits the hours field to be omitted.
        ([m, s], SubtitleFormat::WebVtt) => (0i64, parse_u_field(m)?, parse_u_field(s)?),
        _ => return None,
    };

    if minutes >= 60 || seconds >= 60 {
        return None;
    }

    let frac_ms = match (frac_part, format) {
        (None, _) => 0,
        (Some(f), _) if f.is_empty() => return None,
        (Some(f), SubtitleFormat::AdvancedSubStation) => {
            // Centiseconds. Two digits canonically; a three-digit fraction is
            // read as milliseconds rather than rejected, since some tools emit
            // ASS with millisecond precision.
            let value = parse_u_field(f)?;
            match f.len() {
                1 => value * 100,
                2 => value * 10,
                3 => value,
                _ => return None,
            }
        }
        (Some(f), _) => {
            let value = parse_u_field(f)?;
            match f.len() {
                1 => value * 100,
                2 => value * 10,
                3 => value,
                _ => return None,
            }
        }
    };

    Some(hours * 3_600_000 + minutes * 60_000 + seconds * 1000 + frac_ms)
}

/// Parse an unsigned decimal field. Rejects empties, signs, whitespace and any
/// non-ASCII-digit character. `str::parse::<i64>` alone would accept `"+5"`
/// and `" 5"`, and Unicode digits, none of which belong in a timestamp.
fn parse_u_field(s: &str) -> Option<i64> {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse::<i64>().ok()
}

/// Render milliseconds back into the format's canonical timestamp text.
///
/// `ms` is expected non-negative (every caller clamps first); a negative value
/// would be unrepresentable, so it is saturated at zero here as a last
/// defence rather than producing a nonsense stamp like `-1:59:59,000`.
pub fn format_timestamp(ms: i64, format: SubtitleFormat) -> String {
    let ms = ms.max(0);
    let hours = ms / 3_600_000;
    let minutes = (ms % 3_600_000) / 60_000;
    let seconds = (ms % 60_000) / 1000;
    let millis = ms % 1000;

    match format {
        SubtitleFormat::SubRip => format!("{hours:02}:{minutes:02}:{seconds:02},{millis:03}"),
        SubtitleFormat::WebVtt => format!("{hours:02}:{minutes:02}:{seconds:02}.{millis:03}"),
        // ASS truncates to centiseconds. Truncation, not rounding: rounding up
        // could push a cue's start past its own end for a 1-frame cue.
        SubtitleFormat::AdvancedSubStation => {
            format!("{hours}:{minutes:02}:{seconds:02}.{:02}", millis / 10)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SRT: &str = "1\n00:00:20,000 --> 00:00:24,400\nHello there.\n\n2\n00:00:25,100 --> 00:00:27,900\nGeneral Kenobi.\n";

    const VTT: &str = "WEBVTT\n\nNOTE this is a comment\n\n1\n00:00:20.000 --> 00:00:24.400 align:start position:10%\nHello there.\n";

    const ASS: &str = "[Script Info]\nTitle: Test\n\n[Events]\nFormat: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\nDialogue: 0,0:00:20.00,0:00:24.40,Default,,0,0,0,,Hello there.\n";

    // ---------- format detection ----------

    #[test]
    fn format_from_extension_covers_the_text_formats_and_rejects_image_ones() {
        assert_eq!(SubtitleFormat::from_extension("srt"), Some(SubtitleFormat::SubRip));
        assert_eq!(SubtitleFormat::from_extension(".SRT"), Some(SubtitleFormat::SubRip));
        assert_eq!(SubtitleFormat::from_extension("vtt"), Some(SubtitleFormat::WebVtt));
        assert_eq!(
            SubtitleFormat::from_extension("ass"),
            Some(SubtitleFormat::AdvancedSubStation)
        );
        assert_eq!(
            SubtitleFormat::from_extension("ssa"),
            Some(SubtitleFormat::AdvancedSubStation)
        );
        // Image-based formats carry their timings in a binary container and
        // cannot be shifted by this module — they must NOT map to a text
        // format, or `apply_offset` would be handed binary and mangle it.
        assert_eq!(SubtitleFormat::from_extension("sup"), None);
        assert_eq!(SubtitleFormat::from_extension("sub"), None);
        assert_eq!(SubtitleFormat::from_extension("idx"), None);
        assert_eq!(SubtitleFormat::from_extension("mkv"), None);
    }

    // ---------- timestamp parsing ----------

    #[test]
    fn parses_canonical_timestamps_in_every_format() {
        assert_eq!(parse_timestamp("00:00:20,000", SubtitleFormat::SubRip), Some(20_000));
        assert_eq!(
            parse_timestamp("01:02:03,456", SubtitleFormat::SubRip),
            Some(3_723_456)
        );
        assert_eq!(parse_timestamp("00:00:20.000", SubtitleFormat::WebVtt), Some(20_000));
        // VTT's optional hours field.
        assert_eq!(parse_timestamp("00:20.000", SubtitleFormat::WebVtt), Some(20_000));
        assert_eq!(
            parse_timestamp("0:00:20.00", SubtitleFormat::AdvancedSubStation),
            Some(20_000)
        );
        // ASS centiseconds: .40 is 400ms, NOT 40ms. Getting this wrong
        // produces a 360ms error on every cue.
        assert_eq!(
            parse_timestamp("0:00:24.40", SubtitleFormat::AdvancedSubStation),
            Some(24_400)
        );
    }

    #[test]
    fn malformed_timestamps_parse_to_none_never_to_zero() {
        // The single most important property in this module: a bad timestamp
        // must be distinguishable from 00:00:00,000.
        for bad in [
            "",
            "   ",
            "not-a-time",
            "00:00:20",           // no fraction is OK, but this next one is not
            "00:00:6a,000",       // non-digit in seconds
            "00:00:20,",          // empty fraction
            "00:00:20,00000",     // over-long fraction
            "-00:00:20,000",      // negative
            "+00:00:20,000",      // signed
            "00:60:20,000",       // minutes >= 60
            "00:00:60,000",       // seconds >= 60
            "20,000",             // too few components for SRT
            "00:00:00:20,000",    // too many components
            "٠٠:٠٠:٢٠,٠٠٠",       // Unicode digits
        ] {
            let parsed = parse_timestamp(bad, SubtitleFormat::SubRip);
            if bad == "00:00:20" {
                // A fraction-less stamp is legitimately 20s; assert that
                // explicitly rather than letting it ride in the reject list.
                assert_eq!(parsed, Some(20_000));
            } else {
                assert_eq!(parsed, None, "{bad:?} must not parse");
                assert_ne!(parsed, Some(0), "{bad:?} must never silently become zero");
            }
        }
    }

    #[test]
    fn timestamp_rendering_round_trips() {
        for ms in [0i64, 1, 999, 1000, 20_000, 3_723_456, 359_999_999] {
            let text = format_timestamp(ms, SubtitleFormat::SubRip);
            assert_eq!(parse_timestamp(&text, SubtitleFormat::SubRip), Some(ms), "srt {ms}");
            let text = format_timestamp(ms, SubtitleFormat::WebVtt);
            assert_eq!(parse_timestamp(&text, SubtitleFormat::WebVtt), Some(ms), "vtt {ms}");
        }
        // ASS truncates to centiseconds, so it round-trips only at 10ms
        // granularity — asserted rather than glossed over.
        assert_eq!(
            format_timestamp(24_449, SubtitleFormat::AdvancedSubStation),
            "0:00:24.44"
        );
        assert_eq!(
            format_timestamp(24_440, SubtitleFormat::AdvancedSubStation),
            "0:00:24.44"
        );
    }

    #[test]
    fn format_timestamp_saturates_a_negative_rather_than_emitting_a_nonsense_stamp() {
        assert_eq!(format_timestamp(-5_000, SubtitleFormat::SubRip), "00:00:00,000");
    }

    // ---------- span parsing ----------

    #[test]
    fn parses_spans_from_each_format() {
        let spans = parse_cue_spans(SRT, SubtitleFormat::SubRip).unwrap();
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[0], CueSpan { start_ms: 20_000, end_ms: 24_400 });
        assert_eq!(spans[1], CueSpan { start_ms: 25_100, end_ms: 27_900 });

        let spans = parse_cue_spans(VTT, SubtitleFormat::WebVtt).unwrap();
        assert_eq!(spans, vec![CueSpan { start_ms: 20_000, end_ms: 24_400 }]);

        let spans = parse_cue_spans(ASS, SubtitleFormat::AdvancedSubStation).unwrap();
        assert_eq!(spans, vec![CueSpan { start_ms: 20_000, end_ms: 24_400 }]);
    }

    #[test]
    fn a_subtitle_with_no_cues_is_an_error_not_an_empty_list() {
        // Fail-closed rule. "Empty" and "unreadable" must not look alike.
        let err = parse_cue_spans("just some text\nno timings here\n", SubtitleFormat::SubRip).unwrap_err();
        assert!(matches!(err, CueError::NoCues { .. }), "got {err:?}");
        let err = parse_cue_spans("", SubtitleFormat::SubRip).unwrap_err();
        assert!(matches!(err, CueError::NoCues { .. }), "got {err:?}");
    }

    #[test]
    fn a_malformed_timing_line_is_an_error_not_a_skipped_line() {
        // The line HAS an arrow, so it is unambiguously meant to be a timing
        // line; failing to parse it must not silently leave that cue alone.
        //
        // BOTH sides are covered. An earlier version of this test only broke
        // the END timestamp, which let a mutation that silently skipped a
        // broken START survive: the END check caught it by luck, not by
        // design.
        for (text, side) in [
            ("1\n00:00:20,000 --> BROKEN\nHello.\n", "end"),
            ("1\nBROKEN --> 00:00:24,400\nHello.\n", "start"),
            ("1\nBROKEN --> ALSO-BROKEN\nHello.\n", "both"),
        ] {
            let err = parse_cue_spans(text, SubtitleFormat::SubRip).unwrap_err();
            match err {
                CueError::MalformedTimestamp { line_number, .. } => {
                    assert_eq!(line_number, 2, "broken {side}")
                }
                other => panic!("expected MalformedTimestamp for a broken {side}, got {other:?}"),
            }
            // Same through the write path: it must not "succeed" having
            // quietly left the broken cue at its original time.
            let err = apply_offset(text, SubtitleFormat::SubRip, 1_000).unwrap_err();
            assert!(
                matches!(err, CueError::MalformedTimestamp { .. }),
                "a broken {side} timestamp must abort the shift, got {err:?}"
            );
        }
    }

    /// A file whose FIRST cue is broken and whose second is fine must not
    /// produce a partially-shifted file. Directly targets the "skip the line
    /// and carry on" failure mode.
    #[test]
    fn one_broken_cue_aborts_the_whole_shift_rather_than_shifting_the_rest() {
        let text = "1\nGARBAGE --> 00:00:24,400\nA\n\n2\n00:00:30,000 --> 00:00:33,000\nB\n";
        let err = apply_offset(text, SubtitleFormat::SubRip, 5_000).unwrap_err();
        assert!(matches!(err, CueError::MalformedTimestamp { .. }), "got {err:?}");
    }

    #[test]
    fn an_inverted_cue_in_the_source_is_refused() {
        let text = "1\n00:00:24,400 --> 00:00:20,000\nBackwards.\n";
        let err = parse_cue_spans(text, SubtitleFormat::SubRip).unwrap_err();
        assert!(matches!(err, CueError::InvertedCue { .. }), "got {err:?}");
        // And the same through the write path.
        let err = apply_offset(text, SubtitleFormat::SubRip, 1000).unwrap_err();
        assert!(matches!(err, CueError::InvertedCue { .. }), "got {err:?}");
    }

    // ---------- offset application ----------

    #[test]
    fn positive_offset_shifts_every_cue_later() {
        let out = apply_offset(SRT, SubtitleFormat::SubRip, 2_500).unwrap();
        assert_eq!(out.cues_shifted, 2);
        assert_eq!(out.clamped_at_zero, 0);
        assert!(out.text.contains("00:00:22,500 --> 00:00:26,900"));
        assert!(out.text.contains("00:00:27,600 --> 00:00:30,400"));
        // Dialogue is untouched.
        assert!(out.text.contains("General Kenobi."));
    }

    #[test]
    fn negative_offset_shifts_every_cue_earlier() {
        let out = apply_offset(SRT, SubtitleFormat::SubRip, -5_000).unwrap();
        assert_eq!(out.cues_shifted, 2);
        assert_eq!(out.clamped_at_zero, 0);
        assert!(out.text.contains("00:00:15,000 --> 00:00:19,400"));
    }

    #[test]
    fn a_negative_offset_past_zero_clamps_and_reports_rather_than_dropping_the_cue() {
        // -30s against a cue at 20s–24.4s: the start goes below zero, the end
        // does not. The cue must survive, pinned at zero, and the clamp must
        // be COUNTED so the operator learns the shift was too large.
        let out = apply_offset(SRT, SubtitleFormat::SubRip, -30_000).unwrap();
        assert_eq!(out.cues_shifted, 2, "no cue may be dropped");
        assert!(out.clamped_at_zero >= 1, "the clamp must be reported, not hidden");
        assert!(out.text.contains("00:00:00,000 -->"));
        // No negative timestamp may ever be emitted.
        assert!(!out.text.contains("-00:"), "a negative stamp is unrepresentable");
        assert!(out.text.contains("Hello there."));
    }

    #[test]
    fn a_cue_shifted_entirely_before_zero_collapses_to_zero_never_inverts() {
        let text = "1\n00:00:02,000 --> 00:00:03,000\nEarly.\n";
        let out = apply_offset(text, SubtitleFormat::SubRip, -60_000).unwrap();
        assert_eq!(out.cues_shifted, 1);
        assert_eq!(out.clamped_at_zero, 2, "both boundaries were clamped");
        assert!(out.text.contains("00:00:00,000 --> 00:00:00,000"));
        // Re-parsing must succeed: whatever we emit has to be a legal file.
        let spans = parse_cue_spans(&out.text, SubtitleFormat::SubRip).unwrap();
        assert_eq!(spans[0].start_ms, 0);
        assert_eq!(spans[0].end_ms, 0);
        assert!(spans[0].end_ms >= spans[0].start_ms, "must never invert");
    }

    #[test]
    fn a_zero_offset_still_validates_and_is_not_a_shortcut() {
        // A caller applying zero is entitled to learn the file is broken.
        let err = apply_offset("1\n00:00:20,000 --> NOPE\nx\n", SubtitleFormat::SubRip, 0).unwrap_err();
        assert!(matches!(err, CueError::MalformedTimestamp { .. }));
        // And a clean file with zero offset comes back byte-identical.
        let out = apply_offset(SRT, SubtitleFormat::SubRip, 0).unwrap();
        assert_eq!(out.text, SRT, "a zero shift must not perturb a single byte");
    }

    #[test]
    fn offset_application_preserves_everything_that_is_not_a_timestamp() {
        // VTT: header, NOTE block, cue settings after the end stamp.
        let out = apply_offset(VTT, SubtitleFormat::WebVtt, 1_000).unwrap();
        assert!(out.text.starts_with("WEBVTT"), "the header must survive");
        assert!(out.text.contains("NOTE this is a comment"));
        assert!(
            out.text.contains("00:00:21.000 --> 00:00:25.400 align:start position:10%"),
            "cue settings must ride through untouched: {}",
            out.text
        );
    }

    #[test]
    fn srt_position_coordinates_survive_the_shift() {
        // SRT's rarely-used-but-legal position coords sit AFTER the end stamp
        // on the same line. A whole-slice parser would eat them.
        let text = "1\n00:00:20,000 --> 00:00:24,400  X1:100 X2:600 Y1:400 Y2:460\nPositioned.\n";
        let out = apply_offset(text, SubtitleFormat::SubRip, 1_000).unwrap();
        assert!(
            out.text.contains("00:00:21,000 --> 00:00:25,400  X1:100 X2:600 Y1:400 Y2:460"),
            "{}",
            out.text
        );
    }

    #[test]
    fn ass_styling_layers_and_comma_bearing_text_survive_the_shift() {
        let out = apply_offset(ASS, SubtitleFormat::AdvancedSubStation, 1_000).unwrap();
        assert!(out.text.contains("[Script Info]"));
        assert!(out.text.contains("Format: Layer, Start, End"));
        assert!(
            out.text.contains("Dialogue: 0,0:00:21.00,0:00:25.40,Default,,0,0,0,,Hello there."),
            "{}",
            out.text
        );

        // Text containing commas and override tags must not be re-split.
        let tricky = "Dialogue: 0,0:00:20.00,0:00:24.40,Default,,0,0,0,,{\\k20}Hello, and, welcome\n";
        let out = apply_offset(tricky, SubtitleFormat::AdvancedSubStation, 500).unwrap();
        assert!(
            out.text.contains("{\\k20}Hello, and, welcome"),
            "comma-bearing dialogue must survive verbatim: {}",
            out.text
        );
        assert!(out.text.contains("0:00:20.50,0:00:24.90"));
    }

    #[test]
    fn ass_comment_events_are_shifted_too() {
        let text = "Comment: 0,0:00:20.00,0:00:24.40,Default,,0,0,0,,hidden\n";
        let out = apply_offset(text, SubtitleFormat::AdvancedSubStation, 1_000).unwrap();
        assert!(out.text.contains("0:00:21.00,0:00:25.40"), "{}", out.text);
    }

    #[test]
    fn crlf_line_endings_and_the_trailing_newline_are_preserved_exactly() {
        let crlf = "1\r\n00:00:20,000 --> 00:00:24,400\r\nHello.\r\n";
        let out = apply_offset(crlf, SubtitleFormat::SubRip, 1_000).unwrap();
        assert!(out.text.contains("\r\n"), "CRLF must survive");
        assert!(!out.text.contains("\n\r"), "line endings must not be mangled");
        assert!(out.text.ends_with("\r\n"), "the trailing terminator must survive");
        assert!(out.text.contains("00:00:21,000 --> 00:00:25,400\r\n"));

        // A file with NO trailing newline must not gain one.
        let no_trailer = "1\n00:00:20,000 --> 00:00:24,400\nHello.";
        let out = apply_offset(no_trailer, SubtitleFormat::SubRip, 0).unwrap();
        assert_eq!(out.text, no_trailer);
    }

    #[test]
    fn non_ascii_dialogue_survives_and_a_long_broken_line_does_not_panic_on_truncation() {
        let text = "1\n00:00:20,000 --> 00:00:24,400\nこんにちは、世界\n";
        let out = apply_offset(text, SubtitleFormat::SubRip, 1_000).unwrap();
        assert!(out.text.contains("こんにちは、世界"));

        // The error path truncates the offending line; a naive byte slice
        // would panic mid-codepoint on a long multibyte line.
        let long_broken = format!("00:00:20,000 --> BAD{}", "あ".repeat(400));
        let err = parse_cue_spans(&long_broken, SubtitleFormat::SubRip).unwrap_err();
        assert!(matches!(err, CueError::MalformedTimestamp { .. }));
        assert!(!err.to_string().is_empty());
    }

    #[test]
    fn applying_an_offset_twice_composes_exactly() {
        // Shift +3000 then -3000 must land back on the original timings —
        // proof there is no accumulating rounding error in the ms formats.
        let once = apply_offset(SRT, SubtitleFormat::SubRip, 3_000).unwrap();
        let back = apply_offset(&once.text, SubtitleFormat::SubRip, -3_000).unwrap();
        assert_eq!(back.text, SRT);
    }

    #[test]
    fn an_arrowless_file_read_as_srt_errors_rather_than_returning_the_input_unchanged() {
        // Handing an ASS file to the SRT path must be loud. Returning the
        // input untouched would report "offset applied" for a no-op.
        let err = apply_offset(ASS, SubtitleFormat::SubRip, 1_000).unwrap_err();
        assert!(matches!(err, CueError::NoCues { .. }), "got {err:?}");
    }

    #[test]
    fn a_timing_line_missing_a_stamp_on_either_side_errors() {
        let err = apply_offset("--> 00:00:24,400\n", SubtitleFormat::SubRip, 0).unwrap_err();
        assert!(matches!(err, CueError::MalformedTimestamp { .. }), "got {err:?}");
        let err = apply_offset("00:00:20,000 -->\n", SubtitleFormat::SubRip, 0).unwrap_err();
        assert!(matches!(err, CueError::MalformedTimestamp { .. }), "got {err:?}");
    }

    #[test]
    fn an_ass_event_line_with_too_few_fields_errors() {
        let err = apply_offset("Dialogue: 0,0:00:20.00\n", SubtitleFormat::AdvancedSubStation, 0).unwrap_err();
        assert!(matches!(err, CueError::MalformedTimestamp { .. }), "got {err:?}");
    }

    #[test]
    fn extreme_offsets_do_not_overflow() {
        // i64 milliseconds has enormous headroom, but the arithmetic must not
        // be able to panic in debug builds on a hostile input.
        let out = apply_offset(SRT, SubtitleFormat::SubRip, i32::MAX as i64).unwrap();
        assert_eq!(out.cues_shifted, 2);
        let out = apply_offset(SRT, SubtitleFormat::SubRip, -(i32::MAX as i64)).unwrap();
        assert_eq!(out.cues_shifted, 2);
        assert!(out.clamped_at_zero > 0);
    }
}
