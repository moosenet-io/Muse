//! Typed configuration loaded from environment variables.
//!
//! Secrets (tokens/keys/URLs with credentials) are never hardcoded — they are
//! materialized into the process environment from <secret-manager> at deploy/runtime
//! (the fleet-standard `.env`-from-<secret-manager> pattern). This module only reads
//! `std::env::var`; it never authors a default secret value.
//!
//! This is the **one, central** place `std::env::var` is read for
//! secret-shaped values (tokens/keys/passwords) in this crate (S1/S3) — Muse
//! has no `SecretManager`/vault crate of its own, so "route secrets through
//! config, not a scattered `std::env::var`" means routing them through
//! *this* module's [`Config::from_env`], same as `api_token`/`plex_token`/
//! every other credential field below. A module that needs a credential
//! (e.g. `download::qbit::QbitClient`) takes it from a already-loaded
//! [`Config`] (see [`Config::qbit`]) rather than reading its own env var.

use crate::download::config::{QbitConfig, QbitPassword};

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
    /// AMETA-1 (key-less metadata): base URL of the Radarr public TMDb
    /// metadata proxy (`MUSE_TMDB_METADATA_URL`, default
    /// `https://api.radarr.video/v1`). This is what `TmdbClient` points at
    /// in proxy mode — the same key-less proxy Radarr itself uses so a user
    /// never has to register a TMDb API key. Not secret-shaped (a public
    /// host, no auth); overridable purely as a test/on-prem seam. Only used
    /// when `tmdb_api_key` is unset and `metadata_keyless` is true.
    pub tmdb_metadata_base_url: Option<String>,
    pub ollama_url: Option<String>,
    pub chord_url: Option<String>,
    /// S125: base URL of Chord's standardized `/v1/embeddings` endpoint
    /// (`CHORD_EMBEDDINGS_URL`). `None` (the default) makes
    /// [`crate::embed::ChordEmbedClient::from_config`] fall back to
    /// [`Self::chord_url`] (same Chord proxy) — this exists purely as an
    /// explicit override seam for a deployment that fronts embeddings on a
    /// different host/port than chat. Not secret-shaped (a host, not a
    /// credential); never a literal here (S1), materialized at runtime.
    pub chord_embeddings_url: Option<String>,
    /// S125: bearer credential (JWT) for Chord's HTTP surface
    /// (`CHORD_API_TOKEN`), <secret-manager>-materialized at runtime, never a
    /// literal (S1/S7). REQUIRED for the embeddings path: Chord's
    /// `/v1/embeddings` is JWT-gated (a tokenless POST 401s), so
    /// [`crate::embed::ChordEmbedClient::from_config`] refuses to build a
    /// client (logs an error, embeddings disabled) when a Chord URL is set
    /// but this is missing — never posts unauthenticated. NOTE: the sibling
    /// chat/vision `taste_model::chord_client::ChordClient` does not yet read
    /// this (chat wasn't observed to be JWT-gated); if/when it is, wire this
    /// into that client too.
    pub chord_api_token: Option<String>,

    // --- MUSEL-A1: TheTVDB v4 metadata provider (`metadata::tvdb::TvdbClient`).
    // All three read together; see `Config::tvdb`. ---
    /// TheTVDB v4 API key (`MUSE_TVDB_API_KEY`), <secret-manager>-materialized at
    /// runtime, never a literal (S1/S7). `None` means the TVDB metadata
    /// provider is unconfigured/inert — `TvdbClient::from_config` returns
    /// `None`, same graceful-degrade posture as `Config::tmdb_api_key`.
    /// Wrapped in `QbitPassword` (S7 review finding) so a stray
    /// `{:?}`/`tracing::debug!(config = ?cfg, ..)` on this `Config` can't
    /// print the real key — same posture `qbit_pass` below already has.
    /// (`tmdb_api_key`/`plex_token` above remain plain `Option<String>` —
    /// a pre-existing gap this fix does not touch; see the MUSEL-A1
    /// worktree report.)
    pub tvdb_api_key: Option<QbitPassword>,
    /// Optional TheTVDB v4 subscriber PIN (`MUSE_TVDB_PIN`), paired with
    /// `tvdb_api_key` for subscription-model keys. Independently optional —
    /// most standard API keys don't need one. Also secret-shaped, also
    /// wrapped.
    pub tvdb_pin: Option<QbitPassword>,
    /// TheTVDB v4 API base URL override (`MUSE_TVDB_BASE_URL`). `None` (the
    /// default) means `metadata::tvdb::TvdbClient::from_config` uses
    /// TheTVDB's real host. Not secret-shaped — a host, not a credential;
    /// exists so a test/on-prem proxy can point the client elsewhere, same
    /// seam `Config::trakt_base_url` provides for Trakt.
    pub tvdb_base_url: Option<String>,

    /// AMETA-1 (key-less metadata): base URL of the Sonarr Skyhook public
    /// TVDB metadata proxy (`MUSE_SKYHOOK_URL`, default
    /// `https://skyhook.sonarr.tv/v1/tvdb`). `TvdbClient` points at this in
    /// Skyhook (key-less) mode — the unauthenticated proxy Sonarr itself uses
    /// so a user never registers a TheTVDB API key (and there's no `/login`
    /// bearer dance). Not secret-shaped (a public host, no auth); overridable
    /// as a test/on-prem seam. Only used when `tvdb_api_key` is unset and
    /// `metadata_keyless` is true.
    pub skyhook_base_url: Option<String>,

    /// AMETA-1 (key-less metadata): master switch (`MUSE_METADATA_KEYLESS`,
    /// default **true**). When true and no raw provider key is set, the
    /// metadata clients build in key-less *proxy* mode (Radarr/Skyhook
    /// public proxies) instead of returning `None` — so a fresh deploy gets
    /// posters/genres/overview/runtime enrichment with ZERO operator key
    /// setup. Set to `0`/`false` to restore the old "no key ⇒ no provider"
    /// behavior. A raw `TMDB_API_KEY`/`MUSE_TVDB_API_KEY` always takes
    /// precedence over the proxy regardless of this flag.
    pub metadata_keyless: bool,

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

    // --- MUSEM-02: qBittorrent download-client adapter
    // (`download::qbit::QbitClient`). All three read together; see
    // `Config::qbit`. ---
    /// qBittorrent WebUI base URL, e.g. `http://192.0.2.60:8080`.
    /// `MUSE_QBIT_URL`.
    pub qbit_url: Option<String>,
    /// qBittorrent WebUI username. `MUSE_QBIT_USER`.
    pub qbit_user: Option<String>,
    /// qBittorrent WebUI password, <secret-manager>-materialized at runtime, never
    /// a literal (S1/S7). Wrapped in [`QbitPassword`] so it can never leak
    /// through a `Debug`/`Display` of `Config` itself (this struct derives
    /// `Debug`) — `MUSE_QBIT_PASS`.
    pub qbit_pass: Option<QbitPassword>,

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
    /// MUSEM-03: the rolling hourly cap on on-demand targeted searches
    /// (`prowlarr::search::search_releases`), passed through to
    /// `ProwlarrClient::targeted_search`'s `max_searches_per_hour`. Shares
    /// the client's single `RateLimiter` instance with the report-pull
    /// path, so this budgets on-demand search specifically -- "sparingly,
    /// never fan a text search across all private indexers on a whim"
    /// (blueprint §4b-C). `MUSE_PROWLARR_SEARCH_MAX_PER_HOUR`.
    pub prowlarr_search_max_per_hour: u64,

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

    /// MUSEL-B1: filesystem root of the read-only library scan
    /// (`MUSE_LIBRARY_ROOT`) — a READ-ONLY mount of the media library (the
    /// QNAP share, per the spec's MUSEL-B0 ops prerequisite). `None` (the
    /// default, unset) makes the scanner an inert clean no-op — see
    /// `crate::library::scan::run_scan`. Deliberately `Option<String>`
    /// rather than the empty-string-means-unset convention `media_root`
    /// uses above: an empty root would be nonsensical to `fs::read_dir`
    /// (unlike `media_root`, which is a path *prefix* where "" is a valid
    /// no-op prefix), so "unset" needs to be an unambiguous `None`, not an
    /// empty string that could also arise from an accidentally-blank env
    /// var.
    pub library_root: Option<String>,

    // --- MUSEF-01: Foundry (media formatting) ---
    // All non-secret behavioral settings. Foundry's later credentials
    // (OPENSUBTITLES_API_KEY in MUSEF-16, MUSE_FOUNDRY_NODE_TOKEN in
    // MUSEF-11) are secret-shaped and will be wrapped in the crate's
    // redacting password type, not added here as bare String.
    /// Default-deny allowlist of roots Foundry may address, `:`-separated.
    /// Unset/empty (the default) means Foundry does not register at all —
    /// an operator who configures nothing gets no Foundry, not a Foundry
    /// pointed at their library.
    pub foundry_allowed_roots: Option<String>,
    /// Scratch dir for transcode output, staged before verify-and-swap.
    /// Should live on a different device from any allowed root.
    pub foundry_work_dir: Option<String>,
    /// Wall-clock ceiling on one production encode, in seconds. Default 6h,
    /// clamped 60s..24h. See `FoundryConfig::encode_timeout`.
    pub foundry_encode_timeout_secs: Option<u64>,
    /// The mutation kill-switch. **Defaults false**: with it closed Foundry
    /// probes, plans and reports but cannot modify a byte.
    pub foundry_enable_mutation: bool,
    /// Retention for superseded originals in the Foundry recycle bin.
    /// `None` lets `foundry::config` apply its own default rather than this
    /// module authoring one.
    pub foundry_retention_days: Option<u32>,
    /// `ffprobe` binary (a `PATH` name or an absolute path).
    pub foundry_ffprobe_bin: Option<String>,
    /// `HandBrakeCLI` binary (a `PATH` name or an absolute path).
    pub foundry_handbrake_bin: Option<String>,

    // --- SUBS-01: the subtitle system ---
    /// Wyzie subtitle-provider API key (`WYZIE_KEY`).
    ///
    /// Wrapped in `QbitPassword` (S7) so a stray `{:?}` on this `Config` — or
    /// on anything holding it — cannot print the credential; it is also
    /// attached to requests through `reqwest`'s query builder and never
    /// interpolated into a URL that could reach a log or an error body (see
    /// `subtitles::wyzie`). Materialized into the process environment from
    /// <secret-manager> at runtime, never a literal in source (S1/S7).
    ///
    /// `None` (the default) means the PROVIDER tier of the subtitle system is
    /// unavailable — embedded and sidecar discovery are unaffected, and the
    /// API reports "provider not configured" rather than "no subtitles
    /// found". Same graceful-degrade posture as `tmdb_api_key`.
    pub wyzie_key: Option<QbitPassword>,
    /// Wyzie base URL override (`MUSE_WYZIE_BASE_URL`). `None` uses
    /// `subtitles::wyzie::DEFAULT_BASE_URL`. Not secret-shaped (a host, not a
    /// credential) — exists so a test (httpmock) or a proxy can point the
    /// client elsewhere, the same seam `tvdb_base_url`/`trakt_base_url`
    /// provide.
    pub wyzie_base_url: Option<String>,
    /// Where Muse writes subtitles it fetched or re-timed
    /// (`MUSE_SUBTITLE_STORE_DIR`).
    ///
    /// Deliberately a SEPARATE directory from the library rather than a
    /// sidecar written beside the media file. Two reasons: the library root is
    /// treated as read-only everywhere else in this crate
    /// (`library::sidecar`, `library::scan`), and an adjusted subtitle must
    /// never be able to overwrite an original the operator may want back.
    /// `None` means Muse cannot persist a fetched or adjusted subtitle and
    /// says so, rather than falling back to writing into the library.
    pub subtitle_store_dir: Option<String>,

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

    // --- MUSEM-06: monitored "wanted" acquisition worker ---
    /// Minimum time between two on-demand searches for the SAME
    /// `monitored_items` row (`MUSE_WANTED_SEARCH_COOLDOWN_SECS`) — the
    /// cooldown [`crate::acquisition::worker::run_wanted_pass`] checks
    /// against `monitored_items.last_search_at` before re-searching an
    /// item, so a still-below-cutoff item doesn't get re-searched every
    /// single maintenance tick. Defaults to 6 hours.
    pub wanted_search_cooldown_secs: i64,
    /// Upper bound on how many NEW grabs one `run_wanted_pass` call will
    /// make (`MUSE_WANTED_MAX_GRABS_PER_PASS`) — bounded for the same
    /// reason `embed_batch_size`/`maintenance_enrichment_limit` are: a
    /// freshly-populated wanted list (e.g. right after a big arr ingest)
    /// must never turn into an unbounded qBittorrent/Prowlarr burst in one
    /// pass. Subsequent passes make forward progress on the rest.
    pub wanted_max_grabs_per_pass: usize,
    /// Upper bound on how many on-demand Prowlarr searches one
    /// `run_wanted_pass` call will issue (`MUSE_WANTED_MAX_SEARCHES_PER_PASS`)
    /// — a second, independent cap from `wanted_max_grabs_per_pass`: even a
    /// pass that never grabs (every candidate rejected) must still stay
    /// polite to Prowlarr. This is on top of, never a replacement for, the
    /// shared `ProwlarrClient` `RateLimiter`'s own hourly cap
    /// (`prowlarr_search_max_per_hour`) that every search — this worker's
    /// included — already funnels through.
    pub wanted_max_searches_per_pass: usize,

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

    // --- MUSEX-14 (Plane TERM #390): taste-targeted promotion + conversational requests ---
    /// Minimum [`crate::promotion::cosine_similarity_01`] score (`[0.0, 1.0]`)
    /// a newly-available title must reach against an opted-in friend's taste
    /// centroid before [`crate::promotion::targeting::promote_new_title`]
    /// targets that friend (`MUSE_PROMOTION_MATCH_THRESHOLD`). GUI-tunable
    /// per the AC — never a bare literal in `promotion`, always threaded
    /// through here. Not secret-shaped, same posture as
    /// `recall_vector_max_distance`. Defaults to `0.55`: a deliberately
    /// permissive starting point (this is a NEW, unreviewed threshold — no
    /// production corpus has tuned it yet, same caveat
    /// `recall_vector_max_distance`'s own doc makes for its default).
    pub promotion_match_threshold: f64,
    /// How often (seconds) a promotion sweep is intended to run
    /// (`MUSE_PROMOTION_CADENCE_SECS`) — GUI-tunable per the AC, same wake-
    /// cadence posture as `maintenance_tick_secs`/`trending_tick_secs`.
    /// **Not yet wired to a scheduled worker** — MUSEX-14 ships the
    /// targeting/dispatch logic and this tunable, but a periodic driver
    /// that calls `promotion::targeting::promote_new_title` on every newly-
    /// landed title is reserved for a follow-up item, same explicit
    /// "accepted but not yet incorporated" posture
    /// `curation::recommend::RecommendRequest::context` documents for
    /// itself. Defaults to 6 hours (21600s).
    pub promotion_cadence_secs: u64,
    /// Whether [`crate::arr::request::classify_tier`] may ever return
    /// [`crate::arr::request::RequestTier::AutoApprovable`] for a
    /// conversational missing-title request
    /// (`MUSE_ARR_REQUEST_AUTO_TIER_ENABLED`). Defaults to `false` — the
    /// conservative, safe default: every missing-title request needs manual
    /// review until an operator explicitly opts in. This flag can never
    /// bypass the read-only *arr posture `crate::arr`'s own module doc
    /// establishes ("Never write to *arr" — S96 §1): even when `true`, the
    /// only thing that changes is which [`crate::arr::request::RequestTier`]
    /// a request is classified into, never whether `crate::arr` itself
    /// gains a live write call.
    pub arr_request_auto_tier_enabled: bool,

    // --- MUSEX-15 (Plane TERM #391): premiere events + engagement tiers ---
    /// How often (seconds) a premiere-announce sweep is intended to run
    /// (`MUSE_PREMIERE_ANNOUNCE_CADENCE_SECS`) — GUI-tunable per the AC,
    /// same "not yet wired to a scheduled worker" posture
    /// `Config::promotion_cadence_secs` documents for itself: MUSEX-15 ships
    /// `premiere::schedule`/`premiere::discussion`/`premiere::engagement`
    /// and this tunable, but a periodic driver is a separate, follow-up
    /// item. Defaults to a week (604800s) — premieres are inherently
    /// low-frequency events.
    pub premiere_announce_cadence_secs: u64,
    /// Weight `[0.0, 1.0]` `premiere::engagement::compute_tier` gives a
    /// friend's own watch-through rate in the composite engagement score
    /// (`MUSE_PREMIERE_ENGAGEMENT_WATCH_THROUGH_WEIGHT`). Defaults to 0.5 —
    /// an even split with `premiere_engagement_household_love_weight`, a
    /// deliberately unopinionated starting point (no production corpus has
    /// tuned these yet, same caveat `promotion_match_threshold`'s own doc
    /// makes for its default).
    pub premiere_engagement_watch_through_weight: f64,
    /// Weight `[0.0, 1.0]` `premiere::engagement::compute_tier` gives the
    /// household-loved rate in the composite engagement score
    /// (`MUSE_PREMIERE_ENGAGEMENT_HOUSEHOLD_LOVE_WEIGHT`). Defaults to 0.5.
    pub premiere_engagement_household_love_weight: f64,
    /// Composite engagement score `[0.0, 1.0]` at/above which a friend earns
    /// [`crate::premiere::engagement::EngagementTier::Trusted`]
    /// (`MUSE_PREMIERE_ENGAGEMENT_TRUSTED_THRESHOLD`). Defaults to 0.4.
    pub premiere_engagement_trusted_threshold: f64,
    /// Composite engagement score `[0.0, 1.0]` at/above which a friend earns
    /// [`crate::premiere::engagement::EngagementTier::Curator`]
    /// (`MUSE_PREMIERE_ENGAGEMENT_CURATOR_THRESHOLD`). Defaults to 0.7.
    pub premiere_engagement_curator_threshold: f64,
    /// The `ratings.rating` value (0-10 scale, see
    /// `migrations/0017_watch_stats_ratings_watchlist.sql`) at/above which a
    /// household rating counts as "loved" for
    /// [`crate::premiere::engagement::gather_engagement_counts`]
    /// (`MUSE_PREMIERE_LOVED_RATING_THRESHOLD`). Defaults to 7.0.
    pub premiere_loved_rating_threshold: f32,
    /// Request budget (per tracking window) a
    /// [`crate::premiere::engagement::EngagementTier::Starter`] friend earns
    /// (`MUSE_PREMIERE_STARTER_BUDGET`). Defaults to 1 — a friend with no
    /// track record yet gets minimal, not zero, headroom.
    pub premiere_starter_budget: u32,
    /// Request budget a
    /// [`crate::premiere::engagement::EngagementTier::Trusted`] friend earns
    /// (`MUSE_PREMIERE_TRUSTED_BUDGET`). Defaults to 3.
    pub premiere_trusted_budget: u32,
    /// Request budget a
    /// [`crate::premiere::engagement::EngagementTier::Curator`] friend earns
    /// (`MUSE_PREMIERE_CURATOR_BUDGET`). Defaults to 6.
    pub premiere_curator_budget: u32,

    // --- MUSEX-16 (Plane TERM #392): watch-history / group-dynamics KG ---
    /// Minimum cosine similarity `[-1.0, 1.0]` between two opted-in
    /// friends' persona centroids for
    /// [`crate::kg::assemble::assemble_shared_graph`] to emit a
    /// `TasteEdge` between them (`MUSE_KG_TASTE_NEIGHBOR_THRESHOLD`).
    /// GUI-tunable per the AC — never a bare literal in `crate::kg`, always
    /// threaded through here, same posture as
    /// `Config::promotion_match_threshold`. Defaults to `0.5`: a
    /// deliberately unopinionated starting point (no production corpus has
    /// tuned this yet, same caveat `promotion_match_threshold`'s own doc
    /// makes for its default).
    pub kg_taste_neighbor_threshold: f32,

    // --- MUSEX-17 (Plane TERM #393): graph-visualization endpoints ---
    /// Max number of entries `crate::kg::viz::build_watch_history` returns
    /// (`MUSE_KG_VIZ_WATCH_HISTORY_LIMIT`) — the most-recent-first cap on an
    /// otherwise-unbounded temporal series, so a long-lived household's
    /// watch history can't blow up the response payload. GUI-tunable per
    /// the AC — never a bare literal in `crate::kg::viz`/`crate::web::graph`,
    /// same posture as `Config::kg_taste_neighbor_threshold`. Defaults to
    /// 200: a deliberately unopinionated starting point (no production
    /// corpus has tuned this yet, same caveat `kg_taste_neighbor_threshold`'s
    /// own doc makes for its default).
    pub kg_viz_watch_history_limit: u64,

    // --- MUSEX-CAP-SEC-01 (Plane TERM #399): endpoint auth ---
    /// Bearer token required on the sensitive/mutating HTTP surface (see
    /// `crate::http::auth`) — `MUSE_API_TOKEN`, <secret-manager>-materialized at
    /// runtime, never a literal (S1/S7). `None` means auth is NOT
    /// configured: per the fail-closed posture, every protected route then
    /// answers `503` rather than either accepting all callers or silently
    /// opening up, unless [`Config::auth_disabled`] is explicitly set.
    pub api_token: Option<String>,
    /// Explicit escape hatch (`MUSE_AUTH_DISABLED=1`/`true`) for a dev box
    /// with no token configured, so the protected surface degrades to open
    /// only when an operator deliberately opts in — never as a silent
    /// default. Ignored (protected routes stay enforced) once
    /// [`Config::api_token`] IS configured; only changes behavior for the
    /// unconfigured-token case.
    pub auth_disabled: bool,
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
            tmdb_metadata_base_url: env_opt("MUSE_TMDB_METADATA_URL"),
            ollama_url: env_opt("MUSE_OLLAMA_URL"),
            chord_url: env_opt("CHORD_URL"),
            chord_embeddings_url: env_opt("CHORD_EMBEDDINGS_URL"),
            chord_api_token: env_opt("CHORD_API_TOKEN"),
            tvdb_api_key: env_opt("MUSE_TVDB_API_KEY").map(QbitPassword::from),
            tvdb_pin: env_opt("MUSE_TVDB_PIN").map(QbitPassword::from),
            tvdb_base_url: env_opt("MUSE_TVDB_BASE_URL"),
            skyhook_base_url: env_opt("MUSE_SKYHOOK_URL"),
            metadata_keyless: env_opt("MUSE_METADATA_KEYLESS")
                .map(|v| !(v == "0" || v.eq_ignore_ascii_case("false")))
                .unwrap_or(true),
            searxng_url: env_opt("MUSE_SEARXNG_URL"),
            news_url: env_opt("MUSE_NEWS_URL"),
            news_api_key: env_opt("MUSE_NEWS_API_KEY"),
            arr_instances_json: env_opt("MUSE_ARR_INSTANCES"),

            qbit_url: env_opt("MUSE_QBIT_URL"),
            qbit_user: env_opt("MUSE_QBIT_USER"),
            qbit_pass: env_opt("MUSE_QBIT_PASS").map(QbitPassword::from),

            prowlarr_tick_interval_secs: env_u64("MUSE_PROWLARR_TICK_INTERVAL_SECS", 60),
            prowlarr_movie_categories: env_int_list("MUSE_PROWLARR_MOVIE_CATEGORIES", &[2000]),
            prowlarr_tv_categories: env_int_list("MUSE_PROWLARR_TV_CATEGORIES", &[5000]),
            prowlarr_resolve_min_confidence: env_f32("MUSE_PROWLARR_RESOLVE_MIN_CONFIDENCE", 0.5),
            release_expiry_days: env_i64("MUSE_RELEASE_EXPIRY_DAYS", 21),
            prowlarr_search_max_per_hour: env_u64("MUSE_PROWLARR_SEARCH_MAX_PER_HOUR", 30),

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
            library_root: env_opt("MUSE_LIBRARY_ROOT").filter(|v| !v.trim().is_empty()),

            foundry_allowed_roots: env_opt("MUSE_FOUNDRY_ALLOWED_ROOTS"),
            foundry_work_dir: env_opt("MUSE_FOUNDRY_WORK_DIR"),
            foundry_encode_timeout_secs: env_opt("MUSE_FOUNDRY_ENCODE_TIMEOUT_SECS")
                .and_then(|v| v.parse().ok()),
            foundry_enable_mutation: env_bool("MUSE_FOUNDRY_ENABLE_MUTATION", false),
            foundry_retention_days: env_opt("MUSE_FOUNDRY_RETENTION_DAYS")
                .and_then(|v| v.parse::<u32>().ok()),
            foundry_ffprobe_bin: env_opt("MUSE_FOUNDRY_FFPROBE_BIN"),
            foundry_handbrake_bin: env_opt("MUSE_FOUNDRY_HANDBRAKE_BIN"),
            // SUBS-01. Read exactly like every other optional credential:
            // from the environment at runtime, never a literal.
            wyzie_key: env_opt("WYZIE_KEY").map(QbitPassword::from),
            wyzie_base_url: env_opt("MUSE_WYZIE_BASE_URL"),
            subtitle_store_dir: env_opt("MUSE_SUBTITLE_STORE_DIR").filter(|v| !v.trim().is_empty()),

            proactive_tick_interval_secs: env_u64("MUSE_PROACTIVE_TICK_INTERVAL_SECS", 3600),

            maintenance_tick_secs: env_u64("MUSE_MAINTENANCE_TICK_SECS", 1800),
            trending_tick_secs: env_u64("MUSE_TRENDING_TICK_SECS", 86400),
            embed_batch_size: env_u64("MUSE_EMBED_BATCH_SIZE", 50) as usize,
            maintenance_enrichment_limit: env_i64("MUSE_MAINTENANCE_ENRICHMENT_LIMIT", 10),

            wanted_search_cooldown_secs: env_i64("MUSE_WANTED_SEARCH_COOLDOWN_SECS", 21_600),
            wanted_max_grabs_per_pass: env_u64("MUSE_WANTED_MAX_GRABS_PER_PASS", 5) as usize,
            wanted_max_searches_per_pass: env_u64("MUSE_WANTED_MAX_SEARCHES_PER_PASS", 20) as usize,

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

            promotion_match_threshold: env_opt("MUSE_PROMOTION_MATCH_THRESHOLD")
                .and_then(|v| v.parse::<f64>().ok())
                .unwrap_or(0.55),
            promotion_cadence_secs: env_u64("MUSE_PROMOTION_CADENCE_SECS", 21_600),
            arr_request_auto_tier_enabled: env_opt("MUSE_ARR_REQUEST_AUTO_TIER_ENABLED")
                .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
                .unwrap_or(false),

            premiere_announce_cadence_secs: env_u64("MUSE_PREMIERE_ANNOUNCE_CADENCE_SECS", 604_800),
            premiere_engagement_watch_through_weight: env_opt(
                "MUSE_PREMIERE_ENGAGEMENT_WATCH_THROUGH_WEIGHT",
            )
            .and_then(|v| v.parse::<f64>().ok())
            .unwrap_or(0.5),
            premiere_engagement_household_love_weight: env_opt(
                "MUSE_PREMIERE_ENGAGEMENT_HOUSEHOLD_LOVE_WEIGHT",
            )
            .and_then(|v| v.parse::<f64>().ok())
            .unwrap_or(0.5),
            premiere_engagement_trusted_threshold: env_opt(
                "MUSE_PREMIERE_ENGAGEMENT_TRUSTED_THRESHOLD",
            )
            .and_then(|v| v.parse::<f64>().ok())
            .unwrap_or(0.4),
            premiere_engagement_curator_threshold: env_opt(
                "MUSE_PREMIERE_ENGAGEMENT_CURATOR_THRESHOLD",
            )
            .and_then(|v| v.parse::<f64>().ok())
            .unwrap_or(0.7),
            premiere_loved_rating_threshold: env_f32("MUSE_PREMIERE_LOVED_RATING_THRESHOLD", 7.0),
            premiere_starter_budget: env_u64("MUSE_PREMIERE_STARTER_BUDGET", 1) as u32,
            premiere_trusted_budget: env_u64("MUSE_PREMIERE_TRUSTED_BUDGET", 3) as u32,
            premiere_curator_budget: env_u64("MUSE_PREMIERE_CURATOR_BUDGET", 6) as u32,

            kg_taste_neighbor_threshold: env_f32("MUSE_KG_TASTE_NEIGHBOR_THRESHOLD", 0.5),
            kg_viz_watch_history_limit: env_u64("MUSE_KG_VIZ_WATCH_HISTORY_LIMIT", 200),

            api_token: env_opt("MUSE_API_TOKEN"),
            auth_disabled: env_opt("MUSE_AUTH_DISABLED")
                .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
                .unwrap_or(false),
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

    /// Assembles [`QbitConfig`] from the already-loaded `MUSE_QBIT_*`
    /// fields. Returns `None` (not an error) unless all three are set —
    /// qBittorrent download-client control is an optional, gracefully
    /// degrading dependency, same posture as [`Self::plex_url`]/
    /// [`Self::plex_token`] for `PlexClient::from_config`. This is the only
    /// place `MUSE_QBIT_PASS` is read into a [`QbitConfig`]; callers (e.g.
    /// `download::qbit::QbitClient::from_config`) never read the env
    /// themselves.
    pub fn qbit(&self) -> Option<QbitConfig> {
        Some(QbitConfig {
            url: self.qbit_url.clone()?,
            user: self.qbit_user.clone()?,
            pass: self.qbit_pass.clone()?,
        })
    }

    /// Assembles [`crate::metadata::config::TvdbConfig`] from the
    /// already-loaded `MUSE_TVDB_*` fields. Returns `None` (not an error)
    /// when `tvdb_api_key` is unset — same posture as [`Self::qbit`]: TVDB
    /// metadata is an optional, gracefully degrading dependency. This is
    /// the only place `tvdb_base_url`'s "empty means the real API host"
    /// default is applied; callers (`metadata::tvdb::TvdbClient::from_config`)
    /// never read the env themselves.
    pub fn tvdb(&self) -> Option<crate::metadata::config::TvdbConfig> {
        let api_key = self.tvdb_api_key.clone()?;
        Some(crate::metadata::config::TvdbConfig {
            base_url: self
                .tvdb_base_url
                .clone()
                .unwrap_or_else(|| crate::metadata::tvdb::DEFAULT_BASE_URL.to_string()),
            api_key,
            pin: self.tvdb_pin.clone(),
        })
    }
}

