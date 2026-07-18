## Testing

See [`docs/TESTING.md`](docs/TESTING.md) for the *why*: the four gating phases, the
snapshots-everywhere invariant and its DSN-guard enforcement, the TASTE two-layer model, and the
shadow-parity retirement gate. This section stays the authoritative *how-to-run* reference (env
vars, per-phase commands, CI) — the two are cross-referenced, not duplicated.

The suite runs green with **no live database** — every DB-touching integration test is gated on
`MUSE_TEST_DATABASE_URL` and skips cleanly (does not fail) when it's unset. Point it at a scratch
Postgres 17 (`vector` + `pg_trgm`) database to actually exercise migrations and the repo layer:

```
MUSE_TEST_DATABASE_URL=postgres://user:pass@192.0.2.30/muse_test cargo test
```

Env-var-reading config tests use `serial_test` (process-global env), so run the suite
single-threaded (`cargo test -- --test-threads=1`) to avoid cross-test env races. HTTP-client tests
use `httpmock` and need no live upstream. The integration tests live *inside* the binary crate
(there is no `[lib]` target) so they can reach `crate::repo`/`crate::models`.

### Endpoint contract/integration harness (`src/endpoint_tests.rs`)

A router-level test suite exercises every route mounted in `http::router` through a real axum
`Router` via `tower::ServiceExt::oneshot` (an actual HTTP request/response round trip, not a bare
handler call) — `/health`, `/query/resolve`, `/query/similar`, `/recommend` +
`/recommend/on_deck` + `/recommend/gaps`, `/proactive/pending` + `/proactive/{id}/ack`,
`/api/channels` + `/api/channels/{id}/lineup`, `/art/{kind}/{id}`, `/channels/{id}/compose`,
`/ops/*`, and the `/ingest` \| `/query` \| `/proactive` fallback-501 contract. Each route gets a
contract test (status + response shape), an error-path test, and — gated on
`MUSE_TEST_DATABASE_URL` like the rest of the suite — a happy-path test against real seeded rows.
The gated `db_gated::read_endpoints_never_mutate_the_database` test snapshots every watched
table's row count, exercises every Phase-0 read endpoint back to back, and asserts the counts are
unchanged — the executable form of this service's read-only-until-Phase-1 posture. The two
intentionally-mutating Phase-0 endpoints (`/channels/{id}/compose`, `/proactive/{id}/ack`) get the
opposite assertion in their own tests: that they write exactly the one row they claim to. No test
in this module ever configures a real Plex/Prowlarr/TMDb/Tautulli/arr/Ollama/Chord client, so it
can never reach a live upstream even by accident — same posture as every other test in this crate.
Run with `cargo test endpoint_tests`.

### Golden-response regression baseline (MUSET-02)

On top of the contract/error-path/happy-path coverage above, the `golden`/`golden_support`
modules at the bottom of `src/endpoint_tests.rs` (plus a nested `golden` module inside
`db_gated` for the endpoints whose representative response needs a seeded row) add a
**golden-response baseline**: the exact, canonicalized JSON — or raw bytes, for the artwork
proxy's placeholder PNG — a representative request currently returns, committed under
[`tests/golden/`](tests/golden) and diffed on every `cargo test` run. This catches drift in
response *content*, not just status code. Non-deterministic fields (timestamps, generated ids,
per-run-unique fixture names) are redacted to a stable `"<redacted>"` placeholder via JSON
Pointer before comparison, rather than skipped — the rest of the shape is still asserted
exactly.

- **Regenerate a baseline** after an intentionally changed response:
  `MUSE_UPDATE_GOLDEN=1 cargo test endpoint_tests`. Never hand-edit a `tests/golden/*.json`
  file — golden tests are otherwise strictly read-only/comparison-only.
- **DB-independent goldens** (`health`, `/query/resolve`'s empty-query short-circuit, the
  `/ops/ingest/*` unconfigured 503s, the `/ingest`\|`/query`\|`/proactive` 501 fallback, the two
  `/proactive/{id}/ack` and `/channels/{id}/compose` 400 error bodies, and the artwork proxy's
  placeholder PNG) run in the default `cargo test` invocation, no `MUSE_TEST_DATABASE_URL`
  needed.
