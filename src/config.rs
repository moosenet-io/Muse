//! Typed configuration loaded from environment variables.
//!
//! Secrets (tokens/keys/URLs with credentials) are never hardcoded — they are
//! materialized into the process environment from <secret-manager> at deploy/runtime
//! (the fleet-standard `.env`-from-<secret-manager> pattern). This module only reads
//! `std::env::var`; it never authors a default secret value.

const DEFAULT_BIND_ADDR: &str = "0.0.0.0:8090";
const DEFAULT_LOG_LEVEL: &str = "info";
const DEFAULT_HDHR_DEVICE_ID: &str = "MUSE0001";
const DEFAULT_FFMPEG_PATH: &str = "ffmpeg";
/// Empty means "no prefix" — `relative_path`/`file_path` values are used
/// as-is. See [`crate::streaming::ffmpeg::join_media_path`].
const DEFAULT_MEDIA_ROOT: &str = "";

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
    /// MUSEX-09: Jellyfin base URL, used only to config-gate the (currently
    /// unverified-stub) `JellyfinSyncPlay` server-sync-primitive adapter —
    /// see `watch_together::sync`. Same graceful-degrade posture as every
    /// other optional integration in this struct: `None` means the
    /// delegated-sync path simply isn't available, never a hardcoded
    /// fallback URL. Muse has no other Jellyfin footprint yet (per the
    /// MUSEX-01 server-abstraction audit).
    pub jellyfin_url: Option<String>,
    /// MUSEX-09: Jellyfin API key, paired with [`Config::jellyfin_url`].
    pub jellyfin_token: Option<String>,
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

    // --- MUSE-28: linear tuner (HDHomeRun-emulation + M3U + XMLTV) ---
    /// Public LAN base URL Plex/other players use to reach this Muse
    /// instance (e.g. `http://192.0.2.10:8090`), advertised in
    /// `/discover.json`'s `BaseURL`/`LineupURL`, `/muse.m3u`'s stream URLs,
    /// and nowhere else needs to hardcode a host. `None` degrades to
    /// `http://{bind_addr}` (only correct when `bind_addr` is itself a
    /// LAN-reachable address, e.g. not `0.0.0.0`) rather than failing the
    /// tuner routes outright.
    pub public_base_url: Option<String>,
    /// HDHomeRun-emulation device id advertised in `/discover.json`
    /// (`MUSE_HDHR_DEVICE_ID`). Not secret-shaped; a stable identifier Plex
    /// uses to recognize this tuner across restarts.
    pub hdhr_device_id: String,
    /// Rolling linear-guide window, in hours, the director keeps
    /// `channel_programs` filled to and XMLTV renders
    /// (`MUSE_CHANNEL_GUIDE_WINDOW_HOURS`, spec pre-flight: 24-48h).
    pub channel_guide_window_hours: i64,
    /// How often the linear-channel scheduler worker wakes up to top off
    /// the rolling guide window (`MUSE_CHANNEL_SCHEDULER_TICK_SECS`).
    /// Purely a wake cadence, not a secret — same posture as
    /// `prowlarr_tick_interval_secs` above.
    pub channel_scheduler_tick_secs: u64,

    /// MUSE-09: the maximum pgvector cosine distance (`embedding <=> query`,
    /// range 0.0 = identical to 2.0 = opposite) a vector-tier match may have
    /// and still be treated as "confident" by `/query/resolve`'s resolution
    /// ladder. A vector hit above this distance is not trustworthy enough to
    /// answer with — the ladder falls through to the pg_trgm tier instead of
    /// returning a wrong-but-vector-scored guess. Tune via
    /// `MUSE_RECALL_VECTOR_MAX_DISTANCE`; the default (0.4) is a
    /// conservative starting point for `nomic-embed-text` cosine distances
    /// at Phase-0 library sizes, not a value derived from a production
    /// corpus yet.
    pub recall_vector_max_distance: f64,

    // --- MUSE-29: ffmpeg channel streaming engine ---
    /// Path (or bare command name resolved via `$PATH`) to the ffmpeg
    /// binary (`MUSE_FFMPEG_PATH`). Not secret-shaped — a deploy-host detail,
    /// same posture as `MUSE_HDHR_DEVICE_ID`. Defaults to `"ffmpeg"` (rely on
    /// `$PATH`); the streaming handler degrades to a clean 501 rather than
    /// 500 when this binary can't be spawned (see
    /// `crate::streaming::ffmpeg::classify_spawn_error`).
    pub ffmpeg_path: String,
    /// Base filesystem path prepended to `media_files.relative_path` /
    /// `interstitials.file_path` to get an absolute path ffmpeg can open
    /// (`MUSE_MEDIA_ROOT`). Empty string (the default) means "no prefix" —
    /// stored paths are used exactly as-is, which is correct when they're
    /// already absolute (as Radarr/Sonarr-sourced `relative_path` values
    /// often are in this codebase's fixtures) or when the process's cwd is
    /// already the library root.
    pub media_root: String,

    /// MUSE-12: how often the proactive-content generator worker wakes up
    /// to run all five generators for every account
    /// (`MUSE_PROACTIVE_TICK_INTERVAL_SECS`). Purely a wake cadence, not a
    /// secret — same posture as `channel_scheduler_tick_secs` above.
    /// Defaults to hourly: proactive nudges are inherently low-frequency
    /// (cooldown windows measure in days), so there's no benefit to a
    /// tighter loop.
    pub proactive_tick_interval_secs: u64,

    // --- MUSE-31: background maintenance pipeline + on-demand ops routes ---
    /// How often the maintenance worker wakes up to run one full pass (arr
    /// ingest -> embed_stale -> per-account taste/divergence recompute ->
    /// bounded enrichment) — `MUSE_MAINTENANCE_TICK_SECS`. Purely a wake
    /// cadence, not a secret. Defaults to every 30 minutes: this is what
    /// keeps a freshly-deployed Muse self-populating (embeddings/
    /// taste_profile/taste_divergence never had a scheduled caller before
    /// MUSE-31 — see the module doc on `crate::maintenance`).
    pub maintenance_tick_secs: u64,
    /// How often the daily trending/population worker wakes up to run
    /// `trending::snapshot_trending` + `compute_population_distributions`
    /// (`MUSE_TRENDING_TICK_SECS`). Purely a wake cadence. Defaults to
    /// 86400s (once a day) — matches TMDb's own trending page's practical
    /// refresh cadence and keeps this polite/low-volume against TMDb.
    pub trending_tick_secs: u64,
    /// Upper bound on how many `media_item` rows one maintenance pass's
    /// `embed_stale` call will actually embed (`MUSE_EMBED_BATCH_SIZE`).
    /// Bounded so one pass can't turn into an unbounded Ollama burst on a
    /// freshly-ingested large library — subsequent passes make forward
    /// progress on the rest (see `embed::pipeline::embed_stale`'s own
    /// paging/batch docs).
    pub embed_batch_size: usize,
    /// Upper bound on how many gap-analysis candidates the maintenance
    /// pass's enrichment step will attempt per account per pass
    /// (`MUSE_MAINTENANCE_ENRICHMENT_LIMIT`). Bounded for the same reason as
    /// `embed_batch_size` — enrichment calls out to SearXNG/news HTTP
    /// endpoints, so an unbounded pass could turn into a request storm
    /// against a self-hosted instance.
    pub maintenance_enrichment_limit: i64,

    // --- MUSET-07 (Plane TERM #372): adversarial reasoning review ---
    /// Base URL of a configured adversarial reasoning-critique panel
    /// endpoint (`MUSE_REASONING_PANEL_URL`). `None` (the default) keeps
    /// [`crate::taste_review::panel::TerminusReasoningPanel`] uninstantiable
    /// — the whole MUSET-07 feature is inert (no live calls, no startup
    /// impact) unless this is explicitly set. Same graceful/opt-in posture
    /// as `chord_url`/`searxng_url` above; never a literal (S1).
    pub reasoning_panel_url: Option<String>,
    /// Optional bearer credential for [`Config::reasoning_panel_url`],
    /// materialized from <secret-manager> at runtime (S7) — never a literal.
    pub reasoning_panel_api_key: Option<String>,
    /// Model name the reasoning panel should use, when the configured
    /// endpoint is model-selectable (`MUSE_REASONING_PANEL_MODEL`). A model
    /// NAME, not an infra value — same posture as
    /// `taste_model::chord_client::DEFAULT_MODEL`.
    pub reasoning_panel_model: Option<String>,
    /// Base URL of the sanctioned Terminus finding-filing surface MUSET-07
    /// uses to file a taste-quality Plane issue on panel consensus
    /// (`MUSE_TASTE_FINDING_SINK_URL`) — the ONE sanctioned Plane door (S9).
    /// `None` (the default) keeps
    /// [`crate::taste_review::sink::TerminusPlaneFindingSink`]
    /// uninstantiable, same opt-in posture as every other integration here.
    pub taste_finding_sink_url: Option<String>,
    /// Optional bearer credential for [`Config::taste_finding_sink_url`],
    /// materialized from <secret-manager> at runtime (S7) — never a literal.
    pub taste_finding_sink_api_key: Option<String>,
    /// Plane project identifier a filed taste-quality finding is tagged
    /// with (`MUSE_TASTE_FINDING_PLANE_PROJECT`) — deliberately not a
    /// hardcoded literal here (S1): which Plane project owns Muse
    /// taste-quality findings is an operator/deploy decision, not something
    /// this crate should guess at or bake in.
    pub taste_finding_plane_project: Option<String>,

    // --- MUSEX-07 (Plane TERM #383): what's-hot / "the talk" cultural layer ---
    /// Trakt API client id (`TRAKT_CLIENT_ID`), Trakt's required
    /// `trakt-api-key` header credential. `None` (the default) keeps
    /// [`crate::cultural::source::TraktTrendSource`] uninstantiable — the
    /// Trakt half of the cultural layer is entirely inert (no live call, no
    /// startup impact) unless this is explicitly set, same graceful/opt-in
    /// posture as `Config::tmdb_api_key`/`Config::chord_url`. Never a
    /// literal (S1); materialized from <secret-manager> at runtime (S7).
    pub trakt_client_id: Option<String>,
    /// Optional Trakt OAuth bearer token (`TRAKT_API_KEY`) for endpoints
    /// that need user-level auth. The trending/talk pulls this module
    /// actually uses are Trakt's PUBLIC endpoints (no user context), so
    /// this is independently optional even when `trakt_client_id` is set —
    /// same posture as `Config::news_api_key` being optional alongside
    /// `Config::news_url`. Never a literal (S1/S7).
    pub trakt_api_key: Option<String>,
    /// Trakt API base URL override (`MUSE_TRAKT_BASE_URL`). `None` — the
    /// default — means `crate::cultural::source::TraktTrendSource::from_config`
    /// uses Trakt's public API host (`TRAKT_DEFAULT_BASE_URL`). Exists so a
    /// test (httpmock server) or an on-prem Trakt proxy can point the client
    /// elsewhere without recompiling — the exact same override seam
    /// `TmdbClient::new(base_url, ..)` already provides for TMDb. Not
    /// secret-shaped (a host, not a credential); still read from env at
    /// runtime, never a literal here.
    pub trakt_base_url: Option<String>,
    /// How long a [`crate::cultural::cache::TrendCache`] pull stays fresh
    /// before the next call re-hits the configured `TrendSource`
    /// (`MUSE_TREND_CACHE_TTL_SECS`) — the rate-limit-respecting cache the
    /// AC requires. Defaults to an hour: trending/talk data doesn't move
    /// fast enough to justify a tighter loop, and this keeps repeated
    /// `/cultural/*` requests from hammering TMDb/Trakt.
    pub trend_cache_ttl_secs: u64,

    /// MUSEX-13: Discord bot API token (`DISCORD_BOT_TOKEN`). Same posture
    /// as every other credential in this struct — materialized into the
    /// process environment from <secret-manager> at runtime, never a literal in
    /// source/config (S1/S7). `None` means the Discord bot is INERT: no
    /// live Discord API call is ever made, and
    /// `discord::client::RealDiscordClient::from_config` returns `None` —
    /// the bot surface degrades to unavailable rather than blocking
    /// startup, same graceful-degrade posture as `plex_token`/
    /// `tmdb_api_key`/every other optional integration here.
    pub discord_bot_token: Option<String>,
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
            jellyfin_url: env_opt("JELLYFIN_URL"),
            jellyfin_token: env_opt("JELLYFIN_TOKEN"),
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

            public_base_url: env_opt("MUSE_PUBLIC_URL"),
            hdhr_device_id: std::env::var("MUSE_HDHR_DEVICE_ID")
                .ok()
                .filter(|v| !v.is_empty())
                .unwrap_or_else(|| DEFAULT_HDHR_DEVICE_ID.to_string()),
            channel_guide_window_hours: env_i64("MUSE_CHANNEL_GUIDE_WINDOW_HOURS", 48),
            channel_scheduler_tick_secs: env_u64("MUSE_CHANNEL_SCHEDULER_TICK_SECS", 900),
            recall_vector_max_distance: env_opt("MUSE_RECALL_VECTOR_MAX_DISTANCE")
                .and_then(|v| v.parse::<f64>().ok())
                .unwrap_or(0.4),

            ffmpeg_path: std::env::var("MUSE_FFMPEG_PATH")
                .ok()
                .filter(|v| !v.is_empty())
                .unwrap_or_else(|| DEFAULT_FFMPEG_PATH.to_string()),
            media_root: std::env::var("MUSE_MEDIA_ROOT")
                .ok()
                .unwrap_or_else(|| DEFAULT_MEDIA_ROOT.to_string()),

            proactive_tick_interval_secs: env_u64("MUSE_PROACTIVE_TICK_INTERVAL_SECS", 3600),

            maintenance_tick_secs: env_u64("MUSE_MAINTENANCE_TICK_SECS", 1800),
            trending_tick_secs: env_u64("MUSE_TRENDING_TICK_SECS", 86400),
            embed_batch_size: env_u64("MUSE_EMBED_BATCH_SIZE", 50) as usize,
            maintenance_enrichment_limit: env_i64("MUSE_MAINTENANCE_ENRICHMENT_LIMIT", 10),

            reasoning_panel_url: env_opt("MUSE_REASONING_PANEL_URL"),
            reasoning_panel_api_key: env_opt("MUSE_REASONING_PANEL_API_KEY"),
            reasoning_panel_model: env_opt("MUSE_REASONING_PANEL_MODEL"),
            taste_finding_sink_url: env_opt("MUSE_TASTE_FINDING_SINK_URL"),
            taste_finding_sink_api_key: env_opt("MUSE_TASTE_FINDING_SINK_API_KEY"),
            taste_finding_plane_project: env_opt("MUSE_TASTE_FINDING_PLANE_PROJECT"),

            trakt_client_id: env_opt("TRAKT_CLIENT_ID"),
            trakt_api_key: env_opt("TRAKT_API_KEY"),
            trakt_base_url: env_opt("MUSE_TRAKT_BASE_URL"),
            trend_cache_ttl_secs: env_u64("MUSE_TREND_CACHE_TTL_SECS", 3600),

            discord_bot_token: env_opt("DISCORD_BOT_TOKEN"),
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
            jellyfin_url: None,
            jellyfin_token: None,
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
            public_base_url: None,
            hdhr_device_id: DEFAULT_HDHR_DEVICE_ID.to_string(),
            channel_guide_window_hours: 48,
            channel_scheduler_tick_secs: 900,
            recall_vector_max_distance: 0.4,
            ffmpeg_path: DEFAULT_FFMPEG_PATH.to_string(),
            media_root: DEFAULT_MEDIA_ROOT.to_string(),
            proactive_tick_interval_secs: 3600,
            maintenance_tick_secs: 1800,
            trending_tick_secs: 86400,
            embed_batch_size: 50,
            maintenance_enrichment_limit: 10,
            reasoning_panel_url: None,
            reasoning_panel_api_key: None,
            reasoning_panel_model: None,
            taste_finding_sink_url: None,
            taste_finding_sink_api_key: None,
            taste_finding_plane_project: None,
            trakt_client_id: None,
            trakt_api_key: None,
            trakt_base_url: None,
            trend_cache_ttl_secs: 3600,
            discord_bot_token: None,
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
            "MUSE_PUBLIC_URL",
            "MUSE_HDHR_DEVICE_ID",
            "MUSE_CHANNEL_GUIDE_WINDOW_HOURS",
            "MUSE_CHANNEL_SCHEDULER_TICK_SECS",
            "MUSE_RECALL_VECTOR_MAX_DISTANCE",
            "MUSE_FFMPEG_PATH",
            "MUSE_MEDIA_ROOT",
            "MUSE_PROACTIVE_TICK_INTERVAL_SECS",
            "MUSE_MAINTENANCE_TICK_SECS",
            "MUSE_TRENDING_TICK_SECS",
            "MUSE_EMBED_BATCH_SIZE",
            "MUSE_MAINTENANCE_ENRICHMENT_LIMIT",
            "MUSE_REASONING_PANEL_URL",
            "MUSE_REASONING_PANEL_API_KEY",
            "MUSE_REASONING_PANEL_MODEL",
            "MUSE_TASTE_FINDING_SINK_URL",
            "MUSE_TASTE_FINDING_SINK_API_KEY",
            "MUSE_TASTE_FINDING_PLANE_PROJECT",
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
        assert!(cfg.public_base_url.is_none());
        assert_eq!(cfg.hdhr_device_id, DEFAULT_HDHR_DEVICE_ID);
        assert_eq!(cfg.channel_guide_window_hours, 48);
        assert_eq!(cfg.channel_scheduler_tick_secs, 900);
        assert!((cfg.recall_vector_max_distance - 0.4).abs() < f64::EPSILON);
        assert_eq!(cfg.ffmpeg_path, DEFAULT_FFMPEG_PATH);
        assert_eq!(cfg.media_root, DEFAULT_MEDIA_ROOT);
        assert_eq!(cfg.proactive_tick_interval_secs, 3600);
        assert_eq!(cfg.maintenance_tick_secs, 1800);
        assert_eq!(cfg.trending_tick_secs, 86400);
        assert_eq!(cfg.embed_batch_size, 50);
        assert_eq!(cfg.maintenance_enrichment_limit, 10);
        assert!(cfg.reasoning_panel_url.is_none());
        assert!(cfg.reasoning_panel_api_key.is_none());
        assert!(cfg.reasoning_panel_model.is_none());
        assert!(cfg.taste_finding_sink_url.is_none());
        assert!(cfg.taste_finding_sink_api_key.is_none());
        assert!(cfg.taste_finding_plane_project.is_none());
    }

    #[test]
    #[serial]
    fn config_reads_muset07_reasoning_review_overrides_from_env() {
        std::env::set_var("MUSE_REASONING_PANEL_URL", "http://192.0.2.30:8300");
        std::env::set_var("MUSE_REASONING_PANEL_API_KEY", "test-panel-key");
        std::env::set_var("MUSE_REASONING_PANEL_MODEL", "qwen3-coder:30b");
        std::env::set_var("MUSE_TASTE_FINDING_SINK_URL", "http://192.0.2.30:8310");
        std::env::set_var("MUSE_TASTE_FINDING_SINK_API_KEY", "test-sink-key");
        std::env::set_var("MUSE_TASTE_FINDING_PLANE_PROJECT", "TESTPROJ");

        let cfg = Config::from_env();

        assert_eq!(
            cfg.reasoning_panel_url.as_deref(),
            Some("http://192.0.2.30:8300")
        );
        assert_eq!(
            cfg.reasoning_panel_api_key.as_deref(),
            Some("test-panel-key")
        );
        assert_eq!(
            cfg.reasoning_panel_model.as_deref(),
            Some("qwen3-coder:30b")
        );
        assert_eq!(
            cfg.taste_finding_sink_url.as_deref(),
            Some("http://192.0.2.30:8310")
        );
        assert_eq!(
            cfg.taste_finding_sink_api_key.as_deref(),
            Some("test-sink-key")
        );
        assert_eq!(cfg.taste_finding_plane_project.as_deref(), Some("TESTPROJ"));

        for key in [
            "MUSE_REASONING_PANEL_URL",
            "MUSE_REASONING_PANEL_API_KEY",
            "MUSE_REASONING_PANEL_MODEL",
            "MUSE_TASTE_FINDING_SINK_URL",
            "MUSE_TASTE_FINDING_SINK_API_KEY",
            "MUSE_TASTE_FINDING_PLANE_PROJECT",
        ] {
            std::env::remove_var(key);
        }
    }

    #[test]
    #[serial]
    fn config_reads_maintenance_overrides_from_env() {
        std::env::set_var("MUSE_MAINTENANCE_TICK_SECS", "60");
        std::env::set_var("MUSE_TRENDING_TICK_SECS", "3600");
        std::env::set_var("MUSE_EMBED_BATCH_SIZE", "5");
        std::env::set_var("MUSE_MAINTENANCE_ENRICHMENT_LIMIT", "2");

        let cfg = Config::from_env();

        assert_eq!(cfg.maintenance_tick_secs, 60);
        assert_eq!(cfg.trending_tick_secs, 3600);
        assert_eq!(cfg.embed_batch_size, 5);
        assert_eq!(cfg.maintenance_enrichment_limit, 2);

        for key in [
            "MUSE_MAINTENANCE_TICK_SECS",
            "MUSE_TRENDING_TICK_SECS",
            "MUSE_EMBED_BATCH_SIZE",
            "MUSE_MAINTENANCE_ENRICHMENT_LIMIT",
        ] {
            std::env::remove_var(key);
        }
    }

    #[test]
    #[serial]
    fn config_reads_proactive_tick_interval_override_from_env() {
        std::env::set_var("MUSE_PROACTIVE_TICK_INTERVAL_SECS", "120");
        let cfg = Config::from_env();
        assert_eq!(cfg.proactive_tick_interval_secs, 120);
        std::env::remove_var("MUSE_PROACTIVE_TICK_INTERVAL_SECS");
    }

    #[test]
    #[serial]
    fn config_reads_streaming_overrides_from_env() {
        std::env::set_var("MUSE_FFMPEG_PATH", "/opt/ffmpeg/bin/ffmpeg");
        std::env::set_var("MUSE_MEDIA_ROOT", "/srv/media");

        let cfg = Config::from_env();

        assert_eq!(cfg.ffmpeg_path, "/opt/ffmpeg/bin/ffmpeg");
        assert_eq!(cfg.media_root, "/srv/media");

        std::env::remove_var("MUSE_FFMPEG_PATH");
        std::env::remove_var("MUSE_MEDIA_ROOT");
    }

    #[test]
    #[serial]
    fn config_reads_tuner_overrides_from_env() {
        std::env::set_var("MUSE_PUBLIC_URL", "http://192.0.2.10:8090");
        std::env::set_var("MUSE_HDHR_DEVICE_ID", "MUSETEST1");
        std::env::set_var("MUSE_CHANNEL_GUIDE_WINDOW_HOURS", "24");
        std::env::set_var("MUSE_CHANNEL_SCHEDULER_TICK_SECS", "60");

        let cfg = Config::from_env();

        assert_eq!(
            cfg.public_base_url.as_deref(),
            Some("http://192.0.2.10:8090")
        );
        assert_eq!(cfg.hdhr_device_id, "MUSETEST1");
        assert_eq!(cfg.channel_guide_window_hours, 24);
        assert_eq!(cfg.channel_scheduler_tick_secs, 60);

        for key in [
            "MUSE_PUBLIC_URL",
            "MUSE_HDHR_DEVICE_ID",
            "MUSE_CHANNEL_GUIDE_WINDOW_HOURS",
            "MUSE_CHANNEL_SCHEDULER_TICK_SECS",
        ] {
            std::env::remove_var(key);
        }
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
