//! MUSEL-C2: "liveness" heuristics on extracted JPEG stills, to catch a
//! file that decodes to dead/blank/slate content rather than real footage
//! -- one of the three signals [`crate::matching::verify`] combines into a
//! [`crate::matching::verify::MatchVerdict`].
//!
//! **Decodes real pixels.** An earlier version of this module deliberately
//! avoided a JPEG-decode dependency, instead running mean/variance/
//! dominant-byte-ratio stats directly over the still's *compressed* byte
//! stream (the theory being that a solid-color frame's DCT blocks are
//! almost all "zero AC coefficients", so the entropy coder should emit a
//! visibly more repetitive byte stream). **That was measured against a
//! REAL encoded JPEG and proven wrong** (review finding: codex, MUSEL-C2):
//! JPEG's Huffman/arithmetic entropy coding makes the *compressed* byte
//! stream look statistically noisy/varied regardless of image content --
//! that's the point of entropy coding, and it swamps the signal. Measured
//! evidence (see this module's `debug_real_jpeg_stats`-derived numbers,
//! captured in the MUSEL-C2 worktree report): a real solid-black 64x64
//! JPEG's raw compressed bytes had variance ~6560 and a real
//! varied/textured 64x64 JPEG had variance ~6070 -- statistically
//! indistinguishable, and both far above what a byte-stream-only
//! "uniform" threshold could plausibly use. The byte-level proxy simply
//! does not work.
//!
//! So this module decodes each still to real luma (grayscale) pixels via
//! the `image` crate (`jpeg`-feature only -- no other format support) and
//! computes mean/variance/dominant-value stats over the actual decoded
//! pixel values, which DO carry the content signal directly: a genuinely
//! solid-black image decodes to a luma buffer that actually is uniform. A
//! still whose bytes don't decode as a JPEG at all (corrupt/truncated
//! capture) is treated as maximally uniform -- "decodes to garbage" fails
//! liveness, per the MUSEL-C2 spec's EDGE CASES, rather than panicking or
//! silently skipping.

use crate::matching::stills::Still;

/// Pixel-level statistics for one still, used to judge whether it
/// genuinely decodes to real, varied visual content.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct StillStats {
    /// Number of luma pixels the still decoded to (0 if decode failed).
    pub len: usize,
    /// Whether `still.bytes` decoded as a JPEG at all. `false` means every
    /// other field is a maximally-uniform placeholder, not a real
    /// measurement -- see [`analyze_still`].
    pub decoded: bool,
    pub mean: f64,
    pub variance: f64,
    /// Fraction of pixels equal to the single most common luma value in
    /// the still. Close to 1.0 for a genuinely uniform/flat image.
    pub dominant_byte_ratio: f64,
}

/// Below this luma variance, a still is treated as suspiciously uniform
/// (a black/solid-color/slate frame rather than real content). Tuned and
/// validated against REAL encoded JPEG fixtures in this module's tests
/// (`real_black_jpeg`/`real_varied_jpeg`), not synthetic bytes -- see the
/// module doc comment.
const UNIFORM_VARIANCE_THRESHOLD: f64 = 25.0;

/// Above this dominant-pixel-ratio, a still is treated as suspiciously
/// uniform even when variance alone doesn't trip (almost every pixel is
/// the same luma value).
const UNIFORM_DOMINANT_BYTE_RATIO_THRESHOLD: f64 = 0.6;

/// Below this many decoded pixels, a still is too small to plausibly
/// carry real content (a 1x1 or otherwise degenerate decode) -- treated
/// as uniform rather than a separate case.
const MIN_PLAUSIBLE_STILL_PIXELS: usize = 64;

