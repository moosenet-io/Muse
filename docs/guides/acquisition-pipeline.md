# The acquisition pipeline

Run a media request end-to-end: request → targeted Prowlarr search → tiered safety
classification → release-decision scoring → qBittorrent grab (S119 Sprint 1). Expected
outcome: a `media_requests` row that either fulfills via a grab or parks as
needs-review, with every step visible in the lifecycle endpoints.

## Prerequisites

- A running `muse` with `MUSE_DATABASE_URL` configured.
- **Prowlarr**: `PROWLARR_URL` + `PROWLARR_API_KEY` (search candidates come from here).
- **qBittorrent**: `MUSE_QBIT_URL` + `MUSE_QBIT_USER` + `MUSE_QBIT_PASS` — all three, or
  the download client is `None` and requests persist but never fulfill (logged at boot
  as `qbit_configured=false`).
- **Auth**: the request-lifecycle routes are protected — set `MUSE_API_TOKEN` and send
  it as a bearer token (without it, protected routes answer 503 by design).

## Steps

1. **Submit a request**: `POST /requests` (see `src/http/requests.rs`
   `create_request_handler`). The handler drives `acquisition::fulfill_request`, which:
   1. runs a bounded on-demand search (`prowlarr::search::search_releases`) — budgeted
      by `MUSE_PROWLARR_SEARCH_MAX_PER_HOUR` on the client's shared rate limiter;
   2. classifies the ask (`arr::request::classify_tier`) into `AutoApprovable` /
      `NeedsReview` / `Blocked` — auto-approval is off unless
      `MUSE_ARR_REQUEST_AUTO_TIER_ENABLED=true`;
   3. scores candidates (`decision::decide_release`) against the quality profile and
      custom-format table;
   4. grabs the winner via `download::qbit::QbitClient` and records the lifecycle in
      `repo::acquisition`.
2. **Inspect**: `GET /requests` lists requests with their state.
3. **Review-gated requests**: approve or deny explicitly —
   `POST /requests/{id}/approve` / `POST /requests/{id}/deny`.
4. **Monitored ("wanted") items** are driven automatically on the maintenance cadence by
   `acquisition::worker::run_wanted_pass`, bounded by
   `MUSE_WANTED_MAX_GRABS_PER_PASS` (default 5), `MUSE_WANTED_MAX_SEARCHES_PER_PASS`
   (default 20), and the per-item `MUSE_WANTED_SEARCH_COOLDOWN_SECS` (default 6h) — a
   freshly-populated wanted list can never become an unbounded burst.

## Troubleshooting

- **Request persists but nothing downloads**: check boot logs for
  `qbit_configured=false` (incomplete `MUSE_QBIT_*` trio) — that degradation is by
  design, not an error.
- **Everything lands in NeedsReview**: expected default posture; auto-tier requires the
  explicit opt-in flag *and* a confident real-time availability signal.
- Deeper detail: [acquisition orchestrator + request lifecycle](../reference/acquisition-orchestrator-request-lifecycle-musem-05.md),
  [release-decision engine](../reference/release-decision-engine.md),
  [monitored wanted worker](../reference/monitored-wanted-acquisition-worker-musem-06.md).
