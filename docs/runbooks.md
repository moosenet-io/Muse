# Muse — Operational Runbooks

Operational procedures grounded in the shipped code. For the data model see [`schema.md`](schema.md);
for behavioral contracts see [`behavior-spec.md`](behavior-spec.md); for env vars see the
[README](../README.md). All example IPs are RFC 5737 documentation addresses (`192.0.2.x`) and
hostnames are placeholders.

## 1. Replacing Tautulli with the native tracker

Muse captures watch telemetry natively so Tautulli can be retired. Three live artifacts (all in
`src/tracker/`), all writing the raw `play_events` stream and reconstructing `play_sessions`:

**(A) Webhook receiver — `POST /ingest/plex-webhook`.** Register a Plex webhook pointing at
`<MUSE_PUBLIC_URL>/ingest/plex-webhook` (Settings → Webhooks in Plex; requires Plex Pass). Plex posts
`multipart/form-data` with the JSON payload in a field named `payload` (any `thumb` field is ignored).
The handler **always returns `200`**, even on malformed multipart / non-JSON / a downstream failure —
so Plex never disables the webhook. It extracts `event`, `Account.id`, `Metadata.ratingKey`,
`Player.uuid`, `Metadata.viewOffset`, and player context; synthesizes a `session_key` when Plex
omits `Metadata.sessionKey`; writes a `play_events` row (`source=plex_webhook`); and folds the
session. `media.rate` is special-cased into a `ratings` upsert rather than session reconstruction.

**(B) Session poller — always spawned.** Every `MUSE_PLEX_POLL_SECS` (default 10s) it GETs Plex
`/status/sessions`. It no-ops (one log line) if Plex is unconfigured. For each active session it maps
the player state to a `media.pause`/`media.play` event (`source=plex_poll`), advances watched time,
captures `play_session_media_info` (transcode decision, codecs, resolution — absence of a
TranscodeSession block is treated as direct-play), and folds the session. The poller is the primary
path when Plex Pass / webhooks aren't available, and fills gaps the webhook misses. A tick failure is
logged and retried next interval — it never dies.

