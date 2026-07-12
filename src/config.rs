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
    /// MUSE-07: session-poller cadence in seconds (`MUSE_PLEX_POLL_SECS`).
    /// `None` when unset/unparseable — the poller falls back to its own
    /// default (10s, see `tracker::poller`) rather than this module
    /// authoring a default value.
    pub plex_poll_secs: Option<u64>,
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

    // --- MUSE-17: Prowlarr report-pull worker (behavioral config, not
    // secret-shaped -- no vault involvement, same posture as MUSE_BIND_ADDR
    // above). ---
    /// How often the report-pull worker's background loop wakes up to check
    /// which indexers are due for a poll (spec S4b-B: "on a per-indexer
    /// interval ... never poll faster than this" -- the *indexer's own*
    /// `polite_min_interval_secs` is the real etiquette gate; this is just
    /// the scheduler's check cadence and should be well under the smallest
    /// configured per-indexer interval).
    pub prowlarr_tick_interval_secs: u64,
    /// Newznab parent category ids treated as "movies" for report-pull
    /// (spec S4b-B: "movies 2000s"). Comma-separated in
    /// `MUSE_PROWLARR_MOVIE_CATEGORIES`.
    pub prowlarr_movie_categories: Vec<i32>,
    /// Newznab parent category ids treated as "tv" for report-pull (spec
    /// S4b-B: "tv 5000s"). Comma-separated in `MUSE_PROWLARR_TV_CATEGORIES`.
    pub prowlarr_tv_categories: Vec<i32>,
    /// Minimum `prowlarr::ParsedRelease::confidence` required before the
    /// worker will attempt to resolve a release to an existing
    /// `media_metadata` title. Below this, the release is still stored
    /// (negative-space discovery, spec S4b-B) but `media_metadata_id` stays
    /// NULL rather than risk a wrong match on a poorly-parsed name.
    pub prowlarr_resolve_min_confidence: f32,
    /// How long a rolling `releases` snapshot row stays before
    /// `repo::release::prune_expired` removes it (spec S3.6: "expired rows
    /// are pruned"). Every re-seen release refreshes this on upsert.
    pub release_expiry_days: i64,
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
            plex_poll_secs: env_opt("MUSE_PLEX_POLL_SECS").and_then(|v| v.parse().ok()),
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

            prowlarr_tick_interval_secs: env_u64("MUSE_PROWLARR_TICK_INTERVAL_SECS", 60),
            prowlarr_movie_categories: env_int_list("MUSE_PROWLARR_MOVIE_CATEGORIES", &[2000]),
            prowlarr_tv_categories: env_int_list("MUSE_PROWLARR_TV_CATEGORIES", &[5000]),
            prowlarr_resolve_min_confidence: env_f32("MUSE_PROWLARR_RESOLVE_MIN_CONFIDENCE", 0.5),
            release_expiry_days: env_i64("MUSE_RELEASE_EXPIRY_DAYS", 21),
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

/// Test/scaffold convenience -- NOT used by `from_env`, which always reads
/// every field explicitly. Lets test modules elsewhere in the crate build a
/// `Config` via struct-update syntax (`Config { prowlarr_url, ..Default::default() }`)
/// without having to enumerate every unrelated field.
impl Default for Config {
    fn default() -> Self {
        Self {
            database_url: None,
            bind_addr: DEFAULT_BIND_ADDR.to_string(),
            log_level: DEFAULT_LOG_LEVEL.to_string(),
            plex_url: None,
            plex_token: None,
            tautulli_url: None,
            tautulli_api_key: None,
            radarr_url: None,
            radarr_api_key: None,
            sonarr_url: None,
            sonarr_api_key: None,
            prowlarr_url: None,
            prowlarr_api_key: None,
            tmdb_api_key: None,
            ollama_url: None,
            chord_url: None,
            arr_instances_json: None,
            prowlarr_tick_interval_secs: 60,
            prowlarr_movie_categories: vec![2000],
            prowlarr_tv_categories: vec![5000],
            prowlarr_resolve_min_confidence: 0.5,
            release_expiry_days: 21,
            searxng_url: None,
            news_url: None,
            news_api_key: None,
            plex_poll_secs: None,
        }
    }
}

fn env_opt(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.is_empty())
}

