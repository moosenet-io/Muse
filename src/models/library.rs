//! `libraries` — first-class multi-instance dimension (blueprint §1/§7.9).

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::FromRow;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type)]
#[sqlx(type_name = "library_kind", rename_all = "snake_case")]
#[serde(rename_all = "snake_case")]
pub enum LibraryKind {
    Movie,
    Tv,
}

#[derive(Debug, Clone, FromRow, Serialize, Deserialize)]
pub struct Library {
    pub id: i64,
    pub name: String,
    pub kind: LibraryKind,
    pub root_folder: String,
    pub source_arr_name: Option<String>,
    pub source_arr_url: Option<String>,
    pub enabled: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Fields needed to create a new library row.
#[derive(Debug, Clone)]
pub struct NewLibrary {
    pub name: String,
    pub kind: LibraryKind,
    pub root_folder: String,
    pub source_arr_name: Option<String>,
    pub source_arr_url: Option<String>,
}