**(C) Reconstruction.** A pure fold stitches all `play_events` for a `session_key` into a finalized
`play_sessions` row (idempotent, late-event tolerant — see [`behavior-spec.md`](behavior-spec.md#2-native-tracker-session-reconstruction)
for the exact thresholds and formulas). Un-resolvable account/media leaves the raw events in place
for a later retry rather than erroring.

**Retiring Tautulli:** once the one-time backfill (below) has run and (A)/(B)/(C) are green for a soak
period, Tautulli is redundant. Muse never depends on Tautulli staying up — it only mines its history
once. Leave it running (harmless) or decommission it.

### One-time Tautulli history backfill (`tautulli::backfill`) — SEAM

`backfill::run(pool, client, options)` pages Tautulli's `get_history` (250/page), maps each row onto
`play_sessions` with `source=tautulli_backfill` and `tautulli_ref_id=reference_id`, enriching
`duration_ms` from `get_metadata` and `play_session_media_info` from `get_stream_data`. It de-dups
twice: (1) skip rows whose `reference_id` is already imported; (2) if a natively-captured session
exists within ±120s of the same `(account, media, started_at)`, **native wins** — no insert, instead
stamp `tautulli_ref_id` onto the native row for provenance.

**There is no route, CLI subcommand, or worker that calls `backfill::run`** — it is intentionally left
to an orchestrator/ops step (the crate has no `[[bin]]` beyond the axum service, and no progress
cursor is wired). To run a backfill today you must invoke it from an ops harness/test against a
configured `TautulliClient`. Configure `TAUTULLI_URL`/`TAUTULLI_API_KEY` first.

## 2. Library ingest (`arr::ingest`) — SEAM

Configure the *arr fleet via `MUSE_ARR_INSTANCES` (JSON array — see the README for the shape;
`src/arr/config.rs`). `Config::arr_instances()` parses it lazily; a malformed value degrades to zero
instances (logged, never fatal) and is held on `AppState.arr_instances`.

`arr::ingest::run(pool, instances)` iterates every instance (Radarr → `/api/v3/movie`; Sonarr →
`/api/v3/series` + `/episode` + `/episodefile`), ensures a `libraries` row per instance, and
upserts `media_metadata` (by `tmdb_id` for movies, `tvdb_id` for shows), `media_items`, `seasons`,
`episodes`, `media_files`, `quality_definitions`. It is idempotent (re-run = no dupes; files dedup on
`(media_item_id, relative_path)`) and fault-isolated (a bad instance or item is logged and skipped;
`run` never returns `Err`). Season-pack files are fetched before episodes so shared `episodeFileId`s
resolve to one `media_files` row.

**Not wired:** no `/ingest/arr` route (that path returns `501`) and no worker calls `run`. Drive it
from an ops harness until a scheduled ingest worker lands.

## 3. Prowlarr report-pull + tracker etiquette

Configure `PROWLARR_URL`/`PROWLARR_API_KEY`. When **both** are set, `spawn_workers` starts the
report-pull worker (`src/prowlarr/worker.rs`); otherwise it is never spawned (no idle task).

**Cadence & etiquette (a first-class safety constraint — protects your tracker account standing):**
- The worker wakes every `MUSE_PROWLARR_TICK_INTERVAL_SECS` (default 60s) — this is just the *check*
  cadence and should be well under the smallest per-indexer interval.
- Each indexer is pulled only once its own `indexers.polite_min_interval_secs` (default 900s) has
  elapsed since `last_rss_pull_at`. This is **DB-backed** (survives restart) via `scheduler::is_due`.
- Defense-in-depth: an in-process `RateLimiter` also gates each RSS pull on the same min-interval and
  caps targeted searches per hour, so a bug in the DB scheduler can't cause hammering.
- Read-only, RSS-first: the worker calls Prowlarr `/api/v1/search` with **no query** (latest/RSS
  mode) per enabled indexer × relevant category (`MUSE_PROWLARR_MOVIE_CATEGORIES` 2000s /
  `MUSE_PROWLARR_TV_CATEGORIES` 5000s, narrowed to what each indexer supports). A search is never a
  grab. Targeted search (ID-preferred, hourly-capped) is a client capability, used sparingly.

**Per tick:** each pulled release is run through the deterministic name parser (`src/prowlarr/parse.rs`),
upserted into `releases` (idempotent on `(indexer_id, guid)`, `expires_at = now + MUSE_RELEASE_EXPIRY_DAYS`),
and — when `parse_confidence >= MUSE_PROWLARR_RESOLVE_MIN_CONFIDENCE` and the parsed title+year matches
a `media_metadata` row — resolved to that title. Unmatched/low-confidence releases are still stored
(negative-space discovery, never dropped). Every resolved title's `availability` rollup is recomputed
after the pull, and expired rows are pruned at tick end (a non-zero prune count is logged). A Prowlarr
outage or one malformed release only skips that indexer/release for the tick; the next tick retries.

## 4. Trending / population feed + TMDb rate limits (`trending`) — SEAM

Configure `TMDB_API_KEY`. `trending::snapshot_trending(pool, client, region)` snapshots TMDb
`/trending/{movie,tv}/{day,week}` + `/popular` into `trending_snapshots`, writes
`streaming_availability` from `/watch/providers` for the top ~20 resolved entries per slice, and
computes a `population_profile` row. Optional richer sources (Trakt/FlixPatrol/JustWatch) are typed
stubs that return `NotImplemented`. Never errors on a TMDb failure (only on a local DB error) —
degrade to TMDb-only / partial.

**Not wired:** `main.rs` documents this as a "follow-on wiring item" — no route or worker calls
`snapshot_trending`. When you do wire it, keep the cadence coarse (daily) to respect TMDb rate limits;
`/popular` has no day/week axis so it is recorded under `window="week"`. Region defaults to `US`.

## 5. Adding Muse as a Plex custom tuner (linear "Muse TV" channels)

Muse presents its `mode='linear'` channels to Plex as an **additional Live-TV tuner**, alongside your
existing HDHomeRun. The tuner routes are live and mounted at the router root (HDHomeRun/M3U/XMLTV
clients expect these exact top-level paths):

- `GET /discover.json` — HDHomeRun device descriptor (`FriendlyName: "Muse TV"`, `DeviceID` =
  `MUSE_HDHR_DEVICE_ID`, `TunerCount: 4`, `LineupURL` = `<base>/lineup.json`).
- `GET /lineup.json` — one entry per linear channel (`GuideNumber`, `GuideName`, `URL` =
  `<base>/auto/v{id}`). GuideNumber uses `channel_number` (integers print without a decimal) or falls
  back to `9{id%1000:03}`.
- `GET /lineup_status.json` — static scan status.
- `GET /muse.m3u` — M3U alternative (Content-Type `audio/x-mpegurl`); each channel `#EXTINF` +
  `<base>/auto/v{id}`.
- `GET /xmltv.xml` — XMLTV EPG (Content-Type `application/xml`) generated from `channel_programs` over
  the `MUSE_CHANNEL_GUIDE_WINDOW_HOURS` window; `<channel id="muse-{id}">` + `<programme>` blocks with
  title/sub-title/desc/icon and `<episode-num>` when the subtitle parses as `SxxEyy`.

`<base>` is `MUSE_PUBLIC_URL` (a stable LAN URL Plex can reach), falling back to `http://{bind_addr}`
(only correct if `bind_addr` is a real LAN address, not `0.0.0.0`).

**Human action (operator):** In Plex → Settings → Live TV & DVR → **Set Up Plex DVR**, add Muse as a
custom device. Either enter the HDHomeRun URL (`<base>/discover.json` — Plex may auto-detect it) or
choose "have an M3U tuner" and supply `<base>/muse.m3u` with the XMLTV guide URL `<base>/xmltv.xml`.
Muse then appears in the same guide as your real HDHomeRun. Prerequisite: your library and taste data
must exist, and at least one `channels` row with `mode='linear'` must exist for a channel to appear.

**Grid upkeep:** the always-spawned `tuner::scheduler` worker wakes every
`MUSE_CHANNEL_SCHEDULER_TICK_SECS` (default 900s) and, for each linear channel, tops off
`channel_programs` from the last scheduled `end_at` forward to `now + MUSE_CHANNEL_GUIDE_WINDOW_HOURS`
(default 48h). It is a deterministic round-robin filler (episodes/movies + interstitial cadence per
`channels.rules`), contiguous and idempotent, and never backfills the past. A per-channel failure is
logged and skipped.

## 6. The channels / tuner / stream / web flow

- **On-demand director (`channels::compose`) — SEAM.** Given a directive + taste + library + watch
  state, it builds a round-robin lineup (next-unwatched or taste-ranked episodes, interstitials at a
  configured cadence) into `channel_runs` + `channel_programs`, optionally calling Chord to reorder
  shows (falls back to deterministic order on any Chord failure). **No HTTP route mounts this yet** —
  it is library-only. Six presets exist (`Saturday Morning`, `Prestige Night`, `90s Chaos`,
  `Comfort Rewatch`, `Discover`, `Household`) in `channels::presets`.
- **Streaming (`GET /auto/v{channel_id}`) — live.** Resolves "what's on now" from `channel_programs`,
  spawns ffmpeg per program (stream-copy `-c copy`, input-seek for join-mid-stream), and chains each
  program's ffmpeg stdout into one MPEG-TS response. Best-effort tops off the grid first. Returns
  **`501`** if the ffmpeg binary is missing entirely, **`503`** if nothing is scheduled "now" or the
  on-now file can't be resolved. ffmpeg path = `MUSE_FFMPEG_PATH`; file paths = `MUSE_MEDIA_ROOT` +
  stored relative path (empty root ⇒ paths used as-is). A later program failing mid-stream is skipped,
  not fatal.
- **Web guide — live.** `GET /` and `/guide` serve a self-contained EPG page (no external assets,
  dark theme, auto-refresh 60s). `GET /api/channels` and `/api/channels/{id}/lineup` back it.
  `GET /art/{kind}/{id}` proxies artwork from `artwork_cache` (Postgres `bytea`), fetching from Plex
  server-side on a cache miss and serving a 1×1 placeholder rather than 404. **The Plex token is used
  strictly server-side and never appears in an `/art/...` response body or header** (asserted by test).
- **Plex cast control (`plex_control`) — DEAD CODE.** The Companion play-queue/cast client is
  implemented and unit-tested but **mounted nowhere and called by nothing**, and never exercised
  against a real Plex server. Do not rely on it; treat its header/query behavior as unverified.

## 7. Taste-model recompute + embedding pipeline

Both are implemented, tested, and **have no scheduled caller** — until wired, `taste_profile`,
`taste_context_centroids`, and `embeddings` are never populated automatically, so `/recommend`'s taste
tier and the `friday_evening` proactive generator return empty even on a full library.

- **Embedding pipeline (`embed::embed_stale`).** Configure `MUSE_OLLAMA_URL` (serving
  `nomic-embed-text`). `embed_stale(pool, client, batch)` scans `media_items`, composes a
  deterministic `source_text` per title (title/year/type/sorted-genres/studio/network/tagline/overview),
  skips titles whose stored `source_text` is unchanged (the change-detection key — no hash column),
  embeds the rest in VRAM-polite sub-batches of 8 with a 300ms pause (<host>'s GPU is shared with the
  resident production serve), and upserts `embeddings` (keyed to `media_item.id`). The read primitive
  `embed::nearest` (used by recall + curation) **is** live.
- **Taste recompute (`taste_model::recompute_taste`).** Configure `CHORD_URL` for the optional prose
  `model_notes` summary (interim model `qwen3-coder:30b` via Chord; any Chord failure degrades to
  `None`, never fails the recompute). Per account it re-derives `taste_signals` from
  `watch_stats`/`ratings`/`watchlist`, aggregates recency-weighted affinities + the embedding centroid,
  and upserts `taste_profile` + `taste_context_centroids`. Idempotent and strictly per-account. Needs
  `embeddings` present for the centroid; `overall_centroid` is `None` until titles are embedded.
- **Radar (`radar::recompute_divergence`).** Computes the per-account `taste_divergence` snapshot from
  the account's genre/decade shares vs. the `population_profile`. Also unwired; the `zeitgeist`
  proactive generator reads its output, so `zeitgeist` is dormant until this is driven.

A future scheduled worker (or an admin route) should call these in order: `embed_stale` → per-account
`recompute_taste` → (with trending ingest) `recompute_divergence`.

## 8. The proactive → Lumina contract

The proactive generator worker is **always spawned** and runs every
`MUSE_PROACTIVE_TICK_INTERVAL_SECS` (default 3600s / hourly), running five generators per account and
writing cooldown/dedup-filtered `proactive_items`. Lumina's scheduler polls the read surface on its
own cadence (independent of Muse's tick):

**`GET /proactive/pending?account_id=<N>&limit=<n>`** (default limit 20, max 100) →
```json
{"items": [ { "id": 123, "account_id": 1, "kind": "abandon_insight",
              "media_item_id": 42, "headline": "You put this on pause: \"…\" — ….",
              "body": { /* structured rationale + facts */ }, "priority": 6,
              "earliest_at": null, "expires_at": "…", "delivered_at": null,
              "created_at": "…", "dedup_key": "…", "status": "pending",
              "dismissed_at": null } ] }
```
Items appear only when `earliest_at` has passed (may be `null`/immediate, or e.g. next-Friday-20:00
UTC for `friday_evening`), `expires_at` hasn't passed, and `delivered_at IS NULL`. Strictly scoped to
`account_id` — never blends accounts.

**`POST /proactive/{id}/ack`** with body `{"outcome": "sent"}` or `{"outcome": "dismissed"}`
(any other value → `400`, not a silent no-op) → `{"item": <updated ProactiveItem>}`.
- `sent` sets `status="sent"` and `delivered_at=now` (drops it from the pending query).
- `dismissed` sets `status="dismissed"` and `dismissed_at=now`.

There is **no separate "expired" outcome** — an unacked item simply stops being returned once
`expires_at` passes. See [`behavior-spec.md`](behavior-spec.md#6-proactive-content-generation) for
each generator's trigger and cooldown. Note `zeitgeist` is dormant until the radar is wired (§7).
