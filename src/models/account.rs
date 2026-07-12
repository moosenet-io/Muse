//! `accounts` — Plex managed/home users (spec §3.2/§3.3). Taste and
//! telemetry are per-account and MUST NEVER be blended across accounts.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::FromRow;

#[derive(Debug, Clone, FromRow, Serialize, Deserialize)]
pub struct Account {
    pub id: i64,
    pub plex_account_id: Option<String>,
    pub username: Option<String>,
    pub friendly_name: Option<String>,
    pub is_home_user: bool,
    pub is_primary: bool,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Default)]
pub struct NewAccount {
    pub plex_account_id: Option<String>,
    pub username: Option<String>,
    pub friendly_name: Option<String>,
    pub is_home_user: bool,
    pub is_primary: bool,
}
