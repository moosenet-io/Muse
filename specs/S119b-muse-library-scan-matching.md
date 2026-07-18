# Muse Library — Metadata Providers, Read-Only Library Scan & Still-Frame Matching Verification
plane_project: MUSE
module: Muse
prefix: MUSEL
spec_id: S119b-muse-library-scan-matching

## Metadata
- **Author:** <operator> (Moose) + Claude (design)
- **Session:** S119
- **Date:** 2026-07-18
- **Module version:** Muse (post-S119 acquisition sprint: MUSEM-01..06 merged)
- **Estimated total:** ~30h autonomous agent work (excl. the operator ops prereqs)
- **Context:** Operator ask (2026-07-18), the phase AFTER the MUSEM acquisition write-path. Three parts: (A) wire
  Muse directly to the web metadata services the *arr suite uses (IMDb / TVDB / TMDb / TVMaze / …) so Muse
  identifies + enriches titles itself, not just via *arr ingest; (B) give Muse READ-ONLY access to the real
  media library on the QNAP NAS so it scans the files on disk, metadata-tags them, and pulls posters / show art
  / resources the *arr suite captured; (C) **CRITICAL** — a sample-based **still-frame matching verification**
  that extracts frames from the actual media file and proves the identification is correct (the file tagged
  "Movie X" really is Movie X), not just a filename/metadata match. This is the verification spine of the whole
  scan/identity effort. Grounds on what Muse already has: the `TmdbClient` pattern (`src/trending/client.rs`),
  the ffmpeg input-seek primitive (`src/streaming/ffmpeg.rs`), the Chord local-inference seam
  (`src/taste_model/chord_client.rs`, OpenAI-compatible, graceful-degrade), the text-embed pipeline
  (`src/embed/`), and the `libraries.root_folder` / `media_metadata` / `media_files` model. HandBrake/transcode
  ingest is the step AFTER this (out of scope).

## Pre-flight
- Repo `moosenet/Muse` on gitea, default `main`. Build/test via the <host> compiler tool (degraded `cargo test`
  fallback while compiler `mode=test` is broken — TERM-423). Plane project `MUSE`, prefix `MUSEL` (confirmed
  free). Review panel `[codex, free, opus]` (agy quota-out). All secrets via central `Config` / <secret-manager> env
  (Muse has NO SecretManager) — provider API keys `MUSE_TVDB_*` / `MUSE_TMDB_API_KEY` (exists) / etc.
- **OPERATOR OPS PREREQUISITES (human-action, block the LIVE parts only — the code + fixture tests build without them):**
  1. **QNAP read-only mount** — provision a READ-ONLY mount of the media library on the Muse host, exposed to
     Muse as `MUSE_LIBRARY_ROOT`. Read-only is a hard constraint (no writes to the library, ever). Phase-B live
     scan is blocked on this; the scanner code + fixture-dir tests are not.
  2. **Vision model via Chord** — Phase C's strongest matching signal is a local VLM routed through Chord. Confirm
     a vision-capable model is reachable via `CHORD_URL` (charter: local-inference-first on <host>). If none is
     available, Phase C degrades to its non-VLM signals (still-liveness + metadata-consistency + the
     mismatch-detection harness) and the VLM check is a no-op until a model is configured.
  3. **Provider API keys** — TVDB (and any others chosen) added to <secret-manager> → materialized into Muse's env.

---

## Phase A — Metadata provider clients (Muse identifies/enriches directly)