/// Test/scaffold convenience -- NOT used by `from_env`, which always reads
/// every field explicitly. Lets test modules elsewhere in the crate build a
/// `Config` via struct-update syntax (`Config { prowlarr_url, ..Default::default() }`)
/// without having to enumerate every unrelated field.
impl Default for Config {
    fn default() -> Self {
        Self {
            foundry_encode_timeout_secs: None,
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
            tmdb_metadata_base_url: None,
            ollama_url: None,
            chord_url: None,
            chord_embeddings_url: None,
            chord_api_token: None,
            tvdb_api_key: None,
            tvdb_pin: None,
            tvdb_base_url: None,
            skyhook_base_url: None,
            metadata_keyless: true,
            arr_instances_json: None,
            qbit_url: None,
            qbit_user: None,
            qbit_pass: None,
            prowlarr_tick_interval_secs: 60,
            prowlarr_movie_categories: vec![2000],
            prowlarr_tv_categories: vec![5000],
            prowlarr_resolve_min_confidence: 0.5,
            release_expiry_days: 21,
            prowlarr_search_max_per_hour: 30,
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
            library_root: None,

            // Foundry defaults are the SAFE values: unconfigured (so it does
            // not register) and mutation closed. A test that wants Foundry
            // opts in explicitly.
            foundry_allowed_roots: None,
            foundry_work_dir: None,
            foundry_enable_mutation: false,
            foundry_retention_days: None,
            foundry_ffprobe_bin: None,
            foundry_handbrake_bin: None,

            // SUBS-01 defaults are the SAFE values: no provider credential
            // (so the provider tier is inert) and no store directory (so
            // nothing can be written anywhere). A test that wants either opts
            // in explicitly.
            wyzie_key: None,
            wyzie_base_url: None,
            subtitle_store_dir: None,
            proactive_tick_interval_secs: 3600,
            maintenance_tick_secs: 1800,
            trending_tick_secs: 86400,
            embed_batch_size: 50,
            maintenance_enrichment_limit: 10,
            wanted_search_cooldown_secs: 21_600,
            wanted_max_grabs_per_pass: 5,
            wanted_max_searches_per_pass: 20,
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
            promotion_match_threshold: 0.55,
            promotion_cadence_secs: 21_600,
            arr_request_auto_tier_enabled: false,
            premiere_announce_cadence_secs: 604_800,
            premiere_engagement_watch_through_weight: 0.5,
            premiere_engagement_household_love_weight: 0.5,
            premiere_engagement_trusted_threshold: 0.4,
            premiere_engagement_curator_threshold: 0.7,
            premiere_loved_rating_threshold: 7.0,
            premiere_starter_budget: 1,
            premiere_trusted_budget: 3,
            premiere_curator_budget: 6,
            kg_taste_neighbor_threshold: 0.5,
            kg_viz_watch_history_limit: 200,
            api_token: None,
            auth_disabled: false,
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

/// Parse a boolean-ish env var. Accepts `1`/`true`/`yes`/`on`
/// (case-insensitive) as true and everything else as false, so a typo fails
/// **closed** — which matters because the only current caller is Foundry's
/// mutation kill-switch, where a misread must never open the gate.
fn env_bool(key: &str, default: bool) -> bool {
    match env_opt(key) {
        Some(v) => {
            let v = v.trim().to_ascii_lowercase();
            matches!(v.as_str(), "1" | "true" | "yes" | "on")
        }
        None => default,
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
            "MUSE_TMDB_METADATA_URL",
            "MUSE_OLLAMA_URL",
            "CHORD_URL",
            "MUSE_TVDB_API_KEY",
            "MUSE_TVDB_PIN",
            "MUSE_TVDB_BASE_URL",
            "MUSE_SKYHOOK_URL",
            "MUSE_METADATA_KEYLESS",
            "MUSE_SEARXNG_URL",
            "MUSE_NEWS_URL",
            "MUSE_NEWS_API_KEY",
            "MUSE_ARR_INSTANCES",
            "MUSE_PROWLARR_TICK_INTERVAL_SECS",
            "MUSE_PROWLARR_MOVIE_CATEGORIES",
            "MUSE_PROWLARR_TV_CATEGORIES",
            "MUSE_PROWLARR_RESOLVE_MIN_CONFIDENCE",
            "MUSE_RELEASE_EXPIRY_DAYS",
            "MUSE_PROWLARR_SEARCH_MAX_PER_HOUR",
            "MUSE_PUBLIC_URL",
            "MUSE_HDHR_DEVICE_ID",
            "MUSE_CHANNEL_GUIDE_WINDOW_HOURS",
            "MUSE_CHANNEL_SCHEDULER_TICK_SECS",
            "MUSE_RECALL_VECTOR_MAX_DISTANCE",
            "MUSE_FFMPEG_PATH",
            "MUSE_MEDIA_ROOT",
            "MUSE_LIBRARY_ROOT",
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
            "DISCORD_BOT_TOKEN",
            "MUSE_PROMOTION_MATCH_THRESHOLD",
            "MUSE_PROMOTION_CADENCE_SECS",
            "MUSE_ARR_REQUEST_AUTO_TIER_ENABLED",
            "MUSE_PREMIERE_ANNOUNCE_CADENCE_SECS",
            "MUSE_PREMIERE_ENGAGEMENT_WATCH_THROUGH_WEIGHT",
            "MUSE_PREMIERE_ENGAGEMENT_HOUSEHOLD_LOVE_WEIGHT",
            "MUSE_PREMIERE_ENGAGEMENT_TRUSTED_THRESHOLD",
            "MUSE_PREMIERE_ENGAGEMENT_CURATOR_THRESHOLD",
            "MUSE_PREMIERE_LOVED_RATING_THRESHOLD",
            "MUSE_PREMIERE_STARTER_BUDGET",
            "MUSE_PREMIERE_TRUSTED_BUDGET",
            "MUSE_PREMIERE_CURATOR_BUDGET",
            "MUSE_KG_TASTE_NEIGHBOR_THRESHOLD",
            "MUSE_API_TOKEN",
            "MUSE_AUTH_DISABLED",
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
        assert!(cfg.tvdb_api_key.is_none());
        assert!(cfg.tvdb_pin.is_none());
        assert!(cfg.tvdb_base_url.is_none());
        // AMETA-1: key-less proxy mode is the default; the proxy base URLs
        // are unset (each client applies its public-proxy default) but the
        // master switch defaults to true so a fresh deploy gets key-less
        // metadata with zero operator env.
        assert!(cfg.tmdb_metadata_base_url.is_none());
        assert!(cfg.skyhook_base_url.is_none());
        assert!(cfg.metadata_keyless);
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
        assert!(cfg.library_root.is_none());
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
        assert!(cfg.discord_bot_token.is_none());
        assert!((cfg.promotion_match_threshold - 0.55).abs() < f64::EPSILON);
        assert_eq!(cfg.promotion_cadence_secs, 21_600);
        assert!(!cfg.arr_request_auto_tier_enabled);
        assert_eq!(cfg.premiere_announce_cadence_secs, 604_800);
        assert!((cfg.premiere_engagement_watch_through_weight - 0.5).abs() < f64::EPSILON);
        assert!((cfg.premiere_engagement_household_love_weight - 0.5).abs() < f64::EPSILON);
        assert!((cfg.premiere_engagement_trusted_threshold - 0.4).abs() < f64::EPSILON);
        assert!((cfg.premiere_engagement_curator_threshold - 0.7).abs() < f64::EPSILON);
        assert!((cfg.premiere_loved_rating_threshold - 7.0).abs() < f32::EPSILON);
        assert_eq!(cfg.premiere_starter_budget, 1);
        assert_eq!(cfg.premiere_trusted_budget, 3);
        assert_eq!(cfg.premiere_curator_budget, 6);
        assert!((cfg.kg_taste_neighbor_threshold - 0.5).abs() < f32::EPSILON);
        assert_eq!(cfg.kg_viz_watch_history_limit, 200);
        assert!(cfg.api_token.is_none());
        assert!(!cfg.auth_disabled);
    }

    #[test]
    #[serial]
    fn musexcapsec01_auth_config_reads_token_and_disabled_flag_from_env() {
        std::env::set_var("MUSE_API_TOKEN", "test-token-value");
        std::env::set_var("MUSE_AUTH_DISABLED", "true");

        let cfg = Config::from_env();

        assert_eq!(cfg.api_token.as_deref(), Some("test-token-value"));
        assert!(cfg.auth_disabled);

        std::env::remove_var("MUSE_API_TOKEN");
        std::env::remove_var("MUSE_AUTH_DISABLED");
    }

    #[test]
    #[serial]
    fn musex16_kg_config_reads_taste_neighbor_threshold_from_env() {
        std::env::set_var("MUSE_KG_TASTE_NEIGHBOR_THRESHOLD", "0.72");

        let cfg = Config::from_env();

        assert!((cfg.kg_taste_neighbor_threshold - 0.72).abs() < f32::EPSILON);

        std::env::remove_var("MUSE_KG_TASTE_NEIGHBOR_THRESHOLD");
    }

    #[test]
    #[serial]
    fn musela1_tvdb_config_none_when_api_key_unset() {
        std::env::remove_var("MUSE_TVDB_API_KEY");
        let cfg = Config::from_env();
        assert!(cfg.tvdb().is_none());
    }

    #[test]
    #[serial]
    fn musela1_tvdb_config_reads_key_pin_and_base_url_override() {
        std::env::set_var("MUSE_TVDB_API_KEY", "tvdb-key");
        std::env::set_var("MUSE_TVDB_PIN", "4242");
        std::env::set_var("MUSE_TVDB_BASE_URL", "http://tvdb.test.invalid/v4");

        let cfg = Config::from_env();
        let tvdb = cfg.tvdb().expect("tvdb config should be Some");

        assert_eq!(tvdb.api_key.expose(), "tvdb-key");
        assert_eq!(tvdb.pin.as_ref().map(|p| p.expose()), Some("4242"));
        assert_eq!(tvdb.base_url, "http://tvdb.test.invalid/v4");

        std::env::remove_var("MUSE_TVDB_API_KEY");
        std::env::remove_var("MUSE_TVDB_PIN");
        std::env::remove_var("MUSE_TVDB_BASE_URL");
    }

    #[test]
    #[serial]
    fn musela1_tvdb_config_defaults_base_url_when_unset() {
        std::env::set_var("MUSE_TVDB_API_KEY", "tvdb-key");
        std::env::remove_var("MUSE_TVDB_BASE_URL");

        let cfg = Config::from_env();
        let tvdb = cfg.tvdb().expect("tvdb config should be Some");

        assert_eq!(tvdb.base_url, crate::metadata::tvdb::DEFAULT_BASE_URL);

        std::env::remove_var("MUSE_TVDB_API_KEY");
    }

    #[test]
    #[serial]
    fn musex17_kg_viz_config_reads_watch_history_limit_from_env() {
        std::env::set_var("MUSE_KG_VIZ_WATCH_HISTORY_LIMIT", "50");

        let cfg = Config::from_env();

        assert_eq!(cfg.kg_viz_watch_history_limit, 50);

        std::env::remove_var("MUSE_KG_VIZ_WATCH_HISTORY_LIMIT");
    }

    #[test]
    #[serial]
    fn musex15_premiere_config_reads_from_env_when_set() {
        std::env::set_var("MUSE_PREMIERE_ANNOUNCE_CADENCE_SECS", "3600");
        std::env::set_var("MUSE_PREMIERE_ENGAGEMENT_WATCH_THROUGH_WEIGHT", "0.6");
        std::env::set_var("MUSE_PREMIERE_ENGAGEMENT_HOUSEHOLD_LOVE_WEIGHT", "0.4");
        std::env::set_var("MUSE_PREMIERE_ENGAGEMENT_TRUSTED_THRESHOLD", "0.3");
        std::env::set_var("MUSE_PREMIERE_ENGAGEMENT_CURATOR_THRESHOLD", "0.6");
        std::env::set_var("MUSE_PREMIERE_LOVED_RATING_THRESHOLD", "8.5");
        std::env::set_var("MUSE_PREMIERE_STARTER_BUDGET", "2");
        std::env::set_var("MUSE_PREMIERE_TRUSTED_BUDGET", "5");
        std::env::set_var("MUSE_PREMIERE_CURATOR_BUDGET", "9");

        let cfg = Config::from_env();

        assert_eq!(cfg.premiere_announce_cadence_secs, 3600);
        assert!((cfg.premiere_engagement_watch_through_weight - 0.6).abs() < f64::EPSILON);
        assert!((cfg.premiere_engagement_household_love_weight - 0.4).abs() < f64::EPSILON);
        assert!((cfg.premiere_engagement_trusted_threshold - 0.3).abs() < f64::EPSILON);
        assert!((cfg.premiere_engagement_curator_threshold - 0.6).abs() < f64::EPSILON);
        assert!((cfg.premiere_loved_rating_threshold - 8.5).abs() < f32::EPSILON);
        assert_eq!(cfg.premiere_starter_budget, 2);
        assert_eq!(cfg.premiere_trusted_budget, 5);
        assert_eq!(cfg.premiere_curator_budget, 9);

        for key in [
            "MUSE_PREMIERE_ANNOUNCE_CADENCE_SECS",
            "MUSE_PREMIERE_ENGAGEMENT_WATCH_THROUGH_WEIGHT",
            "MUSE_PREMIERE_ENGAGEMENT_HOUSEHOLD_LOVE_WEIGHT",
            "MUSE_PREMIERE_ENGAGEMENT_TRUSTED_THRESHOLD",
            "MUSE_PREMIERE_ENGAGEMENT_CURATOR_THRESHOLD",
            "MUSE_PREMIERE_LOVED_RATING_THRESHOLD",
            "MUSE_PREMIERE_STARTER_BUDGET",
            "MUSE_PREMIERE_TRUSTED_BUDGET",
            "MUSE_PREMIERE_CURATOR_BUDGET",
        ] {
            std::env::remove_var(key);
        }
    }

    #[test]
    #[serial]
    fn musex14_config_reads_from_env_when_set() {
        std::env::set_var("MUSE_PROMOTION_MATCH_THRESHOLD", "0.8");
        std::env::set_var("MUSE_PROMOTION_CADENCE_SECS", "60");
        std::env::set_var("MUSE_ARR_REQUEST_AUTO_TIER_ENABLED", "true");

        let cfg = Config::from_env();

        assert!((cfg.promotion_match_threshold - 0.8).abs() < f64::EPSILON);
        assert_eq!(cfg.promotion_cadence_secs, 60);
        assert!(cfg.arr_request_auto_tier_enabled);

        std::env::remove_var("MUSE_PROMOTION_MATCH_THRESHOLD");
        std::env::remove_var("MUSE_PROMOTION_CADENCE_SECS");
        std::env::remove_var("MUSE_ARR_REQUEST_AUTO_TIER_ENABLED");
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
    fn config_library_root_unset_is_none() {
        std::env::remove_var("MUSE_LIBRARY_ROOT");
        let cfg = Config::from_env();
        assert!(cfg.library_root.is_none());
    }

    #[test]
    #[serial]
    fn config_library_root_blank_is_treated_as_unset() {
        // An accidentally-blank env var (set but empty) must not be
        // mistaken for "yes, scan an empty-string path" — same posture as
        // the `MUSE_LIBRARY_ROOT` field doc.
        std::env::set_var("MUSE_LIBRARY_ROOT", "   ");
        let cfg = Config::from_env();
        assert!(cfg.library_root.is_none());
        std::env::remove_var("MUSE_LIBRARY_ROOT");
    }

    #[test]
    #[serial]
    fn config_reads_library_root_override_from_env() {
        std::env::set_var("MUSE_LIBRARY_ROOT", "/mnt/qnap-library-ro");
        let cfg = Config::from_env();
        assert_eq!(cfg.library_root.as_deref(), Some("/mnt/qnap-library-ro"));
        std::env::remove_var("MUSE_LIBRARY_ROOT");
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
        std::env::set_var("MUSE_PROWLARR_SEARCH_MAX_PER_HOUR", "12");

        let cfg = Config::from_env();

        assert_eq!(cfg.prowlarr_tick_interval_secs, 30);
        assert_eq!(cfg.prowlarr_movie_categories, vec![2000, 2010]);
        assert_eq!(cfg.prowlarr_tv_categories, vec![5000, 5010]);
        assert!((cfg.prowlarr_resolve_min_confidence - 0.75).abs() < f32::EPSILON);
        assert_eq!(cfg.release_expiry_days, 7);
        assert_eq!(cfg.prowlarr_search_max_per_hour, 12);

        for key in [
            "MUSE_PROWLARR_TICK_INTERVAL_SECS",
            "MUSE_PROWLARR_MOVIE_CATEGORIES",
            "MUSE_PROWLARR_TV_CATEGORIES",
            "MUSE_PROWLARR_RESOLVE_MIN_CONFIDENCE",
            "MUSE_RELEASE_EXPIRY_DAYS",
            "MUSE_PROWLARR_SEARCH_MAX_PER_HOUR",
        ] {
            std::env::remove_var(key);
        }
    }

    #[test]
    #[serial]
    fn config_reads_qbit_settings_from_env_and_qbit_accessor_assembles_them() {
        for key in ["MUSE_QBIT_URL", "MUSE_QBIT_USER", "MUSE_QBIT_PASS"] {
            std::env::remove_var(key);
        }

        // Unset -> no live qbit config, gracefully, not an error.
        assert!(Config::from_env().qbit().is_none());

        std::env::set_var("MUSE_QBIT_URL", "http://192.0.2.60:8080");
        std::env::set_var("MUSE_QBIT_USER", "admin");
        std::env::set_var("MUSE_QBIT_PASS", "hunter2");

        let cfg = Config::from_env();
        assert_eq!(cfg.qbit_url.as_deref(), Some("http://192.0.2.60:8080"));
        assert_eq!(cfg.qbit_user.as_deref(), Some("admin"));
        assert_eq!(
            cfg.qbit_pass.as_ref().map(|p| p.expose().to_string()),
            Some("hunter2".to_string())
        );
        // MUSE_QBIT_PASS itself never appears verbatim in a Debug of Config.
        assert!(!format!("{cfg:?}").contains("hunter2"));

        let qbit = cfg.qbit().expect("all three vars set");
        assert_eq!(qbit.url, "http://192.0.2.60:8080");
        assert_eq!(qbit.user, "admin");
        assert_eq!(qbit.pass.expose(), "hunter2");

        for key in ["MUSE_QBIT_URL", "MUSE_QBIT_USER", "MUSE_QBIT_PASS"] {
            std::env::remove_var(key);
        }
    }

    #[test]
    #[serial]
    fn qbit_accessor_none_when_only_partially_configured() {
        for key in ["MUSE_QBIT_URL", "MUSE_QBIT_USER", "MUSE_QBIT_PASS"] {
            std::env::remove_var(key);
        }
        std::env::set_var("MUSE_QBIT_URL", "http://192.0.2.60:8080");
        std::env::set_var("MUSE_QBIT_USER", "admin");
        // MUSE_QBIT_PASS intentionally left unset.

        assert!(Config::from_env().qbit().is_none());

        for key in ["MUSE_QBIT_URL", "MUSE_QBIT_USER", "MUSE_QBIT_PASS"] {
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