/// Compute [`StillStats`] for one still by decoding it as a JPEG and
/// analyzing its luma (grayscale) pixels. Never panics -- a decode
/// failure (empty bytes, corrupt/truncated capture, not actually a JPEG)
/// produces `decoded: false` and maximally-uniform placeholder stats
/// (fails liveness, per the "garbage decodes fail liveness" edge case),
/// rather than propagating an error or crashing the caller.
pub fn analyze_still(still: &Still) -> StillStats {
    let placeholder_uniform = StillStats { len: 0, decoded: false, mean: 0.0, variance: 0.0, dominant_byte_ratio: 1.0 };

    if still.bytes.is_empty() {
        return placeholder_uniform;
    }

    let Ok(decoded) = image::load_from_memory_with_format(&still.bytes, image::ImageFormat::Jpeg) else {
        return placeholder_uniform;
    };

    let luma = decoded.to_luma8();
    let pixels = luma.as_raw();
    if pixels.is_empty() {
        return placeholder_uniform;
    }

    let len = pixels.len();
    let sum: u64 = pixels.iter().map(|&p| p as u64).sum();
    let mean = sum as f64 / len as f64;
    let variance = pixels
        .iter()
        .map(|&p| {
            let d = p as f64 - mean;
            d * d
        })
        .sum::<f64>()
        / len as f64;

    let mut counts = [0u32; 256];
    for &p in pixels.iter() {
        counts[p as usize] += 1;
    }
    let max_count = counts.into_iter().max().unwrap_or(0);
    let dominant_byte_ratio = max_count as f64 / len as f64;

    StillStats { len, decoded: true, mean, variance, dominant_byte_ratio }
}

fn is_uniform(stats: &StillStats) -> bool {
    !stats.decoded
        || stats.len < MIN_PLAUSIBLE_STILL_PIXELS
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

/// Test-only REAL JPEG fixture generation, shared with
/// `matching::verify`'s tests (`pub(crate)` so `verify.rs` can build the
/// same authentic stills its own mismatch-harness tests need, rather than
/// duplicating the encoder setup or -- worse -- falling back to
/// non-JPEG synthetic bytes that this module's decode-based analysis
/// would just reject as undecodable).
#[cfg(test)]
pub(crate) mod fixtures {
    use crate::matching::stills::Still;

    /// Encode a REAL JPEG (headers, quant tables, real Huffman-coded scan
    /// data -- exactly the shape ffmpeg's `mjpeg` output has) via
    /// `image`'s encoder, from an explicit per-pixel function.
    pub(crate) fn encode_real_jpeg(width: u32, height: u32, pixel: impl Fn(u32, u32) -> [u8; 3]) -> Vec<u8> {
        let mut img = image::RgbImage::new(width, height);
        for y in 0..height {
            for x in 0..width {
                img.put_pixel(x, y, image::Rgb(pixel(x, y)));
            }
        }
        let mut buf = Vec::new();
        let mut encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut buf, 85);
        encoder.encode_image(&img).expect("jpeg encode should succeed");
        buf
    }

    /// A real, solid-black encoded JPEG -- what ffmpeg emits for an
    /// actual black/blank frame.
    pub(crate) fn real_black_jpeg() -> Vec<u8> {
        encode_real_jpeg(64, 64, |_, _| [0, 0, 0])
    }

    /// A real, visually varied/textured encoded JPEG -- what ffmpeg emits
    /// for a frame with genuine content. `seed` shifts the pattern so
    /// several calls produce genuinely different (not just re-encoded
    /// identical) images.
    pub(crate) fn real_varied_jpeg(seed: u32) -> Vec<u8> {
        encode_real_jpeg(64, 64, |x, y| {
            [
                (((x * 7 + y * 13 + seed * 41) % 256) as u8),
                (((x * 3 + y * 29 + seed * 17) % 256) as u8),
                (((x + y * 5 + seed * 61) % 256) as u8),
            ]
        })
    }

    pub(crate) fn real_still(bytes: Vec<u8>, ts: i64) -> Still {
        Still { bytes, timestamp_ms: ts }
    }
}

#[cfg(test)]
mod tests {
    use super::fixtures::{real_black_jpeg, real_still, real_varied_jpeg};
    use super::*;

    #[test]
    fn analyze_still_decodes_real_varied_jpeg_with_high_variance() {
        let stats = analyze_still(&real_still(real_varied_jpeg(1), 0));
        assert!(stats.decoded);
        assert!(
            stats.variance > UNIFORM_VARIANCE_THRESHOLD,
            "expected varied-image variance above {UNIFORM_VARIANCE_THRESHOLD}, got {}",
            stats.variance
        );
        assert!(stats.dominant_byte_ratio < UNIFORM_DOMINANT_BYTE_RATIO_THRESHOLD);
    }

