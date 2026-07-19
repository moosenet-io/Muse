# tracker

The native Plex playback tracker (85 KG nodes) — **the Tautulli replacement** (MUSE-07,
spec §4). Three cooperating pieces over one append-only source of truth, plus an
interpretation layer:

- **`webhook`** — `POST /ingest/plex-webhook`: Plex Pass event push.
- **`poller`** — a background `/status/sessions` poll loop that fills the gaps the
  webhook misses, or is the only path when webhooks aren't available.
- **`reconstruct`** — folds the raw `play_events` stream (written by both paths) into a
  single, idempotent, late-event-tolerant `play_sessions` row per session key.
- **`interpret`** — MUSEX-10: interprets a stopped session's pattern (dislike vs fatigue
  vs interruption vs delight) as the passive signal a future adaptation loop consumes.

Both ingest paths funnel through the same `play_events` table and the same
`reconstruct_and_persist` — there is exactly one reconstruction algorithm, not one per
source.

## Key types and functions

| Symbol | Kind | File | What it does |
|---|---|---|---|
| `tracker::reconstruct::fold_events` | fn | `src/tracker/reconstruct.rs` | The core fold: a slice of raw `PlayEvent`s → one `Fold` (session state), tolerant of late/duplicate events |
| `tracker::reconstruct::advance` | fn | `src/tracker/reconstruct.rs` | Advances the fold state machine one event at a time |
| `tracker::reconstruct::extract_duration_ms` | fn | `src/tracker/reconstruct.rs` | Pulls duration from the raw event JSON defensively |
| `tracker::interpret::interpret_play_state` | fn | `src/tracker/interpret.rs` | `SessionPattern` → `InterpretedSignal` (the dislike/fatigue/interruption/delight disambiguation) |
| `tracker::webhook::str_field` | fn | `src/tracker/webhook.rs` | Defensive nested-path string extraction from the Plex webhook payload |
| `tracker::poller::spawn` | fn | `src/tracker/poller.rs` | Spawns the background session-poll worker (always spawned; no-ops when Plex is unconfigured) |

## How it connects

The webhook handler is mounted by `http::router`; the poller is spawned by
`workers::spawn_workers`. Both call `plex` (the read-only typed client) for session data
and persist through `repo::play_event` / `repo::play_session`. Downstream, `taste_model`
derives `taste_signals` from the watch stats this subsystem produces, and the operator
`shadow` runner reuses the *real* fold/resolve functions (`fold_events`,
`resolve_rating_key` — widened to `pub(crate)` for exactly that) instead of
reimplementing them, so shadow analytics can never drift from production reconstruction.

## Configuration

- `PLEX_URL`, `PLEX_TOKEN` — enable the Plex client; unset means the poller no-ops.
- `MUSE_PLEX_POLL_SECS` — poll cadence (poller default: 10s when unset).

## Notes and gaps

- `interpret` is read-only with respect to any live server; the adaptation loop that
  would consume its signals (MUSEX-11) is a separate concern.
- Webhook ingest requires Plex Pass; the poller-only path is the fallback and is the
  reason the poller is always spawned.
- Not covered here: the Tautulli history *backfill* (`src/tautulli/`), a separate,
  manually-invoked seam.
