//! MUSEL-C1: the ffmpeg sample-still extraction primitive — the raw material
//! MUSEL-C2's `verify_match` will judge for "is this really the identified
//! title". Read-only on the input media file; bytes are captured in memory
//! only (never written to disk beside the media).
//!
//! Split the same way [`crate::streaming`] is: [`crate::streaming::ffmpeg`]'s
//! `build_still_args` is the pure arg builder (unit-tested without ever
//! invoking ffmpeg); this module is the one impure layer that actually
//! spawns a process per sample timestamp and reads its stdout.

use std::process::Stdio;

use tokio::io::AsyncReadExt;
use tokio::process::Command;

use crate::error::{MuseError, MuseResult};
use crate::streaming::ffmpeg;

/// One extracted sample still: the raw JPEG bytes ffmpeg wrote to stdout,
/// plus the timestamp (milliseconds into the file) it was taken at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Still {
    pub bytes: Vec<u8>,
    pub timestamp_ms: i64,
}

/// Extract up to `n` sample stills from `file_path`, spread across
/// `runtime_ms`, by spawning ffmpeg once per timestamp (bounded — never more
/// than `n` spawns).
///
/// Takes `ffmpeg_path` explicitly (rather than assuming a bare `"ffmpeg"` on
/// `PATH`) so this stays consistent with `crate::streaming`'s
/// `Config::ffmpeg_path` convention and never hardcodes an infra-specific
/// binary location.
///
/// A decode failure on one timestamp is logged and skipped — it does not
/// abort the rest. If the ffmpeg *binary itself* is missing, that's a
/// deployment-level gap (not a per-file problem), so it's detected on the
/// first spawn attempt and surfaced once as [`MuseError::NotImplemented`]
/// (the same graceful posture `crate::streaming` uses for a missing binary)
/// rather than repeating the same "not found" failure `n` times.
pub async fn extract_sample_stills(
    ffmpeg_path: &str,
    file_path: &str,
    runtime_ms: i64,
    n: usize,
) -> MuseResult<Vec<Still>> {
    let timestamps = spread_timestamps(runtime_ms, n);
    if timestamps.is_empty() {
        return Ok(Vec::new());
    }

    let mut stills = Vec::with_capacity(timestamps.len());
    for timestamp_ms in timestamps {
        match capture_still(ffmpeg_path, file_path, timestamp_ms).await {
            Ok(bytes) => stills.push(Still { bytes, timestamp_ms }),
            Err(MuseError::NotImplemented) => {
                // The binary itself is absent — every subsequent attempt
                // would fail identically, so stop here and let the caller
                // treat this as "no stills available on this deployment"
                // rather than a per-file matching problem.
                return Err(MuseError::NotImplemented);
            }
            Err(e) => {
                tracing::warn!(file_path, timestamp_ms, error = %e, "failed extracting sample still; skipping this timestamp");
            }
        }
    }

    Ok(stills)
}

/// Spawn ffmpeg for a single still, capture its stdout (the MJPEG bytes) in
/// memory, and classify a spawn failure using the same
/// [`ffmpeg::classify_spawn_error`] helper `crate::streaming` uses.
async fn capture_still(ffmpeg_path: &str, file_path: &str, seek_ms: i64) -> MuseResult<Vec<u8>> {
    let mut child = Command::new(ffmpeg_path)
        .args(ffmpeg::build_still_args(file_path, seek_ms))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| match ffmpeg::classify_spawn_error(&e) {
            ffmpeg::StreamAvailability::BinaryMissing => MuseError::NotImplemented,
            ffmpeg::StreamAvailability::SpawnError => {
                MuseError::ServiceUnavailable(format!("ffmpeg failed to start: {e}"))
            }
        })?;

    let Some(mut stdout) = child.stdout.take() else {
        return Err(MuseError::ServiceUnavailable(
            "ffmpeg child had no stdout pipe".to_string(),
        ));
    };

    let mut bytes = Vec::new();
    stdout
        .read_to_end(&mut bytes)
        .await
        .map_err(|e| MuseError::ServiceUnavailable(format!("failed reading ffmpeg stdout: {e}")))?;

    let status = child
        .wait()
        .await
        .map_err(|e| MuseError::ServiceUnavailable(format!("ffmpeg wait failed: {e}")))?;

    if !status.success() || bytes.is_empty() {
        return Err(MuseError::ServiceUnavailable(format!(
            "ffmpeg produced no still at {seek_ms}ms (exit status: {status:?})"
        )));
    }

    Ok(bytes)
}

