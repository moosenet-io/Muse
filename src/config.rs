//! Typed configuration loaded from environment variables.
//!
//! Secrets (tokens/keys/URLs with credentials) are never hardcoded — they are
//! materialized into the process environment from <secret-manager> at deploy/runtime
//! (the fleet-standard `.env`-from-<secret-manager> pattern). This module only reads
//! `std::env::var`; it never authors a default secret value.

const DEFAULT_BIND_ADDR: &str = "0.0.0.0:8090";
const DEFAULT_LOG_LEVEL: &str = "info";

/// Muse service configuration, assembled from environment variables at startup.
#[derive(Debug, Clone)]
pub struct Config {
    /// Postgres connection string. Required for real operation; the pool is
    /// built lazily so a missing/unreachable DB never blocks process startup.
    pub database_url: Option<String>,
    /// Address the HTTP server binds to.
    pub bind_addr: String,
    /// Tracing/log level filter.
    pub log_level: String,

    // --- External service placeholders (Phase 0+ integrations) ---
    pub plex_url: Option<String>,
    pub plex_token: Option<String>,
    pub tautulli_url: Option<String>,
    pub tautulli_api_key: Option<String>,
    pub radarr_url: Option<String>,
    pub radarr_api_key: Option<String>,
    pub sonarr_url: Option<String>,
    pub sonarr_api_key: Option<String>,
    pub prowlarr_url: Option<String>,
    pub prowlarr_api_key: Option<String>,
    pub tmdb_api_key: Option<String>,
    pub ollama_url: Option<String>,
    pub chord_url: Option<String>,

    /// MUSE-14: fleet SearXNG instance base URL, used for forum/critic
    /// sentiment + "does it get good" enrichment queries. `None` disables
    /// that enrichment source (graceful degrade, same posture as every
    /// other optional integration in this struct).
    pub searxng_url: Option<String>,
    /// MUSE-14: news-search endpoint base URL, used for renewal/trailer
    /// enrichment queries. `None` disables that enrichment source.
    pub news_url: Option<String>,
    /// MUSE-14: optional bearer API key for [`Config::news_url`]. Many
    /// self-hosted news aggregators need none, so this is independently
    /// optional even when `news_url` is set.
    pub news_api_key: Option<String>,

    /// MUSE-05: raw JSON describing the multi-instance Radarr/Sonarr fleet
    /// (the operator runs 8 *arr instances — 5 Radarr + 3 Sonarr, sharded by
    /// root folder — see `arr::config::ArrInstanceConfig`). Kept as an
    /// unparsed string here (same "config only reads env" discipline as
    /// every other field); [`Config::arr_instances`] parses it lazily so a
    /// malformed value degrades that one feature rather than blocking
    /// startup. Never a literal instance list — always sourced from
    /// `MUSE_ARR_INSTANCES` at runtime (<secret-manager>-materialized).
    pub arr_instances_json: Option<String>,
}

impl Config {
    /// Load configuration from the process environment.
    pub fn from_env() -> Self {
        Self {
            database_url: env_opt("MUSE_DATABASE_URL"),
            bind_addr: std::env::var("MUSE_BIND_ADDR")
                .unwrap_or_else(|_| DEFAULT_BIND_ADDR.to_string()),
            log_level: std::env::var("MUSE_LOG_LEVEL")
                .unwrap_or_else(|_| DEFAULT_LOG_LEVEL.to_string()),

            plex_url: env_opt("PLEX_URL"),
            plex_token: env_opt("PLEX_TOKEN"),
            tautulli_url: env_opt("TAUTULLI_URL"),
            tautulli_api_key: env_opt("TAUTULLI_API_KEY"),
            radarr_url: env_opt("RADARR_URL"),
            radarr_api_key: env_opt("RADARR_API_KEY"),
            sonarr_url: env_opt("SONARR_URL"),
            sonarr_api_key: env_opt("SONARR_API_KEY"),
            prowlarr_url: env_opt("PROWLARR_URL"),
            prowlarr_api_key: env_opt("PROWLARR_API_KEY"),
            tmdb_api_key: env_opt("TMDB_API_KEY"),
            ollama_url: env_opt("MUSE_OLLAMA_URL"),
            chord_url: env_opt("CHORD_URL"),
            searxng_url: env_opt("MUSE_SEARXNG_URL"),
            news_url: env_opt("MUSE_NEWS_URL"),
            news_api_key: env_opt("MUSE_NEWS_API_KEY"),
            arr_instances_json: env_opt("MUSE_ARR_INSTANCES"),
        }
    }

    /// Parse the configured *arr instance fleet (MUSE-05). Returns an empty
    /// list (not an error) when `MUSE_ARR_INSTANCES` is unset — the ingest
    /// routine simply has nothing to do, same graceful-degrade posture as
    /// `PlexClient::from_config`. Returns `Err` only for a genuinely
    /// malformed JSON value, so the caller can decide whether to log and
    /// continue with zero instances or treat it as fatal.
    pub fn arr_instances(
        &self,
    ) -> crate::error::MuseResult<Vec<crate::arr::config::ArrInstanceConfig>> {
        crate::arr::config::load_arr_instances(self.arr_instances_json.as_deref())
    }
}

fn env_opt(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    /// Env vars are process-global, so tests that read/mutate them must not
    /// run concurrently with each other.
    #[test]
    #[serial]
    fn config_parses_with_defaults_when_env_unset() {
        for key in [
            "MUSE_DATABASE_URL",
            "MUSE_BIND_ADDR",
            "MUSE_LOG_LEVEL",
            "PLEX_URL",
            "PLEX_TOKEN",
            "TAUTULLI_URL",
            "TAUTULLI_API_KEY",
            "RADARR_URL",
            "RADARR_API_KEY",
            "SONARR_URL",
            "SONARR_API_KEY",
            "PROWLARR_URL",
            "PROWLARR_API_KEY",
            "TMDB_API_KEY",
            "MUSE_OLLAMA_URL",
            "CHORD_URL",
            "MUSE_SEARXNG_URL",
            "MUSE_NEWS_URL",
            "MUSE_NEWS_API_KEY",
            "MUSE_ARR_INSTANCES",
        ] {
            std::env::remove_var(key);
        }

        let cfg = Config::from_env();

        assert_eq!(cfg.bind_addr, DEFAULT_BIND_ADDR);
        assert_eq!(cfg.log_level, DEFAULT_LOG_LEVEL);
        assert!(cfg.database_url.is_none());
        assert!(cfg.plex_url.is_none());
        assert!(cfg.tmdb_api_key.is_none());
        assert!(cfg.arr_instances_json.is_none());
        assert!(cfg
            .arr_instances()
            .expect("empty instances should parse")
            .is_empty());
    }

    #[test]
    #[serial]
    fn config_reads_overrides_from_env() {
        std::env::set_var("MUSE_BIND_ADDR", "127.0.0.1:9999");
        std::env::set_var("MUSE_LOG_LEVEL", "debug");
        std::env::set_var("MUSE_DATABASE_URL", "postgres://example/muse");

        let cfg = Config::from_env();

        assert_eq!(cfg.bind_addr, "127.0.0.1:9999");
        assert_eq!(cfg.log_level, "debug");
        assert_eq!(cfg.database_url.as_deref(), Some("postgres://example/muse"));

        std::env::remove_var("MUSE_BIND_ADDR");
        std::env::remove_var("MUSE_LOG_LEVEL");
        std::env::remove_var("MUSE_DATABASE_URL");
    }
}
