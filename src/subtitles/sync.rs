//! SUBS-01 — audio-based subtitle offset DETECTION.
//!
//! # It proposes. It never applies.
//!
//! Nothing in this module writes a file, updates a row, or changes what a
//! viewer sees. It returns an [`OffsetProposal`] — a number and a confidence —
//! and stops. Applying it requires an explicit, separate, operator-confirmed
//! call (see [`super::adjust`] and the `offset/apply` route).
//!
//! That split is the whole design, and it is not timidity. A subtitle shifted
//! the *wrong* way is worse than a subtitle left alone, because "Muse checked
//! the timing" is a claim a viewer will believe. Leaving it alone produces a
//! problem the viewer can see and diagnose in five seconds; shifting it wrongly
//! produces a problem they will blame on the file, the player, or their own
//! ears. So the machine's job here ends at "here is what I measured and how
//! sure I am", and a human closes the loop.
//!
//! # How the detection works, and why it is not an LLM problem
//!
//! Judging whether subtitles "look right" is a perceptual task. Measuring
//! whether two event streams are offset from one another is signal processing,
//! and it is the technique `ffsubsync` uses:
//!
//! 1. Reduce the **audio track** to a binary speech/silence signal over time.
//!    `ffmpeg -af silencedetect` reports every silent interval; the complement
//!    of those intervals is where someone is (probably) talking.
//! 2. Reduce the **subtitle** to the same kind of signal: a cue is on screen,
//!    or it is not. Subtitle cues track speech closely — that is what they are.
//! 3. **Cross-correlate** the two signals across a bounded range of candidate
//!    shifts. The shift whose correlation peaks is the offset.
//!
//! No model, no language understanding, no frame inspection. Two square waves
//! and a dot product.
//!
//! # Impurity boundary
//!
//! Exactly one function in this module runs a process:
//! [`extract_speech_activity`], which spawns ffmpeg. Everything it produces is
//! then handed to pure functions — [`parse_silencedetect`], [`activity_grid`],
//! [`cues_to_activity_grid`], [`cross_correlate`], [`propose_offset`] — which
//! are unit-tested against synthetic signals and never touch the outside world.

use std::path::Path;
use std::process::Command;

use serde::Serialize;

use super::cues::{CueSpan, SubtitleFormat};

/// Resolution of the activity grids, in milliseconds per bin.
///
/// 100ms is the working resolution of this whole detector. It is chosen
/// against the problem, not arbitrarily: subtitle sync errors that a viewer
/// notices start around 100–150ms, `silencedetect`'s own duration threshold is
/// coarser than that, and a finer grid multiplies the correlation cost for
/// precision the input signal does not actually carry. A proposal is therefore
/// only ever accurate to ±1 bin, and [`OffsetProposal`] says so.
pub const BIN_MS: i64 = 100;

/// Largest shift, in either direction, the detector will consider (60s).
///
/// Bounded deliberately. Genuine subtitle offsets are seconds, occasionally
/// tens of seconds (a release with a distributor logo reel before the cold
/// open). Beyond a minute, the far more likely explanation is that the
/// subtitle is for a *different cut* — extra scenes, a different framerate —
/// which a constant shift cannot fix at all. Searching further would let the
/// correlation find a spurious peak in unrelated dialogue and report it with
/// confidence.
pub const MAX_SHIFT_MS: i64 = 60_000;

/// Minimum `silencedetect` noise floor and duration. Passed to ffmpeg.
const SILENCE_NOISE_DB: &str = "-30dB";
const SILENCE_MIN_DURATION: &str = "0.5";

/// How long the detector will wait for ffmpeg before giving up.
///
/// Scanning a feature's whole audio track is minutes of work, so this is
/// generous — but bounded, because an operator pressing "check timing" must
/// not be able to wedge a request handler forever.
const FFMPEG_TIMEOUT_SECS: u64 = 900;

/// How sure the detector is about the offset it measured.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Confidence {
    /// A single sharp correlation peak, well clear of its nearest rival. The
    /// measurement is behaving the way a genuine constant offset behaves.
    High,
    /// A peak, but a less decisive one. Worth showing the operator; worth
    /// their scepticism.
    Low,
    /// No usable peak — the correlation is flat, or several unrelated shifts
    /// score nearly the same. **An offset is still returned, and it must not
    /// be trusted.** Reported rather than suppressed, because "I could not
    /// tell" is information; a suppressed result would read as "no offset
    /// needed", which is a different and possibly false claim.
    Inconclusive,
}

impl Confidence {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::High => "high",
            Self::Low => "low",
            Self::Inconclusive => "inconclusive",
        }
    }

    /// Whether the UI should offer a one-click accept. Never a gate in code —
    /// the operator may accept an inconclusive proposal if they want to — but
    /// the default affordance should match the evidence.
    pub fn worth_offering(&self) -> bool {
        matches!(self, Self::High | Self::Low)
    }
}

/// Peak-prominence threshold above which a proposal is [`Confidence::High`].
///
/// **Prominence is measured against the BACKGROUND, not against the runner-up
/// shift.** That choice is the heart of the confidence model and it was made
/// empirically, after the runner-up approach failed on correctly-synced input.
///
/// The reason: subtitle activity comes in blocks of seconds, not instants, so
/// the correlation peak is BROAD. A perfectly-synced subtitle shifted by a few
/// hundred milliseconds still overlaps the speech almost perfectly, so the
/// runner-up shift scores nearly as well as the winner and "peak beat its
/// nearest rival by 7%" — which reads as a weak result for what is in fact a
/// perfect match. Widening the rival-exclusion window only moves the problem,
/// because the right width depends on the programme's own dialogue rhythm.
///
/// The background — the median correlation across every shift searched — has
/// no such dependence. It measures what two signals of this density score
/// against each other by coincidence. A true match towers over it. Two
/// unrelated talky signals sit on it. So:
///
/// `prominence = (peak - background) / peak`
///
/// which is "how much of the peak is real signal rather than coincidence".
const HIGH_PROMINENCE: f64 = 0.25;
/// Below this, the peak is not meaningfully above the coincidence floor.
const LOW_PROMINENCE: f64 = 0.10;

/// How close to the edge of the searched range counts as "at the boundary".
///
/// Two bins of slack rather than an exact equality test: a correlation still
/// climbing when the search ends often peaks one or two bins short of the
/// literal edge because of how the overlap-fraction guard trims the extremes.
const BOUNDARY_MARGIN_BINS: i64 = 2;

