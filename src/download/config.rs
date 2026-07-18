//! qBittorrent WebUI connection configuration — a plain data holder.
//!
//! Per S1/S3 ("secrets via config, not scattered `std::env::var`"),
//! `MUSE_QBIT_URL`/`MUSE_QBIT_USER`/`MUSE_QBIT_PASS` are read in exactly one
//! place: the CENTRAL `crate::config::Config::from_env` (mirroring how every
//! other credential in this crate — `api_token`, `plex_token`,
//! `tautulli_api_key`, etc. — is read). Muse has no `SecretManager`/vault
//! crate of its own; "config, not env::var" here means "the central
//! `Config`", not a second bespoke loader. This module does NOT read the
//! environment itself — [`QbitConfig`] is assembled from an already-loaded
//! `Config` via [`crate::config::Config::qbit`], which callers
//! (`download::qbit::QbitClient::from_config`) use instead of touching
//! `std::env::var` directly.

use std::fmt;

/// Wraps the qBittorrent WebUI password so it can never leak through a
/// `Debug`/`Display` of [`QbitConfig`], `crate::config::Config` (which
/// embeds a `QbitPassword` field directly), or `qbit::QbitClient` — all
/// derive/implement their `Debug` in terms of this type rather than a bare
/// `String`, so an accidental `{:?}`/`tracing::debug!(config = ?cfg, ...)`
/// on the whole struct redacts it instead of printing the secret.
#[derive(Clone)]
pub struct QbitPassword(String);

impl QbitPassword {
    /// The only way to get the real value back out — call sites that need
    /// to actually send the password (the login POST body) call this
    /// explicitly, which makes every place the secret is exposed
    /// grep-able.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl From<String> for QbitPassword {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl fmt::Debug for QbitPassword {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<redacted>")
    }
}

impl fmt::Display for QbitPassword {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<redacted>")
    }
}

/// qBittorrent WebUI connection details. Assembled from the central
/// `crate::config::Config` via [`crate::config::Config::qbit`] — see the
/// module doc. A plain data holder: no `from_env`/env-reading of its own.
#[derive(Debug, Clone)]
pub struct QbitConfig {
    pub url: String,
    pub user: String,
    pub pass: QbitPassword,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn password_debug_and_display_never_print_the_secret() {
        let pass = QbitPassword::from("hunter2".to_string());
        assert_eq!(format!("{pass:?}"), "<redacted>");
        assert_eq!(format!("{pass}"), "<redacted>");
    }

    #[test]
    fn config_debug_redacts_password_field() {
        let cfg = QbitConfig {
            url: "http://192.0.2.50:8080".to_string(),
            user: "admin".to_string(),
            pass: QbitPassword::from("hunter2".to_string()),
        };
        let debug = format!("{cfg:?}");
        assert!(!debug.contains("hunter2"));
        assert!(debug.contains("<redacted>"));
    }
}