/// Parse a comma-separated list of integers from `key`, falling back to
/// `default` when unset, empty, or fully unparseable. Individual tokens that
/// fail to parse are skipped (not fatal) rather than dropping the whole
/// list -- a single typo'd category id shouldn't take out the rest.
fn env_int_list(key: &str, default: &[i32]) -> Vec<i32> {
    match std::env::var(key) {
        Ok(v) if !v.trim().is_empty() => {
            let parsed: Vec<i32> = v
                .split(',')
                .filter_map(|s| s.trim().parse::<i32>().ok())
                .collect();
            if parsed.is_empty() {
                default.to_vec()
            } else {
                parsed
            }
        }
        _ => default.to_vec(),
    }
}

fn env_u64(key: &str, default: u64) -> u64 {
    env_opt(key)
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(default)
}

fn env_i64(key: &str, default: i64) -> i64 {
    env_opt(key)
        .and_then(|v| v.parse::<i64>().ok())
        .unwrap_or(default)
}

fn env_f32(key: &str, default: f32) -> f32 {
    env_opt(key)
        .and_then(|v| v.parse::<f32>().ok())
        .unwrap_or(default)
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
            "MUSE_PLEX_POLL_SECS",
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
            "MUSE_PROWLARR_TICK_INTERVAL_SECS",
            "MUSE_PROWLARR_MOVIE_CATEGORIES",
            "MUSE_PROWLARR_TV_CATEGORIES",
            "MUSE_PROWLARR_RESOLVE_MIN_CONFIDENCE",
            "MUSE_RELEASE_EXPIRY_DAYS",
        ] {
            std::env::remove_var(key);
        }

        let cfg = Config::from_env();

        assert_eq!(cfg.bind_addr, DEFAULT_BIND_ADDR);
        assert_eq!(cfg.log_level, DEFAULT_LOG_LEVEL);
        assert!(cfg.database_url.is_none());
        assert!(cfg.plex_url.is_none());
        assert!(cfg.plex_poll_secs.is_none());
        assert!(cfg.tmdb_api_key.is_none());
        assert!(cfg.arr_instances_json.is_none());
        assert!(cfg
            .arr_instances()
            .expect("empty instances should parse")
            .is_empty());
        assert_eq!(cfg.prowlarr_tick_interval_secs, 60);
        assert_eq!(cfg.prowlarr_movie_categories, vec![2000]);
        assert_eq!(cfg.prowlarr_tv_categories, vec![5000]);
        assert!((cfg.prowlarr_resolve_min_confidence - 0.5).abs() < f32::EPSILON);
        assert_eq!(cfg.release_expiry_days, 21);
    }

    #[test]
    #[serial]
    fn config_reads_prowlarr_worker_overrides_from_env() {
        std::env::set_var("MUSE_PROWLARR_TICK_INTERVAL_SECS", "30");
        std::env::set_var("MUSE_PROWLARR_MOVIE_CATEGORIES", "2000, 2010,bogus");
        std::env::set_var("MUSE_PROWLARR_TV_CATEGORIES", "5000,5010");
        std::env::set_var("MUSE_PROWLARR_RESOLVE_MIN_CONFIDENCE", "0.75");
        std::env::set_var("MUSE_RELEASE_EXPIRY_DAYS", "7");

        let cfg = Config::from_env();

        assert_eq!(cfg.prowlarr_tick_interval_secs, 30);
        assert_eq!(cfg.prowlarr_movie_categories, vec![2000, 2010]);
        assert_eq!(cfg.prowlarr_tv_categories, vec![5000, 5010]);
        assert!((cfg.prowlarr_resolve_min_confidence - 0.75).abs() < f32::EPSILON);
        assert_eq!(cfg.release_expiry_days, 7);

        for key in [
            "MUSE_PROWLARR_TICK_INTERVAL_SECS",
            "MUSE_PROWLARR_MOVIE_CATEGORIES",
            "MUSE_PROWLARR_TV_CATEGORIES",
            "MUSE_PROWLARR_RESOLVE_MIN_CONFIDENCE",
            "MUSE_RELEASE_EXPIRY_DAYS",
        ] {
            std::env::remove_var(key);
        }
    }

    #[test]
    fn env_int_list_falls_back_to_default_when_all_tokens_unparseable() {
        std::env::set_var("MUSE_TEST_ENV_INT_LIST_ALL_BOGUS", "a,b,c");
        assert_eq!(
            env_int_list("MUSE_TEST_ENV_INT_LIST_ALL_BOGUS", &[42]),
            vec![42]
        );
        std::env::remove_var("MUSE_TEST_ENV_INT_LIST_ALL_BOGUS");
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
