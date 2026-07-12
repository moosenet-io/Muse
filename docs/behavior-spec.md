# Muse — Behavior Specification

The behavioral contract of the shipped system: exact thresholds, formulas, weights, and
degradation rules, cited to source. This is the "what does it actually do, and why" companion to
the reference material in [`schema.md`](schema.md) and the procedures in [`runbooks.md`](runbooks.md).
Constants below are verbatim from `src/`.

## 0. Cross-cutting invariants

- **Read-only / benign.** Phase 0 is strictly read-only against Plex/Tautulli/*arr/Prowlarr.
  Phase 0.5 (channels) only starts playback — it never mutates, downloads, deletes, or organizes.
  No acquisition/organize/delete surface exists.
- **Graceful degradation, never 500 on a down dependency.** Every upstream client is optional
  (`from_config` → `None` when unconfigured). A missing or failing Plex / Prowlarr / TMDb / Ollama /
  Chord / SearXNG / news endpoint degrades that one feature (a skipped tier, an empty result, a
  templated fallback) — it never blocks startup and never becomes a `500`. `/health` never 500s even
  with the DB down.
- **Multi-user isolation.** Taste is per-account and never blended. Every taste/recommend/proactive
  read and write is scoped to a single `account_id`.
- **Grounded rationale.** Every recommendation and proactive nudge is built from a `facts` list of
  real, computed signals. When an LLM (via Chord) rephrases them, its prompt explicitly forbids
  inventing anything beyond those facts, and any LLM failure falls back to a deterministic template —
  the feature never fails because the model is down or the GPU is busy.

## 1. Library ingest behavior (`arr::ingest`)

Idempotent upsert with provider-ID reconciliation: movies keyed on `(kind, tmdb_id)`, shows on
`(kind, tvdb_id)`; `media_files` deduped on `(media_item_id, relative_path)`. Fault-isolated: a bad
instance or item is logged and skipped; `run` never returns `Err`. Release type mapped
`seasonpack→SeasonPack`, `multiepisode→Multi`, else `Single`. **Seam** (no scheduled caller).

## 2. Native tracker: session reconstruction

Thresholds (`src/tracker/reconstruct.rs`): `COMPLETE_THRESHOLD = 0.90`, `ABANDON_THRESHOLD = 0.15`.

`fold_events` sorts a session's `play_events` by `(received_at, id)` (deterministic → idempotent,
late-event tolerant) and folds:
- `watched_ms` accrues per interval where playback was active: `cur_offset - prev_offset` when the
  offset advanced, else wall-clock elapsed on a backward jump/rewind (never negative). This runs
  every event, so polled-only sessions accrue time between ticks.
- `media.play`/`media.resume` → playing; `media.pause` → not playing, `paused_counter += 1`,
  accumulate `paused_ms`; `media.stop` → set `stopped_at`; `media.scrobble` → `is_finished = true`.
- `duration_ms` = max observed runtime; `view_offset_ms` = last offset;
  `percent_complete = (view_offset_ms or watched_ms) / duration_ms`, clamped `[0,1]`, only when
  `duration_ms > 0`.
- `is_finished` also becomes true when `percent_complete >= 0.90`.
- `is_abandoned = percent_complete < 0.15`, **only** when `stopped_at` is set and not finished.
- Context: `started_hour`, `started_dow`, and `is_cinema_context` (device-string match against
  `tv`/`living room`/`roku`/`chromecast`/`appletv`/`shield`/`firetv`).

Persistence is idempotent via the `NULLS NOT DISTINCT` natural key; unresolvable account/media leaves
raw events for a later retry (returns `Ok(None)`, not an error). **Live** (webhook + poller both call it).

**Tautulli backfill mapping** (`src/tautulli/backfill.rs`, same thresholds): `is_finished = watched_status >= 1.0 || percent_complete >= 0.90`;
`is_abandoned = !is_finished && percent_complete < 0.15`; `percent_complete = row.percent_complete/100`;
`paused_ms` hardcoded 0 (Tautulli exposes only a count). Native capture wins over backfill within a
±120s overlap. **Seam.**

## 3. Vector recall + search (`recall`, live)

### `POST /query/resolve`
Request `{query, limit?, include_tmdb=false}`. A resolution ladder runs in order, stopping at the
first tier with results (`run_ladder`):
1. **vector** — embeds `query` via Ollama (`MUSE_OLLAMA_URL`), cosine-ANN over `embeddings`. A match
   counts only if `distance <= MUSE_RECALL_VECTOR_MAX_DISTANCE` (default `0.4`); results are
   ascending-distance so the tier breaks at the first over-threshold hit. Unconfigured Ollama /
   embed failure / no confident match → empty → fall through.
2. **trigram** — `pg_trgm` fuzzy title search over `media_metadata`.
3. **tmdb** — only when `include_tmdb=true`; TMDb `/search/multi` beyond the library, every hit tagged
   `note: "not in your library — found on TMDb"`.

Response `{tier: "vector"|"trigram"|"tmdb"|"none", results:[...]}` with each hit tagged by `source`.
Empty/whitespace query or a total miss → `tier:"none"`, empty results — never an error. `limit`
clamps to `[1,50]`, default 10.

### `POST /query/similar`
Request `{media_item_id, limit?}`. A **non-existent `media_item_id` is a genuine 404/error** (unlike
the free-text ladder). Prefers the seed's own stored embedding (cosine-ANN, over-fetch by one to drop
the seed itself → `tier:"vector"`); falls back to shared-genre similarity (`tier:"genre"`,
`media_item_id` may be `null` for metadata-only hits) when the seed has no embedding or no neighbor
resolves; `tier:"none"` when neither finds anything.

## 4. Embedding pipeline behavior (`embed`, write-path is a SEAM)

Model `nomic-embed-text`, `vector(768)`, HNSW cosine. `compose_source_text` is byte-deterministic
(stable field order, sorted+deduped genres, empty fields omitted) — it doubles as the
change-detection key. `embed_stale` pages `media_items` (200/page, ≤25 pages/call), skips titles whose
stored `source_text` is unchanged, embeds the rest in sub-batches of 8 with a 300ms pause
(VRAM-politeness on the shared <host> GPU), and upserts keyed to `media_item.id`. Per-row failure is
logged, not fatal. Ollama unconfigured/failing degrades cleanly. **No scheduled caller** — embeddings
are not written in a running deployment until wired.

## 5. Taste model (`taste_model` + `radar`, both SEAMs)

### Signal weights (`src/taste_model/signals.rs`)
```
WEIGHT_FINISH               = 1.0
WEIGHT_REWATCH_PER          = 2.5   (× rewatch_count, additive on top of the finish signal)
WEIGHT_ABANDON              = -1.5
RATING_MIDPOINT             = 5.0   (Plex 0-10 scale; 5/10 is neutral)
RATING_WEIGHT_SCALE         = 2.0   (10/10 → +2.0, 0/10 → -2.0)
WEIGHT_WATCHLIST_ADD        = 0.3
WEIGHT_WATCHLIST_FULFILLED  = +0.3  (bonus on top of the add weight)
DEFAULT_HALF_LIFE_DAYS      = 180.0 (~6 months)
```
`rating_weight = ((rating - 5)/5 * 2).clamp(-2, 2)`. A finished + rewatched + rated title emits three
separate auditable `taste_signals` rows (never one blended row); a human `curation_note` is never
touched by a recompute.

**Recency decay:** `recency_weight = 0.5 ^ (days_since / half_life_days)`, with `days_since` clamped
`>= 0` (future timestamps → full weight `1.0`, never > 1). Half-life ≤ 0 → always `1.0`.

### Profile aggregation (`src/taste_model/profile.rs`)
Every affinity uses `total[key] += raw_weight * recency_weight(observed_at, now, half_life)`:
- `genre_affinity` / `person_affinity` — recency-weighted sums, flat JSON maps.
- `keyword_affinity` — stores `{"keywords":{...},"decades":{...}}` (no dedicated decade column;
  documented divergence).
- `runtime_pref` — coarse buckets over *finished* titles: `short ≤ 40 min`, `medium ≤ 90 min`, else
  `long`; `None` if no finished titles with known runtime.
- `overall_centroid` — recency-**and**-rewatch-weighted mean of finished titles' embeddings; weight =
  `recency_weight * (1 + rewatch_count * 0.5)`; titles without an embedding are skipped; `None` if
  nothing to average.
- `quality_sensitivity` — **deferred, always `None`** in v0.
- `taste_context_centroids` — finished sessions bucketed by `context_key_for(hour, dow)` into
  `{weekend|weekday}_{morning(5-11)|daytime(12-16)|evening(17-21)|late_night(22-4)}`, averaged
  **unweighted** (context, not recency). Empty buckets skipped.

`model_notes` (optional) — Chord `qwen3-coder:30b`, `temperature 0.4`, `max_tokens 220`; prompt asks
for a warm 2-3 sentence prose summary from the genre affinities, "describe the pattern, don't repeat
raw numbers." Short-circuits to `None` if Chord unconfigured or the affinity map is empty; any failure
degrades to `None` — never fails the recompute. **Seam** (no scheduled caller).

### You-vs-masses radar (`src/radar/divergence.rs`)
`EPSILON = 0.01`, list caps `DIVERGENCE_LIST_LIMIT = 10`. All formulas are deliberately legible:
- shares: `normalize` → non-negative weights summing to 1.
- `genre_index`/`decade_index`: `(account_share + ε) / (population_share + ε)` per key (>1 over-index).
- `overlap`: histogram intersection `Σ min(account[k], population[k])` (0 if either side empty).
- `mainstream_score`: `0.7*genre_overlap + 0.3*decade_overlap` (clamped 0..1), or genre-only when no
  decade data. (Distribution-overlap based, *not* the spec's "cosine of centroids" — no embeddings
  feed the radar.)
- `adventurousness = 1 - mainstream_score`.
- `contrarian_index = (1 - pearson_r)/2` over genre shares; `r=1→0`, `r=-1→1`, `r=0`/undefined `→0.5`.
- `were_early`: account titles watched before the population sample's `trended_at`, ranked by
  `lead_days` desc.
- `blind_spots`: unwatched population titles ranked by best rank (then popularity).
- `guilty_pleasures`: account titles with `rewatch_count > 0` absent from the trending sample, ranked
  by rewatch count.

`taste_divergence` is append-only (tracked over time). **Seam** (no scheduled caller / no HTTP surface).

## 6. Curation / recommend (`curation`, live)

Four candidate sources, each grounded in real facts:
- **on-deck** (`taste_fit = avg_percent/100`), fact "you're N% through it".
- **gap** (`taste_fit = avg_percent or 60/100`), fact grounded in `next_airing`/`status`.
- **taste** — needs `taste_profile.overall_centroid` (empty on cold start); cosine-ANN excluding
  finished titles; `taste_fit = (1 - distance/2).clamp(0,1)`; facts "N% match to your overall taste
  profile" + "you rate {top_genre} highly".
- **available-now** — trending, not-in-library, joined against `availability`; `taste_fit =
  popularity/100`; grabbability facts (seeders/freeleech or "not currently available").

**Dedup** collapses same `media_metadata_id` with source priority `on-deck < gap < taste < available-now`
(keeping all merged facts). **Scoring** (`score_candidate`):
```
source_weight: OnDeck 1.0, Gap 0.85, Taste 0.7, AvailableNow 0.6
score = source_weight * taste_fit
  + 0.15 (grabbable available-now: release_count > 0)
  - 0.10 (checked-but-unavailable available-now)
score clamped >= 0
```
Ranked descending (stable tie-break). **Rationale**: deterministic template from `facts` always;
Chord rephrases with a strict "ground ONLY in these facts, never invent" prompt when `CHORD_URL` is
set, falling back to the template on any failure.

- `POST /recommend {account_id, context?, limit?, include_unavailable=false}` — full ranked list.
  `context` is accepted but **not yet used in ranking** (reserved). `include_unavailable=false` drops
  (not just deprioritizes) checked-unavailable picks. Default limit 10, max 50. Region hardcoded `US`.
- `GET /recommend/on_deck` / `GET /recommend/gaps` — single-source variants.

Taste-tier candidates are empty until `embeddings` + `taste_profile` are populated (see §4/§5 seams).

## 7. Proactive content generation (`proactive`, live worker; one dormant generator)

Worker runs every `MUSE_PROACTIVE_TICK_INTERVAL_SECS` (default 3600s), first tick skipped, per account.
Cooldown per kind (`cooldown_days`): `new_season 14`, `friday_evening 5`, `abandon_insight 21`,
`grab_window 3`, `zeitgeist 7`. Default `expires_at = now + 2*cooldown` when a generator doesn't set
its own. The five generators (`proactive_items.kind`):

1. **`new_season`** — an engaged, continuing show; fires only with a grounded fact from `next_airing`
   or `status`. Priority 5.
2. **`friday_evening`** — needs an `_evening` context centroid with `sample_size > 0` (max-sample
   bucket) plus a taste candidate; `earliest_at` = next Friday 20:00 **UTC** (no per-account timezone
   yet — documented divergence). Priority 5. Dormant until taste profiles/centroids exist.
3. **`abandon_insight`** — an abandoned title; fires only if a fact can be grounded: `does_it_get_good`
   enrichment (`gets_good_at_episode`, or `patience_payoff >= 0.4`) and/or another household account
   finished it. Priority 6 if a specific episode is known, else 5.
4. **`grab_window`** — a taste-relevant, not-in-library title with `availability.release_count > 0`;
   lead "Freeleech grab window open:" (priority 7) if freeleech, else "It's grabbable right now:"
   (priority 5). Account-agnostic (whole-library).
5. **`zeitgeist`** — reads the latest `taste_divergence` snapshot (skips if none, or older than 30
   days); emits up to 3 `were_early` ("You were early on", priority 5) and 3 `blind_spots`
   ("Worth the hype?", priority 4). **Functionally dormant** because nothing computes `taste_divergence`
   in a running deployment (see §5).

**Message building**: deterministic template `"{lead} \"{title}\" — {facts}."` always; Chord rephrases
with a strict grounding prompt when configured, falling back to template on failure. **Orchestrator**
(`generate_for_account`) runs all five (each fault-isolated), then drops any item whose
`(account, kind, dedup_key)` fired within its cooldown window — which also makes a same-tick re-run
idempotent — before phrasing + persisting the survivors with `create_with_dedup`.

See the [Lumina poll/ack contract in the runbook](runbooks.md#8-the-proactive--lumina-contract).

## 8. The pseudo-TV director (channels / tuner / stream, mixed wiring)

- **On-demand composer (`channels::compose`) — SEAM.** Round-robin next-unwatched (or taste-ranked)
  episodes across shows, one per round, interleaving an interstitial every
  `interstitial_every_n_items` items (avoiding the immediately-prior interstitial), until
  `target_session_ms` or all queues empty; contiguous timeline (`start_at[i+1] == end_at[i]`).
  Fallback durations: episode 22 min, interstitial 30s. Optional Chord reorder (validated to be an
  exact permutation of the input show set; any failure → deterministic order + templated rationale).
  Regeneration always inserts a fresh `channel_runs` row (never mutates history). Six presets.
- **Linear grid (`tuner::scheduler`) — live worker.** A separate deterministic round-robin filler
  (not `channels::compose`) keeps `channel_programs` topped off `MUSE_CHANNEL_GUIDE_WINDOW_HOURS`
  ahead every `MUSE_CHANNEL_SCHEDULER_TICK_SECS`; content per `channels.rules` (`content_kind`
  episode/movie/mixed, `library_ids`, `interstitial_every` default 4, `interstitial_kind`); candidate
  rows require `in_library=true AND has_file=true`; round-robin cursors wrap on exhaustion (endless).
- **Streaming (`streaming::stream_channel`) — live.** "On now" = the `channel_programs` row covering
  `now`, tie-broken toward the latest `start_at`; `seek_ms = (now - start_at)` clamped to the
  program's duration (join-mid-stream). ffmpeg `-c copy` (stream-copy, never re-encode — a 24/7
  channel can't re-encode on a shared GPU), input-seek (`-ss` before `-i`) only when `seek_ms > 0`.
  `501` if the ffmpeg binary is missing (`NotImplemented`), `503` if nothing is on now or the on-now
  file is unresolvable (`ServiceUnavailable`); a later program failing mid-stream is skipped, not fatal.
- **Web guide / artwork — live.** Self-contained EPG page; artwork proxied from Postgres (`bytea`),
  Plex-fetched server-side on a miss, 1×1 placeholder rather than 404, **Plex token never reaches the
  browser** (asserted by test).
- **Cast control (`plex_control`) — DEAD CODE.** Implemented, unit-tested, unverified against a real
  Plex server, and mounted/called nowhere.

## 9. Error → HTTP status contract (`src/error.rs`)

| `MuseError` variant | Status | Used for |
|---|---|---|
| `Database`, `Config`, `Internal` | 500 | server-side failures/misconfiguration |
| `BadRequest` | 400 | well-formed request, invalid content (e.g. bad ack `outcome`) |
| `NotFound` | 404 | e.g. `/query/similar` with a non-existent `media_item_id` |
| `Conflict` | 409 | rate-limiter conflicts |
| `NotImplemented` | 501 | un-mounted `/ingest`,`/query`,`/proactive` sub-paths; ffmpeg binary missing |
| `Http`, `Upstream` | 502 | transport/non-success talking to an upstream (Plex/TMDb/Prowlarr/Chord/…) |
| `ServiceUnavailable` | 503 | channel has no program "now"; transient ffmpeg spawn error |

Every error body is `{"error": message}`. `/health` is exempt — it always returns `200` with
`db:"up"|"down"`.