/// Minimum normalized correlation for any confidence at all: **at the best
/// shift, more than half of the subtitle's on-screen time must land on
/// speech.**
///
/// This is the second half of the confidence model, and it does the work
/// prominence cannot. Prominence asks "is this shift better than the others?"
/// — a question two unrelated signals can still answer affirmatively, since
/// one of their many mediocre alignments is always a bit better than the rest.
/// Peak score asks "is this shift any good in absolute terms?", and an
/// unrelated subtitle cannot fake that: whatever shift you choose, most of its
/// cues sit over silence.
///
/// Measured on synthetic signals a genuine pairing scores ~1.0 and an
/// unrelated one ~0.3, so 0.5 sits in a wide gap rather than on a cliff. It is
/// deliberately a floor on *absolute* quality: below it, the honest answer is
/// that these two do not go together at any shift in range — most often
/// because the subtitle is for a different cut of the programme.
///
/// **Not yet tuned against a real corpus.** Real subtitles carry on-screen
/// signs and forced narrative over silence, and `silencedetect`'s -30dB floor
/// reads quiet dialogue as silence, so real-world peak scores will run lower
/// than synthetic ones. If genuine matches start coming back inconclusive,
/// this threshold is the first thing to revisit — the failure direction is the
/// safe one (a real match reported as uncertain), which is why it was set here
/// rather than lower.
const MIN_PEAK_SCORE: f64 = 0.50;

/// A measured offset proposal. **Never applied by this module.**
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct OffsetProposal {
    /// The proposed shift in milliseconds. Positive means the subtitle is
    /// EARLY and must be pushed later; negative means it is late and must be
    /// pulled earlier. Apply it with
    /// [`super::cues::apply_offset`] and this same sign.
    pub offset_ms: i64,
    pub confidence: Confidence,
    /// Normalized correlation at the peak, `[0.0, 1.0]`.
    pub peak_score: f64,
    /// `(peak - best_rival) / peak` — how much the winning shift beat the best
    /// genuinely-different alternative. This, not `peak_score`, is what
    /// decides the confidence.
    pub prominence: f64,
    /// The resolution the measurement was made at. A proposal is accurate to
    /// ±this; presenting `offset_ms` as exact would overstate it.
    pub resolution_ms: i64,
    /// Plain-language statement of what was measured and how far to trust it.
    /// Always populated, including — especially including — when the answer is
    /// inconclusive.
    pub explanation: String,
    /// How many 100ms bins carried speech in the audio, and in the subtitle.
    /// Surfaced because a tiny count is itself the explanation for a weak
    /// result.
    pub audio_active_bins: usize,
    pub subtitle_active_bins: usize,
}

/// Why a detection could not be performed at all.
///
/// Every variant is a hard failure. **None of them is ever reported as
/// "offset 0"** — a detector that returns zero when it failed is telling the
/// operator their subtitle is correctly synced, which is a claim it has not
/// earned.
#[derive(Debug, Clone, PartialEq)]
pub enum SyncError {
    /// The ffmpeg binary is not present on this host.
    ToolMissing { binary: String },
    /// ffmpeg exists but could not be spawned.
    Spawn { binary: String, message: String },
    /// ffmpeg ran and failed.
    ExitFailure { code: Option<i32>, stderr: String },
    /// ffmpeg exceeded [`FFMPEG_TIMEOUT_SECS`].
    Timeout { seconds: u64 },
    /// ffmpeg ran but reported no silence at all across the whole file.
    ///
    /// This is an error, not "the whole file is speech". `silencedetect`
    /// reporting nothing on a real programme means the noise floor never got
    /// quiet enough — a loud continuous mix, a music-only track, or a wrong
    /// threshold. Without silences there is no structure to correlate against,
    /// so a correlation run would return a shift derived from a constant
    /// signal: a number with no information in it.
    NoSilenceDetected,
    /// The audio track had no measurable speech activity.
    NoSpeechActivity,
    /// The subtitle produced no cue spans to correlate.
    NoSubtitleActivity,
    /// The media file's duration is unknown or non-positive, so no grid can be
    /// built. Guessing a duration would silently truncate or pad the signal.
    UnknownDuration,
}

impl std::fmt::Display for SyncError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ToolMissing { binary } => write!(
                f,
                "ffmpeg binary `{binary}` is not installed on this host — subtitle timing \
                 cannot be measured (this is NOT the same as the timing being correct)"
            ),
            Self::Spawn { binary, message } => write!(f, "could not spawn ffmpeg `{binary}`: {message}"),
            Self::ExitFailure { code, stderr } => write!(
                f,
                "ffmpeg exited with {} while analysing the audio track: {}",
                code.map(|c| c.to_string()).unwrap_or_else(|| "a signal".into()),
                truncate(stderr)
            ),
            Self::Timeout { seconds } => write!(f, "ffmpeg did not finish analysing the audio within {seconds}s"),
            Self::NoSilenceDetected => write!(
                f,
                "the audio track reported no silence at all, so there is no structure to \
                 correlate against — the timing could not be measured (not: the timing is correct)"
            ),
            Self::NoSpeechActivity => write!(f, "no speech activity could be derived from the audio track"),
            Self::NoSubtitleActivity => write!(f, "the subtitle produced no cue spans to correlate"),
            Self::UnknownDuration => write!(
                f,
                "the media file's duration is unknown, so no comparable timeline could be built"
            ),
        }
    }
}

impl std::error::Error for SyncError {}

fn truncate(s: &str) -> String {
    let limit = 400;
    match s.char_indices().nth(limit) {
        Some((i, _)) => format!("{}…", &s[..i]),
        None => s.to_string(),
    }
}

/// A half-open interval of silence, in milliseconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SilenceSpan {
    pub start_ms: i64,
    pub end_ms: i64,
}

// ---------------------------------------------------------------------------
// The impure boundary: exactly one function spawns a process.
// ---------------------------------------------------------------------------