    #[test]
    fn analyze_still_decodes_real_black_jpeg_with_low_variance() {
        let stats = analyze_still(&real_still(real_black_jpeg(), 0));
        assert!(stats.decoded);
        assert!(
            stats.variance < UNIFORM_VARIANCE_THRESHOLD,
            "expected black-frame variance below {UNIFORM_VARIANCE_THRESHOLD}, got {}",
            stats.variance
        );
        assert!(stats.dominant_byte_ratio > UNIFORM_DOMINANT_BYTE_RATIO_THRESHOLD);
    }

    /// THE discrimination proof requested in review (codex, MUSEL-C2): a
    /// real encoded black JPEG and a real encoded varied JPEG must
    /// classify oppositely under `is_uniform`. This is what the earlier
    /// compressed-byte-stream proxy failed (see the module doc comment
    /// for the measured counter-evidence); pixel-decode-based stats pass.
    #[test]
    fn analyze_still_genuinely_discriminates_real_black_from_real_varied_jpeg() {
        let black = analyze_still(&real_still(real_black_jpeg(), 0));
        let varied = analyze_still(&real_still(real_varied_jpeg(1), 0));

        assert!(is_uniform(&black), "a real black JPEG must classify as uniform: {black:?}");
        assert!(!is_uniform(&varied), "a real varied JPEG must NOT classify as uniform: {varied:?}");
    }

    #[test]
    fn analyze_still_empty_bytes_does_not_panic() {
        let stats = analyze_still(&real_still(Vec::new(), 0));
        assert_eq!(stats.len, 0);
        assert!(!stats.decoded);
    }

    #[test]
    fn analyze_still_undecodable_bytes_fail_liveness_not_panic() {
        // Not a JPEG at all (garbage/truncated capture) -- must degrade
        // to a maximally-uniform, non-decoded result, per the MUSEL-C2
        // spec's "a file that decodes to garbage ... fails liveness"
        // edge case, never a panic.
        let stats = analyze_still(&real_still(vec![0xDE, 0xAD, 0xBE, 0xEF, 1, 2, 3], 0));
        assert!(!stats.decoded);
        assert!(is_uniform(&stats));
    }

    #[test]
    fn check_liveness_empty_input_is_empty_outcome() {
        let verdict = check_liveness(&[]);
        assert_eq!(verdict.outcome, LivenessOutcome::Empty);
    }

    #[test]
    fn check_liveness_varied_stills_are_live() {
        let stills = vec![
            real_still(real_varied_jpeg(1), 0),
            real_still(real_varied_jpeg(2), 1000),
            real_still(real_varied_jpeg(3), 2000),
        ];
        let verdict = check_liveness(&stills);
        assert_eq!(verdict.outcome, LivenessOutcome::Live);
    }

    #[test]
    fn check_liveness_all_black_stills_are_uniform() {
        let stills = vec![
            real_still(real_black_jpeg(), 0),
            real_still(real_black_jpeg(), 1000),
            real_still(real_black_jpeg(), 2000),
        ];
        let verdict = check_liveness(&stills);
        assert_eq!(verdict.outcome, LivenessOutcome::Uniform);
    }

    #[test]
    fn check_liveness_mixed_uniform_and_live_is_still_live_but_reports_the_count() {
        let stills = vec![
            real_still(real_black_jpeg(), 0),
            real_still(real_varied_jpeg(2), 1000),
            real_still(real_varied_jpeg(3), 2000),
        ];
        let verdict = check_liveness(&stills);
        assert_eq!(verdict.outcome, LivenessOutcome::Live);
        assert!(verdict.reasons.iter().any(|r| r.contains("1 of 3")));
    }

    #[test]
    fn check_liveness_identical_live_stills_are_all_identical() {
        let bytes = real_varied_jpeg(2);
        let stills = vec![
            real_still(bytes.clone(), 0),
            real_still(bytes.clone(), 1000),
            real_still(bytes, 2000),
        ];
        let verdict = check_liveness(&stills);
        assert_eq!(verdict.outcome, LivenessOutcome::AllIdentical);
    }

    #[test]
    fn check_liveness_single_live_still_cannot_be_all_identical() {
        // Only one sample point -- nothing to compare against, so it's a
        // plain Live verdict rather than a spurious AllIdentical.
        let verdict = check_liveness(&[real_still(real_varied_jpeg(1), 0)]);
        assert_eq!(verdict.outcome, LivenessOutcome::Live);
    }
}
