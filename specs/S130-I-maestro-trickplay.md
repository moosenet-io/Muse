# S130-I — Maestro: trickplay tiles, keyframe index, chapters and markers

plane_project: MUSE
module: Maestro
prefix: MTRK
spec_id: S130-I-maestro-trickplay

## Metadata
- **Author:** Moose
- **Session:** S130
- **Date:** 2026-08-01
- **Module version:** Maestro v0.1 (child spec **I** of `S130-maestro-epic.md`)
- **Repo / binary:** `moosenet/Muse`, second `[[bin]]` target `maestro` (epic §2). **Not a new repo.**
  Three items land on the **Muse** side of the split and say so explicitly (MTRK-01, MTRK-02,
  MTRK-09); every other item is Maestro's.
- **Estimated total:** ~62h autonomous agent work across 14 items
- **North-Star layer:** module
- **Module-Contract:** meets §4 clauses 1–7 as scoped. Clause 2 is load-bearing here and is
  discharged concretely: trickplay is a *derived* capability, so every surface it adds renders
  **inert with a stated reason** when tiles do not exist yet — which is the normal state for most of
  the library for most of this spec's life. Clause 4 (assistant-operable) is satisfied by MTRK-13's
  read-only diagnostics tool surface; nothing here needs a new mutating tool.
- **Depends on:** `S130-A-maestro-probe.md` (the shared `src/media/probe.rs` core and
  `media_files.media_info`), `S130-D-maestro-delivery.md` (`MediaHandle`, the read-only library
  resolution, the session model, the Maestro HTTP surface).
- **Relates to (does not depend on):** `S130-E-maestro-transcode.md` — E's segment-alignment math
  (MTRX-09) and seek path (MTRX-10) consume this spec's keyframe index directly. See §3.
  `S130-G-maestro-player-gui.md` — this spec extends G's `ScrubBar` and `PlayerControls`; it does not
  build a second player.
- **Context:** Both architecture reviews named scrub previews, chapters and accurate seeking as the
  single most-felt gap versus Plex once basic playback works. They are also the cheapest large win
  in the epic: none of them requires a transcoder, all of them are derived artifacts a background
  job can produce at leisure, and every one of them is visible the first time someone drags the
  scrub bar. This spec delivers the tiles, the index, the marker contract, and the player surfaces
  that render them.

---

## 0. The boundary this spec must not cross

Epic §2 splits the constellation into a **brain** (Muse) and a **muscle** (Maestro), and §4's
deferred list is unambiguous about where intro/credit detection sits:

> intro/credit detection — this is media *analysis* and belongs in **Muse** (it feeds markers, which
> Maestro merely consumes), never in Maestro

**This spec therefore contains no detection of any kind.** Not audio fingerprinting, not a
frame-similarity pass across a season, not a black-frame heuristic, not a Chord vision call. It
specifies the **marker consumption contract** — a Muse-owned table, a Muse-owned read endpoint, and
a player that renders whatever is in it — and stops there. Detection is a separate future Muse spec
(§9), and if a reviewer sees a detection heuristic appear inside `src/maestro/`, that item is
rejected on epic §2 grounds alone, however small the diff.

The split runs through the middle of this spec, so it is worth stating as a table rather than
leaving it to be inferred item by item:

