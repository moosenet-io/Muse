# S130-B — Maestro: sidecar binary, the `PlaybackBackend` trait, and the plex adapter

plane_project: MUSE
module: Muse
prefix: MBAK
spec_id: S130-B-maestro-backends

## Metadata
- **Author:** Moose
- **Session:** S130
- **Date:** 2026-08-01
- **Module version:** Muse v0.1.0 → v0.2.0 (adds the `maestro` binary target)
- **Estimated total:** ~52h autonomous agent work (13 items)
- **North-Star layer:** module
- **Module-Contract:** meets §4 clauses 1–7, with **one documented carve-out that this spec does not
  yet exercise**: clause 1 covers the control plane (through `proxy_maestro`), and when the *native*
  backend eventually serves bytes (spec D) that media plane is served **direct from Maestro** (epic
  §8.6 corollary) — routing sustained video through the tool-hub process would couple playback
  uptime to Terminus restarts and trade away the crash isolation this epic exists to buy. **In this
  spec no bytes flow through Maestro at all** (§0.5b), so clause 1 holds without qualification today.
  Clause 2 via `/healthz`+`/readyz`+`GET /backends`; clause 3 via the durable play-event outbox;
  clause 4 via the enumerated `maestro_*` tool family (MBAK-12) — **Lumina can drive playback in
  phase 1**; clause 5 deferred to spec G (no UI here); clause 6 by construction; clause 7 via the
  `plex` backend.
- **Parent epic:** `specs/S130-maestro-epic.md`. Every §7 standing constraint and every §10b
  cross-cutting requirement applies verbatim.
- **Depends on:** nothing to start. **Sequencing note:** epic §8.8 assigns Plex-session ownership to
  **spec J**, which "must land before or with B, not after" — MBAK-07's plex adapter becomes the sole
  Plex session observer, subsuming `src/tracker/poller.rs`. B does **not** perform that cutover; see
  §0.6.
- **Blocks:** S130-C (`MDEC`) — this spec does **not** define `DeviceProfile`, which now lives in the
  shared media core per epic §2b; S130-D (`MDLV`) implements the `MediaSource` facet, the stream
  route, and signed URLs on top of MBAK-04's trait and `StreamToken` seam, **and is additionally
  gated on MBAK-14** (the workspace split, epic item `W`); S130-G/H build against the plex backend
  delivered here — as a **remote-control + now-playing** surface, not a video element (§0.3).
- **Context:** The epic's central claim is that Muse "integrates with an existing media server OR is
  one." Nothing makes that claim true until there is one interface behind which Plex and a future
  native engine are interchangeable, **and one place the bytes flow through**. This spec stands up
  the Maestro binary — a second `[[bin]]` in this repo, per epic §2 — that interface, the plex
  adapter, and the always-Maestro data plane. Get the seam wrong here and specs D/E either contort to
  fit it or quietly bypass it.

---

## §0. Design rationale (read before implementing any item)

### 0.1 Same repo, separate process — and what that changes

Per epic §2, **Maestro is not a new repo.** It is a second binary target in `moosenet/Muse`:

- `Cargo.toml` grows a second `[[bin]]` (`maestro`, at `src/bin/maestro/main.rs`); Maestro's own
  modules live under `src/maestro/`.
- `src/config.rs`, `src/error.rs`, `src/models/`, `src/repo/`, and the shared media core
  `src/media/` (epic §2b) are **shared**. One `Cargo.lock`, one review pipeline, one mirror, one KG,
  and — the point — one definition of what a media item is.
- Crash isolation comes from the **process** boundary: two systemd units, two cgroups. A wedged or
  OOM-killed ffmpeg takes down `maestro.service` and nothing else.

Simpler than a cross-repo split: MBAK-06/07 do not port `src/plex_control/` anywhere — they extend
and wrap it in place, which is what "reuse, do not rewrite" actually means when there is one tree.

Harder, and this is the risk the spec must defend against: **sharing a repo must not become sharing
ownership.** The compiler will happily let the `maestro` binary call `repo::media_item::upsert`.
Epic §2's structural mechanisms — schema rule, CI greps, the **two-role DB privilege split**, two GUI
proxies — are built in MBAK-08, not left to review discipline.

The privilege split is the sharpest of them, and worth stating here because it shapes several items:
**`maestro_ro`** may `SELECT` library and account tables and has **no grant at all** on taste,
embedding, or play-event tables; **`maestro_rw`** may read *and* write exactly two Maestro-owned
tables (`playback_sessions`, `maestro_event_outbox`) and nothing else. Maestro therefore cannot
read taste even by accident, and cannot write library or watch state even deliberately — while still
fully owning its own session and outbox state. MBAK-14 (the crate split) later adds a compile-time
layer on top of the same boundary; until it lands, **the roles are the enforcement** and the crate
graph is doing no work yet.

### 0.2 The three-facet split (epic §8.6 corollary)

The existing `CastController` in `src/plex_control/cast.rs` is a **remote-control** abstraction:
"tell a device that already knows how to fetch media to start item X." Its doc comment explicitly
anticipates a second implementation. That is the right instinct and the wrong shape to generalise
directly, because the native engine (specs D/E) is a **serving** abstraction: "produce the bytes the
client fetches from *us*." A trait that models only remote control forces the native engine to invent
a device registry it does not have. A trait that models only serving cannot be implemented by Plex —
Plex will not hand us its transcode session.

The resolution is **not** a lowest-common-denominator core trait. It is three facets, of which a
backend implements the ones it genuinely has:

| Facet | What it does | plex | native |
|---|---|---|---|
| `MediaSource` | Produce bytes: start a stream session, return a plan with a Maestro-relative URL | **not implemented** (§0.5b) | spec D |
| `DeviceControl` | Start playback on a target device; play/pause/stop/seek/volume/mute/next; list targets | **yes, now** | spec K |
| `SessionSource` | Observe: list active sessions, accept progress reports, end a session | **yes, now** | spec D |

`PlaybackBackend` is the thin core — identity, `probe()`, `capabilities()` — plus three accessors:
`fn media(&self) -> Option<&dyn MediaSource>`, `fn devices(&self) -> Option<&dyn DeviceControl>`,
`fn sessions_facet(&self) -> Option<&dyn SessionSource>`.

**Why facets rather than one fat trait full of `NotImplemented`.** This repo already has the
counter-example in tree: `GoogleCastController` (`src/plex_control/cast.rs:82`) is a complete
`CastController` impl whose every method returns `MuseError::NotImplemented`. It compiles, it
satisfies the type system, and a caller cannot tell it is inert until it calls it and gets an error.
That is exactly what Module Contract clause 2 forbids: the Player panel needs to know whether to
*render* a scrub bar or a cast button, not discover at click time that it errors.

The asymmetry is **real and permanent**, not a not-yet-implemented state: `plex` will never implement
`MediaSource` (§0.5b), and `native` has no device registry until spec K. A facet a backend does not
implement is `None`, and `BackendCapabilities` (§0.4) says so before the UI renders. The
`Option<&dyn ...>` accessor makes the answer unforgeable at the type level.

**Object safety.** `#[async_trait]`, `&self` receivers, no generic methods, no `where Self: Sized`,
no associated types. Facet accessors return `Option<&dyn Trait>`. The registry stores
`Arc<dyn PlaybackBackend>`, and a test coerces every adapter to that type (MBAK-04).

**`DeviceProfile` is NOT defined here.** Epic §2b puts `MediaInfo`, `DeviceProfile` and the pure
`plan()` in the shared `src/media/`, consumed by both Maestro (play time) and Foundry (curation
time). Spec A builds the probe/`MediaInfo`; spec C builds `DeviceProfile`+`plan()`. This spec
**declares its dependency** on `src/media::DeviceProfile` in the `MediaSource` signature and, if C
has not landed when B starts, defines a minimal placeholder struct **in `src/media/`** (never under
`src/maestro/`) with a `TODO(S130-C)` marker, so C fills it in without touching a trait signature.
Defining a second `DeviceProfile` under `src/maestro/` is precisely the double-booking epic §2b calls
the cheapest-now, most-expensive-later mistake in the epic.

### 0.3 `plex` = control + observe. `native` = bytes.

Per epic §8.6 as it now stands:

- **`plex` mode is control + observe.** No bytes flow through Maestro. There is no in-browser
  `<video>` playback of Plex content, and that is a property of the backend, not a gap in this spec.
  Maestro casts to a device, steers transport, and reports what is playing.
- **`native` mode is bytes.** In-browser playback works, and that media plane is served **direct from
  Maestro**, not through the Terminus gateway — routing sustained video through the tool-hub process
  would couple playback uptime to Terminus restarts and destroy the crash isolation the epic exists
  to buy. That is spec D's carve-out to exercise; this spec ships no media plane at all.

**Consequence for spec G.** Phase 1's Player panel is a **remote-control + now-playing** surface, not
a video element (epic §4 says exactly this). It must branch on `capabilities.in_browser_stream`,
**never on the backend's name** — see §0.4.

### 0.4 `BackendCapabilities` — the asymmetry, declared up front

Epic §8.6's corollary names the descriptor exactly, and this spec implements it verbatim:

| Field | Plex | Native (spec D/E) |
|---|---|---|
| `in_browser_stream` | **`false`, permanently** (§0.5b) | `true` |
| `device_cast` | `true` (Plex Companion targets) | `false` until spec K |
| `server_side_transcode_decision` | `true` (Plex decides; we do not) | `false` (we decide, spec C) |
| `seek_during_transcode` | `true` (Plex's problem) | `false` until spec E |
| `syncplay` | `false` (no verified Plex group primitive — see `watch_together/sync.rs`) | `false` until later |
| `can_report_transcode_detail` | `false` | `true` |

Two rules follow, and both are acceptance criteria rather than advice:

1. **`in_browser_stream: false` for plex is a permanent property, not a TODO.** Nothing in this epic
   is scheduled to flip it. Code and comments must not describe it as "not yet".
2. **Every consumer branches on capability, never on backend name.** A `if backend == "plex"` in the
   GUI, the tools, or the API layer is a review rejection — it is the exact coupling the facet split
   exists to remove, and it breaks the moment a second control-only backend appears.

`can_report_transcode_detail: false` is why spec H's Activity panel must render "Plex cannot report
this" rather than displaying zeros as though they were facts. Publishing the descriptor over
`GET /backends` is what lets the GUI and the assistant tools do that honestly instead of discovering
the asymmetry at integration time.

### 0.5 Why jellyfin and emby are cut from this spec (epic §8.5)

An earlier draft of this spec built a jellyfin/emby adapter family with a connect-time capability
probe absorbing their post-fork divergence. **That is cut.** No Jellyfin or Emby server is live in the
household; `JELLYFIN_URL`/`JELLYFIN_TOKEN` exist in `src/config.rs` but are unset and only gate the
unverified `JellyfinSyncPlay` stub. Two adapters written against no test target are dead code with a
maintenance tax, and — the decisive point — the "one adapter family with a capability probe" bet
**cannot be evaluated without a live server to probe**. Writing it now would mean shipping an
unfalsifiable design claim.

They become a follow-up spec, written when a server exists to test against. The
"integrates with an existing media server OR is one" requirement is fully satisfied by
trait + plex + native; it never required three adapters on day one.

**What this spec still owes that future work:** the trait must not acquire Plex-shaped assumptions
that a second adapter would have to fight. `BackendMediaRef` (§0.6) already carries a
`JellyfinItemId` arm for exactly this reason, and `BackendCapabilities` already has a `syncplay`
field no current backend sets true. Reserving the shape costs nothing; building the adapter costs a
sprint against nothing.

### 0.5b Why the Plex byte-proxy is cut — do not propose it again

An earlier draft of this spec contained a twelfth item, **MBAK-09: a Plex reverse-proxy data plane**
— `GET /stream/:session_id` streaming Plex's output through Maestro with full HTTP range passthrough,
so that `start_session` could return a Maestro-relative URL for *every* backend. **It is cut, and the
item number is deliberately left unused so this section is what a reader finds when they go looking
for it.**

The argument for it was genuinely appealing, which is why it needs answering rather than merely
deleting. It ran: every client holds a Maestro URL from day one, so the eventual native swap is
invisible; `X-Plex-Token` never reaches the browser; and session accounting exists in one place.

The argument against it won, on evidence:

- **The surface it proxies is not a contract.** Plex's `transcode/universal/*` endpoints are
  undocumented, token-lifecycle-bound, keepalive-sensitive, and change without notice. Re-streaming
  another server's HLS output means tracking a moving private API with no test target we control.
- **The cost lands in the wrong place.** That is weeks of brittle work spent polishing **the very
  backend the strangler fig exists to replace.** Every hour of it is written off the day the native
  engine lands — and it would sit on the critical path of specs D and G in the meantime.
- **The benefit it buys is smaller than it looks.** "Invisible native swap" only matters for
  in-browser playback, and phase-1 Plex playback is remote-control-to-a-device (epic §4), where the
  bytes never wanted to come through us in the first place. Plex is already delivering to that device
  perfectly well.
- **The token exposure it prevents does not arise.** With no browser stream, no Plex URL is handed to
  a web page at all. The exposure the proxy was defending against is a consequence of the proxy's own
  design goal.

**What survives the cut:** the stream-token seam. Spec D needs session-scoped, expiring stream
tokens for the *native* media plane — a `<video>` element cannot set an `Authorization` header and a
Cast receiver holds no cookie (epic §8.7). MBAK-04 keeps the token type and its mint/validate
functions so D inherits a seam rather than inventing one; it ships **no route and no proxying**, and
the HMAC-signed expiring scheme remains D's to define.

**If a future reader wants to reopen this:** the thing to change is not this spec but epic §8.6, and
the evidence needed is a stable, documented Plex streaming contract we can test against — not an
argument about client elegance.

### 0.6 Item resolution is a `BackendMediaRef`, not a file path

"Muse: item → file path" is native-only. The plex adapter needs `plex_rating_key` (already a column
on `media_items`); a future jellyfin adapter needs an external-ID join. So resolution returns:

```
enum BackendMediaRef {
    FilePath { path: String, media_info: serde_json::Value },
    PlexRatingKey(String),
    JellyfinItemId(String),
}
```

**Transport decision (mine to make, flagged):** epic §8.6 calls this "the Muse resolution API", and
epic §2 puts the runtime data path on a **read-only Postgres query** rather than an HTTP hop. These
are reconciled as: the *type* is the API, the *transport* is the `maestro_ro` pool.
`src/maestro/library.rs` resolves `muse_item_id → BackendMediaRef` with a SELECT over
`media_items`/`media_files`, in-crate, on the hot path. There is no HTTP resolution hop, because
adding one would put a network round trip in front of every playback for no ownership benefit the
read-only role does not already give. The Maestro→Muse HTTP credential (§0.7) still exists and is
still required — it carries **events** (MBAK-10), which must go over HTTP for the single-writer
reason below.

### 0.7 Two credentials, not one (epic §10b)

- **`CONSTELLATION_MAESTRO_TOKEN`** — Terminus → Maestro, injected by `proxy_maestro`. Its value is
  Maestro's own inbound `MAESTRO_API_TOKEN`.
- **`MAESTRO_MUSE_TOKEN`** — Maestro → Muse, for event delivery (and any future HTTP resolution).

Both <secret-manager>-materialised. Epic §11 previously listed only the first; TERM #549 taught this lesson
once already — an unprovisioned token produces 401s that read as "the module is broken."

**`MAESTRO_API_TOKEN` is a name in its own right, not a duplicate** (confirmed with the epic owner;
the epic's credential table now lists it). The fleet convention is a **pair of names for one shared
secret** — the injecting side and the validating side — exactly as `CONSTELLATION_MUSE_TOKEN`
(Terminus, `src/constellation/proxy.rs`) pairs with `MUSE_API_TOKEN` (Muse, `src/http/auth.rs`).
Collapsing the pair to a single name would make Maestro the only module in the fleet whose validating
side reads the injector's variable, which is both surprising and harder to rotate one side at a time.

`MAESTRO_API_TOKEN` is also deliberately **distinct in value from `MUSE_API_TOKEN`**: one shared
secret across two differently-exposed surfaces means a leak of the playback token also grants the
taste/graph/settings surface.

Alongside the two HTTP credentials sit the **two DB DSNs** (`MAESTRO_DATABASE_URL_RO`,
`MAESTRO_DATABASE_URL_RW`, §0.1) — five provisioned names in total, all in MBAK-03.

### 0.8 Why events go over HTTP when resolution goes direct

Play events are POSTed to Muse's ingest surface even though Maestro could reach the table directly.
The interpretation fold (`src/tracker/interpret.rs`) and the watch-state write path live in Muse and
there must be exactly one of them; a second writer that happens to be in the same binary tree is
still a second writer, and watch-state drift corrupts the taste model sitting downstream of it. The
HTTP hop is off the playback hot path — unlike resolution, which is why *that* one goes direct.

Per epic §10b, delivery is **durable**: a local outbox with retry and dedupe keys, and a
**versioned payload (`"v":1`) from the first commit**. A lost stop-event is a corrupted watch
duration, which corrupts taste — the one failure mode that silently damages the product rather than
visibly breaking it. Muse's `tracker/reconstruct.rs` already performs idempotent session
reconstruction; that is the consumer contract.

### 0.9 Spec J dependency — Plex session ownership

Epic §8.8: Maestro's plex adapter becomes the **sole Plex session observer**, and Muse's
`src/tracker/poller.rs` becomes a pure consumer of Maestro's event stream. That cutover is **spec J
(`MTRC`)**, which must land before or with B.

**What B does:** MBAK-07's `sessions()` reads Plex `/status/sessions` — the same upstream
`tracker/poller.rs` reads today. **What B does not do:** disable, rewrite, or repoint the poller.
Until J lands there are transiently two readers of one upstream. Two *readers* is a tolerable,
bounded overlap (both are read-only against Plex); two *writers* to `play_sessions` is the failure
§2 forbids, and MBAK-10 avoids it by routing every Maestro-originated event through Muse's single
ingest writer with dedupe keys. Each item's PR description must say which of the two it is; a change
that has Maestro write `play_sessions` directly is a rejection.

### 0.10 Testing and build-host constraints

- **`ffmpeg`/`ffprobe` are NOT on the dev box** (verified 2026-08-01, epic §11). No item here invokes
  either — that is specs A/D/E — but `/readyz` probes for `ffmpeg` (MBAK-01), so its "missing" branch
  is the one exercised locally and must be tested as a first-class outcome, not an error path.
- The workspace build/test gate goes through the **compiler tool** on a build-capable host; do not
  hand-run a full workspace `cargo test` on the dev box.
- **No test contacts a live Plex or a live Postgres** (S9). Everything uses `httpmock`, the existing
  `src/fixtures.rs` harness, and the `FakeBackend` from MBAK-04 (epic §10b names it).

---

## §1. Items

### MBAK-01: `maestro` binary target, module skeleton, `/healthz` + `/readyz`
- **Priority:** Critical
- **Labels:** muse, maestro, scaffold, rust
- **Agent:** claude
- **Estimate:** 5h
- **Description:** Add the second binary. It boots, serves a **split health surface** and metrics,
  initialises tracing, and reads its config from the **shared** `Config` — reusing `src/error.rs`,
  `src/config.rs`, `src/metrics.rs` rather than growing parallel copies. No playback logic.

  ## FILES
  - `Cargo.toml` — a second `[[bin]]`: `name = "maestro"`, `path = "src/bin/maestro/main.rs"`. No
    new dependencies (axum/tokio/tracing/prometheus/reqwest are already present).
  - `src/bin/maestro/main.rs` — tracing init, `Config::from_env()`, build `MaestroState`, serve on
    `MAESTRO_BIND_ADDR`. Deliberately thin; everything real lives under `src/maestro/`.
  - `src/maestro/mod.rs` — module root, with the §0.1 ownership firewall as its headline doc comment.
  - `src/maestro/http/mod.rs` — `MaestroState` + `router()`; `/healthz`, `/readyz`, `/metrics`
    mounted **unauthenticated**, mirroring `src/http/mod.rs`'s open/protected split.
  - `src/maestro/ready.rs` — the readiness probe.
  - `src/config.rs` — new fields: `maestro_bind_addr`, `maestro_api_token` (`MAESTRO_API_TOKEN`),
    `maestro_auth_disabled`, `maestro_database_url_ro`, `maestro_database_url_rw`,
    `maestro_muse_url`, `maestro_muse_token`.
    Follow existing conventions verbatim: `env_opt()`, doc comment per field, no defaulted secrets.
  - `src/lib.rs` — `pub mod maestro;`
  - `README.md` — a Maestro section: what it is, the two-process model, the ownership split, and
    every new env var with placeholder values.

  ## APPROACH
  1. **Health split (epic requirement).** `/healthz` = process liveness only: 200 as long as the
     process answers, depending on **nothing** external. `/readyz` = 200 only when the readiness
     checks pass, and 503 with a per-check body otherwise. Checks: at least one backend reachable,
     the library mount present, `ffmpeg` found on `PATH`. The panels need to distinguish
     "Maestro up, Plex down" from "Maestro absent" — one endpoint cannot express both.
  2. `/readyz` body: `{"ready":bool,"checks":{"backends":…,"library_mount":…,"ffmpeg":…}}`. Each
     check reports `{ok, detail}`. A failing check is **information**, not an error — this is the
     endpoint the Player panel's capability gate reads.
  3. Deploy gates on `/healthz`, never `/readyz` (MBAK-13): a readiness gate would roll back a good
     deploy every time Plex was down.
  4. `MaestroState` holds only what Maestro needs. It must **not** be `crate::http::AppState` —
     sharing that struct would hand Maestro the Plex/TMDb/Prowlarr/qBittorrent clients and the write
     pool, exactly the ownership leak §0.1 exists to prevent.
  5. Register a `maestro_build_info` metric so `/metrics` is provably non-empty, using the existing
     process-global registry in `src/metrics.rs`.
  6. Maestro binds a **different port** from Muse (`MAESTRO_BIND_ADDR`, with no default that could
     collide with `MUSE_BIND_ADDR`'s `0.0.0.0:8090`).

  ## TEST PLAN
  - `cargo test` — `oneshot` router tests (the harness already used in `src/endpoint_tests.rs`):
    `/healthz` → 200 with `version`; `/metrics` → 200, `text/plain`, non-empty.
  - `/readyz` with **no** backends configured and **no** ffmpeg on `PATH` → 503 with all three checks
    reporting `ok:false` and a readable detail — the dev-box case, tested as a first-class outcome.
  - `/readyz` with a `FakeBackend` reachable and the checks stubbed pass → 200.
  - `Config::from_env()` with nothing set yields `None` for every new field, including for a var set
    to the **empty string** (the existing `env_opt` filter).
  - `cargo build --bins` produces both `muse` and `maestro`.
  - Verify no hardcoded IPs, hostnames, tokens, or emails.

  ## EDGE CASES
  - `MAESTRO_BIND_ADDR` unset — refuse to start with a clear message rather than silently defaulting
    onto Muse's port.
  - Empty-string env var (a common `.env`-from-<secret-manager> artifact) must read as `None`, not
    `Some("")` — the bug class that stalled the Tautulli backfill.
  - `/readyz` must not hang when a backend is unreachable — every check is individually timeout-bounded.
  - `ffmpeg` present but not executable ⇒ the check reports `ok:false` with the reason, not a panic.

- **Acceptance criteria:**
  - [ ] `cargo build --bins` produces both a `muse` and a `maestro` binary from one crate
  - [ ] `/healthz` returns 200 while depending on nothing external; `/readyz` returns 503 with per-check detail when a dependency is missing
  - [ ] Negative test: with no backends and no `ffmpeg`, `/readyz` is 503 with all three checks failing and readable details — and `/healthz` is still 200
  - [ ] Negative test: `MAESTRO_BIND_ADDR` unset exits with a descriptive error rather than defaulting onto Muse's port
  - [ ] Every new config field is `None` when its env var is unset **or** empty
  - [ ] `MaestroState` does not reuse `crate::http::AppState` and holds no Muse integration client
  - [ ] README documents the Maestro binary, the ownership split, and every new env var with placeholders
  - [ ] No hardcoded infrastructure values in new/modified code
  - [ ] All existing tests still pass

---

### MBAK-02: Two credentials, shared bearer auth, and the `proxy_maestro` Terminus door
- **Priority:** Critical
- **Labels:** muse, maestro, terminus, auth, security
- **Agent:** claude
- **Estimate:** 5h
- **Blocked by:** MBAK-01
- **Description:** Give Maestro fail-closed inbound auth by **generalising** Muse's existing
  middleware rather than copying it; wire the Maestro→Muse outbound credential; and add the single
  sanctioned control-plane door: `proxy_maestro` in Terminus, mirroring `proxy_muse`.

  ## FILES
  - `src/http/auth.rs` — generalise `require_api_token` so the token and disable-flag come from the
    caller's state rather than being hardwired to `AppState.config.api_token`. Preserve every
    behaviour: constant-time comparison; configured token ⇒ 401 without a matching header;
    **unconfigured** ⇒ 503, never open; `*_AUTH_DISABLED=1` has no effect once a token is set. Muse's
    existing call site must be behaviourally unchanged.
  - `src/maestro/http/mod.rs` — a `protected` sub-router with the middleware attached via
    `Router::route_layer` (not `layer`), exactly as `src/http/mod.rs` does, so it can never leak onto
    `/healthz`/`/readyz`/`/metrics`.
  - `src/maestro/muse_client.rs` — the outbound client skeleton holding `MAESTRO_MUSE_URL` +
    `MAESTRO_MUSE_TOKEN`; `None` when unconfigured (MBAK-10 gives it its send path).
  - **Terminus:** `src/constellation/proxy.rs` — `proxy_maestro` + a pure
    `maestro_upstream_headers(token: Option<String>) -> Vec<(&'static str, String)>`, modelled on
    `muse_upstream_headers`/`proxy_muse`.
  - **Terminus:** `src/config.rs` — `constellation_maestro_url()` / `constellation_maestro_token()`,
    point-of-use `env_nonempty` reads, mirroring the Muse pair.
  - **Terminus:** `src/constellation/mod.rs` — register `.route("/api/maestro/*path",
    any(proxy::proxy_maestro))` and add `maestro` to the `/api/health` per-system probe list.

  ## APPROACH
  1. Document the **two-credential** model (§0.7) in the auth module doc, including why
     `MAESTRO_API_TOKEN` is distinct from `MUSE_API_TOKEN`.
  2. In `proxy_maestro`, follow the Muse arm's decisions and state the reasoning in code: token
     absent ⇒ no header at all (a token-less dev Maestro keeps working); `auth_failure_detail: None`
     so a real 401 reaches the browser verbatim rather than being masked into an `{available:false}`
     body a panel would render as data — the documented Muse-arm reasoning at `proxy.rs:434-447`.
  3. **`proxy_maestro` carries the control plane only.** This spec routes nothing else through it —
     there is no media plane yet (§0.3, §0.5b). Add a doc paragraph and a `TODO(S130-D)` recording
     that when the *native* backend serves bytes, those stream routes are served **direct from
     Maestro** and must never be proxied here: a future contributor adding a `stream/` arm would
     silently couple playback uptime to Terminus restarts.
  4. The shared `proxy` helper already forwards no inbound header except `content-type`; add a test
     proving it for this arm rather than assuming.

  ## TEST PLAN
  - `oneshot`: protected Maestro route without a header → 401 (token configured); with the correct
    header → 200; token unconfigured → 503; unconfigured **and** `MAESTRO_AUTH_DISABLED=1` → 200;
    configured **and** `MAESTRO_AUTH_DISABLED=1` → still 401.
  - Regression: Muse's existing auth tests pass **unmodified** after the generalisation.
  - Terminus: `maestro_upstream_headers(None)` → empty; `Some(t)` → one lowercase `authorization`.
  - Terminus: `/api/maestro/*` with no configured backend URL → 200
    `{"system":"maestro","available":false}`; a caller-supplied `Authorization` is not forwarded.
  - `MuseClient::from_config` returns `None` when either the URL or the token is missing.

  ## EDGE CASES
  - `CONSTELLATION_MAESTRO_TOKEN` set but wrong ⇒ Maestro 401 forwarded verbatim (diagnostic), not
    masked — the TERM #549 lesson.
  - `CONSTELLATION_MAESTRO_URL` unset ⇒ the standard degraded 200 body, never a 502.
  - `MAESTRO_MUSE_TOKEN` unset ⇒ the client is `None`; MBAK-10's outbox must degrade rather than
    spin. Assert the degrade path here so it is not discovered in MBAK-10.
  - Query-string and trailing-slash preservation through `build_target` (the agy CONST-02 finding).

- **Acceptance criteria:**
  - [ ] Muse's existing auth tests pass unmodified after the middleware is generalised
  - [ ] A protected Maestro route rejects an unauthenticated request with 401 before the handler runs
  - [ ] Negative test: `MAESTRO_AUTH_DISABLED=1` does **not** bypass a configured token (still 401)
  - [ ] Unconfigured inbound token yields 503 on protected routes; `/healthz`, `/readyz`, `/metrics` stay open
  - [ ] `GET /api/maestro/<path>` reaches Maestro with an injected bearer; an inbound `Authorization` is never forwarded
  - [ ] Both credentials exist and are distinct: `MAESTRO_API_TOKEN` (inbound) and `MAESTRO_MUSE_TOKEN` (outbound), documented in code
  - [ ] `proxy_maestro`'s doc records that a future native media plane is served direct, never proxied here
  - [ ] README (both repos) documents the new env vars with placeholder values
  - [ ] No hardcoded infrastructure values in new/modified code
  - [ ] All existing tests still pass

---

### MBAK-03: Pre-flight — prefix promotion, both tokens, both DB roles
- **Priority:** Critical
- **Labels:** muse, maestro, infra, human-action
- **Agent:** <operator>
- **Estimate:** 40m
- **Type:** human-action
- **Description:** Operator prerequisites. Epic §10.4 is explicit: an unprovisioned
  `CONSTELLATION_MAESTRO_TOKEN` reproduces TERM #549 — protected routes 401, panels render "not yet
  wired", and it looks like a Maestro bug for days. Epic §10b adds a **second** credential, and epic
  §2 adds **two DB roles**. Five provisioned names in total. **No repo is created** — Maestro lives
  in `moosenet/Muse`.
- **Steps:**
  1. `plane_prefix_promote MBAK` (project `MUSE`) — the prefixes are registered but not yet durable
     in the baseline (epic §11).
  2. Generate a secret and add it to <secret-manager> under **both** `MAESTRO_API_TOKEN` (Maestro's inbound)
     and `CONSTELLATION_MAESTRO_TOKEN` (Terminus injects it). Same value — this pairing is what was
     missing in TERM #549.
  3. Generate a **second, different** secret for `MAESTRO_MUSE_TOKEN` (Maestro → Muse). Its value must
     be accepted by Muse's inbound auth, i.e. it is Muse's `MUSE_API_TOKEN` — record explicitly which
     it is so MBAK-10's 401 path is diagnosable.
  4. **Create both Postgres roles.** This is epic §2's privilege mechanism — a stray write to a
     library table fails at the database, not at code review.

     | Role | DSN | Grants |
     |---|---|---|
     | `maestro_ro` | `MAESTRO_DATABASE_URL_RO` | `SELECT` on `media_items`, `media_files`, `accounts`, `browser_account_map`. **No `SELECT` at all** on taste, embedding, or play-event tables. |
     | `maestro_rw` | `MAESTRO_DATABASE_URL_RW` | `SELECT`, `INSERT`, `UPDATE`, `DELETE` on **only** `playback_sessions` and `maestro_event_outbox`. |

     **`maestro_rw` must include `SELECT`.** `maestro_ro` has no grant on those two tables, so a
     write-only `maestro_rw` would fail at the first session read or outbox poll. Do not trim it to
     the write verbs — that exact narrowing was caught in review.

     Both roles' grants are asserted by MBAK-08's startup probe, including the negatives, so a
     mis-provisioned role shows up as a boot-time warning rather than a mystery at load. If either
     role cannot be created, say so explicitly: MBAK-08's code-level surface and CI greps are then
     carrying the invariant alone, which is weaker.
  5. Add `CONSTELLATION_MAESTRO_URL` to the Terminus environment, and `MAESTRO_BIND_ADDR` +
     `MAESTRO_MUSE_URL` to the Muse host's materialized env.
  6. Confirm the chosen Maestro host (epic §10b: alongside Muse for CPU tiers) has the read-only
     library mount and `ffmpeg`/`ffprobe`. Not needed by this spec, but `/readyz` reports on it and
     specs A/D/E depend on it.
  7. Verify: `/api/maestro/healthz` through an authenticated Terminus session returns Maestro's own
     body, not a degraded one; and `/api/maestro/readyz` reports honestly.

---

### MBAK-04: The `PlaybackBackend` trait, `BackendCapabilities`, `BackendMediaRef`, `FakeBackend`
- **Priority:** Critical
- **Labels:** muse, maestro, architecture, trait
- **Agent:** claude
- **Estimate:** 6h
- **Blocked by:** MBAK-01
- **Description:** The load-bearing item. Define the trait and its types, with the §0.2/§0.3/§0.4
  rationale written into the module doc comment so every reviewer of specs D/E/G/H can see why the
  seam is shaped this way. No adapters, no HTTP — types, trait, and the `FakeBackend` epic §10b
  requires.

  ## FILES
  - `src/maestro/backend/mod.rs` — `PlaybackBackend` (the thin core) plus the three facet traits
    `MediaSource`, `DeviceControl`, `SessionSource`, and the §0.2–§0.5b rationale as the module doc.
    Re-exports.
  - `src/maestro/backend/types.rs` — `BackendId`, `BackendCapabilities`, `PlaybackSession`,
    `DeviceStartRequest`, `StreamRequest`, `PlaybackPlan`, `DeliveryMethod`, `ProgressReport`,
    `Target`, `SeekTarget`.
  - `src/maestro/stream_token.rs` — the surviving seam from the cut item (§0.5b): a `StreamToken`
    type with mint/validate bound to `(session_id, expiry)`. **No route, no proxying, no HMAC** —
    spec D adds the route and replaces the opaque form with the signed expiring scheme (epic §8.7).
  - `src/media/mod.rs` (shared, epic §2b) — `BackendMediaRef`; and, **only if spec C has not landed**,
    a minimal placeholder `DeviceProfile` with a `TODO(S130-C)` marker. Never define either under
    `src/maestro/`.
  - `src/maestro/backend/fake.rs` — `FakeBackend` (epic §10b): configurable capabilities, selectable
    facets, and canned sessions, used by GUI, session, and registry tests so nothing needs a live
    server (S9). Available to integration tests, not just `#[cfg(test)]` unit tests, since spec G
    needs it. It must be able to model **both** shapes: control-only (like plex) and
    media-only (like native pre-K).

  ## APPROACH
  1. Core trait (`#[async_trait]`, `Send + Sync`, object-safe), returning `MuseResult<_>` — one error
     vocabulary across both binaries:
     - `fn id(&self) -> BackendId` — `plex` | `native` (the enum reserves `jellyfin`/`emby` per §0.5).
     - `fn capabilities(&self) -> BackendCapabilities` — cheap, never blocks a render.
     - `async fn probe(&self) -> MuseResult<BackendCapabilities>` — connect-time handshake; refreshes
       the cached descriptor.
     - `fn media(&self) -> Option<&dyn MediaSource>`
     - `fn devices(&self) -> Option<&dyn DeviceControl>`
     - `fn sessions_facet(&self) -> Option<&dyn SessionSource>`
  2. **`MediaSource` — native only, no implementor in this spec.**
     - `async fn start_stream(&self, req: &StreamRequest) -> MuseResult<PlaybackPlan>` where
       `StreamRequest { muse_item_id, media_ref: BackendMediaRef, profile: &DeviceProfile,
       account_id: Option<String>, offset_ms: i64 }`.
     The trait is **defined** here so spec D implements it without a signature negotiation, and it is
     **not implemented** by anything B ships. `PlexBackend::media()` returns `None` — permanently
     (§0.5b), not pending. Say that in the trait's doc so nobody "completes" it later.
  3. **`DeviceControl` — plex now, native at spec K.**
     - `async fn start_on_device(&self, req: &DeviceStartRequest) -> MuseResult<PlaybackSession>`
       where `DeviceStartRequest { muse_item_id, media_ref, target, account_id, offset_ms }`. Note
       there is no `DeviceProfile`: the device and its server negotiate that themselves, which is
       exactly what makes plex a control backend.
     - `play`, `pause`, `stop`, `seek_to`, `set_volume`, `mute`, `next` — see MBAK-06 for why the
       last three are new work.
     - `async fn list_targets(&self) -> MuseResult<Vec<Target>>`.
  4. **`SessionSource` — both.**
     - `async fn sessions(&self) -> MuseResult<Vec<PlaybackSession>>`
     - `async fn report_progress(&self, report: &ProgressReport) -> MuseResult<()>`
     - `async fn end_session(&self, session_id: &str) -> MuseResult<()>`
  5. `BackendCapabilities` is **exactly** epic §8.6's six fields — `in_browser_stream`,
     `device_cast`, `server_side_transcode_decision`, `seek_during_transcode`, `syncplay`,
     `can_report_transcode_detail` — plus `server_version: Option<String>` and
     `supported_delivery: Vec<DeliveryMethod>`. `Serialize`; `GET /backends` (MBAK-11) publishes it.
     Do not invent extra fields: the GUI and the assistant tools consume this exact set.
     **`in_browser_stream` must equal `media().is_some()`** — that is the invariant tying the honest
     signalling to the type system, and it is directly testable.
  6. `PlaybackPlan { session_id, stream_url, container, method: DeliveryMethod, expires_at }` is
     produced **only** by `MediaSource`, and its `stream_url` is Maestro-relative. A control-only
     backend never constructs one — starting playback there yields a `PlaybackSession`, not a plan.
     `DeliveryMethod` = `DirectPlay | Remux | PartialTranscode | FullTranscode | Unknown` (the epic
     §6 tiers). The `RemoteDelegated` variant an earlier draft added for the byte-proxy is **not**
     included: with the proxy cut there is nothing for it to describe, and a control-started session
     reports its delivery method as `Unknown` because Plex does not tell us
     (`can_report_transcode_detail: false`).
  7. `PlaybackSession` carries `account_id: Option<String>` from day one — and, per epic §8.1's
     **corrected** decision, it is **Muse's account id**, the id-space the taste model already uses,
     **not** the constellation-web cookie session (which carries roles, not household members).
     Document that in the field's doc comment; a third id-space is how taste attribution silently
     fails to join.
  8. **Structural anti-metadata (epic §2 mechanism 1+2):** no type in `src/maestro/` may carry
     `title`, `poster`, `overview`, `year`, or `artwork`. Payloads reference `muse_item_id` only.
     MBAK-08 adds the CI grep; this item establishes the rule in the doc and obeys it.
  9. Write §0.2 into the module doc, including the `GoogleCastController` counter-example and the
     §0.5b cut, so both "why facets, not one fat trait" and "why plex has no `MediaSource`" are
     answered in-tree and do not get relitigated per review.

  ## TEST PLAN
  - `cargo test` — object-safety proof: `let _: Arc<dyn PlaybackBackend> = Arc::new(FakeBackend::default());`
  - **Facet/capability agreement, asserted in both directions:** `in_browser_stream == media().is_some()`,
    `device_cast == devices().is_some()`. A `FakeBackend` configured to lie fails the test.
  - A control-only `FakeBackend` (the plex shape) returns `None` from `media()` and still serves
    `devices()` and `sessions_facet()`.
  - A media-only `FakeBackend` (the native shape) returns `None` from `devices()` and still serves
    `media()` and `sessions_facet()`.
  - `BackendCapabilities` serializes to exactly the epic §8.6 field names — golden fixture, so a
    rename is a visible test break rather than a silent contract break for panels and tools.
  - `PlaybackPlan.stream_url` is asserted relative (no scheme/host) wherever a plan is produced.
  - `StreamToken` mint→validate round-trips; a tampered or expired token fails validation.
  - `BackendMediaRef` round-trips through serde with all three arms.
  - Verify no `title`/`poster`/`overview`/`artwork` field exists on any type in `src/maestro/`.

  ## EDGE CASES
  - `probe()` fails ⇒ `capabilities()` returns an all-false descriptor with the error recorded —
    never a panic, and never stale-true values that would make the UI render a dead transport bar.
  - A caller asking a control-only backend for a stream ⇒ the **facet is absent**, so the API layer
    answers 409 before dispatch (MBAK-11). There is no method to call and fail, which is the point of
    the split.
  - A backend that supports transport but not volume (a bare Cast target) ⇒ `set_volume` returns
    `NotImplemented`; capability flags, not errors, are what the UI reads.
  - `BackendMediaRef::JellyfinItemId` has no consumer yet (§0.5) — it must be constructible and
    round-trippable, and every `match` over the enum must handle it explicitly rather than via a
    catch-all, so the future adapter gets compiler help instead of silent fallthrough.
  - `StreamToken` has no consumer in this spec — it must not acquire a route, an HMAC, or a config
    key here (§0.5b). A test asserting the module exposes only mint/validate keeps it honest.

- **Acceptance criteria:**
  - [ ] `PlaybackBackend` is object-safe and exposes exactly three facet accessors: `media()`, `devices()`, `sessions_facet()`
  - [ ] `in_browser_stream == media().is_some()` and `device_cast == devices().is_some()`, asserted in both directions
  - [ ] Negative test: a control-only `FakeBackend` returns `None` from `media()` while still serving `devices()` and `sessions_facet()`
  - [ ] `MediaSource` is defined but has **no implementor** in this spec, and its doc states plex will never implement it
  - [ ] `PlaybackPlan` is producible only via `MediaSource`; no `RemoteDelegated` delivery variant exists
  - [ ] `BackendCapabilities` carries exactly epic §8.6's six fields and serializes to those names (golden fixture)
  - [ ] `StreamToken` mint/validate round-trips, rejects a tampered or expired token, and ships no route or HMAC
  - [ ] `account_id` is documented as Muse's account id, explicitly not the constellation-web session
  - [ ] `DeviceProfile` lives in shared `src/media/`, never under `src/maestro/`
  - [ ] No type under `src/maestro/` carries a textual-metadata field (epic §2 mechanism 1)
  - [ ] No hardcoded infrastructure values in new/modified code
  - [ ] All existing tests still pass

---

### MBAK-05: Backend registry, per-request policy, and the kill switch
- **Priority:** High
- **Labels:** muse, maestro, config, resilience
- **Agent:** claude
- **Estimate:** 4h
- **Blocked by:** MBAK-04
- **Description:** Build the live backend set from the shared config, and make backend selection
  **per-request policy, not a compile-time switch** (epic §10b) — so one named device can be routed
  through `native` while the household stays on `plex`, with a one-line kill switch back.

  ## FILES
  - `src/maestro/backend/registry.rs` — `BackendRegistry` over `HashMap<BackendId, Arc<dyn
    PlaybackBackend>>`; `from_config()`, `get()`, `select(request)`, `status()`.
  - `src/maestro/backend/policy.rs` — the selection policy: explicit per-request `backend` >
    per-device override (`MAESTRO_BACKEND_OVERRIDES`, a `device=backend` list) >
    `MAESTRO_DEFAULT_BACKEND` > sole configured backend > `None`.
  - `src/config.rs` — **reuse the existing** `plex_url`/`plex_token`; add `maestro_default_backend`
    and `maestro_backend_overrides`.
  - `src/maestro/http/mod.rs` — `/readyz`'s `backends` check reflects `registry.status()`.

  ## APPROACH
  1. `from_config()` constructs an adapter only when *all* required config values are present;
     otherwise that backend is absent. Log one `info` line per skipped backend naming the missing var
     — never its value.
  2. Probe each constructed backend once at startup, concurrently, bounded. A probe failure does
     **not** remove the backend and does **not** fail startup: it is recorded `reachable: false` and
     re-probed lazily, with a short negative-cache window so a down server does not add latency to
     every request.
  3. **`MAESTRO_DEFAULT_BACKEND=plex` is the documented kill switch** (epic §10b) — a one-line revert
     for any native rollout. Say so in the config field's doc comment; that is the concrete mechanism
     behind epic §8.2's claim that Plex retirement stays a later decision.
  4. No default backend and no explicit `backend` on the request ⇒ **400 listing available ids**,
     never an arbitrary pick.
  5. An empty registry is legal and non-fatal: `/healthz` 200, `/readyz` 503 with the backends check
     failing, panels render inert (Module Contract clause 2).
  6. Reusing `plex_url`/`plex_token` means Muse's tracker/composer and Maestro's plex backend read the
     same credentials — correct, and worth a doc line so nobody adds `MAESTRO_PLEX_URL`.

  ## TEST PLAN
  - Zero configured backends: registry constructs, `status()` empty, `/healthz` 200, `/readyz` 503.
  - Policy table test, one case per precedence level, including a per-device override beating the
    default and an explicit request beating the override.
  - `MAESTRO_DEFAULT_BACKEND` naming an unconfigured backend ⇒ startup warning, `None` default; no
    panic, and **no silent substitution** of another backend.
  - A backend whose `probe()` errors stays in the registry with `reachable: false`; a sibling
    backend's calls are unaffected.
  - Malformed `MAESTRO_BACKEND_OVERRIDES` ⇒ the malformed entry is skipped with a warning and the
    rest still parse (never a startup failure over a config typo, never a silent whole-list drop).

  ## EDGE CASES
  - An override naming an unconfigured backend ⇒ that device falls through to the default with a
    warning, rather than failing playback outright.
  - A backend URL with a trailing slash — normalized once at construction, as `PlexControlClient::new`
    already does via `trim_end_matches('/')`.
  - Credential present but URL missing (or vice versa) ⇒ backend absent; the warning names the
    missing var only.

- **Acceptance criteria:**
  - [ ] Backend selection is per-request policy with the documented precedence, covered by a table test
  - [ ] `MAESTRO_DEFAULT_BACKEND=plex` works as a one-line kill switch and is documented as such
  - [ ] Negative test: `MAESTRO_DEFAULT_BACKEND` naming an unconfigured backend does not panic and does not silently substitute another
  - [ ] Negative test: a malformed override entry is skipped with a warning; the rest still parse
  - [ ] An unreachable backend does not fail startup and does not degrade a sibling backend
  - [ ] Zero configured backends is legal: `/healthz` 200, `/readyz` 503 with a failing backends check
  - [ ] Maestro reuses the existing `PLEX_*` config fields rather than adding parallel ones
  - [ ] README documents every backend env var with placeholder values
  - [ ] No hardcoded infrastructure values in new/modified code
  - [ ] All existing tests still pass

---

### MBAK-06: Generalise `CastController` — add `seek_to`, `set_volume`, `mute`
- **Priority:** High
- **Labels:** muse, maestro, plex, transport
- **Agent:** claude
- **Estimate:** 5h
- **Blocked by:** MBAK-04
- **Description:** `CastController` today has **play / pause / stop / skip_next / poll_timeline and
  nothing else** — no seek, no volume, no mute. A player panel without a scrub bar or a volume
  control is not a player, so generalising the seam is materially more work than "implement the
  existing trait for a second backend." This item does the transport work; MBAK-07 does the adapter.

  ## FILES
  - `src/plex_control/client.rs` — add `seek_to(target, position_ms)` (Companion
    `/player/playback/seekTo?offset=`), `set_volume(target, level)` and `mute(target, bool)`
    (`/player/playback/setParameters?volume=`), each with the existing header + monotonic `commandID`
    discipline. Additive only; no existing signature changes.
  - `src/plex_control/cast.rs` — `CastController` gains the three methods with **default
    implementations returning `NotImplemented`**, so `GoogleCastController` and any other existing
    implementor keeps compiling unchanged; `PlexControlClient` overrides all three.
  - `src/maestro/backend/transport.rs` — the bridge making `PlexControlClient` satisfy the
    `DeviceControl` facet's transport half without duplicated method bodies.

  ## APPROACH
  1. Default-method additions are the deliberate choice over a breaking trait change: the existing
     `channels`/`watch_together` callers must not be touched by a transport extension, and
     `GoogleCastController` genuinely cannot do these things. **But** — the capability flags, not the
     defaults, are the contract (§0.2): `BackendCapabilities` reports what is really supported, and
     the `NotImplemented` default is the backstop, not the signal.
  2. Volume semantics: Plex's `setParameters?volume=` takes 0–100. `DeviceControl::set_volume`
     takes a normalized `0.0..=1.0` and the adapter converts, so a future backend with a different
     scale does not leak Plex's units into the API. Clamp out of range rather than erroring — a
     volume request is not worth failing a session over.
  3. `mute` is modelled separately from `set_volume(0.0)` because they are genuinely different: mute
     preserves the prior level. If Plex has no distinct mute command, implement it as
     "remember-then-zero / restore" **inside the adapter**, and document that it is emulated so a
     reviewer does not mistake it for a native capability.
  4. Keep `poll_timeline` where it is — MBAK-07's `sessions()` consumes it.

  ## TEST PLAN
  - `httpmock`: `seek_to` hits `/player/playback/seekTo` with the right `offset` and target header;
    `set_volume` hits `setParameters` with the converted 0–100 value; `mute` performs the documented
    sequence.
  - `commandID` remains monotonic across the new commands.
  - Trait-default test: a type implementing only the original `CastController` methods still compiles
    and returns `NotImplemented` for the three new ones — the `GoogleCastController` case, asserted.
  - Volume normalization: `1.0` → 100, `0.0` → 0, `1.5` → clamped to 100 (not an error).
  - Regression: every pre-existing `plex_control` test passes unmodified.
  - Verify no hardcoded infrastructure values.

  ## EDGE CASES
  - A target that accepts `seekTo` but ignores it (a known Companion inconsistency) ⇒ we report
    success on the command, and the timeline poll is the source of truth for actual position. Say so
    in the doc; do not fabricate a confirmation we did not receive.
  - Negative or out-of-range `position_ms` ⇒ clamped to `[0, duration]` when duration is known,
    rejected with a typed error when it is not.
  - `mute` emulation racing a concurrent `set_volume` ⇒ last-write-wins, documented, not locked; a
    lock here would be more machinery than the problem deserves.

- **Acceptance criteria:**
  - [ ] `CastController` gains `seek_to`, `set_volume`, `mute` with `NotImplemented` defaults, and existing implementors compile unchanged
  - [ ] `PlexControlClient` implements all three against the Companion endpoints, with monotonic `commandID`
  - [ ] `set_volume` takes a normalized `0.0..=1.0` and converts at the adapter boundary; out-of-range clamps rather than errors
  - [ ] Negative test: a type implementing only the original methods returns `NotImplemented` for the three new ones
  - [ ] Emulated behaviour (mute) is documented as emulated, not reported as native
  - [ ] All pre-existing `plex_control` tests pass unmodified
  - [ ] No hardcoded infrastructure values in new/modified code
  - [ ] All existing tests still pass

---

### MBAK-07: The `plex` adapter — `DeviceControl` + `SessionSource`, no `MediaSource`
- **Priority:** High
- **Labels:** muse, maestro, plex, adapter
- **Agent:** claude
- **Estimate:** 5h
- **Blocked by:** MBAK-04, MBAK-06, MBAK-08
- **Description:** Implement the plex backend by **wrapping `src/plex_control/` in place** — no port,
  no copy, no second `PlexControlClient`. One repo means the play-queue, transport and timeline code
  and its full `httpmock` suite are already here. Plex is a **control + observe** backend (§0.3): it
  implements `DeviceControl` and `SessionSource`, and **deliberately does not implement
  `MediaSource`** (§0.5b).

  ## FILES
  - `src/maestro/backend/plex/mod.rs` — `PlexBackend` implementing `PlaybackBackend` +
    `DeviceControl` (via MBAK-06's bridge) + `SessionSource`.
  - `src/maestro/backend/plex/sessions.rs` — `GET /status/sessions` → `Vec<PlaybackSession>`.
  - `src/plex_control/client.rs` — add `list_sessions()` if MBAK-06 has not already; additive only.
  - `src/plex_control/mod.rs` — keep the "out of scope" doc accurate; note the new consumer.

  ## APPROACH
  1. Capabilities (the concrete row of §0.4's table): `in_browser_stream: **false**` — permanent, not
     pending (§0.5b); `device_cast: true`; `server_side_transcode_decision: true`;
     `seek_during_transcode: true`; `syncplay: false`; `can_report_transcode_detail: false`. The doc
     comment must say *permanently* and cite §0.5b, so a future contributor reads it as a decision
     rather than an omission to fix.
  2. `fn media(&self) -> Option<&dyn MediaSource>` returns **`None`**. There is no proxy, no stream
     route, and no `PlaybackPlan` constructed anywhere in this adapter.
  3. `DeviceControl::start_on_device()`:
     - takes `BackendMediaRef::PlexRatingKey` (resolved by MBAK-08); any other arm ⇒ a typed
       "backend cannot play this ref" error, **explicitly matched, not a catch-all**.
     - optionally builds a play queue via the existing `create_play_queue` when the caller supplies
       an ordered list.
     - delegates to the existing `play_media(target, rating_key, play_queue_id, offset_ms)` and
       returns a `PlaybackSession` — Plex is now playing on that device, and the bytes never touch us.
     - there is no `DeviceProfile` parameter (MBAK-04 §3): the device and Plex negotiate that between
       themselves, which is precisely what makes this a control backend.
  4. `SessionSource::sessions()` → `/status/sessions`, mapped to `PlaybackSession` with `account_id` populated from
     Plex's reported user **mapped into Muse's account id-space** (epic §8.1) — a raw Plex user id
     would mint the third id-space §0.4 warns about. Where no mapping exists, `None`, never a guess.
  5. `DeviceControl::list_targets()` → `list_clients()`, carrying the existing `is_cast_target`
     heuristic **and its documented limitation**. Do not upgrade the claim.
  6. `SessionSource::report_progress()` returns `NotImplemented` **and**
     `can_report_transcode_detail` is false — Plex tracks its own progress and `src/tracker/` already
     ingests it. The flag is the contract, the error is the backstop.
  7. Preserve the module's honest caveat that this code has never been exercised against a live Plex
     server. Deleting it would be a false claim of verification.
  8. **Spec J boundary (§0.9):** this adapter *reads* the same `/status/sessions` the existing poller
     reads. Do not disable, rewrite, or repoint `src/tracker/poller.rs` here, and do not write
     `play_sessions` directly. State in the PR description that the overlap is read-only and bounded
     until spec J lands.

  ## TEST PLAN
  - `httpmock`: `sessions()` parses a mocked `/status/sessions` body, including a session with no
    resolvable account (⇒ `account_id: None`).
  - `start_on_device()` with a `PlexRatingKey` issues the expected `playMedia` call and returns a
    `PlaybackSession`; with a `FilePath` ref it returns the typed "cannot play this ref" error.
  - **Facet test:** `media()` is `None` and `capabilities().in_browser_stream` is `false` — asserted
    together, so the two can never drift apart.
  - Consistency test: `devices()` is `Some` **iff** `capabilities().device_cast`.
  - `report_progress()` returns `NotImplemented` **and** the capability flag is false.
  - All pre-existing `plex_control` tests pass unmodified.
  - Verify no hardcoded infrastructure values — existing fixtures use RFC 5737 TEST-NET-1
    (`192.0.2.x`) documentation addresses; keep them, they are not infrastructure.

  ## EDGE CASES
  - `GET /clients` returns targets the server can see but cannot control (the documented heuristic
    gap) — surface them with the flag; never claim controllability.
  - Plex unreachable mid-session ⇒ `sessions()` errors, the registry marks it unreachable, and a
    sibling backend is unaffected. Nothing is streaming through us, so there is no in-flight transfer
    to tear down — one of the quieter dividends of §0.5b.
  - A `media_item` with no `plex_rating_key` ⇒ MBAK-08 returns a `FilePath` ref, which this adapter
    rejects with the typed error — the correct outcome, not a bug; the API layer maps it to a
    readable message.
  - A caller (or a panel) asking this backend for in-browser playback ⇒ the facet is absent, so
    MBAK-11 answers 409 before dispatch. There is no method here to call and fail.

- **Acceptance criteria:**
  - [ ] `PlexBackend` implements `PlaybackBackend` + `DeviceControl` + `SessionSource` over the **existing** `PlexControlClient` — no second client type
  - [ ] `media()` returns `None` and `capabilities().in_browser_stream` is `false`, documented as permanent per §0.5b — asserted together in one test
  - [ ] `start_on_device()` returns a `PlaybackSession`; the adapter constructs no `PlaybackPlan` and no stream URL anywhere
  - [ ] Negative test: a non-`PlexRatingKey` `BackendMediaRef` yields a typed "cannot play this ref" error via an explicit match, not a catch-all
  - [ ] `account_id` is mapped into Muse's id-space or left `None` — never a raw Plex user id
  - [ ] The PR states that the Plex-session overlap with `tracker/poller.rs` is read-only and bounded until spec J
  - [ ] All pre-existing `plex_control` tests pass unmodified
  - [ ] No hardcoded infrastructure values in new/modified code
  - [ ] All existing tests still pass

---

### MBAK-08: Item resolution + the ownership firewall
- **Priority:** Critical
- **Labels:** muse, maestro, data-ownership, architecture
- **Agent:** claude
- **Estimate:** 6h
- **Blocked by:** MBAK-01
- **Description:** Resolve `muse_item_id → BackendMediaRef` from the shared Postgres **read-only**,
  and build all four of epic §2's structural mechanisms so the ownership split is enforced by the
  database and the build, not by review discipline. **Two roles, two pools** — the privilege split is
  the enforcement.

  ## FILES
  - `src/media/mod.rs` (shared) — `BackendMediaRef` (defined in MBAK-04) plus the resolution result
    type. Shared because Foundry will want the same lookup (epic §2b).
  - `src/maestro/library.rs` — `resolve_media_ref(muse_item_id, backend: BackendId) ->
    MuseResult<BackendMediaRef>`: the **only** module that may use the **RO** pool. Its doc
    enumerates every permitted `repo::` function with a one-line justification.
  - `src/maestro/db.rs` — **both** pools and nothing else may construct one:
    - `ro_pool()` from `MAESTRO_DATABASE_URL_RO` — role `maestro_ro`.
    - `rw_pool()` from `MAESTRO_DATABASE_URL_RW` — role `maestro_rw`, used **only** by
      `src/maestro/session.rs` and `src/maestro/events/` (MBAK-10).
    Each falls back to `DATABASE_URL` **with a warning naming which guarantee is weakened**, and each
    carries a **small** `max_connections` so Maestro cannot exhaust the pool Muse's workers depend on.
  - `src/maestro/db/grants.rs` — the startup grant-probe (approach 5).
  - `tests/ownership_guard.rs` — the CI-enforced greps (epic §2 mechanisms 1+2).
  - `src/repo/media_item.rs`, `src/repo/media_file.rs` — SELECT-only helpers, only if the needed
    query does not already exist. No new writes.

  ## APPROACH
  1. Resolution is backend-aware: for `plex`, prefer `media_items.plex_rating_key` →
     `BackendMediaRef::PlexRatingKey`; for `native`, join `media_files` → `FilePath { path,
     media_info }`. If the preferred ref is unavailable, return the other rather than erroring —
     the adapter decides whether it can use it (MBAK-07's typed rejection), because "which refs can I
     play" is backend knowledge, not library knowledge.
  2. Distinguish **item unknown** from **item known but has no playable ref**. Different states for
     the player; they must not collapse into one error.
  3. Path handling: return the path exactly as Muse stores it (`MUSE_MEDIA_ROOT` may be a prefix).
     Do not `stat` it, do not join a root — that is spec D's delivery-time concern, together with the
     `MUSE_FOUNDRY_ALLOWED_ROOTS`-style symlink-resolving default-deny allowlist epic §10b mandates.
     Add a `TODO(S130-D)` naming that so nobody adds filesystem access here and calls it resolution.
  4. **Two roles, and why the split is shaped this way:**

     | Role | DSN | Grants |
     |---|---|---|
     | `maestro_ro` | `MAESTRO_DATABASE_URL_RO` | `SELECT` on `media_items`, `media_files`, `accounts`, `browser_account_map`. **No `SELECT` at all** on taste, embedding, or play-event tables. |
     | `maestro_rw` | `MAESTRO_DATABASE_URL_RW` | `SELECT`, `INSERT`, `UPDATE`, `DELETE` on **only** `playback_sessions` and `maestro_event_outbox`. |

     Two properties are load-bearing and easy to get wrong:
     - **`maestro_rw` needs `SELECT`, not just the write verbs.** `maestro_ro` has no grant on those
       two tables, so if `maestro_rw` were write-only, session retrieval and outbox polling would
       fail at the very first query. An earlier draft made exactly that mistake; review caught it.
       The verbs are not decoration — Maestro reads its own tables constantly.
     - **`maestro_ro`'s *absence* of grants is the point.** It cannot read taste or embeddings at all,
       so a future contributor cannot quietly start reasoning about taste inside Maestro even by
       accident. A read grant nobody uses today is a read grant somebody uses next year.
  5. **Startup grant-probe.** On boot, assert the privilege model rather than assuming it, and log a
     single structured line with the result:
     - `maestro_ro` **can** SELECT `media_items`; **cannot** SELECT a taste table.
     - `maestro_rw` **can** SELECT and INSERT `playback_sessions`; **cannot** touch `media_items` —
       the negative assertion matters as much as the positive one.
     A failed probe is a **loud warning, not a hard exit**: on a dev box or a fallback DSN the roles
     may legitimately not exist, and refusing to boot would make the degraded path unusable. `/readyz`
     reports the probe result so the weakening is visible rather than assumed.
  6. **The four mechanisms:**
     - *Privilege split* — the two roles above (MBAK-03 provisions them). A stray write to a library
       table fails at the database, not at code review.
     - *Single DB entry point* — `src/maestro/db.rs` is the only pool constructor under
       `src/maestro/`; `library.rs` is the only RO consumer, and `session.rs`/`events/` the only RW
       consumers. Every other Maestro module receives resolved data, never a `PgPool`.
     - *CI grep — writes* — a test scanning `src/maestro/**` for `INSERT`/`UPDATE`/`DELETE`/`upsert`
       against any table **other than** the two Maestro-owned ones, failing the build on a hit. The
       allowlist is exactly `playback_sessions` and `maestro_event_outbox` and lives in one named
       constant, so widening it is a visible diff rather than a regex tweak.
     - *CI grep — metadata* — a test asserting no type reachable from Maestro's API surface carries
       `title`/`poster`/`overview`/`year`/`artwork`. Cheap, and it does not forget.
  7. Both greps must be **proven to fire**: each test includes a fixture string that should match, so
     a vacuous pass is impossible. A guard nobody has seen fail is a guard nobody knows works.
  8. No caching beyond a short TTL keyed by item id, and no persistence. A cache outliving a playback
     session is the first step toward the dual-library-ownership failure epic §2 names.

  ## TEST PLAN
  - `resolve_media_ref` against the existing `src/fixtures.rs` harness: an item with a
    `plex_rating_key` resolves to `PlexRatingKey` for the plex backend; an item with only a file
    resolves to `FilePath`.
  - Unknown item and item-with-no-playable-ref yield **distinct** typed errors.
  - Grant-probe unit tests over a mocked privilege response, covering all four assertions —
    including the two negatives (`maestro_ro` cannot read taste; `maestro_rw` cannot read
    `media_items`) and the "probe failed ⇒ warn, boot anyway, surface in `/readyz`" path.
  - Write-guard test fires on its own positive fixture, **and** passes for a legitimate write to
    `playback_sessions` — a guard that blocked Maestro's own tables would be worse than none.
  - Metadata-guard test fires on its own positive fixture.
  - Pool config: each Maestro pool's `max_connections` is strictly less than Muse's.
  - Either DSN unset ⇒ falls back to `DATABASE_URL` **and** logs a warning naming which guarantee is
    weakened (RO and RW tested separately — the messages must be distinguishable).
  - Verify no hardcoded infrastructure values.

  ## EDGE CASES
  - Postgres unreachable ⇒ resolution fails with a typed error; `/healthz` stays 200 and `/readyz`
    reports it (liveness is not readiness; rolling back a deploy over a transient DB blip is worse
    than a degraded read).
  - `maestro_ro` rejecting a write at runtime ⇒ surface it as a clear **ownership violation** in the
    log, not a generic DB error: that is a code bug, not an ops one.
  - **Both DSNs pointing at the same superuser role** (the all-fallback dev case) ⇒ everything works
    and the probe warns twice. It must not silently look healthy — that is precisely the state an
    operator needs to notice before it reaches production.
  - `maestro_rw` provisioned without `SELECT` ⇒ the probe's positive assertion fails loudly at boot
    with a message naming the missing verb, rather than surfacing later as a mystifying
    session-retrieval error under load.
  - `media_files` rows with a NULL/empty path ⇒ "no playable ref", not a valid empty path.
  - Two `media_files` for one item (multi-version) ⇒ return them all and let spec C/D choose; silently
    picking the first would be a playback decision made in the wrong layer.

- **Acceptance criteria:**
  - [ ] `resolve_media_ref` returns `PlexRatingKey` or `FilePath` per backend preference, via the RO pool
  - [ ] Unknown item and item-with-no-playable-ref are distinct, testable outcomes
  - [ ] Two pools exist with distinct DSNs; `src/maestro/db.rs` is the only constructor, and RW is reachable only from `session.rs`/`events/`
  - [ ] The startup grant-probe asserts all four properties, including that **`maestro_rw` can SELECT its own tables** and **cannot touch `media_items`**
  - [ ] Negative test: a failed grant-probe warns and boots, surfacing the weakening in `/readyz` rather than exiting
  - [ ] Negative test: either DSN unset falls back with a **distinguishable** warning naming which guarantee is weakened
  - [ ] The write grep blocks writes to any table outside the named two-table allowlist, is **proven to fire**, and permits a legitimate `playback_sessions` write
  - [ ] No Maestro API type carries `title`/`poster`/`overview`/`year`/`artwork`, enforced by a CI grep **proven to fire**
  - [ ] README documents both roles, their grants, and the single-writer rule for watch state
  - [ ] No hardcoded infrastructure values in new/modified code
  - [ ] All existing tests still pass

---

> **MBAK-09 is intentionally unused.** It was the Plex reverse-proxy data plane, cut when epic §8.6
> was revised. The number is left vacant rather than renumbering, so that a reader who finds a
> reference to it in an earlier draft or review thread lands here and then on §0.5b, which explains
> why it will not be coming back.

---

### MBAK-10: Durable play-event outbox → Muse
- **Priority:** High
- **Labels:** muse, maestro, context-bus, tracker
- **Agent:** claude
- **Estimate:** 6h
- **Blocked by:** MBAK-04, MBAK-08
- **Description:** Maestro emits play events; Muse consumes them and stays the single writer of watch
  state (§0.8). Per epic §10b, delivery is **durable**: a local outbox with retry and dedupe keys and
  a **versioned (`"v":1`) payload from the first commit**. A lost stop-event is a corrupted watch
  duration, which corrupts taste — the one failure that silently damages the product.

  ## FILES
  - `migrations/00XX_maestro_event_outbox.sql` — the outbox table: `id`, `dedupe_key`, `payload
    jsonb`, `attempts`, `next_attempt_at`, `created_at`, `delivered_at`. **Maestro-owned**, no
    textual-metadata columns. The migration must also `GRANT SELECT, INSERT, UPDATE, DELETE` to
    `maestro_rw` — a new Maestro-owned table that forgets its grant is a runtime failure the
    grant-probe will catch at the next boot but which nothing catches at migration time.
    Accessed via MBAK-08's **RW** pool: this is one of exactly two tables Maestro writes.
  - `src/maestro/events/mod.rs` — the emitter: enqueue on the write path, a background delivery loop
    with exponential backoff, bounded attempts, then park-and-alert (never silent discard).
  - `src/maestro/events/payload.rs` — the versioned payload type.
  - `src/tracker/interpret.rs` — a `maestro` arm in the source dispatch, mapping Maestro's vocabulary
    (`start`/`progress`/`pause`/`resume`/`stop`) onto the existing `PlayStateEventKind`, following the
    shape of `from_jellyfin_notification_type`.
  - `src/http/mod.rs` + handler — `POST /ingest/play-event` on the ingest router (currently a 501
    fallback), accepting `source: "maestro"`. **Protected** — it writes watch state.
  - `src/metrics.rs` — events enqueued / delivered / retried / parked.

  ## APPROACH
  1. Payload: `{ "v":1, source:"maestro", session_id, dedupe_key, account_id, muse_item_id,
     event_type, position_ms, duration_ms, backend_id, occurred_at }`. `muse_item_id` — never a
     backend-native id; Muse must not reverse-map. **No title/poster/overview** (epic §2 mechanism 1).
  2. `dedupe_key = (session_id, event_type, position_ms / 10_000)` so an at-least-once retry cannot
     double-count a progress tick. Maestro sends it; **Muse enforces it**, because the consumer is
     where idempotency has to be real. Muse's `tracker/reconstruct.rs` already performs idempotent
     session reconstruction — that is the consumer contract, and the handler must land in front of
     it, not beside it.
  3. Write to the outbox **in the same transaction as the session-state change** where one exists, so
     an event cannot be lost by a crash between the two. Where no transaction exists (a transport
     command), enqueue before returning success to the caller.
  4. Delivery loop: exponential backoff, a bounded attempt count, then **park** the row with a
     counter and a log line — never delete it. A parked row is recoverable; a discarded one is a
     permanently wrong watch duration.
  5. Emission points: `start_on_device` success (and `start_stream` once spec D adds an implementor),
     each progress report, transport pause/resume/stop,
     session teardown, and a synthetic `stop` on session reap so an abandoned stream still closes.
  6. Version the payload from commit one and **reject an unknown `v` on the Muse side** with a
     readable error rather than best-effort parsing. Silent tolerance of an unknown version is how a
     schema change becomes a data-corruption incident.
  7. `MAESTRO_MUSE_TOKEN` authenticates the POST; unconfigured ⇒ events still enqueue and the loop
     parks them with a clear "not configured" reason, so nothing is lost when the token lands.

  ## TEST PLAN
  - `interpret.rs` unit tests proving a Maestro `stop` folds into the same interpreted session as the
    equivalent Plex `media.stop` and Jellyfin `PlaybackStop`, mirroring the existing
    `plex_and_jellyfin_event_names_normalize_to_the_same_kind` and
    `jellyfin_stream_flows_end_to_end_and_matches_plex` tests.
  - `oneshot`: `POST /ingest/play-event` unauthenticated → 401; authenticated → 200; a duplicate
    `dedupe_key` is ingested exactly once; an unknown `"v":2` → 400 with a readable error.
  - Outbox (`httpmock`): a 500 from Muse retries with backoff, then parks the row (still present,
    `delivered_at IS NULL`) and increments the parked counter.
  - Unconfigured Muse URL/token ⇒ rows enqueue and park with the "not configured" reason; no retry
    storm, no panic, nothing lost.
  - A crash between session-state change and enqueue is impossible for the transactional path —
    asserted by a rollback test.
  - Verify no hardcoded infrastructure values.

  ## EDGE CASES
  - Muse down for a whole session ⇒ the outbox grows, bounded by a documented row cap; on hitting it,
    park-and-alert rather than drop the newest, because the **stop** event is the one that matters
    most and it arrives last.
  - Clock skew — `occurred_at` is Maestro's clock and is documented as such; Muse orders by its own
    receipt time for anything ordering-sensitive.
  - A `stop` delivered before its `start` (retry reordering) — `reconstruct.rs` tolerates partial
    sequences; add a test rather than assuming.
  - `account_id` absent (no mapping, epic §8.1) ⇒ ingested with a null account, never dropped.
  - The outbox table must not be reaped by any Muse maintenance job that does not know it exists —
    name it distinctly and note the ownership in the migration comment.

- **Acceptance criteria:**
  - [ ] Events are persisted to an outbox before delivery and survive a Maestro restart
  - [ ] Payload carries `"v":1`; Muse rejects an unknown version with a readable 400 rather than best-effort parsing
  - [ ] A duplicate `dedupe_key` is ingested exactly once, enforced on the Muse side
  - [ ] Negative test: Muse unreachable retries with backoff then **parks** the row — never discards it, never panics, and playback is unaffected
  - [ ] Negative test: `MAESTRO_MUSE_TOKEN` unconfigured enqueues and parks with a clear reason, losing nothing
  - [ ] A Maestro `stop` interprets to the same session outcome as the equivalent Plex event
  - [ ] Maestro never writes `play_sessions` directly; all watch state flows through Muse's ingest writer
  - [ ] The outbox is reached only through MBAK-08's **RW** pool, and its migration grants `maestro_rw` the four verbs including `SELECT`
  - [ ] The outbox table carries no textual-metadata column
  - [ ] README documents the event contract and the versioning rule
  - [ ] No hardcoded infrastructure values in new/modified code
  - [ ] All existing tests still pass

---

### MBAK-11: The Maestro control-plane HTTP API, including `GET /backends`
- **Priority:** High
- **Labels:** muse, maestro, api, axum
- **Agent:** claude
- **Estimate:** 5h
- **Blocked by:** MBAK-05, MBAK-07
- **Description:** Expose the registry and the facets over HTTP so specs G and H have something to
  build against immediately — including the `GET /backends` capability endpoint epic §8.6 requires
  and the active-sessions view epic §5 notes does not exist anywhere today. **Control plane only:
  this spec serves no media** (§0.3, §0.5b).

  ## FILES
  - `src/maestro/http/mod.rs` — mount the control-plane routes on the protected sub-router.
  - `src/maestro/http/playback.rs` — handlers.

  ## APPROACH
  1. Routes (all protected; `/healthz`, `/readyz`, `/metrics` are not):
     - `GET /backends` → `[{id, configured, reachable, capabilities: BackendCapabilities}]` — the
       endpoint epic §8.6 names. This is what lets spec H's Activity panel print "Plex cannot report
       this" instead of rendering zeros as facts, and what lets spec G decide whether to render a
       video element at all.
     - `GET /sessions` → active `PlaybackSession`s across reachable backends, each tagged with its
       `backend_id`; an unreachable backend contributes an error entry, not a 500.
     - `POST /sessions` → `{backend?, muse_item_id, target?, offset_ms?, account_id?}`. The response
       **depends on which facet served it**: with `target` present it routes to
       `DeviceControl::start_on_device` and returns a `PlaybackSession`; without `target` it requires
       `MediaSource` and returns a `PlaybackPlan`. In this spec only the first path has an
       implementor, so a target-less request against `plex` is a 409 (see 3).
     - `DELETE /sessions/:id` → end a session.
     - `POST /transport/:command` → `{backend?, target}`; `command` ∈
       `play|pause|stop|seek|next|volume|mute`; `seek` takes `position_ms`, `volume` takes `level`.
     - `GET /devices` → controllable targets across backends.
     - `POST /progress` → a client-side progress report, fanned to `SessionSource::report_progress()`
       + the MBAK-10 outbox.
     There is **no** `/stream` route in this spec. Adding one is spec D's work, and it must not be
     mounted behind the control-plane bearer when it arrives (§0.3).
  2. Backend selection uses MBAK-05's policy; no default and no explicit backend ⇒ 400 listing ids.
  3. **A facet the selected backend does not implement ⇒ 409 Conflict naming the capability**, not
     501. The distinction matters: 409 means "this backend can't"; 501 would mean "Maestro can't",
     and the UI reacts differently. Check `capabilities()`/the facet accessor **before** dispatching,
     so the error is deterministic rather than dependent on which upstream call fails first. The
     canonical case in this spec: asking `plex` for in-browser playback ⇒ 409 `in_browser_stream`.
  4. **The API layer must never branch on backend name** (§0.4 rule 2) — every decision reads a
     capability flag or a facet accessor. A `if backend == "plex"` here is a review rejection.
  5. Muse is pinned to axum 0.7, where `{id}` brace routes are broken (memory
     `muse_axum_brace_route_bug`) — use `:param` syntax, consistent with the rest of this crate. An
     axum 0.8 upgrade is its own item.
  6. Per-session structured log line (epic §10b): session id, item, facet used, backend, client.

  ## TEST PLAN
  - `oneshot` over the real router with a `FakeBackend` registry, for every route.
  - `GET /backends` returns the exact `BackendCapabilities` field set (golden fixture shared with
    MBAK-04) — the contract spec G/H and MBAK-12 consume.
  - **`POST /sessions` with no `target` against a control-only backend → 409 naming
    `in_browser_stream`, with no upstream call attempted** (assert via the mock's call count).
  - `POST /transport/volume` against a backend whose capability is false → 409 naming the capability,
    again with no upstream call attempted.
  - `POST /sessions` with a `target` against a control-only backend → 200 with a `PlaybackSession`.
  - `POST /sessions` with no `backend` and two configured → 400 listing both ids.
  - `GET /sessions` with one backend unreachable → 200 with the healthy backend's sessions plus an
    error entry for the other.
  - Verify no hardcoded infrastructure values, and no backend-name comparison anywhere in the handlers.

  ## EDGE CASES
  - Unknown `command` ⇒ 400 listing the valid set, never a 404 that reads as "Maestro is down".
  - Unknown `backend` id ⇒ 404 naming the id and listing the configured ones.
  - `GET /sessions` with an empty registry ⇒ 200 with `[]`, never an error.
  - A slow backend must not stall the `/sessions` fan-out — per-backend timeout, partial results.
  - `DELETE /sessions/:id` for an already-ended session ⇒ 204, idempotent; a 404 would make client
    retry logic wrong.
  - A request carrying a `profile` field ⇒ accepted and ignored for device starts, with the response
    documenting that the device negotiates its own profile. Rejecting it would break spec G's
    single request shape for no benefit.

- **Acceptance criteria:**
  - [ ] `GET /backends` publishes `BackendCapabilities` per backend, matching the MBAK-04 golden fixture
  - [ ] `GET/POST/DELETE /sessions`, `POST /transport/:command`, `GET /devices`, `POST /progress` all respond per contract
  - [ ] Negative test: a target-less `POST /sessions` against a control-only backend yields 409 naming `in_browser_stream`, **before** any upstream call (assert call count)
  - [ ] Negative test: an unreachable backend yields a partial `GET /sessions` result, never a 500
  - [ ] No handler branches on backend name; every decision reads a capability flag or a facet accessor
  - [ ] `DELETE /sessions/:id` is idempotent (204 for an already-ended session)
  - [ ] No `/stream` route is mounted in this spec
  - [ ] Control-plane routes require the bearer; `/healthz`, `/readyz`, `/metrics` do not
  - [ ] Each session start emits the epic §10b structured log line
  - [ ] README documents every route with request/response examples using placeholder values
  - [ ] No hardcoded infrastructure values in new/modified code
  - [ ] All existing tests still pass

---

### MBAK-12: Assistant-operable surface — the `maestro_*` tool family (Lumina drives playback)
- **Priority:** High
- **Labels:** muse, maestro, terminus, tools, assistant
- **Agent:** claude
- **Estimate:** 5h
- **Blocked by:** MBAK-11
- **Description:** Module Contract clause 4 and epic §9.4. This is arguably the epic's **best early
  demo**: with the plex backend, Lumina can drive real playback in phase 1 — "pause the living room",
  "what's playing?", "resume it" — before a single line of transcoding exists.

  ## FILES
  - **Terminus:** `src/maestro/mod.rs` — the domain module + tool registration, modelled on
    `src/media/mod.rs` including its graceful-degradation doc section.
  - **Terminus:** `src/maestro/client.rs` — a thin typed client over Maestro's control plane, reading
    `constellation_maestro_url()`/`constellation_maestro_token()` — the same values `proxy_maestro`
    uses, so there is exactly one credential and one base URL.
  - **Terminus:** `src/registry.rs` — `crate::maestro::register(registry);`
  - **Terminus:** `src/lib.rs` — `pub mod maestro;`

  ## APPROACH
  1. **v1 tool surface, enumerated:**
     - `maestro_now_playing` — what is playing right now, across backends. No arguments. This is the
       context-bus payload epic §9 clause 3 calls the point of the whole exercise.
     - `maestro_sessions` — the fuller session list, including per-session backend and delivery method.
     - `maestro_devices` — controllable targets, with which backend each belongs to.
     - `maestro_play(item, device)` — `item` is a Muse `media_item_id`; `device` is a target id from
       `maestro_devices`. **`device` is required, not optional**, in this spec: with plex as the only
       backend there is no in-browser destination to default to (§0.3), and a tool whose required
       argument silently became optional later would be a worse contract than one that starts honest.
     - `maestro_pause`, `maestro_resume`, `maestro_seek(position_ms)` — transport, each taking a
       device (or defaulting to the sole active session when there is exactly one, which is the
       household-scale reality and what makes "pause it" work as an utterance).
  2. Every tool degrades to `ToolError::NotConfigured` when `CONSTELLATION_MAESTRO_URL` is unset —
     never a panic, never a hardcoded fallback. Mirror `media_domain_status`'s posture.
  3. **Capability-aware errors.** A 409 from Maestro surfaces as "this backend cannot do that", not
     "playback failed", so the assistant can say something true. `maestro_devices` includes the
     backend's relevant capability flags for the same reason.
  4. Tool descriptions are written for an assistant to route on — what the tool does **and what it
     does not**: `maestro_play` does not search; it takes a `media_item_id` that Muse's own tools
     resolve. Getting this wrong produces an assistant that calls `maestro_play("The Thing")`.
  5. Do **not** add a second HTTP path to Maestro: these tools use the same base URL and token as the
     proxy arm. Two doors to one backend is the failure `proxy.rs`'s single-door doc warns about.
  6. The tools are control-plane only. They never reference a stream route — none exists (§0.5b) —
     and, per §0.4 rule 2, they branch on capability flags, never on backend name.

  ## TEST PLAN
  - `cargo test` in Terminus — each tool's `parameters()` is valid JSON Schema; each returns
    `NotConfigured` with the env unset.
  - `httpmock` — `maestro_now_playing` parses a `/sessions` body; `maestro_seek` posts the right path
    and body; a Maestro 409 surfaces as a readable capability message, not a generic failure.
  - `maestro_pause` with exactly one active session and no device argument targets that session;
    with two active sessions and no device it returns a disambiguation error listing both.
  - Registry test: the tool names register and do not collide with the existing `media_*` family.
  - Verify no hardcoded infrastructure values.

  ## EDGE CASES
  - Maestro configured but unreachable ⇒ a clear upstream error naming Maestro, not a bare timeout.
  - `maestro_play` with an unknown item id ⇒ Maestro's typed not-found surfaced so the assistant can
    say "that isn't in the library" rather than "playback failed".
  - Zero active sessions ⇒ `maestro_now_playing` returns an explicit empty result, not an error —
    "nothing is playing" is a valid, useful answer.
  - A device that exists but whose backend is unreachable ⇒ the error names the backend, so the
    assistant can say "Plex isn't responding" rather than blaming the device.

- **Acceptance criteria:**
  - [ ] The enumerated v1 tools are registered and callable through the Terminus registry
  - [ ] Every tool degrades to `NotConfigured` when `CONSTELLATION_MAESTRO_URL` is unset
  - [ ] A Maestro 409 surfaces as a capability-specific message, not a generic failure
  - [ ] Negative test: `maestro_pause` with two active sessions and no device returns a disambiguation error listing both
  - [ ] Negative test: zero active sessions yields an explicit empty result from `maestro_now_playing`, not an error
  - [ ] The tools reuse `constellation_maestro_url()`/`constellation_maestro_token()` — no second credential, no second base URL, and no stream-route reference
  - [ ] No tool branches on backend name; capability flags drive every decision
  - [ ] Tool descriptions state what each tool does not do
  - [ ] README documents the tool family
  - [ ] No hardcoded infrastructure values in new/modified code
  - [ ] All existing tests still pass

---

### MBAK-13: Deploy wiring — second bin in the existing `muse` OCI module
- **Priority:** Medium
- **Labels:** muse, maestro, deploy, ops, human-action
- **Agent:** <operator>
- **Estimate:** 45m
- **Type:** human-action
- **Blocked by:** MBAK-01
- **Description:** Ship the `maestro` binary inside the **existing** `muse` OCI image and module. No
  new module, no new conf, no new CI config — one image, two bins, all-or-nothing deploy with a
  shared rollback (epic §2). Ops config on a live host is the sanctioned no-spec-item exception; it
  is an item only because forgetting it means the binary builds and never runs.
- **Steps:**
  1. Publish both bins in one image:
     `oci-publish.sh muse moosenet/Muse main muse maestro`. Muse is rustls-only and ffmpeg is a
     subprocess (epic §7.1), so the **musl-static default is correct — do not pass `TARGET_NATIVE=1`**
     (getting this backwards is what left Chord un-deployed, TERM #558).
  2. Extend the existing muse module conf on the deploy host:
     `OCI_INSTALL=( "muse:/opt/muse/muse:muse.service" "maestro:/opt/muse/maestro:maestro.service" )`.
     Leave `MODE=oci`, `VERSION_MARKER`, `CHANNEL` unchanged. Keep every registry-derived variable
     assigned **after** the `source` of `/etc/constellation/secrets` (the S124 trap: an assignment
     above the source pins the placeholder and breaks skopeo).
  3. Install `maestro.service`: same `EnvironmentFile` as `muse.service` plus its own cgroup caps
     (`MemoryMax`, `MemorySwapMax=0`) — spec E holds CPU for minutes at a time, and the swap-off is
     what keeps an over-budget transcode from thrashing the node.
  4. Health-gate `maestro.service` on **`/healthz`**, never `/readyz` — a readiness gate would roll
     back a good deploy every time Plex was down (MBAK-01).
  5. Confirm the host has the **read-only library mount** and `ffmpeg`/`ffprobe` (epic §10b), and that
     the segment-scratch path is **not** on a removable-card-backed volume (the fleet has lost a
     card-backed LV before).
  6. Deploy: `constellation-update.sh --force --skip-idle muse`. Confirm `.deployed_sha` holds an OCI
     **digest**, not a git sha — a git-sha marker makes the nightly re-pull the stale image and revert
     the deploy. Never close this with a hand-built binary swap.
  7. Verify both units are active, `muse` still serves `/health`, `maestro` serves `/healthz`, and
     `/readyz` reports honestly on what is and is not present.

---

### MBAK-14: The Cargo workspace split (epic item `W`)
- **Priority:** High
- **Labels:** muse, maestro, workspace, refactor
- **Agent:** claude
- **Estimate:** 8h
- **Blocked by:** MBAK-01; **spec A's `MPRB-01`** (the probe/`MediaInfo` layer that defines what
  belongs in the shared core)
- **Blocks:** **spec D** — it is the prerequisite for the first Maestro code that owns persistence.
- **Description:** Split the single crate into a three-crate workspace —
  `crates/{muse-core, muse, maestro}` — so the ownership boundary §0.1 describes acquires a
  **compile-time** layer on top of the runtime one. Until this lands, the DB roles (MBAK-08) are the
  enforcement and the crate graph is doing no work; this item is what changes that, and the epic
  deliberately claims less until it does.

  ## FILES
  - `Cargo.toml` (root) — becomes a virtual workspace manifest: `[workspace] members = ["crates/*"]`,
    with `[workspace.dependencies]` hoisting every shared dependency so versions cannot drift
    between members.
  - `crates/muse-core/` — the shared core: `config.rs`, `error.rs`, `models/`, `repo/`, `media/`
    (the epic §2b shared media core: `MediaInfo`, `DeviceProfile`, `plan()`, `BackendMediaRef`).
  - `crates/muse/` — the brain: library, taste, curation, acquisition, channels, requests, tracker,
    web, the `muse` binary. Depends on `muse-core`.
  - `crates/maestro/` — backends, facets, registry, session, events, http; the `maestro` binary.
    Depends on `muse-core`. **Must not depend on `muse`.**
  - `rust-toolchain.toml`, `.gitea/workflows/ci.yml` — path updates only.
  - `tests/` — relocated per crate; the cross-cutting ownership guards (MBAK-08) move to
    `crates/maestro/tests/`.

  ## APPROACH
  1. **Do this as a move, not a rewrite.** `git mv` the trees, fix `use` paths, and change no logic.
     A workspace split that also "tidies" code produces a diff no reviewer can gate — and this diff
     is already large enough that reviewers will be reading path changes, not semantics.
  2. **The load-bearing edge is what `maestro` may depend on.** `maestro → muse-core` only; never
     `maestro → muse`. That single missing edge is what makes "Maestro has no library scanner and no
     metadata provider" a **compile error** rather than a grep. State it in `crates/maestro`'s
     manifest as a comment, since a dependency addition is otherwise a one-line diff nobody notices.
  3. **What goes in `muse-core` is decided by a rule, not by taste:** a module belongs there iff both
     binaries need it *and* it carries no library-ownership semantics. `models/` and `repo/` qualify
     (they are shared shapes and shared SQL); `library/`, `metadata/`, `taste_model/` emphatically do
     not and stay in `crates/muse`. When in doubt, leave it in `muse` — pulling something *into* core
     later is cheap, pushing it back out is not.
  4. **Spec A dependency:** `MPRB-01` establishes `MediaInfo` and the probe layer. Splitting before it
     lands would mean guessing which half of the media core is shared and then moving it twice.
  5. Keep the OCI publish working: `oci-publish.sh muse moosenet/Muse main muse maestro` must still
     find both binaries. Workspace binary output paths do not change (`target/release/<bin>`), but
     verify rather than assume — MBAK-13's deploy depends on it.
  6. Migrations stay at the repo root, not inside a crate: they are owned by the deployment, and both
     crates' tests reference them.

  ## TEST PLAN
  - `cargo build --workspace --bins` produces both `muse` and `maestro` at the same paths as before.
  - `cargo test --workspace` passes with the **same test count** as before the split (modulo tests
    that moved) — a drop means tests were orphaned by the move, which is the classic silent failure
    of a workspace refactor.
  - **Dependency-edge test:** a check asserting `crates/maestro/Cargo.toml` does not depend on the
    `muse` crate, failing the build if it ever does. Cheap, and it is the whole point of the item.
  - MBAK-08's ownership guards still run and are still **proven to fire** from their new location.
  - `cargo tree -p maestro` shows no path to the `muse` crate.
  - Verify no hardcoded infrastructure values.

  ## EDGE CASES
  - A module genuinely needed by both but library-owning (the `repo::media_item` case) ⇒ it stays in
    `muse-core` as SELECT-only helpers **plus** write helpers, and it is the DB roles — not the crate
    graph — that stop Maestro calling the writers. The crate split does not subsume the privilege
    split; the two guards cover different halves, and neither is redundant.
  - `#[cfg(test)]` fixtures (`src/fixtures.rs`) used by both crates ⇒ move to `muse-core` behind a
    `test-fixtures` feature rather than duplicating; a duplicated fixture drifts.
  - `sqlx` offline query metadata (`.sqlx/`) is workspace-scoped — regenerate it in the same commit
    or CI builds fail on a stale cache with an error that names the wrong thing entirely.
  - The split lands mid-sprint while other MBAK items are in flight ⇒ **merge it on a quiet main**,
    not concurrently with several open item branches. Every in-flight branch will conflict on paths.
    Sequence it deliberately; this is the one item whose merge timing matters more than its content.

- **Acceptance criteria:**
  - [ ] `crates/{muse-core, muse, maestro}` exist; `cargo build --workspace --bins` produces both binaries at unchanged paths
  - [ ] `crates/maestro` does not depend on `crates/muse`, enforced by a build-failing check and confirmed by `cargo tree`
  - [ ] `cargo test --workspace` passes with no net loss of tests versus the pre-split baseline
  - [ ] Negative test: adding a `muse` dependency to `crates/maestro` fails the check
  - [ ] The split is a move: no logic changes in the same commit
  - [ ] MBAK-08's ownership guards run from their new location and are still proven to fire
  - [ ] `muse-core` contains no library-ownership module (`library/`, `metadata/`, `taste_model/` stay in `muse`)
  - [ ] README documents the crate layout and the forbidden dependency edge
  - [ ] No hardcoded infrastructure values in new/modified code
  - [ ] All existing tests still pass

---

## §2. Dependency order

```
MBAK-03 (pre-flight, operator: 2 tokens + 2 DB roles)
MBAK-01 (bin + health split) ──┬── MBAK-02 (two credentials + proxy_maestro)
                               ├── MBAK-13 (deploy wiring, operator)
                               ├── MBAK-08 (resolution + two-role firewall) ──┐
                               └── MBAK-04 (facets + capabilities) ──┬── MBAK-05 (registry + policy)
                                                                     ├── MBAK-06 (transport: seek/volume/mute)
                                                                     └── MBAK-10 (event outbox)
MBAK-04 + MBAK-06 + MBAK-08 ── MBAK-07 (plex adapter) ── MBAK-11 (API) ── MBAK-12 (tools)

spec A / MPRB-01 ── MBAK-14 (workspace split, epic item W) ── [blocks spec D]
```

MBAK-05, MBAK-06, MBAK-08 and MBAK-10 are mutually independent and should run in parallel.
**Spec J (`MTRC`) lands before or with this spec** (epic §8.8, §0.9). MBAK-09 does not exist — see
the note above MBAK-10 and §0.5b.

**MBAK-14 is sequenced by merge timing, not by logic.** It touches every path in the repo, so it
should merge onto a **quiet main** — after the in-flight MBAK items have landed, or in a deliberate
pause between them. Running it concurrently with several open item branches guarantees path
conflicts on all of them. It is otherwise independent of everything in this spec except MBAK-01,
and it gates spec D.

## §3. Pre-flight checklist (before any item starts)

- [ ] Spec J's sequencing confirmed with the epic owner (§0.9) — B does not perform the tracker cutover
- [ ] MBAK-03 complete — prefix promoted, **both** tokens and **both** DB roles provisioned (or their
      absence explicitly recorded as a weakened guarantee), with `maestro_rw` holding `SELECT`
      alongside the write verbs
- [ ] Spec A/C status checked: if the shared `DeviceProfile` does not yet exist, B adds the minimal
      placeholder **in the shared media core** with a `TODO(S130-C)` — never a second one under
      `src/maestro/`
- [ ] MBAK-14 sequencing agreed: spec A's `MPRB-01` landed, and a quiet-main merge window identified
- [ ] Baseline: `cargo test` green on Muse `main` and Terminus `main`, with test counts recorded
- [ ] Atlas KG consulted for `plex_control`, `tracker::interpret`/`poller`, `repo::media_item`/
      `media_file`, `streaming`, and `constellation::proxy` blast radius before editing any of them
- [ ] Build/test gates run through the compiler tool on a build-capable host — not the dev box
      (16 GB, and no `ffmpeg`/`ffprobe`)
- [ ] Epic §7 constraints and §10b cross-cutting requirements re-read

## §4. Out of scope (named so it does not creep in)

- **Jellyfin and Emby adapters** — cut per epic §8.5 (§0.5). The trait reserves their shape; the
  adapters are a follow-up spec written when a live server exists to test against.
- **A Plex byte-proxy / media data plane** — cut per the revised epic §8.6 (§0.5b). **No bytes flow
  through Maestro in this spec at all**, and `plex` will never implement `MediaSource`. Do not
  reintroduce it; the reasoning and the conditions for reopening it are in §0.5b.
- **In-browser playback of Plex content** — a permanent property of the plex backend, not a gap.
  In-browser playback arrives with the native engine (spec D).
- Any ffmpeg invocation, transcoding, or segmenting — specs D and E.
- **HMAC-signed expiring stream URLs, and the stream route itself** — spec D (epic §8.7). MBAK-04
  keeps only the `StreamToken` mint/validate seam so D inherits rather than invents it.
- **Path-allowlist enforcement** for file serving (`MUSE_FOUNDRY_ALLOWED_ROOTS`-style, epic §10b) —
  spec D, since B serves no bytes of any kind.
- `DeviceProfile` and `plan()` — spec C, in shared `src/media/` (epic §2b).
- The **tracker cutover** making Muse a pure consumer of Maestro's event stream — spec J.
- Any `<video>` element, HLS library, or panel — specs G and H.
- GPU anything — spec F. Trickplay/keyframe index — spec I. Cast receiver — spec K.
- Removing or reworking `CastController`'s existing callers (`channels`, `watch_together`) — MBAK-06
  extends the trait alongside them; a cutover is spec G's business.
- A real identity service — epic §8.1; `account_id` is modelled in Muse's id-space and passed
  through, nothing more.
- The chaos test that SIGKILLs ffmpeg mid-session (epic §10b) — it needs a real transcode, so it
  belongs with spec E.
