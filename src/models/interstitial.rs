//! `interstitials` — the bumper/commercial/music-video/ident pool (MUSE-23,
//! spec §3.8/§4d-B). Auto-tagging (kind/decade/theme/mood/duration) is a
//! local-LLM pass that populates these rows; that tagging logic itself is
//! out of scope here — this module only owns the storage shape.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::FromRow;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type)]
#[sqlx(type_name = "interstitial_kind", rename_all = "snake_case")]
#[serde(rename_all = "snake_case")]
pub enum InterstitialKind {
    Bumper,
    Commercial,
    MusicVideo,
    Ident,
    Short,
    Trailer,
}

#[derive(Debug, Clone, FromRow, Serialize, Deserialize)]
pub struct Interstitial {
    pub id: i64,
    pub plex_rating_key: Option<String>,
    pub kind: InterstitialKind,
    pub title: Option<String>,
    pub decade: Option<i32>,
    pub theme: Option<String>,
    pub genre: Option<String>,
    pub mood: Option<String>,
    pub duration_ms: Option<i64>,
    pub tags: Vec<String>,
    pub source: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct NewInterstitial {
    pub plex_rating_key: Option<String>,
    pub kind: InterstitialKind,
    pub title: Option<String>,
    pub decade: Option<i32>,
    pub theme: Option<String>,
    pub genre: Option<String>,
    pub mood: Option<String>,
    pub duration_ms: Option<i64>,
    pub tags: Vec<String>,
    pub source: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interstitial_kind_serde_round_trip() {
        let json = serde_json::to_string(&InterstitialKind::MusicVideo).unwrap();
        assert_eq!(json, "\"music_video\"");
        let back: InterstitialKind = serde_json::from_str(&json).unwrap();
        assert_eq!(back, InterstitialKind::MusicVideo);
    }
}