### MUSEL-A1: Metadata provider trait + TVDB client
- **Priority:** High
- **Labels:** muse, metadata, providers
- **Agent:** claude
- **Estimate:** 5h
- **Description:** Add a `MetadataProvider` seam and a TheTVDB v4 client (the primary TV-metadata source the
  *arr suite uses, TVDB-keyed), mirroring the existing `TmdbClient` shape. Read-only lookups: resolve a title /
  id → normalized metadata (title, overview, genres, images, ratings, network, airdates, provider ids). No writes.

  ## FILES
  - `src/metadata/mod.rs` — new module: `MetadataProvider` async trait + `ProviderMetadata` normalized struct + a `MockMetadataProvider`
  - `src/metadata/tvdb.rs` — `TvdbClient` (v4 API; login→token if required, `from_config` → Option)
  - `src/metadata/config.rs` — provider config read from central `Config` (`MUSE_TVDB_API_KEY`/`_PIN` etc.)
  - `src/config.rs` — add the TVDB fields to central `Config::from_env`
  - `src/lib.rs`/`src/main.rs` — declare the module
  - `README.md` — document the metadata-provider layer + env

  ## APPROACH
  1. `MetadataProvider` trait: `resolve_by_id(kind, provider_id) -> MuseResult<Option<ProviderMetadata>>` and `search(query, kind) -> MuseResult<Vec<ProviderMetadata>>`. `ProviderMetadata` carries a `provider_ids` map (tvdb/tmdb/imdb/…), title, overview, genres, images (poster/backdrop URLs), ratings, first_aired/year, network — the union Muse needs, nullable per provider.
  2. `TvdbClient` follows `TmdbClient`: `struct { http, base_url, api_key }`, `new`, `from_config(config) -> Option<Self>`; TheTVDB v4 needs a login (`POST /login` with the apikey → bearer token) before queries — cache the token, re-auth on 401 (mirror the qbit reauth shape). Map `/series/{id}` / `/movies/{id}` / `/search` responses into `ProviderMetadata`.
  3. Keys via central `Config` only; token/key never logged.
  4. httpmock parsing tests (login→token; resolve_by_id parses; search parses; 401→one reauth; missing key → `from_config` None).

  ## TEST PLAN
  - `compiler_build(module="muse", ref=<branch>, mode=test)` — tests pass.
  - httpmock: TVDB login token flow; series+movie resolve; search; 401 reauth; `from_config` None without a key.
  - Negative: a 5xx / not-found → typed error / `None`, never a panic.
  - No hardcoded infra; key via `Config`, not `std::env::var`.

  ## EDGE CASES
  - TVDB token expiry mid-run → single transparent reauth.
  - A title present in TVDB but missing some fields → nullable in `ProviderMetadata`, not an error.
  - `from_config` None (no key) must let callers degrade gracefully (the provider is simply absent).

- **Acceptance criteria:**
  - [ ] `MetadataProvider` trait + `TvdbClient` (v4, token auth w/ one reauth) resolve/search into normalized `ProviderMetadata`
  - [ ] Keys via central `Config`/SecretManager; `from_config` returns None when unset; token never logged
  - [ ] httpmock tests cover login, resolve, search, reauth, and the no-key path
  - [ ] Negative test: 5xx/not-found returns typed error/None, no panic
  - [ ] No hardcoded infrastructure values; README documents the provider layer + env
  - [ ] All existing tests still pass

