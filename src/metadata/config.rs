//! TheTVDB v4 config, narrowed from the central `crate::config::Config` —
//! same posture as `download::config::QbitConfig`/`Config::qbit`: this
//! module does NOT read the environment itself. `MUSE_TVDB_API_KEY`/
//! `MUSE_TVDB_PIN` are read in exactly one place, `Config::from_env`, and
//! assembled into a [`TvdbConfig`] via [`crate::config::Config::tvdb`].
//!
//! S7 secret-hygiene (review: codex): `api_key`/`pin` are secret-shaped
//! (the TVDB `/login` credential), so they're wrapped in
//! [`crate::download::config::QbitPassword`] — the SAME redacting
//! newtype MUSEM-02 already established for `download::config::QbitConfig`'s
//! password, reused here rather than inventing a second redaction pattern.
//! Despite its qbit-specific name, `QbitPassword` is a generic "opaque
//! secret string with a `Debug`/`Display` that always prints
//! `<redacted>`" wrapper — the same posture `crate::config::Config` itself
//! uses for `qbit_pass`. This keeps `{:?}`/`tracing::debug!(..)` on
//! `TvdbConfig` (and on `Config`'s own `tvdb_api_key`/`tvdb_pin` fields,
//! which use this same wrapper — see `Config::from_env`) from ever
//! printing the real key/pin.

use crate::download::config::QbitPassword;

/// TheTVDB v4 connection configuration.
#[derive(Debug, Clone)]
pub struct TvdbConfig {
    /// TheTVDB v4 API base URL, e.g. `https://api4.thetvdb.com/v4`.
    /// Overridable (`MUSE_TVDB_BASE_URL`) so tests/an on-prem proxy can
    /// point the client elsewhere, same seam `TmdbClient::new(base_url, ..)`
    /// already provides for TMDb. Not secret-shaped (a host, not a
    /// credential) — a plain `String`.
    pub base_url: String,
    /// TheTVDB v4 API key (`MUSE_TVDB_API_KEY`), the credential `POST
    /// /login` exchanges for a bearer token. Never a literal (S1);
    /// <secret-manager>-materialized at runtime (S7). Wrapped in `QbitPassword`
    /// so it can never leak through a `Debug`/`Display` of this struct.
    pub api_key: QbitPassword,
    /// Optional subscriber PIN (`MUSE_TVDB_PIN`) TheTVDB v4's `/login`
    /// accepts alongside `apikey` for subscription-model API keys. Most
    /// standard API keys don't need one — independently optional, same
    /// posture as `Config::news_api_key` being optional alongside
    /// `Config::news_url`. Also secret-shaped, also wrapped.
    pub pin: Option<QbitPassword>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Mirrors MUSEM-02's `download::config::tests::config_debug_redacts_password_field`:
    /// proves a `{:?}` of the whole `TvdbConfig` never prints the real
    /// api_key/pin (the S7 finding this module's doc comment addresses).
    #[test]
    fn debug_redacts_api_key_and_pin() {
        let cfg = TvdbConfig {
            base_url: "https://api4.thetvdb.com/v4".to_string(),
            api_key: QbitPassword::from("super-secret-tvdb-key".to_string()),
            pin: Some(QbitPassword::from("9999".to_string())),
        };

        let debug = format!("{cfg:?}");
        assert!(!debug.contains("super-secret-tvdb-key"));
        assert!(!debug.contains("9999"));
        assert!(debug.contains("<redacted>"));
    }
}
