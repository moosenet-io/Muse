# MUSE constellation-web interface — build the full guide
plane_project: MUSE
module: Soma
prefix: MGUI
spec_id: S129-muse-constellation-web-interface

## Metadata
- **Author:** <operator> (Moose)
- **Session:** S129
- **Date:** 2026-07-30
- **Module version:** Muse (web surface in `moosenet/Terminus` `constellation-web`)
- **Estimated total:** ~54h
- **North-Star layer:** module
- **Module-Contract:** meets §4 clauses 1–7 — the surface is Terminus-fronted (every call goes through
  the constellation proxy, never a direct Muse URL from the browser), capability-gated
  (`useMuseSection` degrades per-endpoint), embeddable (a panel in the constellation shell, not a
  bespoke app), and read-only in this spec so it inherits Muse's safe posture. Context-bus
  citizenship (clause 3) is **deferred**: constellation-web has no context bus yet — the panels
  publish nothing, which is the same posture as every other module panel today.
- **Context:** The operator uses the constellation web GUI at the Terminus port on <host> and reports:
  "the pages are still empty", "the taste page is empty and shows Module unavailable", "channels is
  the same, blank cards, zero content", and asks "when do I get the scrollable list of my media".

  Ground truth captured with Playwright against the live deployment (login via operator secret,
  full API trace per page) — this is not inference:

  | Route | API calls observed | Rendered |
  |---|---|---|
  | `/muse/dashboard` | `200 /stats`, `200 /gaps`, `401 /on_deck`, `401 /premiere` | Library size **1892**, Last ingest **4d ago**; On Deck + Premieres show "Module unavailable — HTTP 401" |
  | `/muse/taste` | `401` ×3 on `/api/graph/{taste-clusters,watch-history,group-dynamics}` | all three cards "Module unavailable — HTTP 401" |
  | `/muse/channels` | `200 /api/channels`, `200 /guide` | "No channels yet" / "No guide data yet" — genuinely 0 rows |

  So there are **three independent causes**, and only one of them is a missing panel:
  1. **A missing upstream credential** — the constellation proxy sends no bearer to Muse, so every
     PROTECTED Muse route returns 401 and `useMuseSection` renders "Module unavailable". This is why
     Taste is entirely dark. Tracked as TERM #549 (code merged) + the `CONSTELLATION_MUSE_TOKEN`
     <secret-manager> provisioning, which is an operator action and is **out of scope for this spec**.
  2. **Empty source data** — `channels` has 0 rows, `monitored_items` 0, `trending_snapshots` empty,
     `genres`/`media_metadata_genres` empty, `personas`/`embeddings` empty. Those panels are correct
     and honest; they have nothing to show. Tracked separately (MUSE #88/#90/#91).
  3. **Missing panels — THIS SPEC.** The handoff guide specifies **16 screens**; constellation-web
     registers **3** (`muse.dashboard`, `muse.taste`, `muse.channels`). Muse's backend already
     serves the reads for most of the missing ones, and critically the whole Library surface is on
     Muse's **public** router, so it works with no credential at all. `GET /api/library` returns
     1892 owned / 1629 on disk with real titles and poster URLs **through the proxy today** —
     verified live. The media grid the operator is asking for is unblocked; it simply was never built.

  Reference: `MUSE Interface Guide.dc.html` from the Claude-Design handoff bundle (16 annotated
  screens + the Lumina Constellation design-system tokens). The guide is a prototype in HTML/CSS/JS;
  per its own README the job is to recreate the visual output in the target stack (React +
  constellation tokens), not to port its DOM.

## Pre-flight
- Repository: `moosenet/Terminus`, subtree `constellation-web/` (React + Vite, rust-embedded dist)
- Design source: the handoff bundle's `MUSE Interface Guide.dc.html` + `_ds/**/tokens/*.css`
- Muse read API: already deployed (digest verified live); no Muse-side change required by this spec
- Verification: the Playwright harness on <host> (`/root/gui-shots/shoot-live.mjs`) logs in with the
  operator secret and captures screenshot + API trace + visible text per route
- Baseline: `tsc --noEmit` clean; `vitest` 135 passing (2 pre-existing empty-suite file failures,
  unrelated — verified by stashing)
- **Deploy prerequisite:** `constellation-web/dist` is COMMITTED and rust-embedded, and
  `oci-publish.sh` has no npm step — every panel change MUST rebuild + commit the dist or the
  deployed GUI is unchanged (this bit us in TERM #550; see TERM #551 for the sibling registry trap)

## The gap, screen by screen

`✓` = panel exists · `○` = backend ready, panel missing · `✗` = no backend yet

| # | Guide screen | Muse endpoint | Router | Panel | This spec |
|---|---|---|---|---|---|
| 01 | Dashboard | `/stats` `/gaps` `/on_deck` `/premiere` `/api/subsystems` | mixed | ✓ partial | MGUI-06 adds the subsystem health grid |
| 02 | Library — poster grid | `/api/library` | **public** | ○ | **MGUI-01** |
| 03 | Library — management table | `/api/library/table` | **public** | ○ | **MGUI-02** |
| 04 | Media detail — inspection | `/api/library/:id` | **public** | ○ | **MGUI-03** |
| 05 | Discover | `/api/discover` | **public** | ○ | **MGUI-04** |
| 06 | Request lifecycle | `/api/requests/:id` | protected | ○ | MGUI-08 |
| 07 | Taste engine — profile & radar | `/api/taste` | protected | ✓ wrong source | MGUI-07 rewires to `/api/taste` |
| 08 | Curation — recommendations | `/api/curation` | protected | ○ | MGUI-09 |
| 09 | TV director — programming grid | `/guide` `/api/channels` | public | ✓ table | MGUI-10 |
| 10 | TV director — broadcast console | — | — | ✗ | deferred, no backend |
| 11 | TV director — channel builder | `channels.compose` (seam) | — | ✗ | deferred, seam |
| 12 | Settings — module control | `/api/settings` `/api/subsystems` | mixed | ○ | MGUI-11 |
| 13 | Settings — integrations | `/api/indexers` | protected | ○ | MGUI-12 |
| 14 | Settings — acquisition & safety | `/api/settings` | protected | ○ | MGUI-13 |
| 15 | Assistant | Lumina | — | ✗ | out of scope (Lumina's own surface) |
| 16 | Wanted & acquisition queue | `/api/requests/queue` | protected | ○ | MGUI-14 |

**Phase 1 (MGUI-01..06) needs no credential** — every endpoint is on Muse's public router, so these
panels populate the moment they ship. **Phase 2 (MGUI-07..14) is credential-gated**: the panels are
correct but render "Module unavailable" until `CONSTELLATION_MUSE_TOKEN` exists. Phase 2 items must
therefore be verified against a *proxy-authenticated* fixture or a direct-to-Muse probe, and their
acceptance criteria say so explicitly rather than pretending a 401 is a pass.

## Verification method (mandatory, every item)

No item is complete on "it compiles". Each panel item MUST:
1. Rebuild the dist (`npm run build` with `VITE_AGG_MODE` **unset** so `assert-http-bundle` confirms
   the shipped default is the real-backend adapter).
2. Capture the route with the Playwright harness on <host> — screenshot + the API trace + the visible
   text — against the live deployment.
3. Assert on the **captured text/trace**, not on a screenshot's existence: the panel must show real
   values from the API (e.g. a known title from `/api/library`), and the trace must show `200` for
   the endpoints it needs. A panel that renders its empty state while its endpoint returned `200`
   with rows is a FAILURE, not a pass.
4. Feed the screenshot **and** the guide's corresponding screen description to `review_run` so an
   outside reviewer validates the built page against the design, not just the diff.

---

## Items

### MGUI-01: Library poster grid panel
- **Priority:** Critical
- **Labels:** muse, constellation-web, library
- **Agent:** claude
- **Estimate:** 6h
- **Description:** The operator's headline request — a scrollable poster wall of the media library.
  Guide screen 02: a search field over the whole library, filter chips (Movies / Series / Wanted /
  Unwatched), a taste sort control, a grid⇄table toggle, and poster tiles carrying an availability
  badge and a rating.

  `GET /api/library` is PUBLIC and already returns everything needed — verified live through the
  proxy: `counts{owned:1892,on_disk:1629,wanted:0}` plus per-title `media_item_id`,
  `media_metadata_id`, `kind`, `title`, `year`, `availability`, `monitored`, `poster_url`,
  `backdrop_url`, and provider ids.

  ## FILES
  - `constellation-web/src/panels/muse/LibraryPanel.tsx` — new panel
  - `constellation-web/src/hooks/useMuse.ts` — add `useMuseLibrary(limit)` returning the grid payload
  - `constellation-web/src/panels/registerPanels.ts` — register `muse.library` at `/muse/library`
  - `constellation-web/dist/**` — rebuilt embedded bundle

  ## APPROACH
  1. Add `MuseLibraryItem`/`MuseLibraryResponse` interfaces to `useMuse.ts` mirroring the ACTUAL
     `/api/library` response (owned[], wanted[], counts) — copy the field names from a live capture,
     do not guess them.
  2. Add `useMuseLibrary(limit)` via the existing `useMuseSection` so it inherits per-endpoint
     degradation unchanged.
  3. Build the grid with the constellation design tokens already in `styles/` — CSS grid,
     `auto-fill` with a poster-aspect min column, and a scroll container so the wall scrolls
     independently of the page (the operator asked for a scrollable list; the page body must not be
     the scroller).
  4. Poster `<img src>` via `museArtUrl('media_metadata', item.media_metadata_id)` — the art
     resolver accepts `media_metadata` and `media_item` ONLY (a variant like `poster` is NOT a kind;
     that was TERM #550). `onError` hides the img so a missing poster degrades to the tile's own
     background rather than a broken-image glyph.
  5. Availability badge per the guide's pattern library: `on_disk` → "On disk" (green),
     `monitored` → "Wanted" (blue). Derive from the `availability` field, never re-derive from
     `monitored`.
  6. Client-side search + kind filter chips over the fetched page (server-side search is a
     follow-up; state that in the panel doc rather than implying full-library search).
  7. Grid⇄table toggle switches to the MGUI-02 table when that lands; ship the toggle disabled with
     a title explaining why if MGUI-02 has not merged.

  ## TEST PLAN
  - `npm run typecheck` clean; `npm run build` passes `assert-http-bundle`
  - `vitest` — a test asserting the tile derives its badge from `availability`, and one asserting the
     art URL uses the `media_metadata` kind (regression for TERM #550)
  - **Live Playwright capture of `/muse/library`**: trace shows `200 /api/muse/api/library`, and the
    visible text contains a real title known to be in the library
  - Verify no hardcoded IPs or hostnames in new files

  ## EDGE CASES
  - `/api/library` returns `200` with `owned: []` → the panel's empty state, distinct from a degrade
  - A title with `poster_url` but no cached art → `/art` serves a placeholder; the tile must not
    collapse
  - `year: null` → render the title alone, never "null"
  - 1892 items at once → the panel requests a bounded page (`limit`), and says so in the header count
    rather than implying it rendered everything

- **Acceptance criteria:**
  - [ ] `/muse/library` renders a scrolling poster grid populated from `GET /api/library`
  - [ ] A Playwright capture of the live route shows a real library title in the visible text
  - [ ] Availability badge derives from the `availability` field
  - [ ] Poster URLs use an art kind the resolver accepts (`media_metadata`)
  - [ ] The grid scrolls within its own container, not the page body
  - [ ] Embedded `dist` rebuilt and committed in the same change
  - [ ] No hardcoded infrastructure values in new/modified code
  - [ ] All existing tests still pass

### MGUI-02: Library management table panel
- **Priority:** High
- **Labels:** muse, constellation-web, library
- **Agent:** claude
- **Estimate:** 4h
- **Description:** Guide screen 03 — the dense metadata table: Title, Year, Kind, Quality profile,
  On disk / cutoff (the upgrade signal), Size, Status. Mono figures, status badges.
  `GET /api/library/table` is PUBLIC and already returns this projection.

  ## FILES
  - `constellation-web/src/panels/muse/LibraryTablePanel.tsx` — new panel
  - `constellation-web/src/hooks/useMuse.ts` — `useMuseLibraryTable(limit)`
  - `constellation-web/src/panels/registerPanels.ts` — register `muse.library.table`
  - `constellation-web/dist/**`

  ## APPROACH
  1. Capture a live `/api/library/table` response FIRST and type the interface from it.
  2. Render with the constellation table styling used by existing panels; tabular-nums for
     size/quality figures per the guide's "mono figures" pattern.
  3. On-disk-vs-cutoff is the upgrade signal — render both and mark a row where on-disk is below
     cutoff, per the guide's "Upgrade available" badge.
  4. Wire the grid⇄table toggle in MGUI-01 to this route.
  5. Horizontal overflow scrolls inside the table container.

  ## TEST PLAN
  - typecheck + build + vitest
  - Live Playwright capture: `200` on the table endpoint and a real title in the text
  - Verify no hardcoded infrastructure values

  ## EDGE CASES
  - Null quality profile / size (not yet imported) → em-dash, never "null" or "0 B"
  - Cutoff absent → no upgrade badge rather than a false "meets cutoff"
  - Very long titles → truncate with a title attribute, don't break the row

- **Acceptance criteria:**
  - [ ] `/muse/library/table` renders rows from `GET /api/library/table`
  - [ ] A live capture shows a real title
  - [ ] Upgrade signal shown only when cutoff data actually exists
  - [ ] Embedded `dist` rebuilt and committed
  - [ ] No hardcoded infrastructure values in new/modified code
  - [ ] All existing tests still pass

### MGUI-03: Media detail panel
- **Priority:** High
- **Labels:** muse, constellation-web, library
- **Agent:** claude
- **Estimate:** 6h
- **Description:** Guide screen 04 — the inspection bench: backdrop band, title/year/rating/kind,
  overview, cached enrichment rows, "More like this" vector recall, the match verdict
  (`✓ CONSISTENT` + score + the vision/liveness/runtime reasoning), provider ids, and the on-disk
  file list with taste fit. `GET /api/library/:id` is PUBLIC.

  ## FILES
  - `constellation-web/src/panels/muse/MediaDetailPanel.tsx`
  - `constellation-web/src/hooks/useMuse.ts` — `useMuseMediaDetail(id)`
  - `constellation-web/src/panels/registerPanels.ts`
  - `constellation-web/dist/**`

  ## APPROACH
  1. Capture a live `/api/library/{a real id}` and type from it. Only render sections the payload
     actually carries — a section with no data is omitted, NOT shown with placeholder prose.
  2. Backdrop band via `museArtUrl` with the fanart variant.
  3. Match verdict uses the guide's three states (CONSISTENT / INCONCLUSIVE / INCONSISTENT) driven
     by the real field; if the payload has no verdict, omit the block entirely rather than implying
     an unverified file is consistent. **This is a correctness rule, not styling** — a fabricated
     "consistent" verdict would misrepresent file integrity.
  4. Navigation in from an MGUI-01 tile and an MGUI-02 row.

  ## TEST PLAN
  - typecheck + build + vitest (a test that a missing verdict omits the block)
  - Live Playwright capture of a real title's detail route showing its actual overview text
  - Verify no hardcoded infrastructure values

  ## EDGE CASES
  - Unknown id → the panel's not-found state, not a crash or a blank shell
  - No files on disk (monitored only) → the file list shows its own empty state
  - Missing enrichment (never enriched) → omit the enrichment block
  - No similar titles (vector recall unavailable) → omit "More like this"

- **Acceptance criteria:**
  - [ ] `/muse/library/:id` renders real detail for a real id
  - [ ] A missing match verdict omits the verdict block (never a default "CONSISTENT")
  - [ ] Reachable from both the grid and the table
  - [ ] Embedded `dist` rebuilt and committed
  - [ ] No hardcoded infrastructure values in new/modified code
  - [ ] All existing tests still pass

### MGUI-04: Discover panel
- **Priority:** Medium
- **Labels:** muse, constellation-web, discover
- **Agent:** claude
- **Estimate:** 4h
- **Description:** Guide screen 05 — TMDb trending cards beyond the library, with a request-tier
  badge and a streaming-provider hint. `GET /api/discover` is PUBLIC.

  **Honest note:** `/api/discover` currently returns an essentially empty body (44 bytes live) —
  no trending snapshots have been ingested. This panel will correctly show its empty state until
  the trending worker runs. The item is still worth doing (it is a real screen and the endpoint is
  wired), but its acceptance criteria must NOT claim populated content, and the panel must render
  a *seam* explanation ("no trending snapshot yet") rather than a bare empty box that reads as a bug.

  ## FILES
  - `constellation-web/src/panels/muse/DiscoverPanel.tsx`
  - `constellation-web/src/hooks/useMuse.ts` — `useMuseDiscover(region?)`
  - `constellation-web/src/panels/registerPanels.ts`
  - `constellation-web/dist/**`

  ## APPROACH
  1. Type from a live capture (even an empty one — confirm the envelope shape).
  2. Trending/Popular/New chips map to the endpoint's own parameters if it takes them; otherwise
     render only the tabs the API supports and omit the rest rather than shipping dead chips.
  3. The "Request →" CTA is a WRITE path — it is **out of scope for this spec** (read-only posture).
     Render it disabled with an explanatory title; do not wire a grab.
  4. Empty state names the reason (no snapshot ingested yet) and points at the worker.

  ## TEST PLAN
  - typecheck + build + vitest
  - Live capture: `200` on `/api/discover`; assert the panel shows its seam empty state (this is the
    correct outcome today) and NOT a spurious "Module unavailable"
  - Verify no hardcoded infrastructure values

  ## EDGE CASES
  - Empty snapshot (today's reality) → seam state naming the cause
  - TMDb unconfigured → the endpoint's own degrade, surfaced honestly
  - A title already owned → no request CTA

- **Acceptance criteria:**
  - [ ] `/muse/discover` renders without error against the live endpoint
  - [ ] With no snapshot, shows a seam state naming the cause (NOT "Module unavailable", NOT a blank card)
  - [ ] The request CTA is inert and visibly disabled (no write path in this spec)
  - [ ] Embedded `dist` rebuilt and committed
  - [ ] No hardcoded infrastructure values in new/modified code
  - [ ] All existing tests still pass

### MGUI-05: Muse module rail — register the new panels and group them
- **Priority:** High
- **Labels:** muse, constellation-web, navigation
- **Agent:** claude
- **Estimate:** 2h
- **Description:** With Library/Table/Detail/Discover added, the Muse rail grows from 3 entries to
  7+. Group them the way the guide's shell does (Dashboard · Library · Discover · Taste · Channels ·
  Settings) so the rail stays legible, and make Library the module's landing panel — the operator
  goes to Muse to look at media.

  ## FILES
  - `constellation-web/src/panels/registerPanels.ts` — ordering + icons + landing panel
  - `constellation-web/dist/**`

  ## APPROACH
  1. Order the Muse panels to match the guide's tab order; Library first after Dashboard.
  2. Keep panel ids stable (`muse.library`, `muse.library.table`, …) — they are the data namespace.
  3. Sub-panels (table, detail) do not each need a rail entry — reach the table via the grid's
     toggle and detail via a tile, per the guide.

  ## TEST PLAN
  - typecheck + build
  - Live capture of `/muse/dashboard` showing the new rail entries in the visible text
  - Verify no hardcoded infrastructure values

  ## EDGE CASES
  - A panel whose backend is unreachable still appears in the rail (availability is per-endpoint,
    not per-rail-entry) — do not hide a tab because one call 401s

- **Acceptance criteria:**
  - [ ] The Muse rail lists the new panels in guide order
  - [ ] Detail/table are reachable without their own rail entries
  - [ ] Embedded `dist` rebuilt and committed
  - [ ] All existing tests still pass

### MGUI-06: Dashboard subsystem health grid
- **Priority:** Medium
- **Labels:** muse, constellation-web, dashboard
- **Agent:** claude
- **Estimate:** 4h
- **Description:** Guide screen 01 shows a "Subsystem health · 18 modules" grid (`src module →
  concern`, with a wiring-status badge per subsystem) and an activity/maintenance log. Muse serves
  `GET /api/subsystems` PUBLICLY and it returns real data (1362 bytes live). The current dashboard
  does not render it — the guide's most information-dense panel is missing.

  ## FILES
  - `constellation-web/src/panels/muse/DashboardPanel.tsx` — add the health grid section
  - `constellation-web/src/hooks/useMuse.ts` — `useMuseSubsystems()`
  - `constellation-web/dist/**`

  ## APPROACH
  1. Type from a live `/api/subsystems` capture.
  2. Render the guide's wiring-status vocabulary — Live / Worker / Seam / Unmounted — from the real
     state field. Do NOT invent a state for a subsystem the payload doesn't classify; show it as
     unclassified.
  3. Place it below the existing stat tiles so the working `/stats` content stays above the fold.

  ## TEST PLAN
  - typecheck + build + vitest (a test that an unknown state renders as unclassified, not as "Live")
  - Live capture: `200 /api/subsystems` and real subsystem names in the visible text
  - Verify no hardcoded infrastructure values

  ## EDGE CASES
  - A subsystem with no concern label → render the module name alone
  - An unrecognized state string → unclassified, never defaulted to Live
  - Endpoint degrades → this section degrades alone, leaving the stat tiles intact

- **Acceptance criteria:**
  - [ ] The dashboard renders a subsystem health grid from `GET /api/subsystems`
  - [ ] A live capture shows real subsystem names
  - [ ] An unrecognized state is shown as unclassified, never as Live
  - [ ] Embedded `dist` rebuilt and committed
  - [ ] No hardcoded infrastructure values in new/modified code
  - [ ] All existing tests still pass

---

## Phase 2 — credential-gated panels (MGUI-07..14)

Every item below targets a PROTECTED Muse route. They are implementable and reviewable now, but they
**cannot be verified as populated** through the proxy until `CONSTELLATION_MUSE_TOKEN` is provisioned
in <secret-manager> (an operator action; TERM #549 shipped the proxy-side injection). Each item's
acceptance criteria therefore require verification against a direct-to-Muse authenticated probe
(proving the panel binds the real shape) **plus** a proxy capture showing the honest degrade — and
explicitly forbid claiming the panel is populated end-to-end until the token exists.

### MGUI-07: Rewire the Taste panel to `/api/taste`
- **Priority:** High
- **Labels:** muse, constellation-web, taste
- **Agent:** claude
- **Estimate:** 5h
- **Description:** Guide screen 07 is the taste PROFILE — genre-lean bars, context centroids, and
  the you-vs-the-masses divergence radar — served by `GET /api/taste` (the MUSE-10/13 taste model +
  radar). The current TastePanel instead calls `/api/graph/{taste-clusters,watch-history,
  group-dynamics}`, which are the household-analytics/KG endpoints, and one of them
  (`taste-clusters`) can never populate here because `personas`/`embeddings` are empty (MUSE #88).
  Rewire the panel to its intended source and keep the graph cards as a secondary section.

  ## FILES
  - `constellation-web/src/panels/muse/TastePanel.tsx`
  - `constellation-web/src/hooks/useMuse.ts` — `useMuseTaste()`
  - `constellation-web/dist/**`

  ## APPROACH
  1. Capture `/api/taste` with a direct authenticated request to Muse and type from it.
  2. Genre-lean bars + context centroids + divergence radar per the guide, reusing the existing viz
     primitives (`viz/`) rather than new chart code.
  3. Keep the seam banner the guide shows for taste (`taste_model.recompute` has no scheduled
     caller) when the payload indicates a stale/manual compute.
  4. Leave the household-analytics cards (watch-history / group-dynamics) as a secondary section —
     they carry real data once the token lands.

  ## TEST PLAN
  - typecheck + build + vitest
  - Direct authenticated probe of `/api/taste` proving the bound shape matches
  - Proxy capture showing the honest degrade while the token is absent
  - Verify no hardcoded infrastructure values

  ## EDGE CASES
  - Taste profile never computed → seam banner, not an empty radar implying zero divergence
  - Divergence absent while genre-lean present → render what exists, omit the radar

- **Acceptance criteria:**
  - [ ] TastePanel binds `GET /api/taste` for the profile/radar sections
  - [ ] Shape verified against a direct authenticated Muse response
  - [ ] A never-computed profile shows the seam banner, not a zeroed radar
  - [ ] Does NOT claim populated end-to-end rendering while `CONSTELLATION_MUSE_TOKEN` is unset
  - [ ] Embedded `dist` rebuilt and committed
  - [ ] All existing tests still pass

### MGUI-08: Request lifecycle panel
- **Priority:** Low
- **Labels:** muse, constellation-web, requests
- **Agent:** claude
- **Estimate:** 4h
- **Description:** Guide screen 06 — the lifecycle stepper (search → decide → grab), the dual
  safety-gate toggles shown READ-ONLY, and the `decide_release` winner with its deterministic
  scoring. `GET /api/requests/:id`, protected.

  ## APPROACH
  1. Type from a direct authenticated capture.
  2. The dual-gate toggles are **display-only** in this spec — they represent a live grab path and
     must not be operable from a read-only panel. Render current state, no control.
  3. Scoring breakdown rendered from the real decision payload; never a synthesized score.

  ## TEST PLAN / EDGE CASES / acceptance criteria
  - As MGUI-07's pattern: typecheck + build + vitest; direct probe for shape; proxy degrade capture
  - Gates render as state, never as controls
  - **Acceptance criteria:**
    - [ ] Lifecycle + gates + decision render from `GET /api/requests/:id`
    - [ ] Safety-gate toggles are non-interactive
    - [ ] Shape verified against a direct authenticated response
    - [ ] Embedded `dist` rebuilt and committed
    - [ ] All existing tests still pass

### MGUI-09: Curation panel
- **Priority:** Medium
- **Labels:** muse, constellation-web, curation
- **Agent:** claude
- **Estimate:** 4h
- **Description:** Guide screen 08 — ranked recommendation rows with on-deck/gap/taste tags, the
  rationale copy, and a taste-fit score. `GET /api/curation`, protected.

  ## APPROACH
  1. Type from a direct authenticated capture.
  2. Render the rationale VERBATIM from the payload — the guide's "rationale copy" is grounded
     narration produced server-side; the panel must not paraphrase or embellish it.
  3. Filter chips (All / On-deck / Gaps / Taste) map to the endpoint's own parameters.

  - **Acceptance criteria:**
    - [ ] Rows render from `GET /api/curation` with tag, rationale and fit score
    - [ ] Rationale text is rendered verbatim, never rewritten client-side
    - [ ] Shape verified against a direct authenticated response
    - [ ] Embedded `dist` rebuilt and committed
    - [ ] All existing tests still pass

### MGUI-10: Channels programming grid
- **Priority:** Medium
- **Labels:** muse, constellation-web, channels
- **Agent:** claude
- **Estimate:** 5h
- **Description:** Guide screen 09 — a channels × time grid with proportional program blocks, a now
  marker, and tuner telemetry. The existing ChannelsPanel renders a table and says so ("not an EPG
  grid — spec §5.4"); the guide specifies the grid. `/guide` + `/api/channels` are public.

  **Honest note:** `channels` has **0 rows** live, so this grid will show its empty state until
  channels are composed (`channels.compose` has no HTTP route — guide screen 11's seam). Build the
  grid, but the acceptance criteria must not claim populated programming.

  ## APPROACH
  1. Proportional blocks from the guide payload's start/end times; a now marker from the client clock.
  2. Keep the table as a secondary view rather than deleting it — it is useful and already correct.
  3. Empty state names the cause (no channels composed; compose has no route yet).

  - **Acceptance criteria:**
    - [ ] `/muse/channels` renders a channels × time grid from `/guide`
    - [ ] With 0 channels, shows an empty state naming the cause (not a blank card)
    - [ ] The existing table remains reachable
    - [ ] Embedded `dist` rebuilt and committed
    - [ ] All existing tests still pass

### MGUI-11: Settings — module control
- **Priority:** Low
- **Labels:** muse, constellation-web, settings
- **Agent:** claude
- **Estimate:** 4h
- **Description:** Guide screen 12 — the module registry with wiring status and concern labels per
  subsystem. Reads `/api/subsystems` (public) + `/api/settings` (protected).
  **Toggles are display-only in this spec** — `/api/settings` has a PUT, but exposing writes is a
  separate, operator-gated change; a read-only panel must not ship live switches.

  - **Acceptance criteria:**
    - [ ] Module registry renders with per-subsystem wiring status
    - [ ] Enable toggles render as state, NOT as operable controls
    - [ ] Embedded `dist` rebuilt and committed
    - [ ] All existing tests still pass

### MGUI-12: Settings — integrations & connections
- **Priority:** Low
- **Labels:** muse, constellation-web, settings
- **Agent:** claude
- **Estimate:** 3h
- **Description:** Guide screen 13 — connection rows with env-var provenance and
  connected/not-configured state. The guide's own caption is load-bearing: "secrets ← <secret-manager> ·
  never authored here". The panel shows the env var NAME and the connection state only.

  ## APPROACH
  1. Render the variable NAME and a connected/not-configured state. **Never render a secret value,
     never a masked prefix, never a length hint** — a masked value still leaks shape.
  2. Source from `/api/indexers` + `/api/settings`; a provider absent from the payload is
     "not configured", never "disconnected" (they differ).

  - **Acceptance criteria:**
    - [ ] Connection rows show env-var names and state
    - [ ] No secret value, mask, or length hint is ever rendered
    - [ ] Absent provider reads "not configured", not "disconnected"
    - [ ] Embedded `dist` rebuilt and committed
    - [ ] All existing tests still pass

### MGUI-13: Settings — acquisition & safety
- **Priority:** Low
- **Labels:** muse, constellation-web, settings, safety
- **Agent:** claude
- **Estimate:** 4h
- **Description:** Guide screen 14 — the dual safety gate (shown OFF, its current safe posture), the
  circuit breaker, quality profiles, and the scored custom-formats blocklist.

  ## APPROACH
  1. Render both gates as STATE with the guide's explanation ("Both off → requests are persisted for
     review but never actioned").
  2. **The gates are not operable from this panel.** They are the dual safety gate for a real-world
     write path with blast radius; flipping them belongs behind an explicit operator-gated control,
     not a read-only settings view. Say so in the panel.

  - **Acceptance criteria:**
    - [ ] Dual gate renders as state with its posture explained
    - [ ] Neither gate is operable from this panel
    - [ ] Quality profiles and blocklist render from the real payload
    - [ ] Embedded `dist` rebuilt and committed
    - [ ] All existing tests still pass

### MGUI-14: Wanted & acquisition queue
- **Priority:** Medium
- **Labels:** muse, constellation-web, requests
- **Agent:** claude
- **Estimate:** 4h
- **Description:** Guide screen 16 — the monitored/wanted list, the qBittorrent download queue with
  progress, and the typed `history_events` log. `GET /api/requests/queue`, protected.

  **Honest note:** `monitored_items` has 0 monitored rows live, so the wanted list will be empty
  until something is monitored.

  - **Acceptance criteria:**
    - [ ] Wanted list, download queue and history render from `GET /api/requests/queue`
    - [ ] Shape verified against a direct authenticated response
    - [ ] Empty wanted list shows an empty state, not a degrade
    - [ ] Embedded `dist` rebuilt and committed
    - [ ] All existing tests still pass

---

## Deliberately out of scope

- **Guide screen 10 (broadcast console)** and **11 (channel builder)** — no HTTP backend exists
  (`channels.compose` is an unmounted seam). Building UI for them would be a mock, which is exactly
  the failure mode this spec exists to end.
- **Guide screen 15 (Assistant)** — the conversational surface belongs to Lumina's own module, not a
  duplicate inside Muse's panels.
- **Every write path** — the request CTA, settings toggles, safety gates, compose/publish. This spec
  is read-only; Muse's write path stays behind its dual safety gate (MUSEM-05).
- **`CONSTELLATION_MUSE_TOKEN` provisioning** — an operator action in <secret-manager>. It gates Phase 2's
  end-to-end population and is tracked outside this spec.
