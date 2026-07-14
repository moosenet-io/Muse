//! MUSET-03 AC5 (the load-bearing safety property): guardrails that make it
//! structurally impossible for the snapshot ingestion pipeline to be pointed
//! at a live media DB or the production muse DB.
//!
//! Every entry point in `snapshot::load` and `snapshot::mod` that opens a
//! connection to the "isolated test Postgres" MUST first pass the configured
//! DSN through [`validate_snapshot_dsn`]. This is deliberately **fail-loud,
//! not fail-quiet**: a DSN that looks even plausibly like a live host/db is
//! rejected with a descriptive [`SnapshotGuardError`], never silently
//! "allowed anyway."
//!
//! Two independent checks, both must pass:
//! 1. **Denylist** -- the DSN's host and database-name segments must not
//!    contain any of a small set of known live-shaped substrings (private
//!    fleet IP prefixes, and db/host name fragments that indicate a
//!    production or source-of-truth database: `prod`, `plex`, `tautulli`,
//!    `radarr`, `sonarr`, `prowlarr`, or a bare `muse` db name with no test
//!    marker).
//! 2. **Allow-marker** -- the database-name segment must carry an explicit
//!    snapshot/test marker (`test`, `snapshot`, or `scratch`, case
//!    insensitive) somewhere in it. A DSN that merely *fails to match* the
//!    denylist is NOT enough on its own -- it must also *affirmatively*
//!    declare itself a test/snapshot database. This is the "refuses a DSN
//!    lacking an explicit `*_test`/snapshot marker" half of AC5.
//!
//! Both checks are pure string inspection -- no network I/O, so this guard
//! runs even when no database is reachable at all (fast, always-on, unit
//! tested with zero setup).

use std::fmt;

/// A DSN the snapshot pipeline refused to use, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SnapshotGuardError {
    /// The DSN could not be parsed well enough to extract a host/db-name to
    /// check. Fails closed -- an unparseable DSN is treated as unsafe, never
    /// passed through.
    Unparseable(String),
    /// The DSN's host or database name matched a known-live substring.
    DenylistMatch {
        field: &'static str,
        matched: String,
    },
    /// The DSN's database name carries no explicit snapshot/test marker.
    NoSnapshotMarker,
}

impl fmt::Display for SnapshotGuardError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SnapshotGuardError::Unparseable(reason) => {
                write!(
                    f,
                    "snapshot DSN guard: could not parse DSN ({reason}) -- refusing to use it"
                )
            }
            SnapshotGuardError::DenylistMatch { field, matched } => write!(
                f,
                "snapshot DSN guard: {field} contains a live-system marker ({matched:?}) -- \
                 this DSN looks like it points at a live/production database, refusing to use it \
                 for the snapshot/test pipeline"
            ),
            SnapshotGuardError::NoSnapshotMarker => write!(
                f,
                "snapshot DSN guard: database name has no explicit snapshot/test marker \
                 (expected \"test\", \"snapshot\", or \"scratch\" to appear in the db name) -- \
                 refusing to use an unmarked DSN for the snapshot/test pipeline"
            ),
        }
    }
}

impl std::error::Error for SnapshotGuardError {}

/// Host/db-name substrings (case-insensitive) that mark a DSN as pointing at
/// a live fleet system rather than a disposable snapshot/test database.
/// Deliberately broad -- false positives (refusing a legitimate DSN that
/// happens to contain one of these words) are the safe failure mode; false
/// negatives are not acceptable here.
const LIVE_DENYLIST: &[&str] = &[
    // Private fleet IP ranges (RFC 1918) -- a snapshot DB should never be
    // addressed by a literal fleet-internal IP in the first place (S1), and
    // never a live-shaped one specifically.
    "192.168.",
    "10.0.",
    "172.16.",
    "172.17.",
    "172.18.",
    "172.19.",
    "172.2",
    "172.30.",
    "172.31.",
    // Known live-system identity fragments.
    "prod",
    "production",
    "plex",
    "tautulli",
    "radarr",
    "sonarr",
    "prowlarr",
    "muse_live",
    "muse-live",
];

/// Marker substrings that affirmatively declare a database name as a
/// disposable snapshot/test target.
const SNAPSHOT_MARKERS: &[&str] = &["test", "snapshot", "scratch"];

/// A DSN split into the two segments this guard cares about. Deliberately
/// minimal -- this is NOT a general-purpose connection-string parser; it
/// extracts just enough to run the two checks above, and fails closed
/// (`Unparseable`) on anything it isn't confident about rather than guessing.
struct DsnParts {
    host_segment: String,
    db_name: String,
}

fn parse_postgres_dsn(dsn: &str) -> Result<DsnParts, SnapshotGuardError> {
    // Accept both `postgres://` and `postgresql://` schemes (both valid per
    // libpq), reject anything else outright.
    let rest = dsn
        .strip_prefix("postgres://")
        .or_else(|| dsn.strip_prefix("postgresql://"))
        .ok_or_else(|| {
            SnapshotGuardError::Unparseable("missing postgres:// / postgresql:// scheme".into())
        })?;

    // rest is: [user[:password]@]host[:port][/dbname][?params]
    let after_auth = match rest.rsplit_once('@') {
        Some((_userinfo, after)) => after,
        None => rest,
    };

    let (host_and_port, path_and_query) = match after_auth.split_once('/') {
        Some((h, p)) => (h, p),
        None => (after_auth, ""),
    };

    if host_and_port.is_empty() {
        return Err(SnapshotGuardError::Unparseable("empty host segment".into()));
    }

    let db_name = path_and_query.split('?').next().unwrap_or("").to_string();

    Ok(DsnParts {
        host_segment: host_and_port.to_string(),
        db_name,
    })
}

