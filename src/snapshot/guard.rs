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
//! Three independent checks, ALL must pass, in this order:
//! 1. **No connection-target override** -- the DSN must carry no query param
//!    outside a tiny allowlist of inert params (`sslmode`, `connect_timeout`,
//!    `application_name`). Params like `host`/`hostaddr`/`dbname`/`port`/
//!    `user`/`password` are honored by sqlx/libpq and would let a DSN whose
//!    *URL* host/db look benign actually connect somewhere else
//!    (`?hostaddr=<fleet-ip>`, `?dbname=muse`). Rejecting them GUARANTEES the
//!    effective target equals the URL components checks 2-3 then validate.
//! 2. **Host is not a live-fleet host** -- the host is parsed as a real
//!    `std::net::IpAddr`; any IPv4 RFC-1918 private address (`10/8`,
//!    `172.16/12`, `192.168/16` -- exactly `Ipv4Addr::is_private()`) or IPv6
//!    unique-local (`fc00::/7`) is rejected (the isolated test Postgres must
//!    live on localhost, not the shared fleet network). Non-IP hostnames are
//!    additionally checked against a small live-system name denylist
//!    (`plex`/`tautulli`/`radarr`/`sonarr`/`prowlarr`/`prod`/`muse_live`).
//! 3. **DB name is a marked snapshot/test DB** -- the database-name segment
//!    must carry no live-system marker AND must carry an explicit
//!    `test`/`snapshot`/`scratch` marker. A DSN that merely *fails* the
//!    denylist is NOT enough -- it must *affirmatively* declare itself a
//!    test/snapshot DB (the "refuses a DSN lacking an explicit
//!    `*_test`/snapshot marker" half of AC5).
//!
//! All three checks are pure string/IP inspection -- no network I/O, so this
//! guard runs even when no database is reachable at all (fast, always-on,
//! unit tested with zero setup).

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
    /// The DSN carries a connection query param that could OVERRIDE the
    /// connection target (host/hostaddr/dbname/port/user/password, or any
    /// other non-allowlisted param) -- so the effective target may not equal
    /// the URL components the rest of the guard validated. Rejected outright.
    DisallowedQueryParam { param: String },
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
            SnapshotGuardError::DisallowedQueryParam { param } => write!(
                f,
                "snapshot DSN guard: DSN carries a connection query param ({param:?}) that could \
                 override the connection target -- only inert params (sslmode, connect_timeout, \
                 application_name) are permitted, refusing this DSN so the effective target \
                 cannot differ from the validated host/database"
            ),
        }
    }
}

impl std::error::Error for SnapshotGuardError {}