/// Run `ffmpeg -af silencedetect` over `media_path`'s audio and return the
/// silent intervals it reported. **The only impure function in this module.**
///
/// The audio is decoded but never encoded (`-f null -`): nothing is written
/// anywhere. The input file is opened read-only by ffmpeg and is not modified.
pub fn extract_speech_activity(ffmpeg_bin: &str, media_path: &Path) -> Result<Vec<SilenceSpan>, SyncError> {
    let args = build_silencedetect_args(media_path);

    let output = match Command::new(ffmpeg_bin).args(&args).output() {
        Ok(out) => out,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(SyncError::ToolMissing {
                binary: ffmpeg_bin.to_string(),
            })
        }
        Err(e) => {
            return Err(SyncError::Spawn {
                binary: ffmpeg_bin.to_string(),
                message: e.to_string(),
            })
        }
    };

    // silencedetect writes to stderr; that is where the report lives even on
    // a completely successful run.
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();

    if !output.status.success() {
        return Err(SyncError::ExitFailure {
            code: output.status.code(),
            stderr,
        });
    }

    classify_silence_report(parse_silencedetect(&stderr))
}

/// Decide whether a parsed silence report is usable. **Pure**, and split out of
/// [`extract_speech_activity`] deliberately.
///
/// It lives here rather than inline because the rule it encodes — *no silence
/// is a FAILURE, not an all-speech signal* — is safety-critical and was
/// previously only reachable by actually spawning ffmpeg, which no test on
/// this fleet can rely on doing. A rule that can only be exercised by a test
/// that skips is a rule with no test at all.
///
/// The rule itself: `silencedetect` reporting nothing across a whole programme
/// does not mean the programme is wall-to-wall speech. It means the noise
/// floor never dropped far enough — a loud continuous mix, a music-only track,
/// or a threshold that does not suit this material. An all-speech grid is
/// CONSTANT, and correlating anything against a constant returns whichever
/// shift the tie-break happens to land on, with a perfect-looking score.
pub fn classify_silence_report(spans: Vec<SilenceSpan>) -> Result<Vec<SilenceSpan>, SyncError> {
    if spans.is_empty() {
        return Err(SyncError::NoSilenceDetected);
    }
    Ok(spans)
}

/// The exact ffmpeg argv used for silence detection. Split out so it is pure
/// and unit-testable — the argv is the part that can be wrong in a way tests
/// can catch, and the spawn is the part they cannot.
pub fn build_silencedetect_args(media_path: &Path) -> Vec<String> {
    vec![
        // Never prompt, never write over anything.
        "-nostdin".to_string(),
        "-hide_banner".to_string(),
        "-i".to_string(),
        media_path.to_string_lossy().into_owned(),
        // First audio stream only. `?` so a file with no audio fails cleanly
        // rather than ffmpeg erroring on the map itself.
        "-map".to_string(),
        "0:a:0?".to_string(),
        "-af".to_string(),
        format!("silencedetect=noise={SILENCE_NOISE_DB}:d={SILENCE_MIN_DURATION}"),
        // Decode and measure; encode nothing, write nothing.
        "-f".to_string(),
        "null".to_string(),
        "-".to_string(),
    ]
}

/// The configured timeout, exposed for the caller that wraps this in a
/// `spawn_blocking` with its own deadline.
pub fn ffmpeg_timeout_secs() -> u64 {
    FFMPEG_TIMEOUT_SECS
}

// ---------------------------------------------------------------------------
// Everything below is pure.
// ---------------------------------------------------------------------------

/// Parse ffmpeg's `silencedetect` report out of its stderr. **Pure.**
///
/// The report looks like:
/// ```text
/// [silencedetect @ 0x...] silence_start: 0
/// [silencedetect @ 0x...] silence_end: 3.52 | silence_duration: 3.52
/// ```
///
/// A `silence_start` with no matching `silence_end` means the file ended while
/// still silent; that trailing span is dropped rather than being closed at a
/// guessed time, because its end is genuinely unknown and inventing one would
/// put fabricated data into the correlation.
pub fn parse_silencedetect(stderr: &str) -> Vec<SilenceSpan> {
    let mut spans = Vec::new();
    let mut open_start: Option<i64> = None;

    for line in stderr.lines() {
        if let Some(rest) = line.split("silence_start:").nth(1) {
            if let Some(secs) = parse_leading_seconds(rest) {
                open_start = Some(secs);
            }
            continue;
        }
        if let Some(rest) = line.split("silence_end:").nth(1) {
            if let (Some(start), Some(end)) = (open_start, parse_leading_seconds(rest)) {
                if end > start {
                    spans.push(SilenceSpan {
                        start_ms: start,
                        end_ms: end,
                    });
                }
                open_start = None;
            }
        }
    }

    spans
}

/// Read the first whitespace-delimited float from `s` and return it as
/// milliseconds. `None` for anything that does not parse — never a zero.
fn parse_leading_seconds(s: &str) -> Option<i64> {
    let token = s.split_whitespace().next()?;
    let secs: f64 = token.parse().ok()?;
    if !secs.is_finite() || secs < 0.0 {
        return None;
    }
    Some((secs * 1000.0).round() as i64)
}

/// Build a speech-activity grid from silence spans: `1.0` where someone is
/// talking, `0.0` where it is silent. **Pure.**
///
/// The grid is the complement of the silences over `[0, duration)`. A bin is
/// marked silent only if a silence span covers its midpoint, which avoids a
/// silence boundary bleeding a whole bin either way.
pub fn activity_grid(silences: &[SilenceSpan], duration_ms: i64) -> Vec<f32> {
    let bins = (duration_ms / BIN_MS).max(0) as usize;
    let mut grid = vec![1.0f32; bins];
    for span in silences {
        let first = (span.start_ms / BIN_MS).max(0) as usize;
        let last = ((span.end_ms + BIN_MS - 1) / BIN_MS).max(0) as usize;
        for bin in grid.iter_mut().take(last.min(bins)).skip(first.min(bins)) {
            *bin = 0.0;
        }
    }
    grid
}

/// Build the same shape of grid from subtitle cue spans: `1.0` while a cue is
/// on screen. **Pure.**
pub fn cues_to_activity_grid(cues: &[CueSpan], duration_ms: i64) -> Vec<f32> {
    let bins = (duration_ms / BIN_MS).max(0) as usize;
    let mut grid = vec![0.0f32; bins];
    for cue in cues {
        if cue.end_ms < 0 {
            continue;
        }
        let first = (cue.start_ms.max(0) / BIN_MS) as usize;
        let last = ((cue.end_ms.max(0) + BIN_MS - 1) / BIN_MS) as usize;
        for bin in grid.iter_mut().take(last.min(bins)).skip(first.min(bins)) {
            *bin = 1.0;
        }
    }
    grid
}

