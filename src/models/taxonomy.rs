//! People / genres / tags / collections (spec §3.2), re-homed per the
//! metadata/instance split: people/genres/collections hang off shared
//! `media_metadata`, tags stay on the per-library `media_items`
//! (see `migrations/0006_media_items.sql` and `0011_people_genres_collections.sql`).

use sqlx::FromRow;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, FromRow, Serialize, Deserialize)]
pub struct Person {
    pub id: i64,
    pub tmdb_person_id: Option<String>,
    pub name: String,
    pub known_for_department: Option<String>,
}

#[derive(Debug, Clone, FromRow, Serialize, Deserialize)]
pub struct MediaMetadataCredit {
    pub media_metadata_id: i64,
    pub person_id: i64,
    pub role: String,
    pub character: Option<String>,
    pub cast_order: Option<i32>,
}

#[derive(Debug, Clone, FromRow, Serialize, Deserialize)]
pub struct Genre {
    pub id: i64,
    pub name: String,
}

#[derive(Debug, Clone, FromRow, Serialize, Deserialize)]
pub struct Tag {
    pub id: i64,
    pub name: String,
    pub source: Option<String>,
}

#[derive(Debug, Clone, FromRow, Serialize, Deserialize)]
pub struct Collection {
    pub id: i64,
    pub name: String,
    pub source: Option<String>,
    pub tmdb_collection_id: Option<String>,
    pub plex_rating_key: Option<String>,
    pub description: Option<String>,
}
