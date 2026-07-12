//! Multi-instance *arr fleet configuration.
//!
//! The instance list is **never hardcoded** — it is a JSON array read from
//! the `MUSE_ARR_INSTANCES` environment variable (materialized from
//! <secret-manager> at runtime, same discipline as every other secret in this
//! crate), parsed by [`load_arr_instances`]. Each entry maps 1:1 onto a
//! `libraries` row (blueprint §1/§7.9: one *arr instance = one library,
//! sharded by root folder/purpose — never N Postgres databases).
//!
//! Expected shape (redacted example — RFC5737 host, not a real fleet IP):
//! ```json
//! [
//!   {"name": "radarr", "kind": "radarr", "base_url": "http://192.0.2.10:7878", "api_key": "...", "library_kind": "movie"},
//!   {"name": "radarr_animated", "kind": "radarr", "base_url": "http://192.0.2.11:7878", "api_key": "...", "library_kind": "movie"},
//!   {"name": "sonarr", "kind": "sonarr", "base_url": "http://192.0.2.20:8989", "api_key": "...", "library_kind": "tv"}
//! ]
//! ```

use serde::Deserialize;

use crate::error::{MuseError, MuseResult};
use crate::models::library::LibraryKind;

/// Which *arr application an instance is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArrKind {
    Radarr,
    Sonarr,
}

impl ArrKind {
    /// The provider-precedence keying this kind uses for `media_metadata`
    /// upserts (blueprint §7.7): Radarr keys on TMDb, Sonarr on TVDB.
    pub fn quality_key_prefix(self) -> &'static str {
        match self {
            ArrKind::Radarr => "radarr",
            ArrKind::Sonarr => "sonarr",
        }
    }
}

/// One configured *arr instance (one row of `MUSE_ARR_INSTANCES`).
#[derive(Debug, Clone, Deserialize)]
pub struct ArrInstanceConfig {
    /// Instance name, also used as the `libraries.name` (e.g. `"radarr"`,
    /// `"radarr_animated"`, `"sonarr_anime"`).
    pub name: String,
    pub kind: ArrKind,
    pub base_url: String,
    pub api_key: String,
    /// The `libraries.kind` this instance maps to (`movie` for every Radarr
    /// instance, `tv` for every Sonarr instance).
    pub library_kind: LibraryKind,
    /// Informational root-folder hint for the `libraries.root_folder`
    /// column. Optional: when absent, ingest falls back to an empty string
    /// rather than failing (root folder is descriptive metadata here, not a
    /// join key) — a real value can be backfilled from a later
    /// `/api/v3/rootfolder` sync.
    #[serde(default)]
    pub root_folder: Option<String>,
}

/// Parse the configured *arr fleet from a raw `MUSE_ARR_INSTANCES` JSON
/// string. `None`/empty is treated as "no instances configured" (an empty
/// `Vec`, not an error) — ingest simply has nothing to do, matching
/// `PlexClient::from_config`'s graceful-degrade posture for an unconfigured
/// dependency. A non-empty value that fails to parse as JSON *is* an error
/// (a real operator typo, worth surfacing rather than silently ignoring).
pub fn load_arr_instances(raw_json: Option<&str>) -> MuseResult<Vec<ArrInstanceConfig>> {
    match raw_json.map(str::trim) {
        None => Ok(Vec::new()),
        Some("") => Ok(Vec::new()),
        Some(s) => serde_json::from_str(s)
            .map_err(|e| MuseError::Config(format!("invalid MUSE_ARR_INSTANCES JSON: {e}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn none_and_empty_parse_to_no_instances() {
        assert!(load_arr_instances(None).unwrap().is_empty());
        assert!(load_arr_instances(Some("")).unwrap().is_empty());
        assert!(load_arr_instances(Some("   ")).unwrap().is_empty());
    }

    #[test]
    fn parses_a_mixed_radarr_sonarr_fleet() {
        let json = r#"[
            {"name": "radarr", "kind": "radarr", "base_url": "http://192.0.2.10:7878", "api_key": "k1", "library_kind": "movie"},
            {"name": "radarr_animated", "kind": "radarr", "base_url": "http://192.0.2.11:7878", "api_key": "k2", "library_kind": "movie", "root_folder": "/media/Animated Movie/"},
            {"name": "sonarr", "kind": "sonarr", "base_url": "http://192.0.2.20:8989", "api_key": "k3", "library_kind": "tv"}
        ]"#;

        let instances = load_arr_instances(Some(json)).expect("fleet should parse");
        assert_eq!(instances.len(), 3);
        assert_eq!(instances[0].name, "radarr");
        assert_eq!(instances[0].kind, ArrKind::Radarr);
        assert_eq!(instances[0].library_kind, LibraryKind::Movie);
        assert!(instances[0].root_folder.is_none());
        assert_eq!(
            instances[1].root_folder.as_deref(),
            Some("/media/Animated Movie/")
        );
        assert_eq!(instances[2].kind, ArrKind::Sonarr);
        assert_eq!(instances[2].library_kind, LibraryKind::Tv);
    }

    #[test]
    fn malformed_json_is_a_config_error_not_a_panic() {
        let result = load_arr_instances(Some("{not valid json"));
        assert!(matches!(result, Err(MuseError::Config(_))));
    }

    #[test]
    fn quality_key_prefix_matches_provider_precedence() {
        assert_eq!(ArrKind::Radarr.quality_key_prefix(), "radarr");
        assert_eq!(ArrKind::Sonarr.quality_key_prefix(), "sonarr");
    }
}
