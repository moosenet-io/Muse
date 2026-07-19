# The snapshot / shadow / parity pipeline

Produce read-only snapshots of the live stack, run Muse's shadow Tautulli-replacement
analytics against them, and generate the retirement-readiness parity report — the
evidence an operator reads before retiring Tautulli's watch-data function (MUSET-03/08/09).

All three steps are **operator CLI subcommands of the `muse` binary**, gated behind argv
checks that return before any server bootstrap: they never run at service startup and
are never invoked by the test suite. Everything connects only through the guarded
snapshot path (`snapshot::load` → `snapshot::guard`), which refuses any DSN that looks
like a live/production database.

## Prerequisites

- An **isolated** Postgres (with pgvector) for snapshots — configured as
  `MUSE_SNAPSHOT_DATABASE_URL` (or `MUSE_TEST_DATABASE_URL`). The guard validates this
  DSN fail-closed; a production-shaped DSN is rejected outright.
- Read access to the source artifacts you want to snapshot.
- `MUSE_SNAPSHOT_OUTPUT_DIR` — a writable directory for acquired artifacts.
- For the `pg_dump` source: the `pg_dump` binary on `$PATH`.

## Step 1 — Acquire snapshots

Configure any subset of the four sources, then run:

```sh
muse snapshot-acquire
```

- `MUSE_SNAPSHOT_PLEX_SQLITE_PATH` → copies `plex-library.sqlite`
- `MUSE_SNAPSHOT_TAUTULLI_SQLITE_PATH` → copies `tautulli-history.sqlite`
- `MUSE_SNAPSHOT_ARR_SQLITE_PATH` → copies `arr.sqlite`
- `MUSE_SNAPSHOT_SOURCE_POSTGRES_URL` → runs a parameterized `pg_dump` to
  `muse-postgres.dump` (this *source* DSN passes a lighter not-prod-marked check —
  reading a live source is what acquisition is)

Each artifact is SHA-256 checksummed; when a snapshot DB is configured, a provenance row
(source identity + vintage timestamp + checksum) is recorded per artifact so later runs
are reproducible against a known data vintage. With no sources configured the command
warns and exits.

## Step 2 — Shadow analytics run

```sh
muse shadow-run
```

Loads/migrates the snapshot DB and runs the shadow analytics pass (`shadow::run`), which
reuses the tracker's **real** session-fold code (`tracker::reconstruct::fold_events`)
over the snapshot's play events — computing session counts and watch stats exactly as
production would. Read-only end to end: it never writes results back and never touches
`MUSE_DATABASE_URL` at all. The summary (`session_keys_considered`, `sessions_folded`,
`stats_produced`) is logged.

## Step 3 — Parity report

```sh
muse parity-report
```

Runs the shadow pass, fetches the snapshot's Tautulli-origin history rows, diffs the two
(`parity::build_report`), and prints a headline plus the full JSON
`RetirementReadinessReport`. This subcommand **cannot retire anything** — there is no
flag, env var, or code path that authorizes retirement; it prints evidence and exits.

## Troubleshooting

- **"neither … is set" error**: `shadow-run`/`parity-report` need
  `MUSE_SNAPSHOT_DATABASE_URL` or `MUSE_TEST_DATABASE_URL`.
- **Guard rejection**: the DSN failed `snapshot::guard::validate_snapshot_dsn` — point
  it at the isolated snapshot DB, not anything production-shaped. The guard is
  fail-closed by design; don't work around it.
- Reference: [snapshot subsystem page](../reference/snapshot.md).
