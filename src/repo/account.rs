//! Repo functions for `accounts`.

use sqlx::PgPool;

use crate::error::{MuseError, MuseResult};
use crate::models::account::{Account, NewAccount};

/// Upsert keyed by `plex_account_id` when present, otherwise a plain insert
/// (a locally-defined account with no Plex identity yet is legitimate but
/// can't be deduped on conflict).
pub async fn upsert_by_plex_account_id(pool: &PgPool, new: &NewAccount) -> MuseResult<Account> {
    let Some(plex_account_id) = new.plex_account_id.as_deref() else {
        return create(pool, new).await;
    };

    sqlx::query_as::<_, Account>(
        r#"
        INSERT INTO accounts (plex_account_id, username, friendly_name, is_home_user, is_primary)
        VALUES ($1, $2, $3, $4, $5)
        ON CONFLICT (plex_account_id) DO UPDATE SET
            username = EXCLUDED.username,
            friendly_name = EXCLUDED.friendly_name,
            is_home_user = EXCLUDED.is_home_user,
            is_primary = EXCLUDED.is_primary
        RETURNING *
        "#,
    )
    .bind(plex_account_id)
    .bind(&new.username)
    .bind(&new.friendly_name)
    .bind(new.is_home_user)
    .bind(new.is_primary)
    .fetch_one(pool)
    .await
    .map_err(MuseError::Database)
}

pub async fn create(pool: &PgPool, new: &NewAccount) -> MuseResult<Account> {
    sqlx::query_as::<_, Account>(
        r#"
        INSERT INTO accounts (plex_account_id, username, friendly_name, is_home_user, is_primary)
        VALUES ($1, $2, $3, $4, $5)
        RETURNING *
        "#,
    )
    .bind(&new.plex_account_id)
    .bind(&new.username)
    .bind(&new.friendly_name)
    .bind(new.is_home_user)
    .bind(new.is_primary)
    .fetch_one(pool)
    .await
    .map_err(MuseError::Database)
}

pub async fn get(pool: &PgPool, id: i64) -> MuseResult<Account> {
    sqlx::query_as::<_, Account>("SELECT * FROM accounts WHERE id = $1")
        .bind(id)
        .fetch_optional(pool)
        .await
        .map_err(MuseError::Database)?
        .ok_or_else(|| MuseError::NotFound(format!("account {id} not found")))
}

pub async fn get_by_plex_account_id(pool: &PgPool, plex_account_id: &str) -> MuseResult<Option<Account>> {
    sqlx::query_as::<_, Account>("SELECT * FROM accounts WHERE plex_account_id = $1")
        .bind(plex_account_id)
        .fetch_optional(pool)
        .await
        .map_err(MuseError::Database)
}

pub async fn list(pool: &PgPool) -> MuseResult<Vec<Account>> {
    sqlx::query_as::<_, Account>("SELECT * FROM accounts ORDER BY id")
        .fetch_all(pool)
        .await
        .map_err(MuseError::Database)
}
