# tuner

The linear tuner (66 KG nodes, MUSE-28, spec §4d-E) — exposes `mode='linear'` channels
to Plex Live TV as a custom **HDHomeRun-emulation** tuner (`/discover.json`,
`/lineup.json`, `/lineup_status.json`) and, as an M3U+XMLTV alternative, `/muse.m3u` +
`/xmltv.xml`. A rolling-window scheduler keeps each linear channel's `channel_programs`
grid topped up 24–48h ahead; `xmltv` renders EPG programme data straight from that grid.

The actual continuous video stream (`/auto/v{id}`, ffmpeg concat with join-mid-stream
semantics) is `crate::streaming` (MUSE-29) — every URL advertised here points at it.

## Key types and functions

| Symbol | Kind | File | What it does |
|---|---|---|---|
| `tuner::base_url` | fn | `src/tuner/mod.rs` | The base URL players use to reach this instance: honors `MUSE_PUBLIC_URL`, degrades to `http://{bind_addr}` (only correct when the bind isn't a wildcard address) |
| `tuner::hdhr::lineup_entries` | fn | `src/tuner/hdhr.rs` | Builds the lineup (one entry per linear channel) for `/lineup.json` |
| `tuner::hdhr::channel_ref` | fn | `src/tuner/hdhr.rs` | Stable, prefixed channel identifier |
| `tuner::m3u::render` | fn | `src/tuner/m3u.rs` | Renders the lineup as an M3U playlist |
| `tuner::xmltv::render` | fn | `src/tuner/xmltv.rs` | Renders channels + programs as XMLTV EPG data (escaping + episode-num handled and tested) |
| `tuner::scheduler::RoundRobin::next` / `is_empty` / `new` | fn | `src/tuner/scheduler.rs` | The deterministic round-robin the grid-filler cycles shows with |
| `tuner::scheduler::spawn` | fn | `src/tuner/scheduler.rs` | The background worker keeping every linear channel's grid topped off a rolling window ahead |

## How it connects

Routes are mounted by `http::router`; the scheduler worker is spawned unconditionally by
`workers::spawn_workers` (a deployment with zero linear channels just ticks a no-op).
The scheduler reads channels/episodes and writes `channel_programs` through `repo`;
`streaming` reads the same grid to answer `/auto/v{id}`; `web`'s guide page renders it
for browsers. Plex is the primary consumer: added as a tuner device, it discovers via
the HDHR endpoints and reads the EPG via XMLTV.

## Configuration

- `MUSE_PUBLIC_URL` — LAN-reachable base URL advertised in `/discover.json` and stream
  URLs.
- `MUSE_HDHR_DEVICE_ID` — stable device id Plex uses to recognize the tuner (default
  `MUSE0001`).
- `MUSE_CHANNEL_GUIDE_WINDOW_HOURS` — rolling guide window (default 48).
- `MUSE_CHANNEL_SCHEDULER_TICK_SECS` — scheduler wake cadence (default 900).

## Notes and gaps

- The scheduler is intentionally a separate, simpler composer from
  `channels::compose`/`director` — linear grids need predictable top-offs, not one-shot
  curated sessions; both write the same `channel_programs` table.
- Setup steps live in the [Plex tuner guide](../guides/plex-tuner.md).
- Not covered here: the ffmpeg process management and join-mid-stream math — see
  `src/streaming/` (`onnow` is the pure "what's on now + seek offset" layer).