/// The outcome of a cross-correlation sweep. **Pure** result of a pure
/// function, so it can be asserted on directly in tests.
#[derive(Debug, Clone, PartialEq)]
pub struct Correlation {
    /// Best shift, in bins. Positive means the SUBTITLE must move later.
    pub best_shift_bins: i64,
    /// Normalized score at the best shift, `[0.0, 1.0]`.
    pub peak_score: f64,
    /// The MEDIAN score across every shift searched — the level two signals of
    /// this density score against each other by coincidence. This is the
    /// baseline the peak has to beat to mean anything.
    pub background_score: f64,
}

impl Correlation {
    /// `(peak - background) / peak`, or `0.0` when the peak is zero.
    ///
    /// Clamped at zero: a peak at or below the background is not a peak, and a
    /// negative prominence would be a nonsense number to show an operator.
    pub fn prominence(&self) -> f64 {
        if self.peak_score <= 0.0 {
            return 0.0;
        }
        ((self.peak_score - self.background_score) / self.peak_score).clamp(0.0, 1.0)
    }
}

/// Cross-correlate `subtitle` against `audio` across every shift in
/// `±max_shift_bins`. **Pure.**
///
/// The score at each shift is the overlap of the two binary signals,
/// normalized by the number of active subtitle bins that actually landed
/// inside the audio grid at that shift. Normalizing by the *overlapped* count
/// rather than the total is what stops the sweep from preferring extreme
/// shifts that push most of the subtitle off the end of the timeline, where a
/// handful of surviving bins could otherwise score a perfect 1.0.
pub fn cross_correlate(audio: &[f32], subtitle: &[f32], max_shift_bins: i64) -> Correlation {
    let mut scores: Vec<(i64, f64)> = Vec::new();

    for shift in -max_shift_bins..=max_shift_bins {
        let mut overlap = 0.0f64;
        let mut considered = 0.0f64;
        for (i, &s) in subtitle.iter().enumerate() {
            if s <= 0.0 {
                continue;
            }
            let target = i as i64 + shift;
            if target < 0 || target as usize >= audio.len() {
                continue;
            }
            considered += 1.0;
            if audio[target as usize] > 0.0 {
                overlap += 1.0;
            }
        }
        // Require a meaningful fraction of the subtitle to still be on the
        // timeline; otherwise the shift is not a real candidate.
        let active_subtitle_bins = subtitle.iter().filter(|&&s| s > 0.0).count() as f64;
        if considered < active_subtitle_bins * 0.5 || considered == 0.0 {
            continue;
        }
        scores.push((shift, overlap / considered));
    }

    let Some(&(best_shift, peak)) = scores
        .iter()
        // `total_cmp` rather than `partial_cmp().unwrap()`: a NaN score (from
        // a degenerate input) must not panic the detector.
        .max_by(|a, b| a.1.total_cmp(&b.1))
    else {
        return Correlation {
            best_shift_bins: 0,
            peak_score: 0.0,
            background_score: 0.0,
        };
    };

    // The MEDIAN, not the mean, and the difference is not cosmetic.
    //
    // The score distribution is skewed: a long tail of poor scores from the
    // extreme shifts sits below a dense cluster of mediocre ones. That tail
    // drags the MEAN down, below the level two signals of this density
    // actually score against each other by coincidence — and since prominence
    // is `(peak - background) / peak`, an understated background OVERSTATES
    // prominence and makes every measurement look more decisive than it is.
    //
    // Measured on a broad-peak signal: median 0.66, mean 0.55, giving
    // prominence 0.34 and 0.45 respectively. The mean would promote that
    // measurement a whole confidence rung on a statistic, not on evidence.
    // The median is the robust estimate of the typical level, so it is the
    // conservative choice, which is the one this module wants.
    let mut sorted: Vec<f64> = scores.iter().map(|(_, score)| *score).collect();
    sorted.sort_by(|a, b| a.total_cmp(b));
    let background = sorted[sorted.len() / 2];

    Correlation {
        best_shift_bins: best_shift,
        peak_score: peak,
        background_score: background,
    }
}

/// Turn audio silences and subtitle cues into a proposal. **Pure.**
///
/// This is the function that decides what to claim, and it is deliberately
/// conservative: it downgrades to [`Confidence::Inconclusive`] on a weak peak
/// or a crowded field rather than reporting a number that looks authoritative.
/// It never returns `Ok` with a fabricated zero — every failure path is a
/// [`SyncError`].
pub fn propose_offset(
    silences: &[SilenceSpan],
    cues: &[CueSpan],
    duration_ms: i64,
) -> Result<OffsetProposal, SyncError> {
    if duration_ms <= 0 {
        return Err(SyncError::UnknownDuration);
    }

    let audio = activity_grid(silences, duration_ms);
    let subtitle = cues_to_activity_grid(cues, duration_ms);

    let audio_active_bins = audio.iter().filter(|&&v| v > 0.0).count();
    let subtitle_active_bins = subtitle.iter().filter(|&&v| v > 0.0).count();

    if audio_active_bins == 0 {
        return Err(SyncError::NoSpeechActivity);
    }
    if subtitle_active_bins == 0 {
        return Err(SyncError::NoSubtitleActivity);
    }

    let max_shift_bins = MAX_SHIFT_MS / BIN_MS;
    let correlation = cross_correlate(&audio, &subtitle, max_shift_bins);
    let prominence = correlation.prominence();
    let offset_ms = correlation.best_shift_bins * BIN_MS;

    // A maximum found at the EDGE of the searched range is not a peak.
    //
    // This rule matters more than it looks. A subtitle offset by more than
    // MAX_SHIFT_MS produces a correlation that is still climbing when the
    // search runs out of room, so the best score sits on the boundary — and it
    // can sit there with a perfectly respectable prominence, because it really
    // is the best of everything we looked at. Reporting that as a confident
    // offset would hand the operator a number that is not the answer but
    // merely the closest we were allowed to get to it, and they would apply
    // it. A boundary hit means "the true offset is probably outside the range
    // I searched", which is a different statement, and it is made explicitly.
    let at_search_boundary = correlation.best_shift_bins.abs() >= max_shift_bins - BOUNDARY_MARGIN_BINS;

    let confidence = if at_search_boundary {
        Confidence::Inconclusive
    } else if correlation.peak_score < MIN_PEAK_SCORE {
        Confidence::Inconclusive
    } else if prominence >= HIGH_PROMINENCE {
        Confidence::High
    } else if prominence >= LOW_PROMINENCE {
        Confidence::Low
    } else {
        Confidence::Inconclusive
    };

    let explanation = if at_search_boundary {
        format!(
            "INCONCLUSIVE — the best alignment found sits at the very edge of the ±{}s range \
             Muse searches, which means the real offset is probably LARGER than that and was \
             never actually located. The reported {offset_ms}ms is the boundary, not a \
             measurement. An offset this large usually means the subtitle is for a different \
             cut of the programme, which no constant shift can fix.",
            MAX_SHIFT_MS / 1000
        )
    } else {
        explain(confidence, offset_ms, correlation.peak_score, prominence)
    };

    Ok(OffsetProposal {
        offset_ms,
        confidence,
        peak_score: correlation.peak_score,
        prominence,
        resolution_ms: BIN_MS,
        explanation,
        audio_active_bins,
        subtitle_active_bins,
    })
}