### MUSEL-A2: Provider-resolution + enrichment aggregator (TMDb + TVDB + IMDb id bridge)
- **Priority:** High
- **Labels:** muse, metadata, enrichment
- **Agent:** claude
- **Estimate:** 5h
- **Description:** A resolver that, given a title's known ids (from arr ingest / filename parse), fans out to the
  configured providers (existing `TmdbClient` + MUSEL-A1 `TvdbClient`, plus an IMDb-id bridge), merges into one
  `ProviderMetadata` (provider-id precedence: movies TMDb-primary, TV TVDB-primary per ARR-BLUEPRINT §7), and
  persists the enrichment onto `media_metadata` (images/ratings/genres/overview/keywords) — read from providers,
  written only to Muse's own DB. This is what lets Muse identify/enrich without *arr.

  ## FILES
  - `src/metadata/resolve.rs` — `resolve_and_merge(ids, kind, providers) -> MuseResult<ProviderMetadata>`
  - `src/repo/media_metadata.rs` — enrichment upsert (images/ratings/genres/overview onto the existing row)
  - `README.md` — document the resolver + precedence

  ## APPROACH
  1. Fan out to each configured provider (skip absent ones — `from_config` None), merge fields with a documented precedence (TMDb-primary for movies, TVDB-primary for TV; fill gaps from the others; union the `provider_ids` map — anime carries 5+ ids per the blueprint).
  2. Persist the merged enrichment onto `media_metadata` via the repo (Muse's DB only; never write to a provider or the library).
  3. Wire it into the existing enrichment/maintenance path as an optional step (gated; degrades cleanly when no providers configured).
  4. Pure-merge logic is unit-testable (providers passed in / mocked).

  ## TEST PLAN
  - `compiler_build … mode=test` — tests pass.
  - Unit: merge precedence (TMDb vs TVDB), gap-fill, provider-id union; a title with only one provider still resolves; no providers configured → clean no-op (not an error).
  - Persistence round-trip (db-gated) writes enrichment onto `media_metadata`.
  - No hardcoded infra; provider keys via `Config`.

  ## EDGE CASES
  - Conflicting fields across providers → precedence rule decides deterministically, logged.
  - A provider down mid-fan-out → that provider skipped, others still merge (graceful).
  - No known ids for a title → falls back to `search` by title, lowest-confidence, flagged (never a wrong-confident match).

- **Acceptance criteria:**
  - [ ] `resolve_and_merge` fans out to configured providers and merges with documented id-precedence; persists enrichment to `media_metadata` (Muse DB only)
  - [ ] Absent/erroring providers are skipped gracefully; no-providers → clean no-op
  - [ ] Unit tests cover precedence, gap-fill, provider-id union, single-provider, and the down-provider path
  - [ ] Never writes to a provider or the library; no hardcoded infra; keys via `Config`
  - [ ] README updated; all existing tests still pass

---

## Phase B — Read-only QNAP library scan (BLOCKED on the RO mount ops prereq for LIVE run; code+fixtures build now)

### MUSEL-B0: Provision the read-only QNAP library mount (OPERATOR)
- **Priority:** High
- **Labels:** muse, infra, ops
- **Agent:** <operator>
- **Estimate:** 30m
- **Type:** human-action
- **Description:** Provision a READ-ONLY mount of the QNAP media library on the Muse host and expose its path to
  Muse as `MUSE_LIBRARY_ROOT` (materialized into Muse's env). Read-only is a hard constraint — Muse must never
  be able to write to the library. Until this exists, MUSEL-B1's live scan is inert (it no-ops when
  `MUSE_LIBRARY_ROOT` is unset); the scanner code + fixture-dir tests do not need it.
- **Steps:** mount the QNAP share read-only on the Muse host (fstab `ro` or a bind mount); confirm Muse's uid can
  read but not write; set `MUSE_LIBRARY_ROOT` in Muse's env (<secret-manager>-materialized or an `/etc/…` EnvironmentFile).

### MUSEL-B1: Read-only filesystem library scanner + art/sidecar pull
- **Priority:** High
- **Labels:** muse, library, scan
- **Agent:** claude
- **Estimate:** 6h
- **Description:** Walk `MUSE_LIBRARY_ROOT` (read-only), enumerate media files, match each to a `media_metadata`
  row (by the arr-captured ids / a filename parse → MUSEL-A2 resolve), record the on-disk `media_files` path,
  and pull posters/art (from sidecar `.nfo`/poster/fanart files beside the media, else re-fetch via the
  providers). READ-ONLY: opens files only for reading; never writes into the library. Persists only to Muse's DB.

  ## FILES
  - `src/library/mod.rs` + `src/library/scan.rs` — the walker + match + record
  - `src/library/sidecar.rs` — parse `.nfo`/detect poster/fanart/season art beside a media file
  - `src/repo/media_files.rs` — upsert scanned file rows
  - `src/config.rs` — `MUSE_LIBRARY_ROOT`
  - `README.md`

  ## APPROACH
  1. Recursively walk `MUSE_LIBRARY_ROOT` for media extensions (mkv/mp4/…); for each, parse title/year/S/E (reuse the prowlarr parse conventions where sensible), resolve to a `media_metadata` row via known ids or MUSEL-A2, and upsert a `media_files` row (path, size, container). Scan is inert (clean no-op) when `MUSE_LIBRARY_ROOT` is unset.
  2. Sidecar: detect `movie.nfo`/`tvshow.nfo`, `poster.jpg`/`fanart.jpg`/`folder.jpg`/season art beside the media — read them (RO) and record/attach as the item's art (store the path or cache the bytes in Muse's art store); else re-fetch from the providers.
  3. Strictly read-only: `OpenOptions` read; assert no write path anywhere in the module. Non-blocking per-file (a bad file logs + continues).
  4. Idempotent: re-scanning an unchanged file is a no-op (mtime/size guard).

  ## TEST PLAN
  - `compiler_build … mode=test` — tests pass.
  - Fixture-dir tests (a temp dir tree with sample filenames + a `.nfo` + a `poster.jpg`) — no live QNAP needed: walk finds the files, parses, matches (mocked resolver), records rows, detects the sidecar art.
  - Read-only proof: a test asserts the scanner opens files read-only and the module has no write/create/remove call (grep-style + behavioral).
  - Unset `MUSE_LIBRARY_ROOT` → clean no-op. Negative: an unreadable/garbage file → skipped, scan continues.
  - No hardcoded infra.

  ## EDGE CASES
  - Symlinks / nested season folders / extras subfolders → handled or explicitly skipped, documented.
  - A media file with no matchable metadata → recorded as unmatched (visible), never a wrong-confident match.
  - Huge library → bounded/streamed walk, capped per pass, resumable.
  - RO mount momentarily unavailable → logged, pass aborts cleanly (never a crash, never a write attempt).