- **DB-gated goldens** (the `/recommend` family for a signal-empty account, an `/api/channels`
  + `/api/channels/{id}/lineup` shape for a real seeded channel, and a
  `/proactive/{id}/ack`-dismissed persisted+returned shape) skip cleanly without
  `MUSE_TEST_DATABASE_URL`, same posture as the rest of the suite.
- **Proof the mechanism actually catches drift**:
  `golden::golden_diff_mechanism_detects_a_deliberately_altered_response` exercises the real
  comparison function against a deliberately mutated response (in a self-contained scratch
  file, never a committed golden) and asserts it panics.

### Snapshot ingestion, fixtures, TASTE, reasoning review, shadow, parity (MUSET-03..09)

Everything below reuses the SAME "skip cleanly, never fail" gate as the sections above —
`MUSE_SNAPSHOT_DATABASE_URL` (preferred) or `MUSE_TEST_DATABASE_URL` (fallback), resolved by
`snapshot::load::snapshot_database_url_from_env`. Every DB-gated test in every module below runs
`snapshot::load::connect_snapshot_db`/`connect_snapshot_db_from_env` first, which validates the
DSN through `snapshot::guard::validate_snapshot_dsn` (MUSET-03 AC5) BEFORE connecting: the DSN
must be loopback-hosted and carry an explicit `test`/`snapshot`/`scratch` marker in the database
name, or the guard refuses it outright. This is structurally airtight against ever touching a
live media DB or the production `muse` DB, even from a misconfigured env var. Each helper
(`*_pool_or_skip`) also runs this crate's own migrations against the connected pool, so no
separate migration step is needed — set the env var, run `cargo test`.

- **Snapshot ingestion pipeline** (`src/snapshot/`, MUSET-03): `snapshot::db_gated` round-trips a
  loaded snapshot (normalize → load → provenance) against the isolated Postgres. Guard unit tests
  (`snapshot::guard::tests`) are pure string/IP checks with no DB and always run.
- **Real-data fixtures** (`src/fixtures/`, MUSET-04): reusable seeded-account fixtures
  (`heavy_rewatcher`, `multi_genre`, `cold_start_empty`, `sparse_metadata`) + their
  `ProfileExpectation`s, gated via `fixtures::loader::tests::fixture_pool_or_skip`.
- **TASTE mechanics** (`src/taste_mechanics_tests.rs`, MUSET-05): the deterministic floor
  (embedding/pgvector ops, context-bucketing, scoring/ranking consistency). DB-gated cases go
  through `mechanics_pool_or_skip`; the pure-math determinism assertions run unconditionally.
- **TASTE golden-set regression** (`src/taste_golden_set.rs`, MUSET-06): hand-computed
  known-good taste-lean values a regression must not drift away from. Grader-sanity,
  tolerance-math, and negative-perturbation tests run with no DB; `golden_pool_or_skip`-gated
  cases need the scratch DB.
- **Adversarial reasoning review** (`src/taste_review/`, MUSET-07): trace + panel-critique +
  finding-sink tests all run against `panel::MockReasoningPanel`/`sink::MockFindingSink` — fully
  in-process, no network, no DB, no Terminus client (Muse has no live Terminus integration yet)
  — so this module's suite runs in the FAST phase, not the full/snapshot phase.
- **Shadow runner** (`src/shadow/`, MUSET-08): read-only Tautulli-replacement analytics computed
  from snapshot `play_events`. DB-gated (`shadow::tests::db_gated`) via `snapshot_pool_or_skip`;
  asserts the runner performs zero writes.
- **Parity diff + retirement evidence** (`src/parity/`, MUSET-09): diffs shadow output against
  snapshot Tautulli-origin numbers. The core diff/report-building tests are pure in-memory data
  transforms with no DB; the seeding/overlap tests (`parity::tests::db_gated`, if present) use
  the same `snapshot_pool_or_skip` idiom as `shadow`.

