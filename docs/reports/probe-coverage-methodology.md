# Probe coverage — methodology, and what is measurable today

**MPRB-09 (S130-A · Plane MUSE #146). Written 2026-08-04.**

This is the written artifact MPRB-09 owes. It is **not** the coverage report itself.

## Why this file exists, and why `probe-coverage.md` does not

S130-A names one artifact, `docs/reports/probe-coverage.md`, and says it is committed in
**MPRB-10** — after the live backfill has run. That file is **generated**: every number in it comes
out of `render_markdown`, a pure function over `CoverageReport`, so it is regenerable and diffable
rather than hand-maintained.

Committing it today would mean committing a report over **zero probed documents**. A zero report is
a valid output of this code (there is a test for it), but a file called "probe coverage" sitting in
`docs/reports/` reads as a measurement of the library, and it would be quoted as one. So it is not
written until there is something to measure.

This file holds the part that is genuinely written rather than generated: what the census covers,
what it delegates to, what today's evidence actually supports, and — the part that matters most —
**which numbers cannot be computed from data that exists today.** The split is deliberate: a single
file that is half generated and half prose drifts, because regenerating it either destroys the prose
or is skipped to preserve it.

---

## 1. The number this item exists to produce

> The epic: *"Spec A's coverage census produces the number that says whether spec E is central or an
> edge case — measure before building."*

**`direct_play_candidate_share` — status: NOT MEASURED. It cannot be computed today.**

The share is `direct_play_candidates / documents_v1`, where a candidate is a file for which
`foundry::directplay::direct_play_blockers` returns an **empty list**. Both terms come from
`media_files.media_info` documents at schema v1 — the envelope MPRB-05 added. **Those documents do
not exist at scale yet.** MPRB-10's backfill is the first thing that populates them. Until it runs,
the denominator is (near) zero, and the code reports the share as `n/a` rather than as `0.0%`:

- `0.0%` is a claim about the library.
- `n/a` is the absence of one.

Rendering the second as the first is the defect this epic has now found more than twenty times, so
the type refuses to: `share_pct` returns `Option<f64>` and is `None` on a zero denominator.
`fmt_share(None)` is the literal string `n/a`. Two tests pin it, and a mutation that renders an
absent share as `0.0%` is killed by both.

### Which function produces it

`src/foundry/directplay.rs::direct_play_blockers(&MediaProbe, &TranscodePolicy) -> Vec<DirectPlayBlocker>`,
called from `src/media/coverage.rs::CoverageCensus::observe_document`.

**It is called, never restated.** S130-A's own APPROACH section describes the headline as *"primary
video H.264 **and** default audio in AAC/AC-3/E-AC-3/MP3, in MP4/MKV"* — a hand-written codec list.
That instruction is deliberately **not** followed. It would have been the fourth statement of a rule
that already has exactly one home, and this repo has already paid for that shape:
`predicted_deletion_refusals` restated the deletion gate instead of calling it and was wrong by a
factor of twenty (3,158 titles, not ~160). Two bitmap-subtitle codec lists had already diverged on
`main` (SUBCODEC-01, #149), and MPRB-03 had to override an instruction to build a second HDR
classifier.

The mutation sweep includes exactly that instruction as a mutation — the spec's own codec list
substituted for the call. It is killed by five tests.

### What the number will mean when it exists — and what it will not

It is a **conservative proxy, pending spec C**, and the rendered artifact says so in the same
paragraph as the number:

- It answers "does anything in the blocker set apply, against a fleet-wide policy", not "will this
  direct-play to device X". The authoritative answer needs spec C's per-device `DeviceProfile`
  matching.
- An empty blocker list is **not a promise of direct play**. Facts we cannot observe (see
  `foundry::hdr::undetectable_formats`) are not in the blocker set.
- The policy it is judged against is `TranscodePolicy::direct_play_normalization()` — FOUNDRY-03's
  existing "will this direct play" constructor, not new thresholds invented here. Its accepted-codec
  lists **are** the compatibility rule; it differs from `TranscodePolicy::default()` only in the two
  *size* ceilings (resolution 3840x2160, bitrate 100 Mbps), which are bandwidth judgements rather
  than direct-play ones. The rendered header prints the policy field by field, from the same object
  the verdict was computed with.

---

## 2. What the distributional census covers

Over every row of `media_files`, as **coverage denominators** (stated before any share, because
"92% H.264" over a 30%-probed library is a lie):

| Bucket | Meaning |
|---|---|
| readable document (v1) | The denominator of every distribution below |
| legacy | Pre-S130 `{"container": …}`, written from the **filename**, never from the file. Excluded from the distributions |
| unprobed | `NULL` / JSON `null` |
| unreadable document (schema vN) | Written by a newer binary, or corrupt at a known version. Its own bucket per version, never folded into v1 |
| `probe_state` histogram | `ok` / `suspicious` / `unreadable` / `probe_failed` / not recorded. `suspicious` is **broken out**, never folded into `ok` |

Over the readable documents only, nine distributions — each with raw file count, share to one
decimal, and total bytes:

| Dimension | Delegates to |
|---|---|
| video codec | primary (non-cover-art) video stream |
| container | `MediaInfoDoc::derived_projection` → `policy::normalize_container` (resolves the `.webm`/`.mkv` shared-demuxer case) |
| audio codec | `media::derive::default_audio` — the track a player picks, **not** the first stream |
| audio channels | that same default track |
| dynamic range | `media::derive::dynamic_range` → `foundry::hdr::classify_hdr` |
| bit depth | `media::derive::bit_depth` → `foundry::hdr::pixel_bit_depth` |
| bit-depth **provenance** | observed (`bits_per_raw_sample`) vs inferred (`pix_fmt`) — not the same evidence |
| resolution class | `media::derive::resolution_class` → `foundry::validate::resolution_band` |
| subtitle kind | `media::derive::has_image_subtitles` → `media::probe::is_bitmap_subtitle_codec` |

Plus a **blocker distribution**: for each `DirectPlayBlocker` kind, how many files carry at least one
(deduped per kind within a file, so three unsupported audio streams are one file with an audio
problem). This is the part that tells spec E *what* it would have to do, not merely how much.

Nothing here records a title, a path, a library name or a hostname (S1). The one channel by which
per-file text can reach the artifact is the container label, which falls back to the file
**extension** for a container the crate has no policy for — bounded to 16 characters by
`MediaInfoDoc`, and pinned by a test.

**The distribution is never truncated.** A codec on one file in 200,000 stays in the table: the long
tail is what a transcode spec needs to see, and it is MPRB-04 wave-2's input.

---

## 3. The two stale premises, checked against the tree

S130-A carries a staleness banner. Both claims relevant to this item were verified rather than
believed.

### "No census exists. Nobody knows what is in the library." — **FALSE.**

A **decision-level** census exists and has run over the whole library:
`foundry::survey::survey_files`, raised from a 500-file sample to full-library scope by FOUNDRY-24
(`src/web/dashboard.rs` clamps the survey limit to 50,000 with a 3,600s deadline; `survey.rs` has a
test asserting exactly that). It answers "how many would the planner rewrite, and why". It answers it
well, and it is not what MPRB-09 was scoped to build.

### "Only the *distributional* census is missing." — **CONFIRMED.**

The survey's output is `SurveySummary { already_optimal, would_transcode, cannot_decide,
probe_failed }` plus per-file reasons. There is no codec, container, resolution, bit-depth, HDR,
audio or subtitle distribution anywhere in the tree, and nothing correlates any of those with a
direct-play verdict. `foundry::validate::diversity_key` comes closest, and it is a *sampling* key —
its job is to pick twelve awkward files, not to describe sixteen thousand.

### The other two prerequisites

- **MPRB-05 merged** — confirmed: `migrations/0113_media_files_probe.sql`, `src/media/doc.rs`
  (`MediaInfoDoc`, `StoredMediaInfo`, `StoredProbeState`, `MEDIA_INFO_SCHEMA_VERSION = 1`), and
  `MediaFile::stored_media_info()` as the one typed reader.
- **MPRB-03 merged** — confirmed: `src/media/derive.rs` exists and every accessor in it delegates.

---

## 4. What IS measured today — and each figure's denominator

These are the numbers that exist in the tree right now. They come from the **decision** layer
(a filesystem walk + live ffprobe), not from the persisted `media_info` documents, and they answer a
*different question* from the direct-play share. They are recorded here with their sources so that
MPRB-10's report can be compared against them rather than replacing them silently.

| Figure | Value | Denominator | Recorded at |
|---|---|---|---|
| Library size | 16,221 media files | — | `src/foundry/survey.rs`, `src/foundry/validate.rs`, `src/foundry/rendition.rs` |
| Container mix, measured sample | 193 avi · 151 mkv · 54 mp4 · 2 m4v | **400 files**, not the library | `src/foundry/validate.rs` module doc |
| Would be re-encoded (full run) | 3,621 titles | 16,221 | `src/web/dashboard.rs` (`RunBody` doc) |
| …of those, original can ever be reclaimed | 463 titles | 3,621 | same |
| Earlier 500-file survey: would be re-encoded | ~60% | **500 files** | `src/foundry/policy.rs` |
| …refused deletion afterwards | ~3,000 titles, 91 of 93 flagged were DTS | 500-file survey, extrapolated | `src/foundry/policy.rs` |

**Three cautions, stated rather than smoothed over.**

1. **The container sample is the single most direct evidence bearing on the direct-play question, and
   it is a sample of 400.** 193 of 400 files were AVI. AVI is not in `acceptable_containers`, so
   every one of those is a `ContainerNotStreamable` blocker before any codec is considered. That is a
   strong signal that the direct-play share is **not** high — but it is a signal from 2.5% of the
   library, drawn for shape coverage rather than at random, and it is **not** the census. It is not
   extrapolated here, and it must not be extrapolated elsewhere.
2. **Two recorded figures do not reconcile from the tree alone.** The 500-file survey says ~60% would
   be re-encoded; the full-library run says 3,621 of 16,221, which is 22.3%. Accepting DTS
   (`policy.rs`) moved ~3,000 titles into `already_optimal` and explains part of the gap, not all of
   it. No reconciliation is attempted here, and neither figure is used as an input to anything.
3. **"Would be re-encoded" is not "would not direct-play."** The transcode policy and the direct-play
   policy differ deliberately: `direct_play_normalization` raises the resolution ceiling to 4K and
   the bitrate ceiling to 100 Mbps, because a high bitrate costs bandwidth and never prevents direct
   play. Reading 22.3% as a direct-play answer would be wrong in both directions.

---

## 5. Numbers that cannot be computed today

| Number | Why not |
|---|---|
| `direct_play_candidate_share` | Needs v1 `media_info` documents; MPRB-10's backfill is the first thing that writes them at scale |
| Every distribution in §2 | Same denominator |
| Blocker distribution | Same |
| Coverage denominators (probed / legacy / unprobed) | Requires a live database; there is no `MUSE_TEST_DATABASE_URL` in the build environment (#130) |

None of these is estimated. A plausible estimate presented as a measurement is the exact defect this
epic keeps finding.

---

## 6. How to produce the real artifact (MPRB-10)

1. Apply migration `0113` through the `pg_ddl` operator door (migrations are not auto-applied).
2. Run MPRB-07's backfill until `probe_state` is populated. Watch `suspicious` and `probe_failed`
   separately — a backfill that has stopped erroring is not a backfill that has finished.
3. `POST /ops/probe/coverage-report` (bearer-protected) → redirect the `text/markdown` body into
   `docs/reports/probe-coverage.md` and commit it. `GET /probe/coverage` returns the same data as
   JSON.
4. Read the coverage table **before** any share. If the probed share is not near 100%, every
   distribution below it describes the probed subset, not the library.
5. Build the binary with `MUSE_GIT_SHA` set, or the header will say `not recorded` — honestly, but
   uselessly.

## 7. What is verified in this item, and what is not

**Verified, executing, no database:** every rule above. 31 tests over the pure fold, the labels and
the renderer, all of which run in the ordinary `cargo test --bin muse`. 28 mutations applied to
production symbols; **28 killed, 0 survivors** — including the spec's own hand-written codec list
substituted for the `direct_play_blockers` call.

**NOT verified — skipped, not passing:** the two `db_gated` tests in `media::coverage::tests::db_gated`
(`the_census_pages_over_every_row_exactly_once`,
`a_report_from_the_pool_carries_its_denominators_and_schema_version`). With no
`MUSE_TEST_DATABASE_URL` they return without asserting anything and report `ok` to cargo, and the
`eprintln!` that says so is captured for a passing test and never reaches CI (#155). What is
consequently unverified against a real database: the keyset paging loop in `census_from_pool`, the
`SELECT`, and the two HTTP handlers end to end. Every rule *below* those is tested here, which is why
the database edge was kept to a `SELECT` and a `for` loop with no aggregation logic in it.
