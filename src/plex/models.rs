//! Typed (partial) models for Plex Media Server / Plex Discover JSON responses.
//!
//! Plex wraps every response in a `MediaContainer` envelope; the payload array
//! key varies by endpoint (`Directory` for library sections, `Metadata` for
//! items/sessions/history, `Account` for local server accounts). Every field
//! here is intentionally permissive (`Option`/`#[serde(default)]`) because
//! Plex's JSON shape varies by server version and item type (movie vs show vs
//! episode vs session) — a strict schema would break parsing on fields we
//! don't otherwise care about.

use serde::Deserialize;

/// Top-level Plex response envelope: `{ "MediaContainer": { ... } }`.
#[derive(Debug, Deserialize)]
pub(crate) struct Envelope<T> {
    #[serde(rename = "MediaContainer")]
    pub(crate) media_container: T,
}

/// `/library/sections` container: a list of library `Directory` entries.
#[derive(Debug, Default, Deserialize)]
pub(crate) struct DirectoryContainer {
    #[serde(rename = "Directory", default)]
    pub(crate) directory: Vec<Library>,
}

/// Container shape shared by `/library/sections/{key}/all`,
/// `/library/metadata/{ratingKey}`, `/status/sessions`,
/// `/status/sessions/history/all`, `/library/onDeck`, `/library/recentlyAdded`,
/// and the Plex Discover watchlist endpoint — all of these return their items
/// under a `Metadata` array.
#[derive(Debug, Default, Deserialize)]
pub(crate) struct MetadataContainer {
    #[serde(rename = "Metadata", default)]
    pub(crate) metadata: Vec<MediaItem>,
}

/// `/accounts` container: local Plex Media Server accounts (users who have
/// accessed this server) — used for per-account taste isolation.
///
/// NOTE: this is distinct from Plex Home/managed-user metadata that lives on
/// `plex.tv` (`/api/home/users`) rather than the local PMS; we intentionally
/// use the local `/accounts` endpoint so `accounts()` only needs the
/// already-configured `PLEX_URL`/`PLEX_TOKEN`. The orchestrator should verify
/// this returns the full managed/home-user set expected for §3.2 `accounts`
/// on a real server — if not, a second call against `plex.tv` may be needed.
#[derive(Debug, Default, Deserialize)]
pub(crate) struct AccountContainer {
    #[serde(rename = "Account", default)]
    pub(crate) account: Vec<Account>,
}

/// A Plex library section (from `/library/sections`).
#[derive(Debug, Clone, Deserialize)]
pub struct Library {
    pub key: String,
    pub title: String,
    #[serde(rename = "type", default)]
    pub library_type: Option<String>,
    #[serde(default)]
    pub agent: Option<String>,
    #[serde(default)]
    pub scanner: Option<String>,
    #[serde(default)]
    pub language: Option<String>,
    #[serde(default)]
    pub uuid: Option<String>,
    #[serde(rename = "updatedAt", default)]
    pub updated_at: Option<i64>,
}

/// A local Plex Media Server account.
#[derive(Debug, Clone, Deserialize)]
pub struct Account {
    pub id: i64,
    #[serde(default)]
    pub key: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(rename = "defaultAudioLanguage", default)]
    pub default_audio_language: Option<String>,
    #[serde(default)]
    pub thumb: Option<String>,
}

/// A `{tag: "..."}` style entry used for genres and collections.
#[derive(Debug, Clone, Deserialize)]
pub struct Tag {
    pub tag: String,
}

/// A person credit entry (`Director`, `Writer`, `Role`/actor). `role` carries
/// the character name and is only populated for `Role` (cast) entries.
#[derive(Debug, Clone, Deserialize)]
pub struct PersonTag {
    pub tag: String,
    #[serde(default)]
    pub role: Option<String>,
    #[serde(default)]
    pub thumb: Option<String>,
}

/// An external-provider GUID, e.g. `{"id": "tmdb://603"}`.
#[derive(Debug, Clone, Deserialize)]
pub struct Guid {
    pub id: String,
}

/// The `User` block Plex attaches to active sessions and history entries.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct SessionUser {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub thumb: Option<String>,
}

/// The `Player` block Plex attaches to active sessions and history entries.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct SessionPlayer {
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub platform: Option<String>,
    #[serde(default)]
    pub product: Option<String>,
    #[serde(default)]
    pub device: Option<String>,
    #[serde(default)]
    pub state: Option<String>,
    #[serde(rename = "machineIdentifier", default)]
    pub machine_identifier: Option<String>,
}