| Concern | Owner | Where | Why |
|---|---|---|---|
| Chapter *data* (times + titles) | **Muse** | `src/media/probe.rs`, stored in `media_files.media_info` | A chapter is a library fact read out of the container. Muse probes and stores; Maestro asks (spec A's own framing). |
| Intro/credit **markers** | **Muse** | `media_markers` table (MTRK-02) | Content analysis output. Maestro never writes it and never derives it. |
| Marker/chapter **read endpoint** | **Muse** | `GET /media/{id}/markers` (MTRK-09) | Chapter titles are *text*. Epic §2 clause 5 and spec G rule (a): Maestro payloads carry ids and playback state; text is Muse's. |
| Trickplay **tiles** | **Maestro** | `MAESTRO_TRICKPLAY_WORK_DIR` | A derived, regenerable, id-keyed binary artifact. No text anywhere in it. |
| **Keyframe index** | **Maestro** | same work dir | Same: a byte/time table with no text. Consumed by spec E's seek. |
| Rendering all of the above | **constellation-web** | `panels/maestro/` | Composes Muse (what things are called) with Maestro (where the bytes are) by id. |

**Consequence, and it is deliberate: the player fetches markers from `proxy_muse` and tiles from
`proxy_maestro`, in the same component.** That looks like extra plumbing and it is exactly the
plumbing epic §2 clause 6 asks for — the client composition itself encodes the ownership split, so
the split cannot quietly erode through the GUI's back door.

---

## 1. Chapters: the argv is done, the model is not

**Spec A is correct and needs no amendment on this point.** Its "What already exists" table records
that `build_ffprobe_args` *already includes* `-show_chapters`, and its corrections list states
plainly that the flag "does **not** need adding — it has been in the argv since MUSEF-02, with a
test asserting it." This spec agrees, and MTRK-01 changes no argv and adds no probe invocation.

**But the argv being done is not the same as the data being available**, and that is the distinction
this section exists to draw. Verified on `main` at `e8499aa`, `src/foundry/probe.rs` — the 948-line
probe spec A's MPRB-01 promotes to `src/media/probe.rs`:

- `build_ffprobe_args` (line 42) already emits `-show_chapters`, and its doc comment (lines 38–41)
  argues for it: *"`-show_chapters` is not optional decoration: the transcode argv promises
  `-map_chapters 0`, and a promise that is never checked is the class of false claim this module
  exists to avoid."*
- `MediaProbe` already carries `chapter_count: usize` (line 103), parsed at line 546.
- There is already a test asserting the flag is present (`the_ffprobe_argv_asks_for_chapters`).

So the flag is not a thing this spec asks anyone to add, and it is tempting to conclude that chapters
are therefore pure *consumption* with nothing to build. **That conclusion is one line short.** Read
line 546: the parser does `chapter_count: raw.chapters.len()`, and `RawFfprobe.chapters` is a
`Vec<serde_json::Value>` (line 319) that is never touched again. **The chapters are counted and then
thrown away** — the times and titles that a chapter rail is made of are parsed off the wire and
discarded before anything downstream can see them. There is nothing to add to the *argv* and nothing
to re-probe; there is one small thing to add to the *model*, and without it MTRK-09 has no data to
serve.

**Why only the count was kept, and why that was right at the time.** This is not an oversight to
correct with a raised eyebrow. `probe.rs`'s own doc comment says what the count is for: the transcode
argv promises `-map_chapters 0`, and `chapter_count` is *"what makes that promise checkable"* —
Foundry needs to verify a rewritten file kept its chapters, and a count is entirely sufficient for
that. Foundry never needed to know *where* a chapter is or *what it is called*, because Foundry does
not draw a UI. MTRK-01 is therefore an **extension for a second consumer**, not a repair, and it must
leave the first consumer's verification working untouched. That reasoning goes into the code comment
so the next reader understands why the field grew rather than assuming someone had been sloppy.

MTRK-01 is therefore a genuinely small item: model the list instead of counting it, keep
`chapter_count` as a derived accessor so Foundry's existing verification is untouched, and let it
ride the **same single ffprobe invocation that already happens**.

**This is what "do not re-probe" means concretely.** There is no second ffprobe process anywhere in
this spec for chapters. There is one additional, different ffprobe invocation for the keyframe index
(MTRK-06) — a packet-level query that is a fundamentally different question from a stream/format
probe and cannot ride the same call — and that one is explicitly justified, bounded, and cached in
its own item rather than smuggled into the probe path.

**Cross-reference, not a correction.** Spec A's MPRB-01 owns the promotion of `src/foundry/probe.rs`
to `src/media/probe.rs` and MPRB-03 owns extending its stream model; MTRK-01 extends the same module
in place, under spec A's conventions and after its items land. **No amendment to spec A is required
or requested** — it already records the argv as complete, and it is right.

---

## 2. What a trickplay artifact actually costs — the number, up front

The spec's own storage estimate, because "bounded and evictable" is meaningless without a magnitude.

**Defaults chosen below:** one tile per **10 s**, tile width **320 px** (320×180 at 16:9), packed
**10×10 = 100 tiles per sheet** (3200×1800), JPEG quality `-q:v 5`.

| Quantity | Value |
|---|---|
| Tiles per hour of content | 360 |
| Sheets per hour (100 tiles each) | 3.6 |
| Bytes per 3200×1800 sheet, live-action, `-q:v 5` | ~400–600 KB |
| **Tiles per hour of content** | **~1.5–2.2 MB — plan on ~1.6 MB/h** |
| Keyframe index per hour (≈1800 keyframes at ~10 B packed) | **~18 KB/h — under 1.5% of the tiles** |
| A 2 h film | ~3.2 MB tiles + ~35 KB index |
| A 45 min episode | ~1.2 MB |
| A 22-episode season | ~26 MB |
| **A ~27 TB library at ~1.5 GB/h average bitrate (~18,000 h)** | **~29 GB tiles + ~0.3 GB index** |

Sensitivity to the two knobs that matter, because an operator *will* turn them up:

| Interval | Tile width | Approx. MB per hour | Whole-library approx. |
|---|---|---|---|
| 20 s | 320 px | ~0.8 | ~14 GB |
| **10 s (default)** | **320 px** | **~1.6** | **~29 GB** |
| 10 s | 480 px | ~3.2 | ~58 GB |
| 5 s | 320 px | ~3.2 | ~58 GB |
| 5 s | 480 px | ~6.4 | ~115 GB |

Three things follow from these numbers and each is an item below:

1. **`MAESTRO_TRICKPLAY_BUDGET_MB` defaults to `32768` (32 GB)** — deliberately *just above* the
   whole-library default estimate. Eviction is therefore a real, exercised path from the first month
   rather than a theoretical safety net nobody ever triggers, and turning the interval down to 5 s
   puts the library at roughly 2× the budget, which is exactly when an operator should be told
   rather than silently filling a disk.
2. **The estimate is a planning figure and must be replaced by a measurement.** MTRK-03 ships a pure
   estimator; MTRK-14 publishes the measured bytes-per-hour from the first live sweep and the
   estimator is asserted against it. A spec that estimates and never checks is how a disk-full
   incident starts.
3. **This is the cheapest artifact in the epic by a wide margin.** The whole library's trickplay is
   ~29 GB against a 27 TB library — 0.1%. The reason to bound it is not that it is large; it is that
   the fleet lost a card-backed volume in July and ran half-missing for three days, and an
   unbounded background writer is precisely the thing that turns a slow storage failure into an
   unattributable one (`pvf1_vgscratch_card_failure_2026_07`). The budget exists because of *how*
   this fleet fails, not because of *how much* this feature stores.

---

## 3. Relationship to spec E's scratch store — two stores, one accounting primitive

Spec E's MTRX-02 defines `src/maestro/transcode/scratch.rs`: a per-session segment scratch with a
budget, a `BudgetVerdict`, and behind-the-playhead eviction. This spec also writes bounded files to
disk. The obvious question is whether that is one store or two, and the answer matters because
getting it wrong produces either duplicated eviction logic or a single budget in which a scrub
preview can evict a live film's segments.

**Decision: two stores with different lifetimes and independent budgets, sharing one accounting
primitive.** The lifetimes are genuinely different and that is the whole argument:

| | E's segment scratch | This spec's derived artifacts |
|---|---|---|
| Keyed by | `session_id` (a UUID, per playback) | `media_file_id` + a source fingerprint |
| Lifetime | Minutes; dies with the session | Indefinite; reused by every future session |
| On restart | Orphaned, swept | Valid, reused |
| Regeneration cost | Seconds (respawn at an offset) | Minutes of full-file decode |
| Eviction policy | Behind the playhead | Least-recently-served, whole artifact |
| Correct pressure response | Refuse new sessions | Evict old artifacts silently |

Sharing one budget across those would let a scrub-preview sweep evict segments out from under a
film — the exact asymmetry MTRX-08 step 4 forbids in the other direction.

**What *is* shared:** the pure budget arithmetic and the free-space read.
`budget_verdict(used, per_unit_used, free, limits) -> BudgetVerdict::{Ok, ReapNeeded, Refuse}` and
`filesystem_free_bytes` are one implementation with two callers. **Whichever of MTRK-04 and MTRX-02
merges first ships them in `src/maestro/store/budget.rs`; the second adopts them and deletes its
own.** MTRK-04's acceptance criteria state this explicitly so the second implementer cannot
reasonably not notice. Both stores live under distinct subdirectories of their own configured roots
and neither ever enumerates the other's.

**What this spec gives E for free:** MTRK-06's keyframe index is exactly the data MTRX-09's segment
alignment and MTRX-10's seek need in order to respawn a transcode at a real keyframe rather than at
an arbitrary `-ss` that ffmpeg then rounds silently. E does not depend on I, and must not — but when
both exist, E's seek should read the index rather than re-derive it, and MTRK-06 exposes the lookup
(`nearest_keyframe_at_or_before(pts_ms)`) as a pure function specifically so E can. Note it in E's
follow-up; do not create the dependency retroactively.

---

## 4. The staleness question, and a deliberate divergence from the artwork-cache precedent

A trickplay artifact describes a file. If Foundry re-encodes that file (epic §2b's verify-and-swap),
or the operator replaces a rip with a better one, the tiles now describe a file that no longer
exists and every scrub preview is subtly wrong — the worst kind of wrong, because it looks like data.

Muse already solved this once, for artwork renditions, in `migrations/0109_artwork_renditions.sql`:
a rendition stores the **SHA-256 content hash of the master it was derived from**, and is served
only when the hashes match, so staleness is structurally impossible rather than merely unlikely.
That migration's comment is explicit that a timestamp is *not* a sound identity, because Postgres
`now()` is transaction-start time and `timestamptz` has finite precision.

**That reasoning is correct for a 1.9 MB poster and wrong for a 40 GB film.** Hashing every media
file to decide whether a thumbnail is stale would read the entire library off a network-mounted
share to answer a question about a 3 MB derivative. So this spec diverges, deliberately and with the
divergence written down:

**Provenance is a composite `source_fingerprint`, not a content hash:**

```
source_fingerprint = sha256(
    media_file_id ‖ size_bytes ‖ mtime_unix_nanos ‖ media_info_version ‖ TRICKPLAY_PARAM_VERSION
)
```

- `size_bytes` + `mtime` is what actually changes under Foundry's verify-and-swap and under any
  file replacement an operator performs — both write a new file and both change at least one.
- `media_info_version` folds in a re-probe, so a schema bump invalidates cleanly.
- `TRICKPLAY_PARAM_VERSION` is a crate constant bumped whenever the interval/tile-size/grid defaults
  change, so a knob change invalidates every artifact rather than mixing geometries in one library.
- The full parameter set (interval, tile size, grid, quality) is **also** recorded in the manifest,
  so a mismatch is detectable even if someone forgets to bump the constant. Belt and braces, because
  forgetting to bump a version constant is the single most predictable failure here.

**The honest statement of what this buys and does not buy:** a file rewritten in place with an
identical size and a preserved mtime (`touch -r`) will keep stale tiles. That is a real hole. It is
accepted because (a) nothing in the fleet does that — Foundry writes to a work dir and swaps, which
changes both — and (b) the consequence is a wrong scrub preview, not corruption, not a wrong
playback decision, and not a lost file. The artwork cache took the expensive option because its
failure mode was serving a *wrong image forever* with no cost to hashing; ours is not that. Say this
in the module doc comment so the next reader sees the divergence was reasoned rather than sloppy.

**Runtime coordination with Foundry (epic §2b).** Foundry may swap a file while Maestro is streaming
it. Trickplay generation must therefore hold its input open for the duration of a job (`kill_on_drop`
plus the ffmpeg child's own fd is sufficient — on Linux the open inode survives the swap), and
recompute the fingerprint **after** the job completes, discarding the output if it changed mid-run.
A tile sheet that is half old-file and half new-file is worse than no tile sheet.

---

## 5. Serving: the control plane, and why that is not a violation

Epic §8.6 carves media bytes out of the Terminus gateway — sustained video is served direct from
Maestro on signed, session-scoped, expiring URLs, because routing a film through the tool hub would
couple playback uptime to Terminus restarts.

**Trickplay sheets, the manifest and the keyframe index go through `proxy_maestro` on the Terminus
gateway — i.e. the control plane, the default, not the carve-out.** Stated as a decision with
reasons, because a reader who has internalised §8.6 will reasonably expect the opposite:

1. **They are not sustained.** A whole film's tiles are ~3 MB fetched lazily, sheet by sheet, only
   while a human is actively dragging. A Terminus restart mid-drag costs one missing preview frame,
   not a stopped film.
2. **Cookie auth already works there.** The browser is the only consumer. Putting tiles on the media
   plane would mean minting signed URLs for images, and signed-URL machinery exists in spec D for
   one reason — that `<video>` and a Cast receiver cannot set an `Authorization` header. Neither
   applies: an `<img>`/canvas fetch from constellation-web carries the cookie, and **a Cast receiver
   never renders scrub previews at all** (the sender's UI does).
3. **They are ideally cacheable.** Immutable, content-addressed by fingerprint, `Cache-Control:
   public, max-age=31536000, immutable` — the browser fetches each sheet once ever. A proxy hop on a
   request that mostly does not happen is free.

The carve-out exists for the case where it is needed. Extending it by reflex to everything Maestro
serves would make "Terminus fronts the module" (Module Contract clause 1) into a slogan.

---

## 6. The manifest, and the timeline decision inside it

The player needs to answer one question thousands of times per drag: *given a pointer at position
`t`, which pixels do I draw?* That is a pure function of a small manifest, and it is worth getting
the manifest shape right once.

```
GET /trickplay/{media_file_id}/manifest.json
```
```jsonc
{
  "v": 1,
  "media_file_id": 5678,
  "source_fingerprint": "sha256:…",
  "state": "ready",              // ready | partial | absent | unavailable | failed
  "duration_ms": 7231000,
  "tile_width": 320,
  "tile_height": 180,
  "grid_cols": 10,
  "grid_rows": 10,
  "tile_count": 723,
  "timeline": { "kind": "uniform", "interval_ms": 10000 },
  "sheets": [ { "index": 0, "tiles": 100, "first_tile": 0, "bytes": 462118 } ]
}
```

`muse_item_id` is **not** in the manifest and the route is not keyed on it. The task framing asked
for tiles keyed on `muse_item_id`; the correction is one level finer and strictly stronger: **tiles
key on `media_file_id`**, which is what a trickplay artifact actually describes. An item can have
several files (a 1080p and a 4K copy, a director's cut), and those have different runtimes and
different frames — one artifact per item would serve one file's previews while playing another's.
The client already knows the file id: spec D's session-open response resolves item → file, and
`GET /playback/sessions/{id}` carries it. The important half of the constraint holds unchanged and
is enforced in MTRK-08's tests: **the manifest carries no title, no poster, no overview, no year,
and no path.** Ids and geometry only.

### `timeline` is an enum, and the default is the non-uniform one

The naive generator is `-vf fps=1/10`, which **fully decodes the file** to emit one frame per 10 s.
For a 2 h HEVC 4K source on a CPU that is 20–40 minutes of decode per title. Across ~18,000 hours of
library it is months of wall clock, and it contends with everything else on the host.

`-skip_frame nokey` decodes **only keyframes** — typically 20–100× faster — but then frames land at
keyframe times, which are not 10 s apart. Rounding them onto a uniform grid and pretending otherwise
is a lie the player would render as a preview that drifts from the pointer.

**Decision: two modes, `keyframe` is the default, and the manifest states which it is.**

```rust
enum Timeline {
    Uniform { interval_ms: u32 },
    Explicit { times_ms: Vec<u32> },   // strictly increasing, len == tile_count
}
```

- **`keyframe` (default)** — select the first keyframe at or after each `interval_ms` boundary and
  record its **exact** time. Fast, honest, and the resulting `Explicit` timeline is *at least* as
  accurate as the uniform one because every tile's time is a real frame time rather than a nominal
  one. Costs ~4 bytes per tile in the manifest (~2.9 KB for a 2 h film) — nothing.
- **`accurate`** — `fps=1/N`, a `Uniform` timeline, exact grid spacing, full decode. Available for
  an operator who wants it on a small subset; never the default.

**Where the exact times come from is the elegant part: MTRK-06's keyframe index already has them.**
Keyframe mode selects tile timestamps *from the index*, not by parsing ffmpeg `showinfo` output.
That is why MTRK-06 blocks MTRK-05's keyframe mode, and why the index is load-bearing for two
features rather than being a bolt-on for spec E.

`Explicit` lookup is a binary search; `Uniform` is a division. Both live in MTRK-03's pure module and
both are golden-tested. **The player never implements either** — it reads the resolved tile index
from one TypeScript function transcribed from the same math (MTRK-10), tested against the same
fixtures.

---

## 7. Config knobs introduced

| Key | Default | Item |
|---|---|---|
| `MAESTRO_TRICKPLAY_ENABLED` | `true` | MTRK-07 |
| `MAESTRO_TRICKPLAY_WORK_DIR` | — (**required**, no default; never a card-backed volume) | MTRK-04 |
| `MAESTRO_TRICKPLAY_INTERVAL_SECS` | `10` | MTRK-03 |
| `MAESTRO_TRICKPLAY_TILE_WIDTH` | `320` | MTRK-03 |
| `MAESTRO_TRICKPLAY_GRID_COLS` / `_ROWS` | `10` / `10` | MTRK-03 |
| `MAESTRO_TRICKPLAY_JPEG_QUALITY` | `5` | MTRK-05 |
| `MAESTRO_TRICKPLAY_DECODE_MODE` | `keyframe` (`keyframe` \| `accurate`) | MTRK-05 |
| `MAESTRO_TRICKPLAY_BUDGET_MB` | `32768` | MTRK-04 |
| `MAESTRO_TRICKPLAY_MIN_FREE_MB` | `5120` | MTRK-04 |
| `MAESTRO_TRICKPLAY_MAX_CONCURRENT_JOBS` | `1` | MTRK-07 |
| `MAESTRO_TRICKPLAY_RATE_TITLES_PER_HOUR` | `60` | MTRK-07 |
| `MAESTRO_TRICKPLAY_JOB_TIMEOUT_SECS` | `1800` | MTRK-07 |
| `MAESTRO_TRICKPLAY_NICE` | `15` | MTRK-05 |
| `MAESTRO_TRICKPLAY_MAX_ATTEMPTS` | `3` | MTRK-07 |
| `MAESTRO_KEYFRAME_INDEX_ENABLED` | `true` | MTRK-06 |
| `MAESTRO_KEYFRAME_INDEX_MAX_ENTRIES` | `262144` | MTRK-06 |
| `MAESTRO_FFPROBE_BIN` | falls back to `MUSE_PROBE_FFPROBE_BIN`, then `ffprobe` | MTRK-06 |
| `MUSE_PROBE_MAX_CHAPTERS` | `1000` | MTRK-01 |

None is secret-shaped; all resolve through the **shared** `src/config.rs` (epic §2 — one config
module, two binaries), never a scattered `std::env::var` (S7). Every Maestro key is `MAESTRO_`-
prefixed so a reader can tell at a glance which binary consumes it.

---

## 8. Pre-flight

- [ ] Register prefix `MTRK` — `plane_prefix_check` → `plane_prefix_register` →
      `plane_prefix_promote`. **It is not in epic §11's registered family** (`MSTR MPRB MBAK MDEC
      MDLV MTRX MGPU MPLY MACT`), so this is a real claim, not a confirmation.
- [ ] Confirm spec A is merged — specifically that MPRB-01's promotion has landed (`src/media/probe.rs`,
      `src/media/capability.rs`, `src/media/paths.rs` exist), MPRB-03 has extended `MediaProbe`, and
      MPRB-05's `MediaInfoDoc` is being written to `media_files.media_info`. MTRK-01 edits the
      promoted module, so starting it before MPRB-01 merges guarantees a conflict.
- [ ] Confirm spec D is merged (`MediaHandle`, `library::resolve`, the Maestro HTTP surface).
- [ ] Confirm `ffmpeg` **and** `ffprobe` on the intended Maestro host. Both are absent on the dev box
      (epic §11, verified 2026-07-31) — every test in this spec must pass without them; the live
      sweep needs them.
- [ ] **Choose the trickplay work-dir volume and record the choice**, per §2 item 3 and MTRX-02's
      rule. Not the host root filesystem, and explicitly **not** any card-backed LV. Record which
      volume and why, in MTRK-14.
- [ ] Confirm `MAESTRO_DATABASE_URL_RO`'s read-only role can `SELECT media_files.media_info` and the new
      `media_markers` table, and still cannot write either. If MTRK-02's table is added after the
      role was granted, the grant must be extended through the `pg_ddl` operator door — a missing
      grant here presents as an empty marker list, which looks exactly like "no markers yet."
- [ ] Migrations are **not** auto-applied (skill v4.6). MTRK-02's migration is applied to the live DB
      with/before the Muse image swap. Note `0109` is already taken (`0109_artwork_renditions.sql`) —
      use the next free number at authoring time, never a number quoted from a sibling spec.
- [ ] Baseline: `cargo test` green on Muse `main` via `compiler_build(module="muse", mode=test)`;
      record the count. `npm run build` + `npm run lint:adherence` clean in
      `Terminus/constellation-web`; record the warning count so MTRK-10/11/12 can prove they add none.

---

## 9. Item map

| Item | Delivers | Side | Blocked by |
|---|---|---|---|
| MTRK-01 | Chapters modelled (not counted) in the shared probe | Muse | — (spec A) |
| MTRK-02 | `media_markers` table, model, repo — the consumption contract | Muse | — |
| MTRK-03 | Pure trickplay geometry, timeline, manifest types, storage estimator | Maestro | — |
| MTRK-04 | Derived-artifact store: layout, provenance, budget, eviction | Maestro | MTRK-03 |
| MTRK-05 | ffmpeg tile extraction — pure argv + the one impure spawn layer | Maestro | MTRK-03, MTRK-06 |
| MTRK-06 | Keyframe index — extraction, compact format, pure lookup | Maestro | MTRK-04 |
| MTRK-07 | Background generation job — bounded, resumable, never blocking | Maestro | MTRK-04, MTRK-05 |
| MTRK-08 | Trickplay HTTP surface — manifest, sheets, keyframe query | Maestro | MTRK-04, MTRK-06 |
| MTRK-09 | Muse `GET /media/{id}/markers` — chapters + markers, composed | Muse | MTRK-01, MTRK-02 |
| MTRK-10 | constellation-web: client arm + hover-scrub previews | web | MTRK-08 |
| MTRK-11 | constellation-web: chapter list + chapter ticks on the scrub bar | web | MTRK-09 |
| MTRK-12 | constellation-web: skip-intro / skip-credits **rendering only** | web | MTRK-09 |
| MTRK-13 | Observability + `/trickplay/{id}/why` diagnostics | Maestro | MTRK-07, MTRK-08 |
| MTRK-14 | Operator: choose the volume, run the sweep, publish the measured cost | ops | MTRK-07, MTRK-13 |

**Phases.** I1 = 01, 02, 03 (all independent, all pure or schema-only — run in parallel).
I2 = 04, 06, then 05, then 07. I3 = 08, 09 (parallel). I4 = 10, 11, 12 (10 is independent of 11/12;
11 blocks 12 because 12 extends 11's marker rail). I5 = 13, 14.

**A future Muse spec, recorded so it is not silently forgotten:** *intro/credit **detection*** —
audio-fingerprint matching across a season, or a Chord vision/audio job — writing into MTRK-02's
`media_markers` with `source = 'analysis'`. It is a Muse spec, it has no dependency on Maestro, and
MTRK-02's table is designed to receive it without a migration. This spec deliberately ships the
table with only `source = 'container'` and `source = 'manual'` populated.

---

## 10. Items

### MTRK-01: Model chapters in the shared probe instead of counting them
- **Priority:** High
- **Labels:** muse, probe, chapters, maestro
- **Agent:** claude
- **Estimate:** 4h
- **Blocked by:** spec A MPRB-01 (the promotion to `src/media/probe.rs`) and MPRB-05 (`MediaInfoDoc`)
- **Description:** `MediaProbe` already asks ffprobe for `-show_chapters` and already parses the
  result — into a `usize`. Replace the count with the list, keep the count as a derived accessor so
  Foundry's existing `-map_chapters 0` verification is untouched, and let the chapters ride into
  `media_files.media_info` inside the `MediaInfoDoc` envelope that already carries the whole
  `MediaProbe`. **No new ffprobe invocation anywhere** (§1).

  This is a **Muse-side** item in `src/media/probe.rs`, per epic §2's ownership table: a chapter is a
  library fact, and probe execution and storage are Muse's whether or not playback exists. A
  reviewer who sees chapter parsing appear under `src/maestro/` should reject it on that basis.

  **Naming, per spec A's explicit decision:** the probe result type is **`MediaProbe`**, not
  `MediaInfo`. Spec A weighed the epic §2b rename against a mechanical diff across `plan.rs` (1,435
  lines), `forge.rs` (2,766) and `policy.rs` (483) and kept `MediaProbe`, with **`MediaInfoDoc`** as
  the separate *persisted envelope* (`{schema_version, probe: MediaProbe, flat keys}`). This item
  adds one field to `MediaProbe` and renames nothing.

  ## FILES
  - `src/media/probe.rs` — `Chapter`; `MediaProbe.chapters: Vec<Chapter>`; `chapter_count()`;
    a typed `RawChapter` replacing the `Vec<serde_json::Value>` at line 319; the parse in
    `parse_probe_json`
  - `src/foundry/forge.rs` — retarget the `-map_chapters 0` verification at `chapter_count()`
  - `tests/golden/probe/` — one fixture with real chapters, one with none (spec A MPRB-04's corpus)
  - `README.md` — document `MUSE_PROBE_MAX_CHAPTERS`

  ## APPROACH
  1. `Chapter { index: u32, start_ms: i64, end_ms: Option<i64>, title: Option<String> }`, deriving
     `Serialize + Deserialize + Clone + Debug + PartialEq`, `#[serde(default)]` on the optionals.
     Times in **milliseconds**, not ffprobe's float seconds: the whole downstream chain (session
     `position_ms`, marker times, the player's `currentTime * 1000`) is integer milliseconds, and a
     float that survives to the GUI is a rounding bug waiting for a chapter boundary to land on it.
  2. ffprobe emits `start_time`/`end_time` as **string-encoded floats in seconds**, plus integer
     `start`/`end` in the stream's own `time_base`. Parse the *string seconds* through the module's
     existing lenient numeric handling — the same path that already turns `"N/A"`, string-vs-number,
     negative and NaN into `None` rather than `0` (`probe.rs:396`) — and convert. The `time_base`
     form needs a per-chapter rational nothing else in the parser carries, and the seconds form is
     what every muxer emits. A chapter whose start does not parse is **dropped and counted**, never
     defaulted to zero: a chapter at 0 that is not really at 0 sends the player to the wrong frame,
     whereas a missing chapter is visibly missing.
  3. **Dropped and truncated chapters are surfaced as a count, following the module's own idiom.**
     `MediaProbe` has no warnings channel and this item must not invent one — but it already has the
     exact convention needed: `data_stream_count` and `unindexed_stream_count` exist precisely so a
     thing the parser could not carry is *surfaced rather than silently vanished* (`probe.rs:85-100`).
     Add `chapters_dropped: u32` in the same spirit and document it the same way. If spec A's MPRB-03
     has meanwhile introduced a general warnings channel, use that instead and drop the field — check
     before adding, and say which you found in the PR description.
  4. `title` from `tags.title` via the module's existing case-insensitive tag lookup (the same one
     that already reads an uppercase `LANGUAGE`, `probe.rs:470`) — trimmed, truncated to spec A's
     `MAX_TAG_LEN` (512), empty ⇒ `None`. **Never synthesise `"Chapter 3"`**: an untitled chapter is
     a fact, and the *player* is the right place to render a positional fallback (MTRK-11), because
     only the player knows the display language and the ordinal it is showing.
  5. **Cap at `MUSE_PROBE_MAX_CHAPTERS` (default 1000)** — the structural cap, parallel to spec A's
     `MAX_STREAMS` (512), applied *after* deserialisation. This does not replace spec A's
     `MUSE_PROBE_MAX_OUTPUT_BYTES`, which already bounds the same risk at the process boundary and
     whose stated motivation is verbatim "a file with 100k chapters must not OOM a 2–4 GB
     container". The output cap stops the read; this cap stops 100k `Chapter` structs and 100k
     `String` titles from being materialised out of an 8 MiB document that passed it. Over the cap ⇒
     keep the first N, record the true count in `chapters_dropped`, and do **not** fail the parse —
     a pathological chapter list is not a reason to lose the file's codec information.
  6. `chapter_count()` is `self.chapters.len()`. Foundry's forge verification changes from reading a
     field to calling this; nothing else about it moves. Keep its existing tests green unmodified —
     if a Foundry test needs editing, the refactor went too far. **Carry §1's reasoning into the doc
     comment**: the count exists to make the `-map_chapters 0` promise checkable, that consumer is
     still correct and still served, and the list is an addition for a second consumer — not a
     repair of an oversight.
  7. **Additive, no schema bump.** `chapters` is a new `#[serde(default)]` field on `MediaProbe`,
     which `MediaInfoDoc` embeds whole, so a v1 document written before this item still deserialises
     and `MEDIA_INFO_SCHEMA_VERSION` stays at 1. No migration. Rows written before this lands carry
     an empty list and are re-probed by the standing backfill worker (MPRB-06's steady-state
     posture) — which is why chapters need no backfill of their own.
     **The flat compatibility projection is not touched.** MPRB-05's flat keys exist to light up
     `MediaDetailPanel.tsx`'s existing dead pixels; chapters are not one of them and adding a flat
     chapter key would widen a contract that exists for backward compatibility only.
  8. Sort by `start_ms` ascending and de-duplicate exact `(start_ms, end_ms)` repeats — some muxers
     emit chapters out of order, and every consumer downstream assumes monotonic.

  ## TEST PLAN
  - `cargo test` via `compiler_build(module="muse", mode=test)`
  - Golden fixture with 3 real chapters: times, order, and titles all assert exactly
  - Golden fixture with none: empty list, `chapter_count() == 0`, `chapters_dropped == 0`
  - A chapter with `start_time: "N/A"` is dropped and counted; the surrounding chapters survive
  - A chapter with no `tags.title` yields `title: None` — assert it is **not** `"Chapter 1"`
  - Out-of-order chapters are sorted; exact duplicates collapse
  - A synthesised 5,000-chapter document keeps 1,000 and records the true count in
    `chapters_dropped` (negative test)
  - A pre-existing v1 `MediaInfoDoc` with no `chapters` key deserialises to an empty list, and
    `MEDIA_INFO_SCHEMA_VERSION` is unchanged
  - Foundry's existing `-map_chapters` verification tests pass **unmodified**
  - The suite passes on a host with no `ffprobe`
  - Verify no hardcoded infrastructure values in new/modified files

  ## EDGE CASES
  - `end_time` absent (common in MP4) — `end_ms: None`; the player derives the boundary from the
    next chapter's start, and the last chapter's from `duration_ms`
  - `end_ms < start_ms` (a genuinely malformed file) — keep `start_ms`, set `end_ms: None`, warn
  - A chapter starting beyond `duration_ms` — kept as-is and warned; clamping it would invent a
    boundary the file does not have, and MTRK-11 already renders out-of-range ticks as absent
  - Title containing what looks like PII (a person's name in a documentary chapter) — it is media
    metadata, kept verbatim; only fixture *paths* are scrubbed (spec A MPRB-04's rule)
  - Non-UTF-8 in a title — serde rejects the document as `MalformedJson`, which is correct and
    must not panic

- **Acceptance criteria:**
  - [ ] `MediaProbe.chapters` carries index, start, end and title; `chapter_count()` derives from it,
        and no type is renamed (spec A keeps `MediaProbe`; `MediaInfoDoc` is the persisted envelope)
  - [ ] No new ffprobe invocation is introduced — chapters ride the existing single call
  - [ ] Times are integer milliseconds; an unparseable start drops that chapter and records it in
        `chapters_dropped` rather than defaulting to zero (negative test)
  - [ ] An untitled chapter yields `None`, never a synthesised label
  - [ ] Over `MUSE_PROBE_MAX_CHAPTERS`, the parse truncates and records the true count instead of
        failing or OOMing
  - [ ] A pre-existing v1 document with no `chapters` key still deserialises (no schema bump)
  - [ ] Foundry's `-map_chapters 0` verification still passes with its tests unmodified
  - [ ] README documents `MUSE_PROBE_MAX_CHAPTERS`
  - [ ] No hardcoded infrastructure values in new/modified code; all existing tests still pass

---

### MTRK-02: `media_markers` — the marker consumption contract, with no detection
- **Priority:** High
- **Labels:** muse, markers, db, migration
- **Agent:** claude
- **Estimate:** 5h
- **Description:** A Muse-owned table of time-ranged markers on a media item, plus its model and
  repo. **This item produces no markers automatically.** It ships the schema, the typed model, the
  repo, an operator-facing write path, and nothing else. Detection — the analysis that would
  populate `source = 'analysis'` — is a separate future Muse spec (§9), and epic §4 places it
  firmly in Muse, never in Maestro.

  Building the contract before the detector is the right order, and it is not busywork. It lets
  MTRK-09 and MTRK-12 ship a complete, testable skip-intro *experience* against manually-entered
  markers on a handful of shows, which is the only way to know the rendering is right before
  committing a sprint to detection. It also fixes the shape detection must produce, rather than
  letting a detector invent one and the player bend to it.

  ## FILES
  - `migrations/{next}_media_markers.sql` — new
  - `src/models/media_marker.rs` — `MediaMarker`, `NewMediaMarker`, `MarkerKind`, `MarkerSource`
  - `src/repo/media_marker.rs` — `list_for_item`, `list_for_episode`, `upsert`, `delete`
  - `src/models/mod.rs`, `src/repo/mod.rs` — registration
  - `README.md` — document the table and that it is not auto-populated yet

  ## APPROACH
  1. Schema:
     ```sql
     CREATE TABLE IF NOT EXISTS media_markers (
         id              bigserial PRIMARY KEY,
         media_item_id   bigint REFERENCES media_items(id) ON DELETE CASCADE,
         episode_id      bigint REFERENCES episodes(id)    ON DELETE CASCADE,
         kind            text        NOT NULL,
         start_ms        bigint      NOT NULL,
         end_ms          bigint      NOT NULL,
         source          text        NOT NULL,
         confidence      real,
         created_at      timestamptz NOT NULL DEFAULT now(),
         updated_at      timestamptz NOT NULL DEFAULT now(),
         CHECK (end_ms > start_ms),
         CHECK (start_ms >= 0),
         CHECK ((media_item_id IS NULL) <> (episode_id IS NULL)),
         CHECK (kind   IN ('intro','credits','recap','preview','ad')),
         CHECK (source IN ('container','analysis','manual','provider'))
     );
     CREATE UNIQUE INDEX IF NOT EXISTS media_markers_unique
         ON media_markers (COALESCE(media_item_id, -1), COALESCE(episode_id, -1), kind, source);
     ```
     Mirroring `play_sessions`' existing item/episode divergence exactly (spec D MDLV-01 step 1)
     rather than inventing a flat model — the library is not flat and pretending otherwise here
     would need a join everywhere else.
  2. **`kind` and `source` are CHECK-constrained text, not Postgres enums** — the same reasoning
     spec A MPRB-05 gives for `probe_state`. Both will plausibly gain a value (`sponsor`,
     `post_credits`), and adding one to a checked text column is a migration whose deploy can be
     ordered independently of the code that emits it.
  3. **`source` is in the unique key on purpose.** A manually-entered intro and a detector-produced
     intro can coexist on the same episode, and MTRK-09 resolves the conflict by *precedence*
     (`manual` > `provider` > `analysis` > `container`) rather than by making one overwrite the
     other. A detector that silently clobbers an operator's correction is a detector nobody trusts
     twice.
  4. `confidence` is `Option<f32>` in `[0,1]`, `NULL` for `manual` and `container`. Detection will
     want it; the endpoint (MTRK-09) exposes it; **the player never renders it as a number** — a
     "87% confident" skip button is noise. A future threshold on it is a server-side decision.
  5. Repo functions take `&PgPool`, return `MuseResult<_>`. `upsert` is `ON CONFLICT … DO UPDATE`
     on the unique index, touching `updated_at`.
  6. The write path is the existing operator surface (`src/http/ops.rs`) behind the existing bearer,
     mirroring how other operator-only Muse routes are exposed. `POST /ops/markers` and
     `DELETE /ops/markers/{id}` — enough to enter a marker by hand for MTRK-12's verification, not a
     marker-editing UI. **Maestro never calls these**, and its read-only role could not.
  7. Doc comment on the model, verbatim intent: *"Markers are content analysis output. Muse produces
     them; Maestro and the player consume and render them. Nothing under `src/maestro/` may write
     this table, and nothing anywhere may derive a marker from pixels or audio — that is a separate
     spec (epic §4)."*

  ## TEST PLAN
  - `cargo test` — model serde round-trip for `MarkerKind` and `MarkerSource`
  - DB-gated tests on the existing `MUSE_TEST_DATABASE_URL` idiom, skipping cleanly when unset
  - `upsert` twice with the same `(item, kind, source)` updates one row rather than inserting two
  - `upsert` with the same `(item, kind)` and a **different** `source` yields two rows
  - CHECK rejects `end_ms <= start_ms`, a negative `start_ms`, both-null and both-set refs, an
    unknown `kind` and an unknown `source` (negative tests)
  - Deleting the parent item cascades the markers away
  - Grep test: no `INSERT`/`UPDATE`/`DELETE` against `media_markers` anywhere under `src/maestro/`
  - Migration is idempotent on a re-run
  - Verify no hardcoded infrastructure values in new/modified files

  ## EDGE CASES
  - A marker longer than the item's runtime (a bad manual entry) — stored as given; MTRK-09 clamps
    at read time against the probed duration and says it clamped. Rejecting at write time would
    make the table unwritable before the item has been probed
  - Two overlapping markers of different kinds (an intro overlapping a recap) — legal and real;
    MTRK-09 returns both and MTRK-12 renders the earliest-ending applicable one
  - An item with no probed duration — markers are still storable; the endpoint reports them with a
    `duration_unknown` flag rather than dropping them
  - Cascade on episode delete during a live session — the session is unaffected; markers are
    advisory rendering, never a playback dependency

- **Acceptance criteria:**
  - [ ] The migration is additive and idempotent, with CHECK constraints on kind, source, ordering
        and the item/episode XOR — each proven by a rejection test
  - [ ] `source` participates in the unique key so a manual marker and an analysis marker coexist
  - [ ] `upsert` is idempotent on `(item, kind, source)`
  - [ ] **This item contains no detection logic of any kind** — no pixel, audio, or cross-episode
        comparison appears in the diff, and the model doc comment says why (epic §4)
  - [ ] Nothing under `src/maestro/` writes the table, enforced by a grep test
  - [ ] README documents the table and states it is not auto-populated yet
  - [ ] No hardcoded infrastructure values in new/modified code; all existing tests still pass

---

### MTRK-03: Pure trickplay geometry, timeline, manifest types and the storage estimator
- **Priority:** Critical
- **Labels:** maestro, trickplay, pure
- **Agent:** codex
- **Estimate:** 5h
- **Description:** All of the arithmetic, none of the I/O. Epic §7.3's purity requirement applies
  with full force: a scrub preview that shows the wrong frame will *present* as a tile-generation
  bug and will almost always *be* an index-arithmetic bug, so the arithmetic is established first,
  separately, and golden-tested before anything writes a byte.

  ## FILES
  - `src/maestro/trickplay/mod.rs` — new module; re-exports and module docs
  - `src/maestro/trickplay/geometry.rs` — `TrickplayParams`, `Timeline`, tile/sheet/cell math
  - `src/maestro/trickplay/manifest.rs` — `TrickplayManifest`, `SheetEntry`, `TrickplayState`
  - `src/maestro/trickplay/estimate.rs` — the §2 storage estimator
  - `src/config.rs` — the `MAESTRO_TRICKPLAY_INTERVAL_SECS` / `_TILE_WIDTH` / `_GRID_COLS` / `_ROWS`
    knobs and `TRICKPLAY_PARAM_VERSION`
  - `src/bin/maestro/main.rs` — register the module

  ## APPROACH
  1. `TrickplayParams { interval_ms: u32, tile_width: u32, grid_cols: u32, grid_rows: u32,
     jpeg_quality: u8 }`, built once from `Config`, validated at construction: every field non-zero,
     `interval_ms >= 1000`, `tile_width` in `[64, 1920]`, `grid_cols * grid_rows` in `[1, 400]`.
     Invalid config is a **startup error**, not a lazy failure — a bad grid discovered on the first
     scrub is the worst version of this bug.
  2. `tile_height` is **derived**, never configured: `round_even(tile_width / display_aspect)` from
     the probed primary video's dimensions, falling back to 16:9 when unknown. Even, because JPEG
     chroma subsampling wants even dimensions and ffmpeg's `scale=W:-2` produces them. A separately
     configured height is how a library ends up with letterboxed tiles for its 2.39:1 films.
  3. `Timeline` as §6 defines it, with three total functions:
     - `tile_index_at(&self, position_ms: i64) -> Option<u32>` — `Uniform` divides and clamps;
       `Explicit` binary-searches for the last time `<= position_ms`. Before the first tile ⇒
       `Some(0)`; past the last ⇒ the last index. `None` **only** for an empty timeline.
     - `tile_time_ms(&self, tile: u32) -> Option<i64>`
     - `tile_count(&self) -> u32`
     `Explicit` construction **validates strict monotonicity** and returns an error otherwise — a
     non-monotonic timeline would make the binary search return an arbitrary neighbour, which is a
     preview that jitters backwards as you drag forwards, and is very hard to diagnose from the
     symptom.
  4. `sheet_and_cell(tile: u32, params) -> (sheet_index: u32, col: u32, row: u32)` and
     `cell_rect(col, row, params) -> (x, y, w, h)` — the CSS `background-position` the browser needs.
     Both trivially reversible; assert the round-trip.
  5. `TrickplayManifest` per §6, `Serialize + Deserialize`. `TrickplayState` is
     `Ready | Partial { tiles_done, tiles_total } | Absent | Queued | Failed { reason } |
     Unavailable { reason }`.
     - `Absent` = never generated. `Queued` = the job knows about it.
     - `Partial` is a first-class state because generation is resumable (MTRK-07) and a half-done
       artifact is genuinely useful — previews work for the first hour of a film while the second is
       still rendering. Collapsing it into `Absent` throws that away.
     - `Unavailable` = generation cannot be attempted (disabled, no ffmpeg, unresolvable file) and is
       distinct from `Failed` (tried, broke). The player renders them differently and an operator
       needs to tell them apart.
  6. **`estimate_bytes(duration_ms, params) -> StorageEstimate`** — the §2 table as executable code,
     returning `{ tiles, sheets, bytes_low, bytes_expected, bytes_high }` from a documented
     bytes-per-megapixel constant with its measurement provenance in a comment. It is used by
     MTRK-07 for pre-admission budget checks and by MTRK-13's diagnostics, and MTRK-14 replaces the
     constant with the measured value. A golden test pins the §2 table's headline figures so that
     table and this function cannot drift apart silently.
  7. `TRICKPLAY_PARAM_VERSION: u16` — a crate constant, bumped whenever a default geometry knob
     changes, folded into §4's fingerprint. Document the obligation next to the constant, because
     forgetting to bump it is the most predictable failure in this spec.

  ## TEST PLAN
  - `cargo test maestro::trickplay` — no filesystem, no clock, no process, no database
  - `tile_index_at` for a uniform 10 s timeline at `-1`, `0`, `9_999`, `10_000`, `10_001`,
    `duration`, `duration + 1e9` — clamps at both ends, never panics, never returns out of range
  - `tile_index_at` on an explicit timeline returns the tile at or before the position, including
    exact-boundary hits
  - A non-monotonic explicit timeline is **rejected at construction** (negative test)
  - An empty timeline returns `None` from every accessor and never divides by zero (negative test)
  - `sheet_and_cell` ∘ inverse round-trips for tiles `0..10_000` across three grid shapes
  - `cell_rect` never exceeds the sheet bounds for the last cell of a partial final sheet
  - `TrickplayParams` rejects zero interval, zero grid, a 4000 px tile, a 500-cell grid
  - `tile_height` derivation is even for a 2.39:1, a 4:3 and a 16:9 source
  - `estimate_bytes` reproduces §2's headline figures (golden), and returns zero — not a panic — for
    a zero/absent duration
  - `TrickplayManifest` round-trips through serde including every `TrickplayState` variant
  - Verify no hardcoded infrastructure values in new/modified files

  ## EDGE CASES
  - `duration_ms` shorter than one interval — exactly one tile, one sheet, one cell
  - `duration_ms` absent (spec A's `Suspicious` state) — no artifact is planned at all; the caller
    gets `Unavailable { reason: "duration_unknown" }` rather than a one-tile guess
  - A tile count that overflows one sheet grid by exactly one — the final sheet holds one cell and
    `cell_rect` stays in bounds
  - `interval_ms` larger than the whole runtime — clamps to a single tile
  - An anamorphic source (`sample_aspect_ratio` ≠ 1:1) — derive from the **display** aspect, not the
    coded one, or every tile of a DVD rip is horizontally squashed

- **Acceptance criteria:**
  - [ ] Every function in `geometry.rs`, `manifest.rs` and `estimate.rs` is pure — no I/O, no clock,
        no `PgPool`, no `std::env` (enforced by a grep test, the same mechanism spec A MPRB-05
        uses to keep the jsonb behind one reader)
  - [ ] `tile_index_at` is total and clamping across the whole `i64` range for both timeline kinds
  - [ ] A non-monotonic explicit timeline is rejected at construction (negative test)
  - [ ] `sheet_and_cell` round-trips and `cell_rect` never exceeds sheet bounds on a partial sheet
  - [ ] `TrickplayState` distinguishes Absent / Queued / Partial / Ready / Failed / Unavailable
  - [ ] `estimate_bytes` reproduces §2's published figures under a golden test
  - [ ] Invalid `TrickplayParams` fails at startup, not on first use
  - [ ] No hardcoded infrastructure values in new/modified code; all existing tests still pass

---

### MTRK-04: The derived-artifact store — layout, provenance, budget, eviction
- **Priority:** Critical
- **Labels:** maestro, trickplay, disk, ops
- **Agent:** codex
- **Estimate:** 6h
- **Blocked by:** MTRK-03
- **Description:** The on-disk substrate for both trickplay sheets and keyframe indices: a work root,
  one directory per `(media_file_id, source_fingerprint)`, a manifest sidecar, and the budget
  arithmetic that keeps it bounded. §3 explains why this is a *different* store from spec E's
  segment scratch and what the two nonetheless share.

  **The disk budget is built into the first disk-touching item, not retrofitted.** The fleet lost a
  card-backed PV in July 2026 and ran half-missing and read-only for three days, with the symptom
  presenting as bogus compiler gates and `EIO` from `systemctl` rather than as a disk fault. An
  unbounded background writer is what turns that from a storage failure into an unattributable one.

  ## FILES
  - `src/maestro/store/mod.rs` — new; the shared store module
  - `src/maestro/store/budget.rs` — `BudgetVerdict`, `budget_verdict`, `filesystem_free_bytes`
    (**shared with spec E's MTRX-02 — see the acceptance criteria**)
  - `src/maestro/trickplay/store.rs` — layout, fingerprint, manifest read/write, eviction
  - `src/config.rs` — `MAESTRO_TRICKPLAY_WORK_DIR`, `_BUDGET_MB`, `_MIN_FREE_MB`
  - `README.md` — the work dir, its budget, and the card-backed prohibition

  ## APPROACH
  1. Layout, one directory per artifact generation:
     ```
     {work_dir}/trickplay/{media_file_id}/{fingerprint_prefix}/
         manifest.json
         sheet00000.jpg …
         keyframes.idx          (MTRK-06)
         .partial               (present while a job is mid-flight)
     ```
     `fingerprint_prefix` is the first 16 hex chars of §4's `source_fingerprint`. Nesting the
     generation **under** the file id means a regeneration writes a sibling directory and the old one
     is removed only after the new manifest lands — so a regeneration never leaves the artifact
     unserviceable, and a crash mid-regeneration leaves the old, valid generation in place.
  2. **Every path component is derived, never supplied.** `media_file_id` is an `i64` formatted by
     us; `fingerprint_prefix` is validated as 16 lowercase hex chars before any join; the sheet name
     is `format!("sheet{:05}.jpg", n)` with a fail-closed `parse_sheet_filename` that returns `None`
     for anything not matching `sheet\d{5,}\.jpg` exactly. No caller-supplied string reaches a path
     component anywhere in this module — the same posture spec D MDLV-02 takes for media paths and
     MTRX-02 for segments.
  3. `source_fingerprint(media_file_id, size_bytes, mtime, media_info_version) -> String` exactly as
     §4 defines, with §4's divergence-from-`artwork_cache` rationale in the doc comment. Pure and
     unit-tested; hashing is over a **length-prefixed** concatenation so that `(12, 345)` and
     `(123, 45)` cannot collide.
  4. Budget arithmetic in `store/budget.rs`, pure:
     `budget_verdict(total_used, unit_used, free_bytes, limits) -> Ok | ReapNeeded | Refuse`.
     `Refuse` on `free_bytes < MIN_FREE_MB` **regardless of the other numbers**, and `Refuse` when
     `filesystem_free_bytes` cannot be read at all — absence of a reading is never read as "plenty
     free". That fail-closed default is the specific lesson from `dsn_guard_fail_closed_lesson`.
  5. **Eviction is least-recently-served, whole-artifact, and never partial.** Maintain an
     `.atime`-style touch file updated (at most once per hour, to avoid a write per sheet GET) when
     an artifact is served. Over budget ⇒ delete whole generations oldest-first until under. Never
     delete individual sheets from a live artifact: a manifest that promises 37 sheets and has 12 is
     a broken preview, whereas a wholly absent artifact regenerates cleanly and the player renders
     the honest `Absent` state.
     **Never evict an artifact for a `media_file_id` with an active playback session.** That is the
     same asymmetry MTRX-08 step 4 establishes for segments, applied here: degrading the film someone
     is watching to make room for a film nobody is watching is always the wrong trade.
  6. Startup validation: `MAESTRO_TRICKPLAY_WORK_DIR` has **no default** — unset is a startup error,
     never a silent `/tmp`. Verify it exists, is a directory, and is writable, at startup. Log the
     configured budget and the current usage once at startup so an operator sees the state without
     asking.
  7. README states the work dir must not sit on a removable-card-backed volume, should not be the
     host root filesystem, and — if placed on tmpfs — needs a tighter budget because the pressure is
     RAM. Document; do not special-case in code.
  8. Orphan sweep at startup: remove any generation directory whose `manifest.json` is missing or
     unparseable, and any `.partial` directory with no live job. Best-effort — a cleanup failure logs
     and continues, never propagates into a playback path.

  ## TEST PLAN
  - `cargo test maestro::store` and `maestro::trickplay::store` — pure tests plus `tempfile` tests
  - `parse_sheet_filename` fail-closed: `sheet1.jpg`, `../etc/passwd`, `sheet00001.jpg.tmp`,
    `SHEET00001.JPG`, `sheet00001` all return `None` (negative test)
  - `format` ∘ `parse` round-trips for `{0, 1, 99_999, 100_000}` — the width must not wrap at 5
  - Fingerprint: length-prefixing means `(12, 345)` and `(123, 45)` differ; a changed size, a changed
    mtime, and a bumped `TRICKPLAY_PARAM_VERSION` each change it
  - `budget_verdict` returns `Refuse` on low free space regardless of the other inputs, and `Refuse`
    on an unreadable free-space value (negative tests)
  - Eviction over budget removes whole generations oldest-first and **never** an artifact with an
    active session (negative test — the most important one here)
  - Regeneration writes a sibling generation and removes the old one only after the new manifest
    lands; a simulated crash mid-write leaves the old generation intact and serviceable
  - Unset / absent / unwritable work dir fails at **startup** with a message naming the variable
  - Startup orphan sweep removes a manifest-less generation dir and a stale `.partial`
  - Verify no hardcoded infrastructure values in new/modified files

  ## EDGE CASES
  - Work dir on the same filesystem as spec E's scratch — legal; the two budgets are independent and
    both then see the same `free_bytes`, so both refuse together under real pressure. Correct.
  - Two jobs racing on the same `(file, fingerprint)` — MTRK-07's single-flight prevents it; the
    store additionally refuses to create an existing generation dir rather than writing into it
  - A `media_file_id` whose artifact exists under a *different* fingerprint — both are on disk
    briefly; the sweep removes the non-current one once the new manifest lands
  - `statvfs` failing on an unusual filesystem — fail closed (see step 4)
  - Manifest present but truncated (a crash between `open` and `write`) — write the manifest to a
    temp file and `rename` it into place; `rename` within a directory is atomic on every filesystem
    this fleet uses

- **Acceptance criteria:**
  - [ ] `MAESTRO_TRICKPLAY_WORK_DIR` has no default; unset/absent/unwritable fails at startup
  - [ ] No caller-supplied string reaches a path component; `parse_sheet_filename` is fail-closed
        (negative test)
  - [ ] `source_fingerprint` is length-prefixed and changes on size, mtime, probe version and
        `TRICKPLAY_PARAM_VERSION`
  - [ ] `budget_verdict` fails closed on low free space **and** on an unreadable free-space value
  - [ ] Eviction is whole-artifact, oldest-first, and never touches an artifact with an active
        session (negative test)
  - [ ] A regeneration or a crash mid-write never leaves the previous generation unserviceable
  - [ ] `store/budget.rs` is written as the **shared** primitive: either it adopts spec E's
        `MTRX-02` implementation if that merged first, or it is written so `MTRX-02` can adopt it —
        one implementation of `budget_verdict`/`filesystem_free_bytes` in the tree, not two (§3)
  - [ ] README states the work dir must not be on a removable-card-backed volume
  - [ ] No hardcoded infrastructure values in new/modified code; all existing tests still pass

---

### MTRK-05: Tile extraction — the pure argv builder and the one impure spawn layer
- **Priority:** Critical
- **Labels:** maestro, trickplay, ffmpeg
- **Agent:** claude
- **Estimate:** 6h
- **Blocked by:** MTRK-03, MTRK-06
- **Description:** Turn one media file into sheet JPEGs. Follows the crate's established split
  exactly — `src/streaming/ffmpeg.rs` is the pure argv builder unit-tested on a host with no ffmpeg,
  and one impure module spawns and reads. `build_args` (MUSE-29) and `build_still_args` (MUSEL-C1)
  already live there; `build_trickplay_args` joins them, in the same file, for the same reason.

  **Reuse note, and the one deliberate divergence.** `src/matching/stills.rs` already extracts
  frames with ffmpeg and this spec does **not** write a second frame extractor — the argv
  conventions (`-hide_banner -loglevel error`, input-side `-ss`, `classify_spawn_error`,
  `kill_on_drop`, `MuseError::NotImplemented` for a missing binary) are lifted from it verbatim, and
  the impure layer here is deliberately shaped like `capture_still`. But `extract_sample_stills`
  spawns **one ffmpeg per timestamp**, which is exactly right for its five-sample match verification
  and catastrophic here: a 2 h film needs ~723 tiles, and 723 process spawns each seeking into a
  network-mounted file would take longer than decoding the film. So trickplay uses **one ffmpeg
  invocation producing many sheets through a filter graph**. The primitive is shared; the invocation
  strategy legitimately differs, and the doc comment says so with this reasoning so nobody
  "unifies" them later.

  ## FILES
  - `src/streaming/ffmpeg.rs` — `build_trickplay_args`, `build_trickplay_keyframe_args` (pure)
  - `src/maestro/trickplay/extract.rs` — the impure spawn/supervise layer
  - `src/config.rs` — `MAESTRO_TRICKPLAY_JPEG_QUALITY`, `_DECODE_MODE`, `_NICE`
  - `README.md` — document the decode modes and their cost

  ## APPROACH
  1. **`accurate` mode** (uniform timeline), pure argv:
     ```
     -hide_banner -loglevel error -y
     -i <path>
     -map 0:v:<primary_index> -an -sn -dn
     -vf fps=1/<interval_secs>,scale=<w>:<h>:flags=bilinear,tile=<cols>x<rows>
     -q:v <quality> -f image2 <dir>/sheet%05d.jpg
     ```
  2. **`keyframe` mode (default)**, and the reason MTRK-06 blocks this item: rather than parsing
     ffmpeg `showinfo` output to learn what times the frames landed on, **select the timestamps from
     MTRK-06's keyframe index** — take the first keyframe at or after each interval boundary — and
     hand ffmpeg an explicit selection:
     ```
     -hide_banner -loglevel error -y
     -skip_frame nokey
     -i <path>
     -map 0:v:<primary_index> -an -sn -dn
     -vf select='<expr>',scale=…,tile=<cols>x<rows>
     -vsync 0 -q:v <quality> -f image2 <dir>/sheet%05d.jpg
     ```
     The selected times become the `Explicit` timeline in the manifest. This is the whole reason the
     timeline is an enum: the manifest records the frames we actually got, not the frames we asked
     for. `-skip_frame nokey` makes decode 20–100× cheaper, which is the difference between a
     library-wide sweep taking days and taking months.
     **The `select` expression is built from validated integers only** — never string-formatted from
     anything a caller supplied — and is capped in length; an expression longer than a documented
     bound falls back to `accurate` mode with a warning rather than handing ffmpeg an
     arbitrarily-long argument.
  3. `-an -sn -dn` and an explicit `-map 0:v:<index>` where the index is the probed **primary video
     stream** from the shipped `MediaProbe::primary_video()` (`probe.rs:177`). Not `0:v:0`: a file with embedded
     cover art carries the poster as a video stream, and `0:v:0` can select the artwork — spec A's
     `attached_pic` handling exists precisely to prevent that, and this is one of the consumers it
     was built for. A whole library of 600×900 poster tiles is a very funny bug to ship.
  4. Spawn with `tokio::process::Command`, `stdin(null)`, `stderr(piped)`, `kill_on_drop(true)`,
     under `MAESTRO_TRICKPLAY_JOB_TIMEOUT_SECS`. On timeout: `start_kill()`, then `wait()` to reap,
     then a typed error. A timeout that leaks a zombie is not a timeout (spec A MPRB-02, which adds
     exactly this bounding to the probe's own invocation).
  5. **Nice the child** to `MAESTRO_TRICKPLAY_NICE` (default 15) via `pre_exec`. Trickplay is
     never urgent and always contends: playback, Foundry, and (on <host>) Chord and MINT all matter
     more. A background job that competes at normal priority with a live transcode is a background
     job that causes a stutter someone will report as a playback bug.
  6. Classify spawn failure with the existing `ffmpeg::classify_spawn_error` — `BinaryMissing` ⇒
     the whole capability reports `Unavailable` once and stops trying (matching `stills.rs`'s posture
     of surfacing a deployment gap once rather than N times); `SpawnError` ⇒ retryable.
  7. **No hardware acceleration.** Not `-hwaccel`, not a GPU decoder, not a VAAPI path. GPU is spec F
     and is arbitrated by Chord; an unannounced GPU decode during a MINT sweep presents as "Chord is
     slow", which epic §10.5 names as an expensive thing to misdiagnose. Note the follow-up; do not
     take it here.
  8. ffmpeg subprocess only — no `ffmpeg-next`, no libav bindings, ever (epic §7.1). This is what
     keeps Maestro musl-publishable and sidesteps the LGPL/GPL question entirely.

  ## TEST PLAN
  - `cargo test` — the argv builders are pure, so the exact argv is asserted **on a host with no
    ffmpeg**, the same posture `build_still_args` and `build_ffprobe_args` already take
  - `accurate` argv contains `fps=1/10`, the derived scale, the configured tile grid, and `-q:v`
  - `keyframe` argv contains `-skip_frame nokey` and `-vsync 0`, and its `select` expression matches
    the timestamps taken from a fixture keyframe index
  - The map argument targets the **primary** video index for a fixture with cover art at `v:0`
    (negative test — assert it is not `0:v:0`)
  - An over-long `select` expression falls back to `accurate` and warns (negative test)
  - Impure layer, via a stub script written to a temp dir at test time (the `stills.rs` pattern):
    a stub that outlives the timeout yields a typed timeout error and leaves no zombie
  - A nonexistent binary yields `BinaryMissing` and the capability reports `Unavailable` once
  - A stub exiting non-zero with stderr yields a typed failure carrying a truncated excerpt
  - The whole suite passes on a host with no ffmpeg installed
  - Verify no hardcoded infrastructure values in new/modified files

  ## EDGE CASES
  - A source with no video stream at all (audio-only) — no artifact is planned; `Unavailable
    { reason: "no_video_stream" }`, never an empty sheet
  - A source whose only video stream is `attached_pic` — same; cover art is not a film
  - A file replaced by Foundry mid-job — the open fd keeps the old inode alive so the job completes
    coherently, and MTRK-07 discards the output on the post-job fingerprint recheck (§4)
  - A source on a stalled network mount — the job timeout is the only defence; do **not** add a
    blocking pre-check, which would itself hang (spec A MPRB-02's edge case, same mount)
  - Extremely long runtime (a 12 h concert recording) — the sheet index exceeds 5 digits; `%05d`
    widens naturally and MTRK-04's round-trip test covers it
  - VFR source where keyframes cluster — keyframe mode may select fewer tiles than intervals; that
    is honest and the `Explicit` timeline records it. Never pad with duplicates

- **Acceptance criteria:**
  - [ ] `build_trickplay_args` is pure and its exact argv is asserted on a host with no ffmpeg
  - [ ] One ffmpeg invocation produces many sheets; there is no per-tile spawn (the `stills.rs`
        divergence is documented in the module doc with its reasoning)
  - [ ] The stream map targets the probed **primary** video stream, never `0:v:0` (negative test)
  - [ ] `keyframe` mode derives its timestamps from MTRK-06's index and records them as an
        `Explicit` timeline — no `showinfo` parsing anywhere
  - [ ] The child is niced, `kill_on_drop`, bounded by a timeout, and leaves no zombie on expiry
  - [ ] A missing ffmpeg reports `Unavailable` once and stops retrying
  - [ ] No `-hwaccel` or GPU path is introduced (epic §7.1 / spec F)
  - [ ] The whole suite passes with no ffmpeg installed
  - [ ] No hardcoded infrastructure values in new/modified code; all existing tests still pass

---

### MTRK-06: Keyframe index — extraction, compact format, and a pure lookup
- **Priority:** High
- **Labels:** maestro, trickplay, seek, ffprobe
- **Agent:** codex
- **Estimate:** 6h
- **Blocked by:** MTRK-04
- **Description:** A per-file table of `(presentation_time, byte_offset)` for every video keyframe.
  Three consumers, which is what justifies building it properly rather than deriving it twice:
  MTRK-05's keyframe-mode tile selection, MTRK-08's seek-target query, and — when spec E lands —
  MTRX-09's segment alignment and MTRX-10's seek respawn, which today would ask ffmpeg for a `-ss`
  and silently accept whatever keyframe it rounded to.

  **This is the one additional ffprobe invocation in the spec, and it is justified rather than
  smuggled.** A packet-level keyframe enumeration is a fundamentally different query from a
  stream/format probe — different flags, different output volume by two orders of magnitude, a
  different failure profile — so it cannot ride spec A's probe call, and folding it in would make
  every library scan pay for data almost nothing needs. It runs once per file, in the background,
  and its result is cached beside the tiles.

  ## FILES
  - `src/streaming/ffmpeg.rs` — `build_keyframe_probe_args` (pure)
  - `src/maestro/trickplay/keyframes.rs` — extraction, encode/decode, lookup
  - `src/config.rs` — `MAESTRO_KEYFRAME_INDEX_ENABLED`, `_MAX_ENTRIES`, `MAESTRO_FFPROBE_BIN`
  - `README.md` — document the index and its size

  ## APPROACH
  1. Pure argv:
     ```
     -v error -hide_banner -select_streams v:<primary_index>
     -skip_frame nokey
     -show_entries packet=pts_time,pos,flags
     -print_format json  --  <path>
     ```
     `--` plus an explicit rejection of a path whose first byte is `-`, matching spec A MPRB-02's
     hardening of the probe invocation — a file literally named `-loglevel` exists in this kind of library.
  2. Read stdout **incrementally into a capped buffer**, not via `output()`. A 3 h film at a 2 s GOP
     yields ~5,400 packets and a few hundred KB of JSON, but a pathological file can emit far more,
     and this runs inside a 2–4 GB container. Over `MAESTRO_KEYFRAME_INDEX_MAX_ENTRIES` (262,144 —
     roughly a 12 h film at a 0.16 s GOP) ⇒ **truncate and mark the index `partial`**, do not fail:
     a keyframe index covering the first eight hours is strictly better than none, and the flag keeps
     the consumer honest about the tail.
  3. On-disk format `keyframes.idx`: a small header (`magic`, `format_version: u16`,
     `entry_count: u32`, `duration_ms: u64`, `truncated: bool`) followed by **delta-varint-encoded**
     `(pts_ms_delta, byte_offset_delta)` pairs. Deltas because both series are monotonically
     increasing and their deltas are small — this lands at ~8–10 bytes per keyframe against ~16 for
     a fixed-width pair, and §2's ~18 KB/hour figure is computed from it. Encode and decode are
     **pure and round-trip-tested**; the file is never parsed anywhere else.
     A wrong magic or an unknown `format_version` ⇒ treat the index as **absent and regenerate**,
     never partially parse. That is the same posture spec A MPRB-05's `StoredMediaInfo` takes for an
     unknown schema version, and
     for the same reason: during a rolling deploy an older binary will genuinely meet a newer file.
  4. **Pure lookups**, which are the whole point of building this:
     - `nearest_keyframe_at_or_before(pts_ms) -> Option<Keyframe>`
     - `nearest_keyframe_at_or_after(pts_ms) -> Option<Keyframe>`
     - `keyframes_in(range) -> &[Keyframe]`
     Binary search over the decoded vector. Total, clamping, no panics at either boundary.
  5. `pos` (byte offset) is `Option<u64>` — some demuxers report `N/A`, and a fabricated offset is
     far worse than an absent one because a consumer would seek to it. `pts_time` missing ⇒ the
     packet is **dropped with a count**, never zero-defaulted: a keyframe at 0 that is not at 0 makes
     a seek land at the start of the film.
  6. Entries are asserted **strictly increasing in `pts_ms`** at decode time. Some containers emit
     out-of-order packets; sort at extraction, and reject a non-monotonic decoded index as corrupt
     (regenerate) rather than binary-searching a list that is not sorted.
  7. Config-gated: `MAESTRO_KEYFRAME_INDEX_ENABLED=false` ⇒ the module reports `Unavailable`, MTRK-05
     falls back to `accurate` mode, and MTRK-08's seek endpoint returns an honest "not indexed"
     rather than a guess. Epic §7.4 — degrade, never break.

  ## TEST PLAN
  - `cargo test maestro::trickplay::keyframes` — pure argv, encode/decode, and lookup tests
  - Argv asserted exactly on a host with no ffprobe; a path starting with `-` is rejected before spawn
  - Encode ∘ decode round-trips for a synthesised 50,000-entry index, byte-for-byte
  - Deltas: a synthesised 1 h/2 s-GOP index encodes to under 20 KB (asserts §2's figure)
  - `nearest_keyframe_at_or_before` at `-1`, exactly on a keyframe, between two, past the end
  - Both lookups return `None` on an empty index and never panic (negative test)
  - A wrong magic and an unknown `format_version` both decode as absent, not partial (negative test)
  - A non-monotonic decoded index is rejected as corrupt (negative test)
  - A packet with `pts_time: "N/A"` is dropped and counted; one with `pos: "N/A"` keeps the entry
    with `None` offset
  - Over `MAX_ENTRIES` truncates and sets `truncated`, rather than failing or OOMing (negative test)
  - Output-cap enforcement: a stub emitting more than the cap does not OOM
  - The suite passes on a host with no ffprobe
  - Verify no hardcoded infrastructure values in new/modified files

  ## EDGE CASES
  - An all-intra source (ProRes, DV) — every frame is a keyframe, so the index hits the entry cap
    immediately. Truncation plus the `partial` flag is exactly the right behaviour; do not special-
    case, but do document that trickplay `keyframe` mode on such a source degrades to `accurate`
  - A source with no video stream — `Unavailable`, no index attempted
  - MPEG-TS with a discontinuity (a PTS wrap) — the monotonicity check rejects it; regeneration will
    reject it again, so record the reason and stop retrying (MTRK-07's terminal-failure path)
  - A growing file (an in-progress download) — the index is generated against a snapshot; the
    fingerprint changes when the file settles and it regenerates. Correct without special handling
  - A file whose primary video stream index differs from `0` — always use the probed index (MTRK-05
    step 3 makes the same point)

- **Acceptance criteria:**
  - [ ] The keyframe probe argv is pure, uses `--`, and rejects a leading-`-` path (negative test)
  - [ ] The index encodes with delta-varints and round-trips byte-for-byte, at under ~20 KB/hour
  - [ ] Both lookups are total and clamping and return `None` on an empty index without panicking
  - [ ] An unknown format version or wrong magic regenerates rather than partially parsing
  - [ ] A non-monotonic or unparseable-pts entry is rejected/dropped, never zero-defaulted
  - [ ] Over the entry cap the index truncates and is flagged `partial`, and stdout is read into a
        capped buffer rather than unbounded
  - [ ] Disabling the index degrades MTRK-05 to `accurate` mode and MTRK-08 to an honest "not
        indexed" — nothing breaks
  - [ ] The suite passes with no ffprobe installed
  - [ ] No hardcoded infrastructure values in new/modified code; all existing tests still pass

---

### MTRK-07: The background generation job — bounded, resumable, never blocking playback
- **Priority:** Critical
- **Labels:** maestro, trickplay, worker, ops
- **Agent:** claude
- **Estimate:** 7h
- **Blocked by:** MTRK-04, MTRK-05
- **Description:** The worker that turns "this file has no tiles" into "this file has tiles",
  under a rate limit, resumably, and without ever making anyone wait. Three properties are
  non-negotiable and each has its own acceptance criterion.

  **Never blocking playback** is the first-class requirement. A session open must never wait on
  generation, a scrub must never trigger a synchronous ffmpeg run, and a missing artifact must render
  as an honest absence rather than a spinner. Trickplay is a nice-to-have layered on top of playback;
  the moment it can delay playback it has inverted its own priority.

  ## FILES
  - `src/maestro/trickplay/job.rs` — the worker loop, admission, single-flight, retry
  - `src/maestro/trickplay/mod.rs` — spawn point
  - `src/bin/maestro/main.rs` — spawn the worker (guarded on config)
  - `src/config.rs` — `MAESTRO_TRICKPLAY_ENABLED`, `_MAX_CONCURRENT_JOBS`, `_RATE_TITLES_PER_HOUR`,
    `_JOB_TIMEOUT_SECS`, `_MAX_ATTEMPTS`
  - `README.md` — document the worker, its knobs, and the on-demand trigger

  ## APPROACH
  1. **Work discovery, and where the queue lives.** The queue is **the filesystem plus the library**,
     not a table: candidates are media files whose current-fingerprint generation directory does not
     exist. Maestro's DB role is read-only on library tables and Maestro must not grow a table of
     library state (epic §2) — so a `trickplay_jobs` table would be either a second library model or
     an over-privileged grant. Scanning the store is cheap (one `read_dir` per candidate) and is
     inherently self-healing: delete an artifact and it regenerates, with no queue to fall out of
     sync with the disk.
  2. **Two priority lanes**, because the useful ordering is not "oldest first":
     - **Foreground lane** — a session opened for a file with no artifact enqueues it at the head.
       Someone is watching it *now*, and while they will not get previews for this viewing, they
       very likely will for the next. This is the single highest-value ordering signal available and
       it costs one enqueue call in the session-open path (fire-and-forget, never awaited — see the
       non-blocking requirement).
     - **Sweep lane** — everything else, ordered by `media_files.id` for a stable keyset cursor.
  3. **Rate limiting** at `MAESTRO_TRICKPLAY_RATE_TITLES_PER_HOUR` (default 60) with
     `MAESTRO_TRICKPLAY_MAX_CONCURRENT_JOBS` (default 1). One at a time and one a minute is
     deliberately unhurried: the library is on a network-mounted read-only share (epic §10.3) and an
     unbounded fan-out of decoders across it degrades playback for the whole household while it
     runs. At 60/hour a ~10,000-title library sweeps in about a week of background time — which is
     fine, because nothing waits for it.
  4. **Resumable, at two granularities.** Between titles: the cursor is the store itself, so a
     restart resumes by re-scanning and skipping what exists — no state to persist and none to
     corrupt. Within a title: a job writes into a `.partial` generation directory, and the manifest
     is written **after** the last sheet, so an interrupted job leaves a `.partial` that the next
     pass either resumes from (sheets already on disk are kept; ffmpeg restarts from the first
     missing sheet's timestamp) or discards after `MAESTRO_TRICKPLAY_MAX_ATTEMPTS`. **A `Partial`
     manifest is published for a long job** so previews light up for the completed prefix rather
     than nothing until the end.
  5. **Pre-admission checks, in order, all cheap, all before any decode:**
     a. `MAESTRO_TRICKPLAY_ENABLED` and ffmpeg present.
     b. The file resolves through spec D's `library::resolve` — a `MediaHandle`, never a path —
        which itself resolves through the shared `PathGuard`/`ResolvedPath` that spec D promotes
        from `src/foundry/paths.rs` to `src/paths.rs`. **Trickplay introduces no allowlist, no
        canonicalisation and no path handling of its own**; it inherits that discipline whole. The
        same applies to MTRK-05's ffmpeg input and MTRK-06's ffprobe input: both take a
        `ResolvedPath`/`MediaHandle`, never a `&str`, so an unvalidated path is unrepresentable
        rather than merely discouraged.
     c. `media_info` has a duration and a non-`attached_pic` video stream; otherwise `Unavailable`.
     d. `estimate_bytes` (MTRK-03) against `budget_verdict` (MTRK-04): `Refuse` ⇒ skip and log with
        the numbers. **Refusing to start is always better than starting and filling a disk**, and
        checking the estimate before the decode rather than the usage after it is what makes the
        budget a limit instead of a post-mortem.
  6. **Single-flight** on `(media_file_id, fingerprint)` — an in-process map of in-flight keys, so a
     foreground enqueue for a file already being swept is a no-op rather than a second decoder on
     the same file.
  7. **Post-job fingerprint recheck (§4).** After the job, recompute the fingerprint from the file's
     current size/mtime. Changed ⇒ **discard the output** and re-enqueue. A half-old, half-new sheet
     set is worse than none, and Foundry's verify-and-swap makes this a real race, not a theoretical
     one.
  8. **Retry taxonomy**, mirroring spec A's `is_retryable()` split because the same distinction
     matters: a spawn failure, a timeout, or a vanished mount is retryable up to `MAX_ATTEMPTS` with
     backoff; a non-monotonic index, no video stream, or an ffmpeg non-zero exit on a readable file
     is **terminal** — recorded in the manifest as `Failed { reason }` and not retried, because
     retrying a broken file forever burns the rate budget the rest of the library needs. A terminal
     failure is a library-health finding, surfaced by MTRK-13.
  9. **On-demand trigger** for the operator: `POST /ops/trickplay/generate` with a `media_file_id`,
     which enqueues at the head of the foreground lane and returns immediately (`202`). It never
     runs the job inline — see the non-blocking requirement.
 10. Config-gated: `MAESTRO_TRICKPLAY_ENABLED=false` ⇒ the worker is not spawned at all and every
     surface reports `Unavailable`. The existing Muse convention (epic §7.4).

  ## TEST PLAN
  - `cargo test maestro::trickplay::job` — the loop's decision logic is factored into pure functions
    (`next_candidate`, `admission_verdict`, `retry_verdict`) tested without a worker, a database or a
    filesystem, and only the thin driver is left untested
  - **Session open never awaits generation** — assert the enqueue is fire-and-forget and that a
    session opens normally with a deliberately-wedged generator (the single most important test here)
  - Rate limiting: N candidates over a simulated clock start no faster than the configured rate
  - Single-flight: two enqueues for the same `(file, fingerprint)` produce one job
  - Resume: a `.partial` with 3 of 8 sheets resumes from sheet 3 and keeps the existing three
  - A `Partial` manifest is published for a long job and the completed prefix is servable
  - Post-job fingerprint change discards the output and re-enqueues (negative test)
  - Admission refuses on `Refuse` from the budget and logs the numbers; no ffmpeg is spawned
    (negative test)
  - Retry: a timeout retries up to `MAX_ATTEMPTS` then goes terminal; a "no video stream" goes
    terminal on the **first** attempt (negative test)
  - `MAESTRO_TRICKPLAY_ENABLED=false` spawns no worker and every surface reports `Unavailable`
  - Verify no hardcoded infrastructure values in new/modified files

  ## EDGE CASES
  - The library mount disappears mid-sweep — every candidate fails `Unreadable`, which is retryable;
    back off hard rather than burning attempts, and log once rather than per file
  - A file deleted between discovery and job start — `Ok(None)` from resolve, skip cleanly
  - Maestro restarted mid-job — the `.partial` survives, the ffmpeg child does not (`kill_on_drop`),
    and the next pass resumes. Assert no orphaned ffmpeg after a simulated restart
  - Budget crossed *during* a sweep — the next admission refuses; the in-flight job is allowed to
    finish (it is bounded by its own estimate and killing it wastes the decode already done)
  - Clock changes / NTP step — use a monotonic instant for rate limiting, never wall-clock
  - Two Maestro instances against one work dir (a bad deploy) — MTRK-04's refuse-to-create-existing
    behaviour is the guard; log loudly, since this is a misconfiguration and not a supported mode

- **Acceptance criteria:**
  - [ ] Generation **never blocks a session open, a scrub, or any playback path** — proven by a test
        that opens a session normally against a wedged generator
  - [ ] Work is discovered from the store + library with no Maestro-owned library table (epic §2)
  - [ ] Rate limit and concurrency cap are respected against a simulated clock
  - [ ] A job is resumable within a title, publishes a `Partial` manifest, and survives a restart
        with no orphaned ffmpeg child
  - [ ] Admission refuses before decoding when the estimate breaches the budget (negative test)
  - [ ] The post-job fingerprint recheck discards output for a file that changed mid-job
  - [ ] Retryable and terminal failures are distinguished; a terminal failure is not retried
  - [ ] Disabling the feature spawns no worker and every surface reports `Unavailable`
  - [ ] README documents the worker, its knobs and the on-demand trigger
  - [ ] No hardcoded infrastructure values in new/modified code; all existing tests still pass

---

### MTRK-08: The trickplay HTTP surface — manifest, sheets, keyframe query
- **Priority:** Critical
- **Labels:** maestro, trickplay, http
- **Agent:** claude
- **Estimate:** 5h
- **Blocked by:** MTRK-04, MTRK-06
- **Description:** Three read-only routes on Maestro's existing axum surface, served through
  `proxy_maestro` per §5. Small, cacheable, and carrying no text.

  ## FILES
  - `src/maestro/http/trickplay.rs` — new; the three handlers
  - `src/maestro/http/mod.rs` — route registration
  - `README.md` — document the routes

  ## APPROACH
  1. Routes:
     ```
     GET /trickplay/{media_file_id}/manifest.json
     GET /trickplay/{media_file_id}/sheet/{n}.jpg
     GET /trickplay/{media_file_id}/keyframes?at_ms=&direction=before|after
     ```
     `media_file_id` parses as `i64` and `n` as `u32` **in the extractor**; a non-numeric segment is
     a `404` from routing and never reaches a path join. Combined with MTRK-04 step 2, there is no
     code path from a request to an arbitrary filesystem location — the structural posture spec D
     MDLV-02 establishes, applied here.
  2. **Every state is a `200` with an honest `state` field, except a genuinely unknown file.** A
     file with no artifact returns `200 {"state":"absent"}`, not a `404`: the player must distinguish
     "no previews for this file" (render the scrub bar without previews, silently) from "the server
     is broken" (render a degraded note), and an HTTP status cannot carry that distinction without
     the client guessing. `404` is reserved for a `media_file_id` that does not exist in the library.
     A sheet request for an absent artifact **is** a `404` — that one is a genuine missing resource.
  3. **Caching.** Sheets are immutable within a generation, so:
     `ETag: "{fingerprint_prefix}-{n}"`, `Cache-Control: public, max-age=31536000, immutable`, and
     honour `If-None-Match` with a `304`. The manifest gets `Cache-Control: no-cache` plus an ETag on
     the fingerprint — it must be revalidated because `state` changes as a job progresses, but the
     revalidation is a `304` almost every time. A film's whole preview set is then fetched once, ever.
  4. Sheets stream from disk with `tokio::fs::File` + `ReaderStream`, `Content-Type: image/jpeg`,
     `Content-Length` from metadata. No range support: a 500 KB immutable image does not need it, and
     spec D MDLV-03's range machinery is for media, not thumbnails. Say so rather than leaving a
     reviewer to wonder why `Accept-Ranges` is absent.
  5. The keyframe route returns `{"pts_ms":…, "byte_offset":…|null, "truncated":bool}` or
     `{"state":"absent"}`. This is the seam spec E's seek will use (§3); shape it for that consumer
     now, since retrofitting it later is a delivery-path change.
  6. **No text, enforced by a test.** A grep-style test over `src/maestro/http/trickplay.rs` and the
     manifest type asserts no `title`, `poster`, `overview`, `year`, `name`, `path` or `filename`
     field is emitted. That is epic §2 clause 5 made mechanical rather than social, and it is the
     kind of rule that otherwise survives exactly until the next hurried change.
  7. Auth is the existing Maestro bearer injected by `proxy_maestro`; these routes add no new
     credential and no new auth path (Module Contract clause 1). Note the standing epic §10.4 risk:
     an unprovisioned `CONSTELLATION_MAESTRO_TOKEN` makes every one of these `401`, which looks
     exactly like "trickplay is broken" — MTRK-13's diagnostics distinguish them.

  ## TEST PLAN
  - `cargo test maestro::http::trickplay` — handler tests over a `tempfile` store
  - `manifest.json` for an absent artifact is `200` with `state: "absent"`, **not** `404`
  - `manifest.json` for an unknown `media_file_id` is `404`
  - A sheet request for an absent artifact is `404`
  - A non-numeric `media_file_id` or sheet index is a routing `404` and never reaches a path join
    (negative test); `../` and a URL-encoded traversal in either segment likewise
  - `If-None-Match` with the current ETag yields `304` with no body
  - A `Partial` manifest reports `tiles_done`/`tiles_total` and its existing sheets are servable
  - The keyframe route returns the correct neighbour in both directions and `state: "absent"` with
    no index (negative test)
  - The no-text grep test fails when a `title` field is deliberately added to the manifest type
  - Verify no hardcoded infrastructure values in new/modified files

  ## EDGE CASES
  - A sheet file deleted by eviction between the manifest read and the sheet GET — `404`; the client
    re-fetches the manifest on a sheet 404 (MTRK-10 step 6) rather than showing a broken image
  - A generation swapped mid-request — the open fd keeps the old inode alive on Linux, so the
    in-flight read completes; document rather than lock
  - A sheet index beyond `sheets.len()` — `404`, never a path join with a large integer
  - Trickplay disabled — every route returns `200 {"state":"unavailable","reason":…}`, except the
    sheet route which is `404`. Honest degradation, not a 500

- **Acceptance criteria:**
  - [ ] `absent` / `queued` / `partial` / `ready` / `failed` / `unavailable` are distinguishable by
        the client from the manifest, and only an unknown file is a `404`
  - [ ] Sheets are immutable-cached with an ETag and honour `If-None-Match` with `304`
  - [ ] No request segment reaches a path join un-parsed; traversal attempts are routing `404`s
        (negative test)
  - [ ] The manifest and every response carry **no** title, poster, overview, year or path —
        enforced by a test that fails when such a field is added (epic §2 clause 5)
  - [ ] The keyframe route answers before/after queries and reports an absent index honestly
  - [ ] Disabling trickplay degrades every route rather than erroring
  - [ ] README documents the three routes
  - [ ] No hardcoded infrastructure values in new/modified code; all existing tests still pass

---

### MTRK-09: Muse `GET /media/{id}/markers` — chapters and markers, composed
- **Priority:** High
- **Labels:** muse, markers, chapters, http
- **Agent:** claude
- **Estimate:** 4h
- **Blocked by:** MTRK-01, MTRK-02
- **Description:** One read endpoint returning everything the player needs to draw a marker rail:
  chapters (from `media_info`, MTRK-01) and markers (from `media_markers`, MTRK-02), resolved,
  clamped, and ordered.

  **This is a Muse endpoint, and that placement is the point** (§0). Chapter titles are text, and
  epic §2 clause 5 plus spec G rule (a) put text on Muse's side of the line: *maestro responses are
  ids and playback state; muse responses are what things are called.* Putting this on Maestro would
  give the sidecar a textual-metadata surface on the day trickplay ships, which is exactly the
  erosion the epic warns about — and it would arrive through the most reasonable-sounding door
  available ("the player already talks to Maestro").

  ## FILES
  - `src/http/mod.rs` — the route
  - `src/web/markers.rs` — new; the handler and the composition
  - `src/models/media_marker.rs` — the response DTO
  - `README.md` — document the endpoint

  ## APPROACH
  1. Response:
     ```jsonc
     {
       "v": 1,
       "media_item_id": 1234,
       "episode_id": null,
       "duration_ms": 7231000,          // null when unprobed
       "duration_known": true,
       "chapters": [ { "index": 0, "start_ms": 0, "end_ms": 612000, "title": "Cold Open" } ],
       "markers":  [ { "kind": "intro", "start_ms": 612000, "end_ms": 701000,
                       "source": "manual", "confidence": null } ]
     }
     ```
  2. Chapters come from spec A MPRB-05's single typed reader — the `StoredMediaInfo` accessor in
     `src/media/doc.rs` that decodes `MediaInfoDoc`, reaching `doc.probe.chapters` — **never an
     ad-hoc jsonb key read**, which spec A's grep guard forbids anyway. `Legacy`, `Absent` or
     `UnknownVersion` ⇒ an empty chapter list with `duration_known: false`, never an error: an
     unprobed item must render a player, just without a chapter rail.
  3. Markers from `repo::media_marker::list_for_*`, resolved by the §MTRK-02 precedence
     `manual > provider > analysis > container` — **at most one marker per `kind` is returned**, and
     the winner carries its `source` so a debugging operator can see which one won. Returning all of
     them and letting the client choose would put a precedence rule in the GUI, where it would
     immediately diverge from the one a future detector assumes.
  4. **Clamp at read time**, and say so: a marker or chapter extending past `duration_ms` is clamped
     to it and the response sets `"clamped": true` on that entry. A marker entirely beyond the
     runtime is **dropped** with a count in a `warnings` array. MTRK-02 deliberately does not reject
     these at write time (an item may be unprobed when a marker is entered), so read time is where
     the reconciliation has to happen. With `duration_known: false`, nothing is clamped or dropped —
     an unknown duration is not permission to invent a bound.
  5. Chapters are already sorted and de-duplicated by MTRK-01; assert it here rather than re-sorting,
     so a regression upstream fails loudly instead of being silently papered over.
  6. Behind the existing `MUSE_API_TOKEN` bearer, reached by the browser through the existing
     `proxy_muse` — no new credential, no new proxy arm. Note the standing TERM #549 defect:
     `CONSTELLATION_MUSE_TOKEN` is unprovisioned, so protected Muse routes `401` today. This endpoint
     inherits that, and MTRK-11's degraded state must therefore be genuinely good, because it is what
     the household will see until the token is provisioned.

  ## TEST PLAN
  - `cargo test` — handler tests over synthesised `media_info` and marker rows
  - An item with chapters and no markers returns the chapters and an empty marker list
  - An item with a `manual` and an `analysis` intro returns **one** intro, the `manual` one, and
    reports `source: "manual"`
  - A marker past `duration_ms` is clamped and flagged; one entirely beyond it is dropped and counted
  - `duration_known: false` clamps and drops nothing (negative test)
  - A `Legacy` / `Absent` / `UnknownVersion` `media_info` yields an empty chapter list and a `200`,
    never a `500` (negative test)
  - An unknown item id is `404`
  - Chapter ordering from MTRK-01 is asserted, not re-derived
  - The endpoint reads `media_info` only through the `StoredMediaInfo` accessor — spec A's grep
    guard stays green
  - Verify no hardcoded infrastructure values in new/modified files

  ## EDGE CASES
  - An episode id vs an item id — both accepted, mirroring `play_sessions`' divergence; exactly one
    is echoed back
  - Overlapping markers of different kinds (intro overlapping recap) — both returned; MTRK-12 picks
  - An item with 1,000 chapters (the MTRK-01 cap) — returned in full; the response is ~60 KB, which
    is fine for a route hit once per playback, and MTRK-11 virtualises its list rather than the API
    truncating
  - A chapter with `end_ms: None` — the response derives it from the next chapter's start, or from
    `duration_ms` for the last one, and marks it `derived: true`. An undeclared derivation is how a
    UI ends up drawing a boundary the file does not have

- **Acceptance criteria:**
  - [ ] One endpoint returns chapters and markers with duration and an explicit `duration_known`
  - [ ] Marker precedence `manual > provider > analysis > container` is applied **server-side**, one
        per kind, with the winning source reported
  - [ ] Out-of-range entries are clamped or dropped and reported; nothing is clamped when the
        duration is unknown (negative test)
  - [ ] An unprobed / legacy / unknown-version `media_info` yields an empty chapter list and a `200`
  - [ ] `media_info` is read only through the `StoredMediaInfo` accessor; spec A's grep guard stays green
  - [ ] **This endpoint lives in Muse, not Maestro**, and its module doc cites epic §2 clause 5
  - [ ] README documents the endpoint
  - [ ] No hardcoded infrastructure values in new/modified code; all existing tests still pass

---

### MTRK-10: constellation-web — the trickplay client arm and hover-scrub previews
- **Priority:** Critical
- **Labels:** maestro, constellation-web, player, trickplay
- **Agent:** claude
- **Estimate:** 7h
- **Blocked by:** MTRK-08
- **Description:** The visible payoff: hovering or dragging the scrub bar shows the frame you are
  about to land on. Extends spec G's `ScrubBar`; does not build a second one.

  ## FILES
  - `constellation-web/src/lib/aggregationClient.ts` — the trickplay arm (**the only fetch site**)
  - `constellation-web/src/panels/maestro/trickplay.ts` — new; pure tile math, transcribed from
    MTRK-03
  - `constellation-web/src/panels/maestro/ScrubPreview.tsx` — new; the preview thumbnail
  - `constellation-web/src/panels/maestro/ScrubBar.tsx` — compose the preview (spec G's component)
  - `constellation-web/src/hooks/useTrickplay.ts` — new
  - `constellation-web/dist/**` — **rebuilt and committed**
  - `constellation-web/README.md` — note the new routes

  ## APPROACH
  1. **`aggregationClient.ts` is the only module permitted to call `fetch`** (grep-enforced, epic
     §7.8). Add a typed `maestro.trickplay.manifest(mediaFileId)` through the existing
     `request<T>('maestro', path)` seam. **Sheet images are binary and follow the `museArtUrl`
     precedent** — a URL builder (`trickplaySheetUrl(mediaFileId, n)`) returning
     `/api/maestro/trickplay/{id}/sheet/{n}.jpg` for a CSS `background-image`, not a `request<T>()`
     JSON call. That is the same carve-out `museArtUrl` already documents for artwork, applied
     identically, and the URL builder still lives in `aggregationClient.ts` so the rule holds.
  2. `trickplay.ts` transcribes MTRK-03's `tile_index_at`, `sheet_and_cell` and `cell_rect` — **pure,
     no React, unit-tested with vitest against the same fixture values as the Rust golden tests**.
     Two implementations of one piece of arithmetic is a real risk; a shared fixture is the cheap
     mitigation, and a divergence then fails on both sides rather than producing a preview that is
     one tile off in a way nobody notices for a month.
  3. Rendering is a **CSS sprite**: a fixed-size `<div>` with `background-image` at the sheet URL and
     `background-position` from `cell_rect`. No canvas, no per-tile image element, no cropping in JS.
     The browser then caches each sheet exactly once (MTRK-08's immutable headers) and a drag across a
     whole film costs a handful of image requests.
  4. **Prefetch the neighbouring sheet** when the pointer enters the last 20% of the current one.
     Dragging across a sheet boundary is otherwise a visible stall on the first pass, and this is one
     `<link rel=prefetch>`-equivalent (an `Image()` warm) rather than a loading state.
  5. **Absence is silent, failure is stated.** `state: absent | queued | partial` (outside the
     completed prefix) ⇒ the scrub bar renders **exactly as spec G ships it**, with no preview and no
     placeholder, no spinner and no "generating…" chrome. Trickplay is an enhancement and a missing
     enhancement is not a fault to report to a viewer. `state: failed | unavailable` and a network
     error ⇒ still no preview, but the `/why` affordance (MPLY-12) shows the reason for an operator.
     This follows the muse panels' established rule — *omit sections whose data is absent; state only
     the observed absence, never a default value*.
  6. A sheet `404` (evicted between manifest and fetch, MTRK-08's edge case) ⇒ refetch the manifest
     **once**, then give up silently for this session. Never a retry loop against an evicted artifact.
  7. Fetch the manifest **once per session open**, not per hover. Hover state is local; nothing here
     polls.
  8. Tokens only — `--radius-md`, `--border-default`, `--shadow-card`, `--bg-surface-raised`,
     `--text-secondary` for the time label. No hex, no raw `px` in the preview's own styles (the
     sheet-derived `background-position` is computed geometry, not a design decision, and belongs in
     an inline computed style with a comment saying so). `npm run lint:adherence` must gain no new
     warnings.
  9. Accessibility: the preview is decorative and mirrors the `<input type="range">` value spec G
     already exposes — `aria-hidden`, with the timestamp remaining in the range's `aria-valuetext`.
     A decorative image announced by a screen reader is noise.
 10. **Rebuild and commit `dist/`** via `npm run build:verify` then `git add constellation-web/dist`.
     A panel change that does not rebuild `dist/` deploys nothing (epic §5; TERM #550).

  ## TEST PLAN
  - `npm run build` (tsc + vite + the http-bundle assertion) and `npm run lint:adherence` — no new
    warnings against the recorded pre-flight baseline
  - vitest: `trickplay.ts` reproduces the **same fixture values** as MTRK-03's Rust golden tests for
    both timeline kinds, including both clamped ends
  - vitest: a pointer at position `t` computes the sheet URL and `background-position` for the
    expected cell
  - vitest: `state: 'absent'` renders no preview element at all — assert absence, not an empty one
    (negative test)
  - vitest: a sheet `404` triggers exactly one manifest refetch and then stops (negative test)
  - vitest: an `Explicit` timeline preview matches the tile at or before the pointer
  - Live capture per spec G §5: drag the scrub bar on a title with a ready artifact and screenshot
    the preview; then on a title with none and confirm the bar is visually unchanged from spec G
  - `fetch` appears nowhere outside `aggregationClient.ts` (the existing grep gate)
  - Verify no hardcoded infrastructure values in new/modified files

  ## EDGE CASES
  - `duration` is `Infinity` (a live/unbounded transcode) — spec G already renders the bar
    unseekable; no preview is attempted
  - A very short item (one tile) — the same tile for every position; correct, not a bug
  - A `partial` artifact — previews inside the completed prefix, nothing outside it, no boundary
    marker on the bar (a "previews end here" tick would be noise about an internal state)
  - Rapid drag across many sheets — the sprite approach means the browser coalesces; do not debounce
    the position, only the prefetch
  - Touch drag — the preview follows the touch point and is offset upward so a thumb does not cover it
  - A 2.39:1 film — `tile_height` comes from the manifest, never assumed 16:9, or the preview letterboxes

- **Acceptance criteria:**
  - [ ] Hovering/dragging the scrub bar shows the frame at that position, from a CSS sprite
  - [ ] `aggregationClient.ts` is the only fetch site; the sheet URL builder lives there too and
        follows the existing `museArtUrl` binary precedent
  - [ ] `trickplay.ts` is pure and asserted against **the same fixtures** as MTRK-03's Rust tests
  - [ ] An absent artifact renders the scrub bar unchanged — no placeholder, no spinner (negative test)
  - [ ] A sheet `404` refetches the manifest once and then stops (negative test)
  - [ ] `npm run lint:adherence` gains no new warnings; no hex or raw `px` in the new styles
  - [ ] **`dist/` rebuilt and committed** (TERM #550)
  - [ ] No hardcoded infrastructure values in new/modified code; all existing tests still pass

---

### MTRK-11: constellation-web — the chapter list and chapter ticks
- **Priority:** High
- **Labels:** maestro, constellation-web, player, chapters
- **Agent:** claude
- **Estimate:** 5h
- **Blocked by:** MTRK-09
- **Description:** Chapter ticks on the scrub bar and a chapter list panel, fed by MTRK-09 through
  `proxy_muse` — the composition that makes the §0 ownership split visible in the client.

  ## FILES
  - `constellation-web/src/lib/aggregationClient.ts` — the markers arm on the **muse** system
  - `constellation-web/src/hooks/useMediaMarkers.ts` — new
  - `constellation-web/src/panels/maestro/ChapterList.tsx` — new
  - `constellation-web/src/panels/maestro/ChapterTicks.tsx` — new
  - `constellation-web/src/panels/maestro/ScrubBar.tsx` — compose the ticks
  - `constellation-web/src/panels/maestro/PlayerPanel.tsx` — mount the list
  - `constellation-web/dist/**` — **rebuilt and committed**

  ## APPROACH
  1. **The markers call goes through `request<T>('muse', …)`, not `'maestro'`.** Add a comment at the
     call site naming epic §2 clause 5 and spec G rule (a), because the "obvious" refactor a year
     from now is to move it onto the maestro arm "since the player uses it", and that refactor is
     the ownership erosion the epic forbids. One comment is cheaper than the review that catches it.
  2. `ChapterTicks` renders one 1px-ish token-sized marker per chapter start over spec G's scrub bar,
     absolutely positioned by `start_ms / duration_ms`. Chapters at 0 are not drawn (every film has
     one and it is noise at the bar's left edge). Ticks are `aria-hidden`; the chapter list is the
     accessible surface.
  3. `ChapterList` is a scrollable list of `{time, title}` rows; clicking one seeks. **Titles come
     from the payload; an untitled chapter renders the positional fallback `Chapter {n}` here** —
     MTRK-01 deliberately does not synthesise it server-side because only the client knows the
     ordinal it is displaying and the display language.
  4. **Absence rules, per the muse panels' established convention:** no chapters ⇒ **the list is not
     rendered at all** and no ticks are drawn. Not an empty panel, not "No chapters" — most of the
     library has no chapters, and a permanent empty section is a permanent dead pixel. A *degraded*
     fetch (a `401` from the unprovisioned `CONSTELLATION_MUSE_TOKEN`, TERM #549; a 5xx) renders an
     `AbsenceNote` stating the observed failure, which is a different thing and must look different.
  5. Virtualise the list beyond ~200 rows (the MTRK-09 edge case allows up to 1,000).
  6. Chapter and marker data are fetched **once per item**, alongside the trickplay manifest, at
     session open. No polling: chapters do not change during playback.
  7. Tokens only; `lint:adherence` gains no warnings. **Rebuild and commit `dist/`.**

  ## TEST PLAN
  - `npm run build` + `npm run lint:adherence` — no new warnings
  - vitest: three chapters render three ticks at the right fractional positions; a chapter at 0
    renders none
  - vitest: an untitled chapter renders `Chapter 2`; a titled one renders its title verbatim
  - vitest: **no chapters ⇒ neither the list nor the ticks appear in the tree at all** (negative test)
  - vitest: a degraded fetch renders an `AbsenceNote` and is visually distinct from the no-chapters
    case (negative test)
  - vitest: clicking a row issues exactly one seek to that chapter's `start_ms`
  - vitest: the markers request targets the **muse** system, not maestro (a real assertion, because
    this is the item's structural point)
  - vitest: a 1,000-chapter payload virtualises rather than mounting 1,000 rows
  - Live capture: a chaptered title showing ticks and the list; an unchaptered one showing neither
  - Verify no hardcoded infrastructure values in new/modified files

  ## EDGE CASES
  - Chapters denser than the bar's pixel width — ticks coalesce visually; do not thin them
    algorithmically, which would misrepresent chapter density
  - A chapter with a `derived: true` end (MTRK-09) — rendered identically; the derivation is a
    server-side concern
  - Duration unknown — no ticks (nothing to position against), but the list still renders with times
  - A chapter title that is extremely long — truncate with an ellipsis and a `title` attribute; never
    wrap the row and shift the list
  - RTL titles — the list inherits the shell's direction handling; the tick rail stays LTR-positioned
    since it maps to time, not text

- **Acceptance criteria:**
  - [ ] Chapter ticks render on the scrub bar and a clickable chapter list seeks correctly
  - [ ] The markers call goes through the **muse** system with a comment citing epic §2 clause 5
        (asserted by a test)
  - [ ] No chapters ⇒ neither the list nor the ticks render at all; a *degraded* fetch renders a
        distinct `AbsenceNote` (two separate negative tests)
  - [ ] An untitled chapter gets a client-side positional fallback; the server never synthesises one
  - [ ] Long chapter lists virtualise
  - [ ] `npm run lint:adherence` gains no new warnings
  - [ ] **`dist/` rebuilt and committed** (TERM #550)
  - [ ] No hardcoded infrastructure values in new/modified code; all existing tests still pass

---

### MTRK-12: Skip intro / skip credits — RENDERING ONLY
- **Priority:** High
- **Labels:** maestro, constellation-web, player, markers
- **Agent:** claude
- **Estimate:** 4h
- **Blocked by:** MTRK-11
- **Description:** The "Skip Intro" button, and nothing behind it. This item **consumes** MTRK-09's
  markers and renders a button; it contains **no detection whatsoever** (§0, epic §4). If this item's
  diff contains a frame comparison, an audio fingerprint, a black-frame heuristic, a cross-episode
  scan, or a Chord call, it is wrong and should be rejected on epic §2 grounds regardless of how well
  it works.

  Until a detection spec exists, this button appears only on items with a **manually entered** marker
  (MTRK-02's `/ops/markers`). That is not a shortcoming — it is exactly how you verify the rendering,
  the timing and the dismissal behaviour are right before committing sprints to detection, and it is
  why the contract is built first.

  ## FILES
  - `constellation-web/src/panels/maestro/SkipButton.tsx` — new
  - `constellation-web/src/panels/maestro/skipPolicy.ts` — new; pure, tested
  - `constellation-web/src/panels/maestro/PlayerPanel.tsx` — mount it
  - `constellation-web/dist/**` — **rebuilt and committed**

  ## APPROACH
  1. `skipPolicy.ts` is **pure**: `activeSkip(positionMs, markers, dismissed) -> Skip | null`. All
     the fiddly behaviour lives here, unit-tested, with no React and no timers:
     - A marker is active when `start_ms <= position < end_ms - LEAD_OUT_MS` (`LEAD_OUT_MS = 2000`).
       The lead-out stops the button appearing for the last two seconds of an intro, where it is
       useless and where clicking it feels broken.
     - Only markers of kind `intro` and `credits` produce a button in this item. `recap`, `preview`
       and `ad` are returned by the API and deliberately **not** rendered yet — each needs its own UX
       decision (auto-skip? a different label?) and inventing three of them here is scope creep.
     - Once dismissed, a marker does not reappear **for that session**, even on a rewind back into
       it. A button you dismissed that keeps coming back is worse than no button.
     - Overlapping applicable markers ⇒ the **earliest-ending** one wins, so the viewer is not
       offered a skip that jumps past content they have not seen.
  2. Behaviour: the button appears with a short fade, seeks to `end_ms` on click, and disappears at
     `end_ms` or on dismiss. **It never auto-skips.** Auto-skip is a preference and preferences need
     a prefs key, a settings surface and a decision about defaults — all of which are a follow-up.
     Shipping it on by default would silently skip content for a household that never asked.
  3. Labels are `Skip Intro` / `Skip Credits`, derived from `kind`, never from a chapter title. A
     chapter named "Opening Titles" is **not** a marker and must not produce a skip button — chapters
     describe structure, markers describe skippable content, and conflating them is how a player ends
     up offering to skip the first act.
  4. Confidence is **never rendered**. MTRK-02 exposes it for a future server-side threshold; a
     percentage on a button is noise the viewer cannot act on.
  5. Positioned bottom-right above the control bar, following it when the controls auto-hide (spec G
     MPLY-05 step 6) so it never floats over the video alone. Keyboard-reachable and in the tab order;
     `Enter`/`Space` activate. Not bound to a bare letter key — spec G MPLY-07 owns the keyboard map
     and a second spec adding a global binding is how shortcut conflicts start.
  6. Tokens only; `lint:adherence` gains no warnings. **Rebuild and commit `dist/`.**

  ## TEST PLAN
  - `npm run build` + `npm run lint:adherence` — no new warnings
  - vitest on `skipPolicy`: active exactly within `[start, end - LEAD_OUT)`; inactive before, after,
    and in the lead-out window
  - vitest: a dismissed marker does not reappear after seeking backwards into it (negative test)
  - vitest: two overlapping applicable markers ⇒ the earliest-ending wins
  - vitest: `recap` / `preview` / `ad` markers produce **no** button (negative test)
  - vitest: **a chapter titled "Opening Titles" produces no skip button** — the item's central
    negative test, proving chapters and markers are not conflated
  - vitest: no marker ⇒ no button element in the tree at all
  - vitest: clicking issues exactly one seek to `end_ms`
  - **Diff-level check, stated as an acceptance criterion: no detection logic.** No frame/audio
    comparison, no cross-episode scan, no Chord call anywhere in the diff
  - Live capture: a manually-marked episode showing the button appear, be clicked, and not return
  - Verify no hardcoded infrastructure values in new/modified files

  ## EDGE CASES
  - A marker covering the whole runtime (a bad manual entry) — MTRK-09 clamps it; the button then
    offers to skip the film. Acceptable for a manual entry, and a reason a future detector needs a
    server-side sanity bound rather than a client-side one
  - Seeking past `end_ms` while the button is showing — it disappears on the next position update
  - A marker starting at 0 — the button appears immediately on play; correct for a cold open
  - Playback at 2× — position updates are position-based, not time-based, so nothing changes
  - Both an intro marker and a chapter tick at the same instant — both render; they are different
    affordances and the tick is 1px

- **Acceptance criteria:**
  - [ ] A skip button appears for `intro`/`credits` markers, seeks to `end_ms`, and does not return
        after dismissal within a session (negative test)
  - [ ] **The diff contains no detection logic of any kind** — no frame or audio comparison, no
        cross-episode scan, no Chord call (epic §2/§4)
  - [ ] A chapter is never treated as a marker — a chapter titled "Opening Titles" produces no button
        (negative test)
  - [ ] `recap`/`preview`/`ad` markers render nothing yet; confidence is never displayed
  - [ ] It never auto-skips
  - [ ] `skipPolicy` is pure and covers the lead-out, overlap and dismissal rules under vitest
  - [ ] `npm run lint:adherence` gains no new warnings
  - [ ] **`dist/` rebuilt and committed** (TERM #550)
  - [ ] No hardcoded infrastructure values in new/modified code; all existing tests still pass

---

### MTRK-13: Observability and the `/why` diagnostics affordance
- **Priority:** Medium
- **Labels:** maestro, trickplay, metrics, ops
- **Agent:** codex
- **Estimate:** 3h
- **Blocked by:** MTRK-07, MTRK-08
- **Description:** Make the whole subsystem explicable without shell access. The failure modes here
  are all quiet — a job that never runs, an artifact that never generates, a `401` from an
  unprovisioned proxy token, a budget silently refusing everything — and every one of them presents
  identically to a viewer as "no previews".

  ## FILES
  - `src/maestro/trickplay/metrics.rs` — new
  - `src/maestro/http/trickplay.rs` — the `/why` route
  - `src/maestro/metrics.rs` — register the families
  - `README.md` — document the metrics and the route

  ## APPROACH
  1. Prometheus metrics, following the crate's existing `metrics.rs` conventions:
     - `maestro_trickplay_artifacts_total{state}` (gauge) — ready / partial / failed
     - `maestro_trickplay_jobs_total{outcome}` (counter) — completed / failed / refused / discarded
     - `maestro_trickplay_job_duration_seconds` (histogram)
     - `maestro_trickplay_bytes_used` / `_budget_bytes` (gauges)
     - `maestro_trickplay_evictions_total` (counter)
     - `maestro_trickplay_pending` (gauge) — candidates with no current artifact
     - `maestro_trickplay_sheet_requests_total{result}` — hit / not_modified / miss
     - `maestro_keyframe_index_total{state}` — ready / truncated / failed
     `maestro_trickplay_pending` is the one to alert on: a number that never falls means the worker
     is not running, and that is otherwise invisible.
  2. `GET /trickplay/{media_file_id}/why` — a small JSON diagnostic naming, in order, **the first
     reason there are no previews**: feature disabled / ffmpeg absent / file unresolvable / no video
     stream / duration unknown / budget refused (with the numbers) / queued (with queue position) /
     in progress (with progress) / terminal failure (with the reason) / ready. Modelled on spec A's
     `/probe/:id/why` and spec G's MPLY-12 `/why` card, because "why is this not working" is the
     question an operator actually has and answering it costs one route.
     **First reason, not all reasons**: a list of five simultaneous conditions makes the reader guess
     which one matters, and the ordering above is the causal chain.
  3. Log every eviction and every budget refusal at `warn` **with the numbers** (used, budget, free).
     A silent eviction that later causes a regeneration storm is unmaintainable.
  4. Log a single summary line per sweep pass: candidates seen, jobs run, refused, failed. Counts
     only — never a path, never a title, at any level (S1).
  5. The `/why` route is read-only and id-keyed, which is what makes it assistant-operable (Module
     Contract clause 4) without a new mutating tool: "why are there no previews for this" becomes a
     tool call rather than an SSH session.

  ## TEST PLAN
  - `cargo test` — `/why` returns each reason for its synthesised condition, in precedence order
  - `/why` for a ready artifact reports ready with the sheet count and generation timestamp
  - Metrics families appear in `gather_text()` output with the expected names and label sets
  - A refusal and an eviction each emit exactly one `warn` carrying the numbers
  - Log lines contain no filesystem paths and no titles (asserted on captured output)
  - `/why` for an unknown file id is `404`
  - Verify no hardcoded infrastructure values in new/modified files

  ## EDGE CASES
  - Trickplay disabled — `/why` still answers, reporting `feature_disabled` (the most important case
    for the route to handle, since every other surface is inert)
  - A very large library — `maestro_trickplay_pending` is computed from a bounded count query, never
    by enumerating every candidate on each scrape
  - `/why` called during a job — reports in-progress with tile progress rather than a stale state

- **Acceptance criteria:**
  - [ ] All listed metric families are exported with the documented label sets
  - [ ] `/why` names the **first** reason previews are absent, in a documented precedence order, and
        distinguishes disabled / unavailable / refused / queued / failed / ready
  - [ ] Every eviction and refusal is logged at `warn` with the numbers
  - [ ] No path or title appears in any log line at any level (S1)
  - [ ] `maestro_trickplay_pending` is computed without enumerating the whole library
  - [ ] README documents the metrics and the route
  - [ ] No hardcoded infrastructure values in new/modified code; all existing tests still pass

---

### MTRK-14: Operator — choose the volume, run the first sweep, publish the measured cost
- **Priority:** High
- **Labels:** maestro, trickplay, ops
- **Agent:** <operator>
- **Estimate:** 2h operator time (plus unattended sweep wall-clock)
- **Type:** human-action
- **Blocked by:** MTRK-07, MTRK-13
- **Description:** Trickplay is not done when it compiles; it is done when tiles exist and §2's
  estimate has been replaced by a measurement. This is also the item that closes the epic's disk
  risk with evidence rather than a default.
- **Steps:**
  1. **Choose the work-dir volume and record it.** Not the host root filesystem, and explicitly not
     any card-backed LV — the fleet lost one in July 2026 and ran half-missing for three days with
     the symptom presenting as bogus compiler gates and `EIO`. Record which volume, its free space,
     and why it was chosen. Confirm it is not the same LV as any allowed library root.
  2. Apply MTRK-02's `media_markers` migration to the live database through the **`pg_ddl`**
     operator-guarded door, sequenced **with or before** the Muse image swap — migrations are not
     auto-applied (skill v4.6). Extend the `maestro_ro` role's `SELECT` grant to the new table in the
     same action; doing them separately is how TERM #549 happened.
  3. Deploy the sanctioned way: `oci-publish.sh muse moosenet/Muse main muse maestro` →
     `constellation-update.sh --force --skip-idle muse`. **Never a hand-built binary swap** — the
     nightly updater compares OCI digests and reverts one.
  4. Confirm `ffmpeg -version` and `ffprobe -version` on the Maestro host.
  5. **Generate one title first** via `POST /ops/trickplay/generate` and inspect it before enabling
     the sweep: check `/trickplay/{id}/why`, open the manifest, look at a sheet, and confirm the
     tiles are the film and not its cover art (MTRK-05 step 3's failure mode is visually obvious and
     worth eyeballing once).
  6. **Measure.** Record the artifact's bytes and the title's runtime → bytes per hour. Repeat over
     ~10 titles spanning SD/HD/4K and live-action/animation (animation compresses far better and
     will skew a small sample low). Compare against §2's ~1.6 MB/h planning figure.
  7. Enable the sweep at the default rate. Watch `maestro_trickplay_pending` fall and, more
     importantly, watch whether household playback degrades. Raise the rate only if it does not.
  8. **Publish the measurement**: update §2's table with the measured figures, update MTRK-03's
     bytes-per-megapixel constant, and commit the result to `docs/reports/trickplay-cost.md` through
     the normal pipeline. An estimate that is never checked is how a disk-full incident starts.
  9. If the measured whole-library figure exceeds `MAESTRO_TRICKPLAY_BUDGET_MB`, decide deliberately:
     raise the budget, or raise `MAESTRO_TRICKPLAY_INTERVAL_SECS` to 15–20 s (§2's sensitivity table
     shows the trade). Do not leave it to eviction to resolve silently.
 10. Review the terminal-failure list from MTRK-13. Files that never generate are a real
     library-health finding (the same population spec A's `ProbeFailed` surfaces) and belong in a
     follow-up item, not silently in a metric.

---

## 11. What this spec deliberately does not do

- **No detection of anything.** No intro/credit detection, no ad detection, no black-frame or
  silence analysis, no cross-episode fingerprinting, no Chord vision or audio call. Markers are
  consumed and rendered; they are produced by a future **Muse** spec (§9). Epic §2 and §4.
- **No chapter or marker *editing* UI.** MTRK-02 ships an operator HTTP write path sufficient to
  enter a marker for verification. A marker editor is its own spec if it is ever wanted.
- **No hardware-accelerated decode.** Not `-hwaccel`, not VAAPI, not NVDEC. GPU is spec F and is
  arbitrated by Chord; an unannounced GPU decode during a MINT sweep presents as "Chord is slow"
  (epic §10.5).
- **No BIF, no Roku trickplay format, no DASH image-adaptation-set.** One manifest, one sprite
  layout, one consumer. A second output format is a follow-up when a second consumer exists.
- **No I-frame-only playlist / trick-mode playback** (fast-forward *through* the media). That is a
  transcode-tier feature and spec E explicitly defers it too.
- **No storyboard scrubbing for live/linear channels.** The tuner's output has no fixed duration and
  no stable file identity; spec L's serving migration is the right place to revisit it.
- **No re-probe of any file for chapters.** §1 — the flag is already in the shipped argv and the data
  is already in the response. One additional ffprobe invocation exists, for the keyframe index, and
  it is justified in MTRK-06.
- **No trickplay for the `plex` backend.** Epic §8.6: in plex mode no bytes flow through Maestro and
  there is no in-browser player to draw a preview into. `BackendCapabilities` gates the surface, and
  the GUI branches on the capability, never on the backend name.
- **No writes to any media file, and no writes to any Muse library or taste table.** Not a byte.
  Everything this spec produces lives in Maestro's own work dir, except MTRK-01/02/09, which are
  Muse-side by design.
- **No second budget/eviction implementation.** §3 — one `budget_verdict`, shared with spec E.

---

## 12. Risks

1. **Chapters and markers get conflated in the GUI.** They look similar and one of them has a title.
   A chapter named "Opening Titles" producing a Skip Intro button would be a plausible-looking bug
   that silently skips content. MTRK-12's central negative test exists for exactly this, and it is
   the test to check first if a reviewer only has time for one.
2. **The two implementations of the tile arithmetic drift** (MTRK-03 in Rust, MTRK-10 in TypeScript).
   The mitigation is shared fixture values asserted on both sides, and it only works if the fixtures
   stay shared — a reviewer should reject a change to one side's expectations that does not touch the
   other's.
3. **The sweep degrades household playback.** The library is a network-mounted read-only share and a
   decoder walking it is a real load. Mitigated by one job at a time, a 60/hour rate, `nice 15`, and
   MTRK-14 step 7's instruction to watch playback before raising the rate. If previews arrive and
   streaming gets worse, the feature is a net loss.
4. **Forgetting to bump `TRICKPLAY_PARAM_VERSION`** after changing a geometry default, leaving a
   library with mixed-geometry artifacts and previews that are subtly wrong for the older half. The
   manifest also records the full parameter set so a mismatch is *detectable*; the belt-and-braces is
   deliberate because this is the most predictable human error in the spec.
5. **`CONSTELLATION_MAESTRO_TOKEN` or `CONSTELLATION_MUSE_TOKEN` unprovisioned** (epic §10.4, TERM
   #549) — every surface here `401`s and the whole feature looks broken rather than absent. MTRK-13's
   `/why` distinguishes them; provision both before MTRK-14.
6. **The keyframe index becomes a dependency spec E takes on retroactively.** E must stay
   independently shippable (epic §10.1). The index is offered as a pure lookup for E to *adopt*, and
   E's items must not be edited to require it — if E ships first, it ships with its own `-ss`
   behaviour and adopts the index as a later improvement.
7. **`Partial` state is treated as a bug rather than a feature.** It will look like a half-finished
   artifact to a reviewer or an operator. It is the correct behaviour for a resumable job over a
   27 TB library, and both MTRK-03's state enum and MTRK-08's manifest say so explicitly so nobody
   "fixes" it by collapsing it into `Absent`.
