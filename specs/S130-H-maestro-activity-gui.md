# S130-H — Maestro: Server Activity (who is watching what, and what the box is doing)
plane_project: MUSE
module: Muse
prefix: MACT
spec_id: S130-H-maestro-activity-gui

## Metadata
- **Author:** Moose
- **Session:** S130
- **Date:** 2026-08-01
- **Module version:** Muse v0.1 (+3 endpoints) · Maestro v0.1 (activity read surface) · constellation-web (new panel)
- **Estimated total:** ~50h across two phases (H1 ~28h, H2 ~22h)
- **North-Star layer:** module
- **Module-Contract:** meets §4 clauses 1–7 with two stated narrowings.
  - Clause 3 (context bus) is **partially deferred**: this spec CONSUMES "what is being watched right
    now" and renders it, but constellation-web still has no context bus to publish onto (the same
    posture as S129/MGUI and every other module panel today).
  - Clause 1 (Terminus-fronted): **fully met, with no carve-out.** Everything this spec fetches is
    control-plane/state and goes through `proxy_muse` / `proxy_maestro`. The epic's §8.6 media
    carve-out (signed, session-scoped, expiring URLs served direct from Maestro) belongs to specs D
    and G; the Activity panel never fetches media bytes. Stated so a reviewer can see the carve-out
    was considered and does not apply here.
  - Clause 4 (assistant-operable): every read is an HTTP endpoint the assistant can call through
    Terminus, and the one mutation (terminate) is a plain `POST` — no panel-only capability.
- **Context:** Child spec **H** of `S130-maestro-epic.md`. It builds the **Server Activity** section:
  who is watching what, what is transcoding, what is being imported, and how the host is holding up.

  **Per the rewritten epic §4 this spec ships in two phases, and H1 is now the FIRST thing to ship in
  the entire epic:**
  - **H1 — MACT-01..08. Needs ZERO Maestro.** Its data (`play_sessions`) is already populated by the
    Plex poller and webhook, and it renders through the *existing* `proxy_muse`. No dependency on A,
    B, C, D or E. One sprint, real visible value, ships before anything else in S130.
  - **H2 — MACT-09..12. Blocked by specs D and E.** Maestro-native live sessions and the
    active-transcode view.

  **The finding that makes H1 possible and necessary: no HTTP endpoint exposes play sessions.**
  Verified on `main` at `e8499aa` (the tree was fast-forwarded 64 commits; this survey is post-merge,
  not against the stale checkout):
  - `src/models/play_session.rs` defines `PlaySession` + `PlaySessionMediaInfo`, with
    `video_decision`/`audio_decision`/`transcode_decision` as a real `decision_kind` enum
    (`direct_play` / `direct_stream` / `transcode` / `copy`), plus `transcode_reason`, container,
    codecs, channels, resolution, bitrate.
  - `src/tracker/poller.rs` writes it every `MUSE_PLEX_POLL_SECS` (default **10s**) from Plex
    `/status/sessions`; `src/tracker/webhook.rs` writes it from webhooks. Both fold through
    `src/tracker/reconstruct.rs`.
  - **Nothing serves any of it.** `src/repo/play_session.rs` has `list_for_account` and
    `list_for_media_item` and no "currently active" query; `src/web/mod.rs` registers no sessions
    route. `git grep sessions/active -- src` → nothing.
  - What *does* exist on main and is reused rather than rebuilt: `/stats` and `/gaps` (public),
    `/on_deck` and `/premiere` (protected), `/api/subsystems` (public), `/api/requests/queue`
    (protected), `/health`, `/metrics`.
  - `src/media/` and `src/maestro/` do **not exist yet** — they are created by specs A/C and B/D.
    Every H2 path below is a forward reference, not a current file.

  The second finding: **the existing `/ws` relay carries Harmony only.**
  `Terminus/src/constellation/ws.rs` dials `constellation_harmony_ws_url()` and wraps frames as
  `{source:'harmony', event:…}`; its own module doc says "No fan-in of Chord/Muse events yet (the
  envelope's `source` field is the seam for that)". A live feed is therefore a real backend item
  (MACT-08), and polling is a shipped fallback, not an afterthought.

---

## The structural decision that governs this whole spec: LIVE and HISTORY are two sources

Epic §2 assigns playback sessions to Maestro, but H1 ships before Maestro exists. The epic §4
reconciliation is explicit, and it is the single most important constraint on this spec:

> **Muse's `play_sessions` is the historical record and always has been**; Maestro's `SessionSource`
> becomes the *live* now-playing source once it exists. The Activity panel must treat "live" and
> "history" as two distinct sources from the start, or its source of truth silently changes identity
> when the backend flips — the exact drift §2 warns about.

So this spec never has "the sessions endpoint". It has **two panes with two independently-labelled
sources**, from the very first commit:

| Pane | H1 source | H2 source | Owner, permanently |
|---|---|---|---|
| **LIVE — now playing** | `GET /api/sessions/live` on Muse: a *derived* live view over the historical store, honestly labelled `source: "muse-derived"` | Maestro `SessionSource` via `proxy_maestro`, labelled `source: "maestro-live"` | **Maestro**, once it exists |
| **HISTORY — recently watched** | `GET /api/sessions/history` on Muse | unchanged | **Muse**, permanently |

Three consequences the items enforce:

1. **The panel renders the live pane's `source` value.** An operator can always see whether "now
   playing" is Maestro's live truth or Muse's derived-from-history approximation. The flip in H2 is
   then a *visible, explained* change, not a silent identity swap.
2. **The client types them separately** — `LiveSession` and `HistorySession` are distinct types with
   distinct methods, even though H1's payloads look similar. Merging them "because the shapes match
   today" is the drift, and it is the thing that would make H2 a rewrite instead of a swap.
3. **H1's live pane is honest about being derived.** A derived live view has a real weakness (a
   crashed player leaves a row open), which is exactly why MACT-01 has a liveness rule and a `stale`
   state rather than pretending an open row means someone is watching.

## The router decision (`/api/sessions/*` is PROTECTED) — and why

**Decision: all three new Muse session routes land on `crate::web::protected_routes()`, behind
`crate::http::auth::require_api_token`.** This follows a precedent already set in the same file, by
the same class of data:

- `GET /on_deck` reads `play_sessions` and is **protected**, with the in-tree rationale
  "per-account viewing history … `/on_deck` is 'who left what half-watched', which is exactly the
  per-account data this group exists to gate" (MUSE #84, CAP-SEC-03).
- `GET /stats` and `GET /gaps` are **public** precisely because they are whole-library aggregates
  with "no per-account component."

A now-playing feed is strictly *more* identifying than `/on_deck`: account, device, player, the exact
title and the exact position, in real time. Muse binds `0.0.0.0`, so "public" here means
*unauthenticated on the LAN*. Putting a live per-account watch feed there would recreate the defect
MUSEX-CAP-SEC-01/03 already fixed once.

