# S119b-muse-library-scan-matching Build Report

**Sprint 2: MUSE Library — metadata providers, read-only library scan, still-frame matching verification.**

## Summary
- Code items completed: **5 / 5** (MUSEL-A1, A2, C1, C2, B1) — all merged to `main`, verified.
- Operator ops prereqs (not code): **MUSEL-B0** (read-only QNAP mount → `MUSE_LIBRARY_ROOT`) and a **vision model via Chord** for C2's strongest signal — the code runs inert/degraded until these exist.
- Final main verify: **1172 passed / 0 failed / 1 ignored** (the documented MUSET-05 guard).
- PRs: #76 (C1), #77 (A1), #78 (C2), #79 (A2), #80 (B1).
- Plane: MUSE-45..50 (Done); follow-ups MUSEL-B2, MUSE-51 (S7 Config Debug leak), MUSE-TEST (db-gated -j flakiness).
- **Real defects caught by the dual-review gate: ~15**, none detected by the local test suites.

## Per-item detail
| Item | PR | Cycles | Real defects caught + fixed |
|------|-----|-----|------|
| MUSEL-C1 ffmpeg still extraction | #76 | 1 | (clean) |
| MUSEL-A1 metadata provider + TVDB | #77 | 4 | v3/v4 field-shape variance; base-vs-`/extended` endpoint (lost overview/genres/**remoteIds** id-bridge); Debug-leak of key/pin |
| MUSEL-C2 **matching-verification (critical)** | #78 | 1 | empty-stills suppressed metadata evidence; **byte-level liveness proxy empirically couldn't tell a real black frame from a varied one → switched to pixel-decode** |
| MUSEL-A2 provider-resolution + enrichment | #79 | 1 | low-confidence title-search could persist as a confident match; keywords/README mismatch; non-additive (clobbering) enrichment |
| MUSEL-B1 read-only scanner | #80 | 6 | failed-id fallthrough; `str` vs `Path` root containment (`/mnt/library2`); sidecar-attach gated on file-change (missed newly-added art); `.nfo` detect-not-attach; **`.nfo` `type=` mis-parse → wrong match**; **symlink sidecar boundary escape**; **title-only confident match** (no year); sidecar-priority determinism |

## Epic Review capstone (advisory)
`[opus, codex, free]` over the assembled surface. opus + codex both VERIFIED the core invariants HOLD:
read-only "airtight" (symlink rejection + structural grep-guard + DB-only persistence), never-wrong-confident
matching (failed explicit id / missing year / stray `.nfo type=` all refused), and verify_match verdict-only.
Credible new finding: **TMDb api_key leaks via `Config`/`TmdbClient` `Debug`** (TVDB was wrapped, TMDb wasn't) —
this is the broader **MUSE-51** S7 gap; prioritize it. `free` HALLUCINATED (cited "non-transactional SQLite
writes" — Muse is Postgres) — rejected.

## What this sprint delivers
Muse can now (once the operator provides `MUSE_LIBRARY_ROOT` + a vision model):
- **Identify/enrich titles itself** via TheTVDB v4 (`/extended`, remoteIds→tmdb/imdb bridge) + the TMDb adapter +
  IMDb bridge, with a resolver that refuses to persist a low-confidence title-guess as authoritative.
- **Scan the real library READ-ONLY** — walk `MUSE_LIBRARY_ROOT`, match files to the catalog (path id tag /
  `.nfo` `<uniqueid>` / title+year, never title-only, never a wrong-confident attach on a failed id), record
  `media_files`, and cache sidecar poster/fanart/`.nfo` art. Read-only is proven two ways and symlink-boundary-safe.
- **Verify a match is real (the critical piece)** — extract sample stills (ffmpeg) → a `MatchVerdict` from a local
  VLM (via the Chord seam, graceful) + real-pixel-decode liveness + metadata consistency; the mismatch-detection
  harness proves a mislabeled file returns `Inconsistent`. **Verdict-only** — flags for operator review, never
  deletes/re-tags.

## Known issues / follow-ups
- **MUSEL-B2** — scanner rescan idempotency-churn (`updated_at` bumped on every rescan).
- **MUSE-51** — `Config` `{:?}` leaks ~13 plaintext credential fields (incl. tmdb_api_key); wrap them all.
- **MUSE-TEST** — db-gated tests race under `-j` (shared `MUSE_TEST_DATABASE_URL`) → flaky gate (retry-passes).
- Nothing DEPLOYED — all merged to gitea `main`. Operator: provision `MUSE_LIBRARY_ROOT` (RO) + a Chord vision
  model, then the scan + matching run live. **Next after this: HandBrake/transcode ingest.**
