# S130-G — Maestro: the constellation-web Player section
plane_project: MUSE
module: Muse
prefix: MPLY
spec_id: S130-G-maestro-player-gui

## Metadata
- **Author:** Moose
- **Session:** S130
- **Date:** 2026-08-01
- **Module version:** Maestro v0.1 (GUI surface in `moosenet/Terminus` `constellation-web`)
- **Estimated total:** ~66h (**G1** ~24h on B · **G2** ~42h on D/E)
- **North-Star layer:** module
- **Module-Contract:** meets §4 clauses 1, 2, 4, 5, 6, 7 — the surface is Terminus-fronted for its
  **control plane** (session start, transport, status, plan, capabilities all go through
  `proxy_maestro`), capability-gated (the
  `maestro` module binds to a health entry and renders inert when absent), assistant-operable (the
  actions this panel performs are the same `PlaybackBackend` operations spec B exposes as tools —
  this panel adds no capability the assistant cannot reach), embeddable (panels in the existing
  constellation shell, under the Muse core), sovereign (no telemetry; Chromecast is opt-in and
  local-network only) and standalone-excellent (it is genuinely useful against the `plex` backend
  before the native engine exists — epic §1). **Clause 3 (context bus) is deferred**: constellation-web
  has no context bus yet, the same posture S129 recorded. What is *not* deferred is the underlying
  data flow — playback progress reaches Muse's tracker via Maestro (epic §2), so "what is being
  watched" is durable even though the browser publishes nothing.

  **Clause 1 carve-out — the media plane does not traverse Terminus (epic §8.6), in native mode.**
  Control goes through the gateway; **media bytes are served direct from Maestro over signed,
  session-scoped, expiring URLs** (spec D, epic §8.7). Under the `plex` backend there is no media
  plane here at all — Plex serves its own bytes to its own devices and Maestro only drives them. This is a deliberate, documented carve-out, not a leak:
  routing sustained video through the tool-hub process would couple playback uptime to Terminus
  restarts, trading away the exact crash isolation this epic exists to buy. It is also forced —
  `<video>` and native Safari cannot set an `Authorization` header on segment fetches, and a Cast
  receiver holds no Terminus cookie. **The discipline that keeps it honest: the browser never
  *composes* a Maestro URL; it only plays back one that a control-plane response handed it.**
- **Context:** The Constellation can browse its media beautifully (S129 shipped the poster wall, the
  management table and the inspection bench) and cannot play any of it. Epic §5 records the reason
  plainly: **no `<video>` element, HLS library, or player component exists anywhere in
  constellation-web.** This spec builds the first one.

  This spec is the **G** child of `S130-maestro-epic.md`, and the epic's §4/§8.6 rewrite **inverted
  its internal ordering**. The obvious plan — ship a `<video>` against the `plex` backend, add HLS
  later — does not work, and the reason is stronger than a sequencing accident.

  **Epic §8.6 (revised): `plex` mode is control-and-observe only. No bytes flow through Maestro, and
  there is no in-browser `<video>` playback of Plex content at all.** An earlier draft had Maestro
  reverse-proxying Plex's stream so it was always the data plane; **that was withdrawn**, because a
  Plex byte-proxy means re-streaming `transcode/universal/*` output that is undocumented,
  token-lifecycle-bound, keepalive-sensitive and changes without notice — weeks of brittle work
  spent polishing the very backend the strangler fig exists to replace. Bytes arrive with the
  **native** engine, and only with it.

  **This is a permanent property of the plex backend, not a not-yet-built state**, and the
  distinction is the whole reason the capability descriptor exists. Under plex,
  `in_browser_stream` is **false and always will be**; it becomes true when the native engine
  serves the session, not when spec D merges. A panel that tells the operator "in-browser playback
  is coming soon" under a plex backend is misinforming them about their own system — see §4b and
  MPLY-09, where the wording is an acceptance criterion rather than a nicety.

  What B *does* unlock is the thing the household actually has today: **remote control and
  now-playing** through the existing `CastController` seam (epic §5 — its doc comment already
  anticipates a non-Plex implementation; §8.6 generalises it into the `DeviceControl` facet). So
  this spec ships as the two units the epic names, **G1** and **G2**:

  | Unit | Depends on | Delivers | Items |
  |---|---|---|---|
  | **G1 — remote control + now-playing** | **B** | browse/select, the "playing on" target picker, transport control of a real device, `/why` diagnostics, the whole degradation seam | MPLY-01, 02, 09, 12 |
  | **G2 — in-browser playback** | **D** (+ E for HLS/subtitles) | the `<video>` player, controls, tracks, keyboard, session lifecycle, Cast sender | MPLY-03..08, 10, 11 |

  G1 is genuinely useful on its own: pick something on the poster wall, send it to the living-room
  TV, drive it, and see why it is transcoding. That is a real product against the backend that
  exists today, and it is deliverable without one line of transcode or delivery code. It is also
  the *only* form the Plex-backed household will ever have — G2 is what the native engine unlocks.

  Dependencies beyond that: **C** for the `/why` reasons, **E** for WebVTT sidecars and HLS,
  **K** for the Cast receiver and App ID (see MPLY-10/11). It owns no server code — every file it
  touches is under `constellation-web/` in `moosenet/Terminus`.

  **Maestro is a second `[[bin]]` in the existing `moosenet/Muse` repo, not a new repo** (epic §2 —
  crash isolation is a *process* boundary, bought with two systemd units and two cgroups, not a
  repository boundary). Two consequences matter to this spec and are carried into §4 and §7 below:
  the DTOs this panel binds are **one Rust definition** shared across both bins rather than two
  schemas kept in sync by hand (§4), and Muse and Maestro **deploy together, all-or-nothing, from one
  OCI image** — so a panel needing both a new Maestro endpoint and a new Muse endpoint never faces a
  backend version-skew window (§6).

---

## 1. The dependency decision: hls.js (ADD, lazily, pinned)

The repo has a documented minimal-dependency posture — no Tailwind, adherence machine-checked, the
design system hand-rolled from tokens. Adding a runtime library needs a stated rationale, so here it
is, with the alternative that was rejected.

**Decision: add `hls.js`, pinned to an exact version, loaded via dynamic `import()` so it is a
separate chunk that is fetched ONLY when a playback plan actually says `hls`.**

1. **The browsers we own cannot play HLS natively.** `HTMLMediaElement.canPlayType(
   'application/vnd.apple.mpegurl')` is truthy on Safari/iOS and falsy on Chrome, Edge and Firefox.
   The operator's browser is one of the latter. Spec E delivers transcoded content as HLS, so
   without a Media Source Extensions client, every transcode-tier item is unplayable in the
   household's actual browser — which is the tier where "it won't play" hurts most.
2. **The build-it-ourselves alternative is not a small one.** An MSE segment loader has to handle
   manifest parsing and live reload, segment fetch/append scheduling, seeking across segment
   boundaries, discontinuities, codec initialisation-segment switching, buffer eviction under
   memory pressure, and error recovery on a stalled append. That is a spec-sized project in its own
   right, and epic §10.1 already names spec E's seek/segment work as the likeliest overrun. Writing
   a second one in TypeScript would be a strictly worse use of the same risk budget.
3. **Licence is clean.** hls.js is Apache-2.0 — compatible with the MIT posture of every
   Constellation repo and safe for the public mirrors. Epic §7.2 forbids GPL code entering any repo;
   this is not that, and it is a dependency rather than copied code either way.
4. **The precedent already exists, and this is cheaper than it.** constellation-web ships six
   `@nivo/*` packages plus `recharts` for charting. The posture is "dependencies are justified, not
   forbidden", and a media-transport library for the one thing the browser genuinely cannot do is
   an easier case than a second charting library.
5. **The cost is bounded by construction.** Static import would put ~200 KB into the shell's main
   chunk for every user on every route, including the ~everyone who only ever direct-plays.
   `await import('hls.js')` inside the HLS branch means the chunk is requested only when a plan
   returns `hls`, and never at all on a direct-play or remux item — the majority tier per epic §6.
6. **Pinned, not floated.** An exact version (no `^`), so a transitive bump can never arrive
   unreviewed in a `npm ci`. Bumping it is a reviewed change like any other dependency move.

**Fallbacks, in order, so the library is never a single point of failure:** native HLS first (if
`canPlayType` is truthy, hand the manifest straight to `<video>` and never load the library at all —
Safari/iOS get the leaner path); then hls.js; and if the dynamic import itself fails (offline chunk,
CSP, a broken build), the player renders the MPLY-12 diagnostics panel explaining that this item
requires HLS and the HLS client could not load — a legible failure, never a black box.

**What is NOT added:** no DASH client (nothing in the epic produces DASH), no `video.js`/`shaka`
(both are much larger and bring their own UI/abstraction layers we would fight), no player-UI
framework — the controls are ours, built from the design tokens like every other surface.

---

## 2. Reuse vs new — stated explicitly, because "do not rebuild the Library" is a requirement

The browse surface is a **new panel that composes existing parts**, not a fork of `LibraryPanel`.

**Reused unchanged (import, do not copy):**

