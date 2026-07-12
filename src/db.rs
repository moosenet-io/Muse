//! Database pool construction + migrations.

use sqlx::postgres::{PgPool, PgPoolOptions};

use crate::config::Config;
use crate::error::{MuseError, MuseResult};

/// Build a Postgres connection pool.
///
/// Uses `connect_lazy` so the pool is constructed successfully even if the
/// database is unreachable at startup — this is a Phase 0 scaffold and the
/// service must still come up (and answer `/health` as db:down) when the DB
/// is down. Actual connections are only attempted on first use.
pub fn build_pool(config: &Config) -> MuseResult<PgPool> {
    let database_url = config
        .database_url
        .as_deref()
        .ok_or_else(|| MuseError::Config("MUSE_DATABASE_URL is not set".to_string()))?;

    let pool = PgPoolOptions::new()
        .max_connections(10)
        .connect_lazy(database_url)
        .map_err(MuseError::Database)?;

    Ok(pool)
}

/// Run pending migrations from `./migrations`.
///
/// A no-op today (the `migrations/` directory is empty scaffolding); real
/// schema lands in MUSE-02/MUSE-03.
pub async fn migrate(pool: &PgPool) -> MuseResult<()> {
    sqlx::migrate!("./migrations")
        .run(pool)
        .await
        .map_err(|e| MuseError::Internal(anyhow::anyhow!(e)))?;

    Ok(())
}
