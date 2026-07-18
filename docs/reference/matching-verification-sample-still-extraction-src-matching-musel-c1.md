## Matching-verification: sample-still extraction (`src/matching/`, MUSEL-C1)

MUSEL-C1 is the raw-material primitive for a matching-verification pipeline: rather than
trusting a provider-ID match blindly, Muse can pull a handful of still frames from a media
file and (in a later item, MUSEL-C2) judge whether the file's actual content is consistent
with what it's labeled as.

- `streaming::ffmpeg::build_still_args(file_path, seek_ms)` — pure arg builder, extending the
  existing channel-streaming `ffmpeg` module. Reuses the same fast input-seek (`-ss` *before*
  `-i`) as `build_args`, and decodes exactly one frame (`-frames:v 1 -f image2pipe -vcodec
  mjpeg pipe:1`) per invocation — one still per ffmpeg process, mirroring MUSE-29's
  one-invocation-per-unit discipline.
- `matching::stills::extract_sample_stills(ffmpeg_path, file_path, runtime_ms, n)` — spreads
  `n` sample timestamps across `runtime_ms` (roughly 10%..90%, so a still never lands on a
  black leader/credits frame or seeks past EOF), spawns ffmpeg once per timestamp, and
  captures each resulting JPEG's bytes + timestamp into a `Still { bytes, timestamp_ms }`.
  Read-only on the input; every still lives in memory only (never written to disk beside the
  media). A decode failure on one timestamp is skipped, not fatal to the rest. `ffmpeg_path`
  is threaded through explicitly (matching `Config::ffmpeg_path`, `MUSE_FFMPEG_PATH`) rather
  than assumed — no hardcoded infra. If the ffmpeg binary itself is missing, the whole call
  degrades gracefully to `MuseError::NotImplemented` (the same posture `streaming` uses for a
  missing binary), instead of panicking or repeating the same failure per timestamp.
