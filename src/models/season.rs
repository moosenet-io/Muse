//! `seasons` — middle level of the 3-level TV hierarchy (blueprint §3/§7.2).

use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use sqlx::FromRow;

#[derive(Debug, Clone, FromRow, Serialize, Deserialize)]
pub struct Season {
    pub id: i64,
    pub media_item_id: i64,
    pub season_number: i32,
    pub title: Option<String>,
    pub overview: Option<String>,
    pub monitored: bool,
    pub air_date: Option<NaiveDate>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct NewSeason {
    pub media_item_id: i64,
    pub season_number: i32,
    pub title: Option<String>,
    pub overview: Option<String>,
    pub monitored: bool,
    pub air_date: Option<NaiveDate>,
}
