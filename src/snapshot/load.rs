//! MUSET-03 AC3: the ISOLATED test-Postgres connection path.
//!
//! Every snapshot-pipeline entry point that needs a Postgres pool for the
//! isolated snapshot/test database goes through [`connect_snapshot_db`] --
//! the ONE place in this module that turns a DSN into a live `sqlx::PgPool`.
//! It ALWAYS validates the DSN via `snapshot::guard::validate_snapshot_dsn`
//! first and refuses to connect at all if that guard rejects it. There is no
//! other constructor in this module that opens a pool without going through
//! the guard -- this is the structural guarantee behind AC3/AC5.

use sqlx::postgres::{PgPool, PgPoolOptions};

use crate::error::{MuseError, MuseResult};
use crate::snapshot::guard::validate_snapshot_dsn;

/// Env var carrying the isolated snapshot/test Postgres DSN. Distinct from
/// `MUSE_DATABASE_URL` (the application's own runtime DB) and from
/// `MUSE_SNAPSHOT_SOURCE_POSTGRES_URL` (the acquisition step's read source,
/// see `snapshot::acquisition`) -- three separate roles, three separate env
/// vars, so a copy-paste mistake can't silently collapse them.
pub const SNAPSHOT_DATABASE_URL_VAR: &str = "MUSE_SNAPSHOT_DATABASE_URL";

/// Fallback env var: reuses the same disposable scratch Postgres the
/// existing MUSET-01/02 `db_gated` test harness already targets
/// (`src/endpoint_tests.rs`'s `test_pool_or_skip`), so a single scratch
/// Postgres instance can serve both plain integration tests and
/// snapshot-loaded tests without provisioning two databases. Checked only
/// when [`SNAPSHOT_DATABASE_URL_VAR`] is unset.
pub const TEST_DATABASE_URL_VAR: &str = "MUSE_TEST_DATABASE_URL";

/// Resolve the configured snapshot/test DSN from the environment, preferring
/// the snapshot-specific var. Returns `None` (never an error) when neither
/// is set -- callers use this for the same "skip cleanly, never fail" gating
/// idiom `endpoint_tests.rs::db_gated::test_pool_or_skip` already uses.
pub fn snapshot_database_url_from_env() -> Option<String> {
    std::env::var(SNAPSHOT_DATABASE_URL_VAR)
        .ok()
        .or_else(|| std::env::var(TEST_DATABASE_URL_VAR).ok())
}

/// Connect to the isolated snapshot/test Postgres, running the AC5 DSN guard
/// FIRST. Refuses (returns `Err`, never connects) if the DSN looks like it
/// could point at a live media DB or the production muse DB.
pub async fn connect_snapshot_db(database_url: &str) -> MuseResult<PgPool> {
    validate_snapshot_dsn(database_url).map_err(|e| MuseError::Config(e.to_string()))?;

    PgPoolOptions::new()
        .max_connections(5)
        .connect(database_url)
        .await
        .map_err(MuseError::Database)
}

/// Convenience: resolve the DSN from the environment (per
/// [`snapshot_database_url_from_env`]) and connect, guard included. Returns
/// `Ok(None)` (not an error) when no snapshot/test DSN is configured at all
/// -- the same clean-skip posture as the rest of this crate's DB-gated
/// tests. Returns `Err` if a DSN IS configured but fails the guard or the
/// connection itself fails.
pub async fn connect_snapshot_db_from_env() -> MuseResult<Option<PgPool>> {
    match snapshot_database_url_from_env() {
        None => Ok(None),
        Some(url) => connect_snapshot_db(&url).await.map(Some),
    }
}

/// Run this crate's migrations against the isolated snapshot/test database
/// -- the snapshot DB shares the production schema (including
/// `snapshot_provenance`, migrations/0099) so muse's own repo/query code
/// runs unmodified against loaded snapshot data.
pub async fn migrate_snapshot_db(pool: &PgPool) -> MuseResult<()> {
    sqlx::migrate!("./migrations")
        .run(pool)
        .await
        .map_err(|e| MuseError::Internal(anyhow::anyhow!(e)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_database_url_from_env_prefers_the_snapshot_specific_var() {
        std::env::set_var(SNAPSHOT_DATABASE_URL_VAR, "postgres://x/muse_snapshot");
        std::env::set_var(TEST_DATABASE_URL_VAR, "postgres://x/muse_test_fallback");
        assert_eq!(
            snapshot_database_url_from_env().as_deref(),
            Some("postgres://x/muse_snapshot")
        );
        std::env::remove_var(SNAPSHOT_DATABASE_URL_VAR);
        std::env::remove_var(TEST_DATABASE_URL_VAR);
    }

    #[test]
    fn snapshot_database_url_from_env_falls_back_to_test_database_url() {
        std::env::remove_var(SNAPSHOT_DATABASE_URL_VAR);
        std::env::set_var(TEST_DATABASE_URL_VAR, "postgres://x/muse_test_fallback");
        assert_eq!(
            snapshot_database_url_from_env().as_deref(),
            Some("postgres://x/muse_test_fallback")
        );
        std::env::remove_var(TEST_DATABASE_URL_VAR);
    }

    #[test]
    fn snapshot_database_url_from_env_is_none_when_neither_var_set() {
        std::env::remove_var(SNAPSHOT_DATABASE_URL_VAR);
        std::env::remove_var(TEST_DATABASE_URL_VAR);
        assert!(snapshot_database_url_from_env().is_none());
    }

    /// AC5 exercised at the connect layer (not just the guard layer
    /// directly): a live-shaped DSN must be refused BEFORE any connection
    /// attempt is made -- proven here by using an unroutable-looking DSN
    /// that would hang/error if a connection were actually attempted, and
    /// asserting the guard error comes back synchronously instead.
    #[tokio::test]
    async fn connect_snapshot_db_refuses_a_live_shaped_dsn_without_attempting_connection() {
        let live_shaped = "postgres://user:pass@<internal-ip>:5432/muse"; // pii-test-fixture
        let result = connect_snapshot_db(live_shaped).await;
        assert!(
            result.is_err(),
            "a live-shaped DSN must never be used to connect, even speculatively"
        );
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("snapshot DSN guard"),
            "expected the guard's error to surface, got: {err}"
        );
    }

    #[tokio::test]
    async fn connect_snapshot_db_refuses_the_bare_prod_muse_dsn() {
        let prod_shaped = "postgres://user:pass@muse-db-host:5432/muse";
        let result = connect_snapshot_db(prod_shaped).await;
        assert!(result.is_err());
    }
}