/// Host/db-name substrings (case-insensitive) that mark a DSN as pointing at
/// a live fleet system rather than a disposable snapshot/test database. These
/// are HOSTNAME identity fragments only -- IP-range detection is handled
/// separately and precisely by [`host_is_private_ip`] (parsing the host as a
/// real `IpAddr`), NOT by fragile IP substring matching. Deliberately broad
/// for the hostname markers -- false positives (refusing a legitimate DSN
/// that happens to contain one of these words) are the safe failure mode;
/// false negatives are not acceptable here.
const LIVE_DENYLIST: &[&str] = &[
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

/// The ONLY connection query params permitted on a snapshot/test DSN. Every
/// one of these is inert with respect to the connection TARGET (host / port /
/// database / credentials) -- so a DSN carrying only these has an effective
/// target equal to its URL components, which is exactly what this guard
/// validates. Anything NOT on this allowlist (notably `host`, `hostaddr`,
/// `dbname`, `port`, `user`, `password`) can OVERRIDE the URL target under
/// sqlx/libpq and is rejected outright -- that closes the query-param
/// override bypass. Case-insensitive.
const ALLOWED_QUERY_PARAMS: &[&str] = &["sslmode", "connect_timeout", "application_name"];

/// A DSN split into the pieces this guard validates. Deliberately minimal --
/// this is NOT a general-purpose connection-string parser; it extracts just
/// enough to run the checks, and fails closed (`Unparseable`) on anything it
/// isn't confident about rather than guessing.
struct DsnParts {
    /// The bare host, port stripped and IPv6 brackets removed (e.g.
    /// `"127.0.0.1"`, `"localhost"`, `"::1"`).
    host: String,
    db_name: String,
    /// The query-param KEYS present on the DSN (lowercased), for the
    /// override-allowlist check.
    query_param_keys: Vec<String>,
}

/// Strip a trailing `:port` from a host authority, correctly handling
/// bracketed IPv6 (`[::1]:5432` -> `::1`, `[fc00::1]` -> `fc00::1`).
fn strip_port(host_and_port: &str) -> &str {
    if let Some(after_bracket) = host_and_port.strip_prefix('[') {
        // Bracketed IPv6: take everything up to the closing `]`.
        return after_bracket.split(']').next().unwrap_or(after_bracket);
    }
    // Bare host[:port]. A colon only means "port" for a hostname/IPv4; a raw
    // (unbracketed) IPv6 literal is not valid authority syntax here, so a
    // single trailing `:digits` is a port. Use rsplit_once so an accidental
    // extra colon still trims only the last segment.
    match host_and_port.rsplit_once(':') {
        Some((h, maybe_port))
            if !maybe_port.is_empty() && maybe_port.chars().all(|c| c.is_ascii_digit()) =>
        {
            h
        }
        _ => host_and_port,
    }
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

    let (db_part, query_part) = match path_and_query.split_once('?') {
        Some((db, q)) => (db, q),
        None => (path_and_query, ""),
    };

    let query_param_keys = if query_part.is_empty() {
        Vec::new()
    } else {
        query_part
            .split('&')
            .filter(|kv| !kv.is_empty())
            .map(|kv| kv.split('=').next().unwrap_or("").to_lowercase())
            .collect()
    };

    Ok(DsnParts {
        host: strip_port(host_and_port).to_string(),
        db_name: db_part.to_string(),
        query_param_keys,
    })
}

/// True if `host` parses as an IP address that belongs to a
/// non-globally-routable "private" fleet range: IPv4 RFC-1918 (the `10/8`,
/// `172.16/12`, and `192.168/16` blocks -- exactly what
/// `Ipv4Addr::is_private()` covers) or IPv6 unique-local (`fc00::/7`). A
/// hostname that is not an IP literal returns `false` here (it's handled by
/// the hostname denylist instead). The isolated test Postgres is required to
/// live on loopback, so rejecting ALL RFC-1918/ULA hosts is correct, not
/// over-broad.
fn host_is_private_ip(host: &str) -> bool {
    match host.parse::<std::net::IpAddr>() {
        Ok(std::net::IpAddr::V4(v4)) => v4.is_private(),
        // IPv6 unique-local addresses are fc00::/7 -- i.e. the top 7 bits are
        // 1111110. `is_unique_local()` is unstable on stable Rust, so test
        // the prefix directly on the first octet.
        Ok(std::net::IpAddr::V6(v6)) => (v6.octets()[0] & 0xfe) == 0xfc,
        Err(_) => false,
    }
}

/// Validate a DSN intended for the snapshot/test Postgres. Returns `Ok(())`
/// only if ALL of these hold, checked BEFORE any connection is ever opened:
/// 1. No connection-target-overriding query param is present (allowlist) --
///    guarantees the effective target equals the URL components this guard
///    then validates (closes the `?hostaddr=`/`?dbname=` bypass).
/// 2. The host is neither an RFC-1918/ULA private IP (`host_is_private_ip`)
///    nor a live-system hostname marker (`LIVE_DENYLIST`).
/// 3. The database name carries no live-system marker AND does carry an
///    explicit snapshot/test marker (`SNAPSHOT_MARKERS`).
///
/// This is the function every snapshot-pipeline connection path (AC3) must
/// call before opening a pool -- see `snapshot::load::connect_snapshot_db`.
///
/// NOTE on scope: this guard protects the LOAD / test-DB path -- the DSN the
/// pipeline connects to and that tests read from. It does NOT govern the
/// acquisition step's SOURCE DSN (`MUSE_SNAPSHOT_SOURCE_POSTGRES_URL`), which
/// is BY DESIGN allowed to read a live source DB: acquisition is an explicit,
/// out-of-band operator action against a source (`snapshot::acquisition`),
/// not something tests or normal startup ever do. See
/// `validate_not_prod_source` for the lighter check applied to that path.
pub fn validate_snapshot_dsn(dsn: &str) -> Result<(), SnapshotGuardError> {
    let parts = parse_postgres_dsn(dsn)?;

    // (1) Reject any query param that could override the connection target,
    // BEFORE trusting the URL's own host/db components.
    for key in &parts.query_param_keys {
        if !ALLOWED_QUERY_PARAMS.contains(&key.as_str()) {
            return Err(SnapshotGuardError::DisallowedQueryParam { param: key.clone() });
        }
    }

    let host_lower = parts.host.to_lowercase();
    let db_lower = parts.db_name.to_lowercase();

    // (2a) Host as a real IP: reject any RFC-1918 / IPv6-ULA private address.
    if host_is_private_ip(&host_lower) {
        return Err(SnapshotGuardError::DenylistMatch {
            field: "host",
            matched: parts.host.clone(),
        });
    }

    // (2b/3) Hostname + db-name identity markers.
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

/// A lighter check for the acquisition SOURCE DSN
/// (`MUSE_SNAPSHOT_SOURCE_POSTGRES_URL`). Reading a live source DB is the
/// intended purpose of acquisition, so this does NOT apply the full
/// snapshot-DB guard -- it only asserts the source is not itself an
/// unmarked/production-marked muse database being mistaken for a source
/// (a defensive nicety, not the blocking load-path guard). Best-effort:
/// returns `Ok(())` for any DSN that isn't obviously the prod muse DB.
pub fn validate_not_prod_source(dsn: &str) -> Result<(), SnapshotGuardError> {
    let parts = parse_postgres_dsn(dsn)?;
    let db_lower = parts.db_name.to_lowercase();
    // A bare `muse` (or explicitly production-marked) db as the SOURCE is
    // almost certainly a mistake -- acquisition reads Plex/Tautulli/*arr or a
    // deliberately-named muse source, not the live prod db by that bare name.
    if db_lower == "muse" || db_lower.contains("prod") || db_lower.contains("muse_live") {
        return Err(SnapshotGuardError::DenylistMatch {
            field: "source database name",
            matched: parts.db_name,
        });
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
        // Full RFC 1918 coverage via real IpAddr parsing (NOT substring
        // matching) -- these previously slipped through a `"10.0."`-only
        // substring denylist. Each is tagged individually (the pii-gate
        // exemption is line-exact).
        for host in [
            "<internal-ip>",      // pii-test-fixture  (<internal-ip>/8, not 10.0.x)
            "<internal-ip>",   // pii-test-fixture
            "<internal-ip>",    // pii-test-fixture
            "<internal-ip>",    // pii-test-fixture  (<internal-ip>/12 low edge)
            "<internal-ip>",    // pii-test-fixture
            "<internal-ip>",  // pii-test-fixture  (<internal-ip>/12 high edge)
            "<internal-ip>", // pii-test-fixture
            "<internal-ip>",   // pii-test-fixture
        ] {
            let dsn = format!("postgres://user:pass@{host}:5432/muse_test");
            let err = validate_snapshot_dsn(&dsn).expect_err(&format!(
                "expected {host} to be rejected as a private-IP host"
            ));
            assert!(
                matches!(err, SnapshotGuardError::DenylistMatch { field: "host", .. }),
                "{host} should reject as a host denylist match, got {err:?}"
            );
        }
    }

    #[test]
    fn accepts_a_non_private_public_ip_host_is_not_auto_rejected_as_private() {
        // A public IP is not RFC-1918; it passes the private-IP check (it
        // may still be rejected for other reasons, but not by
        // host_is_private_ip). 8.8.8.8 with a marked test db is allowed.
        assert!(!host_is_private_ip("8.8.8.8"));
        let dsn = "postgres://user:pass@8.8.8.8:5432/muse_test";
        assert!(validate_snapshot_dsn(dsn).is_ok());
    }

    #[test]
    fn rejects_ipv6_unique_local_host() {
        // fc00::/7 (unique-local) -- both an fc.. and an fd.. prefix, with
        // brackets as they'd appear in a real DSN authority.
        for host in ["[fc00::1]", "[fd12:3456:789a::1]"] {
            let dsn = format!("postgres://user:pass@{host}:5432/muse_test");
            let err = validate_snapshot_dsn(&dsn)
                .expect_err(&format!("expected {host} (IPv6 ULA) to be rejected"));
            assert!(
                matches!(err, SnapshotGuardError::DenylistMatch { field: "host", .. }),
                "{host} should reject as a host denylist match, got {err:?}"
            );
        }
    }

    #[test]
    fn accepts_ipv6_loopback_with_marked_db() {
        // ::1 loopback is NOT unique-local -- the isolated test DB on
        // loopback must still be allowed.
        assert!(!host_is_private_ip("::1"));
        let dsn = "postgres://user:pass@[::1]:5432/muse_test";
        assert!(validate_snapshot_dsn(dsn).is_ok());
    }

    // -----------------------------------------------------------------
    // FIX 1: query-parameter connection-target override bypass. These DSNs
    // pass the URL-component checks but sqlx/libpq would connect elsewhere
    // -- they MUST now be rejected outright, with no connection attempt.
    // -----------------------------------------------------------------

    #[test]
    fn rejects_hostaddr_query_param_override_bypass() {
        // URL host=localhost/db=muse_test would PASS the component checks,
        // but a hostaddr override would make sqlx connect to a fleet host.
        let dsn = "postgres://localhost/muse_test?hostaddr=<internal-ip>"; // pii-test-fixture
        let err = validate_snapshot_dsn(dsn).expect_err("hostaddr override must be rejected");
        assert!(
            matches!(err, SnapshotGuardError::DisallowedQueryParam { ref param } if param == "hostaddr"),
            "got {err:?}"
        );
    }

    #[test]
    fn rejects_host_query_param_override_bypass() {
        let dsn = "postgres://localhost/muse_test?host=plex-db";
        let err = validate_snapshot_dsn(dsn).expect_err("host override must be rejected");
        assert!(matches!(
            err,
            SnapshotGuardError::DisallowedQueryParam { .. }
        ));
    }

    #[test]
    fn rejects_dbname_query_param_override_bypass() {
        // Passes on the URL's `muse_test`, but ?dbname=muse redirects to the
        // prod DB under libpq.
        let dsn = "postgres://localhost/muse_test?dbname=muse";
        let err = validate_snapshot_dsn(dsn).expect_err("dbname override must be rejected");
        assert!(
            matches!(err, SnapshotGuardError::DisallowedQueryParam { ref param } if param == "dbname"),
            "got {err:?}"
        );
    }

    #[test]
    fn rejects_port_user_password_query_param_overrides() {
        for param in ["port=5432", "user=postgres", "password=secret"] {
            let dsn = format!("postgres://localhost/muse_test?{param}");
            let err = validate_snapshot_dsn(&dsn)
                .expect_err(&format!("{param} override must be rejected"));
            assert!(
                matches!(err, SnapshotGuardError::DisallowedQueryParam { .. }),
                "{param} -> {err:?}"
            );
        }
    }

    #[test]
    fn rejects_any_unknown_query_param_via_allowlist() {
        // Even a benign-looking unknown param is rejected -- allowlist, not
        // denylist, so a future libpq target-affecting param can't sneak in.
        let dsn = "postgres://localhost/muse_test?options=-csearch_path%3Dpublic";
        assert!(matches!(
            validate_snapshot_dsn(dsn),
            Err(SnapshotGuardError::DisallowedQueryParam { .. })
        ));
    }

    #[test]
    fn allows_inert_query_params_on_a_marked_db() {
        // The allowlisted params don't change the target, so a marked test
        // DSN carrying only them still passes.
        let dsn = "postgres://localhost:5433/muse_test?sslmode=disable&connect_timeout=5&application_name=muse_snapshot";
        assert!(validate_snapshot_dsn(dsn).is_ok());
    }

    // -----------------------------------------------------------------
    // The lighter acquisition-SOURCE check (not the load-path guard).
    // -----------------------------------------------------------------

    #[test]
    fn validate_not_prod_source_rejects_the_bare_prod_muse_source() {
        assert!(validate_not_prod_source("postgres://source-host:5432/muse").is_err());
        assert!(validate_not_prod_source("postgres://source-host:5432/muse_production").is_err());
    }

    #[test]
    fn validate_not_prod_source_allows_a_real_source_db() {
        // A Plex/Tautulli/*arr-shaped source, or a deliberately-named muse
        // source snapshot, is fine to read from.
        assert!(validate_not_prod_source("postgres://source-host:5432/tautulli").is_ok());
        assert!(validate_not_prod_source("postgres://source-host:5432/muse_source_export").is_ok());
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