/// Plain-language account of the measurement. **Pure.**
///
/// Written for an operator, not a log reader, and it never overstates: an
/// inconclusive result says so first, before it says any number.
fn explain(confidence: Confidence, offset_ms: i64, peak: f64, prominence: f64) -> String {
    let direction = match offset_ms {
        0 => "already aligned".to_string(),
        ms if ms > 0 => format!(
            "the subtitle appears {:.1}s EARLY and would need to be pushed later",
            ms as f64 / 1000.0
        ),
        ms => format!(
            "the subtitle appears {:.1}s LATE and would need to be pulled earlier",
            -ms as f64 / 1000.0
        ),
    };
    let stats = format!(
        "(speech/cue overlap {:.0}% at the best shift, {:.0}% above the coincidence \
         floor for signals of this density; measured to the nearest {BIN_MS}ms)",
        peak * 100.0,
        prominence * 100.0
    );

    match confidence {
        Confidence::High => format!("{direction} {stats}"),
        Confidence::Low => format!(
            "Uncertain: {direction}, but the measurement is not decisive — check it before \
             accepting {stats}"
        ),
        Confidence::Inconclusive => format!(
            "INCONCLUSIVE — the audio and this subtitle do not line up clearly at any shift, \
             so no offset should be trusted from this measurement. The best-scoring shift was \
             {offset_ms}ms, but that number is not meaningful here {stats}. This is most often \
             a subtitle for a different cut of the programme (extra scenes or a different \
             framerate), which no constant offset can fix."
        ),
    }
}

