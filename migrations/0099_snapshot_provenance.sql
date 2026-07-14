-- MUSET-03: provenance ledger for snapshot-ingestion test fixtures.
--
-- The snapshot ingestion pipeline (src/snapshot/) loads READ-ONLY, out-of-band
-- copies of source data (Plex library SQLite, Tautulli history SQLite, *arr
-- SQLite, and a `pg_dump` of the muse Postgres) into an ISOLATED test
-- database. This table is that isolated test database's own bookkeeping: one
-- row per snapshot load, recording *what* was loaded, *when* the source
-- snapshot was taken, and a checksum -- so a test run against the loaded
-- snapshot is reproducible against a known data vintage (AC4).
--
-- This table only ever lives in the isolated snapshot/test database
-- (`MUSE_SNAPSHOT_DATABASE_URL` / `MUSE_TEST_DATABASE_URL`), which shares the
-- same migration set as the production schema (per MUSET-01's `test_pool_or_skip`
-- idiom -- `sqlx::migrate!("./migrations")` runs against the scratch DB too),
-- so it is scoped and named to be unambiguous even if a migration set is ever
-- accidentally pointed at a real deployment.
CREATE TABLE snapshot_provenance (
    id BIGSERIAL PRIMARY KEY,
    -- Logical identity of the source this snapshot was taken from, e.g.
    -- "plex-library-sqlite", "tautulli-history-sqlite", "arr-sqlite",
    -- "muse-postgres". Never a hostname/IP/credential -- see snapshot::guard.
    source_identity text NOT NULL,
    -- One of the SnapshotSourceKind variants (src/snapshot/provenance.rs),
    -- stored as text for forward-compatible schema evolution.
    source_kind text NOT NULL,
    -- When the SOURCE snapshot was captured (out-of-band, by the operator's
    -- acquisition step) -- the data "vintage" a test run is reproducible
    -- against. Distinct from `loaded_at` below (when it entered *this* DB).
    snapshot_taken_at timestamptz NOT NULL,
    -- SHA-256 of the snapshot artifact (the copied SQLite file, or the
    -- pg_dump archive) at acquisition time, hex-encoded.
    checksum_sha256 text NOT NULL,
    -- Free-form note: row counts, source release/version, operator notes.
    -- Never contains a hostname/IP/credential -- see snapshot::guard.
    notes text,
    loaded_at timestamptz NOT NULL DEFAULT now()
);

CREATE INDEX idx_snapshot_provenance_source_kind ON snapshot_provenance (source_kind);