**The cost, stated plainly:** protected Muse routes reach the browser through `proxy_muse`'s bearer
injection, and `CONSTELLATION_MUSE_TOKEN` is **still unprovisioned** (TERM #549; epic §5 and risk 4).
Until an operator provisions it, the session panes render the honest "Module unavailable — HTTP 401"
degrade, exactly like S129's Phase 2 panels. That is the correct trade: an unauthenticated
now-playing feed is a security defect we would have to undo, whereas a missing token is an operator
action with a tracked owner. **No item may claim end-to-end population of a protected pane while that
token is unset.** The public aggregates (`/stats`, `/gaps`, `/api/subsystems`, `/health`) keep the
panel from ever being fully dark.

**Maestro's side (H2)** is credential-gated through `proxy_maestro` + `CONSTELLATION_MAESTRO_TOKEN`.
Epic §10b notes there are **two** credentials — the Maestro → Muse token for item resolution and
event delivery is spec B/D's concern, but do not assume a single token exists.

## The two-proxy composition (structural enforcement of the ownership split)

Epic §2's fourth enforcement mechanism is **"two proxies in the GUI"**, and this spec is where it is
made real on the client:

- **Metadata and artwork — `proxy_muse` only.** Titles, years, series/episode identity, poster and
  backdrop URLs come from Muse (`/api/library/:id`, `/art/{kind}/{id}`).
- **Session, transport and transcode state — `proxy_maestro` only.**
- **Maestro payloads carry `muse_item_id` and nothing textual.** Epic §2 mechanism 1 forbids
  `title`/`poster`/`overview`/`year` on Maestro's tables and types; mechanism 2 is a build-failing CI
  grep for `title|poster|overview|artwork` in Maestro's API types. **A title on a Maestro payload
  means Maestro has grown a metadata cache** — that is the failure this catches. The panel joins:
  Maestro says "session S is playing `muse_item_id` 4212 at 00:41:07"; the panel resolves 4212's
  title and poster through `proxy_muse`.

## Branch on capability, never on backend name

Epic §8.6: "The GUI and the assistant tools branch on **`BackendCaps`**, never on backend name." The
descriptor (`in_browser_stream`, `device_cast`, `server_side_transcode_decision`,
`seek_during_transcode`, `syncplay`, `can_report_transcode_detail`) is served by `GET /backends`.

The panel therefore contains **no `if (backend === 'plex')`**. It asks
`caps.can_report_transcode_detail` and renders accordingly. This is checkable in review and in a
grep, and it is what makes a future backend render correctly with no panel change.

## The honesty rule (three states, never conflated)

An operator reads this panel to decide things ("is the box busy?", "why is the fan running?"). Three
visually distinct states are mandatory, and each has a test:

1. **A real value** — including a genuine `0`.
2. **Not reported by this backend** — `—` plus "not reported by the Plex backend", driven by
   `BackendCaps`. **Never `0`.** A `0.0×` transcode speed reads as a stalled transcode and sends
   someone debugging a healthy server; a `0` GPU figure reads as an idle GPU when the truth is that
   nobody can see it.
3. **Degraded** — the endpoint 401/404s or is unreachable. Distinct from both of the above, and from
   a true empty list ("nobody is watching"), per the `useMuseSection` convention S129 established.

Plus: **a stale reading is never presented as live.** Every live pane carries `as of HH:MM:SS` and
dims with "last updated Ns ago" when its feed stops, rather than freezing on the last good frame.

---

## Pre-flight

- **One repository: `moosenet/Muse`.** Per epic §2, **Maestro is NOT a new repo** — it is a second
  `[[bin]]` in this crate (`src/bin/maestro/main.rs`, modules under `src/maestro/`, shared `models/`,
  `config.rs`, `repo/`, `error.rs`, one `Cargo.lock`). GUI items are `moosenet/Terminus` (subtree
  `constellation-web/` plus `src/constellation/`).
- Branch off `origin/main` (`e8499aa` at authoring time).
- Prefix `MACT` is checked + registered (epic §11); `plane_prefix_promote` still outstanding.
- **H1 has no upstream spec dependency.** H2 requires spec **D** (session model) and spec **E**
  (transcode lifecycle), and consumes spec **B**'s `proxy_maestro`, `SessionSource` facet,
  `BackendCaps` and `GET /backends`.
- `ffmpeg`/`ffprobe` are **not on the dev box** (epic §11, verified). H2 items needing a live
  transcode must gate on a host that has them; H1 needs neither.
- Baselines to record: `cargo test` green on Muse `main`; `tsc --noEmit` clean and `vitest` counts on
  `Terminus/constellation-web`.
- **Deploy prerequisite, every UI item:** `constellation-web/dist/` is COMMITTED, embedded via
  `include_dir!`, and `oci-publish.sh` has **no npm step**. A panel change that does not rebuild and
  commit `dist/` deploys **nothing** (TERM #550).

## Standing constraints (epic §7 — acceptance criteria on every applicable item)

1. **`src/lib/aggregationClient.ts` is the ONLY module permitted to call `fetch`** (grep-enforced).
2. **Design tokens from `constellation-web/src/styles/globals.css`. No Tailwind.**
   `npm run lint:adherence` passes.
3. Every panel wraps in **`PanelRoot`** and composes existing `src/components/` primitives (`Card`,
   `CardTitle`, `DataTable`, `Badge`, `ProgressBar`, `MetricCard`, `StatusPill`, `SkeletonList`,
   `EmptyState`, `ConfirmDialog`, `RoleGate`), registering in `src/panels/registerPanels.ts`.
4. Chart-shaped content uses the `viz/` kit and `ChartCard`'s **`degraded` prop** — never a throw,
   never a zero plot. Follow the fleet dataviz conventions for tiles and sparklines.
5. **Secrets via <secret-manager> at runtime** (`SecretManager::get()`), never `std::env::var` for anything
   token-shaped (S7). **No literal IPs/hostnames/tokens/emails** (S1).
6. Rust reads degrade to an honest `200` with an explicit unavailability marker, or propagate a real
   error — never a fabricated zero.

---

# Phase H1 — Muse-only Activity (zero Maestro; the epic's first shipped item)

### MACT-01: Muse — `GET /api/sessions/live` + `GET /api/sessions/history`
- **Priority:** Critical
- **Labels:** muse, sessions, api, backend
- **Agent:** claude
- **Estimate:** 6h
- **Description:** The missing endpoints. **Two routes, deliberately, because they are two sources**
  (see the LIVE/HISTORY decision above) — not one route with a `?state=` filter, which would erase
  the distinction the epic requires this panel to preserve.

  - `GET /api/sessions/live` — the derived live view: sessions with `stopped_at IS NULL` that pass
    the liveness rule below. The envelope carries **`source: "muse-derived"`** so the client can
    label it and so H2's flip to `maestro-live` is visible rather than silent. Per row: account (id +
    display name), item (title, year, kind, series/episode, `media_item_id`), same-origin poster/
    backdrop URLs, position (`view_offset_ms`), duration (`duration_ms`), `progress_pct`,
    `player`/`platform`/`product`/`device`, player state, `started_at`, and the joined
    `play_session_media_info` decision block (decisions, `transcode_reason`, container, codecs,
    channels, resolution, bitrate).
  - `GET /api/sessions/history?limit=` — **Muse's permanent role**: the historical record over
    stopped sessions. Same projection, `source: "muse-history"`. This route does NOT change in H2.

  **Account identity: `account_id` is the Muse account** (epic §8.1, corrected) — the same id-space
  the taste model uses. It is **NOT** the constellation-web cookie session, which carries roles
  (`operator|viewer`), not household members. Label a Muse account; **never derive the watcher from
  the logged-in shell user** — that would mint a third id-space matching neither Plex nor Muse and
  silently break taste attribution.

  **This is a READER; stay agnostic about who WRITES the table.** Epic §8.8 (spec **J**) makes
  Maestro's plex adapter the sole Plex session observer, with Muse's tracker becoming a consumer of
  its event stream. `play_sessions` remains Muse's store, so **J changes the source of the rows, not
  the shape of these endpoints.** This item must NOT reference `tracker::poller` from a handler,
  assume the poller is running, key behaviour on `source = 'plex_poll'`, or bake the Plex cadence into
  anything but a default.

  **Liveness must not be guessed.** `stopped_at IS NULL` alone is not "active" — a crashed player, a
  missed stop event or an ingest outage leaves a row open forever. A session is LIVE iff
  `stopped_at IS NULL` **and** its newest `play_events` row is within
  `MUSE_SESSION_ACTIVE_GRACE_SECS` (default `max(3 × ingest cadence, 60)`, so a slow deployment does
  not flap). An open-but-stale row is returned as `state: "stale"` with `last_event_at` — **not**
  dropped and **not** shown as playing. This weakness is intrinsic to a derived live view, which is
  precisely why H2 replaces it with a real `SessionSource`.

  ## FILES
  - `src/repo/play_session.rs` — `list_live(pool, grace_secs)` + `list_history(pool, limit)`, each
    joining `play_session_media_info`, `accounts`, `media_items`/`media_metadata` and the newest
    `play_events` row per session
  - `src/web/dashboard.rs` — `get_live_sessions` / `get_session_history` + `Serialize` types
  - `src/web/mod.rs` — register both on `protected_routes()` with a comment stating WHY, mirroring the
    existing `/on_deck` comment
  - `src/config.rs` — `MUSE_SESSION_ACTIVE_GRACE_SECS` (behavioural config, plain env, not a secret)
  - `README.md`

  ## APPROACH
  1. Write `list_live` as ONE query with a `LATERAL` join for the newest `play_events` row — not N+1
     per session — bounded (`LIMIT 100`). A household never has 100 concurrent streams, and an
     unbounded scan over a corrupted table must not become the failure mode.
  2. `progress_pct`: `percent_complete` is a **fraction in 0..1 despite the column name** — MUSE #87
     already fixed a bug where passing it through unscaled made every progress bar read ~0. Scale
     here, with a unit test asserting 0.48 ⇒ 48.
  3. Player state from the newest event: a pause event ⇒ `paused`; a newer play/progress event ⇒
     `playing`; nothing inside the grace window ⇒ `stale`.
  4. Emit the persisted `decision_kind` values verbatim. Do NOT collapse them into a boolean "is
     transcoding" — `direct_stream` (remux) and `transcode` are materially different to an operator,
     and `copy` is different again.
  5. **Do not emit `ip_address`.** It is on the model, it is per-person, and nothing here needs it.
  6. Errors propagate as `MuseResult` rather than failing open to `[]`, for the reason `get_gaps` and
     `get_stats` already document in-tree: a 2xx empty body renders as a CLAIM ("nobody is watching"),
     not as the absence of one.

  ## TEST PLAN
  - `cargo test` — 0..1 → percentage scaling; grace-window classification (fresh ⇒ live, beyond grace
    ⇒ `stale`, never dropped); decision-kind passthrough for all four variants; `ip_address` absent
    from the serialised body; both envelopes carry their `source` discriminator
  - An sqlx integration test inserting an open session + a recent event, asserting one live row with
    joined media info, and a stopped session appearing only in `/history`
  - A test that unauthenticated requests to both routes get `401`
  - A grep/review check that neither handler references `tracker::poller`
  - Verify no hardcoded IPs, hostnames or tokens in new/modified files

  ## EDGE CASES
  - No ingest configured → empty table; return `{sessions: [], source: "muse-derived"}` — a true
    empty, distinguishable from a degrade
  - Open-but-stale row → `state: "stale"` + `last_event_at`; never "playing", never dropped
  - Session with no resolved `media_item_id` → return it with a null item rather than dropping a real
    stream
  - `duration_ms` null → progress omitted, not `0%`
  - Two devices, one account → two rows, keyed by `session_key`

- **Acceptance criteria:**
  - [ ] `GET /api/sessions/live` and `GET /api/sessions/history` are SEPARATE routes, each carrying an
        explicit `source` discriminator
  - [ ] Live rows include account, item, position/duration/progress, device, player state and the
        decision block
  - [ ] `account_id` is the Muse account id — never derived from the cookie session
  - [ ] Both on `protected_routes()`; unauthenticated ⇒ `401`
  - [ ] `progress_pct` is a percentage (0.48 ⇒ 48), unit-tested
  - [ ] An open-but-stale session is `stale`, never playing and never dropped
  - [ ] Handlers are agnostic to who writes `play_sessions` (no poller coupling), so spec J changes
        the writer without touching them
  - [ ] `ip_address` is never serialised
  - [ ] A query error propagates; it never fails open to an empty list
  - [ ] No hardcoded infrastructure values in new/modified code
  - [ ] All existing tests still pass

### MACT-02: Muse — `POST /api/sessions/:session_key/terminate`
- **Priority:** High
- **Labels:** muse, sessions, api, mutation, safety
- **Agent:** claude
- **Estimate:** 5h
- **Blocked by:** MACT-01
- **Description:** Stop someone's stream — the only mutation in this spec, and the only item with
  real-world blast radius: it interrupts a person mid-film.

  Muse already has the mechanism: `src/plex_control/client.rs` exposes `stop(target)` behind the
  `CastController` trait in `src/plex_control/cast.rs`, whose doc comment explicitly anticipates a
  non-Plex implementation (spec B generalises it into the `DeviceControl` facet). **Dependency note:**
  `CastController` today offers play/pause/stop/skip/poll and lacks seek and volume; spec B is
  extending it. This item needs **only `stop`, which exists**, so it is not blocked on that work.

  **Three gates, two of which are enforcement:**
  1. **Terminus role gate — real.** The route reaches the browser through `proxy_muse` on Terminus's
     `protected_router`, layered with `enforce_viewer_role_gate`
     (`Terminus/src/constellation/auth.rs`): a `viewer`'s `POST/PUT/PATCH/DELETE` gets
     `403 {"error":"forbidden","required_role":"operator"}` **before the request is proxied**.
  2. **Muse bearer — real.** `require_api_token`. Muse has one shared bearer and no per-user identity,
     so it cannot itself distinguish operator from viewer; it proves the caller came through Terminus.
  3. **`RoleGate` in the panel — cosmetic only** (MACT-07). Its own doc comment says so.

  ## FILES
  - `src/plex_control/mod.rs` (or `src/sessions/terminate.rs`) — resolve `session_key` → live player
    target, then call the controller's `stop`
  - `src/web/dashboard.rs` — `terminate_session` handler
  - `src/web/mod.rs` — register on `protected_routes()`
  - `README.md`

  ## APPROACH
  1. Key on `session_key`, resolved against MACT-01's LIVE set. A caller cannot pass an arbitrary
     player target and have Muse relay a stop to it — the indirection is the point.
  2. Go through the trait, never a second HTTP path to the backend. No controller configured ⇒ `503`
     with an explicit body, never a `200` implying the stream stopped.
  3. Accept an optional `{"reason": "..."}`; where the backend can surface it to the viewer, pass it
     through, and where it cannot, say so (`reason_delivered: false`) rather than implying the person
     saw an explanation.
  4. Report what actually happened: `{stopped, backend, reason_delivered}`. A best-effort stop the
     player ignored is `stopped: false`, never an optimistic `true`.
  5. Log at `info` with session key + target; no account PII beyond the internal id.

  ## TEST PLAN
  - `cargo test` — unknown/inactive key ⇒ `404` (never a blind relay); no controller ⇒ `503`;
    controller error ⇒ `stopped: false`, never fabricated success; route is protected (⇒ `401`
    unauthenticated)
  - **Cross-repo verification recorded in the PR:** a `viewer`-session `POST` through the Terminus
    proxy returns `403 {"error":"forbidden","required_role":"operator"}`
  - Verify no hardcoded infrastructure values

  ## EDGE CASES
  - `session_key` not in the live set → `404`, no relay attempted
  - Controller unconfigured / backend unreachable → `503`, never a false success
  - Player accepts but keeps playing → `stopped: false` with the backend's own report
  - Concurrent terminate → idempotent; the second is `404`/`stopped:false`, never a 500

- **Acceptance criteria:**
  - [ ] `POST /api/sessions/:session_key/terminate` stops a live stream via `CastController::stop`
  - [ ] Uses only the existing trait method — not blocked on spec B's seek/volume extension
  - [ ] Keyed on a session from the live set; unknown key ⇒ `404` with no relay
  - [ ] A viewer's POST through the proxy is `403` — captured and recorded
  - [ ] No controller configured ⇒ `503`, never a fabricated success
  - [ ] The response reports the REAL outcome (`stopped`, `reason_delivered`)
  - [ ] No hardcoded infrastructure values in new/modified code
  - [ ] All existing tests still pass

### MACT-03: aggregationClient — typed LIVE and HISTORY surfaces (the only fetch site)
- **Priority:** Critical
- **Labels:** terminus, constellation-web, client, contract
- **Agent:** claude
- **Estimate:** 4h
- **Blocked by:** MACT-01, MACT-02
- **Description:** The client seam. `aggregationClient.ts` is the ONLY module allowed to call `fetch`
  (grep-enforced), so every read lands here first.

  H1 adds `muse.sessions.live()`, `muse.sessions.history(limit?)` and
  `muse.sessions.terminate(key, reason?)`, each degrading via the house convention
  (`{available:false, detail}`, never a throw — the `terminus.activity()` / `useMuseSection` pattern).

  **This item is where the LIVE/HISTORY split becomes a type-level fact.** `LiveSession` and
  `HistorySession` are **distinct interfaces** with distinct methods and a discriminated `source`
  field, even though H1's payloads currently look alike. Merging them "because the shapes match
  today" is exactly the drift epic §4 forbids, and it is what would make H2 a rewrite instead of a
  one-line source swap. The doc comment must say so, because the next person to touch this file will
  be tempted.

  It also documents the **two-proxy rule** (metadata/art from `muse.*`; session/transport state from
  `maestro.*` once it exists) for the same reason.

  ## FILES
  - `constellation-web/src/lib/aggregationClient.ts` — interfaces, http adapter, mock fixtures
  - `constellation-web/src/hooks/useMuse.ts` — `useMuseLiveSessions()` / `useMuseSessionHistory()` via
    the existing `useMuseSection`, inheriting per-endpoint degradation unchanged
  - `constellation-web/dist/**`

  ## APPROACH
  1. **Type from a real capture, not from this spec's prose** — a direct authenticated probe for the
     protected routes, field names copied from the response. S129 records what guessing costs here.
  2. Mock fixtures must be shape-faithful, including the `source` discriminator; the file already
     carries an in-tree comment about a mock that invented an envelope Muse never returned. Cover
     playing, paused, stale, direct-play, remux and full-transcode rows.
  3. `terminate` is the only mutation; route it through the existing mutation-result path so a `403`
     surfaces as a typed forbidden result, distinct from a network error.

  ## TEST PLAN
  - `npm run typecheck` clean; `npm run build` passes `assert-http-bundle` (`VITE_AGG_MODE` UNSET, so
    the shipped default is the real-backend adapter)
  - `vitest` — a 401/404 yields `{available:false}` and never throws; `terminate`'s 403 is typed
    distinctly from a transport failure; a type test that `LiveSession` and `HistorySession` are not
    interchangeable
  - **A grep assertion that no file outside `aggregationClient.ts` calls `fetch` or constructs a
    `WebSocket`**
  - Verify no hardcoded infrastructure values

  ## EDGE CASES
  - Protected route 401s (TERM #549) → `{available:false, detail}` with the 401 surfaced so the panel
    can name the cause instead of showing an empty list
  - `terminate` 403 → typed forbidden, rendered "operator role required", not "failed"
  - A future `maestro.*` method returning textual metadata → caught by the convention test in MACT-09

- **Acceptance criteria:**
  - [ ] `muse.sessions.live()` / `.history()` / `.terminate()` exist with degrade-not-throw semantics
  - [ ] `LiveSession` and `HistorySession` are distinct types carrying a `source` discriminator, with
        a doc comment explaining why they must not be merged
  - [ ] Shapes typed from live captures, not from this document
  - [ ] Mock fixtures cover playing/paused/stale and direct-play/remux/transcode
  - [ ] A 403 from `terminate` is typed distinctly from a transport failure
  - [ ] The two-proxy rule is documented in the module doc comment
  - [ ] Grep proves `fetch`/`WebSocket` appear in no other module
  - [ ] Embedded `dist` rebuilt and committed in the same change
  - [ ] No hardcoded infrastructure values in new/modified code
  - [ ] All existing tests still pass

### MACT-04: The Activity panel — LIVE pane + HISTORY pane
- **Priority:** Critical
- **Labels:** maestro, constellation-web, activity, panel
- **Agent:** claude
- **Estimate:** 8h
- **Blocked by:** MACT-03
- **Description:** The panel, modelled directly on
  `constellation-web/src/panels/terminus/ActivityPanel.tsx` — the house pattern for exactly this shape
  of screen: an `available === null | false | true` tri-state, filter chips built from the data's own
  distinct values, a paged `DataTable`, and an explicit degrade card that names the cause instead of
  rendering an empty table.

  **Two panes, visibly two sources:**
  1. **LIVE — now playing.** One card per live session: poster art, title (+ series/episode), account
     (a Muse account label), device/player, a progress bar with `position / duration`, a state pill
     (playing / paused / **stale**), and a stream-decision badge (Direct play / Remux / Transcode)
     whose tooltip carries the transcode reason. The pane header states its source: "live view derived
     from Muse watch history" in H1, "Maestro live sessions" in H2. Empty ⇒ "Nobody is watching right
     now" — a fact, visually distinct from the degrade card.
  2. **HISTORY — recently watched.** The paged `DataTable` over `/api/sessions/history`, with
     `ActivityPanel`'s filter chips adapted to `account` / `device` / `decision`. Labelled as Muse's
     historical record, which it remains permanently.

  ## FILES
  - `constellation-web/src/panels/maestro/ActivityPanel.tsx` — new
  - `constellation-web/src/panels/maestro/nowPlaying.ts` — pure helpers (progress formatting,
    decision → badge tone, staleness, source labelling), unit-testable without a DOM
  - `constellation-web/src/panels/registerPanels.ts` — register `maestro.activity` at
    `/maestro/activity`; register the `maestro` module descriptor **idempotently** (spec G may register
    it first — whichever lands first wins; the second must not double-register)
  - `constellation-web/dist/**`

  ## APPROACH
  1. Copy `terminus/ActivityPanel.tsx`'s structure deliberately: tri-state, `PanelRoot`, `CardTitle` +
     subtitle, `Card variant="content"`, `SkeletonList` while loading, `DataTable` with distinct
     `emptyMessage`s for "no rows" vs "no rows match this filter".
  2. **Render the live pane's `source` in its header**, so the H2 flip from `muse-derived` to
     `maestro-live` is a visible, explained change rather than a silent identity swap.
  3. Poster art via the same-origin `/art/media_item/{id}` proxy the payload returns (`proxy_muse`
     side of the two-proxy rule). `onError` hides the `<img>` so a missing poster degrades to the card
     background, never a broken-image glyph (S129/MGUI-01; art kinds are `media_metadata` and
     `media_item` ONLY — TERM #550).
  4. Progress bar from the existing `ProgressBar`. `progress_pct` arrives already scaled (MACT-01) —
     the panel must NOT rescale it. Unit-test that.
  5. Decision badge tones from `globals.css` tokens, using the vocabulary discipline
     `SubsystemHealth.tsx` established: an unrecognised value renders **verbatim + "(unclassified)"**,
     never coerced to a friendly default.
  6. `stale` is its own visibly-different state carrying `last_event_at`, not a synonym for paused.
  7. No `<video>`, no HLS library, no media URL — this panel observes; playback is spec G.

  ## TEST PLAN
  - `npm run typecheck`, `npm run build`, `npm run lint:adherence`
  - `vitest` over the pure helpers: progress is not double-scaled; an unknown decision string is
    unclassified, never "Direct play"; a stale session is neither "playing" nor dropped; the source
    label renders from the envelope, not a hardcoded string
  - **Live Playwright capture of `/maestro/activity`**: with a stream running, the visible text
    contains that title and account; with none, the "nobody is watching" state — and NOT a degrade
    card while the endpoint returned 200
  - Feed the screenshot to `review_run` for an outside read of the built page, not just the diff
  - Verify no hardcoded infrastructure values

  ## EDGE CASES
  - 401 (token unprovisioned) → the degrade card NAMES the cause ("Muse session feed requires
    `CONSTELLATION_MUSE_TOKEN`"), not a bare "unavailable"
  - Zero live sessions with a 200 → empty state, never the degrade card
  - Live degrades while history works (or vice versa) → per-pane degradation; one pane never blanks
    the other
  - A session with no resolved item → render device/account/progress with the item marked unresolved
  - Very long titles → truncate with a `title` attribute; the card must not reflow the grid
  - Six simultaneous streams → the card grid wraps and scrolls inside its own container, never the
    page body

- **Acceptance criteria:**
  - [ ] `/maestro/activity` renders a LIVE pane of now-playing cards and a separate HISTORY table,
        from the two distinct endpoints
  - [ ] Each pane displays its own source label, read from the payload's `source`
  - [ ] The two panes degrade independently
  - [ ] Empty (200, no sessions) and degraded (401/unreachable) are visibly distinct, and the degrade
        names its cause
  - [ ] An unrecognised decision value renders unclassified, never as Direct play
  - [ ] `stale` is its own state, distinct from paused
  - [ ] Progress is not double-scaled (unit-tested)
  - [ ] Wrapped in `PanelRoot`, composed from existing `src/components/`, registered in
        `registerPanels.ts` (module registration idempotent)
  - [ ] `npm run lint:adherence` passes; no Tailwind; tokens from `globals.css`
  - [ ] Does NOT claim end-to-end population while `CONSTELLATION_MUSE_TOKEN` is unset
  - [ ] Embedded `dist` rebuilt and committed in the same change
  - [ ] No hardcoded infrastructure values in new/modified code
  - [ ] All existing tests still pass

### MACT-05: Import / acquisition activity section
- **Priority:** Medium
- **Labels:** muse, constellation-web, activity, acquisition
- **Agent:** claude
- **Estimate:** 4h
- **Blocked by:** MACT-04
- **Description:** "What is Muse importing right now." **Surface the existing pipeline state; invent
  no new tracking.** `GET /api/requests/queue` (protected) already returns exactly this — verified in
  `src/web/dashboard.rs::get_requests_queue`: `wanted[]` (monitored, no file) plus `queue[]` across the
  real statuses `queued` / `downloading` / `completed` / `importing`, each row carrying release title,
  indexer, protocol, status, size and `added_at`.

  **The seam that must be rendered honestly:** that handler emits `"progress": Value::Null` behind an
  in-code `// SEAM: real download %/ETA not persisted` comment. qBittorrent progress genuinely is not
  persisted today. This section shows status + size + age and **explicitly says progress is not
  tracked yet** — it does not draw a bar at an invented percentage, and it does not silently drop the
  column as though it were never wanted. Persisting real progress is a follow-up against the
  acquisition worker, not this item.

  ## FILES
  - `constellation-web/src/panels/maestro/ImportActivity.tsx`
  - `constellation-web/src/hooks/useMuse.ts` — reuse/extend the requests-queue hook
  - `constellation-web/dist/**`

  ## APPROACH
  1. Bind `GET /api/requests/queue` through `aggregationClient`; type from a live authenticated
     capture.
  2. Group by status in pipeline order (queued → downloading → importing → completed) so row order
     tells the story.
  3. The progress column renders "not tracked" with a tooltip naming the seam.
  4. `wanted[]` renders as a compact "waiting on a release" count linking to the existing Muse
     requests panel (MGUI-14) rather than duplicating it.
  5. Where `/api/subsystems` already reports library-scan/import wiring state, surface that rather than
     adding a parallel notion of "importing".

  ## TEST PLAN
  - typecheck + build + `lint:adherence` + `vitest` (a null progress renders the seam text and NEVER a
    0% bar)
  - Direct authenticated probe of `/api/requests/queue` to prove the bound shape
  - Proxy capture showing the honest degrade while `CONSTELLATION_MUSE_TOKEN` is unset
  - Verify no hardcoded infrastructure values

  ## EDGE CASES
  - Empty queue (the live reality — `monitored_items` had 0 rows at S129) → an empty state naming that
    nothing is monitored, not a degrade
  - Download client unconfigured (`state.download` is `None`) → the queue is empty for a REASON; name
    it from `/api/subsystems`' wiring state rather than guessing
  - A row stuck in `downloading` for days → show its age; do not editorialise a verdict
  - 401 → degrade card naming the token, consistent with MACT-04

- **Acceptance criteria:**
  - [ ] The section renders live acquisition state from the EXISTING `GET /api/requests/queue`
  - [ ] No new tracking table, worker or endpoint is introduced
  - [ ] Missing download progress is labelled an untracked seam — never a 0% or invented bar
  - [ ] Empty-because-nothing-monitored is distinct from degraded
  - [ ] Embedded `dist` rebuilt and committed
  - [ ] No hardcoded infrastructure values in new/modified code
  - [ ] All existing tests still pass

### MACT-06: Stat tiles — the H1 (Muse-only) set
- **Priority:** High
- **Labels:** muse, constellation-web, activity, dataviz
- **Agent:** claude
- **Estimate:** 4h
- **Blocked by:** MACT-04
- **Description:** The tile row, built from what exists today. **Explicit reuse-vs-new accounting,**
  checked against the tree rather than assumed:

  | Tile | Source | Reused or new |
  |---|---|---|
  | Library size / pending / last ingest | Muse `GET /stats` (**public**, MUSE #84) | **reused, no change** |
  | Library gaps backlog | Muse `GET /gaps` (**public**, untruncated `total`) | **reused** |
  | Muse subsystem wiring | Muse `GET /api/subsystems` (**public**) | **reused** — S129/MGUI-06 already renders the grid |
  | Per-module up/down | Terminus `GET /api/health` | **reused** — drives the module registry |
  | Muse liveness + DB probe | Muse `GET /health` (public) | **reused** |
  | Muse app metrics | Muse `GET /metrics` | **reused, and deliberately NOT used for host stats** — a Prometheus registry of exactly two recommend-engine metrics (`muse_recommend_requests_total`, `muse_recommend_duration_seconds`). It contains **no host CPU/RAM**. Do not mine it for something it does not have. |
  | Live stream count | MACT-01's live list | derived client-side |
  | **Host CPU/RAM · transcodes vs cap · scratch headroom** | **H2 (MACT-11)** | **inert placeholder in H1** |

  In H1 the host/capacity tiles render an **inert seam** ("requires Maestro — not deployed"), not
  zeros and not a spinner. MACT-11 fills them.

  ## FILES
  - `constellation-web/src/panels/maestro/ActivityTiles.tsx`
  - `constellation-web/src/panels/maestro/tileFormat.ts` — pure formatters (byte sizes, counts,
    relative times), unit-tested
  - `constellation-web/dist/**`

  ## APPROACH
  1. Tiles use the existing `MetricCard` primitive and the fleet dataviz stat-tile conventions;
     chart-shaped content uses `ChartCard` with its **`degraded` prop** — never a throw, never a flat
     zero line (`SubsystemHealth.tsx` is the in-tree reference).
  2. **Three states, never conflated** (the honesty rule): a real value including a genuine `0`;
     "not reported" as `—`; degraded. Unit-tested.
  3. Colour is meaning, not decoration: reserve tone for a real warning so a toned tile always means
     "look at this".

  ## TEST PLAN
  - typecheck + build + `lint:adherence`
  - `vitest`: `null` ⇒ `—` (never `0`); the Maestro-dependent tiles render the inert seam, not a
    spinner or a zero; a degraded source uses `ChartCard`'s degraded state rather than throwing
  - Live capture showing real library counts from `/stats`
  - Verify no hardcoded infrastructure values

  ## EDGE CASES
  - `/stats` degrades while `/api/subsystems` works → per-tile degradation, never a whole-row failure
  - Zero live streams → `0` is a FACT here (from a successful 200) and renders as `0`
  - Maestro-dependent tiles in H1 → inert seam naming Maestro, never `0`

- **Acceptance criteria:**
  - [ ] Tiles render library counts, gap backlog, subsystem health and live-stream count from the
        EXISTING public endpoints
  - [ ] `/metrics` is not mined for host stats it does not contain
  - [ ] Host/capacity/scratch tiles render an inert Maestro seam in H1, never `0`
  - [ ] Real-value / not-reported / degraded are three distinct rendered states (unit-tested)
  - [ ] A degraded source uses `ChartCard`'s `degraded` prop, never a throw or a zero plot
  - [ ] Embedded `dist` rebuilt and committed
  - [ ] No hardcoded infrastructure values in new/modified code
  - [ ] All existing tests still pass

### MACT-07: Terminate control — `RoleGate` + proven server-side 403
- **Priority:** High
- **Labels:** maestro, constellation-web, mutation, safety, rbac
- **Agent:** claude
- **Estimate:** 4h
- **Blocked by:** MACT-02, MACT-04
- **Description:** The stop-this-stream control on each now-playing card. The panel's first mutation,
  so the `operator|viewer` split has to be real, not decorative.

  - **Cosmetic:** wrap the control in `RoleGate` — a viewer sees the button disabled with an
    "operator role required" tooltip. Its own doc comment states it is a courtesy layer that curl
    bypasses trivially.
  - **Enforcement:** `enforce_viewer_role_gate` on Terminus's `protected_router` — the router the
    `/api/muse/*path` proxy arm lives on. A viewer's `POST` gets `403` before being proxied anywhere.
    **Proven by a direct viewer POST, not asserted.**

  Because it interrupts a person mid-film it also needs `ConfirmDialog`, naming the Muse account, the
  title and the position it will be stopped at, plus an optional reason.

  ## FILES
  - `constellation-web/src/panels/maestro/TerminateControl.tsx`
  - `constellation-web/src/panels/maestro/ActivityPanel.tsx` — wire the control per card
  - `constellation-web/dist/**`

  ## APPROACH
  1. `RoleGate` wraps the button; `ConfirmDialog` gates the call; the mutation goes through
     `aggregationClient`'s typed `terminate` (never a direct `fetch`).
  2. **Render the real outcome.** MACT-02 returns `{stopped, reason_delivered}`; `stopped:false` shows
     "the player did not stop" — never an optimistic success toast. The list refreshes from the server
     rather than optimistically removing the card, so the panel always shows the truth.
  3. A `403` renders "operator role required" — the same wording as the tooltip — distinguishable from
     a transport failure.
  4. **No bulk "terminate all".** A per-session, individually-confirmed action only; a one-click
     mass-stop on a household media server is a footgun with no matching use case.

  ## TEST PLAN
  - typecheck + build + `lint:adherence`
  - `vitest` — a viewer renders the control disabled; confirm-cancel issues no call; `stopped:false`
    renders the honest failure; a 403 renders the role message
  - **Live proof recorded in the PR:** a viewer-session POST returns
    `403 {"error":"forbidden","required_role":"operator"}`; the same POST as operator succeeds
  - Verify no hardcoded infrastructure values

  ## EDGE CASES
  - Session ends between render and confirm → `404`; the panel refreshes and says the session already
    ended, not "failed"
  - No playback controller → `503` rendered as "no playback controller configured"
  - Double-submit → the control disables while in flight; the second call is a no-op
  - Viewer bypassing `RoleGate` with dev tools → still `403`; that is the whole point

- **Acceptance criteria:**
  - [ ] Each now-playing card has an operator-only terminate control behind `ConfirmDialog`
  - [ ] The control is wrapped in `RoleGate` (cosmetic disable for a viewer)
  - [ ] **A viewer's direct POST is proven to return 403 server-side** — captured output in the PR
  - [ ] `stopped:false` renders honestly; no optimistic success, no optimistic card removal
  - [ ] No bulk-terminate control exists
  - [ ] Embedded `dist` rebuilt and committed
  - [ ] No hardcoded infrastructure values in new/modified code
  - [ ] All existing tests still pass

### MACT-08: Live feed — `/ws` fan-in, with a polling fallback
- **Priority:** High
- **Labels:** terminus, websocket, constellation-web, realtime
- **Agent:** claude
- **Estimate:** 6h
- **Blocked by:** MACT-04
- **Description:** The panel should be live, and the epic prefers the existing `/ws` socket over
  polling. **But `/ws` today relays Harmony only** — `Terminus/src/constellation/ws.rs` dials
  `constellation_harmony_ws_url()`, wraps frames as `{source:'harmony', event:…}`, and its own doc says
  "No fan-in of Chord/Muse events yet (the envelope's `source` field is the seam for that, added when
  those sources exist)." A live feed is therefore a backend change, not a client one.

  This item takes the seam the relay already designed:
  1. **Server:** fan in an activity source emitting `{source:'muse', event:{type:'activity_tick', …}}`
     — a lightweight CHANGE SIGNAL, not the payload.
  2. **Client:** `useWebSocket` already routes construction through `aggregationClient.ws.connect`;
     add `activity_tick` handling that refetches through the client.
  3. **Polling fallback, always present.** The relay already sends typed close frames
     (`4000 NO_UPSTREAM`, `4001 UPSTREAM_LOST`) precisely so the client can fall back.

  **Why a change signal rather than pushing the payload:** the payload is credential-gated per system
  and passes through `mask_response`; pushing it would duplicate the proxy's auth and masking on a
  second path. A tick keeps the single fetch door intact and the socket cheap — and means H2 needs no
  second transport, since Maestro's live pane rides the same tick.

  **Cadence (specified, not left to the implementer):**
  - WS connected: refetch on tick, coalesced to at most **once per 2s**
  - WS unavailable: live pane every **5s**, stat tiles every **10s**, history every **60s**
    (H2 adds transcodes at 5s)
  - Panel not visible (tab hidden / route unmounted): **stop polling entirely** — a panel left open
    overnight must not poll a protected endpoint 17k times

  ## FILES
  - `Terminus/src/constellation/ws.rs` — the activity fan-in source
  - `constellation-web/src/lib/aggregationClient.ts` — the tick event type
  - `constellation-web/src/hooks/useActivityFeedLive.ts` — WS-or-poll with the cadence above
  - `constellation-web/src/types/events.ts` — extend `WsEventType`
  - `constellation-web/dist/**`

  ## APPROACH
  1. Keep the existing masking and bounded-reconnect discipline exactly as it is; the new source rides
     the same envelope and the same `mask_response` path. Do not add a second WS client — `ws.rs`
     documents itself as the single door for the event socket.
  2. The hook exposes `{data, degraded, lastUpdatedAt, live}` so the panel can show "live" vs "polling
     every 5s" honestly and dim when a reading goes stale.
  3. Back off on repeated poll failures (5s → 10s → 30s cap) so a 401 loop does not hammer the proxy.
  4. `document.visibilityState` gates the timer; reconnect + immediate refetch on becoming visible.

  ## TEST PLAN
  - `cargo test` — a relay test that an activity frame is wrapped in the envelope and masked
  - `vitest` — falls back to polling on a close frame; coalesces ticks to ≤1/2s; stops polling when
    hidden; backs off on repeated failures
  - Live: the panel updates within one cadence interval when a stream starts/stops
  - Verify no hardcoded infrastructure values

  ## EDGE CASES
  - Harmony WS URL unconfigured → relay closes `4000`; the client polls; the panel says "polling", not
    "live"
  - Socket connected but the fan-in source silent → the panel still polls on a floor interval rather
    than sitting frozen and looking live
  - Rapid start/stop churn → coalesced refetch, never one fetch per frame
  - Session expiry mid-connection → the poll path's 401 surfaces the degrade; no silent freeze

- **Acceptance criteria:**
  - [ ] The relay fans in an activity source using the existing `source` envelope + masking
  - [ ] The panel updates live over `/ws` when available
  - [ ] Polling fallback engages on any close/failure, at the cadences specified above
  - [ ] Polling stops when the panel is not visible
  - [ ] The panel states which mode it is in ("live" vs "polling every Ns")
  - [ ] No second WebSocket client is introduced anywhere
  - [ ] Embedded `dist` rebuilt and committed
  - [ ] All existing tests still pass

---

# Phase H2 — Maestro live sessions + transcode view (blocked by specs D and E)

H2 begins only once spec **D** (session model) and spec **E** (transcode lifecycle) have landed.
Everything here lives in the **same repository** — `moosenet/Muse`, modules under `src/maestro/`,
binary `src/bin/maestro/main.rs` (epic §2). `src/maestro/` does not exist yet; these are forward
references.

### MACT-09: Flip the LIVE pane to Maestro's `SessionSource`
- **Priority:** High
- **Labels:** maestro, sessions, api, backend
- **Agent:** claude
- **Estimate:** 5h
- **Blocked by:** spec D (`MDLV`) · spec B (`MBAK`: `proxy_maestro`, the `SessionSource` facet,
  `BackendCaps`, `GET /backends`) · MACT-03, MACT-04
- **Description:** The pane swap the LIVE/HISTORY split was built for. Maestro exposes
  `GET /api/sessions/live` over spec B's **`SessionSource`** facet, and the panel's live pane switches
  to it — **history stays on Muse and is untouched.** Because the panes were separate types and
  separate sources from the first commit, this is a source swap plus a label change, not a rewrite.

  Per row: `session_id`, `account_id` (**Muse's account id**, epic §8.1), **`muse_item_id`**,
  `position_ms`/`duration_ms`, player state, client/device + resolved `DeviceProfile` name, the
  **decision tier** from spec C's `plan()` (`direct_play` | `remux` | `partial_transcode` |
  `full_transcode`), the **structured plan reason** (MACT-10 renders it), and `started_at`.

  **No textual metadata, structurally.** Epic §2 forbids `title`/`poster`/`overview`/`year` on
  Maestro's types, enforced by a build-failing CI grep. **A title on a Maestro payload means Maestro
  has grown a metadata cache** — that is what this catches. The panel joins via `proxy_muse`.

  **Branch on `BackendCaps`, never on backend name** (epic §8.6). The response carries the descriptor
  (or the panel reads `GET /backends`), and every conditional render keys off a capability flag.

  ## FILES
  - `src/maestro/api/sessions.rs` — handler + response types
  - `src/maestro/backend/mod.rs` — the `SessionSource` facet's `sessions()`
  - `src/maestro/backend/plex.rs` — the plex adapter's implementation + its honest capability set
  - `constellation-web/src/lib/aggregationClient.ts` — `maestro.sessions.live()`, `maestro.backends()`,
    and the Muse-metadata join helper
  - `constellation-web/src/hooks/useMaestro.ts` — new, mirroring `useMuse.ts`'s shape
  - `constellation-web/src/panels/maestro/ActivityPanel.tsx` — live pane source swap + label
  - `README.md`, `constellation-web/dist/**`

  ## APPROACH
  1. Implement `sessions()` on the `SessionSource` facet; the plex adapter maps `/status/sessions`,
     `native` reads spec D's session store.
  2. Capabilities are a static, reviewed table per adapter derived from what it genuinely populates —
     never a runtime guess that can drift into optimism.
  3. Reuse spec C's decision enum verbatim; do not mint a parallel vocabulary for the API.
  4. Client: batch-resolve titles and art through `proxy_muse` and cache them, so N sessions do not
     become N metadata round-trips per tick.
  5. The live pane's header label now reads `maestro-live`; the swap is visible to the operator,
     which is the entire point of having carried `source` since H1.
  6. Where Muse's derived view and Maestro's live view describe the same stream (both Plex-derived,
     pre-spec-J), Maestro wins — one card per stream, deduped on the backend session key.

  ## TEST PLAN
  - `cargo test` — golden-fixture tests per adapter mapping a captured payload to the canonical shape;
    an unreported field serialises as `null`/`unknown`, never a default; `capabilities` matches what
    the adapter actually populates
  - **A test (and the CI grep) that no Maestro API type carries `title`/`poster`/`overview`/`year`**
  - `vitest` — a convention test asserting `maestro.*` returns no textual metadata and that titles/art
    come from `muse.*`; a grep test that the panel contains no `backend === '...'` comparison
  - Verify no hardcoded infrastructure values

  ## EDGE CASES
  - No backend configured → `{sessions: [], backend: null}` + an all-false capability set
  - Backend unreachable → an explicit degrade marker, never an empty list implying idle
  - `muse_item_id` resolving to nothing → render the session with the item marked unresolved; never
    drop it, and never let Maestro invent a label
  - Clock skew → report positions as the backend gave them, with the observation timestamp

- **Acceptance criteria:**
  - [ ] The LIVE pane is served by Maestro's `SessionSource`; the HISTORY pane still reads Muse and is
        unchanged by this item
  - [ ] The pane's source label flips to `maestro-live` and is visible to the operator
  - [ ] Rows carry `muse_item_id`; **no Maestro type carries a title, poster, overview or year**
        (CI grep green)
  - [ ] `account_id` is the Muse account id, never a cookie-session identity
  - [ ] The panel branches on `BackendCaps`, never on backend name (grep-checked)
  - [ ] Metadata/art come via `proxy_muse` and session state via `proxy_maestro`, proven by a client
        convention test
  - [ ] An unreportable field is `null`/`unknown`, never a plausible default
  - [ ] Embedded `dist` rebuilt and committed
  - [ ] No hardcoded infrastructure values in new/modified code
  - [ ] All existing tests still pass

### MACT-10: Active transcode view + the per-session plan reason
- **Priority:** High
- **Labels:** maestro, transcode, api, constellation-web, diagnostics
- **Agent:** claude
- **Estimate:** 8h
- **Blocked by:** MACT-09 · spec E (`MTRX`) · spec C (`MDEC`, for `plan()` reasons)
- **Description:** What is transcoding right now — the section that answers "why is the fan running",
  and the one an operator will trust most, so it must be sourced from the running job.

  Per active transcode: `session_id` (joins MACT-09), **tier** (remux / partial / full),
  **source → target** per stream (container, video codec, audio codec, channels, resolution),
  **throughput** (realtime speed ratio, fps, output bitrate), **segment window** (produced index,
  client-requested index, and the derived lead/lag in seconds), **encoder** and whether it is CPU or
  GPU, elapsed, `last_progress_at`, and **spec C's structured plan reason**.

  **The plan reason is the operator-facing payoff and is nearly free.** Spec C emits structured
  reasons — "transcoding because: audio TrueHD unsupported on this Cast generation". Rendered beside
  the realtime speed ratio, that pair *is* the answer to "why is the fan running". Render C's reason
  **verbatim**; never paraphrase or compose one client-side.

  **Capability-gated, never zero-filled.** `can_report_transcode_detail` decides whether the segment
  window and encoder render at all. A `plex` backend reports decisions, container, reason and a coarse
  speed and nothing else — so those fields render `—` "not reported by this backend". **A `0.0×` speed
  reads as a stalled transcode and sends someone debugging a healthy server**; that is the specific
  harm this rule prevents.

  **Lead/lag is the operationally meaningful number** — "3 segments ahead" is the difference between a
  healthy transcode and one about to buffer. Derive it from the two indices and report both raw
  indices so the derived number is auditable.

  ## FILES
  - `src/maestro/api/transcodes.rs`
  - `src/maestro/transcode/session.rs` — expose the progress the ffmpeg supervisor already parses; do
    not add a second parser
  - `src/maestro/backend/plex.rs` — map Plex's `TranscodeSession` block onto the same shape, marking
    what it cannot report as unsupported
  - `constellation-web/src/panels/maestro/TranscodeDetail.tsx`
  - `constellation-web/src/panels/maestro/transcodeFormat.ts` — pure formatters, unit-tested
  - `README.md`, `constellation-web/dist/**`

  ## APPROACH
  1. Read from spec E's session registry — the supervisor already tracks the subprocess and its
     progress stream. This item EXPOSES that; it must not spawn probes or re-parse logs.
  2. Encoder attribution is a fact for `native` (Maestro chose the encoder) and unknown for `plex`;
     gate it on `can_report_transcode_detail`, not on the backend's name.
  3. Show the plan reason on the session card as well as in the transcode row — the question "why is
     this transcoding" is asked from the card, not from a detail table.
  4. `throughput.realtime_ratio < 1.0` is the "this will buffer" signal. Expose the number; do not
     embed a threshold verdict in the API. The panel tones it.
  5. No progress since `last_progress_at + N` renders as **stalled**, not as running at its last known
     speed.

  ## TEST PLAN
  - `cargo test` — parsing tests over captured ffmpeg progress output; a lead/lag test including the
    negative (client ahead of producer) case; a plex-adapter fixture asserting segment window and
    encoder come back UNSUPPORTED, not zero; a not-yet-started transcode reports `null` throughput,
    not `0x`
  - `vitest` — not-reported ⇒ `—` and never `0.0×`; the below-realtime warning fires; plan reasons
    render verbatim
  - Live: force a transcode with an incompatible client profile on a host that has ffmpeg (the dev box
    does not — epic §11) and capture real codecs and a real speed in the visible text
  - Verify no hardcoded infrastructure values

  ## EDGE CASES
  - Zero active transcodes → true empty (the healthy case), distinct from Maestro absent
  - Plex backend → tier/codecs/reason present; segment window + encoder unsupported, rendered `—`
  - Client seeking → the segment window jumps; report observed indices, do not smooth them
  - A wedged ffmpeg → `last_progress_at` drives the stalled treatment
  - `plan()` reasons absent (pre-C deployment) → omit the reason line rather than showing an empty
    explanation

- **Acceptance criteria:**
  - [ ] `GET /api/transcodes/active` reports tier, source→target codecs, throughput, segment window,
        encoder + CPU/GPU, elapsed, `last_progress_at` and the structured plan reason
  - [ ] The plan reason renders verbatim, on the session card and in the transcode row
  - [ ] Lead/lag is derived from both raw segment indices, and both are in the payload
  - [ ] Fields the backend cannot report render `—` via `can_report_transcode_detail` — never `0.0×`
        and never `0`
  - [ ] A stalled transcode is shown as stalled, not as running at its last speed
  - [ ] Embedded `dist` rebuilt and committed
  - [ ] No hardcoded infrastructure values in new/modified code
  - [ ] All existing tests still pass

### MACT-11: Maestro host + capacity stats, filling the H1 tile placeholders
- **Priority:** Medium
- **Labels:** maestro, metrics, api, constellation-web
- **Agent:** claude
- **Estimate:** 5h
- **Blocked by:** MACT-10, MACT-06
- **Description:** Fill the three tiles MACT-06 left as an inert seam: host CPU/RAM, active transcodes
  vs cap, and segment-scratch headroom.

  **Reuse first.** Epic §10b already mandates Maestro Prometheus metrics (active sessions, tier
  distribution, transcode realtime-ratio, segment latency, reap counts, event-delivery failures) —
  those are the counts, and this item must not duplicate them with a parallel tally. What §10b does
  NOT provide is host sampling and scratch headroom, so `GET /api/system/stats` adds exactly that:
  `{host: {cpu_pct, load_avg, mem_used_mb, mem_total_mb, scope}, transcodes: {active, cap},
  scratch: {path_label, used_bytes, free_bytes, total_bytes}, uptime_secs, backend}`.

  Maestro is the honest owner: it is the only Constellation component that holds CPU (and optionally
  GPU) for minutes at a time (epic §2.2), and it owns the scratch dir.

  ## FILES
  - `src/maestro/api/system.rs`
  - `src/maestro/system/sampler.rs` — a small cached sampler (`/proc` + cgroup)
  - `constellation-web/src/panels/maestro/ActivityTiles.tsx` — replace the H1 seam placeholders
  - `README.md`, `constellation-web/dist/**`

  ## APPROACH
  1. Sample on a short cache (~2s) so a polling panel cannot become a load source.
  2. **Report the cgroup's own limits where they exist**, not the bare host's — Maestro runs capped,
     and showing a 96GB host when the cgroup allows 8GB is a number that will be misread at exactly
     the wrong moment. State which was used (`scope: "cgroup" | "host"`) and surface it on the tile.
  3. `scratch.path_label` is a LABEL, never an absolute path (S1). Epic §10b also requires the scratch
     not live on a removable card-backed volume; if the sampler can cheaply detect that, surface it as
     a warning.
  4. Any unavailable sample is `null` with a per-field reason, never `0`; `cap: null` renders "no cap",
     never `/0`.

  ## TEST PLAN
  - `cargo test` — sampler parsing against captured `/proc` + cgroup fixtures; unreadable source ⇒
    `null`, not `0`; the cache bounds sampling frequency; no absolute path in the serialised body
  - `vitest` — the tiles switch from the H1 seam to real values; low-headroom tones fire; the scope
    note renders
  - Verify no hardcoded infrastructure values

  ## EDGE CASES
  - Not under a cgroup limit → `scope: "host"`, stated in the payload and on the tile
  - Scratch dir not yet created → `null` sizes with a reason, not "0 bytes free"
  - Cap unset/unbounded → `cap: null` ⇒ "no cap"
  - Sampling failure → per-field `null`; the rest of the payload still returns

- **Acceptance criteria:**
  - [ ] `GET /api/system/stats` returns host CPU/RAM, transcodes active-vs-cap and scratch headroom
  - [ ] Counts reuse the §10b Prometheus metrics rather than a parallel tally
  - [ ] Cgroup limits preferred over host figures, with `scope` in the payload and on the tile
  - [ ] No absolute filesystem path is serialised
  - [ ] An unavailable sample is `null` with a reason, never `0`
  - [ ] Sampling is cached so polling cannot amplify load
  - [ ] Embedded `dist` rebuilt and committed
  - [ ] No hardcoded infrastructure values in new/modified code
  - [ ] All existing tests still pass

### MACT-12: Degradation + honesty verification gate
- **Priority:** High
- **Labels:** maestro, constellation-web, degradation, verification
- **Agent:** claude
- **Estimate:** 4h
- **Blocked by:** MACT-11
- **Description:** The closing item. Verify — against the live deployment, in each configuration —
  that the panel degrades as epic §9 clause 2 requires (inert, never broken) and never presents an
  unknown as a fact.

  | Configuration | Required behaviour |
  |---|---|
  | **Maestro absent** (H1's shipped state, and any Maestro outage) | Transcode + host/capacity sections inert with a "Maestro is not deployed" seam. The LIVE pane falls back to the Muse-derived source **with its label saying so**; history, imports and library tiles keep working. Never blank, never an error. |
  | **Maestro present, `plex` backend** | Sessions + tier + codecs + plan reason render. Segment window, encoder and CPU/GPU render `—` "not reported by this backend", driven by `can_report_transcode_detail`. **Never `0`, never `0.0×`.** |
  | **`CONSTELLATION_MUSE_TOKEN` unset** (today's reality, TERM #549) | Session/import panes show the degrade card NAMING the token. Public tiles (`/stats`, `/gaps`, `/api/subsystems`, `/health`) still populate. |
  | **Everything configured, nothing playing** | "Nobody is watching right now" + "nothing transcoding" — true-empty states, visually distinct from every degrade above. |

  ## FILES
  - `constellation-web/src/panels/maestro/*` — degrade-path fixes the captures find
  - `constellation-web/src/panels/maestro/degradation.test.ts` — the matrix as unit tests
  - `README.md` / the panel doc comment — record the matrix so the next reader does not re-derive it
  - `constellation-web/dist/**`

  ## APPROACH
  1. Drive each configuration with the mock adapter for the unit tests; capture the reachable ones live
     with the Playwright harness.
  2. **Assert on captured text and the API trace, not on a screenshot's existence.** A panel that
     renders its empty state while its endpoint returned 200 with rows is a FAILURE (the S129 rule);
     so is one that renders `0` where the payload carried `null`.
  3. Feed the captures plus this matrix to `review_run` so an outside reviewer judges the built page
     against the requirement, not just the diff.
  4. Fix what the captures find in this item rather than filing it forward — this is the gate that
     makes the honesty rule real.

  ## TEST PLAN
  - `vitest` — one test per matrix row against mock fixtures
  - Live Playwright captures for the reachable configurations, with API traces attached
  - `npm run typecheck`, `npm run build` (`VITE_AGG_MODE` unset), `npm run lint:adherence`
  - Grep: no `fetch`/`WebSocket` outside `aggregationClient.ts`; no `backend === '...'` branch in any
    panel; the Maestro-types metadata grep is green; no hardcoded infrastructure values

  ## EDGE CASES
  - Maestro healthy but its transcode endpoints 404 (partial deploy) → per-section degrade, not a
    whole-panel failure
  - Maestro dies mid-session → the live pane falls back to the Muse-derived source and **says so**;
    it never keeps rendering Maestro's last frame as current
  - `/ws` open but the fan-in source silent → the panel shows "polling", never a frozen "live"
  - Every source degraded at once → the panel still renders its shell, header and section headings
  - A backend that later gains a capability → the descriptor drives the render, so no panel change is
    needed (assert with a fixture flipping one capability to true)

- **Acceptance criteria:**
  - [ ] All four configurations verified, with captured text + API traces recorded in the PR
  - [ ] Maestro absent ⇒ transcode/host sections inert; the live pane falls back to the Muse-derived
        source with an honest label; the panel still renders
  - [ ] Plex backend ⇒ unreportable fields render `—` with the reason, never `0`/`0.0×`
  - [ ] Token unset ⇒ named degrade for protected panes; public tiles still populate
  - [ ] True-empty states are visibly distinct from every degrade state
  - [ ] The matrix is captured as unit tests and documented in the panel doc
  - [ ] Embedded `dist` rebuilt and committed
  - [ ] All existing tests still pass

---

## Sequencing

```
H1 — no Maestro, ships before A and B
  MACT-01 (live + history endpoints) ──> MACT-02 (terminate API) ─────────────┐
        └──> MACT-03 (typed client) ──> MACT-04 (panel: 2 panes) ──┬──────────┴─> MACT-07 (terminate control)
                                                                   ├─> MACT-05 (import activity)
                                                                   ├─> MACT-06 (stat tiles)
                                                                   └─> MACT-08 (live feed / polling)

H2 — blocked by D + E
  spec B/D/E ──> MACT-09 (LIVE pane flips to Maestro SessionSource)
                       └──> MACT-10 (transcodes + plan reason) ──> MACT-11 (host stats) ──> MACT-12 (gate)
```

MACT-01..08 have **no upstream spec dependency** and are one sprint — per epic §4 this is the first
thing shipped in S130. MACT-09..12 begin only after specs D and E land. Spec **J** may land in
between; it changes who writes `play_sessions`, not what MACT-01 serves.

## Deliberately out of scope

- **Playback of any kind.** No `<video>`, no HLS library, no media URL, no Cast sender. This panel
  observes. In-browser playback is spec **G** (and needs spec **D**); the Cast receiver is spec **K**.
- **Per-user identity.** Muse has one shared bearer (epic §5, §8.1). This spec renders the Muse
  `account_id` the tracker already records; a real identity service is its own spec.
- **Changing who observes Plex sessions.** That is spec **J** (epic §8.8); MACT-01 is deliberately
  agnostic so J can land without touching it.
- **Live qBittorrent download progress.** `get_requests_queue` emits `progress: null` behind an
  in-code SEAM comment. MACT-05 renders that honestly; persisting real progress is a follow-up against
  the acquisition worker.
- **Historical analytics** (watch-time charts, per-account leaderboards). This is a *right now* panel
  plus a recent-history strip; household analytics already live at `/api/graph/*`.
- **Bandwidth-per-stream and remote-vs-local classification.** Not reliably available from the current
  session model; inventing it is exactly what the honesty rule forbids.
- **Transport control** (pause/seek/volume on someone else's stream). `CastController` lacks seek and
  volume; spec B's `DeviceControl` facet and spec G own that surface. Terminate needs only the
  existing `stop`.
- **Credential provisioning** (`CONSTELLATION_MUSE_TOKEN`, `CONSTELLATION_MAESTRO_TOKEN`, and the
  Maestro → Muse token of epic §10b). Operator actions in <secret-manager>, tracked outside this spec.