/// Convenience: the cue spans a subtitle text yields, for the detector.
/// Delegates to [`super::cues::parse_cue_spans`] so there is exactly one
/// timestamp parser in the crate.
pub fn cue_spans(text: &str, format: SubtitleFormat) -> Result<Vec<CueSpan>, super::cues::CueError> {
    super::cues::parse_cue_spans(text, format)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build synthetic silences that leave speech in the given ms ranges.
    fn silences_leaving_speech(speech: &[(i64, i64)], duration_ms: i64) -> Vec<SilenceSpan> {
        let mut out = Vec::new();
        let mut cursor = 0i64;
        for &(start, end) in speech {
            if start > cursor {
                out.push(SilenceSpan {
                    start_ms: cursor,
                    end_ms: start,
                });
            }
            cursor = end;
        }
        if cursor < duration_ms {
            out.push(SilenceSpan {
                start_ms: cursor,
                end_ms: duration_ms,
            });
        }
        out
    }

    fn cues(spans: &[(i64, i64)]) -> Vec<CueSpan> {
        spans
            .iter()
            .map(|&(start_ms, end_ms)| CueSpan { start_ms, end_ms })
            .collect()
    }

    // An irregular speech pattern. Irregularity matters: a perfectly periodic
    // signal correlates equally well at every period, which is exactly the
    // low-prominence case the confidence model must catch.
    const SPEECH: &[(i64, i64)] = &[
        (5_000, 9_000),
        (12_000, 13_500),
        (30_000, 38_000),
        (41_000, 42_000),
        (60_000, 71_000),
        (95_000, 99_500),
        (130_000, 141_000),
        (150_000, 152_000),
    ];
    const DURATION: i64 = 180_000;

    // ---------- silencedetect parsing ----------

    #[test]
    fn parses_a_real_shaped_silencedetect_report() {
        let stderr = "\
[silencedetect @ 0x55d1c0] silence_start: 0
[silencedetect @ 0x55d1c0] silence_end: 5.02 | silence_duration: 5.02
[silencedetect @ 0x55d1c0] silence_start: 9.014
[silencedetect @ 0x55d1c0] silence_end: 12.5 | silence_duration: 3.486
";
        let spans = parse_silencedetect(stderr);
        assert_eq!(
            spans,
            vec![
                SilenceSpan { start_ms: 0, end_ms: 5_020 },
                SilenceSpan { start_ms: 9_014, end_ms: 12_500 },
            ]
        );
    }

    #[test]
    fn an_unterminated_silence_span_is_dropped_not_closed_at_a_guessed_time() {
        let stderr = "silence_start: 0\nsilence_end: 5.0\nsilence_start: 170.0\n";
        let spans = parse_silencedetect(stderr);
        assert_eq!(spans.len(), 1, "the trailing open span must not be invented an end");
    }

    #[test]
    fn unparseable_silencedetect_lines_yield_nothing_rather_than_a_span_starting_at_zero() {
        assert!(parse_silencedetect("silence_start: banana\nsilence_end: also-banana\n").is_empty());

        // The case that actually discriminates, and that an earlier version of
        // this test missed: an unparseable START with a VALID end. If the
        // parser substituted 0 for the unreadable start, it would fabricate a
        // 0..5000ms silence that never existed — real structure invented out
        // of a parse failure, which then feeds the correlation.
        let spans = parse_silencedetect("silence_start: banana\nsilence_end: 5.0\n");
        assert!(
            spans.is_empty(),
            "an unreadable start must not become 0 and manufacture a span: {spans:?}"
        );

        // Symmetrically, a valid start with an unreadable end must not close
        // the span at 0 (or anywhere else guessed).
        let spans = parse_silencedetect("silence_start: 5.0\nsilence_end: banana\n");
        assert!(spans.is_empty(), "an unreadable end must not be guessed at: {spans:?}");

        // A negative or non-finite time is corruption, not a time.
        assert!(parse_silencedetect("silence_start: -1.0\nsilence_end: 5.0\n").is_empty());
        assert!(parse_silencedetect("silence_start: NaN\nsilence_end: 5.0\n").is_empty());
    }

    #[test]
    fn no_silence_in_the_report_is_a_typed_error_not_an_all_speech_signal() {
        // Directly exercises the rule, in a pure function, with no ffmpeg —
        // rather than only asserting the wording of an error nothing produces.
        let err = classify_silence_report(Vec::new()).unwrap_err();
        assert_eq!(err, SyncError::NoSilenceDetected);
        assert!(err.to_string().contains("no structure"));

        let real = vec![SilenceSpan { start_ms: 0, end_ms: 5_000 }];
        assert_eq!(classify_silence_report(real.clone()).unwrap(), real);
    }

    #[test]
    fn ffmpeg_output_with_no_silencedetect_lines_parses_to_nothing() {
        assert!(parse_silencedetect("frame= 100 fps=0.0 q=-1.0 size=N/A time=00:00:04.00\n").is_empty());
        assert!(parse_silencedetect("").is_empty());
    }

    // ---------- grids ----------

    #[test]
    fn the_audio_grid_is_the_complement_of_the_silences() {
        let silences = vec![SilenceSpan { start_ms: 0, end_ms: 1_000 }];
        let grid = activity_grid(&silences, 3_000);
        assert_eq!(grid.len(), 30);
        assert_eq!(grid[0], 0.0, "silent at the start");
        assert_eq!(grid[20], 1.0, "speech after the silence");
    }

    #[test]
    fn the_cue_grid_marks_only_the_spans_a_cue_is_on_screen() {
        let grid = cues_to_activity_grid(&cues(&[(1_000, 2_000)]), 5_000);
        assert_eq!(grid.len(), 50);
        assert_eq!(grid[0], 0.0);
        assert_eq!(grid[15], 1.0);
        assert_eq!(grid[45], 0.0);
    }

    #[test]
    fn grids_do_not_panic_on_degenerate_inputs() {
        assert!(activity_grid(&[], 0).is_empty());
        assert!(cues_to_activity_grid(&[], 0).is_empty());
        // Out-of-range spans must be clamped, not indexed with.
        let grid = cues_to_activity_grid(&cues(&[(-5_000, -1_000), (0, 999_999)]), 2_000);
        assert_eq!(grid.len(), 20);
        let grid = activity_grid(&[SilenceSpan { start_ms: -100, end_ms: 999_999 }], 2_000);
        assert_eq!(grid.len(), 20);
    }

    // ---------- correlation on synthetic signals ----------

    #[test]
    fn a_perfectly_synced_subtitle_yields_a_zero_offset_with_high_confidence() {
        let silences = silences_leaving_speech(SPEECH, DURATION);
        let proposal = propose_offset(&silences, &cues(SPEECH), DURATION).unwrap();
        assert_eq!(proposal.offset_ms, 0);
        assert_eq!(proposal.confidence, Confidence::High, "{}", proposal.explanation);
        assert!(proposal.explanation.contains("already aligned"));
    }

    #[test]
    fn a_subtitle_shifted_early_is_measured_with_the_right_sign_and_magnitude() {
        // Subtitle cues 3s EARLIER than the speech: they need to be pushed
        // LATER, so the proposal must be POSITIVE 3000ms.
        let silences = silences_leaving_speech(SPEECH, DURATION);
        let early: Vec<(i64, i64)> = SPEECH.iter().map(|&(s, e)| (s - 3_000, e - 3_000)).collect();
        let proposal = propose_offset(&silences, &cues(&early), DURATION).unwrap();
        assert_eq!(proposal.offset_ms, 3_000, "{}", proposal.explanation);
        assert_eq!(proposal.confidence, Confidence::High);
        assert!(proposal.explanation.contains("EARLY"));
    }

    #[test]
    fn a_subtitle_shifted_late_is_measured_with_a_negative_offset() {
        let silences = silences_leaving_speech(SPEECH, DURATION);
        let late: Vec<(i64, i64)> = SPEECH.iter().map(|&(s, e)| (s + 4_500, e + 4_500)).collect();
        let proposal = propose_offset(&silences, &cues(&late), DURATION).unwrap();
        assert_eq!(proposal.offset_ms, -4_500, "{}", proposal.explanation);
        assert!(proposal.explanation.contains("LATE"));
    }

    #[test]
    fn the_proposed_offset_actually_realigns_the_signals() {
        // End-to-end property, not just a number: applying the proposal must
        // move the cue grid onto the speech grid.
        let silences = silences_leaving_speech(SPEECH, DURATION);
        let early: Vec<(i64, i64)> = SPEECH.iter().map(|&(s, e)| (s - 2_700, e - 2_700)).collect();
        let proposal = propose_offset(&silences, &cues(&early), DURATION).unwrap();

        let corrected: Vec<(i64, i64)> = early
            .iter()
            .map(|&(s, e)| (s + proposal.offset_ms, e + proposal.offset_ms))
            .collect();
        let recheck = propose_offset(&silences, &cues(&corrected), DURATION).unwrap();
        assert_eq!(recheck.offset_ms, 0, "applying the proposal must leave nothing to correct");
    }

    #[test]
    fn an_unrelated_subtitle_is_reported_inconclusive_rather_than_given_a_confident_number() {
        // The safety-critical case. A subtitle for a different programme must
        // NOT come back as a confident offset — the operator would accept it.
        let silences = silences_leaving_speech(SPEECH, DURATION);
        // Dense, uniform cue coverage: correlates mediocrely with everything
        // and decisively with nothing.
        let noise: Vec<(i64, i64)> = (0..170)
            .map(|i| (i * 1_000, i * 1_000 + 900))
            .collect();
        let proposal = propose_offset(&silences, &cues(&noise), DURATION).unwrap();
        // Specifically INCONCLUSIVE, not merely "not High". An earlier version
        // of this test asserted only `!= High`, which let a mutation that
        // collapsed the inconclusive branch into Low survive — and `Low` still
        // renders as an offer the operator can accept in one click
        // (`worth_offering()`), which is exactly the outcome this case must
        // not produce.
        assert_eq!(
            proposal.confidence,
            Confidence::Inconclusive,
            "an unrelated subtitle must be reported inconclusive, not merely uncertain: {}",
            proposal.explanation
        );
        assert!(
            !proposal.confidence.worth_offering(),
            "an unrelated subtitle must not be offered for one-click acceptance"
        );
        assert!(proposal.explanation.starts_with("INCONCLUSIVE"), "{}", proposal.explanation);
    }

    /// The OTHER route to inconclusive: a subtitle that overlaps speech well
    /// (high peak score) but at every shift equally (low prominence).
    ///
    /// This is a real case, not a contrived one — a densely-talky programme
    /// with almost no silence gives a nearly-constant audio signal, so every
    /// shift scores about the same and the measurement localises nothing. It
    /// must report inconclusive, not `Low`: `Low` is offered to the operator
    /// for one-click acceptance, and there is nothing here to accept.
    ///
    /// It is also the branch that a mutation collapsing the final `else` into
    /// `Low` survives on if it is untested, which is exactly what happened
    /// before this test existed.
    #[test]
    fn a_high_overlap_that_localises_nothing_is_inconclusive_not_merely_low() {
        let duration = 120_000;
        // Almost wall-to-wall speech: two brief silences in two minutes.
        let silences = vec![
            SilenceSpan { start_ms: 40_000, end_ms: 40_600 },
            SilenceSpan { start_ms: 80_000, end_ms: 80_600 },
        ];
        // Dense cue coverage across the whole programme.
        let dense: Vec<(i64, i64)> = (0..118).map(|i| (i * 1_000, i * 1_000 + 950)).collect();

        let proposal = propose_offset(&silences, &cues(&dense), duration).unwrap();
        assert!(
            proposal.peak_score >= MIN_PEAK_SCORE,
            "this case must pass the absolute-quality floor, so it exercises the PROMINENCE \
             branch: peak {}",
            proposal.peak_score
        );
        assert!(
            proposal.prominence < LOW_PROMINENCE,
            "and it must fail the prominence check: prominence {}",
            proposal.prominence
        );
        assert_eq!(
            proposal.confidence,
            Confidence::Inconclusive,
            "an unlocalisable measurement must be inconclusive, not offered as Low: {}",
            proposal.explanation
        );
        assert!(!proposal.confidence.worth_offering());
    }

    /// The confidence ladder must have three distinct rungs that are actually
    /// reachable. A model that can only ever answer High or Low has silently
    /// lost its ability to say "I could not tell".
    #[test]
    fn the_inconclusive_rung_is_reachable_and_distinct_from_low() {
        let silences = silences_leaving_speech(SPEECH, DURATION);

        // Reachable: dense uniform noise.
        let noise: Vec<(i64, i64)> = (0..170).map(|i| (i * 1_000, i * 1_000 + 900)).collect();
        assert_eq!(
            propose_offset(&silences, &cues(&noise), DURATION).unwrap().confidence,
            Confidence::Inconclusive
        );

        // Distinct: a true match is High, so the ladder is not collapsed the
        // other way either.
        assert_eq!(
            propose_offset(&silences, &cues(SPEECH), DURATION).unwrap().confidence,
            Confidence::High
        );
    }

    #[test]
    fn an_inconclusive_proposal_says_so_before_it_says_a_number() {
        let text = explain(Confidence::Inconclusive, 2_500, 0.4, 0.01);
        assert!(text.starts_with("INCONCLUSIVE"), "{text}");
        assert!(
            text.contains("not meaningful"),
            "the explanation must disclaim the number it reports: {text}"
        );
        assert!(!Confidence::Inconclusive.worth_offering());
        assert!(Confidence::High.worth_offering());
        assert!(Confidence::Low.worth_offering());
    }

    #[test]
    fn the_search_is_bounded_and_an_offset_beyond_the_bound_is_not_invented() {
        // A 90s shift is outside MAX_SHIFT_MS. The detector must not report
        // 90s (it never looked there) and must not report high confidence.
        let silences = silences_leaving_speech(SPEECH, DURATION);
        let far: Vec<(i64, i64)> = SPEECH.iter().map(|&(s, e)| (s + 90_000, e + 90_000)).collect();
        let proposal = propose_offset(&silences, &cues(&far), DURATION).unwrap();
        assert!(
            proposal.offset_ms.abs() <= MAX_SHIFT_MS,
            "the proposal must stay inside the searched range"
        );
        assert_ne!(
            proposal.confidence,
            Confidence::High,
            "an out-of-range offset must not masquerade as a confident in-range one: {}",
            proposal.explanation
        );
    }

    #[test]
    fn extreme_shifts_that_push_the_subtitle_off_the_timeline_cannot_win() {
        // Without the overlap-fraction guard, a shift that leaves only a
        // couple of cue bins on the timeline scores a PERFECT 1.0 on those two
        // bins and wins outright — a confident answer derived from 5% of the
        // subtitle.
        //
        // The earlier version of this test used an all-ones audio grid, where
        // the honest shift also scores 1.0, so it passed with the guard
        // removed. This one is built so the cheat and the honest answer score
        // very differently.
        //
        // Audio: speech only in bins 0..5. Subtitle: 40 active bins at 50..90.
        // - Honest shifts overlap a few of those 40 bins -> low score.
        // - Shift -88 puts exactly bins 88,89 onto 0,1 and drops the other 38
        //   off the front -> considered = 2, overlap = 2/2 = 1.0.
        let mut audio = vec![0.0f32; 100];
        for bin in audio.iter_mut().take(5) {
            *bin = 1.0;
        }
        let mut subtitle = vec![0.0f32; 100];
        for bin in subtitle.iter_mut().take(90).skip(50) {
            *bin = 1.0;
        }

        let correlation = cross_correlate(&audio, &subtitle, 95);
        assert!(
            correlation.best_shift_bins > -85,
            "a shift that discards most of the subtitle must not be selected, got shift {} \
             scoring {}",
            correlation.best_shift_bins,
            correlation.peak_score
        );
        assert!(
            correlation.peak_score < 0.99,
            "a perfect score computed from a handful of surviving bins is not a real match"
        );
    }

    // ---------- fail closed ----------

    #[test]
    fn a_failed_detection_is_never_reported_as_a_zero_offset() {
        // The single most important rule in this module.
        let err = propose_offset(&[], &cues(SPEECH), 0).unwrap_err();
        assert_eq!(err, SyncError::UnknownDuration);

        // No speech in the audio at all (everything is silence).
        let all_silent = vec![SilenceSpan { start_ms: 0, end_ms: DURATION }];
        let err = propose_offset(&all_silent, &cues(SPEECH), DURATION).unwrap_err();
        assert_eq!(err, SyncError::NoSpeechActivity);

        // No cues.
        let silences = silences_leaving_speech(SPEECH, DURATION);
        let err = propose_offset(&silences, &[], DURATION).unwrap_err();
        assert_eq!(err, SyncError::NoSubtitleActivity);

        // Every one of those is an Err, not an Ok(offset_ms: 0).
        for err in [
            SyncError::UnknownDuration,
            SyncError::NoSpeechActivity,
            SyncError::NoSubtitleActivity,
            SyncError::NoSilenceDetected,
        ] {
            let msg = err.to_string();
            assert!(!msg.is_empty());
            assert!(
                !msg.contains("in sync") && !msg.contains("correct timing"),
                "a failure must never read as a clean bill of health: {msg}"
            );
        }
    }

    #[test]
    fn correlation_does_not_panic_on_empty_or_nan_inputs() {
        let c = cross_correlate(&[], &[], 10);
        assert_eq!(c.best_shift_bins, 0);
        assert_eq!(c.peak_score, 0.0);

        let c = cross_correlate(&[1.0, 1.0], &[], 10);
        assert_eq!(c.peak_score, 0.0);

        let c = cross_correlate(&[f32::NAN, 1.0, 1.0], &[1.0, 1.0, 1.0], 1);
        assert!(c.peak_score.is_finite() || c.peak_score == 0.0);
    }

    /// The background statistic is the MEDIAN, and that must stay true.
    ///
    /// Pinned directly because the alternative (the mean) is sitting right
    /// there and looks equivalent: on a skewed score distribution the mean
    /// runs LOWER than the median, which inflates prominence and can promote a
    /// measurement a whole confidence rung. Asserting the statistic itself is
    /// the only way to catch that swap — no end-to-end proposal test on
    /// realistic input distinguishes them reliably.
    #[test]
    fn the_background_is_the_median_not_the_mean_so_prominence_stays_conservative() {
        // A deliberately broad peak: one long speech block matched by one long
        // cue block, which is what real dialogue runs produce.
        let mut audio = vec![0.0f32; 1000];
        for bin in audio.iter_mut().take(600).skip(100) {
            *bin = 1.0;
        }
        let subtitle = audio.clone();

        let correlation = cross_correlate(&audio, &subtitle, 600);

        // Recompute both statistics over the same score set the function saw.
        let mut scores: Vec<f64> = Vec::new();
        let active = subtitle.iter().filter(|&&s| s > 0.0).count() as f64;
        for shift in -600i64..=600 {
            let mut overlap = 0.0f64;
            let mut considered = 0.0f64;
            for (i, &s) in subtitle.iter().enumerate() {
                if s <= 0.0 {
                    continue;
                }
                let target = i as i64 + shift;
                if target < 0 || target as usize >= audio.len() {
                    continue;
                }
                considered += 1.0;
                if audio[target as usize] > 0.0 {
                    overlap += 1.0;
                }
            }
            if considered < active * 0.5 || considered == 0.0 {
                continue;
            }
            scores.push(overlap / considered);
        }
        let mut sorted = scores.clone();
        sorted.sort_by(|a, b| a.total_cmp(b));
        let median = sorted[sorted.len() / 2];
        let mean = scores.iter().sum::<f64>() / scores.len() as f64;

        assert!(
            (correlation.background_score - median).abs() < 1e-9,
            "the background must be the median ({median}), got {}",
            correlation.background_score
        );
        assert!(
            median > mean + 0.05,
            "this fixture must actually distinguish the two statistics \
             (median {median}, mean {mean})"
        );
        assert!(
            correlation.prominence() < (correlation.peak_score - mean) / correlation.peak_score,
            "the median must give the MORE conservative prominence of the two"
        );
    }

    #[test]
    fn prominence_is_zero_for_a_zero_peak_and_never_negative() {
        let c = Correlation {
            best_shift_bins: 0,
            peak_score: 0.0,
            background_score: 0.0,
        };
        assert_eq!(c.prominence(), 0.0);
        let c = Correlation {
            best_shift_bins: 0,
            peak_score: 0.5,
            background_score: 0.9,
        };
        assert_eq!(
            c.prominence(),
            0.0,
            "a background above the peak must clamp, not produce a negative prominence"
        );
    }

    // ---------- the impure boundary's argv ----------

    #[test]
    fn the_ffmpeg_argv_decodes_and_measures_but_writes_nothing() {
        let args = build_silencedetect_args(Path::new("/library/Movie/Movie.mkv"));
        assert!(args.contains(&"-nostdin".to_string()), "must never prompt");
        // `-f null -` is what makes this a measurement, not an encode. If this
        // ever became a real output path, the detector would start writing
        // files during what the operator asked to be a read-only check.
        let f_idx = args.iter().position(|a| a == "-f").expect("-f present");
        assert_eq!(args[f_idx + 1], "null", "the output format must be the null muxer");
        assert_eq!(args.last().unwrap(), "-", "output must go to the null sink");
        assert!(
            args.iter().any(|a| a.starts_with("silencedetect=")),
            "the silencedetect filter is the whole point: {args:?}"
        );
        // The input must be mapped audio-only; decoding video would cost
        // minutes of CPU for a measurement that ignores it.
        let map_idx = args.iter().position(|a| a == "-map").expect("-map present");
        assert_eq!(args[map_idx + 1], "0:a:0?");
        // No `-y`, no output path: nothing can be overwritten.
        assert!(!args.contains(&"-y".to_string()));
    }

    #[test]
    fn a_missing_ffmpeg_binary_is_a_typed_error_not_a_zero_offset() {
        // Exercises the real spawn path against a binary that cannot exist,
        // so no ffmpeg is required on the test host.
        let err = extract_speech_activity(
            "muse-subtitles-no-such-ffmpeg-binary",
            Path::new("/nonexistent/file.mkv"),
        )
        .unwrap_err();
        assert!(matches!(err, SyncError::ToolMissing { .. }), "got {err:?}");
        assert!(err.to_string().contains("NOT the same as the timing being correct"));
    }
}
