# snapshot

The snapshot-ingestion pipeline (123 KG nodes, MUSET-03): read-only, out-of-band
snapshots of source systems (Plex library SQLite, Tautulli history SQLite, an *arr
SQLite, and a `pg_dump` of the Muse Postgres) loaded into an **isolated** test Postgres,
so Muse's own logic can run against real-shaped data without the test suite ever holding
a live connection to a production system.

Five pieces and one hard safety rule:

- **`acquisition`** — operator-run artifact production (SQLite file copy, parameterized
  `pg_dump`), reachable *only* via the `muse snapshot-acquire` subcommand, which returns
  before any service bootstrap.
- **`normalize`** — maps snapshot-shaped records into Muse's real schema, so query/
  handler code runs against the loaded snapshot unmodified.
- **`load`** — the ONE connection path to the isolated snapshot/test Postgres; every
  pool it hands out is guard-checked first.
- **`provenance`** — source identity + snapshot vintage + SHA-256 checksum recorded per
  load, so a test run is reproducible against a known data vintage.
- **`guard`** — the load-bearing rule: **no DSN used by this pipeline may point at a
  live media DB or the production Muse DB.** `load::connect_snapshot_db` enforces it
  unconditionally via `guard::validate_snapshot_dsn` before opening any connection.

## Key types and functions

| Symbol | Kind | File | What it does |
|---|---|---|---|
| `snapshot::guard::validate_snapshot_dsn` | fn | `src/snapshot/guard.rs` | The fail-closed DSN gate — the subsystem's top-ranked symbol; the MUSET-03 review hardened it through 7 adversarial rounds (9 real bypasses found and closed; allowlist, not denylist) |
| `snapshot::guard::parse_and_canonicalize_dsn` | fn | `src/snapshot/guard.rs` | Canonicalizes a DSN before validation so encoding tricks can't slip past the gate |
| `snapshot::guard::host_is_loopback` / `host_is_private_ip` | fn | `src/snapshot/guard.rs` | Host-class checks the guard composes |
| `snapshot::load::connect_snapshot_db` | fn | `src/snapshot/load.rs` | The one guarded connection path to the isolated DB |
| `snapshot::load::snapshot_database_url_from_env` | fn | `src/snapshot/load.rs` | Resolves `MUSE_SNAPSHOT_DATABASE_URL` / `MUSE_TEST_DATABASE_URL` |
| `snapshot::load::migrate_snapshot_db` | fn | `src/snapshot/load.rs` | Runs the real migrations against the isolated DB |
| `snapshot::provenance::checksum_bytes` | fn | `src/snapshot/provenance.rs` | SHA-256 provenance checksum (stable, content-sensitive) |

## How it connects

Entry points are the three operator CLI subcommands in `src/main.rs`
(`snapshot-acquire`, and — consuming the loaded data — `shadow-run` and
`parity-report`), all gated behind argv checks that return before server bootstrap and
never invoked by the test suite. `shadow::run` reuses the tracker's real fold analytics
over the snapshot DB; `parity::build_report` diffs those results against
Tautulli-origin rows (via `repo::play_event::list_tautulli_snapshot_events`) into a
retirement-readiness report. Live-DB tests across the crate connect through the same
guarded path.

## Configuration

- `MUSE_SNAPSHOT_DATABASE_URL` / `MUSE_TEST_DATABASE_URL` — the isolated DB (guard-checked).
- `MUSE_SNAPSHOT_OUTPUT_DIR` — where acquired artifacts are written.
- `MUSE_SNAPSHOT_PLEX_SQLITE_PATH`, `MUSE_SNAPSHOT_TAUTULLI_SQLITE_PATH`,
  `MUSE_SNAPSHOT_ARR_SQLITE_PATH`, `MUSE_SNAPSHOT_SOURCE_POSTGRES_URL` — the sources
  (the source Postgres DSN additionally passes `guard::validate_not_prod_source`).

## Notes and gaps

- The security lesson from this module's review — fail-closed allowlists beat denylists
  for DSN parsing — is recorded in the S119 build reports and is why the guard's shape
  looks the way it does.
- `shadow`/`parity` are structurally incapable of retiring anything: no flag or code
  path writes back or flips a switch; they compute and print.
- Usage walkthrough: [snapshot pipeline guide](../guides/snapshot-pipeline.md).