/// Compute `n` sample timestamps (milliseconds) spread across
/// `runtime_ms`, avoiding the very start/end of the file (roughly 10%..90%
/// of the runtime, so a still never lands on a black leader/credits frame or
/// past EOF).
///
/// Pure/sync so it's directly unit-testable without a real media file or
/// ffmpeg — mirrors `ffmpeg::classify_spawn_error`'s "pure logic, gate-host
/// safe" pattern.
///
/// Returns an empty `Vec` (no stills requested/extractable) when `n == 0` or
/// `runtime_ms` is non-positive (unknown/invalid runtime — there's no safe
/// way to spread timestamps across a runtime we don't have; the caller
/// treats "no stills" as inconclusive, never a crash).
fn spread_timestamps(runtime_ms: i64, n: usize) -> Vec<i64> {
    if n == 0 || runtime_ms <= 0 {
        return Vec::new();
    }

    // Clamp so every timestamp lands strictly before EOF even for a very
    // short runtime (e.g. runtime_ms == 1 => max_ts == 0).
    let max_ts = runtime_ms - 1;

    (0..n)
        .map(|i| {
            let frac = if n == 1 {
                0.5
            } else {
                0.1 + (i as f64) * (0.8 / (n as f64 - 1.0))
            };
            let ts = (frac * runtime_ms as f64).round() as i64;
            ts.clamp(0, max_ts)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spread_timestamps_covers_middle_of_runtime_avoiding_edges() {
        // 90-minute runtime, 5 stills spread ~10%..90%.
        let runtime_ms = 90 * 60_000;
        let ts = spread_timestamps(runtime_ms, 5);
        assert_eq!(ts.len(), 5);

        // Ascending, and none at the very start or end.
        assert!(ts.windows(2).all(|w| w[0] <= w[1]));
        assert!(ts[0] > 0);
        assert!(ts[ts.len() - 1] < runtime_ms);

        // Roughly 10%/30%/50%/70%/90% of the runtime.
        let expected = [0.10, 0.30, 0.50, 0.70, 0.90];
        for (t, frac) in ts.iter().zip(expected.iter()) {
            let expected_ms = (frac * runtime_ms as f64).round() as i64;
            assert!((t - expected_ms).abs() <= 1, "{t} vs {expected_ms}");
        }
    }

    #[test]
    fn spread_timestamps_single_still_lands_mid_runtime() {
        let runtime_ms = 100_000;
        let ts = spread_timestamps(runtime_ms, 1);
        assert_eq!(ts, vec![50_000]);
    }

    #[test]
    fn spread_timestamps_never_seeks_past_eof() {
        for &runtime_ms in &[1_i64, 10, 100, 999] {
            let ts = spread_timestamps(runtime_ms, 5);
            for t in ts {
                assert!(t < runtime_ms, "timestamp {t} must be < runtime {runtime_ms}");
                assert!(t >= 0);
            }
        }
    }

    #[test]
    fn spread_timestamps_clamps_unknown_or_zero_runtime() {
        assert_eq!(spread_timestamps(0, 5), Vec::<i64>::new());
        assert_eq!(spread_timestamps(-1, 5), Vec::<i64>::new());
    }

    #[test]
    fn spread_timestamps_zero_stills_requested_is_empty() {
        assert_eq!(spread_timestamps(90 * 60_000, 0), Vec::<i64>::new());
    }

    #[tokio::test]
    async fn extract_sample_stills_missing_binary_is_graceful_not_implemented() {
        let result = extract_sample_stills(
            "/definitely/not/a/real/ffmpeg/binary",
            "/media/Movies/Foo/Foo.mkv",
            90 * 60_000,
            5,
        )
        .await;

        assert!(matches!(result, Err(MuseError::NotImplemented)));
    }

    #[tokio::test]
    async fn extract_sample_stills_zero_stills_requested_returns_empty_without_spawning() {
        // n == 0 should short-circuit on the pure spread math before ever
        // touching a process, so this must succeed even with a bogus
        // ffmpeg_path.
        let result = extract_sample_stills(
            "/definitely/not/a/real/ffmpeg/binary",
            "/media/Movies/Foo/Foo.mkv",
            90 * 60_000,
            0,
        )
        .await;

        assert_eq!(result.unwrap(), Vec::<Still>::new());
    }

    #[tokio::test]
    async fn extract_sample_stills_unknown_runtime_returns_empty_without_spawning() {
        let result = extract_sample_stills(
            "/definitely/not/a/real/ffmpeg/binary",
            "/media/Movies/Foo/Foo.mkv",
            0,
            5,
        )
        .await;

        assert_eq!(result.unwrap(), Vec::<Still>::new());
    }
}
