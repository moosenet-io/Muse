//! TheTVDB v4 config, narrowed from the central `crate::config::Config` —
//! same posture as `download::config::QbitConfig`/`Config::qbit`: this
//! module does NOT read the environment itself. `MUSE_TVDB_API_KEY`/
//! `MUSE_TVDB_PIN` are read in exactly one place, `Config::from_env`, and
//! assembled into a [`TvdbConfig`] via [`crate::config::Config::tvdb`].

/// TheTVDB v4 connection configuration.
#[derive(Debug, Clone)]
pub struct TvdbConfig {
    /// TheTVDB v4 API base URL, e.g. `https://api4.thetvdb.com/v4`.
    /// Overridable (`MUSE_TVDB_BASE_URL`) so tests/an on-prem proxy can
    /// point the client elsewhere, same seam `TmdbClient::new(base_url, ..)`
    /// already provides for TMDb.
    pub base_url: String,
    /// TheTVDB v4 API key (`MUSE_TVDB_API_KEY`), the credential `POST
    /// /login` exchanges for a bearer token. Never a literal (S1);
    /// <secret-manager>-materialized at runtime (S7).
    pub api_key: String,
    /// Optional subscriber PIN (`MUSE_TVDB_PIN`) TheTVDB v4's `/login`
    /// accepts alongside `apikey` for subscription-model API keys. Most
    /// standard API keys don't need one — independently optional, same
    /// posture as `Config::news_api_key` being optional alongside
    /// `Config::news_url`.
    pub pin: Option<String>,
}