- **Acceptance criteria:**
  - [ ] Scanner walks `MUSE_LIBRARY_ROOT` READ-ONLY, matches files to `media_metadata`, records `media_files`, and pulls sidecar/provider art — persisting only to Muse's DB
  - [ ] Provably read-only (test asserts no write/create/remove path; files opened read-only)
  - [ ] Inert clean no-op when `MUSE_LIBRARY_ROOT` unset; idempotent re-scan; non-blocking per file
  - [ ] Fixture-dir tests cover walk/parse/match/sidecar without a live mount; unmatched files recorded not mis-matched
  - [ ] No hardcoded infrastructure values; README updated; all existing tests still pass

---

## Phase C — Still-frame matching verification (CRITICAL)

### MUSEL-C1: ffmpeg sample-still extraction primitive
- **Priority:** High
- **Labels:** muse, ffmpeg, matching
- **Agent:** claude
- **Estimate:** 4h
- **Description:** Extend the existing `streaming/ffmpeg.rs` input-seek primitive to extract N sample still frames
  from a media file at spread timestamps (e.g. 10/30/50/70/90% of runtime), as in-memory JPEGs — the raw
  material for the matching verification. Read-only on the media file; bounded; graceful when ffmpeg is absent.

  ## FILES
  - `src/streaming/ffmpeg.rs` — add `build_still_args(file_path, seek_ms) -> Vec<String>` (`-ss` before `-i`, `-frames:v 1 -f image2pipe -vcodec mjpeg pipe:1`)
  - `src/matching/mod.rs` + `src/matching/stills.rs` — `extract_sample_stills(file_path, runtime_ms, n) -> MuseResult<Vec<Still>>` (spawns ffmpeg per timestamp, captures stdout JPEG bytes)
  - `README.md`

  ## APPROACH
  1. `build_still_args`: reuse the fast input-seek (`-ss` before `-i`), decode exactly one frame, output MJPEG to `pipe:1` — one still per invocation (mirrors MUSE-29's one-invocation-per-program discipline).
  2. `extract_sample_stills`: compute spread timestamps from `runtime_ms`, spawn ffmpeg per timestamp (bounded/capped), capture the JPEG bytes + the timestamp into a `Still`. Never writes to disk beside the media (bytes in memory or Muse's own scratch). ffmpeg-missing → typed `NotImplemented`/graceful (same posture as `streaming`).
  3. Read-only on the input; a decode failure on one timestamp doesn't abort the rest.

  ## TEST PLAN
  - `compiler_build … mode=test` — tests pass.
  - Unit: `build_still_args` places `-ss` before `-i` and requests exactly one MJPEG frame; timestamp-spread math (n stills across runtime, endpoints avoided). (Pure arg/logic tests — no real ffmpeg needed in CI.)
  - ffmpeg-absent → typed graceful error, not a panic.
  - No hardcoded infra.

  ## EDGE CASES
  - Very short / unknown runtime → clamp the spread, avoid seeking past EOF.
  - A black/slate frame at a timestamp → still captured (liveness judged in MUSEL-C2), not silently dropped.
  - ffmpeg missing / decode error on one timestamp → that still skipped, others returned.

- **Acceptance criteria:**
  - [ ] `build_still_args` (input-seek, single MJPEG frame) + `extract_sample_stills` (spread timestamps, per-timestamp ffmpeg, bytes+ts captured), read-only, bounded
  - [ ] ffmpeg-absent and per-timestamp failures degrade gracefully (no panic, others returned)
  - [ ] Unit tests cover the arg builder + spread math without a real ffmpeg
  - [ ] No hardcoded infra; README updated; all existing tests still pass

### MUSEL-C2: Matching-verification harness (the critical "is the match real?" check + mismatch detection)
- **Priority:** Critical
- **Labels:** muse, matching, verification
- **Agent:** claude
- **Estimate:** 6h
- **Description:** THE critical piece: given an identified file + its `ProviderMetadata`, extract sample stills
  (MUSEL-C1) and produce a `MatchVerdict{Consistent|Inconsistent|Inconclusive, confidence, reasons}` proving the
  file really is the identified title — plus a **mismatch-detection harness** that proves the check actually
  DISCRIMINATES (a deliberately-mislabeled file must come back `Inconsistent`). Signals, in order of strength,
  each optional/graceful: (1) **local VLM via Chord** — ask a vision model "is this frame consistent with
  <title> (era/genre/setting; not a slate/test-pattern/black frame)?" routed through the existing `ChordClient`
  seam (charter local-inference-first; no-op when no vision model configured); (2) **still-liveness** — reject
  all-black / uniform / near-duplicate stills (a file that yields no real content fails); (3) **metadata
  consistency** — runtime vs provider runtime, aspect/resolution vs expected, container sanity. The verdict
  combines them; the VLM, when present, is the strongest discriminator.

  ## FILES
  - `src/matching/verify.rs` — `verify_match(file, metadata, stills, vision) -> MatchVerdict`
  - `src/matching/vision.rs` — a thin adapter over `ChordClient` for the frame-consistency question (graceful None)
  - `src/matching/liveness.rs` — still-liveness heuristics (black/uniform/dup detection on JPEG bytes)
  - `tests/` or `#[cfg(test)]` — the **mismatch harness** (a labeled-correct case → Consistent; a labeled-wrong/synthetic-mismatch case → Inconsistent)
  - `README.md`

  ## APPROACH
  1. `verify_match`: run the three signal groups, each returning a partial score + reasons; combine into `MatchVerdict` (VLM-present → high-confidence Consistent/Inconsistent; VLM-absent → the liveness+metadata signals give a weaker Consistent/Inconclusive, and can still flag a clear Inconsistent from a hard metadata/liveness contradiction).
  2. `vision.rs`: build the multimodal Chord request (image bytes + the title/kind/year prompt) through `ChordClient`; `from_config` None (no `CHORD_URL` / no vision model) → the vision signal is skipped, verdict falls back to the other signals (never fails the pipeline).
  3. `liveness.rs`: cheap JPEG heuristics — mean/variance luma (reject near-uniform/black), inter-still difference (reject all-identical), so a file that decodes to garbage/slates fails liveness.
  4. Emit the verdict + reasons for a scan report; NEVER auto-delete/re-tag on an Inconsistent — it flags for operator review (the safety posture).
  5. **The mismatch-detection harness is the acceptance spine:** deterministic fixtures (or synthetic stills + a mismatched `ProviderMetadata`, e.g. wrong runtime + a "vision says no" mock) prove `verify_match` returns `Inconsistent` for a mislabeled file and `Consistent` for a correct one — i.e. the matching logic genuinely discriminates. This is the operator's "ensure the matching logic is actually working."

  ## TEST PLAN
  - `compiler_build … mode=test` — tests pass.
  - **Positive:** correct metadata + live/varied stills + a mock vision "yes" → `Consistent` (high confidence).
  - **Negative / mismatch (CRITICAL):** (a) a mock vision "no" + wrong runtime → `Inconsistent`; (b) all-black/uniform stills → `Inconsistent`/`Inconclusive` via liveness regardless of vision; (c) a runtime that grossly disagrees with the provider → flagged.
  - **VLM-absent:** `from_config` None → verdict computed from liveness+metadata only, never a panic, never a false Consistent from vision alone.
  - `verify_match` never mutates the file/library/metadata (verdict-only); no hardcoded infra; Chord routed via `ChordClient`, no direct model URL.

  ## EDGE CASES
  - No stills extractable (ffmpeg missing / unreadable) → `Inconclusive` (not a false Consistent, not a crash).
  - VLM available but errors/timeouts → that signal skipped, verdict from the rest (graceful).
  - A genuinely correct but visually-atypical title (e.g. black-and-white / abstract) → liveness tuned to avoid false Inconsistent; the VLM/metadata carry it; document the tuning.
  - NEVER acts on a verdict (no delete/re-tag) — flags only.

- **Acceptance criteria:**
  - [ ] `verify_match` returns `MatchVerdict{Consistent|Inconsistent|Inconclusive, confidence, reasons}` from VLM(via ChordClient) + still-liveness + metadata-consistency, each optional/graceful
  - [ ] **Mismatch harness proves discrimination:** a mislabeled/synthetic-mismatch file → `Inconsistent`; a correct file → `Consistent` (the critical acceptance test)
  - [ ] VLM-absent path computes a verdict from the other signals with no panic and no false Consistent
  - [ ] Verdict-only — never deletes/re-tags/mutates the file, library, or metadata (flags for operator review)
  - [ ] Chord routed via the existing `ChordClient` seam; no hardcoded infra/model URL
  - [ ] README documents the verification signals + the mismatch harness; all existing tests still pass

---

## Operator decisions to confirm (non-blocking; sensible defaults chosen)
1. **QNAP RO mount** (MUSEL-B0) — provision it; confirm `MUSE_LIBRARY_ROOT` + read-only.
2. **Vision model** — which vision-capable model via Chord for MUSEL-C2 (or confirm none yet → the harness runs
   on liveness+metadata until one is configured). This is the strongest matching signal; worth a real VLM on <host>.
3. **Providers to prioritize** — TVDB (A1) + the TMDb we have cover movies+TV; add TVMaze / MusicBrainz / Fanart
   later (music + richer art). Confirm the initial set.
4. **Match-action policy** — this spec is verdict-ONLY (flags, never acts). Confirm Muse should never
   delete/re-tag on an Inconsistent verdict without explicit operator action (the safe default).
