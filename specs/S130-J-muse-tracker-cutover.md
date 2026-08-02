# S130-J — Muse tracker cutover: one Plex observer, one watch-state writer

plane_project: MUSE
module: Muse
prefix: MTRC
spec_id: S130-J-muse-tracker-cutover

## Metadata
- **Author:** Moose
- **Session:** S130
- **Date:** 2026-08-01
- **Module version:** Muse v0.2.x (no new binary; changes the tracker's ingest seam and its ownership)
- **Estimated total:** ~42h autonomous agent work (9 items)
- **North-Star layer:** module
- **Module-Contract:** meets §4 clauses 1–7 — clause 3 is the whole point of this spec (watch state is
  the Constellation's richest shared context and must have exactly one owner); clause 5 is untouched
  (this spec ships no UI); clause 7 holds because the household's watch history keeps accruing
  unbroken through every step.
- **Parent epic:** `specs/S130-maestro-epic.md`. Every §7 standing constraint applies verbatim.
- **Depends on:** S130-B (`MBAK`) — specifically MBAK-04 (the trait + `SessionSource`), MBAK-06 (the
  `plex` backend), and MBAK-09 (the emitter and the first cut of `POST /ingest/play-event`).
- **Blocks:** nothing structurally, but it **must land before or with B's deploy**, not after — see §0.1.
- **Context:** Epic §8.8. `src/tracker/poller.rs` polls Plex `/status/sessions` every
  `MUSE_PLEX_POLL_SECS` and `src/tracker/webhook.rs` receives Plex webhooks; both write
  `play_events` and both drive `reconstruct::reconstruct_and_persist` into `play_sessions`. Spec B's
  plex adapter needs the *same upstream state* for Activity, transport control and progress. That is
  two observers of one upstream, and the epic's §2 ownership split forbids it. The epic's decision:
  **Maestro's plex adapter becomes the sole Plex session observer; Muse's tracker becomes a pure
  consumer of a normalised event stream.** This spec makes the tracker backend-agnostic, performs the
  ownership flip in a sequence that never has zero observers and never has two authoritative writers,
  and proves watch history joins cleanly across the boundary.

---

## §0. Design rationale (read before implementing any item)

### 0.1 Why this is a prerequisite and not a follow-up

The dual-ownership hazard the epic warns about does not arrive with the native engine in spec D. It
arrives the moment the `plex` adapter starts polling `/status/sessions`, which is MBAK-06 — day one
of Maestro. If B deploys with its adapter observing Plex while Muse's poller is still running, the
household immediately has two independent observers of one upstream feeding one store. That is not a
hypothetical: MBAK-06's `SessionSource` implementation and `src/tracker/poller.rs` call the same Plex
endpoint on overlapping cadences.

The corruption is quiet. `play_sessions` rows are not obviously wrong; `paused_counter` and
`paused_ms` drift, session boundaries fray at the edges, and the taste model — which sits downstream
via `taste_model/aggregate.rs` rolling `play_sessions` into `watch_stats` — degrades without anything
visibly breaking. **This is the failure class the epic singles out as worse than an outage**, and it
is why this spec is sequenced before B's deploy rather than after.

### 0.2 What is actually Plex-shaped in the tracker today (verified in tree, `e8499aa`)

The tracker is *not* uniformly Plex-coupled. Precisely one layer is:

| Layer | File | Plex-coupled? |
|---|---|---|
| Raw event row | `models/play_event.rs`, `migrations/0014_play_events.sql` | **Mostly not.** Columns are generic (`source`, `event_type`, `account_ref`, `session_key`, `rating_key`, `view_offset_ms`, player/platform/product/device, `raw`). Only the `event_type` vocabulary and `rating_key`'s meaning are Plex-flavoured. |
| Observation | `tracker/poller.rs`, `tracker/webhook.rs` | **Entirely.** Multipart Plex webhook parsing, `/status/sessions` snapshot mapping, `TranscodeSession` → `play_session_media_info`. |
| Fold | `tracker/reconstruct.rs` | **Almost not.** `fold_events` matches on `media.play|pause|resume|stop|scrobble` strings and reads `Metadata.duration`/`duration` out of `raw`. Everything else is generic accounting. |
| Resolution | `reconstruct::resolve_rating_key` | **Yes** — `plex_rating_key` on `media_items`/`episodes`. |
| Interpretation | `tracker/interpret.rs` | **Already backend-aware.** `PlayStateEventKind` is explicitly a normalised shape with `from_plex_event_type`, `from_jellyfin_notification_type`, and `to_plex_event_type`. |

So the normalisation seam the epic asks for **already half exists** — `interpret.rs` built it and then
only used it for interpretation, not for ingest. This spec finishes the job: it lifts
`PlayStateEventKind`'s normalisation *upstream* of `play_events` so that observation itself is
backend-agnostic, instead of normalising after the fact. **Do not invent a second normalised
vocabulary.** Reuse `PlayStateEventKind` and `to_plex_event_type()`.

### 0.3 The single most important design decision: `source` records ORIGIN, not OBSERVER

`play_events` dedupes on `UNIQUE (source, event_type, session_key, view_offset_ms)`
(`migrations/0014_play_events.sql` — `received_at` is deliberately excluded, and the migration
comment explains why). Today `source` is `plex_webhook` / `plex_poll` / `tautulli_backfill` — i.e.
**how we saw it**.

If Maestro emits the same underlying Plex event tagged `maestro`, the UNIQUE constraint does *not*
collapse it against the poller's row, because the `source` differs. Two observers then produce two
rows per underlying event, and the fold sees a denser stream than reality.

**Decision: for the cutover-era Plex path, `source` is the canonical upstream origin — `"plex"` —
regardless of which process observed it.** Then the same underlying Plex event, observed by the old
poller and by Maestro's adapter, is *the same row*, and the UNIQUE constraint makes dual observation
idempotent by construction rather than by hope. This is what makes the transition safe and the
rollback safe, and it is what makes the fail-open relay fallback in MTRC-05 acceptable.

Two consequences, both deliberate:

1. **Legacy rows are not migrated.** Existing `plex_webhook`/`plex_poll` rows stay exactly as they
   are. `interpret.rs`'s dispatch (currently `if source.contains("jellyfin") { .. } else { plex }`)
   becomes explicit prefix routing that maps `plex*` → the Plex adapter, so every legacy value keeps
   routing exactly where it does today. This is why the change is additive rather than a rewrite.
2. **Observer identity is not lost** — it moves into the `raw` jsonb as an `observed_by` field, which
   is where forensic provenance belongs. It must not sit in a dedupe-key column, because a dedupe key
   that includes the observer is a dedupe key that cannot dedupe across an observer change.

### 0.4 The invariant, stated so it can be tested

The epic asks for "never zero observers, never two." The precise, checkable form:

> **At every instant there is at least one observer of Plex state, and exactly one authoritative
> writer of `play_sessions`.**

Two observers are *tolerable* (§0.3 makes their output identical); two authoritative writers are not.
The transition is a strict ladder of modes, each a config value plus a restart:

```
muse  ──►  shadow  ──►  maestro
 ▲                          │
 └──────── rollback ────────┘
```

| Mode | Observers of Plex | Authoritative writer | Purpose |
|---|---|---|---|
| `muse` (default) | Muse poller + webhook | Muse | today's behaviour, byte-for-byte |
| `shadow` | Muse poller + webhook **and** Maestro's adapter | **Muse** | prove Maestro's stream reconstructs identically before trusting it |
| `maestro` | Maestro's adapter (Muse's webhook relays, see MTRC-05) | Muse's ingest core, fed only by Maestro | end state |

Note what stays true in every mode: **Muse writes `play_sessions`.** Maestro never does — it emits, per
epic §2 and MBAK-09. "Sole observer" is about who watches Plex, not about who owns watch state. Muse
owns watch state permanently; that is not what is being handed over.

### 0.5 Continuity is the hard requirement, and here is the mechanism

`repo::play_session::upsert` keys on `(account_id, media_item_id, episode_id, started_at)`, and
`started_at` is *the first event's `received_at`* for that `session_key`
(`reconstruct::reconstruct_and_persist`). Therefore:

- A session observed by the poller and then, mid-film, observed by Maestro instead **stays one row**
  — provided both observers use the *same* `session_key`.
- If Maestro mints its own session id, the same film becomes two `play_sessions` rows, both partial,
  both feeding `watch_stats`. That is the tear.

**Therefore: `session_key` is derived from the upstream Plex `sessionKey`, verbatim, forever.** Maestro
sends `upstream_session_key`; Muse uses it unchanged. Maestro's own internal session id travels in a
separate field and is never used as the reconstruction key. MTRC-04 owns this and proves it with the
test the epic asks for: one upstream session observed across the flip ⇒ exactly one reconstructed
session.

### 0.6 What the poller does that a naive cutover would silently drop

`tracker/poller.rs::plex_media_info` reads the `Media` and `TranscodeSession` blocks that appear
**only on `/status/sessions`** — never on webhook payloads — and upserts `play_session_media_info`
(codecs, resolution, bitrate, `DecisionKind`, `transcode_reason`). Nothing else populates that table
except the Tautulli backfill.

If Maestro becomes the sole observer and its event payload carries no media-info block, that table
stops being populated for live sessions. It is consumed by spec H's Activity panel and is the
substrate for the transcode-frequency telemetry epic §4b makes spec F conditional on. **This is the
easiest thing in the whole cutover to forget**, so it is called out here and owned by MTRC-04.

### 0.7 Testing and build-host constraint

Every unit test in this spec is in-process. The live-DB round-trip tests follow the existing
`MUSE_TEST_DATABASE_URL` skip-when-unset pattern (`src/tracker/mod.rs`, `src/integration_tests.rs`) —
the crate's default test run must stay green with no database. No test contacts a live Plex or a live
Maestro (S9). The workspace build/test gate goes through the **compiler tool** on a build-capable
host per the standing single-build-door rule; do not hand-run a workspace `cargo test` on the dev box.

---

## Pre-flight

- Repository: `moosenet/Muse` on Gitea. Worktree per item off fresh `origin/main`.
- Baseline: `cargo test` green on Muse `main` (`e8499aa` or later); record the count before starting.
- Prefix `MTRC` — `plane_prefix_check` then `plane_prefix_register`, then `plane_prefix_promote` for
  the durable baseline entry.
- Dependencies: no new crates. `axum` stays pinned at 0.7 — use `:param` route syntax, never `{param}`
  (memory `muse_axum_brace_route_bug`).
- Config: this spec introduces `MUSE_TRACKER_PLEX_OBSERVER`, `MUSE_TRACKER_INGEST_MAX_SKEW_SECS`,
  `MUSE_TRACKER_RELAY_TIMEOUT_MS`, and `MUSE_TRACKER_CONTINUITY_WINDOW_HOURS`. All are non-secret
  behavioural config read via the existing `env_opt()` convention in `src/config.rs`, and each must be
  added to the `config_parses_with_defaults_when_env_unset` env-var list in that file's test module.
- Secrets: none new. Maestro→Muse authentication reuses `MUSE_API_TOKEN` via `SecretManager` (MBAK-09).
  Never `std::env::var` for anything token-shaped (S7).
- Infrastructure: Gitea, Plane (via the Terminus Plane tool), and the compiler tool reachable.
- Operator note: `MUSE_TRACKER_PLEX_OBSERVER` defaults to `muse`, so merging this spec changes nothing
  in production until an operator sets it. The flip is an ops action, deliberately.
- **No `ffprobe`/`ffmpeg` required by any item here.**

---

## §1. Items

### MTRC-01: `PlaybackEvent` — the normalised, versioned, backend-agnostic ingest type
- **Priority:** Critical
- **Labels:** muse, tracker, maestro, contract, rust
- **Agent:** claude
- **Estimate:** 5h
- **Description:** Define the one type every observation becomes before it touches the database,
  whatever produced it — the Plex poller, the Plex webhook, Maestro's plex adapter, or (later) the
  native engine. Today there is no such type: `poller.rs` and `webhook.rs` each construct a
  `NewPlayEvent` directly from Plex-shaped input. This item introduces the shared shape and the
  canonical-origin rule from §0.3, and nothing else — no behaviour changes in this item.

  ## FILES
  - `src/tracker/event.rs` (new) — `PlaybackEvent`, `EventOrigin`, `PlaybackMediaInfo`, the
    `to_new_play_event()` mapping, and the `"v": 1` wire envelope. Module doc states §0.3 and §0.5 in
    full: this is the doc a reviewer of a future backend will be pointed at.
  - `src/tracker/mod.rs` — declare the module; extend the module doc to describe the ingest seam.
  - `src/tracker/interpret.rs` — replace the substring dispatch in `PlayStateEvent::from_play_event`
    and `SessionPattern`'s builder with explicit prefix routing (`jellyfin*`, `plex*`, `maestro*`,
    default = plex adapter for backward compatibility). No vocabulary changes.
  - `src/models/play_event.rs` — doc-comment only: record that `source` is the upstream origin.

  ## APPROACH
  1. `PlaybackEvent` fields: `version: u8` (always 1), `origin: EventOrigin`, `observed_by: String`,
     `upstream_session_key: String`, `kind: PlayStateEventKind` (reuse — do **not** define a second
     vocabulary), `account_ref: Option<String>`, `item_ref: ItemRef`, `position_ms: Option<i64>`,
     `duration_ms: Option<i64>`, client context (`player`/`platform`/`product`/`device`/`ip_address`),
     `media_info: Option<PlaybackMediaInfo>`, `occurred_at: Option<DateTime<Utc>>`, `raw: Json`.
  2. `EventOrigin` is an enum (`Plex`, `Jellyfin`, `Emby`, `Native`) whose `as_source_str()` returns
     the canonical `source` column value — `"plex"` for every Plex-origin observation regardless of
     observer, per §0.3. `observed_by` (`"muse_poller"`, `"muse_webhook"`, `"maestro:plex"`) is
     written into `raw` under an `observed_by` key, never into a dedupe-key column. Document why in
     the same place, because the next person's instinct will be to put it back.
  3. `ItemRef` is an enum — `PlexRatingKey(String)` today, `MuseMediaItemId(i64)` reserved for the
     native engine — mapping onto the existing `rating_key` column for the Plex arm. It must be
     possible to add the native arm later without a migration; state how in the doc comment.
  4. `to_new_play_event()` produces `NewPlayEvent` with `event_type = kind.to_plex_event_type()` — the
     existing on-disk vocabulary is preserved exactly, so `fold_events` needs no change at all. That
     is the whole point: this is a new front door onto an unchanged fold.
  5. `occurred_at` is the observer's clock and is recorded in `raw` for forensics only. Ordering is
     always Muse's DB-assigned `received_at` (`fold_events` sorts by `(received_at, id)`). Say so in
     the doc comment; clock skew across a process boundary must never reorder a fold.
  6. Serde: `#[serde(deny_unknown_fields)]` is **not** used — an unknown field from a newer emitter is
     ignored, not rejected. A stricter contract turns a benign emitter upgrade into an ingest outage.
     A *missing* required field is still a hard parse error.

  ## TEST PLAN
  - Golden JSON fixtures under `src/fixtures/` for one event of each `PlayStateEventKind`, asserting
    the exact serialized field names — the contract MBAK-09's emitter is written against.
  - `to_new_play_event()` for a Plex-origin event from `muse_poller` and the same underlying event
    from `maestro:plex` produce byte-identical `(source, event_type, session_key, view_offset_ms)`
    tuples — the §0.3 dedupe-key property, asserted directly.
  - `interpret.rs` regression: legacy `plex_webhook`, `plex_poll`, `jellyfin_webhook` and
    `snapshot:tautulli` source strings all route to exactly the adapter they route to today; extend
    the existing `from_play_event_source_selects_the_adapter` test rather than replacing it.
  - Round-trip serde for `PlaybackEvent`; unknown extra field is ignored, missing `kind` errors.
  - `cargo test --workspace` via the compiler tool; verify no hardcoded infrastructure values.

  ## EDGE CASES
  - `version` != 1 ⇒ the type parses but `to_new_play_event()` returns an error naming the version.
    Forward compatibility is a decision, not an accident.
  - `position_ms` absent (a `Scrobble` or `Rate` often carries none) ⇒ maps to a NULL
    `view_offset_ms`, which the UNIQUE constraint treats as distinct. Document that this is the
    pre-existing behaviour of the table and is unchanged here.
  - Negative `position_ms` from a misbehaving client ⇒ clamped to 0 with a counted warning, never
    persisted negative (`fold_events`'s `advance` already guards its own arithmetic, but garbage
    should not reach it).
  - `upstream_session_key` empty or whitespace ⇒ rejected at construction, never mapped to a NULL
    `session_key`; a session with no key cannot be reconstructed and would become an orphan row.

