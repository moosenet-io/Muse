# Muse — Test Suite

MUSET-11 (Plane TERM #376). This document is the reference guide to *why* the muse test suite is
shaped the way it is — the four phases, the cardinal snapshots-only invariant, the TASTE two-layer
model, and the shadow-parity retirement gate. For the exact run commands, env vars, and
module-by-module file listing, see [`README.md`](../README.md#testing) — that section is kept
current as the single source of truth for "how do I run this locally"; this document
cross-references it rather than duplicating it, and focuses on *why* the suite is built this way.

## 1. The four phases, and why each gates the next

The suite was built in four phases (plus a CI phase), each one a prerequisite for the next rather
than an independent slice:

| Phase | Plane items | What it validates | Key modules |
|---|---|---|---|
| 1. Endpoints | MUSET-01, MUSET-02 | Every route mounted in `http::router` actually round-trips through a real axum `Router` (status + shape), plus a byte-for-byte golden-response baseline that catches content drift, not just status-code drift | `src/endpoint_tests.rs`, `tests/golden/` |
| 2. Snapshot harness | MUSET-03, MUSET-04 | An isolated, structurally-guarded way to load real-shaped Plex/Tautulli data into a scratch Postgres, plus a small set of reusable named fixtures with documented "correct" expectations | `src/snapshot/*` (esp. `guard.rs`), `src/fixtures/*` |
| 3. TASTE | MUSET-05, MUSET-06, MUSET-07 | That the taste/recommend pipeline is mechanically deterministic (Phase 3a), doesn't numerically regress from a known-good baseline (Phase 3b), and isn't just *coincidentally* right — its stated reasoning is defensible (Phase 3c) | `src/taste_mechanics_tests.rs`, `src/taste_golden_set.rs`, `src/taste_review/*` |
| 4. Shadow-parity | MUSET-08, MUSET-09 | Muse's own Tautulli-replacement analytics, run non-authoritatively in shadow, demonstrably match Tautulli's own numbers closely enough to justify retiring Tautulli for that function | `src/shadow/mod.rs`, `src/parity/mod.rs` |
| CI | MUSET-10 | The above four phases actually run, automatically, on every change (fast) and nightly/on-demand (full) — the standing proof gate | `.gitea/workflows/test-fast.yml`, `.gitea/workflows/test-full.yml` |

Why the ordering matters: Phase 2's snapshot harness is what Phase 3 and Phase 4 seed their data
through — TASTE mechanics, the golden set, and the shadow runner all load fixtures or replayed
snapshot rows via the exact same guarded connection path MUSET-03 established
(`snapshot::load::connect_snapshot_db`/`connect_snapshot_db_from_env`). Phase 3's mechanics floor
(3a) has to hold before the golden-set regression (3b) means anything — a numeric drift assertion
is meaningless on top of a pipeline that isn't even deterministic yet. And Phase 4's shadow runner
(MUSET-08) computes its watch analytics by folding the same snapshot `play_events` the Phase-2
harness loads, so MUSET-09's parity diff (Muse's shadow-computed watch numbers vs. Tautulli's own
numbers from the snapshot) rests on that same guarded, snapshot-only data path — not a black box.
Each phase is a foundation the next is built on, which is why the suite is described as four
*gating* phases rather than four independent test buckets.

For exact run commands per phase (`cargo test endpoint_tests`, `cargo test snapshot::`, etc.) and
the two DB env vars, see [README.md § Testing](../README.md#testing) — not duplicated here.

## 2. The cardinal invariant: snapshots everywhere, never a live read

**No test in this suite — across all four phases — ever holds a connection to a live Plex,
Tautulli, or *arr instance, or to the production `muse` database.** Every DB-touching test reads
from an isolated, loaded-from-snapshot scratch Postgres, and every test that doesn't need a
database skips DB access entirely rather than reaching for one. This is not a convention the test
authors happened to follow — it's structurally enforced.

### Why this invariant exists

A test suite that could, even accidentally (a copy-pasted DSN, an inherited env var, a
misconfigured CI runner), open a connection to a live fleet database is a standing risk: a
`DELETE`/`UPDATE` in a buggy test, a runaway migration, or simply read contention against a
production Postgres serving real traffic. The "zero blast radius" design goal is that **no
possible test failure, typo, or CI misconfiguration can touch a live system** — the worst case is
a test failing to connect to *anything* and skipping cleanly, never a test connecting to the wrong
thing. A hardcoded live-DB connection string inside a test is treated as a spec violation on par
with a hardcoded secret, and is negative-tested against (see below) — not just discouraged in
review.

### The enforcement mechanism: MUSET-03's fail-closed DSN guard

[`snapshot::guard::validate_snapshot_dsn`] (`src/snapshot/guard.rs`) is the choke point every
**snapshot-family** DB-gated test passes through before connecting — it's called from
`snapshot::load::connect_snapshot_db`/`connect_snapshot_db_from_env`, which every `*_pool_or_skip`
helper in the snapshot-family modules (`snapshot`, `fixtures::loader`, `taste_mechanics_tests`,
`taste_golden_set`, `shadow`, `parity`) calls first. It fails **loud and closed**, never
"quiet and permissive": a DSN that even plausibly looks live-shaped is rejected outright with a
descriptive `SnapshotGuardError`, rather than allowed through with a warning. Four checks, all
must pass:

1. **No connection-target-overriding query params.** Only a small allowlist (`sslmode`,
   `connect_timeout`, `application_name`) is permitted — params like `host`/`dbname`/`port` that
   `sqlx`/libpq would honor are rejected outright, so a DSN whose *URL* host/db look benign can't
   silently connect somewhere else via a query param.
2. **Host is not a live-fleet host.** Any RFC-1918 private IPv4 (`10/8`, `172.16/12`,
   `192.168/16`) or IPv6 unique-local address is rejected, and non-IP hostnames are checked
   against a live-system name denylist (`plex`/`tautulli`/`radarr`/`sonarr`/`prowlarr`/`prod`/
   `muse_live`).
3. **DB name must carry an explicit `test`/`snapshot`/`scratch` marker**, and no live-system
   marker. Merely *failing* the denylist isn't enough — the DSN must *affirmatively* declare
   itself a test/snapshot database.
4. **Load host must be loopback** (`localhost` / `127.0.0.0/8` / `::1`) — the primary, airtight
   gate. This closes the whole class of host-resolution tricks (numeric shorthands, DNS games)
   that enumerating denylist entries could never fully cover.

`snapshot::guard::tests` are pure string/IP unit tests with no DB dependency — they always run,
every `cargo test`, including in the fast CI job. The guard's own negative tests are what makes
"a live-DB connection string in a test is a spec violation" an enforced, tested property rather
than a documentation claim.

**One honest caveat about scope.** The guard covers the snapshot-family DB access described above,
which is every test built from MUSET-03 onward. The *older* Phase-1 endpoint tests
(`endpoint_tests::db_gated::test_pool_or_skip`) — along with `integration_tests.rs`, `http::ops`,
and the channel tests — predate the snapshot guard and connect **directly** via
`PgPoolOptions::connect` after reading `MUSE_TEST_DATABASE_URL`, *not* routed through
`validate_snapshot_dsn`. For those paths the loopback-only/marker invariant is a convention (and
the documented example DSNs honor it), not a code-enforced gate. What keeps them safe in practice
is a different structural property: none of those tests ever configures a real
Plex/Prowlarr/TMDb/Tautulli/arr/Ollama/Chord client, so they can't reach a live *upstream* even by
accident — the guard's airtight enforcement applies specifically to the snapshot-family Postgres
connections, and the doc doesn't claim more than that.

## 3. TASTE's two layers, and why both are needed

Phase 3 validates the taste/recommend pipeline through two structurally different layers, plus
the mechanics floor underneath them:

- **3a — Mechanics** (`src/taste_mechanics_tests.rs`, MUSET-05): the deterministic floor. Asserts
  that embedding/pgvector operations, context bucketing (weekend/weekday × time-of-day), and
  scoring/ranking all produce identical output on identical input. This is *not* a test of taste
  *quality* — it's the mechanical contract everything above depends on.
- **3b — Golden-set regression** (`src/taste_golden_set.rs`, MUSET-06): a small set of
  hand-computed, known-good `(history → expected taste-lean)` values, graded against a tuned
  tolerance. Automatable and cheap — it runs on every full-suite invocation and fails the moment a
  change moves a computed taste-lean away from its documented correct value. This catches
  **"it got worse"** — quantitative degradation.
- **3c — Adversarial reasoning review** (`src/taste_review/*`, MUSET-07): builds an interrogable
  `ReasoningTrace` from an already-computed recommendation (`trace::build_reasoning_trace`), sends
  it to an adversarial critique panel via `panel::ReasoningPanel` asking "is the STATED reason for
  this recommendation actually defensible, not just whether it's a good pick," and files a
  `TasteQualityFinding` (via `sink::FindingSink`) on consensus-spurious, escalates to a human on
  no-consensus, and produces nothing on consensus-sound. This catches **"right answer, wrong
  reason"** — a recommendation that is a perfectly fine pick but for a spurious reason (e.g. one
  old rating dominating the affinity math because nothing since has diluted it), which no
  numeric-tolerance check like 3b could ever detect, because the *output number* looks fine.

### Why both layers are necessary

3b is cheap, automatable, and catches regressions a human would never manually re-review for
every change — but it is fundamentally a check on a **number**, and a taste computation can drift
onto a bad *reason* while producing a number that still falls inside tolerance (the correlation
that happens to still hold, this time for the wrong underlying cause). 3c is the only layer that
actually interrogates the *reasoning*, but it needs an LLM-backed critique panel and is not the
kind of thing you'd want gating every commit at automation cost — hence why (see §5) it runs fully
in-process against mocks (`panel::MockReasoningPanel`, `sink::MockFindingSink`) in the FAST test
phase rather than requiring a live panel endpoint. Neither layer subsumes the other: 3b would miss
"right answer, wrong reason," and 3c alone would be too expensive/non-deterministic to run as a
tight degradation-floor gate on every change. Together they cover both axes.

## 4. The shadow-parity retirement gate

Phase 4 is the evidence pipeline for the strangler-fig plan to eventually retire Tautulli, one
function at a time:

- **Shadow runner** (`src/shadow/mod.rs`, MUSET-08): runs Muse's own Tautulli-replacement
  analytics (folding `play_events` via `tracker::reconstruct::fold_events`, the same pure fold the
  live `tracker` path uses) against snapshot data, in **shadow**. It is non-authoritative by
  construction: it only ever reads (`SELECT`s, no `INSERT`/`UPDATE`/`UPSERT` anywhere in the
  module), there is no "promote to authoritative" switch anywhere in the crate, and its
  `ShadowResult` is a plain value the caller may log, compare, or discard.
- **Parity diff / retirement-readiness evidence** (`src/parity/mod.rs`, MUSET-09): diffs the
  shadow runner's output against the snapshot's own Tautulli-origin numbers (parsed back out of
  each snapshot `play_events` row's `raw` JSON — `percent_complete`, `watched_status`, `duration`
  — not raw Plex telemetry, so this is a genuine Muse-computed-number vs.
  Tautulli-computed-number diff) field-by-field, and assembles a `RetirementReadinessReport`: the
  evidence an operator would read before deciding whether to flip Tautulli off for a given
  function.

### The evidence authorizes; it never executes

Nothing in either module can retire Tautulli itself. `parity::mod` deliberately has no `PgPool`
parameter on any function — the diff is a pure, in-memory transform over two already-fetched
inputs — and no `INSERT`/`UPDATE` of any kind anywhere in the module. There is no "retired" /
"authoritative" flag anywhere in the crate for this code to flip. `RetirementReadinessReport`'s
only behavior beyond holding data is `headline()`, which formats a human-readable summary that
*always* states retirement is not authorized and remains a human decision — this is a load-bearing
assertion, directly tested by
`parity::tests::retirement_is_never_auto_triggered_even_at_100_percent_parity`. The report is
evidence an operator (<operator>) reads and acts on manually; the suite guarantees the code path has no
mechanism to act on it automatically, at any parity percentage.

## 5. How to run each phase

Full, current run commands and env vars live in [README.md § Testing](../README.md#testing) — this
section only summarizes the shape so this doc doesn't drift out of sync with the authoritative
copy:

- **No database needed at all**: `cargo test` — every DB-touching test across every phase skips
  cleanly (never fails) when its env var is unset, per each module's own `*_pool_or_skip` helper.
  Note the gating is **not uniform**: the snapshot-family tests (Phase 2, plus 3a/3b's
  pgvector-backed cases, and Phase 4's shadow/parity) read `MUSE_SNAPSHOT_DATABASE_URL`
  (preferred), falling back to `MUSE_TEST_DATABASE_URL`; the older Phase-1
  endpoint/`integration_tests`/`http::ops`/channel DB-gated tests read **only**
  `MUSE_TEST_DATABASE_URL`. With neither var set, all of them skip. This DB-free run includes 3a's
  pure-math determinism tests, all of 3c (`taste_review`, fully mocked, network- and DB-free), and
  the DB-independent golden responses in Phase 1.
- **DB-gated cases** (Phase 1's happy-path/non-mutation goldens, Phase 2's snapshot round-trip +
  fixtures, 3a/3b's pgvector-backed tests, Phase 4's shadow/parity seeding tests): point a local
  **scratch** Postgres 17 with `vector` + `pg_trgm`, whose database name carries an explicit
  `test`/`snapshot`/`scratch` marker (for the snapshot-family tests this is enforced by the §2
  guard — a real/shared host is rejected outright; for the direct-connect endpoint/integration
  tests it's the documented convention). Which var(s) you set determines what runs, per the two
  resolvers: the snapshot-family resolver (`snapshot::load::snapshot_database_url_from_env`)
  *prefers* `MUSE_SNAPSHOT_DATABASE_URL` but *falls back to* `MUSE_TEST_DATABASE_URL`, while the
  direct endpoint/integration/http/channel helpers read **only** `MUSE_TEST_DATABASE_URL`. So:
    - **only `MUSE_TEST_DATABASE_URL`** → runs the *entire* DB-gated suite (snapshot-family falls
      back to it; the direct tests read it) — this one var alone is sufficient;
    - **only `MUSE_SNAPSHOT_DATABASE_URL`** → runs *only* the snapshot family; the direct tests
      have no var set and skip;
    - **both** → runs everything, snapshot-family on the snapshot var and direct tests on the test
      var. MUSET-10's `test-full.yml` sets both — not because one alone wouldn't cover the suite
      (`MUSE_TEST_DATABASE_URL` would), but for explicit separation, and so the two families can be
      pointed at different DBs if desired.
  Then run `cargo test -- --test-threads=1` (single-threaded because `config.rs`'s env-reading
  tests use `serial_test` against process-global env).
- **Per-phase / per-module runs** (e.g. `cargo test taste_golden_set`, `cargo test shadow::`) work
  the same way — DB-gated cases within that module skip or run depending on whether the env vars
  above are set.
- **CI** (`.gitea/workflows/`, MUSET-10) automates exactly this split: `test-fast.yml` runs on
  every push/PR with the DB vars deliberately left unset (rustfmt, clippy, then the DB-free
  subset of every phase); `test-full.yml` runs on demand and nightly, brings up a throwaway
  `pgvector/pgvector:pg17` service container local to the CI runner, and points both DB vars at
  it — a loopback host with a `_test`-marked db name, so it satisfies the §2 guard the same way a
  human's local scratch Postgres does. See README.md's CI subsection for the full breakdown; not
  duplicated here.

## 6. Real bugs this suite has caught

Two examples of the suite finding actual defects, kept here as evidence the invariants above are
doing real work rather than just process for its own sake:

- **MUSE-ROUTE-01 (fixed).** Building the Phase 1 endpoint harness (`src/endpoint_tests.rs`)
  surfaced that muse depends on axum 0.7, whose path-parameter syntax is `:id`, but its routes
  were written with the axum 0.8 brace syntax (`{id}`) — under 0.7 a `{id}` segment is a *literal*
  path segment, not a capture, so every `{param}` route (`/proactive/{id}/ack`,
  `/channels/{id}/compose`, `/api/channels/{id}/lineup`, `/art/{kind}/{id}`) never reached its
  real handler and fell through to the 404/501 fallback. This has since been fixed — all route
  strings were migrated to axum-0.7 `:id` syntax — and the tests that caught it are now active,
  regular regression guards (they always asserted the *correct* contract, so fixing the routes
  just flipped them green rather than requiring new test code).
- **MUSE-DEDUP-01 (open, tracked, not fixed by the suite itself).** Phase 3a's mechanics tests
  found that `curation::candidates::dedup_candidates` collects its de-duplicated result via
  `HashMap<i64, Candidate>::into_values().collect()` — and `std::collections::HashMap`'s iteration
  order depends on a fresh per-call `RandomState` seed, so two calls to `dedup_candidates` on
  byte-identical, tied-score input are not guaranteed to return survivors in the same order. The
  practical consequence: two otherwise-identical `/recommend` calls can return the same
  tied-score items in a different order. Per the MUSET-05 build brief's guidance ("if you find a
  real nondeterminism bug in the taste code, do not fix the app here — document it as an
  `#[ignore]`d regression guard"), this is captured as a documented, `#[ignore]`d test —
  `taste_mechanics_tests::pure_math::dedup_then_rank_output_order_is_not_guaranteed_deterministic_for_tied_scores`
  — with the finding, why it's `#[ignore]`d rather than a normally-failing test (asserting
  inequality unconditionally would itself be flaky), and a suggested fix (swap the `HashMap` for
  a `BTreeMap`, or add an explicit tiebreak key) recorded in its doc comment. This remains open
  and tracked as a follow-up item, not resolved by MUSET-05 itself.