/// A Plex `Metadata` entry — reused across library items, session entries,
/// history entries, on-deck/recently-added, and watchlist entries. Not every
/// field is populated for every endpoint; callers should treat all of these
/// as best-effort.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct MediaItem {
    #[serde(rename = "ratingKey", default)]
    pub rating_key: Option<String>,
    #[serde(default)]
    pub key: Option<String>,
    #[serde(default)]
    pub guid: Option<String>,
    #[serde(rename = "type", default)]
    pub item_type: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(rename = "titleSort", default)]
    pub title_sort: Option<String>,
    #[serde(default)]
    pub summary: Option<String>,
    #[serde(default)]
    pub year: Option<i64>,
    #[serde(default)]
    pub thumb: Option<String>,
    #[serde(default)]
    pub art: Option<String>,
    #[serde(default)]
    pub duration: Option<i64>,
    #[serde(rename = "addedAt", default)]
    pub added_at: Option<i64>,
    #[serde(rename = "updatedAt", default)]
    pub updated_at: Option<i64>,
    #[serde(rename = "originallyAvailableAt", default)]
    pub originally_available_at: Option<String>,
    #[serde(rename = "contentRating", default)]
    pub content_rating: Option<String>,
    #[serde(default)]
    pub studio: Option<String>,

    // --- ratings ---
    #[serde(rename = "audienceRating", default)]
    pub audience_rating: Option<f64>,
    #[serde(default)]
    pub rating: Option<f64>,
    #[serde(rename = "userRating", default)]
    pub user_rating: Option<f64>,

    // --- credits/tags (§ "genres, directors/actors, ... collections") ---
    #[serde(rename = "Genre", default)]
    pub genres: Vec<Tag>,
    #[serde(rename = "Director", default)]
    pub directors: Vec<PersonTag>,
    #[serde(rename = "Writer", default)]
    pub writers: Vec<PersonTag>,
    #[serde(rename = "Role", default)]
    pub actors: Vec<PersonTag>,
    #[serde(rename = "Collection", default)]
    pub collections: Vec<Tag>,
    #[serde(rename = "Guid", default)]
    pub guids: Vec<Guid>,

    // --- hierarchy (episodes/seasons) ---
    #[serde(rename = "grandparentTitle", default)]
    pub grandparent_title: Option<String>,
    #[serde(rename = "grandparentRatingKey", default)]
    pub grandparent_rating_key: Option<String>,
    #[serde(rename = "parentTitle", default)]
    pub parent_title: Option<String>,
    #[serde(rename = "parentRatingKey", default)]
    pub parent_rating_key: Option<String>,
    #[serde(rename = "librarySectionID", default)]
    pub library_section_id: Option<serde_json::Value>,

    // --- playback / session state (active sessions + history) ---
    #[serde(rename = "viewOffset", default)]
    pub view_offset: Option<i64>,
    #[serde(rename = "viewCount", default)]
    pub view_count: Option<i64>,
    #[serde(rename = "lastViewedAt", default)]
    pub last_viewed_at: Option<i64>,
    /// History entries carry `accountID` (int) directly on the Metadata node.
    #[serde(rename = "accountID", default)]
    pub account_id: Option<i64>,
    #[serde(rename = "sessionKey", default)]
    pub session_key: Option<String>,
    #[serde(rename = "User", default)]
    pub user: Option<SessionUser>,
    #[serde(rename = "Player", default)]
    pub player: Option<SessionPlayer>,
}

impl MediaItem {
    /// First GUID whose scheme matches `provider` (e.g. `"tmdb"`, `"tvdb"`,
    /// `"imdb"`), with the `scheme://` prefix stripped.
    fn guid_id(&self, provider: &str) -> Option<&str> {
        let prefix = format!("{provider}://");
        self.guids
            .iter()
            .find_map(|g| g.id.strip_prefix(prefix.as_str()))
    }

    pub fn tmdb_id(&self) -> Option<&str> {
        self.guid_id("tmdb")
    }

    pub fn tvdb_id(&self) -> Option<&str> {
        self.guid_id("tvdb")
    }

    pub fn imdb_id(&self) -> Option<&str> {
        self.guid_id("imdb")
    }

    /// Best-effort resolved account id for this entry: prefers the explicit
    /// `accountID` (history entries), then the nested `User.id` (active
    /// sessions), which Plex represents as a string.
    pub fn resolved_account_id(&self) -> Option<String> {
        if let Some(id) = self.account_id {
            return Some(id.to_string());
        }
        self.user.as_ref().and_then(|u| u.id.clone())
    }
}
