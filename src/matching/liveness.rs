//! MUSEL-C2: cheap byte-level "liveness" heuristics on extracted JPEG
//! stills, to catch a file that decodes to dead/blank/slate content rather
//! than real footage -- one of the three signals [`crate::matching::verify`]
//! combines into a [`crate::matching::verify::MatchVerdict`].
//!
//! **Deliberately does NOT decode JPEG pixels.** `image`/a JPEG decoder is
//! not currently a dependency of this crate, and full pixel decode is more
//! than this signal needs. Instead this operates directly on the still's
//! raw entropy-coded byte stream, which already carries the signal cheaply:
//! a solid-color/black/slate frame's DCT blocks are almost entirely "all
//! zero AC coefficients", so the JPEG entropy coder emits a highly
//! repetitive run of end-of-block symbols -- the *compressed* byte stream
//! itself ends up dominated by a narrow set of repeated byte values and has
//! much lower byte-value variance than a frame with real, varied content.
//! Mean/variance and a dominant-byte-ratio over the raw bytes are a
//! decent, dependency-free proxy for "did this decode to something real"
//! without ever needing to decode a pixel.
//!
//! This is a **documented simplification, not a substitute for true luma
//! decode** (see the EDGE CASE note on visually-atypical-but-correct
//! titles, e.g. black-and-white or otherwise low-contrast content, in
//! `specs/S119b-muse-library-scan-matching.md`'s MUSEL-C2 item). If this
//! proves too coarse in production, swap in a lightweight decode-and-
//! downsample step behind the same [`StillStats`]/[`LivenessOutcome`] API
//! without touching call sites in `verify.rs`.

use crate::matching::stills::Still;

/// Byte-level statistics for one still, used to judge whether it plausibly
/// decodes to real, varied content.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct StillStats {
    pub len: usize,
    pub mean: f64,
    pub variance: f64,
    /// Fraction of bytes equal to the single most common byte value in the
    /// still. Close to 1.0 for a highly repetitive (near-uniform) stream.
    pub dominant_byte_ratio: f64,
}

/// Below this raw-byte variance, a still is treated as suspiciously
/// uniform (a black/solid-color/slate frame rather than real content).
/// Tuned against synthetic fixtures in this module's tests, not real
/// footage -- see the module doc's note on visually-atypical titles if
/// this needs loosening once real stills are observed in production.
const UNIFORM_VARIANCE_THRESHOLD: f64 = 400.0;

/// Above this dominant-byte-ratio, a still is treated as suspiciously
/// uniform even when variance alone doesn't trip (a highly repetitive
/// compressed stream where a single byte value dominates).
const UNIFORM_DOMINANT_BYTE_RATIO_THRESHOLD: f64 = 0.35;

/// Below this many bytes, a still is too small to plausibly be a decoded
/// real frame (a truncated/near-empty capture) -- treated as uniform
/// rather than a separate case, since it can't carry real content either.
const MIN_PLAUSIBLE_STILL_BYTES: usize = 256;

/// Compute [`StillStats`] for one still. Pure/sync -- no I/O, no decode --
/// so it's directly unit-testable on synthetic byte vectors that don't
/// need to be real JPEGs (this heuristic never actually decodes them).
pub fn analyze_still(still: &Still) -> StillStats {
    let bytes = &still.bytes;
    if bytes.is_empty() {
        return StillStats {
            len: 0,
            mean: 0.0,
            variance: 0.0,
            dominant_byte_ratio: 1.0,
        };
    }

    let len = bytes.len();
    let sum: u64 = bytes.iter().map(|&b| b as u64).sum();
    let mean = sum as f64 / len as f64;
    let variance = bytes
        .iter()
        .map(|&b| {
            let d = b as f64 - mean;
            d * d
        })
        .sum::<f64>()
        / len as f64;

    let mut counts = [0u32; 256];
    for &b in bytes.iter() {
        counts[b as usize] += 1;
    }
    let max_count = counts.into_iter().max().unwrap_or(0);
    let dominant_byte_ratio = max_count as f64 / len as f64;

    StillStats { len, mean, variance, dominant_byte_ratio }
}

fn is_uniform(stats: &StillStats) -> bool {
    stats.len < MIN_PLAUSIBLE_STILL_BYTES
        || stats.variance < UNIFORM_VARIANCE_THRESHOLD
        || stats.dominant_byte_ratio > UNIFORM_DOMINANT_BYTE_RATIO_THRESHOLD
}

