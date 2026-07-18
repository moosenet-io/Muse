# S119-muse-media-management Build Report

**Sprint 1: MUSE native media management — the acquisition write-path foundation**
(Sonarr/Radarr/Lidarr/<media-service> responsibilities, native; Prowlarr + qBittorrent kept as substrate.)

## Summary
- Items completed: **6 / 6** (MUSEM-01..06) — all merged to `main`, verified.
- Final main verify: **1050 passed / 0 failed / 1 ignored** (the documented MUSET-05 guard).
- PRs: #70 (schema), #71 (Prowlarr search), #72 (qbit), #73 (decision), #74 (request lifecycle), #75 (wanted worker).
- Plane: MUSE-36..42 (all Done); follow-ups MUSE-43 (MUSEM-07), MUSE-44 (MUSEM-08); TERM-423 (compiler defect).
- Total review cycles: ~22 across the 6 items (MUSEM-06 took 8 — the subtlest).
- **Real defects caught by the dual-review gate: 12** (per-item) + 3 more (Epic capstone), none of which the local test suites detected.

## Per-item detail
| Item | PR | Review cycles | Real defects caught + fixed |
|------|-----|-----|------|
| MUSEM-01 schema | #70 | 2 | download_queue FK `SET NULL` violated the has-source CHECK on parent-delete → CASCADE |
| MUSEM-02 qbit | #72 | 4 | scattered secret read → central Config; urlencoded → multipart/form-data; `200 Fails.` treated as success → body-validated |
| MUSEM-03 Prowlarr search | #71 | 1 | (clean) |
| MUSEM-04 decision engine | #73 | 5 | repack over-ranking; tier-regression on upgrade; missing-cutoff direction; cutoff-0; nested quality-group `allowed` |
| MUSEM-05 request lifecycle | #74 | 1 (+3 findings) | **master gate bypassable at the sink → enforced inside `fulfill_request` chokepoint**; capability ignored download client; approve stranded requests |
| MUSEM-06 wanted worker | #75 | 8 | broken idempotency (queue row lacked `monitored_item_id`); dishonest search cap (double-search); no-cap/NeedsReview persist spam; existence-guard gap (failed-then-NeedsReview lost its request); AutoApprovable orphaned the pending request |

## Epic Review capstone (advisory)
Royal panel `[opus, codex, free]` over the assembled write-path. opus confirmed all 4 safety invariants HOLD
(gate inside `fulfill_request`; never writes to *arr; auth-gated endpoints; secrets via central Config).
Credible residuals → **MUSEM-08**: (1) `POST /requests` still double-searches (worker fixed it, handler didn't);
(2) idempotency is read-before-write not atomic (concurrent double-grab; generalizes MUSEM-07). **`free`
HALLUCINATED** non-existent files/functions — all its findings rejected (documented capstone lesson).

## What this sprint delivers
MUSE can now natively take a title **request → targeted Prowlarr search → scored release decision → grab into
qBittorrent → persist queue + typed history**, and drive a monitored "wanted" list — the <media-service> +
Sonarr/Radarr acquisition-decision + grab responsibilities, keeping Prowlarr/qBittorrent as substrate. The
master acquisition gate defaults **OFF** and is enforced at the grab chokepoint (nothing grabs a torrent until
the operator enables it). NOT yet built (deliberate, later sprints): import/rename/organize (the guarded 80%),
Lidarr/music, *arr retirement cutover.

## Process notes
- Compiler `mode=test` is defective (spurious instant 0/0/0) → **TERM-423**; test-gates ran via the skill's
  sanctioned degraded fallback (capped `cargo test` on the <host> build host), logged.
- Prefix-registry baseline validator rejects project `MUSE` (only HARM/LUM/CHRD/TERM/RAIL/HW/PSH) — `MUSEM`
  claimed in the overlay; `plane_prefix_promote` blocked pending a small Terminus fix.
- Muse has no Atlas KG (MUSEM-00, ops) — grounding was the verified module audit + ARR-BLUEPRINT recon;
  capstone `kg_rebuild` skipped (repo not on SCRIBE_ALLOWED_REPO_ROOTS).
- codex's 120s wall-clock timeout forced per-provider split reviews on the larger diffs.

## Known issues / follow-ups
- **MUSEM-07** — DB-enforce the one-pending-request-per-monitored-item invariant (partial unique index).
- **MUSEM-08** — capstone follow-ups (POST /requests double-search; atomic idempotency across approve/queue).
- Nothing DEPLOYED — all merged to gitea `main`; deploying muse (constellation-updater) is operator-aware.
- **Next phase (operator ask):** metadata-provider links (IMDb/TVDB/TMDb/…) + read-only QNAP library scan +
  still-frame matching verification (critical), then HandBrake ingest.
