//! ffmpeg command construction + path/availability helpers (MUSE-29). Pure
//! functions only — nothing here spawns a process; [`crate::streaming`]'s
//! HTTP handler is the one impure caller that actually invokes ffmpeg.
//!
//! **Codec copy vs transcode.** This builder always emits `-c copy` (stream
//! copy / remux, no re-encode). Re-encoding a rolling 24/7 linear channel
//! would burn CPU continuously on a host (<host>) that's already a shared
//! GPU/inference box — not affordable for a "benign playback" feature. The
//! tradeoff: `-c copy` only produces a clean transport stream when
//! consecutive inputs share compatible codecs/parameters (true for a
//! Plex-managed library, which is the only source this reads from). Rather
//! than force a decode+encode pass to paper over a hypothetical mismatch,
//! MUSE-29 keeps each scheduled program in its *own* ffmpeg invocation
//! (see `streaming::stream_channel`) — the join between programs happens by
//! chaining separate processes' stdout into one HTTP response, not by
//! asking a single ffmpeg process to concat mismatched inputs. That sidesteps
//! the concat-filter's decode requirement entirely; a real transcode tier is
//! a documented follow-up, not required for this item.

use std::io;

/// Build the ffmpeg CLI arguments (everything after the binary name) to
/// stream `file_path` as MPEG-TS on stdout, seeking `seek_ms` milliseconds
/// into it first when `seek_ms > 0` (the join-mid-stream offset for
/// whichever program is airing "now"; every subsequent program in a
/// playlist streams from its own start, `seek_ms == 0`).
///
/// `-ss` is placed *before* `-i` (input seeking) rather than after — ffmpeg
/// input-seeks are fast (keyframe-nearest demuxer seek, no decode of
/// skipped data) and are the correct choice for a stream-copy pipeline;
/// output seeking (`-ss` after `-i`) would force a decode pass, which
/// contradicts the copy-not-transcode design above.
pub fn build_args(file_path: &str, seek_ms: i64) -> Vec<String> {
    let mut args = vec![
        "-hide_banner".to_string(),
        "-loglevel".to_string(),
        "error".to_string(),
        "-y".to_string(),
    ];

    if seek_ms > 0 {
        args.push("-ss".to_string());
        args.push(format!("{:.3}", seek_ms as f64 / 1000.0));
    }

    args.push("-i".to_string());
    args.push(file_path.to_string());

    args.push("-c".to_string());
    args.push("copy".to_string());
    args.push("-f".to_string());
    args.push("mpegts".to_string());
    args.push("pipe:1".to_string());

    args
}

/// Join a configured media root onto a stored `relative_path`/`file_path`.
/// An empty `media_root` (the default — see [`crate::config::Config`])
/// means "use the stored value exactly as-is," which is correct both for
/// already-absolute paths and for a process whose cwd is the library root.
/// Otherwise joins with exactly one `/` regardless of whether either side
/// already carries a slash.
pub fn join_media_path(media_root: &str, relative_path: &str) -> String {
    if media_root.is_empty() {
        return relative_path.to_string();
    }
    format!(
        "{}/{}",
        media_root.trim_end_matches('/'),
        relative_path.trim_start_matches('/')
    )
}

/// Why spawning the ffmpeg process failed — distinguishes "the binary
/// doesn't exist on this host" (a hard, deployment-level gap — degrade to
/// `501 Not Implemented`) from any other spawn failure (permissions,
/// resource limits, ...) which is more honestly a transient
/// `503 Service Unavailable`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamAvailability {
    BinaryMissing,
    SpawnError,
}

/// Classify a [`std::process::Command`]/[`tokio::process::Command`] spawn
/// error. Pure/sync so it's directly unit-testable without ever actually
/// invoking ffmpeg (the gate rule this item is built under — ffmpeg may be
/// entirely absent on the test/gate host).
pub fn classify_spawn_error(err: &io::Error) -> StreamAvailability {
    if err.kind() == io::ErrorKind::NotFound {
        StreamAvailability::BinaryMissing
    } else {
        StreamAvailability::SpawnError
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_args_includes_seek_when_mid_program() {
        let args = build_args("/media/TV/Show/S01E01.mkv", 17 * 60_000);
        assert!(args.contains(&"-ss".to_string()));
        let idx = args.iter().position(|a| a == "-ss").unwrap();
        assert_eq!(args[idx + 1], "1020.000");
        // -ss must precede -i (input seek, not output seek).
        let i_idx = args.iter().position(|a| a == "-i").unwrap();
        assert!(idx < i_idx);
    }

    #[test]
    fn build_args_omits_seek_when_starting_from_zero() {
        let args = build_args("/media/Movies/Foo/Foo.mkv", 0);
        assert!(!args.contains(&"-ss".to_string()));
    }

    #[test]
    fn build_args_never_seeks_negative() {
        // Defensive: even if a caller passes a negative offset (shouldn't
        // happen post-clamp in onnow::resolve_on_now, but this function
        // doesn't know that), it must not emit "-ss" for a non-positive
        // value.
        let args = build_args("/media/Movies/Foo/Foo.mkv", -5);
        assert!(!args.contains(&"-ss".to_string()));
    }

    #[test]
    fn build_args_uses_stream_copy_and_mpegts_to_stdout() {
        let args = build_args("/media/Movies/Foo/Foo.mkv", 0);
        assert_eq!(
            args,
            vec![
                "-hide_banner",
                "-loglevel",
                "error",
                "-y",
                "-i",
                "/media/Movies/Foo/Foo.mkv",
                "-c",
                "copy",
                "-f",
                "mpegts",
                "pipe:1",
            ]
        );
    }

    #[test]
    fn build_args_input_ordering_with_seek_present() {
        let args = build_args("/media/Movies/Foo/Foo.mkv", 5_000);
        assert_eq!(
            args,
            vec![
                "-hide_banner",
                "-loglevel",
                "error",
                "-y",
                "-ss",
                "5.000",
                "-i",
                "/media/Movies/Foo/Foo.mkv",
                "-c",
                "copy",
                "-f",
                "mpegts",
                "pipe:1",
            ]
        );
    }

    #[test]
    fn join_media_path_with_empty_root_returns_as_is() {
        assert_eq!(join_media_path("", "/media/TV/Show/ep.mkv"), "/media/TV/Show/ep.mkv");
        assert_eq!(join_media_path("", "TV/Show/ep.mkv"), "TV/Show/ep.mkv");
    }

    #[test]
    fn join_media_path_normalizes_slashes() {
        assert_eq!(join_media_path("/srv/media", "TV/Show/ep.mkv"), "/srv/media/TV/Show/ep.mkv");
        assert_eq!(join_media_path("/srv/media/", "/TV/Show/ep.mkv"), "/srv/media/TV/Show/ep.mkv");
        assert_eq!(join_media_path("/srv/media", "/TV/Show/ep.mkv"), "/srv/media/TV/Show/ep.mkv");
    }

    #[test]
    fn classify_spawn_error_distinguishes_missing_binary() {
        let not_found = io::Error::new(io::ErrorKind::NotFound, "No such file or directory");
        assert_eq!(classify_spawn_error(&not_found), StreamAvailability::BinaryMissing);

        let denied = io::Error::new(io::ErrorKind::PermissionDenied, "Permission denied");
        assert_eq!(classify_spawn_error(&denied), StreamAvailability::SpawnError);

        let other = io::Error::new(io::ErrorKind::Other, "boom");
        assert_eq!(classify_spawn_error(&other), StreamAvailability::SpawnError);
    }
}
