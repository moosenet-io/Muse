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
    /// The DSN authority is a comma-separated MULTI-HOST / failover list.
    /// sqlx/libpq would connect to one of several hosts, only one of which
    /// this guard could validate -- rejected fail-closed.
    MultiHost { authority: String },
    /// The DSN's host did not cleanly parse as either a single hostname or a
    /// single IP literal (e.g. a mangled authority with a residual `:`).
    /// Rejected fail-closed rather than allowed to fall through.
    UnparseableHost { host: String },
    /// The DSN carries no explicit, non-empty path database name. libpq/
    /// `pg_dump` would resolve the target via connection defaults (commonly
    /// the username, or `PGDATABASE`) -- an unvalidated effective target.
    /// Rejected fail-closed so the effective db can never differ from what
    /// the guard checked.
    MissingDatabaseName,
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
            SnapshotGuardError::MultiHost { authority } => write!(
                f,
                "snapshot DSN guard: DSN authority ({authority:?}) is a comma-separated \
                 multi-host/failover list -- a single isolated test/snapshot DB (or acquisition \
                 source) must name exactly one host, refusing it so sqlx cannot connect to an \
                 unvalidated failover host"
            ),
            SnapshotGuardError::UnparseableHost { host } => write!(
                f,
                "snapshot DSN guard: host ({host:?}) did not parse as a single hostname or IP \
                 address -- refusing fail-closed rather than treating an unparseable host as safe"
            ),
            SnapshotGuardError::MissingDatabaseName => write!(
                f,
                "snapshot DSN guard: DSN carries no explicit database name -- libpq/pg_dump would \
                 default the target (to the username or PGDATABASE), an unvalidated effective DB, \
                 so an explicit non-empty database name is required"
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

/// A DSN reduced to its canonical connection-target components by
/// [`parse_and_canonicalize_dsn`]. Because that parser is FAIL-CLOSED
/// (rejecting multi-host authorities, disallowed query params, and any host
/// that doesn't cleanly parse as a single hostname or IP), each field here is
/// guaranteed to be the ACTUAL effective connection target -- so the two
/// callers can apply their own policy to these components without worrying
/// about a hidden override or a second failover host.
struct CanonicalDsn {
    /// Exactly one host: a bare hostname (alphanumerics/dots/hyphens) or a
    /// single IP literal (IPv6 brackets already removed). Never empty, never
    /// contains a residual `:`/`,`/`@`.
    host: String,
    db_name: String,
}

/// True if `host` is a syntactically-clean bare hostname: non-empty,
/// alphanumerics/dots/hyphens/underscores only (no residual `:`, `,`, `@`,
/// `[`, `]`, or whitespace). An IP literal is validated separately by an
/// `IpAddr` parse; this covers the non-IP case so a mangled authority can't
/// fall through to "allowed."
fn is_clean_hostname(host: &str) -> bool {
    !host.is_empty()
        && host
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
}

/// The ONE shared, fail-closed DSN parser used by BOTH the load path
/// (`validate_snapshot_dsn`) and the source path (`validate_not_prod_source`).
///
/// It rejects, BEFORE returning any components:
/// - a non-`postgres(ql)://` scheme,
/// - a MULTI-HOST authority (any comma in the host:port section) -- sqlx/
///   libpq failover DSNs have no legitimate use for a single isolated
///   test/snapshot DB or a single acquisition source, and letting one through
///   means sqlx could connect to a host this guard never validated,
/// - any query param outside [`ALLOWED_QUERY_PARAMS`] (host/hostaddr/dbname/
///   port/user/password/etc. all override the connection target),
/// - a host that parses as neither a single clean hostname NOR a single
///   `IpAddr` (fail-closed: a mangled/unparseable host is rejected, never
///   passed through as "not private, so allowed").
///
/// On success the returned host/db_name ARE the effective connection target,
/// so each caller only has to apply its own host/db policy.
fn parse_and_canonicalize_dsn(dsn: &str) -> Result<CanonicalDsn, SnapshotGuardError> {
    // Accept both `postgres://` and `postgresql://` schemes (both valid per
    // libpq), reject anything else outright.
    let rest = dsn
        .strip_prefix("postgres://")
        .or_else(|| dsn.strip_prefix("postgresql://"))
        .ok_or_else(|| {
            SnapshotGuardError::Unparseable("missing postgres:// / postgresql:// scheme".into())
        })?;

    // rest is: [user[:password]@]host[:port][,host[:port]...][/dbname][?params]
    let after_auth = match rest.rsplit_once('@') {
        Some((_userinfo, after)) => after,
        None => rest,
    };

    let (authority, path_and_query) = match after_auth.split_once('/') {
        Some((h, p)) => (h, p),
        None => (after_auth, ""),
    };

    if authority.is_empty() {
        return Err(SnapshotGuardError::Unparseable("empty host segment".into()));
    }

    // FAIL-CLOSED: reject any comma-separated multi-host / failover authority.
    if authority.contains(',') {
        return Err(SnapshotGuardError::MultiHost {
            authority: authority.to_string(),
        });
    }

    let (db_part, query_part) = match path_and_query.split_once('?') {
        Some((db, q)) => (db, q),
        None => (path_and_query, ""),
    };

    // Reject any non-allowlisted query param (target-overriding), for BOTH
    // paths, before trusting the URL's own host/db components.
    if !query_part.is_empty() {
        for kv in query_part.split('&').filter(|kv| !kv.is_empty()) {
            let key = kv.split('=').next().unwrap_or("").to_lowercase();
            if !ALLOWED_QUERY_PARAMS.contains(&key.as_str()) {
                return Err(SnapshotGuardError::DisallowedQueryParam { param: key });
            }
        }
    }

    // Extract exactly one host, port stripped, IPv6 brackets removed.
    let host = strip_single_host(authority)?;

    // FAIL-CLOSED host validation: must be EITHER a clean hostname OR a
    // single parseable IpAddr. Anything else (a residual colon from a
    // mangled authority, stray punctuation, empty) is rejected, never
    // allowed to fall through.
    if !is_clean_hostname(&host) && host.parse::<std::net::IpAddr>().is_err() {
        return Err(SnapshotGuardError::UnparseableHost { host });
    }

    Ok(CanonicalDsn {
        host,
        db_name: db_part.to_string(),
    })
}

/// Strip exactly one `:port` from a single-host authority, correctly handling
/// bracketed IPv6 (`[::1]:5432` -> `::1`, `[fc00::1]` -> `fc00::1`). The
/// authority is already known comma-free (multi-host rejected upstream), so
/// this only has to deal with one host.
fn strip_single_host(authority: &str) -> Result<String, SnapshotGuardError> {
    if let Some(after_bracket) = authority.strip_prefix('[') {
        // Bracketed IPv6: everything up to the closing `]`.
        let (inner, _after) =
            after_bracket
                .split_once(']')
                .ok_or_else(|| SnapshotGuardError::UnparseableHost {
                    host: authority.to_string(),
                })?;
        return Ok(inner.to_string());
    }
    // Bare host[:port]. An UNBRACKETED host containing a colon is only valid
    // as host:port -- split off exactly the trailing `:digits` port; if the
    // remaining host still contains a colon it's a raw (illegal here) IPv6 or
    // a mangled authority and the fail-closed host check downstream rejects
    // it.
    let host = match authority.rsplit_once(':') {
        Some((h, maybe_port))
            if !maybe_port.is_empty() && maybe_port.chars().all(|c| c.is_ascii_digit()) =>
        {
            h
        }
        _ => authority,
    };
    Ok(host.to_string())
}

/// True if `host` parses as an IP address that belongs to a
/// non-globally-routable "private" fleet range: IPv4 RFC-1918 (the `10/8`,
/// `172.16/12`, and `192.168/16` blocks -- exactly what
/// `Ipv4Addr::is_private()` covers) or IPv6 unique-local (`fc00::/7`). A
/// hostname that is not an IP literal returns `false` here (it's handled by
/// the hostname denylist instead). The isolated test Postgres is required to
/// live on loopback, so rejecting ALL RFC-1918/ULA hosts is correct, not
/// over-broad.
///
/// IPv4-in-IPv6 forms are handled explicitly: an `::ffff:a.b.c.d`
/// (v4-mapped) or `::a.b.c.d` (deprecated v4-compatible) host connects to
/// the embedded IPv4 address, so the RFC-1918 check MUST run on that
/// embedded address -- otherwise a `::ffff:<rfc1918-addr>` host would slip
/// past a v6-only ULA check and reach the fleet address. Loopback forms
/// (`::1`, `::`) map into the `0.0.0.x` block, which is not `is_private()`,
/// so loopback is correctly NOT rejected here.
fn host_is_private_ip(host: &str) -> bool {
    match host.parse::<std::net::IpAddr>() {
        Ok(std::net::IpAddr::V4(v4)) => v4.is_private(),
        Ok(std::net::IpAddr::V6(v6)) => {
            // Reject a private RFC-1918 address embedded in a v4-mapped
            // (`::ffff:a.b.c.d`) or v4-compatible (`::a.b.c.d`) IPv6 host.
            if let Some(v4) = v6.to_ipv4_mapped().or_else(|| v6.to_ipv4()) {
                if v4.is_private() {
                    return true;
                }
            }
            // IPv6 unique-local addresses are fc00::/7 -- i.e. the top 7
            // bits are 1111110. `is_unique_local()` is unstable on stable
            // Rust, so test the prefix directly on the first octet.
            (v6.octets()[0] & 0xfe) == 0xfc
        }
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
    // Shared fail-closed parse: rejects multi-host, target-overriding query
    // params, and any unparseable host BEFORE we trust host/db_name below.
    let canonical = parse_and_canonicalize_dsn(dsn)?;

    let host_lower = canonical.host.to_lowercase();
    let db_lower = canonical.db_name.to_lowercase();

    // Belt-and-suspenders: reject an empty/omitted db name (the marker check
    // below already requires a non-empty, marked db name, but make the
    // no-libpq-defaulting rule explicit and shared with the source path).
    if db_lower.is_empty() {
        return Err(SnapshotGuardError::MissingDatabaseName);
    }

    // LOAD policy (a): host must NOT be a private RFC-1918 / IPv6-ULA fleet
    // address -- the isolated test DB lives on loopback.
    if host_is_private_ip(&host_lower) {
        return Err(SnapshotGuardError::DenylistMatch {
            field: "host",
            matched: canonical.host.clone(),
        });
    }

    // LOAD policy (b): hostname + db-name identity markers.
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

    // LOAD policy (c): db name MUST affirmatively be a test/snapshot target.
    let has_marker = SNAPSHOT_MARKERS.iter().any(|m| db_lower.contains(m));
    if !has_marker {
        return Err(SnapshotGuardError::NoSnapshotMarker);
    }

    Ok(())
}

/// The acquisition SOURCE DSN check (`MUSE_SNAPSHOT_SOURCE_POSTGRES_URL`).
///
/// Reading a live source DB is the INTENDED purpose of acquisition and the
/// source legitimately lives on the private fleet network, so this does NOT
/// apply the load-path host policy (no private-IP / marker requirement). But
/// it uses the SAME fail-closed [`parse_and_canonicalize_dsn`] -- so it
/// inherits the multi-host rejection AND the query-param override rejection
/// (closing `…/muse_source_export?dbname=muse` reaching the prod DB via
/// libpq). Its policy on the canonical components:
/// - the DSN MUST carry an explicit, non-empty path database name. An omitted
///   db name would let libpq/`pg_dump` default the target -- commonly to the
///   USERNAME (or `PGDATABASE`) -- so e.g. `postgres://muse@source-host`
///   (empty path db) would effectively read the `muse` DB AFTER passing a
///   bare-name check. Rejecting empty removes all reliance on libpq defaults.
/// - that explicit db name must not be the bare/production `muse` database
///   (almost certainly a misconfiguration -- acquisition reads Plex/Tautulli/
///   *arr or a deliberately-named muse source, not the live prod db).
///
/// The source legitimately lives on the private fleet network, so a
/// private-IP host is NOT rejected here (unlike the load path).
pub fn validate_not_prod_source(dsn: &str) -> Result<(), SnapshotGuardError> {
    let canonical = parse_and_canonicalize_dsn(dsn)?;
    let db_lower = canonical.db_name.to_lowercase();
    // FAIL-CLOSED: require an explicit db name -- never let libpq default it
    // (which would resolve to the username, e.g. `muse`, reaching prod).
    if db_lower.is_empty() {
        return Err(SnapshotGuardError::MissingDatabaseName);
    }
    if db_lower == "muse" || db_lower.contains("prod") || db_lower.contains("muse_live") {
        return Err(SnapshotGuardError::DenylistMatch {
            field: "source database name",
            matched: canonical.db_name,
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

    // -----------------------------------------------------------------
    // FAIL-CLOSED shared-parser gaps: multi-host / failover authority and
    // unparseable hosts must be rejected before any policy check, on BOTH
    // paths, so sqlx/libpq can never connect to a host the guard didn't see.
    // -----------------------------------------------------------------

    #[test]
    fn rejects_multi_host_failover_dsn_on_load_path() {
        // sqlx/libpq would connect to the FIRST host (a private fleet
        // address); the whole authority must be rejected fail-closed.
        let dsn = "postgres://<internal-ip>:5432,localhost:5432/muse_test"; // pii-test-fixture
        let err = validate_snapshot_dsn(dsn).expect_err("multi-host authority must be rejected");
        assert!(
            matches!(err, SnapshotGuardError::MultiHost { .. }),
            "got {err:?}"
        );
    }

    #[test]
    fn rejects_bare_comma_authority() {
        let dsn = "postgres://localhost,otherhost/muse_test";
        assert!(matches!(
            validate_snapshot_dsn(dsn),
            Err(SnapshotGuardError::MultiHost { .. })
        ));
    }

    #[test]
    fn rejects_a_host_that_wont_parse_fail_closed() {
        // A mangled authority with a residual colon (not host:port, not a
        // valid IP) is neither a clean hostname nor a single IpAddr, so it
        // must be rejected fail-closed, not passed through as "not private".
        let dsn = "postgres://foo:bar:baz/muse_test";
        let err = validate_snapshot_dsn(dsn).expect_err("mangled host must be rejected");
        assert!(
            matches!(err, SnapshotGuardError::UnparseableHost { .. }),
            "got {err:?}"
        );
    }

    #[test]
    fn load_path_rejects_an_empty_database_name() {
        // Belt-and-suspenders: an omitted/empty db name is rejected as
        // MissingDatabaseName (before the marker check would also catch it),
        // so libpq can never default the load target.
        for dsn in [
            "postgres://localhost:5433",
            "postgres://localhost:5433/",
            "postgres://localhost:5433/?sslmode=disable",
        ] {
            let err =
                validate_snapshot_dsn(dsn).expect_err(&format!("{dsn} (no db) must be rejected"));
            assert!(
                matches!(err, SnapshotGuardError::MissingDatabaseName),
                "{dsn} -> {err:?}"
            );
        }
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
        // ::1 loopback is NOT unique-local and does NOT map to a private
        // IPv4 -- the isolated test DB on loopback must still be allowed,
        // even after the v4-mapped/compatible embedding check was added.
        assert!(!host_is_private_ip("::1"));
        let dsn = "postgres://user:pass@[::1]:5432/muse_test";
        assert!(validate_snapshot_dsn(dsn).is_ok());
    }

    #[test]
    fn rejects_ipv4_mapped_ipv6_private_host() {
        // An RFC-1918 address embedded in a v4-mapped (`::ffff:a.b.c.d`) or
        // v4-compatible (`::a.b.c.d`) IPv6 host connects to that private
        // IPv4 -- so it must be rejected on the embedded address, not slip
        // past the v6-only ULA check. Bracketed as in a real DSN authority.
        for host in [
            "[::ffff:<internal-ip>]", // pii-test-fixture
            "[::ffff:<internal-ip>]",      // pii-test-fixture
            "[::ffff:<internal-ip>]",    // pii-test-fixture
        ] {
            let dsn = format!("postgres://user:pass@{host}:5432/muse_test");
            let err = validate_snapshot_dsn(&dsn).expect_err(&format!(
                "expected {host} (v4-mapped private IPv6) to be rejected"
            ));
            assert!(
                matches!(err, SnapshotGuardError::DenylistMatch { field: "host", .. }),
                "{host} should reject as a host denylist match, got {err:?}"
            );
        }
        // Direct unit check of the helper for the v4-compatible form too.
        assert!(host_is_private_ip("::ffff:<internal-ip>")); // pii-test-fixture
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
        // source snapshot, is fine to read from. The source legitimately
        // lives on the private fleet network, so a private-IP host is NOT
        // rejected here (unlike the load path).
        assert!(validate_not_prod_source("postgres://source-host:5432/tautulli").is_ok());
        assert!(validate_not_prod_source("postgres://source-host:5432/muse_source_export").is_ok());
        let private_source = "postgres://<internal-ip>:5432/tautulli"; // pii-test-fixture
        assert!(validate_not_prod_source(private_source).is_ok());
    }

    #[test]
    fn validate_not_prod_source_rejects_dbname_query_param_override() {
        // GAP 2: the URL db is a benign `muse_source_export`, but
        // ?dbname=muse would make pg_dump/libpq read the prod DB. The shared
        // fail-closed parser rejects the override before the db-name policy
        // even runs.
        let dsn = "postgres://source-host:5432/muse_source_export?dbname=muse";
        let err =
            validate_not_prod_source(dsn).expect_err("source dbname override must be rejected");
        assert!(
            matches!(err, SnapshotGuardError::DisallowedQueryParam { ref param } if param == "dbname"),
            "got {err:?}"
        );
    }

    #[test]
    fn validate_not_prod_source_rejects_multi_host() {
        let dsn = "postgres://source-a:5432,source-b:5432/tautulli";
        assert!(matches!(
            validate_not_prod_source(dsn),
            Err(SnapshotGuardError::MultiHost { .. })
        ));
    }

    #[test]
    fn validate_not_prod_source_allows_inert_query_params() {
        let dsn = "postgres://source-host:5432/tautulli?sslmode=require&connect_timeout=5";
        assert!(validate_not_prod_source(dsn).is_ok());
    }

    #[test]
    fn validate_not_prod_source_rejects_omitted_db_defaulting_to_username() {
        // `postgres://muse@source-host` has an EMPTY path db, but libpq/
        // pg_dump would default the target to the username `muse` -> prod.
        // Reject fail-closed on the empty db, before any bare-name check.
        for dsn in [
            "postgres://muse@source-host",
            "postgres://source-host",
            "postgres://source-host/",
            "postgres://user@source-host:5432/",
        ] {
            let err = validate_not_prod_source(dsn)
                .expect_err(&format!("{dsn} (no explicit db) must be rejected"));
            assert!(
                matches!(err, SnapshotGuardError::MissingDatabaseName),
                "{dsn} -> {err:?}"
            );
        }
    }

    #[test]
    fn validate_not_prod_source_allows_an_explicit_non_prod_source_db() {
        // A valid, explicit source db passes even with a username present.
        assert!(validate_not_prod_source("postgres://user@source-host/plex_library").is_ok());
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