| Thing | Where it lives | Used for |
|---|---|---|
| `PanelRoot` | `src/components/PanelRoot.tsx` | every panel's scroll frame (`hf-scroll`, `min-height:0`) |
| `Card` / `Badge` / `StatusPill` / `EmptyState` / `SkeletonList` / `DataTable` / `Button` / `Toolbar` | `src/components/` | all chrome; no new primitives |
| `ChartCard` | `src/viz/ChartCard.tsx` | the loading / degraded / empty tri-state that S129's panels render through |
| `cardGridTemplate`, `CATALOG_TRACK_BASE`, `fluidBodyHeight` | `src/lib/catalogLayout.ts` | grid track sizing and fluid body height |
| `CardSizeSlider` / `useCardSize` + the `museCardSize` pref | `src/panels/muse/CardSizeSlider.tsx`, `aggregationClient` prefs allowlist | the card-density control, **shared state with the Library wall** — one slider position means the same thing on every catalog surface |
| `museArtUrlAt('media_metadata', id, 160)` | `src/hooks/useMuse.ts` | poster art at a rendition width; the master is ~1.9 MB (MUSE #100) |
| `useMuseSection` per-endpoint degradation | `src/hooks/useMuse.ts` | the pattern `useMaestroSection` mirrors |
| `registerPanel` / `registerModule` / `parentId` / `wide` / `hideInRail` | `src/lib/moduleRegistry.ts` | registration and rail nesting |
| `LibraryPanel` itself | `src/panels/muse/LibraryPanel.tsx` | reached by a **link**, unchanged — Library remains the place to browse everything |

**New, and why each cannot be the Library panel:**

- **Continue-watching / resume rows.** Row-oriented, ordered by recency of a *session*, showing a
  progress bar and a remaining-time figure per tile. `/api/library` projects no watched state at all
  (MUSE #101 — this is exactly why the Library wall has no "Unwatched" chip). The data comes from
  Maestro's resume endpoint, a different source with a different shape.
- **A play-intent grid.** Same poster tiles, but every tile's action is "play this now" rather than
  "inspect this". A tile whose item has no playable file is visibly not-playable here, which would
  be wrong on the Library wall (an item you do not have on disk is a perfectly valid Library row).
- **The player itself.** No analogue exists.

**Explicitly not duplicated:** no second search implementation, no second filter-chip vocabulary, no
second grid⇄table toggle written from scratch, no second card-size preference key. Where the browse
surface needs a chip row or a toggle, it uses the same chip styling and the same `View` shape as
`LibraryPanel`, and MPLY-02 extracts them to a shared module rather than copying them — the review
gate should reject a copy-paste of `LibraryPanel`'s chip/sort block.

---

## 3. Where this lives in the shell (a decision, with the precedent it follows)

Maestro registers as its own **module** (`ModuleId: 'maestro'`) whose panels render as a sub-group
of the **Muse core**, exactly as `models` and `mint` render under the Terminus core in
`src/lib/cores.ts`. It does **not** become a sixth top-level core tab.

Why: `CORE_MEMBERS` already has the one-core-many-modules shape, and the top strip is the five real
constellation members. A playback engine for Muse's library is a subsystem of the media experience,
not a peer of Lumina and Chord. Making it a module rather than a handful of `muse` panels is what
buys the health gating — `ModuleDescriptor.healthSystem` is the mechanism that makes the panels
disappear cleanly when Maestro is absent, and a `muse`-owned panel would inherit Muse's health and
render broken instead.

Concretely this widens three unions in MPLY-01: `ModuleId`, `ModuleDescriptor['healthSystem']`, and
`aggregationClient`'s `SystemId`. All three are additive.

---

## 4. The Maestro API contract this spec binds

**These shapes are ASSUMED from epic §3 and the B/C/D child specs, and MUST be reconciled against a
live capture before implementation.** S129's hardest-won rule applies unchanged: *type the interface
from a captured response, never from this table.* A field named here that Maestro does not emit is a
bug in this document, and the correct response is to fix the panel to the real shape and note the
divergence — not to make the panel render a field that does not exist.

**There is exactly one authority for these shapes.** Because Maestro is a second bin in the Muse
crate (epic §2), the session / plan / target / resume DTOs are `serde` types living under
`src/maestro/` in `moosenet/Muse`, sharing `models/` with Muse itself — one definition, not a Muse
schema and a Maestro schema kept in step by hand. So a TypeScript interface here is transcribed from
**one** Rust type, and when the panel and the payload disagree there is a single place to look. Each
interface added by this spec carries a comment naming the Rust type it mirrors, so the next reader
can check it in one hop instead of inferring the contract from a capture.

| Purpose | Assumed route (through `/api/maestro/…`) | Owned by | Phase |
|---|---|---|---|
| Health probe | `GET /health` | B | 1 |
| **Backend capabilities** | `GET /backends` → per-backend `BackendCapabilities` (below) | B | 1 |
| Transport targets | `GET /playback/targets` → `[{id, name, kind, available}]` | B | 1 |
| Drive a remote target | `POST /playback/targets/:id/{play,pause,seek,stop}` | B | 1 |
| Now playing (remote) | `GET /playback/sessions` → active sessions with `muse_item_id`, position, plan | B | 1 |
| Playback plan (the `/why` payload) | `GET /playback/plan?muse_item_id=&profile=` → `{method, container, video, audio, reasons[], speed}` | C | 1 |
| Resume / continue watching | `GET /playback/resume` → `{muse_item_id, position_secs, duration_secs, updated_at}` | D | 2 |
| Start a session | `POST /playback/sessions` → `{session_id, account_id, stream_url (SIGNED), expires_at, method, position_secs, media:{…}, plan}` | D | 2 |
| Heartbeat progress | `POST /playback/sessions/:id/progress` `{position_secs, paused, playback_rate}` | D | 2 |
| Stop | `DELETE /playback/sessions/:id` | D | 2 |
| Subtitle sidecar | `GET /playback/sessions/:id/subtitles/:track` → WebVTT | E | 2 |

**Three rules govern how this panel consumes the above. Each is an acceptance criterion somewhere
below, because each is a structural guarantee rather than a style preference.**

**(a) Maestro payloads carry `muse_item_id` only — never a title, never a poster, never a year.**
Metadata and artwork come from **Muse** through `proxy_muse` (`useMuse` hooks, `museArtUrlAt`);
session, plan and transport state come from **Maestro** through `proxy_maestro`. The panel composes
the two by id. This is not a data-fetching preference — **it is the structural enforcement of epic
§2's ownership split.** The moment a Maestro payload carries a title, Maestro has a metadata cache,
and the dual-ownership failure the epic forbids has arrived through the GUI's back door. A reviewer
who sees a title read off a Maestro response should reject the item. Encode it in
`aggregationClient.ts` as a stated convention beside the `SystemId` union: *`maestro` responses are
ids and playback state; `muse` responses are what things are called.*

**(b) `BackendCapabilities` decides what renders — and the GUI branches on capabilities, NEVER on
backend name.** Epic §8.6 splits the one trait into three facets — `MediaSource` (native yes, plex
**no**), `DeviceControl` (plex yes, native later), `SessionSource` (both) — precisely so the GUI
never grows `if backend === 'plex'`. A backend-name check is a reviewable defect in this spec: it
bakes today's asymmetry into the client and breaks the moment a third backend appears. Branch on
`in_browser_stream` / `device_cast` / `can_report_transcode_detail`, and the panel stays correct
for backends that do not exist yet.

**Absent data is marked absent, never zeroed.**
Spec B exposes `in_browser_stream`, `device_cast`, `server_side_transcode_decision`,
`seek_during_transcode`, `syncplay` and `can_report_transcode_detail` per backend. The plex backend
cannot report transcode detail. **A panel that renders `0 fps` or `0×` for a figure the backend never
reported is lying**, and it is the specific kind of lie that gets believed because it looks like
data. Where a capability is false, the affected control or figure renders as *"not reported by this
backend"* — a distinct visual state from both zero and degraded. This applies to the `/why` card
(MPLY-12) and the target picker (MPLY-09).

**(c) `account_id` is Muse's account id (epic §8.1, corrected) — NOT the cookie session.** The
cookie session carries *roles* (`operator|viewer`), not household members. The proxy maps the session
to a configured default Muse account, and the id in every session payload lives in the same id-space
the taste model uses. So when this panel labels "who is watching", it labels a **Muse account**, and
it must never derive that label from the logged-in shell user — those are different things, and
conflating them would attribute one household member's viewing to another in the taste model.

**The device profile.** Every plan/session request carries a `DeviceProfile` (spec C) describing what
*this browser* can decode. It is built once from `MediaSource.isTypeSupported` /
`canPlayType` probes at module load — measured, never a hardcoded table, because the same build runs
in Safari and Chrome and they differ on exactly the codecs that matter (HEVC, AC-3 passthrough,
native HLS). For a **remote** target the profile is the *target's*, resolved server-side from the
backend — the browser must never send its own profile as though it described the living-room TV.

---

## 5. Verification method (mandatory, every UI item)

Per S129, with two additions the video element forces.

1. Rebuild the dist: `npm run build` with **`VITE_AGG_MODE` unset**, so `assert-http-bundle`
   confirms the shipped default is the real-backend adapter. **Commit `constellation-web/dist/`.**
2. `npm run lint:adherence` — no new warnings attributable to the changed files.
3. Capture the live route with the Playwright harness on <host>: screenshot **plus** the API trace
   **plus** the visible text.
4. Assert on the captured text and trace, never on a screenshot's existence. A panel rendering its
   empty state while its endpoint returned `200` with rows is a FAILURE.
5. **New — assert on media-element state, not on pixels.** A `<video>` screenshot is a black
   rectangle whether it is working or not. The harness asserts on `readyState`, `networkState`,
   `duration`, `buffered.length` and an advancing `currentTime`, read out of the page — plus the
   session trace showing a `POST /playback/sessions` and at least one heartbeat.
6. **New — the harness cannot prove decoding, and must not claim to.** Headless Chromium's
   open-source build ships without the proprietary decoders (H.264/AAC) that Chrome has. So a
   harness run may legitimately report a `MEDIA_ERR_SRC_NOT_SUPPORTED` on a file the operator's real
   browser plays perfectly. Where that happens, the item's evidence is: the session/trace assertions
   above (which prove the wiring), **plus** an explicit operator confirmation in a real browser
   (MPLY-11's sibling check). An item that reports "playback verified" from a headless run that
   never decoded a frame is a false pass and will be treated as one.

---

## 6. Sequencing — the skew boundary that exists, and the one that does not

**The backend has no internal skew window.** `muse` and `maestro` are two bins in one OCI image
(epic §2): `oci-publish.sh muse moosenet/Muse main muse maestro` packages both, and `OCI_INSTALL`
deploys both all-or-nothing with a shared rollback. So an item here that needs a new Maestro endpoint
*and* a new Muse endpoint gets them in a single deploy, from one merge, gated by one review pipeline.
There is no "Maestro is ahead of Muse" state to defend against, and no item in this spec should carry
compatibility code for one.

**The boundary that IS real is the GUI's.** constellation-web ships inside the **Terminus** image,
which deploys on its own cadence. So the ordering that matters is: **backend first, panel second.**
A panel merged before its endpoint exists renders the honest degrade (`404`/`501` ⇒ *not yet wired*,
per MPLY-01) rather than breaking — which is the correct behaviour and the reason the degradation
seam is item one. What is NOT acceptable is shipping a panel that *claims* a populated state it has
never been shown, which is the failure S129 spent a whole spec correcting.

**G1 (B): MPLY-01, 02, 09, 12.** The data seam, the browse surface, the target picker with remote
transport, and the `/why` card. All four are deliverable against the plex backend with no delivery
code anywhere, and together they are a usable product: choose something, send it to a device, drive
it, and see why it is transcoding. **For a Plex-only household this is the finished feature**, not a
staging post — G2 needs the native engine, not merely more time.

**G2 (D, then E): MPLY-03, 05, 08 → 04, 06 → 07, 10, 11.** In-browser video and everything that
hangs off it. MPLY-04 additionally needs E for a real HLS source and MPLY-06 needs E's sidecars.
MPLY-10's sender needs **K**'s receiver + App ID to be verified end-to-end, and ships dark until then.

If a dependency slips, ship the panel with its degrade **proven by capture** and say so in the PR
rather than holding the whole section — but do not ship a G2 panel that pretends to be G1 usable. An
in-browser player with no native backend to serve it is not a degraded player, it is an absent
feature, and MPLY-01's capability gate is what should be hiding it.

---

## 7. Pre-flight

- Repository this spec changes: `moosenet/Terminus`, subtree `constellation-web/` (React 18 + TS +
  Vite, `include_dir!`-embedded, **`dist/` committed**). **No change to `moosenet/Muse` is authored
  here.**
- Backend it binds to: the `maestro` bin in `moosenet/Muse` (`src/maestro/`, epic §2), deployed as
  the second binary of the existing `muse` OCI module. There is no `moosenet/Maestro` repo — do not
  create one, and do not look for the DTOs anywhere but the Muse tree.
- **Spec B must have landed** `proxy_maestro` (bearer injection) and a `maestro` entry in
  `/api/health`. Until then MPLY-01 ships the client seam + mock fixtures and every panel degrades
  honestly; **do not invent a second route to Maestro** — no direct URL, no new proxy arm authored
  here (epic §7.9, single sanctioned doors).
- **`CONSTELLATION_MAESTRO_TOKEN` provisioned in <secret-manager>** — an operator action, epic §11. Its
  absence repeats TERM #549: protected routes 401 and the panels look broken rather than absent.
  This spec does not provision it and must not hardcode a stopgap.
- Baseline before starting: `npm run typecheck` clean, `npm run test` (vitest) green, and the
  current warning count from `npm run lint:adherence` recorded so a regression is visible.
- Playwright harness on <host> (`/root/gui-shots/`) reachable and logging in with the operator secret.

---

## Items

### MPLY-01: Maestro data seam — module registration, health gating, and the client arm
- **Priority:** Critical
- **Labels:** maestro, constellation-web, foundation
- **Agent:** claude
- **Estimate:** 6h
- **Phase:** G1 (B)
- **Blocked by:** spec B (`proxy_maestro`, the `maestro` health entry, `GET /backends`)
- **Description:** Everything else in this spec sits on this item. It adds `maestro` as a first-class
  module in the shell, binds its availability to a health entry, fetches `BackendCapabilities`, and
  adds the single typed client arm
  through which every later panel talks to Maestro. No user-visible panel lands here — the deliverable
  is that `maestro` is a registered, health-gated, zero-panel module, which is a valid state (the same
  one `muse` occupied between CONST-19 and CONST-20).

  It also establishes the **two-proxy composition convention** (§4a) that every later item obeys:
  `maestro` answers with ids and playback state, `muse` answers with what things are called. Getting
  that stated in one place, in the client, is what stops it being re-litigated per panel.

  This is also the item that establishes **degradation** for the whole section: with Maestro absent,
  `getAvailableModules` omits the module, so the rail group and its panels simply are not there —
  registered but not rendered. Reaching a player route directly by URL in that state must land on an
  honest "Maestro is not available" card, never a blank shell or a crash.

  ## FILES
  - `constellation-web/src/lib/moduleRegistry.ts` — widen `ModuleId` and `ModuleDescriptor.healthSystem` with `'maestro'`
  - `constellation-web/src/lib/cores.ts` — `CORE_MEMBERS.muse = ['muse', 'maestro']`; `MEMBER_LABEL.maestro`
  - `constellation-web/src/lib/aggregationClient.ts` — widen `SystemId` with `'maestro'`; mock health entry; `MOCK_GET` fixtures for the Maestro routes
  - `constellation-web/src/hooks/useMaestro.ts` — new: `useMaestroSection<T>`, `useMaestroHealth()`, `useMaestroCapabilities()`, the `DeviceProfile` probe
  - `constellation-web/src/panels/registerPanels.ts` — `registerModule({ id: 'maestro', … })`
  - `constellation-web/README.md` — document the module and the new client arm
  - `constellation-web/dist/**`

  ## APPROACH
  1. Widen the three unions. All additive — no existing panel, route or health consumer changes.
  2. `registerModule({ id: 'maestro', title: 'Player', icon: '▶', healthSystem: 'maestro', order: 4 })`.
     Order places it directly after `muse` so the Muse core reads Muse → Maestro.
  3. `useMaestroSection` is a near-copy of `useMuseSection`'s **contract**, not its body: extract the
     shared generic into one place if that is clean, otherwise implement it thinly and say why. It
     must reproduce the same semantics — `404`/`501` ⇒ `not yet wired`, any other error ⇒ the error
     detail, a `null` mock resolution ⇒ the not-wired sentinel, never a throw.
  4. `DeviceProfile` is **measured**: `MediaSource.isTypeSupported` for the mp4/HEVC/AV1 codec
     strings and `video.canPlayType('application/vnd.apple.mpegurl')` for native HLS, evaluated once
     at module scope and memoised. Never a hardcoded per-browser table.
  5. Mock fixtures for every route in §4, shaped from spec B/D's handlers. A mock that disagrees
     with the real response is worse than no mock (the `MOCK_MUSE_CHANNELS` lesson in
     `aggregationClient.ts`) — each fixture carries a comment naming its source.
  6. `useMaestroCapabilities()` exposes `BackendCapabilities` as a typed record with an explicit
     **unknown** state (capabilities not yet loaded ≠ capability false). A consumer must be able to
     distinguish "this backend cannot do it" from "we do not know yet"; collapsing the two is how a
     panel ends up confidently disabling a control that works.
  7. Nothing in this item constructs a Maestro URL outside `aggregationClient`. Add the §4a
     convention as a comment beside the `SystemId` union so the next panel author reads it before
     reaching for a title on a Maestro response.

  ## TEST PLAN
  - `npm run typecheck`; `npm run build` passes `assert-http-bundle`; `npm run lint:adherence`
  - vitest: `getAvailableModules` **omits** `maestro` when its health entry is `available:false`, and
    includes it when true
  - vitest: `useMaestroSection` maps `404` to `not yet wired` and a `500` to its error detail
  - vitest: `modulesInCore('muse', …)` returns `[muse, maestro]` in that order
  - Live Playwright capture of `/muse/dashboard`: with Maestro absent, no Maestro rail entry appears
    and no console error is raised
  - Verify no hardcoded IPs, hostnames or tokens in new/modified files

  ## EDGE CASES
  - `/api/health` has no `maestro` entry at all (spec B not deployed) → treated identically to
    `available:false`; module absent, nothing logs an error
  - Health flaps → App.tsx's existing 2-cycle grace applies unchanged; do not add a second grace
  - `proxy_maestro` returns `401` (token unprovisioned) → the section degrades with the real detail,
    and MPLY-12's diagnostics name the credential as the likely cause rather than blaming the media

- **Acceptance criteria:**
  - [ ] `maestro` is a registered module rendering as a sub-group of the Muse core
  - [ ] With its health entry absent or `available:false`, the module and its panels do not render — and no error is logged
  - [ ] `useMaestroSection` reproduces `useMuseSection`'s degradation semantics, proven by unit test
  - [ ] `BackendCapabilities` is exposed with a three-state (true / false / not-yet-known) shape, proven by unit test
  - [ ] The `DeviceProfile` is probed from the live browser, never a hardcoded table
  - [ ] No Maestro URL is constructed outside `aggregationClient.ts`
  - [ ] The two-proxy composition convention (§4a) is stated in `aggregationClient.ts` beside `SystemId`
  - [ ] README documents the module, its client arm and the two-proxy convention
  - [ ] Embedded `dist` rebuilt and committed in the same change
  - [ ] No hardcoded infrastructure values in new/modified code; all existing tests still pass

### MPLY-02: Play browse surface — continue-watching rows over a play-intent grid
- **Priority:** Critical
- **Labels:** maestro, constellation-web, browse
- **Agent:** claude
- **Estimate:** 7h
- **Phase:** G1 (B) — the grid; the resume region activates with D
- **Blocked by:** MPLY-01
- **Description:** `/muse/play` — the surface you open when you want to watch something rather than
  look at your collection. Two regions: **Continue watching** across the top (resume rows from
  Maestro, each with a progress bar and a remaining-time figure), and below it a poster grid of
  playable items whose tiles lead to the player rather than the inspection bench.

  **The two regions land in different phases and that is fine.** The grid needs only Muse's library
  reads and ships in G1. `GET /playback/resume` is spec D, so until D lands the resume region
  renders nothing at all — not an empty shelf, not a spinner. Its absence is invisible rather than
  broken, which is exactly the behaviour MPLY-01's degradation seam exists to give it.

  **This panel is the clearest instance of the two-proxy composition rule (§4a).** A resume row is a
  Maestro `muse_item_id` plus a position; its title, year and poster are fetched from **Muse**. The
  panel joins them by id. It must never read a title off a Maestro payload even if one appears there
  — an unexpected title field is a spec-B/D bug to report, not a convenience to consume.

  This deliberately does not duplicate the Library. It reuses `LibraryPanel`'s poster-tile shape,
  chip styling, grid⇄table toggle, `cardGridTemplate` tracks and the shared `museCardSize`
  preference — so the two surfaces feel like one system and the density slider means one thing
  everywhere. What is new is the resume region (Library has no watched state — MUSE #101) and the
  tile's intent (play, not inspect). A link to the full Library sits in the header for "I actually
  want to browse".

  ## FILES
  - `constellation-web/src/panels/maestro/PlayBrowsePanel.tsx` — new
  - `constellation-web/src/panels/maestro/ContinueWatchingRow.tsx` — new
  - `constellation-web/src/panels/muse/catalogChrome.tsx` — new: the chip/sort/toggle block extracted from `LibraryPanel` and imported by both (no copy-paste)
  - `constellation-web/src/hooks/useMaestro.ts` — `useMaestroResume()`
  - `constellation-web/src/panels/registerPanels.ts` — register `maestro.play` (`wide: true`)
  - `constellation-web/dist/**`

  ## APPROACH
  1. Capture `GET /playback/resume` live and type from it. If Maestro is not yet deployed, type from
     the Rust DTO in `src/maestro/` (§4) and mark the interface with a comment saying it is
     transcribed, not captured — the same honesty `useMuse.ts` applies to the request-lifecycle
     shapes. Expect `muse_item_id` and position fields ONLY; resolve the title/art through the
     existing `useMuse` hooks keyed on that id.
  2. Extract the chip/sort/view-toggle block out of `LibraryPanel` into `catalogChrome.tsx` and have
     `LibraryPanel` import it. This is a refactor with no visual change; the existing
     `LibraryPanel.test.ts` must still pass untouched. **A reviewer seeing a second copy of that
     block should reject the item.**
  3. Resume rows: horizontal scroller of wider (16:9 backdrop) tiles, each with a progress bar from
     `position/duration` and a remaining-time label. Art via `museArtUrlAt` at a rendition width.
  4. The grid reuses `cardGridTemplate(cardSize, CATALOG_TRACK_BASE.poster)` and `useCardSize`, so it
     shares the operator's stored density with the Library wall.
  5. A tile links to `/muse/play/:id`. A tile whose item has **no playable file** renders visibly
     inert with a title explaining why, and links to the Library detail bench instead — an item you
     do not have is not an error, it is just not playable.
  6. Body height via `fluidBodyHeight`; the grid scrolls in its own container, never the page body.

  ## TEST PLAN
  - typecheck + build + `lint:adherence`; existing `LibraryPanel.test.ts` passes with no edits
  - vitest: a resume row with `position >= duration` does not render (a finished item is not "continue watching")
  - vitest: an item with no playable file renders inert and does not link to the player
  - Live capture of `/muse/play`: trace shows `200` on the resume + library reads, and the visible
    text contains a real title from the library
  - Verify no hardcoded infrastructure values

  ## EDGE CASES
  - Zero resume rows (nothing watched yet) → the region is omitted entirely rather than shown as an
    empty shelf; the grid moves up
  - `duration` null/0 on a resume row → show the title without a progress bar, never a divide-by-zero bar
  - Resume endpoint degrades while the grid succeeds → only the resume region degrades (per-endpoint rule)
  - A library larger than the page cap → the header states the cap, as `librarySubtitle` already does

- **Acceptance criteria:**
  - [ ] `/muse/play` renders a play-intent poster grid from live Muse endpoints (G1)
  - [ ] Resume rows render once `GET /playback/resume` exists; before that the region is absent, not empty-or-spinning
  - [ ] Every title, year and poster on this panel comes from a `muse` response; no title is read from a `maestro` payload
  - [ ] The chip/sort/toggle chrome is imported from one shared module by both this panel and `LibraryPanel` — no duplicated block
  - [ ] The card-density slider shares the `museCardSize` preference with the Library wall
  - [ ] A finished item does not appear in continue-watching; an unplayable item is inert, not a dead link
  - [ ] A live capture shows a real library title in the visible text
  - [ ] Embedded `dist` rebuilt and committed
  - [ ] No hardcoded infrastructure values in new/modified code; all existing tests still pass

### MPLY-03: The player shell — `<video>`, direct play, and the session-backed source
- **Priority:** Critical
- **Labels:** maestro, constellation-web, player
- **Agent:** claude
- **Estimate:** 8h
- **Phase:** G2 (D)
- **Blocked by:** MPLY-01; **spec D** (session model, native delivery, signed URLs) — and a **native** backend at runtime: plex mode never serves a browser (epic §8.6)
- **Description:** The Constellation's first `<video>` element. `/muse/play/:id` starts a Maestro
  session, binds the returned `stream_url` to a media element, and plays. No library, no HLS, no
  controls beyond the browser's own in this item — direct play and progressive/remux sources only,
  which per epic §6 is the majority tier and the one that needs nothing but an `src` and working
  range requests.

  Shipping this alone is a genuine milestone: an item that direct-plays is playable in the browser
  from this item forward, against the `plex` backend, with no transcoding anywhere.

  ## FILES
  - `constellation-web/src/panels/maestro/PlayerPanel.tsx` — new
  - `constellation-web/src/panels/maestro/useVideoElement.ts` — new: the media-element state hook
  - `constellation-web/src/hooks/useMaestro.ts` — `useMaestroSession(itemId)`
  - `constellation-web/src/panels/registerPanels.ts` — register `maestro.player` at `/muse/play/:id` (`hideInRail: true`, `wide: true`)
  - `constellation-web/README.md` — document the player surface and the media-URL exemption below
  - `constellation-web/dist/**`

  ## APPROACH
  1. `useParams()` for the item id, exactly as `MediaDetailPanel` does.
  2. On mount, start a session (MPLY-08 owns the lifecycle; this item calls it and renders). Bind
     `stream_url` to `<video src>`, `preload="metadata"`, `playsInline`, no `autoplay` attribute —
     autoplay policy makes an unmuted autoplay a promise rejection, so play is user-initiated and a
     rejected `play()` surfaces as a "press play" state rather than a silent nothing.
  3. `useVideoElement` centralises the element's observable state (`readyState`, `paused`,
     `currentTime`, `duration`, `buffered`, `error`) behind one hook so MPLY-05's controls and
     MPLY-08's heartbeat read one source of truth instead of each attaching their own listeners.
  4. **The media-URL exemption, documented in code and README.** `aggregationClient` remains the only
     module that calls `fetch`. `<video src>` and `<track src>` are not `fetch` — they are the
     browser's media stack, which is the only thing that can do ranged media transport, and this was
     always going to be true of any player. The discipline that keeps the rule meaningful: **every
     URL handed to a media element comes from an aggregationClient response** (the session's signed
     `stream_url`) or from an existing same-origin path helper (`museArtUrl`, the precedent already
     in `useMuse.ts`) — never composed from a host, port or scheme in panel code. Add this exemption
     as a comment beside the README's fetch rule so the next grep-audit reads it as a decision.
  4b. **The stream URL is signed, session-scoped and EXPIRING (epic §8.7), and is NOT same-origin.**
     Media is served direct from Maestro, deliberately bypassing the Terminus gateway so playback
     uptime is not coupled to Terminus restarts. Three consequences the implementer must handle:
     the URL is opaque — never parse, rewrite, log or cache it; it can **expire mid-playback**, so a
     `403`/`401` on a segment or range request means *re-mint the session*, not "the file is gone";
     and it must never be persisted anywhere (no prefs, no localStorage, no URL bar). Treat it as a
     short-lived credential, because that is exactly what it is.
  5. An `error` event on the element renders MPLY-12's diagnostics card in place of the video —
     never a black rectangle.

  ## TEST PLAN
  - typecheck + build + `lint:adherence`
  - vitest: `useVideoElement` derives `buffered` ranges and a `MEDIA_ERR_*` code into the shapes the
    controls and diagnostics consume
  - vitest: a session response with no `stream_url` renders the diagnostics card, not an empty `<video>`
  - Live capture of `/muse/play/:id` for a known direct-play item: trace shows `POST /playback/sessions`
    → `200`, the page reports `readyState >= 1` and a non-zero `duration`
  - Where the headless harness cannot decode the codec (§5.6), record the `MEDIA_ERR` and the session
    trace, and state plainly that decoding was confirmed by the operator rather than by the harness
  - Verify no hardcoded infrastructure values

  ## EDGE CASES
  - Session start returns `4xx`/`5xx` → diagnostics card naming the status; no `<video>` mounted
  - `play()` promise rejects (autoplay policy) → a "press play" affordance, never a silent stall
  - Unknown item id → the panel's not-found state, matching `MediaDetailPanel`'s convention
  - The element stalls (`waiting` with no progress) → a buffering indicator, distinct from an error
  - A second player mounted while one is open → the first session is stopped before the second starts

- **Acceptance criteria:**
  - [ ] `/muse/play/:id` starts a Maestro session and binds its `stream_url` to a `<video>` element
  - [ ] A direct-play item reaches `readyState >= 1` with a real `duration` in a live capture
  - [ ] A failed session or a media error renders the diagnostics card, never a black box
  - [ ] An expired signed URL re-mints the session rather than reporting the media as missing, proven by unit test
  - [ ] The signed `stream_url` is never persisted, logged or parsed
  - [ ] No media URL is composed from a host/port/scheme in panel code; the exemption is documented in README
  - [ ] `fetch` still appears only in `aggregationClient.ts` (plus the pre-existing exceptions)
  - [ ] Embedded `dist` rebuilt and committed
  - [ ] No hardcoded infrastructure values in new/modified code; all existing tests still pass

### MPLY-04: HLS source adapter — native first, hls.js lazily, honest failure
- **Priority:** High
- **Labels:** maestro, constellation-web, player, dependency
- **Agent:** claude
- **Estimate:** 6h
- **Phase:** G2 (D + E)
- **Blocked by:** MPLY-03; spec E (for a real HLS source to test against)
- **Description:** Adds the HLS delivery path beside the direct-play one, implementing §1's decision.
  The player picks a source strategy from the session's `method`/`container`: direct/progressive →
  plain `src` (MPLY-03, unchanged); HLS → native if the browser has it, else a dynamically imported
  hls.js attached to the same element.

  ## FILES
  - `constellation-web/package.json` / `package-lock.json` — add `hls.js` at an EXACT pinned version
  - `constellation-web/src/panels/maestro/sourceStrategy.ts` — new: the pure `plan → strategy` function
  - `constellation-web/src/panels/maestro/attachHls.ts` — new: the lazy import + attach/detach
  - `constellation-web/src/panels/maestro/PlayerPanel.tsx` — use the strategy
  - `constellation-web/README.md` — record the dependency and its rationale
  - `constellation-web/dist/**`

  ## APPROACH
  1. `chooseStrategy(plan, deviceProfile)` is a **pure function** with unit tests — `'progressive' |
     'native-hls' | 'hlsjs' | 'unsupported'`. Every downstream "it won't play" bug will present as a
     player bug and be a strategy bug, so it is tested in isolation (epic §7.3's discipline applied
     to the client).
  2. `attachHls` does `const Hls = (await import('hls.js')).default` **inside the branch**, so the
     chunk is never requested on a direct-play item. Verify that in the build output, not by
     assertion: the item's evidence includes the emitted chunk list showing hls.js in its own chunk.
  3. Configure hls.js with same-origin relative manifest URLs only (the session's `stream_url`), the
     default loader, and `xhrSetup` adding no credentials beyond the cookie session the shell already
     has. No custom loader, no CDN, no worker URL from another origin.
  4. Detach and `destroy()` on unmount and on source change — a leaked instance keeps fetching
     segments and keeps a session alive after navigation, which is the exact failure MPLY-08 exists
     to prevent.
  5. `'unsupported'` (or a failed dynamic import) → MPLY-12's diagnostics, naming the reason:
     "this item needs HLS; this browser has no native HLS and the HLS client did not load".
  6. Pin exactly; no caret. Record the version and the Apache-2.0 licence in README.

  ## TEST PLAN
  - typecheck + build + `lint:adherence`
  - vitest for `chooseStrategy`: progressive-mp4 → `progressive`; HLS + native-capable → `native-hls`;
    HLS + not-native-capable → `hlsjs`; a codec the profile rejects → `unsupported`
  - vitest: `attachHls` failing to import resolves to the unsupported path rather than throwing
  - Build output shows hls.js in a SEPARATE chunk (paste the chunk list in the PR)
  - Live capture of a direct-play item: the network trace contains **no** hls.js chunk request
  - Live capture of an HLS item (once spec E can serve one): manifest + segment requests, `buffered.length > 0`
  - Verify no hardcoded infrastructure values

  ## EDGE CASES
  - Safari (native HLS) → hls.js is never loaded at all; assert this rather than assuming it
  - Manifest `404` (session expired mid-load) → diagnostics naming the status, and the session is stopped
  - `hls.js` fatal error (network/media) → one recovery attempt per its documented pattern, then diagnostics; never a silent retry loop
  - Source changes while attached (target switch, MPLY-09) → previous instance destroyed first

- **Acceptance criteria:**
  - [ ] `chooseStrategy` is pure and unit-tested across progressive / native-HLS / hls.js / unsupported
  - [ ] hls.js is pinned to an exact version and emitted as a separate chunk
  - [ ] A direct-play capture shows no hls.js chunk being requested
  - [ ] A browser with native HLS never loads the library
  - [ ] An unsupported item or a failed import renders diagnostics explaining why, not a black box
  - [ ] README records the dependency, its version and its rationale
  - [ ] Embedded `dist` rebuilt and committed
  - [ ] No hardcoded infrastructure values in new/modified code; all existing tests still pass

### MPLY-05: Transport controls — play/pause, scrub with buffered indicator, volume, speed, fullscreen
- **Priority:** Critical
- **Labels:** maestro, constellation-web, player, controls
- **Agent:** claude
- **Estimate:** 7h
- **Phase:** G2 (D)
- **Blocked by:** MPLY-03; reuses MPLY-09's `TransportRow`
- **Description:** The custom control bar, built from the design tokens like every other surface —
  `controls` on the element is not used, because it cannot show a buffered range, a cast target, a
  subtitle picker or our own keyboard model, and it looks nothing like the shell.

  Includes: play/pause, a scrub bar showing **played** and **buffered** as distinct ranges, elapsed
  and remaining time in mono figures, volume with mute, playback speed (0.5–2×), and fullscreen.

  ## FILES
  - `constellation-web/src/panels/maestro/PlayerControls.tsx` — new
  - `constellation-web/src/panels/maestro/ScrubBar.tsx` — new
  - `constellation-web/src/panels/maestro/formatTime.ts` — new (pure, tested)
  - `constellation-web/src/panels/maestro/PlayerPanel.tsx` — compose the bar
  - `constellation-web/dist/**`

  ## APPROACH
  1. Buffered ranges come from `TimeRanges`, which is a *list* of ranges, not one — render every
     range. After a seek there are typically two, and drawing only `buffered.end(0)` misreports the
     buffer as far smaller than it is (a classic and very visible bug).
  2. Scrub: pointer drag updates a local preview position and only commits `currentTime` on release,
     so dragging does not fire a seek per pointer-move. The bar is a real `<input type="range">`
     under the styling so keyboard and screen readers work without reimplementing either.
  3. Volume persists via the existing prefs seam (`PrefsClient`) — that means **adding a key to the
     allowlist**, which is deliberate and reviewed, exactly as `museCardSize` was. Never write an
     unallowlisted key.
  4. Speed via `playbackRate`, snapped to a fixed step list. Speed is reported in the session
     heartbeat (MPLY-08) so progress accounting is not silently wrong at 2×.
  5. Fullscreen via the Fullscreen API on the player container (not the raw element, so the controls
     remain composited over it); reflect state from `fullscreenchange` rather than assuming the
     request succeeded.
  6. Controls auto-hide on inactivity during playback and reappear on pointer move or any key —
     and are **always** visible when paused.
  7. All colours/sizes from tokens; `lint:adherence` must not gain warnings.

  ## TEST PLAN
  - typecheck + build + `lint:adherence` (no new warnings)
  - vitest: `formatTime` for 0, sub-minute, hour-plus, `NaN`/`Infinity` duration (live stream) → a
    stable placeholder, never "NaN:NaN"
  - vitest: a two-range `TimeRanges` renders two buffered segments
  - vitest: a scrub drag commits exactly one `currentTime` write on release
  - Live capture: the control bar is present in the DOM with real elapsed/duration text
  - Verify no hardcoded infrastructure values

  ## EDGE CASES
  - `duration` is `Infinity` (a live/unbounded transcode) → the scrub bar renders as unseekable with a
    reason, rather than a bar that cannot be dragged anywhere
  - Seeking beyond `buffered` on a transcode → the backend's seek behaviour (spec E) governs; the UI
    shows buffering, not an error
  - Volume slider on a device with fixed volume (iOS) → the control is hidden rather than shown inert
  - `requestFullscreen` rejects (permissions/iframe) → the button reverts, no thrown error

- **Acceptance criteria:**
  - [ ] Play/pause, scrub, volume, speed and fullscreen all operate the media element
  - [ ] The scrub bar renders EVERY buffered range, proven by a unit test with two ranges
  - [ ] An `Infinity` duration renders an explained unseekable state, never "NaN"
  - [ ] Volume persists through the prefs seam with its key explicitly allowlisted
  - [ ] `npm run lint:adherence` gains no new warnings from these files
  - [ ] Embedded `dist` rebuilt and committed
  - [ ] No hardcoded infrastructure values in new/modified code; all existing tests still pass

### MPLY-06: Audio and subtitle track selection, including WebVTT sidecars
- **Priority:** High
- **Labels:** maestro, constellation-web, player, subtitles
- **Agent:** claude
- **Estimate:** 6h
- **Phase:** G2 (E)
- **Blocked by:** MPLY-05; spec E (sidecar extraction)
- **Description:** Track pickers in the control bar. Audio tracks come from the session payload (and,
  for hls.js, from its own track API); subtitles come from three places that must be presented as one
  list: WebVTT sidecars Maestro extracts (spec E), text tracks embedded in an HLS manifest, and "off".

  **The correctness rule is honesty about what the browser can render.** A browser can render WebVTT
  and (in an HLS manifest) CEA-608/708. It cannot render PGS/VOBSUB image subtitles or ASS styling.
  A track that needs burn-in must say so and be offered only if Maestro can serve it — never listed
  as selectable and then silently do nothing when picked.

  ## FILES
  - `constellation-web/src/panels/maestro/TrackMenu.tsx` — new
  - `constellation-web/src/panels/maestro/tracks.ts` — new: pure merge/normalise of the track sources
  - `constellation-web/src/panels/maestro/PlayerPanel.tsx` — mount `<track>` elements for sidecars
  - `constellation-web/src/panels/maestro/PlayerControls.tsx` — the two menus
  - `constellation-web/dist/**`

  ## APPROACH
  1. `mergeTracks(session, hlsTracks)` is pure and tested: normalises to `{id, kind, label, lang,
     format, selectable, reason?}` and de-duplicates a track that appears in both sources.
  2. Sidecars mount as `<track kind="subtitles" src={sidecarUrl} srclang label default={false}>` with
     the URL taken from the session payload (§4) — same media-URL discipline as MPLY-03.
  3. Selection drives `textTracks[i].mode = 'showing' | 'disabled'`; exactly one showing at a time,
     and "Off" is always present and is the default unless a stored preference says otherwise.
  4. Audio-track switching: HLS via hls.js's audio-track API; for a progressive source with multiple
     audio tracks the browser generally exposes none, so the selector offers the **server-side**
     switch (a new session with a different audio track, spec D) and says that is what it does — a
     switch that restarts the stream is fine; a switch that appears to work and does not is not.
   5. Preferred language persists through the prefs seam (allowlisted key), applied on session start.
  6. Track labels come from the payload verbatim — never re-derived or prettified from a language code
     the server already labelled.

  ## TEST PLAN
  - typecheck + build + `lint:adherence`
  - vitest: `mergeTracks` de-duplicates a track present in both sources; a PGS track is `selectable:false` with a reason
  - vitest: selecting a subtitle sets exactly one `textTrack` to `showing`
  - vitest: "Off" is always present and selecting it disables all tracks
  - Live capture with a sidecar-bearing item: the `<track>` src request returns `200 text/vtt` and the
    cue text appears in the captured page text
  - Verify no hardcoded infrastructure values

  ## EDGE CASES
  - No subtitle tracks at all → the menu is omitted, not shown empty
  - Sidecar fetch `404`s → that track goes unselectable with the reason; other tracks unaffected
  - An HLS audio-track switch mid-playback → position is preserved across the switch
  - A track with a null/blank label → fall back to the language code, then to "Track N"; never blank
  - Multiple `showing` tracks (a browser default) → normalised to one on mount

- **Acceptance criteria:**
  - [ ] Audio and subtitle menus render from the merged, normalised track list
  - [ ] A WebVTT sidecar renders cues in a live capture
  - [ ] A track the browser cannot render is shown unselectable with a reason, never silently inert
  - [ ] "Off" always exists and disables every text track
  - [ ] Preferred subtitle language persists via an allowlisted prefs key
  - [ ] Embedded `dist` rebuilt and committed
  - [ ] No hardcoded infrastructure values in new/modified code; all existing tests still pass

### MPLY-07: Keyboard shortcuts and player accessibility
- **Priority:** Medium
- **Labels:** maestro, constellation-web, player, a11y
- **Agent:** claude
- **Estimate:** 4h
- **Phase:** G2 (D)
- **Blocked by:** MPLY-05
- **Description:** Space/K play-pause, ←/→ seek 10s, J/L seek 10s, ↑/↓ volume, M mute, F fullscreen,
  C subtitles toggle, 0–9 seek to percentage, Esc exit fullscreen. Plus the accessibility work that
  makes the custom control bar as usable as the native one it replaced.

  **Scoping is the whole risk here.** `LibraryPanel` carries a hard-won note: it tried to bind `/`
  and lost to the shell's global handler, and stealing a shell-wide key for one panel would have been
  wrong even if it had won. So these bindings attach to the **player container**, not `window`, and
  never fire while focus is in a text input.

  ## FILES
  - `constellation-web/src/panels/maestro/useKeyboardShortcuts.ts` — new
  - `constellation-web/src/panels/maestro/PlayerPanel.tsx` — container focus management
  - `constellation-web/src/panels/maestro/PlayerControls.tsx` — ARIA + focus-visible
  - `constellation-web/README.md` — document the shortcut table
  - `constellation-web/dist/**`

  ## APPROACH
  1. Bind on the container with `tabIndex={0}`, focused on mount so the shortcuts work without a
     click. Check `event.target` against inputs/textareas/`contentEditable` and bail.
  2. Cross-check the whole table against the shell's existing bindings (`App.tsx`, the command
     palette) before implementing, and drop or remap any collision — do not shadow a global key.
  3. Every control has an `aria-label` that reflects its *current* state ("Pause", not "Play/Pause"),
     the scrub bar is a real range input with `aria-valuetext` as a time, and the menus are proper
     listboxes with arrow-key navigation.
  4. A visible, token-styled focus ring on every control; never `outline: none` without a replacement.
  5. Announce state changes that have no visual affordance (subtitles on/off, speed) via a polite
     live region.

  ## TEST PLAN
  - typecheck + build + `lint:adherence`
  - vitest: each binding calls its action; none fire when the event target is an input
  - vitest: the binding table contains no key already claimed by the shell (assert against the
    registered command list so the test fails if the shell claims one later)
  - Live capture: tab order reaches every control; `aria-label`s appear in the accessibility snapshot
  - Verify no hardcoded infrastructure values

  ## EDGE CASES
  - Focus is in the search box of another panel → no player key fires
  - A modifier is held (Ctrl/Cmd/Alt) → the browser's own shortcut wins; we do not intercept
  - Fullscreen → the container keeps focus so keys keep working (the classic fullscreen dead-keys bug)
  - Repeat-held arrow key → seeks are coalesced rather than firing one per repeat event

- **Acceptance criteria:**
  - [ ] The documented shortcuts operate the player when it has focus
  - [ ] No shortcut fires while focus is in a text input, proven by unit test
  - [ ] No binding collides with a shell-global key, proven by a test against the registered commands
  - [ ] Every control is keyboard reachable with a state-accurate `aria-label` and a visible focus ring
  - [ ] README documents the shortcut table
  - [ ] Embedded `dist` rebuilt and committed
  - [ ] No hardcoded infrastructure values in new/modified code; all existing tests still pass

### MPLY-08: Session lifecycle — start, heartbeat, resume, and a guaranteed clean stop
- **Priority:** Critical
- **Labels:** maestro, constellation-web, player, session
- **Agent:** claude
- **Estimate:** 8h
- **Phase:** G2 (D)
- **Blocked by:** MPLY-03; spec D (session model)
- **Description:** The wiring that makes playback *count*: a session starts on open, heartbeats
  position while playing, resumes from the stored position, and — the part that is easy to get
  wrong — **stops cleanly on every exit path**, both a route change inside the SPA and a tab
  close/reload. A session that is never stopped holds a transcode process, pins a device slot, and
  leaves a stale row in the Server Activity panel (spec H) forever.

  Progress reaching Muse's tracker is what feeds taste (epic §2), so a dropped stop is not a cosmetic
  leak — it corrupts the watch state the recommendations sit on.

  **This item also owns auto-resume across a backend restart (epic §2c.3), and that is not a nicety.**
  `muse` and `maestro` ship in one OCI image, so **a Muse-only hotfix restarts `maestro.service` and
  would otherwise kill a film at minute 90** — a failure mode the separate-repo model did not have,
  and the real price of the same-repo decision. The epic pays it with three mitigations; **this is
  the client half.** A player that re-establishes its session at the last reported position turns a
  restart into a two-second blip, and it is most of the rollback story for free: the epic's
  kill-switch is `systemctl stop maestro`, and a client that reconnects cleanly is what makes that
  cheap enough to actually use. It also covers the far more common case of a flaky network, which is
  the same code path.

  ## FILES
  - `constellation-web/src/panels/maestro/useSession.ts` — new: the whole lifecycle
  - `constellation-web/src/hooks/useMaestro.ts` — `startSession` / `reportProgress` / `stopSession`
  - `constellation-web/src/panels/maestro/PlayerPanel.tsx` — consume it
  - `constellation-web/dist/**`

  ## APPROACH
  1. Start on mount with the item id, the measured `DeviceProfile`, and a resume position. Apply
     `position_secs` on `loadedmetadata` (setting `currentTime` before metadata is a no-op that
     silently starts from zero — the single most common resume bug).
  2. Heartbeat on an interval **while playing only** (paused ⇒ no heartbeat, but do send one
     `paused:true` beat on the pause transition so the server knows), carrying position, paused state
     and `playbackRate`.
  3. Stop on: unmount, route change, item change, and `pagehide`/`beforeunload`. **Both** SPA and
     browser exits — a router-only cleanup misses a tab close, and a `beforeunload`-only cleanup
     misses every in-app navigation.
  4. The unload path uses `navigator.sendBeacon` (fire-and-forget survives teardown; a normal request
     is cancelled). That is a network call outside `aggregationClient`, so the beacon URL is
     **built by `aggregationClient`** and only *dispatched* here — document that split in code.
     If `sendBeacon` is unavailable, fall back to a keepalive request and say so.
  5. Idempotent stop: a session stopped twice (unmount then `pagehide`) must not error. Guard with a
     stopped flag and treat a `404` on stop as success.
  6. Save-on-exit is the final position from the element, not the last heartbeat's — those differ by
     up to one interval, which is exactly the gap that makes a resume land visibly early.
  7. `ended` → report completion explicitly rather than letting the position imply it; a
     watched-to-credits item and one stopped at 98% are different facts to the taste model.
  8. **Auto-resume (epic §2c.3).** Treat a `5xx`, a network error, or a media error whose cause is a
     dead/expired stream as RECOVERABLE: keep the last known position in component state, re-mint the
     session, rebind the source, seek back, and resume — **without leaving the route and without
     losing the user's track/speed/volume selections.** Retry with capped exponential backoff
     (~1s → ~15s, a small bounded number of attempts) so a genuinely-down Maestro is not hammered,
     and surface a quiet, non-modal reconnecting indicator rather than an error card while attempts
     remain. Exhausting them falls through to MPLY-12's diagnostics.
  9. **Distinguish recoverable from terminal, and do not paper over the second.** A `404` on the item,
     a `403` that is an authorisation refusal rather than an expiry, and an unsupported-codec error
     are terminal — retrying them just delays an honest message. Only transport-shaped failures
     (5xx, network, expired signed URL) auto-resume. Getting this wrong in either direction is bad:
     retrying a terminal error looks like a hang, and failing a transient one throws away a film.

  ## TEST PLAN
  - typecheck + build + `lint:adherence`
  - vitest: resume applies on `loadedmetadata`, never before
  - vitest: heartbeats stop while paused and resume on play
  - vitest: stop is idempotent and a `404` on stop is treated as success
  - vitest: `pagehide` dispatches a beacon with the element's CURRENT position, not the last heartbeat's
  - vitest: a `5xx` on the stream re-mints the session and seeks back to the last position, preserving
    the selected audio/subtitle track, speed and volume
  - vitest: a `404`/unsupported-codec error does NOT retry — it goes straight to diagnostics
  - vitest: retries back off and are bounded; an unreachable Maestro is not hammered
  - **Live restart test (the one that proves §2c.3):** start playback, `systemctl restart
    maestro.service` on the host mid-film, and confirm the player reconnects and resumes within a few
    seconds at the same position, with no user action and no lost track selection
  - Live capture: the trace shows `POST /playback/sessions`, ≥1 progress call, and a stop on
    navigating away; a second capture confirms reopening the item resumes at the stored position
  - Verify no hardcoded infrastructure values

  ## EDGE CASES
  - Two tabs playing the same item → two sessions; neither stop cancels the other (assert on session id)
  - Network drops mid-playback → heartbeats fail silently and retry on the next tick; playback is not interrupted and no error card appears
  - The tab is backgrounded → heartbeats continue if playback continues; a throttled timer is acceptable, a lost stop is not
  - Maestro restarts mid-film (the routine same-image hotfix) → reconnect and resume; the operator sees a blip, not a stopped film
  - Maestro is genuinely down → bounded retries, then diagnostics naming it; never an infinite spinner
  - The signed URL expires exactly at a reconnect → re-mint rather than reporting the media as gone (shares MPLY-03's path)
  - Resume position within the last ~30s of the item → start from the beginning instead of the credits
  - Session start succeeds but the media never loads → the session is stopped rather than left dangling

- **Acceptance criteria:**
  - [ ] A session starts on open, heartbeats while playing, and stops on BOTH route change and tab close
  - [ ] Resume applies after `loadedmetadata` and reopening an item resumes at the stored position in a live capture
  - [ ] Stop is idempotent; a double stop and a `404` are both non-errors
  - [ ] The exit position comes from the media element, not the last heartbeat
  - [ ] A failed heartbeat never interrupts playback or shows an error card
  - [ ] A `5xx`/disconnect auto-resumes at the last reported position, preserving track, speed and
        volume selections, with bounded backoff (epic §2c.3)
  - [ ] A restart of `maestro.service` mid-playback resumes automatically, verified live on the host
  - [ ] A terminal error (404 / unsupported codec / authorisation refusal) does NOT retry
  - [ ] Embedded `dist` rebuilt and committed
  - [ ] No hardcoded infrastructure values in new/modified code; all existing tests still pass

### MPLY-09: "Playing on" — target selector and remote transport control
- **Priority:** Critical
- **Labels:** maestro, constellation-web, player, cast
- **Agent:** claude
- **Estimate:** 8h
- **Phase:** G1 (B) — **this is the G1 headline feature**
- **Blocked by:** MPLY-01; spec B (`PlaybackBackend` transport + targets)
- **Description:** A target picker: **This browser** or any target the active backend reports — a
  Plex client today, a future native renderer later. Choosing a remote target drives the backend's
  transport API, with the position reflected by polling.

  This is the generalisation of Muse's existing `CastController` trait, whose doc comment already
  anticipates a non-Plex implementation (epic §5) — the GUI must not assume Plex anywhere.

  **Re-scoped by the epic §4/§8.6 rewrite, and the change matters.** This item was originally a menu
  bolted onto MPLY-05's control bar, sequenced late. It is now the **first genuinely useful thing
  this spec ships** — and against a Plex backend it is the *only* thing, because plex mode is
  control-and-observe and puts no bytes in a browser, ever (epic §8.6). So this item must stand up
  **without** MPLY-05: it carries its own minimal transport row (play/pause, seek, stop, position)
  for the remote case, and MPLY-05 later reuses that row for the local case rather than the reverse.
  Nothing here may import from `PlayerControls.tsx`.

  **How "This browser" must read, and why the wording is an acceptance criterion.** The entry is
  always listed, and it is enabled iff the active backend's `in_browser_stream` is true. When it is
  false the panel must say *why* in terms of the backend's nature, not the project's schedule:

  > **This browser** — unavailable. The Plex backend can drive playback on your devices but cannot
  > stream to a browser. In-browser playback arrives with Maestro's native engine.

  **Not** "coming soon", "not yet implemented", or a bare disabled control. Under plex,
  `in_browser_stream: false` is a **permanent property of that backend**, and a "soon" tells the
  operator something false about their own system — they would wait for a release that will never
  change this, when the actual answer is "switch backends". MPLY-01's three-state capability shape
  is what makes the distinction expressible: *false* gets the sentence above, *not-yet-known* gets a
  neutral loading state, and only those two are possible — there is no "false but temporary".

  **Capability-honest (§4b), and never backend-name-driven.** Targets are offered per
  `BackendCapabilities`: no `device_cast` ⇒ no remote targets, said plainly rather than shown as an
  empty menu. Where the backend cannot report a figure (`can_report_transcode_detail` false on plex),
  the readout says what it knows and marks the rest not-reported — never a confident `0:00`. No code
  in this item may test the backend's *name*.

  ## FILES
  - `constellation-web/src/panels/maestro/TargetMenu.tsx` — new
  - `constellation-web/src/panels/maestro/useTransport.ts` — new: one interface over local element and remote target
  - `constellation-web/src/panels/maestro/TransportRow.tsx` — new: the minimal play/pause/seek/stop/position row this item owns (MPLY-05 later reuses it)
  - `constellation-web/src/panels/maestro/NowPlayingPanel.tsx` — new: the G1 surface hosting the picker + transport row
  - `constellation-web/src/hooks/useMaestro.ts` — `useMaestroTargets()`, `useMaestroActiveSessions()`, transport calls
  - `constellation-web/src/panels/registerPanels.ts` — register `maestro.nowplaying`
  - `constellation-web/dist/**`

  ## APPROACH
  1. `useTransport` presents ONE interface (`play/pause/seek/stop/state`) with two implementations,
     so `PlayerControls` does not branch on target anywhere. That indirection is the item's real
     deliverable — without it, every future control grows a local/remote fork.
  2. "This browser" is always present and always first, even when the targets endpoint degrades — the
     local player must never become unreachable because a discovery call failed. Its enabled state is
     read from `in_browser_stream`, never a hardcoded flag and never the backend's name, so it
     self-enables the moment a native backend serves the session, with no code change here. Its
     disabled copy is the backend-nature sentence above, not a schedule promise.
  2b. Now-playing rows compose per §4a: `muse_item_id` from the session, title/art from Muse. Label
     the watcher by **Muse account** (§4c), never by the logged-in shell user.
  3. Remote state comes from polling the backend's session state at a modest interval; the scrub bar
     is read-only-ish (seek still commits) and clearly labelled with the target name.
  4. Switching targets mid-playback carries the position across and stops the source cleanly first.
  5. Remote control is a **mutating** action: gate it on the `operator` role via the existing
     `RoleGate`, as the shell already does for mutating controls. A viewer can watch locally and
     cannot seize the living-room TV.

     **`RoleGate` is presentation, not enforcement, and must never be described as security.** It
     hides a control from a viewer; it cannot stop anyone who opens a console. The actual guarantee
     has to be a **server-side `403`** on every mutating playback route — transport control, target
     switching and session termination — enforced by Maestro from the identity the proxy resolves,
     not from anything the browser sends. That enforcement is spec B/D's to implement; this item's
     obligation is to (a) fail gracefully and legibly on a `403` rather than assuming the hidden
     control means it can never arrive, and (b) **verify the server actually returns one** with a
     direct viewer-role request. If the route answers `200` to a viewer, that is a finding to file
     against B/D — not something to paper over with a better-hidden button.
  6. Target labels come from the backend verbatim. No inferred device icons that could mislabel a
     device we know nothing about.

  ## TEST PLAN
  - typecheck + build + `lint:adherence`
  - vitest: with the targets endpoint degraded, "This browser" is still offered and playable
  - vitest: `useTransport` routes each control to the right implementation for the selected target
  - vitest: switching targets stops the previous transport before starting the next
  - vitest: a `403` from a transport call renders an explained refusal, not a generic failure and not
    a silent no-op
  - **Server-side check (not a UI test):** issue a mutating transport request with a viewer-role
    session and confirm Maestro answers `403`. Record the actual status in the PR. A `200` here is a
    finding against spec B/D and must be reported, not worked around client-side
  - Live capture: the menu lists at least "This browser"; where a real backend target exists, the
    trace shows the transport call and the state poll
  - Verify no hardcoded infrastructure values

  ## EDGE CASES
  - Zero remote targets → the menu shows only "This browser" with a one-line note, not an empty menu
  - A target disappears mid-session (device sleeps) → the UI says so and offers to resume here at the last known position
  - Transport call `409`/`503` → surfaced with the backend's own reason, never a generic failure
  - A viewer-role user → the remote targets are visible but not selectable, with the reason stated

- **Acceptance criteria:**
  - [ ] A target menu lists "This browser" plus every backend-reported target
  - [ ] The panel ships and is usable with NO import from `PlayerControls.tsx` (G1 standalone)
  - [ ] "This browser" self-enables from the `in_browser_stream` capability, not a hardcoded flag
  - [ ] Under a plex backend its disabled copy explains the BACKEND's nature and never promises a
        future release ("coming soon"/"not yet implemented" fail this criterion), asserted on the
        rendered string in a unit test
  - [ ] No code in this item branches on the backend's NAME — capabilities only, proven by grep + test
  - [ ] A figure the backend cannot report is marked not-reported, never rendered as `0`
  - [ ] Now-playing rows take their title/art from Muse and label the watcher by Muse account
  - [ ] With target discovery degraded, local playback still works and is still offered
  - [ ] `useTransport` is the only place with local-vs-remote branching, proven by test
  - [ ] Remote control is hidden from a viewer by `RoleGate` AND refused server-side with a `403`,
        with the observed status recorded in the PR
  - [ ] A `403` renders an explained refusal, never a silent no-op
  - [ ] A target lost mid-session is explained and offers a local resume
  - [ ] Embedded `dist` rebuilt and committed
  - [ ] No hardcoded infrastructure values in new/modified code; all existing tests still pass

### MPLY-10: Chromecast sender support (feature-flagged, default OFF)
- **Priority:** Medium
- **Labels:** maestro, constellation-web, player, cast
- **Agent:** claude
- **Estimate:** 5h
- **Phase:** G2 (D + K)
- **Blocked by:** MPLY-09; **spec K** for the receiver + App ID (end-to-end verification only)
- **Description:** Chromecast as another entry in the MPLY-09 target menu, via the Cast sender SDK.
  Per epic §8.4, casting needs a **receiver app and a registered Cast App ID** — now built and
  registered by **spec K** (`S130-K-maestro-cast-receiver.md`), and one this item must NOT block on. So the sender ships behind a config flag that is
  **off by default**: with no App ID configured, the Cast entry simply is not offered and everything
  else in the section is unaffected.

  **This is the item most likely to be mis-reported.** "Cast support merged" while no App ID exists
  means the code path has never run end to end. Its acceptance criteria say that explicitly.

  ## FILES
  - `constellation-web/src/panels/maestro/cast.ts` — new: SDK load, session management, guards
  - `constellation-web/src/panels/maestro/TargetMenu.tsx` — the conditional Cast entry
  - `constellation-web/src/panels/maestro/useTransport.ts` — the Cast transport implementation
  - `constellation-web/README.md` — document the flag and the App ID prerequisite
  - `constellation-web/dist/**`

  ## APPROACH
  1. The App ID comes from the Maestro health/capability payload (MPLY-01), **not** a build-time
     constant and never a literal in source — it is deployment configuration, and hardcoding it is
     exactly the class of thing S1 exists to stop.
  2. The Cast sender SDK is loaded lazily and only when an App ID is present. It is an external
     script, which the shell otherwise never loads; if the CSP or the network blocks it, the Cast
     entry becomes unavailable with a reason and nothing else degrades.
  3. The Cast transport is one more `useTransport` implementation. Zero changes to `PlayerControls`
     — if this item needs to touch the controls, MPLY-09's abstraction was wrong and should be fixed
     there instead.
  4. Media loaded on the receiver must be a URL the **receiver** can reach — that is a different
     network position from the browser's, and a same-origin proxy path may not resolve there. Take
     the cast-reachable URL from the session payload; if Maestro does not supply one, the Cast entry
     is unavailable with that reason rather than casting a URL that will fail on the device.
  5. Chromecast's supported formats are a published, closed matrix (epic §6). Do not re-derive it
     client-side; pass the target's profile to Maestro and let spec C's `plan()` decide.

  ## TEST PLAN
  - typecheck + build + `lint:adherence`
  - vitest: with no App ID in the capability payload, the Cast entry is absent and the SDK is never loaded
  - vitest: an SDK load failure marks Cast unavailable and leaves every other target working
  - vitest: a session with no cast-reachable URL marks Cast unavailable with that reason
  - Live capture with the flag off (today's state): the target menu shows no Cast entry and the trace
    contains no external script request
  - End-to-end cast verification is **deferred to spec K** and must not be claimed before it
  - Verify no hardcoded infrastructure values

  ## EDGE CASES
  - No Cast devices on the network → the entry is present but empty-with-a-reason, not an error
  - Receiver rejects the media → surface the receiver's own error text
  - Cast session ends on the device → the UI returns to "This browser" at the last known position
  - Two senders → the second sees the device busy and says so

- **Acceptance criteria:**
  - [ ] With no App ID configured, no Cast entry appears and no external SDK is loaded
  - [ ] The App ID is read from deployment configuration, never hardcoded in source
  - [ ] The Cast transport adds NO branching to `PlayerControls`
  - [ ] An unreachable-from-receiver URL makes Cast unavailable with a reason rather than failing on the device
  - [ ] The PR states plainly that end-to-end casting is unverified pending spec K
  - [ ] README documents the flag and points at spec K for the receiver/App ID
  - [ ] Embedded `dist` rebuilt and committed
  - [ ] No hardcoded infrastructure values in new/modified code; all existing tests still pass

### MPLY-11: Real-browser playback confirmation (operator action)
- **Priority:** High
- **Labels:** maestro, constellation-web, verification, operator
- **Agent:** <operator>
- **Estimate:** 1h
- **Type:** human-action
- **Phase:** G2 (D)
- **Description:** The one thing the automated harness structurally cannot do. Per §5.6, headless
  Chromium's open-source build ships without the proprietary decoders (H.264/AAC) that a real Chrome
  has, so a harness run can report `MEDIA_ERR_SRC_NOT_SUPPORTED` on a file the operator's browser
  plays perfectly. The harness proves the *wiring* (session start, heartbeat, stop, buffered ranges);
  only a human with a real browser proves the *picture*.

  **The Cast App ID and receiver registration that used to live here are now spec K**
  (`S130-K-maestro-cast-receiver.md` — receiver app, App ID registration, receiver auth handshake,
  depending on D and G). That is the right home: the receiver is an application we build and deploy,
  not a checkbox, and the signed-URL handshake (epic §8.7) is real engineering rather than an ops
  step. MPLY-10 ships dark until K lands and nothing else in this spec depends on it.
- **Steps:**
  1. Open `/muse/play` in a real desktop browser; confirm the poster grid and, once D has landed,
     the continue-watching rows populate.
  2. Play one **direct-play** item. Confirm picture and sound, then scrub, pause, change volume and
     enter/exit fullscreen.
  3. Play one item that **transcodes**. Confirm it plays, then open `/why` and confirm the tier and
     at least one reason string describe what is actually happening.
  4. Navigate away mid-playback, then close the tab mid-playback. Confirm via the Activity surface
     that **no session is left running** in either case (this is MPLY-08's real-world test).
  5. Reopen a partially-watched item and confirm it resumes at the right position.
  6. Repeat step 2 in Safari if available — it takes the native-HLS path and never loads hls.js,
     which is the one branch no other check exercises.
  7. Report each outcome. A step that fails is a finding against the named item, not a note.

### MPLY-12: `/why` — the playback-plan diagnostics affordance
- **Priority:** High
- **Labels:** maestro, constellation-web, player, diagnostics
- **Agent:** claude
- **Estimate:** 5h
- **Phase:** G1 (B + C)
- **Blocked by:** MPLY-01; spec C (`plan()` reasons)
- **Description:** A disclosure — in the Phase-1 now-playing row, and later in the player and the
  card it falls back to on failure — that shows **the chosen playback plan and why**: direct play /
  remux / partial / full transcode, the source and target codecs and container, the device-profile
  facts that drove the decision, the ordered `reasons` list spec C emits, and the **transcode speed
  as a realtime ratio** (e.g. `1.8×` — above 1 keeps up, below 1 will stutter).

  Spec C's reasons are structured and specific ("transcoding because: audio TrueHD unsupported on
  this Cast generation"). Surfacing one verbatim in the now-playing row is the operator-facing answer
  to **"why is the fan running"**, and it is nearly free once the backend emits it.

  This is the cheapest genuinely valuable thing in the spec. Every "it won't play" and every
  "why is my GPU hot" is answered here in seconds instead of by reading server logs — and the
  information already exists the moment C is done. It is also the honest failure surface: an
  unplayable item explains itself rather than showing a dead black rectangle.

  ## FILES
  - `constellation-web/src/panels/maestro/WhyPanel.tsx` — new
  - `constellation-web/src/panels/maestro/PlayerPanel.tsx` — the disclosure + the failure fallback
  - `constellation-web/src/hooks/useMaestro.ts` — `useMaestroPlan(itemId)`
  - `constellation-web/dist/**`

  ## APPROACH
  1. Render the plan's `reasons` **verbatim and in order** — they are the decision engine's own
     account of itself. Do not paraphrase, reorder, summarise or add a friendly gloss; a rewritten
     reason is a different claim from the one the server made. (Same rule S129 set for Muse's
     curation rationale.)
  2. Show the tier prominently with the design system's status vocabulary: direct play is the good
     outcome, full transcode is the expensive one. State the tier, not a judgement of it.
  3. On a media error, this card **replaces** the video with: the media error code, the plan, the
     reasons, and the most likely cause — distinguishing the ones that look identical to a user:
     a `401`/degraded backend (credential — see the TERM #549 pattern), an unsupported codec (device
     profile), a missing file, and a stalled/failed transcode.
  4. Never invent a cause. If the plan is absent, say the plan could not be retrieved and show the
     error — do not diagnose from the absence (`MediaDetailPanel`'s omission rule, applied here).
  4b. **Capability-honest (§4b), and this card is where it bites hardest.** The plex backend has
     `can_report_transcode_detail: false` — it does not tell us the speed, the codec pair, or the
     reason. Those rows render **"not reported by this backend"**, visually distinct from both zero
     and degraded. A `0.0×` speed on a Plex session would be read as "the transcode is stalled" and
     send someone debugging a transcode that is running fine on a server that simply does not report.
     That is the single most expensive lie this card could tell, so it gets its own unit test.
  5. Reachable from the control bar at all times, not only on failure — it is a diagnostic, and its
     value is highest just before something goes wrong.
  6. Include the session id and the selected target, so a report from the operator can be matched to
     a server-side session without guesswork.

  ## TEST PLAN
  - typecheck + build + `lint:adherence`
  - vitest: reasons render verbatim and in the payload's order
  - vitest: an absent plan renders "plan unavailable" and does NOT render a fabricated tier
  - vitest: a `401` degrade renders the credential-shaped explanation, distinct from the codec one
  - Live capture: the disclosure shows a real tier and at least one real reason string from `/playback/plan`
  - Live capture of a deliberately unplayable item: the diagnostics card replaces the video and names
    the observed error
  - Verify no hardcoded infrastructure values

  ## EDGE CASES
  - Plan endpoint degrades while playback works → the disclosure says the plan is unavailable; playback is untouched
  - A reason string longer than the card → wraps/scrolls in its own container, never truncated to a
    misleading half-sentence
  - Maestro reports a tier this build does not know → render it verbatim as unclassified, never coerced to a known tier
  - No session yet (still starting) → a loading state, not an empty card

- **Acceptance criteria:**
  - [ ] A `/why` disclosure shows the tier, codecs, device-profile facts, transcode realtime ratio and the verbatim ordered reasons
  - [ ] A backend with `can_report_transcode_detail: false` renders "not reported", NEVER `0` or `0.0×`, proven by unit test
  - [ ] Reasons are never paraphrased, reordered or summarised, proven by unit test
  - [ ] An unplayable item renders this card in place of the video, naming the observed error
  - [ ] An absent plan says so and does not fabricate a tier or a cause
  - [ ] An unknown tier renders verbatim as unclassified
  - [ ] Embedded `dist` rebuilt and committed
  - [ ] No hardcoded infrastructure values in new/modified code; all existing tests still pass

---

## Deliberately out of scope

- **Any Maestro server code.** This spec builds only the constellation-web surface. The session
  model, decision engine and segment server live in `src/maestro/` in `moosenet/Muse` and belong to
  B/C/D/E; `proxy_maestro` lives in `Terminus/src/constellation/proxy.rs` and belongs to B. An
  MPLY item that edits Rust is out of scope and should be handed back.
- **The Server Activity panel** — that is spec H, and it needs a Muse-side endpoint that does not
  exist yet (epic §5).
- **`CONSTELLATION_MAESTRO_TOKEN` provisioning** — an operator action in <secret-manager> (epic §11). Its
  absence makes protected routes 401; the panels degrade honestly and this spec does not work around
  it.
- **The Cast receiver app, its App ID and the receiver auth handshake** — **spec K**. This spec builds
  only the *sender*, which ships dark until K lands.
- **Signed-stream-URL minting and expiry policy** — spec D (epic §8.7). This panel consumes a signed
  URL and re-mints on expiry; it never mints, parses or extends one.
- **Any in-browser playback of Plex content** — structurally impossible, not deferred (epic §8.6).
  A Plex byte-proxy was explicitly rejected; do not reintroduce one from the client side by pointing
  a `<video>` at a Plex URL.
- **Per-user identity.** Sessions carry an `account_id` from day one (epic §8.1) — **Muse's account
  id, in the id-space the taste model uses, NOT the constellation-web cookie session**, which carries
  roles rather than household members (§4c). The proxy maps the session to a configured default Muse
  account. This surface displays that id's account and never derives a watcher from the logged-in
  shell user. A real identity service unifying the two id-spaces is its own spec.
- **Offline/download, watch-together sync, and a picture-in-picture mini-player.** All reasonable,
  none required to make playback exist, and each large enough to distort this spec.
- **HDR tone-mapping controls.** Out of scope through spec E by epic §8.3; there is nothing for a
  control to drive.
- **A DASH client, a second player library, or a player-UI framework.** See §1.