- **Acceptance criteria:**
  - [ ] The same underlying Plex event, observed by Muse's poller and by Maestro's adapter, produces an identical `play_events` dedupe key
  - [ ] `event_type` values written are byte-identical to today's, so `fold_events` is untouched
  - [ ] Negative test: a `PlaybackEvent` with an unsupported `version` is rejected with an error naming the version, and one with an empty `upstream_session_key` fails construction
  - [ ] Regression: every legacy `source` string routes to the same interpret adapter it does today; all existing tests still pass
  - [ ] Golden fixtures pin the wire field names for every event kind
  - [ ] No hardcoded infrastructure values in new/modified code

---

### MTRC-02: One ingest core — refactor the poller and webhook onto `PlaybackEvent`
- **Priority:** Critical
- **Labels:** muse, tracker, refactor, rust
- **Agent:** claude
- **Estimate:** 6h
- **Blocked by:** MTRC-01
- **Description:** Make `PlaybackEvent` the *only* path into `play_events` for live observation. The
  Plex poller and webhook stop constructing `NewPlayEvent` themselves and become **adapters** that
  translate Plex shapes into `PlaybackEvent` and hand it to one shared ingest function. This is a
  pure refactor — no observable behaviour change, no new routes, no config. Its value is that after
  it lands, a Maestro-originated event and a poller-originated event traverse *identical* code, which
  is what makes the cutover a config change rather than a second pipeline.

  ## FILES
  - `src/tracker/ingest.rs` (new) — `ingest_playback_event(pool, &PlaybackEvent) -> MuseResult<IngestOutcome>`:
    validate → `to_new_play_event()` → `repo::play_event::insert` → on a non-dedup insert,
    `reconstruct::reconstruct_and_persist` → optional `play_session_media_info` upsert. Returns
    `IngestOutcome { deduped, session_id, reconstructed }`.
  - `src/tracker/poller.rs` — `ingest_one_session` builds a `PlaybackEvent` (including the
    `plex_media_info` block, now returned as `PlaybackMediaInfo`) and calls the ingest core. Delete
    its direct `repo::play_event::insert` / `reconstruct_and_persist` / `upsert_media_info` calls.
  - `src/tracker/webhook.rs` — `handle_payload` builds a `PlaybackEvent` and calls the ingest core.
    The `media.rate` branch and `handle_rating` stay exactly where they are (ratings are not playback
    accounting) but are reached via the same dispatch.
  - `src/metrics.rs` — counters: ingested / deduped / reconstruct-skipped / rejected, labelled by
    origin and `observed_by`.

  ## APPROACH
  1. Move the media-info upsert into the ingest core so it happens for *any* origin that supplies a
     `media_info` block, not only the poller. Today it is poller-only (§0.6); after this item it is a
     property of the ingest path.
  2. Preserve the webhook's existing dedupe short-circuit semantics exactly: a deduped insert skips
     reconstruction (`webhook.rs`'s current early return) because re-folding an unchanged event set is
     redundant, not incorrect. The poller currently reconstructs unconditionally; keep that too — the
     ingest core takes a `reconstruct_on_dedup: bool` from the caller rather than silently unifying
     two behaviours that were chosen for different reasons. Document both choices.
  3. The webhook handler's non-2xx-never rule is unchanged and load-bearing: every failure path still
     logs and returns `200 OK` (Plex retries aggressively; one bad delivery must not stall ingestion).
     The ingest core returns `Result`; the handler swallows it exactly as it does today.
  4. `poller.rs` keeps its own `plex_media_info` mapping function — it is Plex-payload parsing and
     belongs with the Plex adapter, not in the generic core. Only its *destination* changes.
  5. Do not change `fold_events`, `resolve_rating_key`, or any repo function signature in this item.
     `src/shadow/` and `src/parity/` both reach into `tracker::reconstruct`; leaving those untouched
     keeps this refactor's blast radius at the observation layer.

  ## TEST PLAN
  - Every existing test in `src/tracker/` passes **unchanged** — that is this item's primary
    acceptance signal. Do not edit an existing tracker test to make it pass; if one fails, the
    refactor changed behaviour.
  - New unit test: the ingest core called twice with the same `PlaybackEvent` inserts once and reports
    `deduped: true` the second time.
  - New unit test: an event carrying a `media_info` block upserts `play_session_media_info`; the same
    event without one leaves the row untouched rather than writing an empty one (matching
    `plex_media_info`'s current `None` behaviour).
  - Live-DB round-trip (`MUSE_TEST_DATABASE_URL`, skip-when-unset): the poller path and the webhook
    path for one session key still fold into one `play_sessions` row, as
    `persist_and_reconstruct_round_trip_is_idempotent_and_late_tolerant` already asserts.
  - Verify no hardcoded infrastructure values.

  ## EDGE CASES
  - A malformed webhook payload that previously produced a partial `NewPlayEvent` must now fail
    `PlaybackEvent` construction — assert it still answers `200 OK` and logs, never 4xx/5xx.
  - Reconstruction failing (unresolved account/media) must still leave the raw event durable; the
    ingest core returns `reconstructed: false`, not an error.
  - A poll tick and a webhook delivery racing on the same `(event_type, session_key, offset)` — with
    §0.3's canonical origin they now collide on the UNIQUE constraint where previously they did not.
    Assert the collapse is correct and that the fold is unchanged by it.
  - Metrics must not be labelled with anything unbounded (a session key or an IP as a label value is
    a cardinality bomb) — origin and `observed_by` only.

- **Acceptance criteria:**
  - [ ] `poller.rs` and `webhook.rs` contain no direct `repo::play_event::insert` or `reconstruct_and_persist` call
  - [ ] Regression: every pre-existing test in `src/tracker/` passes without modification
  - [ ] `play_session_media_info` is upserted by the ingest core for any origin supplying it, not only the poller
  - [ ] Negative test: a malformed webhook payload still returns `200 OK`, logs, and persists nothing partial
  - [ ] Ingest metrics are labelled by origin and observer only, with no unbounded label values
  - [ ] No hardcoded infrastructure values in new/modified code
  - [ ] All existing tests still pass

---

### MTRC-03: `POST /ingest/play-event` — the versioned, idempotent consumer surface
- **Priority:** Critical
- **Labels:** muse, tracker, maestro, api, axum
- **Agent:** claude
- **Estimate:** 4h
- **Blocked by:** MTRC-02
- **Description:** The door Maestro delivers through. MBAK-09 introduces this route with an ad-hoc
  body; this item makes it speak the MTRC-01 contract and routes it through the MTRC-02 core, so
  there is exactly one ingest implementation rather than a Maestro pipeline beside a Plex pipeline.
  **Reconciliation rule: if MBAK-09 has already merged, this item converts its handler and its body
  shape to `PlaybackEvent` and keeps its tests; if MBAK-09 has not merged, this item creates the route
  and MBAK-09's emitter is written against these fixtures.** Either way the wire contract is owned
  here.

  ## FILES
  - `src/http/mod.rs` — mount `POST /ingest/play-event` on the **protected** router (it writes watch
    state; `/ingest/plex-webhook` stays open because Plex cannot present a bearer). The `/ingest`
    group's `not_implemented` fallback stays for everything else.
  - `src/http/play_event.rs` (new) — the handler: deserialize `PlaybackEvent`, call the ingest core,
    map `IngestOutcome` to a response body.
  - `README.md` — document the route, its auth, its body, and its idempotency contract, using
    placeholder values only.

  ## APPROACH
  1. Request body is a single `PlaybackEvent` (`"v": 1`). A batch array is **not** accepted in v1 —
    Maestro's emitter is already buffered and per-event retryable (MBAK-09); a batch endpoint would
    need partial-failure semantics nobody needs yet. Say so in the handler doc so it is a decision
    rather than an omission.
  2. Response `200 {"ingested": true, "deduped": bool, "session_id": Option<i64>}`. A dedup is a
     **success**, not a 409 — an at-least-once emitter retrying must see success, or it will retry
     forever.
  3. Idempotency is enforced by the database, not by the handler: the `play_events` UNIQUE constraint
     (§0.3). The handler carries no dedupe cache. State this in the doc comment; a handler-side cache
     would be a second, weaker source of truth that disagrees after a restart.
  4. Reject with 400 and a named reason: unsupported `version`, empty `upstream_session_key`, unknown
     `kind`. Reject with 401 when unauthenticated (the standard `auth::require_api_token` layer).
  5. `occurred_at` further from `now()` than `MUSE_TRACKER_INGEST_MAX_SKEW_SECS` is **accepted** and
     counted, not rejected — ordering uses Muse's `received_at` anyway (§0.5/MTRC-01), so a skewed
     clock is a metric, not an error. Rejecting it would drop real watch history over a clock bug.
  6. Follow `src/http/ops.rs` conventions for handler shape and error mapping; no new error variants.

  ## TEST PLAN
  - `oneshot` over the real router: unauthenticated → 401; authenticated valid event → 200 with
    `ingested: true`; the identical event again → 200 with `deduped: true` and the same `session_id`.
  - `oneshot`: unsupported version → 400 naming the version; empty session key → 400; unknown kind → 400.
  - `oneshot`: an event with `occurred_at` an hour in the past → 200, skew counter incremented.
  - The golden fixtures from MTRC-01 deserialize successfully through the route (contract stability
    for MBAK-09's emitter).
  - Verify no hardcoded infrastructure values.

  ## EDGE CASES
  - An event whose account/media cannot be resolved ⇒ 200 with `session_id: null`; the raw event is
    durable and a later call reconstructs it. Never a 4xx — Maestro must not retry a resolvable-later
    event forever.
  - A body that is valid JSON but not a `PlaybackEvent` ⇒ 400 with the field named, never a 500.
  - Route mounted with `:param`-free path — no axum 0.7 brace-route exposure here, but the reviewer
    should confirm no `{}` syntax crept in.
  - Concurrent identical deliveries ⇒ one insert wins, the other reports `deduped`; assert no error
    surfaces from the `ON CONFLICT DO NOTHING` race.

- **Acceptance criteria:**
  - [ ] `POST /ingest/play-event` requires the bearer token and accepts a `"v": 1` `PlaybackEvent`
  - [ ] A duplicate delivery returns 200 with `deduped: true` and the same `session_id` — never a 409, never a second row
  - [ ] Negative test: unsupported version, empty session key, and unknown kind each return 400 naming the reason; unauthenticated returns 401
  - [ ] Idempotency is enforced by the `play_events` UNIQUE constraint, with no handler-side dedupe cache
  - [ ] README documents the route, auth, body and idempotency contract with placeholder values
  - [ ] No hardcoded infrastructure values in new/modified code
  - [ ] All existing tests still pass

---

### MTRC-04: Session identity + media-info continuity — the no-double-count proof
- **Priority:** Critical
- **Labels:** muse, tracker, continuity, taste, rust
- **Agent:** claude
- **Estimate:** 5h
- **Blocked by:** MTRC-03
- **Description:** The item the epic's requirement 3 asks for, made explicit and testable. Pin the
  session-identity contract (§0.5), pin the media-info contract (§0.6), and prove with a test that
  **one upstream Plex session, observed across an observer change, produces exactly one reconstructed
  `play_sessions` row** — not two, and not one with doubled watch time.

  ## FILES
  - `src/tracker/ingest.rs` — enforce the identity contract: `session_key` is
    `upstream_session_key` verbatim. Reject (400 upstream, error internally) any attempt to pass a
    Maestro-minted id in that field, identified by a documented reserved prefix.
  - `src/tracker/event.rs` — add `emitter_session_id: Option<String>` (Maestro's own id, forensics
    only, written into `raw`, never used as a key) and document the distinction at length.
  - `src/tracker/mod.rs` — extend the live-DB test module with the cutover round-trip below.
  - `src/tracker/ingest.rs` — media-info: an event whose `media_info` is `None` must **leave an
    existing `play_session_media_info` row untouched**, never overwrite it with nulls.
  - `README.md` — a short "watch-state ownership" section stating the consumer contract.

  ## APPROACH
  1. The consumer contract, stated once and referenced everywhere: **`reconstruct.rs` is idempotent
     over the event *set*** (`fold_events` sorts by `(received_at, id)` and folds; re-running over an
     unchanged set reproduces the identical row). Therefore an emitter may deliver at-least-once and a
     re-observation may overlap, provided every observation of one upstream session carries the same
     `session_key`. That single proviso is the entire consumer contract, and it is why identity is
     pinned here rather than left to each backend.
  2. Reserved-prefix guard: any `upstream_session_key` beginning with the documented Maestro-internal
     prefix is rejected at ingest with a named error. This catches the exact mistake — an emitter
     sending its own id — at the earliest possible point instead of at taste-model review time months
     later.
  3. Media info: merge semantics, not replace. `None` means "this observation did not carry it," not
     "it is now unknown." A poller row followed by a Maestro row without transcode detail must not
     erase the codecs the Activity panel is rendering.
  4. `PlaybackMediaInfo` maps onto the existing `NewPlaySessionMediaInfo` (`DecisionKind`,
     container/codecs/resolution/bitrate/dimensions/`transcode_reason`) with no schema change. If
     Maestro's plex adapter cannot report a field, it sends `None` — never a fabricated default. This
     is the same "refuse to fabricate an unobserved fact" convention `foundry/plan.rs` already
     follows, and epic §8.6's `can_report_transcode_detail` capability exists precisely so the panel
     can say "Plex cannot report this" instead of rendering a zero as a fact.
  5. The double-count analysis, written into the module doc so it is not re-derived: `fold_events`
     accumulates `view_offset_ms` deltas while playing, falling back to wall-clock only when the
     offset goes backwards. Two observations at the *same* offset contribute a zero delta, so
     duplicate observation cannot inflate `watched_ms`. What it *can* inflate is `paused_counter` /
     `paused_ms`, if the two observers disagree about player state on adjacent ticks. That residual
     is the honest reason `shadow` mode (MTRC-06) exists rather than flipping straight to `maestro`.

  ## TEST PLAN
  - **The epic's required test**, live-DB (`MUSE_TEST_DATABASE_URL`, skip-when-unset): persist a
    play/pause/resume stream as `muse_poller`, then continue the *same* upstream session as
    `maestro:plex` through to `stop`, reconstruct, and assert exactly one `play_sessions` row, one
    `started_at`, and a `watched_ms` equal to the single-observer control run.
  - Pure-fold unit test (no DB): interleave duplicate observations of the same offsets from two
    observers and assert `watched_ms` and `percent_complete` are identical to the single-observer fold.
  - Reserved-prefix guard: an emitter-minted session key is rejected with a named error.
  - Media-info merge: a media-info-bearing event followed by one without leaves the row intact.
  - Verify no hardcoded infrastructure values.

  ## EDGE CASES
  - Plex reuses `sessionKey` values across sessions after a server restart — two genuinely different
    sessions can share a key. `started_at` (first event's `received_at`) already separates them via
    the `play_sessions` UNIQUE; add a test asserting a reused key with a large time gap produces two
    rows, which is correct, and note the limitation honestly in the doc.
  - The webhook's synthesized `webhook:{account}:{player}:{rating_key}` key (used when
    `Metadata.sessionKey` is absent) must keep working unchanged — assert it.
  - A `stop` arriving before its `start` (retry reordering) — the fold tolerates partial sequences;
    assert it rather than assuming.
  - A session with no resolvable account ⇒ no `play_sessions` row yet, raw events retained; assert the
    later resolution still produces exactly one row.

- **Acceptance criteria:**
  - [ ] One upstream Plex session observed across an observer change reconstructs to exactly one `play_sessions` row with the same `watched_ms` as a single-observer control
  - [ ] Negative test: an emitter-minted session key is rejected at ingest with a named error
  - [ ] A media-info-free observation never erases existing `play_session_media_info`
  - [ ] A reused upstream session key separated by a time gap correctly yields two sessions, and this limitation is documented
  - [ ] README documents the watch-state ownership and consumer contract
  - [ ] No hardcoded infrastructure values in new/modified code
  - [ ] All existing tests still pass

---

### MTRC-05: Observer ownership mode — the config-gated flip
- **Priority:** High
- **Labels:** muse, tracker, maestro, config, ops
- **Agent:** claude
- **Estimate:** 6h
- **Blocked by:** MTRC-04
- **Description:** The mechanism that hands Plex observation to Maestro, and hands it back. One
  config value, three modes (§0.4), enforced at every place that observes Plex, with the mode visible
  in `/health` and in metrics so "which process is watching Plex right now" is never a guess.

  ## FILES
  - `src/config.rs` — `tracker_plex_observer: TrackerObserverMode` (`MUSE_TRACKER_PLEX_OBSERVER`,
    default `Muse`), `tracker_relay_timeout_ms`. Add both to the env-var list in the config test module.
  - `src/tracker/observer.rs` (new) — the `TrackerObserverMode` enum, its parsing (unknown value ⇒
    **fail closed to `Muse`** with a loud warning, never to `Maestro`), and the predicates
    `should_poll()`, `should_reconstruct_locally()`, `should_relay_webhook()`.
  - `src/workers.rs` / `src/tracker/poller.rs` — `poller::spawn` consults the mode: it spawns in
    `muse` and `shadow`, and does not spawn in `maestro`. It logs the mode and the reason on exactly
    one line at startup, same posture as its existing "poller disabled" log.
  - `src/tracker/webhook.rs` — in `maestro` mode the handler **relays** the raw payload to Maestro's
    observation ingress instead of ingesting locally, then returns `200 OK` regardless. On relay
    failure or timeout it **falls back to local ingest** and increments a downgrade counter.
  - `src/http/mod.rs` — `/health` gains `"tracker_plex_observer": "<mode>"`.
  - `src/metrics.rs` — a mode gauge plus relay attempted/failed/downgraded counters.
  - `README.md` — the mode ladder, what each mode means operationally, and the flip/rollback procedure.

  ## APPROACH
  1. Why the webhook **relays** rather than being switched off: Plex's webhook target is configured in
     Plex, an operator-owned setting that will lag a mode flip. Silently dropping deliveries during
     that lag loses watch history permanently. Relaying keeps Muse's URL valid while making Maestro
     the interpreter, which is what "sole observer" actually means.
  2. Why the relay **fails open** to local ingest: §0.3's canonical-origin rule makes a duplicated
     observation collapse on the UNIQUE constraint, so the cost of a fallback is bounded (a possible
     `paused_counter` wobble, per MTRC-04's analysis) while the cost of a dropped delivery is
     permanent history loss. Fail-open is the correct trade **only because** §0.3 holds — state that
     dependency in the doc comment, because if someone later reverts §0.3 this fallback becomes unsafe.
  3. Never-zero-observers: the mode ladder only ever *adds* an observer before removing one. `shadow`
     runs both. Going `muse` → `maestro` directly is permitted by config but the startup log warns
     that the shadow comparison (MTRC-06) was skipped. Going to `maestro` while Maestro is unreachable
     is caught by the readiness assertion in step 4.
  4. Startup readiness assertion: in `maestro` mode, Muse probes Maestro's health once at boot. If it
     is unreachable, Muse logs an error, increments a counter, and **keeps the poller running** for
     this boot (an explicit, logged temporary two-observer state) rather than leaving Plex unobserved.
     Zero observers is the one state the system must never enter; two is survivable by §0.3.
  5. Reuse the existing `plex_url`/`plex_token` config for the poller; add no new Plex credential. The
     relay target and its token follow MBAK-02/MBAK-09's existing config and `SecretManager` access —
     never `std::env::var` for the token (S7).
  6. Mode changes take effect at process start only. Hot-reload is explicitly out of scope: a mode
     change mid-process would need to drain in-flight reconstruction, and a restart is already the
     deploy unit. Say so rather than leaving it ambiguous.

  ## TEST PLAN
  - Unit: mode parsing for each valid value; an unknown/garbage value parses to `Muse` with a warning
    (fail-closed), and an empty value does the same.
  - Unit: `should_poll()` / `should_relay_webhook()` / `should_reconstruct_locally()` truth table for
    all three modes, asserted exhaustively — this table *is* the invariant.
  - `httpmock`: in `maestro` mode a webhook delivery is relayed; a 500 from the relay target triggers
    local-ingest fallback and increments the downgrade counter; a timeout does the same.
  - `oneshot`: `/health` reports the active mode in every mode.
  - Unit: in `maestro` mode with an unreachable Maestro at boot, the poller is still spawned and the
    "kept polling" counter is incremented.
  - Verify no hardcoded infrastructure values.

  ## EDGE CASES
  - Plex unconfigured (`PLEX_URL`/`PLEX_TOKEN` unset) in `maestro` mode ⇒ the existing "poller
    disabled" path wins and no relay target is contacted; assert the single log line, no panic.
  - A relay that succeeds but Maestro drops the event internally is invisible to Muse — that is
    exactly what MTRC-06's shadow comparison and MTRC-07's continuity check exist to catch. Note the
    limitation rather than pretending the relay is an end-to-end guarantee.
  - Mode gauge must be a small bounded label set (three values), not a free-form string.
  - The webhook handler must still never return non-2xx, including on relay failure.

- **Acceptance criteria:**
  - [ ] `MUSE_TRACKER_PLEX_OBSERVER` defaults to `muse`, so merging this spec changes nothing until an operator flips it
  - [ ] The mode truth table (`should_poll`/`should_relay_webhook`/`should_reconstruct_locally`) is asserted exhaustively for all three modes
  - [ ] Negative test: an unknown mode value fails closed to `muse` with a warning, and a failed relay falls back to local ingest with a counted downgrade
  - [ ] In `maestro` mode with Maestro unreachable at boot, the poller keeps running rather than leaving Plex unobserved
  - [ ] `/health` and a metrics gauge report the active mode
  - [ ] README documents the mode ladder and the flip/rollback procedure
  - [ ] No hardcoded infrastructure values in new/modified code
  - [ ] All existing tests still pass

---

### MTRC-06: Shadow mode — prove equivalence before trusting the new observer
- **Priority:** High
- **Labels:** muse, tracker, parity, verification
- **Agent:** claude
- **Estimate:** 5h
- **Blocked by:** MTRC-05
- **Description:** The intermediate rung of the ladder. In `shadow` mode both observers run, Muse
  stays the authoritative writer, and Maestro's stream is compared against Muse's own. The flip to
  `maestro` is gated on that comparison being clean — evidence, not optimism. This reuses the parity
  machinery already in tree (`src/parity/`, `src/shadow/`), which exists to fold `play_events` and
  compare reconstructions, rather than building a second comparison harness.

  ## FILES
  - `src/tracker/shadow_compare.rs` (new) — for each session key observed in a window, fold the
    events attributable to each observer separately (via the `observed_by` marker in `raw`) and diff
    the resulting `Fold`s field by field.
  - `src/parity/mod.rs` — reuse its existing fold-comparison helpers; extend rather than duplicate.
  - `src/http/ops.rs` + `src/http/mod.rs` — `POST /ops/tracker/shadow-report` returning the
    divergence report (protected, under the existing `ops` router).
  - `src/metrics.rs` — divergence counters by field.
  - `README.md` — how to read the report and what "clean" means.

  ## APPROACH
  1. The comparison unit is the `Fold`, not the persisted row: `fold_events` is pure and already the
     thing everything downstream depends on. Comparing folds isolates observation differences from
     resolution/persistence differences.
  2. Report per session: sessions seen by both, by Muse only, by Maestro only, and — the important
     one — per-field divergence (`watched_ms`, `percent_complete`, `paused_counter`, `paused_ms`,
     `is_finished`, `is_abandoned`, `stopped_at`). Tolerances are explicit and configurable, not
     hidden: `watched_ms` within one poll interval is expected and not a divergence; `is_finished`
     differing is *always* a divergence because it changes taste attribution.
  3. **Maestro-only sessions are a good sign, not a bug** (its adapter may see sessions the poller
     missed between ticks). **Muse-only sessions are the red flag** — they mean Maestro would have
     lost that history. The report must state which asymmetry it found, in those terms, rather than
     reporting a symmetric "N differences."
  4. The report is read-only. It never mutates `play_sessions` and never changes the mode. Promotion
     to `maestro` remains an operator config change informed by the report — an automatic flip on a
     clean report would be a system that promotes itself past a gate.
  5. Bound the scan by `MUSE_TRACKER_CONTINUITY_WINDOW_HOURS` and a row cap, so the report is cheap
     enough to run repeatedly against a live database.

  ## TEST PLAN
  - Unit: two identical event streams tagged with different observers ⇒ zero divergences.
  - Unit: a Maestro stream missing the final `stop` ⇒ divergence on `stopped_at` and `is_finished`,
    classified as a Muse-only-completion red flag.
  - Unit: `watched_ms` differing by less than one poll interval ⇒ within tolerance, not reported.
  - Unit: a session seen only by Maestro is reported as coverage gained, not as a divergence.
  - `oneshot`: the ops route is protected; unauthenticated → 401; empty window → 200 with an empty
    report, never an error.
  - Verify no hardcoded infrastructure values.

  ## EDGE CASES
  - Legacy rows with no `observed_by` marker (everything before MTRC-01) ⇒ attributed to Muse, never
    dropped from the report silently.
  - A window containing zero sessions ⇒ a clean report is **not** evidence of equivalence; the report
    must state the sample size and refuse to render a verdict below a minimum count.
  - Very long sessions spanning the window boundary ⇒ include by session start, and say so.
  - The report must contain no PII: no IP addresses, no account names — session keys and counts only.

- **Acceptance criteria:**
  - [ ] Identical streams from two observers report zero divergences; a Maestro stream missing a `stop` reports an `is_finished` divergence
  - [ ] Muse-only sessions are classified as a red flag and Maestro-only sessions as coverage gained, distinctly
  - [ ] Negative test: a window with fewer than the minimum sample size refuses to render a verdict rather than reporting "clean"
  - [ ] The report is read-only — it never mutates watch state and never changes the observer mode
  - [ ] The report contains no PII (no IPs, no account identifiers)
  - [ ] README explains how to read the report and what gates the flip
  - [ ] No hardcoded infrastructure values in new/modified code
  - [ ] All existing tests still pass

---

### MTRC-07: Watch-history continuity verification and reconciliation
- **Priority:** Critical
- **Labels:** muse, tracker, taste, continuity, ops
- **Agent:** claude
- **Estimate:** 5h
- **Blocked by:** MTRC-05
- **Description:** The epic's requirement 4, made operational. Flipping backends must never tear or
  duplicate history, because `taste_model/aggregate.rs` rolls `play_sessions` into `watch_stats` and
  corruption there is silent. This item ships the check that proves the join is clean across the
  cutover boundary, plus the reconciliation pass that repairs the one failure it can repair.

  ## FILES
  - `src/tracker/continuity.rs` (new) — the checks and the reconciliation pass.
  - `src/http/ops.rs` + `src/http/mod.rs` — `POST /ops/tracker/continuity` (protected) running the
    checks and returning a structured verdict; `?repair=true` runs the reconciliation.
  - `src/metrics.rs` — gauges for suspected tears and duplicate candidates.
  - `README.md` — the post-flip verification procedure, as a numbered runbook.

  ## APPROACH
  1. Three checks over a window spanning the cutover instant:
     - **No tear.** Two `play_sessions` rows for the same `(account_id, media_item_id, episode_id)`
       whose `[started_at, stopped_at]` intervals are adjacent or overlapping within a small
       threshold are a suspected torn session. This is the exact shape a session-key change produces.
       `repo::play_session::find_overlapping_native` already implements overlap detection for the
       Tautulli importer — reuse it rather than writing a second overlap query.
     - **No duplicate.** Two rows for the same triple with near-identical `started_at` and
       `watched_ms` are a suspected double-count.
     - **No gap.** Compare observed session counts per hour either side of the boundary against the
       trailing baseline; a step change beyond a configured factor is flagged. A quiet household hour
       is not a gap, so the check reports a ratio and a sample size, never a bare boolean.
  2. Reconciliation (`?repair=true`) does exactly one thing: **re-run
     `reconstruct_and_persist` for every session key in the window.** The fold is idempotent over the
     event set, so re-folding repairs any row that was persisted from a partial view. It never
     deletes, never merges rows, and never writes a value it did not derive from `play_events`. Any
     tear that re-folding cannot fix is *reported for operator decision*, not auto-merged — merging
     two watch sessions is a judgement about what the household actually watched, and a wrong merge
     is unrecoverable.
  3. `play_events` is append-only and was never mutated by any step of this cutover, which is what
     makes re-folding a safe repair rather than a guess. State that explicitly; it is the single
     property the whole rollback and repair story rests on.
  4. The verdict is a structured object (per-check status, counts, sample sizes, and the specific
     session ids implicated) so it can be pasted into a Plane comment as evidence. A bare "OK" is not
     evidence.
  5. Run the check **before** the flip (as a baseline), and again after. The README procedure says so
     explicitly — a post-flip-only check cannot distinguish a new tear from a pre-existing one.

  ## TEST PLAN
  - Unit with synthetic rows: a torn pair (adjacent intervals, same triple) is flagged; a legitimate
    re-watch hours later is **not**.
  - Unit: a duplicate pair (same triple, near-identical start and watched time) is flagged.
  - Unit: a quiet hour with a small sample is reported as insufficient sample, not as a gap.
  - Live-DB (skip-when-unset): a session persisted from a partial event view, then repaired by
    re-folding, converges to the same row the full event set produces — asserting repair is
    convergence, not mutation.
  - `oneshot`: the route is protected; `?repair=true` without auth → 401.
  - Verify no hardcoded infrastructure values.

  ## EDGE CASES
  - A genuine back-to-back double feature of the *same* item (rewatch immediately after finishing)
    looks exactly like a tear. The threshold must be tunable and the check must report it as
    *suspected*, never auto-repair it. Document this as the known false positive.
  - Sessions with NULL `media_item_id`/`episode_id` (unresolved) — Postgres treats NULLs as distinct
    in the `play_sessions` UNIQUE (documented in `repo::play_session::upsert`), so these can
    legitimately multiply. Exclude them from the duplicate check and count them separately.
  - The repair pass must be bounded and resumable — a window cap and a row cap, never an unbounded
    full-table re-fold on a live database.
  - Re-folding a session whose account/media is still unresolved must be a no-op, not an error.

- **Acceptance criteria:**
  - [ ] The continuity check reports no-tear, no-duplicate and no-gap verdicts with counts, sample sizes and implicated session ids
  - [ ] Reconciliation repairs only by re-folding the append-only event set; it never deletes, merges, or fabricates a value
  - [ ] Negative test: a legitimate re-watch hours later is not flagged as a tear, and a quiet hour is reported as insufficient sample rather than a gap
  - [ ] Unresolved (NULL-item) sessions are excluded from the duplicate check and counted separately
  - [ ] The repair pass is bounded and resumable
  - [ ] README documents the before-and-after verification runbook
  - [ ] No hardcoded infrastructure values in new/modified code
  - [ ] All existing tests still pass

---

### MTRC-08: Rollback — return to the poller without losing anything observed in between
- **Priority:** High
- **Labels:** muse, tracker, rollback, ops
- **Agent:** claude
- **Estimate:** 3h
- **Blocked by:** MTRC-07
- **Description:** The epic's requirement 6. Rollback is `MUSE_TRACKER_PLEX_OBSERVER=muse` plus a
  restart — but that is only true if the events Maestro delivered during the cutover era remain valid
  input to the fold afterwards. This item proves that and writes it down as a runbook.

  ## FILES
  - `src/tracker/observer.rs` — doc comment: the rollback contract and why no data migration is needed.
  - `src/tracker/continuity.rs` — a post-rollback reconciliation entry point (the same bounded
    re-fold, scoped to the cutover-era window).
  - `README.md` — the rollback runbook, numbered, with the verification step included rather than
    implied.
  - `docs/` — if a tracker operations doc exists, cross-link; otherwise the README section stands alone.

  ## APPROACH
  1. Why no data migration: Maestro-era events are ordinary `play_events` rows with `source = "plex"`
     (§0.3) and the standard event vocabulary (MTRC-01 step 4). After rollback, the poller writes rows
     that are indistinguishable in kind, and `fold_events` folds all of them together — it has no
     notion of who observed what. **Nothing about a Maestro-era row is only interpretable by Maestro.**
     This is a direct dividend of §0.3 and is worth stating as such.
  2. Rollback sequence, in order, each step verifiable: (a) run the continuity check and record the
     verdict as the pre-rollback baseline; (b) set the mode to `muse`; (c) restart Muse; (d) confirm
     `/health` reports `muse` and the poller's startup log line appears; (e) confirm Plex's webhook
     target still points at Muse (it was never changed — the relay preserved it, which is the second
     reason for the relay design); (f) run the bounded reconciliation over the cutover-era window;
     (g) re-run the continuity check and compare against the baseline.
  3. There is deliberately **no** "roll back the data" step. The append-only event log means the
     correct repair is always re-folding, never deletion. Any runbook step that deletes `play_events`
     rows is wrong and the doc says so explicitly.
  4. Maestro may keep running after a Muse-side rollback — its emitter's deliveries simply become a
     second observer again, which §0.3 makes idempotent. The runbook notes that stopping
     `maestro.service` is optional and is a separate decision from the tracker mode.

  ## TEST PLAN
  - Live-DB (skip-when-unset): a session whose events came half from `maestro:plex` and half from the
    restored poller folds to one row with the correct `watched_ms` — the rollback mirror of MTRC-04's
    cutover test.
  - Unit: the post-rollback reconciliation entry point is bounded by the same window/row caps as
    MTRC-07's.
  - Documentation check: the runbook's steps map one-to-one onto real, existing routes/log lines/health
    fields — no step references something that does not exist.
  - Verify no hardcoded infrastructure values.

  ## EDGE CASES
  - Rolling back mid-session: the in-flight session's remaining events arrive from the poller under
    the same session key and fold into the same row. Assert it.
  - Rolling back while Maestro is still emitting ⇒ two observers, idempotent by §0.3; assert no
    duplicate rows result.
  - Rolling back after a long `maestro`-mode period ⇒ the reconciliation window must be widened; the
    runbook must say how rather than leaving the operator to guess.

- **Acceptance criteria:**
  - [ ] Rollback is a config value plus a restart, with no data migration and no deletion step
  - [ ] A session split across Maestro-era and post-rollback poller events folds to exactly one row with correct watch time
  - [ ] Negative test: rolling back while Maestro is still emitting produces no duplicate sessions
  - [ ] The runbook's every step maps onto a real route, log line, or health field
  - [ ] README documents the rollback runbook including its verification step
  - [ ] No hardcoded infrastructure values in new/modified code
  - [ ] All existing tests still pass

---

### MTRC-09: Say plainly what this retires of the Tautulli path — and what it does not
- **Priority:** Medium
- **Labels:** muse, tracker, docs, tautulli
- **Agent:** gemini
- **Estimate:** 3h
- **Type:** documentation
- **Blocked by:** MTRC-08
- **Description:** `src/tracker/mod.rs` opens with "the native Plex tracker — THE Tautulli
  replacement." After this spec that sentence is half wrong, and leaving it is how a future agent
  concludes the Tautulli importer is dead code and deletes 74k lines of working backfill. This item
  states the boundary precisely, in the tree, where it will actually be read.

  ## AUDIENCE
  The operator, and the next agent to open `src/tracker/` or `src/tautulli/` — including a reviewer
  of spec D or E who needs to know which watch-state paths are live.

  ## OUTLINE
  - **What this retires (~200 words).** Muse's *ongoing live capture* of Plex sessions — the
    `/status/sessions` poll loop and the local interpretation of Plex webhook payloads. In `maestro`
    mode those move behind Maestro's plex adapter. This is the strangler-fig step: the Tautulli
    *replacement capture path* is now one backend adapter among several rather than the only way
    watch state enters the system.
  - **What this explicitly does NOT retire (~350 words).** All of it stays, all of it still runs:
    `src/tautulli/backfill.rs` (the one-time historical import), `src/tautulli/client.rs`,
    `play_sessions.tautulli_ref_id` provenance, `repo::play_session::find_by_tautulli_ref` /
    `find_overlapping_native` / `attach_tautulli_ref` (the last of which MTRC-07 now also depends on),
    `POST /ops/ingest/tautulli`, `POST /ops/library/resolve`, the `snapshot:tautulli` normalisation
    path in `src/snapshot/normalize.rs`, and the `TAUTULLI_URL`/`TAUTULLI_API_KEY` config. Historical
    `play_events` rows with legacy `source` values keep routing exactly as they do today.
  - **What is unchanged in principle (~150 words).** Muse remains the single writer of watch state
    (epic §2); `reconstruct.rs` remains the one reconstruction algorithm; the taste model's input
    contract is untouched.
  - **The mode ladder and where to find the runbooks (~100 words).** Point at MTRC-05's README
    section and MTRC-07/08's runbooks rather than restating them.

  ## SOURCES
  - `src/tracker/mod.rs`, `src/tracker/observer.rs`, `src/tautulli/mod.rs`, `src/tautulli/backfill.rs`
  - `specs/S130-maestro-epic.md` §2, §4b, §8.8
  - `specs/S130-B-maestro-backends.md` §0.2, MBAK-09
  - This spec's §0.

  ## TONE
  Technical reference, direct, no filler. Correct the existing claim rather than deleting it — say
  what it meant, what it means now, and when it changed. No hardcoded infrastructure values; env var
  names and placeholder values only.

  ## APPROACH
  1. Rewrite the `src/tracker/mod.rs` module doc's opening paragraphs; keep its accurate description
     of the four cooperating pieces and add the ingest seam and the observer mode.
  2. Add the boundary statement to `src/tautulli/mod.rs`'s module doc — that file is where someone
     stands when they wonder whether it is dead.
  3. Add a "watch state and the tracker" section to `README.md` covering the ownership split, the mode
     ladder, and links to the runbooks.
  4. Amend `specs/S130-maestro-epic.md` §8.8 to record that spec J landed and what its final shape
     was, per the epic's own convention of amending rather than diverging.

- **Acceptance criteria:**
  - [ ] `src/tracker/mod.rs`'s "THE Tautulli replacement" claim is corrected with what changed and when
  - [ ] `src/tautulli/mod.rs` states explicitly that the backfill importer and its provenance surface are live and not retired
  - [ ] README has a watch-state section covering the ownership split, the mode ladder, and links to the flip/rollback/continuity runbooks
  - [ ] Epic §8.8 is amended to record spec J's landed shape
  - [ ] No hardcoded infrastructure values in any modified file
  - [ ] All existing tests still pass

---

## §2. Dependency graph

```
MTRC-01 (PlaybackEvent)
   └── MTRC-02 (one ingest core; poller + webhook refactored onto it)
          └── MTRC-03 (POST /ingest/play-event, versioned + idempotent)
                 └── MTRC-04 (session identity + media-info continuity; the one-session proof)
                        ├── MTRC-05 (observer mode: muse | shadow | maestro)
                        │      └── MTRC-06 (shadow divergence report — gates the flip)
                        └── MTRC-07 (continuity verification + re-fold reconciliation)
                               └── MTRC-08 (rollback runbook + post-rollback reconciliation)
                                      └── MTRC-09 (docs: what the Tautulli path retires, and does not)
```

MTRC-01→04 are pure code and can be built and merged with no operational effect whatsoever — the
observer mode does not exist until MTRC-05, and it defaults to today's behaviour when it does. That
ordering is deliberate: the entire backend-agnostic ingest path lands, is reviewed, and is tested
before anything about who observes Plex changes.

## §3. Ownership after the cutover — the one-line answers

| Concern | Owner after this spec |
|---|---|
| Observing Plex `/status/sessions` | **Maestro's plex adapter** (`maestro` mode) |
| Receiving Plex webhook deliveries (transport) | **Muse** — relays to Maestro, falls back to local ingest |
| Normalising an observation into `PlaybackEvent` | **The observer** (Maestro's adapter, or Muse's Plex adapters pre-flip) |
| Writing `play_events` | **Muse**, via `tracker::ingest` only |
| Reconstructing `play_sessions` | **Muse**, via `reconstruct.rs` only — unchanged, and still the only algorithm |
| `play_session_media_info` | **Muse**, from whatever `media_info` the observation carried |
| Interpretation, taste, `watch_stats` | **Muse** — untouched by this spec |
| Historical Tautulli import and its provenance | **Muse** — untouched by this spec |
