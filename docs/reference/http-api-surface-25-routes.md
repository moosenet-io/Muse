## HTTP API surface (25 routes)

Every route below is real. The `/ingest/*`, `/query/*`, and `/proactive/*` nests each also carry a
`fallback` that answers **`501 Not Implemented`** for any un-mounted sub-path (a documented seam for
future spec items). Error→status mapping lives in `src/error.rs`.

**Health**
- `GET /health` — `{status:"ok", version, db:"up"|"down"}`, never 500s.

**Ingest**
- `POST /ingest/plex-webhook` — native Plex tracker webhook receiver (multipart, `payload` field). Always `200`.

**Query / recall**
- `POST /query/resolve` — free-text → library item, via a vector → trigram → (opt-in) TMDb ladder. `{tier, results}`.
- `POST /query/similar` — "more like this" for a `media_item_id`, vector-first with genre fallback. `{tier, results}`.

**Recommend / curation**
- `POST /recommend` — full ranked list (on-deck + gap + taste + availability-aware). `{items}`.
- `GET /recommend/on_deck?account_id=&limit=` — continue-watching only.
- `GET /recommend/gaps?account_id=&limit=` — gap analysis only.

**Proactive (Lumina's poll surface)**
- `GET /proactive/pending?account_id=&limit=` — eligible undelivered nudges. `{items}`.
- `POST /proactive/{id}/ack` — body `{"outcome":"sent"|"dismissed"}`; other values → `400`. `{item}`.

**Web guide + artwork**
- `GET /` and `GET /guide` — self-contained EPG-style channel-guide HTML page.
- `GET /api/channels` — channel summaries.
- `GET /api/channels/{id}/lineup` — a channel's program lineup (window `now-2h … now+24h`).
- `GET /art/{kind}/{id}?variant=poster` — artwork proxy (Postgres-cached; never leaks the Plex token; serves a 1×1 placeholder rather than 404).

**Request lifecycle (MUSEM-05, auth-gated — `Authorization: Bearer <MUSE_API_TOKEN>`)**
- `POST /requests` — body `{provider_ids, kind, title, quality_profile_id?}`. Classifies the
  request via `arr::request::classify_tier`, using a REAL on-demand Prowlarr search (MUSEM-03) as
  the availability signal rather than a fabricated one (see `src/acquisition`'s module doc).
  `Blocked` (no Prowlarr configured, or no `quality_profile_id`) → `400`, never persisted.
  `AutoApprovable` → persisted, then fulfilled immediately (search → decide → grab). Anything else
  → persisted as `Requested` for manual review. `{request, tier, outcome}`.
- `GET /requests?status=` — list requests, optionally filtered to one `media_requests.status`
  value; omitted lists across every status. Auth-gated so an unauthenticated caller can never
  enumerate request data (the same CAP-SEC-03 lesson `/recommend*` already applies).
- `POST /requests/{id}/approve` — fulfills a `Requested` request now (search → decide → grab).
  Idempotent: approving anything not currently `Requested` (already `Grabbed`/`Failed`/`Denied`/…)
  is a no-op, never a second grab.
- `POST /requests/{id}/deny` — marks `Requested` → `Denied` and records a `history_events` row.
  Idempotent the same way `approve` is.

Gated by TWO independent switches, both of which must be on for a live grab to ever happen:
`ExperienceSettings.acquisition.enabled` (the master gate, `crate::settings::AcquisitionSettings`,
default **off**) AND `Config::arr_request_auto_tier_enabled` (`MUSE_ARR_REQUEST_AUTO_TIER_ENABLED`,
default **off**, the same tiered-safety flag `arr::request::classify_tier` has always used). With
either off, `POST /requests` still persists the request — it just never reaches
`acquisition::fulfill_request`.

**Sessions (MACT-01, auth-gated — `Authorization: Bearer <MUSE_API_TOKEN>`)**
- `GET /api/sessions/live` — the derived live view: `play_sessions` rows with `stopped_at IS NULL`
  passing a liveness check (the newest matching `play_events` row must be within
  `MUSE_SESSION_ACTIVE_GRACE_SECS`, default `max(3 × poll cadence, 60)` — an open-but-stale row is
  reported `state: "stale"` with its `last_event_at`, never dropped and never shown as playing).
  Envelope: `{source: "muse-derived", sessions: [...]}`. Per session: account (id + display name,
  the Muse account — never the constellation-web cookie session), item (title/year/kind/
  season+episode when applicable/`media_item_id`), same-origin poster/backdrop URLs, position/
  duration/`progress_pct` (a real percentage; the field is OMITTED entirely — not present as
  `null` — when `duration_ms` is unknown, never a fabricated `0%`), player/platform/product/device,
  `state` (`"playing"`/`"paused"`/`"stale"`), and
  the joined decision block (`video_decision`/`audio_decision`/`transcode_decision` emitted verbatim
  — `direct_play`/`direct_stream`/`transcode`/`copy` — plus `transcode_reason`, container, codecs,
  channels, resolution, bitrate). `ip_address` is never serialized. A query failure propagates as an
  error rather than a false empty list.
- `GET /api/sessions/history?limit=` — Muse's permanent historical record over stopped sessions
  (`source: "muse-history"`), same per-session projection minus the liveness fields. `limit` defaults
  to 50, capped at 500.
- `POST /api/sessions/{session_key}/terminate` (MACT-02) — stop a live stream. The only mutation in
  this group. `session_key` is resolved against the SAME live set `GET /api/sessions/live` reports —
  a caller can never name an arbitrary player; Muse decides the target. Resolution is a REFUSAL, not
  a tiebreak, when ambiguous: both `session_key` (Plex reuses it) and the display-name join Muse
  bridges through to a `plex_clients.machine_identifier` (it doesn't yet stamp the stable client id
  at ingest — tracked as spec J territory) are non-unique columns, so more than one candidate is
  `409 Conflict`, never a silent "pick the newest one". Optional body `{"reason": "..."}`, logged for
  the operator (today's `CastController::stop` has no channel to surface it to the viewer, so
  `reason_delivered` is always `false`). Relays through `CastController::stop`
  (`src/plex_control/cast.rs`) — never a second HTTP path to Plex.
  `{stopped, backend, reason_delivered}` reports what Muse can establish; `stopped` is `false` on any
  controller error, AND `false` if a follow-up timeline poll shows the player still actively
  playing/paused/buffering (accepted but didn't take) — never a fabricated `true`. A `true` on a
  `200` means "the backend accepted the stop and nothing since contradicted it", not an
  independently-confirmed end of playback in every case. `404` for an unknown or already-stopped
  `session_key` (no relay attempted). `409` for an ambiguous `session_key` or player-name match
  (refused, no relay attempted). `503` when no cast controller is configured, or when the live
  session has no resolvable Plex client target — never a `200` implying the stream stopped.
  Layered auth: Terminus's `proxy_muse` rejects a `viewer`'s `POST` with `403` before it is proxied
  here at all; this route's bearer check is Muse's independent second layer.

**Linear tuner (HDHomeRun-emulation) + streaming**
- `GET /discover.json` — HDHomeRun device descriptor.
- `GET /lineup_status.json` — static scan status.
- `GET /lineup.json` — channel lineup (GuideNumber/GuideName/URL per `mode='linear'` channel).
- `GET /muse.m3u` — M3U playlist alternative.
- `GET /xmltv.xml` — XMLTV EPG generated from `channel_programs`.
- `GET /auto/v{channel_id}` — continuous MPEG-TS stream (join-mid-stream). `501` if ffmpeg is missing, `503` if nothing is scheduled "now".