/// Two stills are "the same frame" for liveness purposes if their basic
/// stats are all close -- a coarse but cheap proxy for "no real motion
/// between sample points" (a stuck/frozen source, or a file that repeats
/// one frame throughout).
fn stills_look_identical(a: &StillStats, b: &StillStats) -> bool {
    let bigger_len = a.len.max(b.len) as f64;
    let len_close = bigger_len == 0.0 || (a.len as f64 - b.len as f64).abs() <= bigger_len * 0.02;
    let mean_close = (a.mean - b.mean).abs() < 1.0;
    let var_close = (a.variance - b.variance).abs() < 5.0;
    len_close && mean_close && var_close
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LivenessOutcome {
    /// At least some sampled stills show real, varied content.
    Live,
    /// Every sampled still is near-uniform/blank (black frame, solid
    /// color, slate, or a near-empty capture).
    Uniform,
    /// Multiple stills were sampled and every one of them looks like the
    /// same frame (no variation across the sample points at all).
    AllIdentical,
    /// No stills were available to judge (extraction failed/skipped) --
    /// distinct from `Uniform`: this says nothing about the content
    /// itself, just that there was nothing to check.
    Empty,
}

#[derive(Debug, Clone, PartialEq)]
pub struct LivenessVerdict {
    pub outcome: LivenessOutcome,
    pub reasons: Vec<String>,
}

/// Judge liveness across a set of sample stills. Never panics on empty or
/// malformed input -- an empty slice is [`LivenessOutcome::Empty`], not an
/// error.
pub fn check_liveness(stills: &[Still]) -> LivenessVerdict {
    if stills.is_empty() {
        return LivenessVerdict {
            outcome: LivenessOutcome::Empty,
            reasons: vec!["no stills available for liveness check".to_string()],
        };
    }

    let stats: Vec<StillStats> = stills.iter().map(analyze_still).collect();
    let mut reasons = Vec::new();

    let uniform_count = stats.iter().filter(|s| is_uniform(s)).count();
    if uniform_count == stats.len() {
        reasons.push(format!(
            "all {} sampled stills are near-uniform/blank (low byte variance or a dominant repeated byte)",
            stats.len()
        ));
        return LivenessVerdict { outcome: LivenessOutcome::Uniform, reasons };
    } else if uniform_count > 0 {
        reasons.push(format!(
            "{} of {} sampled stills are near-uniform/blank",
            uniform_count,
            stats.len()
        ));
    }

    if stats.len() >= 2 && stats.windows(2).all(|w| stills_look_identical(&w[0], &w[1])) {
        reasons.push("all sampled stills are near-identical (no variation across sample points)".to_string());
        return LivenessVerdict { outcome: LivenessOutcome::AllIdentical, reasons };
    }

    reasons.push(format!("{} sampled stills show live, varied content", stats.len()));
    LivenessVerdict { outcome: LivenessOutcome::Live, reasons }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A synthetic "live" still: varied byte content whose mean/variance
    /// genuinely differ per `seed` (a sawtooth shifted by `seed`, not
    /// wrapped mod 256 -- a modulo-wrapped shift just rotates the same
    /// 0..255 sweep and leaves the arithmetic mean almost unchanged across
    /// seeds, which would make every "different" still look identical to
    /// `stills_look_identical`). Not a real JPEG -- this heuristic never
    /// decodes pixels, so a deterministic varied byte pattern is a
    /// faithful stand-in for test purposes.
    fn live_bytes(seed: i64) -> Vec<u8> {
        (0..4000_i64)
            .map(|i| ((i % 200) + seed * 25).clamp(0, 255) as u8)
            .collect()
    }

    fn black_bytes() -> Vec<u8> {
        vec![0u8; 3000]
    }

    fn still(bytes: Vec<u8>, ts: i64) -> Still {
        Still { bytes, timestamp_ms: ts }
    }

    #[test]
    fn analyze_still_reports_high_variance_for_varied_bytes() {
        let stats = analyze_still(&still(live_bytes(1), 0));
        assert!(stats.variance > UNIFORM_VARIANCE_THRESHOLD);
        assert!(stats.dominant_byte_ratio < UNIFORM_DOMINANT_BYTE_RATIO_THRESHOLD);
    }

    #[test]
    fn analyze_still_reports_low_variance_for_black_bytes() {
        let stats = analyze_still(&still(black_bytes(), 0));
        assert_eq!(stats.variance, 0.0);
        assert_eq!(stats.dominant_byte_ratio, 1.0);
    }

    #[test]
    fn analyze_still_empty_bytes_does_not_panic() {
        let stats = analyze_still(&still(Vec::new(), 0));
        assert_eq!(stats.len, 0);
    }

    #[test]
    fn check_liveness_empty_input_is_empty_outcome() {
        let verdict = check_liveness(&[]);
        assert_eq!(verdict.outcome, LivenessOutcome::Empty);
    }

    #[test]
    fn check_liveness_varied_stills_are_live() {
        let stills = vec![still(live_bytes(1), 0), still(live_bytes(2), 1000), still(live_bytes(3), 2000)];
        let verdict = check_liveness(&stills);
        assert_eq!(verdict.outcome, LivenessOutcome::Live);
    }

    #[test]
    fn check_liveness_all_black_stills_are_uniform() {
        let stills = vec![still(black_bytes(), 0), still(black_bytes(), 1000), still(black_bytes(), 2000)];
        let verdict = check_liveness(&stills);
        assert_eq!(verdict.outcome, LivenessOutcome::Uniform);
    }

    #[test]
    fn check_liveness_mixed_uniform_and_live_is_still_live_but_reports_the_count() {
        let stills = vec![still(black_bytes(), 0), still(live_bytes(2), 1000), still(live_bytes(3), 2000)];
        let verdict = check_liveness(&stills);
        assert_eq!(verdict.outcome, LivenessOutcome::Live);
        assert!(verdict.reasons.iter().any(|r| r.contains("1 of 3")));
    }

    #[test]
    fn check_liveness_identical_live_stills_are_all_identical() {
        let bytes = live_bytes(2);
        let stills = vec![still(bytes.clone(), 0), still(bytes.clone(), 1000), still(bytes, 2000)];
        let verdict = check_liveness(&stills);
        assert_eq!(verdict.outcome, LivenessOutcome::AllIdentical);
    }

    #[test]
    fn check_liveness_single_live_still_cannot_be_all_identical() {
        // Only one sample point -- nothing to compare against, so it's a
        // plain Live verdict rather than a spurious AllIdentical.
        let verdict = check_liveness(&[still(live_bytes(1), 0)]);
        assert_eq!(verdict.outcome, LivenessOutcome::Live);
    }
}