Run any single phase locally the same way as the sections above, e.g.:

```
cargo test snapshot::
cargo test taste_mechanics_tests
cargo test taste_golden_set
cargo test taste_review
cargo test shadow::
cargo test parity::
```

To exercise the DB-gated cases in any of them, point a **local scratch** Postgres 17 with
`vector` + `pg_trgm` available, whose database name carries an explicit
`test`/`snapshot`/`scratch` marker (per the MUSET-03 guard above). Set **both** env vars to the
same DSN: the snapshot-family tests prefer `MUSE_SNAPSHOT_DATABASE_URL` (falling back to
`MUSE_TEST_DATABASE_URL`), while `endpoint_tests.rs`'s `db_gated` module, `integration_tests.rs`,
and `http::ops` read `MUSE_TEST_DATABASE_URL` directly — so exporting both is what exercises
*every* DB-gated test in one pass rather than only the subset keyed off one var:

```
export MUSE_SNAPSHOT_DATABASE_URL=postgres://user:pass@localhost/muse_dev_test
export MUSE_TEST_DATABASE_URL=postgres://user:pass@localhost/muse_dev_test
cargo test -- --test-threads=1
```

Never point either var at a real/shared host. For the **snapshot-family** DB access (everything
routed through `snapshot::load::connect_snapshot_db`, i.e. the snapshot/fixtures/taste/shadow/
parity tests) this is code-enforced, not just a convention — `snapshot::guard::validate_snapshot_dsn`
rejects any non-loopback host (and any db name lacking a `test`/`snapshot`/`scratch` marker)
outright. The older `MUSE_TEST_DATABASE_URL`-direct paths (`endpoint_tests::db_gated`,
`integration_tests.rs`, `http::ops`, channel tests) connect via `PgPoolOptions::connect` and are
**not** routed through that guard, so for those the loopback-only rule is a documented convention;
what structurally keeps them safe is that none of them ever configures a real upstream client. See
[`docs/TESTING.md`](docs/TESTING.md#2-the-cardinal-invariant-snapshots-everywhere-never-a-live-read)
for the full scope of what the guard does and does not enforce.

### CI (`.gitea/workflows/`, MUSET-10)

Two Gitea Actions workflows wire the phases above into CI as the standing proof gate, splitting
on cost the same way the sections above split on DB-gating:

- **`test-fast.yml`** — runs on **every push and every pull request**, any branch.
  `rustfmt --check`, `clippy --all-targets -D warnings`, then `cargo test -- --test-threads=1`
  with `MUSE_SNAPSHOT_DATABASE_URL`/`MUSE_TEST_DATABASE_URL` left **unset** — every DB-gated test
  above skips cleanly (per its own `*_pool_or_skip`), so this job needs no service container and
  covers the endpoint/golden/mechanics/golden-set/reasoning-review fast tests on every change,
  with no live dependency at all.
- **`test-full.yml`** — runs **on demand** (`workflow_dispatch`) and **nightly**
  (`schedule: cron`). Brings up a `pgvector/pgvector:pg17` service container local to the CI
  runner (`localhost:5432`, throwaway `POSTGRES_USER`/`POSTGRES_PASSWORD`/`POSTGRES_DB` values
  scoped to the job's lifetime only — not a fleet secret, never a literal prod DSN), points
  `MUSE_SNAPSHOT_DATABASE_URL`/`MUSE_TEST_DATABASE_URL` at
  `postgres://muse_ci:muse_ci_scratch@localhost:5432/muse_ci_test`, and runs the full suite. That
  DSN is loopback-hosted with an explicit `_test` marker in the db name, so it independently
  satisfies MUSET-03's `snapshot::guard::validate_snapshot_dsn` guard — the same structural
  guarantee a human running the suite locally gets, not a CI-only exception.

Neither workflow embeds a real hostname, credential, or fleet DSN (S1/S7) — the full job's
Postgres exists only inside that job's runner for that job's duration.
