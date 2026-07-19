# premiere

Premiere events + engagement tiers (103 KG nodes, MUSEX-15): three opt-in,
config-tunable capabilities layered on already-shipped pieces — inventing no second
consent model, rationale generator, or request-safety gate:

- **`schedule`** — scheduled premiere events: a title + time + RSVP + a grounded "why
  this pairing" rationale, announced via `discord`'s `RichEmbed` shape. Only
  opted-in/allowlisted friends (`discord::identity::TrustedFriends`) can be invited or
  RSVP.
- **`discussion`** — async, per-title "book-club" discussion threads, persisted via
  `repo::premiere_discussion`, posting gated by the same allowlist/opt-in check RSVP
  uses.
- **`engagement`** — engagement-tiered request budgets computed from real watch-through
  and household-loved signals, which **modulate (never bypass)** `arr::request`'s tiered
  safety gate — the budget is strictly a brake, never an accelerator.

Premieres live *beside* `watch_together`, not inside it: a lobby answers "who's on the
couch right now"; a premiere is the scheduled, announced flavor with a multi-day
lifecycle (announce → RSVPs → event → discussion).

## Key types and functions

| Symbol | Kind | File | What it does |
|---|---|---|---|
| `premiere::schedule::schedule_premiere` | fn | `src/premiere/schedule.rs` | Creates a premiere event, inviting only opted-in requested friends (tested) |
| `premiere::schedule::PremiereEvent::invited_count` | fn | `src/premiere/schedule.rs` | Invitee accounting on the event |
| `premiere::http::run_premiere_rsvp` | fn | `src/premiere/http.rs` | The RSVP route logic (inert when watch-together is disabled — tested) |
| `premiere::http::to_rsvp_response` | fn | `src/premiere/http.rs` | RSVP outcome → HTTP response shape |
| `premiere::engagement::EngagementCounts::watch_through_rate` | fn | `src/premiere/engagement.rs` | A friend's watch-through leg of the composite score |
| `premiere::engagement::EngagementCounts::household_love_rate` | fn | `src/premiere/engagement.rs` | The household-loved leg (ratings at/above the loved threshold) |
| `premiere::engagement::compute_tier` | fn | `src/premiere/engagement.rs` | Weighted composite → `EngagementTier` (Starter/Trusted/Curator) → request budget |

## How it connects

Announcements render through `discord::client::RichEmbed` and respect
`discord::identity` consent end-to-end; discussion rows persist through
`repo::premiere_discussion`; engagement reads real watch/ratings data (the same
`watch_stats`/`ratings` tables the taste model uses) and its budgets feed into
`arr::request::classify_tier`'s surrounding request flow. HTTP routes are mounted by
`http::router` in the protected group.

## Configuration

All GUI/config-tunable, none secret-shaped:

- `MUSE_PREMIERE_ANNOUNCE_CADENCE_SECS` — intended announce-sweep cadence (default 1 week).
- `MUSE_PREMIERE_ENGAGEMENT_WATCH_THROUGH_WEIGHT` / `MUSE_PREMIERE_ENGAGEMENT_HOUSEHOLD_LOVE_WEIGHT`
  — composite-score weights (defaults 0.5/0.5).
- `MUSE_PREMIERE_ENGAGEMENT_TRUSTED_THRESHOLD` / `MUSE_PREMIERE_ENGAGEMENT_CURATOR_THRESHOLD`
  — tier cutoffs (defaults 0.4/0.7).
- `MUSE_PREMIERE_LOVED_RATING_THRESHOLD` — what counts as "loved" (default 7.0 on the
  0–10 ratings scale).
- `MUSE_PREMIERE_STARTER_BUDGET` / `MUSE_PREMIERE_TRUSTED_BUDGET` / `MUSE_PREMIERE_CURATOR_BUDGET`
  — per-tier request budgets (defaults 1/3/6).

## Notes and gaps

- **No scheduled announce driver is wired yet** — the cadence tunable exists but a
  periodic worker is an explicit follow-up item (the config field documents this).
- The engagement weights/thresholds are deliberately unopinionated defaults; no
  production corpus has tuned them.
- Not covered here: the live lobby flow — see `src/watch_together/` and
  [EXPERIENCE_LAYER.md](../EXPERIENCE_LAYER.md).
