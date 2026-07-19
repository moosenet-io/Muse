# Add Muse as a Plex tuner

Muse exposes its `mode='linear'` channels to Plex Live TV as a custom HDHomeRun-emulation
tuner (MUSE-28), with ffmpeg-backed continuous streams (MUSE-29). Expected outcome: Plex
lists Muse channels in Live TV with EPG data, and tuning in joins the stream mid-program
at "what's on now."

## Prerequisites

- A running `muse` instance with `MUSE_DATABASE_URL` configured and at least one
  `mode='linear'` channel with library content scheduled (the `tuner::scheduler` worker
  keeps the grid filled a rolling `MUSE_CHANNEL_GUIDE_WINDOW_HOURS` ahead — default 48h).
- `ffmpeg` reachable at `MUSE_FFMPEG_PATH` (default: `ffmpeg` on `$PATH`).
- `MUSE_MEDIA_ROOT` set if your stored file paths are relative to a library root.

## Steps

1. **Set the advertised base URL.** Plex must be able to reach Muse at the URL Muse
   advertises. Set `MUSE_PUBLIC_URL` to a LAN-reachable base URL for the Muse host. If
   unset, Muse degrades to `http://{MUSE_BIND_ADDR}` — which is only correct when the
   bind address is itself LAN-reachable (i.e. **not** the default `0.0.0.0`).
2. **Verify the tuner endpoints** respond:
   - `GET /discover.json` — device info (`MUSE_HDHR_DEVICE_ID`, default `MUSE0001`)
   - `GET /lineup.json` — one entry per linear channel
   - `GET /lineup_status.json`
3. **Add the tuner in Plex**: Settings → Live TV & DVR → Set Up Plex DVR. If Plex
   doesn't auto-discover, enter the Muse address manually (the value you put in
   `MUSE_PUBLIC_URL`).
4. **Provide the guide**: point Plex's XMLTV guide option at `GET /xmltv.xml`. The EPG
   is rendered directly from the scheduled `channel_programs` grid.
5. **Tune a channel.** Plex requests the stream URL from the lineup, which points at
   `GET /auto/v{channel_id}` — a continuous MPEG-TS stream that concatenates the
   scheduled programs and joins mid-stream at the current position.

**Alternative (non-Plex players):** any IPTV player can use `GET /muse.m3u` (playlist)
plus `GET /xmltv.xml` (EPG) instead of the HDHR endpoints.

## Troubleshooting

- **Stream answers 501**: the ffmpeg binary couldn't be spawned — check
  `MUSE_FFMPEG_PATH` (the handler deliberately returns a clean 501, not a 500, for a
  missing binary).
- **Empty lineup**: no `mode='linear'` channels exist yet, or the scheduler hasn't
  ticked (`MUSE_CHANNEL_SCHEDULER_TICK_SECS`, default 900s).
- **Plex can reach discovery but not the stream**: `MUSE_PUBLIC_URL` is missing or
  points at a wildcard/unreachable address — every advertised URL is built from it.
