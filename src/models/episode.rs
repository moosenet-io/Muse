//! `episodes` — leaf level of the 3-level TV hierarchy (blueprint §3).

use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use sqlx::FromRow;

#[derive(Debug, Clone, FromRow, Serialize, Deserialize)]
pub struct Episode {
    pub id: i64,
    pub season_id: i64,
    pub media_item_id: i64,
    pub episode_number: i32,
    pub absolute_episode_number: Option<i32>,
    pub scene_absolute_episode_number: Option<i32>,
    pub scene_season_number: Option<i32>,
    pub scene_episode_number: Option<i32>,
    pub unverified_scene_numbering: bool,
    pub title: Option<String>,
    pub overview: Option<String>,
    pub air_date: Option<NaiveDate>,
    pub air_date_utc: Option<DateTime<Utc>>,
    pub runtime_minutes: Option<i32>,
    pub monitored: bool,
    pub has_file: bool,
    pub tvdb_id: Option<String>,
    pub plex_rating_key: Option<String>,
    pub last_search_time: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct NewEpisode {
    pub season_id: i64,
    pub media_item_id: i64,
    pub episode_number: i32,
    pub absolute_episode_number: Option<i32>,
    pub title: Option<String>,
    pub overview: Option<String>,
    pub air_date: Option<NaiveDate>,
    pub air_date_utc: Option<DateTime<Utc>>,
    pub runtime_minutes: Option<i32>,
    pub monitored: bool,
    pub tvdb_id: Option<String>,
}