/// Validate a DSN intended for the snapshot/test Postgres. Returns `Ok(())`
/// only if BOTH the denylist check and the allow-marker check pass.
///
/// This is the function every snapshot-pipeline connection path (AC3) must
/// call before opening a pool -- see `snapshot::load::connect_snapshot_db`.
pub fn validate_snapshot_dsn(dsn: &str) -> Result<(), SnapshotGuardError> {
    let parts = parse_postgres_dsn(dsn)?;
    let host_lower = parts.host_segment.to_lowercase();
    let db_lower = parts.db_name.to_lowercase();

    for needle in LIVE_DENYLIST {
        if host_lower.contains(needle) {
            return Err(SnapshotGuardError::DenylistMatch {
                field: "host",
                matched: (*needle).to_string(),
            });
        }
        if db_lower.contains(needle) {
            return Err(SnapshotGuardError::DenylistMatch {
                field: "database name",
                matched: (*needle).to_string(),
            });
        }
    }

    let has_marker = SNAPSHOT_MARKERS.iter().any(|m| db_lower.contains(m));
    if !has_marker {
        return Err(SnapshotGuardError::NoSnapshotMarker);
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------
    // AC5, the load-bearing safety test: NO connection string that points
    // at a live media DB or the prod muse DB may ever pass this guard.
    // -----------------------------------------------------------------

    #[test]
    fn rejects_a_literal_fleet_ip_host_even_with_a_test_dbname() {
        // Looks like it *tries* to be a test db, but the host is a
        // live-shaped private fleet IP -- must still be refused.
        let dsn = "postgres://user:pass@<internal-ip>:5432/muse_test"; // pii-test-fixture
        let err = validate_snapshot_dsn(dsn).expect_err("must reject a fleet-IP host");
        assert!(matches!(
            err,
            SnapshotGuardError::DenylistMatch { field: "host", .. }
        ));
    }

    #[test]
    fn rejects_a_dbname_containing_plex() {
        let dsn = "postgres://user:pass@snapshot-host:5432/plex_snapshot";
        let err = validate_snapshot_dsn(dsn).expect_err("must reject a plex-named db");
        assert!(matches!(
            err,
            SnapshotGuardError::DenylistMatch {
                field: "database name",
                ..
            }
        ));
    }

    #[test]
    fn rejects_a_dbname_containing_tautulli() {
        let dsn = "postgres://user:pass@snapshot-host:5432/tautulli_test";
        assert!(validate_snapshot_dsn(dsn).is_err());
    }

    #[test]
    fn rejects_the_bare_production_muse_dbname() {
        // The real prod muse DB, no test/snapshot marker at all.
        let dsn = "postgres://user:pass@muse-primary:5432/muse";
        let err = validate_snapshot_dsn(dsn).expect_err("must reject an unmarked prod-shaped db");
        assert_eq!(err, SnapshotGuardError::NoSnapshotMarker);
    }

    #[test]
    fn rejects_a_dbname_explicitly_marked_prod() {
        let dsn = "postgres://user:pass@some-host:5432/muse_production";
        let err = validate_snapshot_dsn(dsn).expect_err("must reject a production-marked db");
        assert!(matches!(
            err,
            SnapshotGuardError::DenylistMatch {
                field: "database name",
                ..
            }
        ));
    }

    #[test]
    fn rejects_a_dsn_with_no_snapshot_test_or_scratch_marker() {
        let dsn = "postgres://user:pass@some-neutral-host:5432/analytics";
        let err = validate_snapshot_dsn(dsn).expect_err("must reject an unmarked db name");
        assert_eq!(err, SnapshotGuardError::NoSnapshotMarker);
    }

    #[test]
    fn rejects_an_unparseable_dsn_fail_closed() {
        let dsn = "not-a-postgres-url-at-all";
        let err = validate_snapshot_dsn(dsn).expect_err("must fail closed on garbage input");
        assert!(matches!(err, SnapshotGuardError::Unparseable(_)));
    }

    #[test]
    fn rejects_each_rfc1918_private_range_by_host() {
        // RFC 1918 private-range literals, exercising the guard's own
        // denylist -- never a real fleet host. Each is tagged individually
        // (the pii-gate exemption is line-exact).
        for host in [
            "<internal-ip>",    // pii-test-fixture
            "<internal-ip>",  // pii-test-fixture
            "<internal-ip>",  // pii-test-fixture
            "<internal-ip>",  // pii-test-fixture
            "<internal-ip>", // pii-test-fixture
        ] {
            let dsn = format!("postgres://user:pass@{host}:5432/muse_test");
            assert!(
                validate_snapshot_dsn(&dsn).is_err(),
                "expected {host} to be denylisted"
            );
        }
    }

    // -----------------------------------------------------------------
    // Positive cases: a genuinely isolated snapshot/test DSN passes.
    // -----------------------------------------------------------------

    #[test]
    fn accepts_a_properly_marked_snapshot_dsn() {
        let dsn = "postgres://user:pass@localhost:5433/muse_snapshot_test";
        assert!(validate_snapshot_dsn(dsn).is_ok());
    }

    #[test]
    fn accepts_a_scratch_marked_dsn_with_no_credentials() {
        let dsn = "postgres://localhost:5433/muse_scratch";
        assert!(validate_snapshot_dsn(dsn).is_ok());
    }

    #[test]
    fn accepts_postgresql_scheme_variant() {
        let dsn = "postgresql://user:pass@localhost:5433/muse_test";
        assert!(validate_snapshot_dsn(dsn).is_ok());
    }

    #[test]
    fn dbname_marker_check_is_case_insensitive() {
        let dsn = "postgres://user:pass@localhost:5433/MUSE_SNAPSHOT";
        assert!(validate_snapshot_dsn(dsn).is_ok());
    }
}
