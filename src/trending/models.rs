//! Typed (partial) models for TMDb JSON responses (MUSE-19).
//!
//! Like `crate::plex::models`, every field is intentionally permissive
//! (`Option`/`#[serde(default)]`) — TMDb overloads several fields by media
//! type (movie vs tv) and a strict schema would break parsing on shapes we
//! don't otherwise need.

use std::collections::HashMap;

use serde::Deserialize;

/// `/trending/{media_type}/{window}` and `/movie|tv/popular` share this
/// paginated-results envelope shape.
#[derive(Debug, Deserialize)]
pub(crate) struct ResultsEnvelope<T> {
    // `default = "Vec::new"` (not bare `#[serde(default)]`) avoids serde adding a
    // spurious `T: Default` bound to the derived Deserialize impl for this
    // generic envelope — TmdbTitle intentionally has no Default.
    #[serde(default = "Vec::new")]
    pub(crate) results: Vec<T>,
}

/// A trending/popular title entry. TMDb uses `title`/`release_date` for
/// movies and `name`/`first_air_date` for tv — both are optional here and
/// resolved via [`Self::display_title`]/[`Self::year`].
#[derive(Debug, Clone, Deserialize)]
pub struct TmdbTitle {
    pub id: i64,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub release_date: Option<String>,
    #[serde(rename = "first_air_date", default)]
    pub first_air_date: Option<String>,
    #[serde(default)]
    pub popularity: Option<f64>,
    #[serde(default)]
    pub vote_average: Option<f64>,
    #[serde(default)]
    pub media_type: Option<String>,
}

impl TmdbTitle {
    /// The movie `title` or tv `name`, whichever TMDb populated.
    pub fn display_title(&self) -> Option<&str> {
        self.title.as_deref().or(self.name.as_deref())
    }

    /// Best-effort release year, parsed from whichever of
    /// `release_date`/`first_air_date` TMDb populated (`YYYY-MM-DD`).
    pub fn year(&self) -> Option<i32> {
        self.release_date
            .as_deref()
            .filter(|d| !d.is_empty())
            .or_else(|| self.first_air_date.as_deref().filter(|d| !d.is_empty()))
            .and_then(|d| d.get(0..4))
            .and_then(|y| y.parse::<i32>().ok())
    }
}

/// `/movie|tv/{id}/watch/providers` envelope: `{"id": .., "results": {"US": {...}}}`.
#[derive(Debug, Deserialize)]
pub(crate) struct WatchProvidersEnvelope {
    #[serde(default)]
    pub(crate) results: HashMap<String, RegionProviders>,
}

/// Per-region availability breakdown by offer type.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct RegionProviders {
    #[serde(default)]
    pub link: Option<String>,
    #[serde(default)]
    pub flatrate: Vec<ProviderEntry>,
    #[serde(default)]
    pub ads: Vec<ProviderEntry>,
    #[serde(default)]
    pub rent: Vec<ProviderEntry>,
    #[serde(default)]
    pub buy: Vec<ProviderEntry>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ProviderEntry {
    pub provider_id: i64,
    pub provider_name: String,
}

/// `/movie|tv/{id}` details response (MUSEL-A2), requested with
/// `append_to_response=external_ids` so the id-bridge fields land in the
/// same call as the rest of the record — see
/// `client::TmdbClient::get_details`. Like [`TmdbTitle`], deliberately
/// permissive: movie vs tv responses share most of this shape but not all
/// of it (e.g. only tv has `name`/`first_air_date`).
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct TmdbDetails {
    #[serde(default)]
    pub(crate) title: Option<String>,
    #[serde(default)]
    pub(crate) name: Option<String>,
    #[serde(default)]
    pub(crate) overview: Option<String>,
    #[serde(default)]
    pub(crate) release_date: Option<String>,
    #[serde(rename = "first_air_date", default)]
    pub(crate) first_air_date: Option<String>,
    #[serde(default)]
    pub(crate) vote_average: Option<f64>,
    #[serde(default)]
    pub(crate) poster_path: Option<String>,
    #[serde(default)]
    pub(crate) backdrop_path: Option<String>,
    #[serde(default)]
    pub(crate) genres: Vec<TmdbGenre>,
    #[serde(default)]
    pub(crate) external_ids: Option<TmdbExternalIds>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct TmdbGenre {
    #[serde(default)]
    pub(crate) name: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub(crate) struct TmdbExternalIds {
    #[serde(default)]
    pub(crate) imdb_id: Option<String>,
}

/// `/find/{external_id}?external_source=imdb_id` — the IMDb-id bridge
/// MUSEL-A2's TMDb adapter uses when only an `imdb_id` is known for a
/// title (no native `tmdb_id` yet). TMDb splits hits by media type; a
/// caller already knows which bucket it wants from the requested
/// `MediaKind`.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct TmdbFindResults {
    #[serde(default)]
    pub(crate) movie_results: Vec<TmdbTitle>,
    #[serde(default)]
    pub(crate) tv_results: Vec<TmdbTitle>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tmdb_title_prefers_movie_fields_then_tv_fields() {
        let movie: TmdbTitle = serde_json::from_str(
            r#"{"id": 27205, "title": "Inception", "release_date": "2010-07-15", "popularity": 80.5}"#,
        )
        .unwrap();
        assert_eq!(movie.display_title(), Some("Inception"));
        assert_eq!(movie.year(), Some(2010));

        let tv: TmdbTitle = serde_json::from_str(
            r#"{"id": 1399, "name": "Game of Thrones", "first_air_date": "2011-04-17"}"#,
        )
        .unwrap();
        assert_eq!(tv.display_title(), Some("Game of Thrones"));
        assert_eq!(tv.year(), Some(2011));
    }

    #[test]
    fn tmdb_title_handles_missing_dates_gracefully() {
        let bare: TmdbTitle = serde_json::from_str(r#"{"id": 1}"#).unwrap();
        assert_eq!(bare.display_title(), None);
        assert_eq!(bare.year(), None);
    }

    #[test]
    fn watch_providers_envelope_parses_region_map() {
        let envelope: WatchProvidersEnvelope = serde_json::from_str(
            r#"{
                "id": 603,
                "results": {
                    "US": {
                        "link": "https://www.themoviedb.org/movie/603/watch",
                        "flatrate": [{"provider_id": 8, "provider_name": "Netflix"}],
                        "rent": [{"provider_id": 2, "provider_name": "Apple TV"}]
                    }
                }
            }"#,
        )
        .unwrap();

        let us = envelope.results.get("US").expect("US region present");
        assert_eq!(us.flatrate.len(), 1);
        assert_eq!(us.flatrate[0].provider_name, "Netflix");
        assert_eq!(us.rent[0].provider_name, "Apple TV");
        assert!(us.buy.is_empty());
    }
}
