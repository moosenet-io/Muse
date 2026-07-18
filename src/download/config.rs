//! qBittorrent WebUI connection configuration.
//!
//! Same posture as `snapshot::acquisition::AcquisitionConfig`: a small,
//! self-contained `from_env()` rather than three more fields bolted onto the
//! already-large `crate::config::Config` — this module owns its own env
//! vars. Credentials are **never hardcoded**; they are materialized into the
//! process environment from <secret-manager> at runtime (S1/S7), and
//! [`QbitConfig::from_env`] only ever reads `std::env::var`.

use std::fmt;

const URL_VAR: &str = "MUSE_QBIT_URL";
const USER_VAR: &str = "MUSE_QBIT_USER";
const PASS_VAR: &str = "MUSE_QBIT_PASS";

/// Wraps the qBittorrent WebUI password so it can never leak through a
/// `Debug`/`Display` of [`QbitConfig`] or `qbit::QbitClient` — both derive
/// their `Debug` in terms of this type rather than a bare `String`, so an
/// accidental `{:?}`/`tracing::debug!(config = ?cfg, ...)` on the whole
/// struct redacts it instead of printing the secret.
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

/// qBittorrent WebUI connection details, read from `MUSE_QBIT_URL` /
/// `MUSE_QBIT_USER` / `MUSE_QBIT_PASS`.
#[derive(Debug, Clone)]
pub struct QbitConfig {
    pub url: String,
    pub user: String,
    pub pass: QbitPassword,
}

impl QbitConfig {
    /// Loads from the process environment. Returns `None` when any of the
    /// three vars is unset/empty — qBittorrent control is an optional,
    /// gracefully-degrading dependency (same posture as
    /// `PlexClient::from_config`): callers simply have no live
    /// `DownloadClient` to construct, rather than the process failing to
    /// start.
    pub fn from_env() -> Option<Self> {
        let url = env_opt(URL_VAR)?;
        let user = env_opt(USER_VAR)?;
        let pass = env_opt(PASS_VAR)?;

        Some(Self {
            url,
            user,
            pass: QbitPassword::from(pass),
        })
    }
}

fn env_opt(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    fn clear_env() {
        std::env::remove_var(URL_VAR);
        std::env::remove_var(USER_VAR);
        std::env::remove_var(PASS_VAR);
    }

    #[test]
    #[serial]
    fn from_env_none_when_unset() {
        clear_env();
        assert!(QbitConfig::from_env().is_none());
    }

    #[test]
    #[serial]
    fn from_env_none_when_partially_set() {
        clear_env();
        std::env::set_var(URL_VAR, "http://192.0.2.50:8080");
        std::env::set_var(USER_VAR, "admin");
        // PASS_VAR intentionally left unset.
        assert!(QbitConfig::from_env().is_none());
        clear_env();
    }

    #[test]
    #[serial]
    fn from_env_parses_all_three() {
        clear_env();
        std::env::set_var(URL_VAR, "http://192.0.2.50:8080");
        std::env::set_var(USER_VAR, "admin");
        std::env::set_var(PASS_VAR, "hunter2");

        let cfg = QbitConfig::from_env().expect("all three vars set");
        assert_eq!(cfg.url, "http://192.0.2.50:8080");
        assert_eq!(cfg.user, "admin");
        assert_eq!(cfg.pass.expose(), "hunter2");

        clear_env();
    }

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
