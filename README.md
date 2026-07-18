# Muse

**Muse is an AI-native media curation & taste companion** — a private, local-inference-first
"brain" for a Plex library. It owns taste modeling, curation, metadata, availability intelligence,
proactive recommendations, and a pseudo-TV channel director, backed by a **mandatory Postgres +
pgvector** database, while Plex stays the playback surface and qBittorrent stays the acquisition
tool.

Muse is built **strangler-fig**: it keeps qBittorrent (acquisition) and Plex (consumption) and
owns the *brain* (taste, curation, metadata, release selection, organization). Each phase ships
independent value and *arr/Tautulli are retired one function at a time — import (the only
high-blast-radius part) is dead last. Everything shipped so far is **Phase 0 + Phase 0.5:
read-only, benign playback only, zero blast radius against the live stack**.

It is a peer to the rest of the constellation — **Harmony** (build orchestrator), **Chord**
(inference proxy/orchestrator), **Terminus** (tool hub), and **Lumina** (assistant). Terminus's
`media_*` tools re-point at Muse as phases land, and Lumina consumes Muse's proactive-content
outbox.

See [`specs/S96-muse-foundation.md`](specs/S96-muse-foundation.md) for the founding spec, and the
[`docs/`](docs/) set for grounded reference material:

- [`docs/architecture.md`](docs/architecture.md) — constellation position, data flow, module layering
- [`docs/schema.md`](docs/schema.md) — the Postgres data model, grouped by concern, with the shipped divergences from the spec
- [`docs/runbooks.md`](docs/runbooks.md) — operational runbooks (Tautulli replacement, Prowlarr etiquette, adding Muse as a Plex tuner, the taste/embedding pipeline, the proactive→Lumina contract)
- [`docs/behavior-spec.md`](docs/behavior-spec.md) — the behavioral contract (taste derivation, curation ranking, proactive triggers/cooldowns, the pseudo-TV director, degradation invariants)
- [`docs/EXPERIENCE_LAYER.md`](docs/EXPERIENCE_LAYER.md) — the S118 MUSEX experience layer (personas, channel director, watch-together, adaptation loop, conversational assistant, Discord bot, what's-hot/cultural relevance, KG + graph visualizations, the settings/GUI control panel); documents the opt-in-only privacy model that runs through all of it, and is explicit about which pieces are implemented-and-tested vs. actually wired into a running deployment

> **Accuracy note.** This documentation is written against the shipped code, not the aspirational
> spec. Where a subsystem is a wired-but-untriggered seam or diverges from the founding spec, it is
> marked as such. In particular, three write-path recompute functions (embeddings, taste profile,
> taste-divergence radar) are fully implemented and tested but have **no scheduled worker or route
> calling them yet** — see [Wiring status](#wiring-status-what-actually-runs) below and the docs.

## Architecture at a glance

Muse is a single `axum` HTTP service (`muse` binary) plus a handful of background workers, all
over `sqlx`→Postgres. It is local-inference-first: embeddings run on a local `nomic-embed-text`
via Ollama, and curation/taste reasoning routes through Chord to a local model on <host>. Secrets are
materialized into the process environment from <secret-manager> at runtime (the fleet `.env`-from-vault
pattern) — the code only ever reads `std::env::var`, never authors a secret literal.

Every external dependency is **optional and graceful**: an unconfigured or down Plex / Prowlarr /
TMDb / Ollama / Chord / SearXNG / news endpoint degrades that one feature (a skipped tier, an
empty result, a `None` client) — it never blocks startup and never turns into a `500`.

Subsystem map (source module → concern):

| Module | Concern |
|---|---|
| `arr/` | Radarr/Sonarr multi-instance library ingest (metadata + files) |
| `plex/` | Read-only Plex API client (libraries, sessions, ratings, watchlist, artwork) |
| `tracker/` | **Native Tautulli-replacement**: webhook receiver + session poller + reconstruction |
| `tautulli/` | One-time Tautulli history backfill importer |
| `prowlarr/` | Availability report-pull worker (indexer sync, RSS pull, release parse, rollup) + on-demand targeted search (`search_releases`) |
| `decision/` | Pure release-decision/scoring engine ("what to grab") — see [Release-decision engine](#release-decision-engine) below |
| `trending/` | TMDb trending/population feed + streaming-availability |
| `embed/` | Local embedding pipeline (`nomic-embed-text` → pgvector) + cosine recall primitive |
| `recall/` | Vector-recall search API (`/query/resolve`, `/query/similar`) |
| `taste_model/` | Behavioral taste model (signals → profile + context centroids + LLM notes) |
| `radar/` | You-vs-masses `taste_divergence` computation |
| `curation/` | Recommend engine (`/recommend`, `/recommend/on_deck`, `/recommend/gaps`) |
| `proactive/` | Five proactive-content generators → `proactive_items` → Lumina |
| `enrichment/` | External enrichment cache (forum sentiment, "does it get good", renewal/trailer news) |
| `channels/` | On-demand pseudo-TV lineup composer + presets |
| `tuner/` | HDHomeRun-emulation linear tuner (discover/lineup/M3U/XMLTV) + rolling-grid scheduler |
| `streaming/` | ffmpeg channel streaming engine (`/auto/v{id}`) |
| `matching/` | Matching-verification support: MUSEL-C1's ffmpeg sample-still extraction primitive (MUSEL-C2's verdict logic lands later) |
| `web/` | Channel-guide web page + JSON API + artwork proxy |
| `plex_control/` | Plex Companion cast/play-queue client (library-only, unwired — see below) |

## Acquisition domain schema (MUSEM-01)

Muse today is a *read-only* observer of the operator's Radarr/Sonarr/Prowlarr fleet (`arr/`,
`prowlarr/`). `migrations/0104_acquisition_domain.sql` lays the **schema + repository foundation**
(`src/models/acquisition.rs`, `src/repo/acquisition.rs`) for a native write-path — monitoring
("wanted"), requests, the download queue, typed history, and a blocklist — mirroring the
Radarr/Sonarr data model (`quality` is a compound `{quality, revision}` value, custom formats are a
named scored-matcher registry, history is typed `jsonb`, provider IDs are a keyed map). **This item
is schema/repo only: no workers, no HTTP endpoints, nothing wired into a running deployment yet**
(see "Wiring status" below and the later MUSEM items for the write path itself).

Tables added:

| Table | Purpose |
|---|---|
| `monitored_items` | The "wanted" driver — monitoring a title within a `library`, decoupled from whether a `media_items`/file exists yet |
| `media_requests` | <media-service>-style request lifecycle (`requested → approved/denied → searching → grabbed → available`) |
| `download_queue` | One row per in-flight/terminal grab; requires at least one of `request_id`/`monitored_item_id` (`download_queue_has_source` CHECK) |
| `history_events` | Typed history (`event_type`-keyed `jsonb` payload), correlated to a download via `download_id` |
| `blocklist` | Releases/hashes a future decision engine must never re-grab |

The pre-existing quality-domain tables (`quality_definitions`, `quality_profiles`, `custom_formats`,
`quality_profile_formats` — added in MUSE-02, `src/models/quality.rs` / `src/repo/quality.rs`) are
reused by FK, **not redefined** — see the migration's header comment for why. `media_requests.kind`
and the `status`/`event_type` columns are plain `text` (not a Postgres enum type) so a future
`'music'` kind or a new status value never needs an `ALTER TYPE`; `src/models/acquisition.rs`
provides the validated Rust-side enums (`RequestStatus`, `QueueStatus`, `HistoryEventType`) with
`as_str()`/`FromStr` conversions.

The hot "wanted" scan is `repo::acquisition::list_wanted(pool, library_id)`: everything monitored
in a library that either has no file yet, or whose best on-disk file quality is strictly below its
quality profile's cutoff (compared via `quality_definitions.sort_order`, never raw quality-tier
ids, which are historical/non-contiguous per the blueprint).

## Running

Build and tests run on the fleet build host (<host> / a glibc-matched host), not the dev box — see
the project's `moosenet-spec` build pipeline. The service needs **PostgreSQL 17 with the `vector`
(pgvector) and `pg_trgm` extensions** on a `muse` database. sqlx queries are checked at runtime
(no offline query cache), so migrations must apply before first use; `db::migrate` runs them
best-effort at startup.

```
cargo run
```

`GET /health` returns `{status, version, db}` and **never 500s** even when the database is down (a
2-second probe reports `db:"up"|"down"`).

### Configuration (environment variables)

All configuration is read from the environment (materialized from <secret-manager> at runtime — **never
hardcode secrets**). Every field below is read by `Config::from_env` (`src/config.rs`). Examples use
RFC 5737 documentation IPs (`192.0.2.x`) and placeholder hostnames — never real infra.

| Variable | Default | Purpose |
|---|---|---|
| `MUSE_DATABASE_URL` | *(none)* | Postgres connection string for the pgvector-enabled `muse` DB. Pool connects lazily; service starts even if the DB is unreachable. |
| `MUSE_BIND_ADDR` | `0.0.0.0:8090` | HTTP bind address. |
| `MUSE_LOG_LEVEL` | `info` | `tracing`/`EnvFilter` level. |
| `PLEX_URL` | *(none)* | Plex Media Server base URL (read-only). e.g. `http://192.0.2.10:32400`. |
| `PLEX_TOKEN` | *(none)* | Plex API token (read-only). Used server-side only; never exposed to the browser. |
| `MUSE_PLEX_POLL_SECS` | *(none → 10)* | Session-poller cadence in seconds. Unset/unparseable falls back to the poller's own 10s default. |
| `TAUTULLI_URL` | *(none)* | Tautulli base URL for the one-time history backfill. |
| `TAUTULLI_API_KEY` | *(none)* | Tautulli API key. |
| `RADARR_URL` | *(none)* | Single-instance Radarr base URL (library ingest). |
| `RADARR_API_KEY` | *(none)* | Radarr API key. |
| `SONARR_URL` | *(none)* | Single-instance Sonarr base URL (library ingest). |
| `SONARR_API_KEY` | *(none)* | Sonarr API key. |
| `PROWLARR_URL` | *(none)* | Prowlarr base URL (availability report-pull). |
| `PROWLARR_API_KEY` | *(none)* | Prowlarr API key. When both this and `PROWLARR_URL` are set, the report-pull worker is spawned. |
| `TMDB_API_KEY` | *(none)* | TMDb API key (metadata, trending feed, `/query/resolve` beyond-the-library tier). |
| `MUSE_TVDB_API_KEY` | *(none)* | TheTVDB v4 API key (MUSEL-A1 metadata provider — the primary TV-metadata source the `*arr` suite uses). `None` keeps `metadata::tvdb::TvdbClient::from_config` uninstantiable; TVDB metadata degrades to unavailable. |
| `MUSE_TVDB_PIN` | *(none)* | Optional TheTVDB v4 subscriber PIN, paired with `MUSE_TVDB_API_KEY` for subscription-model keys. Most standard keys don't need one. |
| `MUSE_TVDB_BASE_URL` | *(none → real API)* | TheTVDB v4 API base URL override. Exists for tests/an on-prem proxy, same seam `MUSE_TRAKT_BASE_URL` provides for Trakt. |
| `MUSE_OLLAMA_URL` | *(none)* | Ollama base URL serving `nomic-embed-text` for local embeddings. |
| `CHORD_URL` | *(none)* | Chord OpenAI-compatible base URL for routed local-model reasoning (rationale, taste notes, channel composition). |
| `MUSE_SEARXNG_URL` | *(none)* | Fleet SearXNG base URL for forum/critic sentiment + "does it get good" enrichment. |
| `MUSE_NEWS_URL` | *(none)* | News-search endpoint base URL for renewal/trailer enrichment. |
| `MUSE_NEWS_API_KEY` | *(none)* | Optional bearer key for `MUSE_NEWS_URL` (many self-hosted aggregators need none). |
| `MUSE_ARR_INSTANCES` | *(none)* | JSON array describing the multi-instance *arr fleet (see below). Malformed JSON degrades ingest to zero instances, never fatal. |
| `MUSE_PROWLARR_TICK_INTERVAL_SECS` | `60` | How often the report-pull worker checks which indexers are due. |
| `MUSE_PROWLARR_MOVIE_CATEGORIES` | `2000` | Comma-separated Newznab parent category ids treated as movies. |
| `MUSE_PROWLARR_TV_CATEGORIES` | `5000` | Comma-separated Newznab parent category ids treated as TV. |
| `MUSE_PROWLARR_RESOLVE_MIN_CONFIDENCE` | `0.5` | Minimum release-name parse confidence before resolving a release to a title. |
| `MUSE_RELEASE_EXPIRY_DAYS` | `21` | How long a rolling `releases` snapshot row lives before it's pruned. |
| `MUSE_PROWLARR_SEARCH_MAX_PER_HOUR` | `30` | Rolling hourly cap on on-demand targeted searches (`prowlarr::search_releases`), shared with the report-pull worker's rate limiter. |
| `MUSE_PUBLIC_URL` | *(none)* | LAN-reachable base URL advertised by the tuner (`/discover.json`, `/muse.m3u` stream URLs). Degrades to `http://{bind_addr}` (only correct when `bind_addr` is a real LAN address). |
| `MUSE_HDHR_DEVICE_ID` | `MUSE0001` | HDHomeRun-emulation device id advertised in `/discover.json`. |
| `MUSE_CHANNEL_GUIDE_WINDOW_HOURS` | `48` | Rolling linear-guide window the director keeps `channel_programs` filled to; also the XMLTV render window. |
| `MUSE_CHANNEL_SCHEDULER_TICK_SECS` | `900` | How often the linear-channel scheduler tops off the guide window. |
| `MUSE_RECALL_VECTOR_MAX_DISTANCE` | `0.4` | Max pgvector cosine distance a `/query/resolve` vector-tier match may have and still count as "confident". |
| `MUSE_FFMPEG_PATH` | `ffmpeg` | Path (or `$PATH` command name) to the ffmpeg binary for the streaming engine. |
| `MUSE_MEDIA_ROOT` | `""` (empty) | Base path prepended to stored `relative_path`/`file_path` values for ffmpeg. Empty means "use stored paths as-is" (correct when they're already absolute). |
| `MUSE_PROACTIVE_TICK_INTERVAL_SECS` | `3600` | How often the proactive generator worker runs the five generators for every account. |
| `MUSE_TEST_DATABASE_URL` | *(none)* | **Test-only.** Points the DB-gated integration/live tests at a scratch Postgres; unset → those tests skip cleanly instead of failing. |
| `MUSE_QBIT_URL` | *(none)* | qBittorrent WebUI base URL, e.g. `http://192.0.2.60:8080` (MUSEM-02 download-client adapter). Read by the central `Config::from_env` like every other field in this table; all three `MUSE_QBIT_*` vars must be set together for `Config::qbit()` to return a live `QbitConfig` (see below), otherwise download-client control degrades to unavailable, same posture as `PLEX_URL`/`PLEX_TOKEN`. |
| `MUSE_QBIT_USER` | *(none)* | qBittorrent WebUI username. |
| `MUSE_QBIT_PASS` | *(none)* | qBittorrent WebUI password. Muse has no `SecretManager`/vault crate of its own — like every other credential in this table, it is materialized into the process environment from <secret-manager> at runtime and read in exactly one place, `Config::from_env` (`src/config.rs`), never a scattered `std::env::var` elsewhere in the codebase. It's stored on `Config` (and handed to `download::qbit::QbitClient` via `Config::qbit()` → `download::config::QbitConfig`) wrapped in `download::config::QbitPassword`, whose `Debug`/`Display` always print `<redacted>` — it never appears in logs even if `Config`/`QbitConfig`/`QbitClient` is formatted wholesale. |

That is **40 runtime environment variables** (plus the test-only `MUSE_TEST_DATABASE_URL`).

`MUSE_ARR_INSTANCES` JSON entry shape (`src/arr/config.rs`):

```json
[
  {"name": "radarr", "kind": "radarr", "base_url": "http://192.0.2.11:7878",
   "api_key": "<from-vault>", "library_kind": "movie", "root_folder": "/media/Movies/"},
  {"name": "sonarr", "kind": "sonarr", "base_url": "http://192.0.2.12:8989",
   "api_key": "<from-vault>", "library_kind": "tv"}
]
```

### Metadata provider layer (`src/metadata/`, MUSEL-A1)

A provider-agnostic seam for normalized title metadata, separate from `trending::client::TmdbClient`
(TMDb-specific, owns its own trending/popular/watch-provider surface). `metadata::MetadataProvider`
is a small async trait — `resolve_by_id(kind, provider_id)` and `search(query, kind)` — implemented
by `metadata::tvdb::TvdbClient` (TheTVDB v4, the primary TV-metadata source the `*arr` suite is
keyed against) and, for tests, `metadata::MockMetadataProvider`. Every implementation is read-only:
it only ever looks up metadata, never writes to the provider. Results normalize into
`metadata::ProviderMetadata` — `provider_ids` (a `tvdb`/`tmdb`/`imdb`/… map), `title`, `overview`,
`genres`, `images` (poster/backdrop URLs), `rating`, `first_aired`/`year`, `network` — every field
beyond `provider_ids` is independently nullable, since providers disagree on per-title coverage.

`metadata::tvdb::TvdbClient` mirrors `TmdbClient`'s shape (`struct { http, base_url, api_key }`,
`new`, `from_config(config) -> Option<Self>`) with one addition TMDb doesn't need: TheTVDB v4
requires a `POST /login` (api key [+ optional PIN] -> a bearer token) before any other call. The
token is cached behind a shared lock and re-fetched exactly once, transparently, on a `401` —
the same single-reauth shape `download::qbit::QbitClient` uses for its qBittorrent session cookie.
`from_config` returns `None` when `MUSE_TVDB_API_KEY` is unset, same graceful-degrade posture as
every other optional integration in `Config`; the token is never logged (a manual `Debug` impl
redacts the API key, PIN, and token).

This is what lets Muse identify/enrich titles without depending on `*arr` for metadata.

### Provider-resolution + enrichment aggregator (`src/metadata/resolve.rs`, MUSEL-A2)

`metadata::resolve::resolve_and_merge(ids, kind, providers) -> MuseResult<ProviderMetadata>` is the
fan-out-then-merge step on top of the MUSEL-A1 seam above: given a title's already-known provider
ids (`metadata::resolve::ResolveIds` — a `provider_ids: HashMap<String, String>` keyed by provider
name, e.g. `"tmdb"`/`"tvdb"`/`"imdb"`, plus a fallback `title`) and a set of *named*, already-
configured providers (`metadata::resolve::NamedProvider { name, provider: &dyn MetadataProvider }`),
it calls each provider's own `resolve_by_id`, merges the results into one `ProviderMetadata`, and
never fails just because a provider is absent, down, or has nothing for this id.

**Precedence (ARR-BLUEPRINT §7.7)**: movies are **TMDb-primary**, TV/anime are **TVDB-primary** —
the primary provider's fields win on a conflict (logged at `debug`, never silently swallowed); a
field the primary didn't populate gap-fills from whichever other provider has it, in fan-out order.
`provider_ids` is always a **union** across every provider that resolved, regardless of which one is
primary — anime alone can carry 5+ ids (tvdb/tmdb/imdb/tvmaze/mal/anilist per the blueprint).

**IMDb-id bridge**: lives inside the MUSEL-A2 TMDb adapter (`trending::client`'s
`impl MetadataProvider for TmdbClient`), not in `resolve_and_merge` itself — an id starting with
`tt` passed to `resolve_by_id` is treated as an IMDb id and bridged via TMDb's
`GET /find/{imdb_id}?external_source=imdb_id`, then re-resolved through `GET /movie|tv/{id}` for the
full record. `ResolveIds::id_for` falls back to the `imdb` id for any provider that has no id of its
own name recorded — TheTVDB has no analogous by-imdb-id lookup, so passing it there is still
graceful (TheTVDB just returns "not found" for an id it doesn't recognize, never an error).

**Graceful degrade**: `providers` empty -> `Ok(ProviderMetadata::default())` (clean no-op, not an
error). A provider present but with no id known for it is skipped for the id-based pass. A provider
that errors or returns `Ok(None)` mid-fan-out is skipped; the rest still merge. **No known ids for a
title** falls back to each configured provider's `search(title, kind)`, first hit only, at the
lowest confidence — logged at `warn` so it's visible in logs, and never promoted to a normal,
confident id-based match.

**Persistence** (`repo::media_metadata::apply_enrichment`, Muse's DB only — never a provider, never
the library) writes the merged result onto an *existing* `media_metadata` row (never creates one;
row creation stays `arr::ingest`'s job): `overview`/`year` fill in only when the merge produced a
value (never clears a previously-enriched field); `tmdb_id`/`tvdb_id`/`imdb_id` fill in only when
the row doesn't already carry that id; `provider_ids` and `images` (by `coverType`) are merged with
what's already on the row, not replaced wholesale; `genres` are additively linked via
`genres`/`media_metadata_genres` (find-or-create by name, link if not already linked) — never
unlinked, so a single provider's genre call never silently removes a curator's or another
provider's tag. `ratings` gets a coarse v1 shape, `{"resolved": {"value": <merged rating>}}` —
`ProviderMetadata::rating` is already a single merged scalar by the time it reaches persistence (the
per-provider breakdown was collapsed during the precedence merge), so there's no richer per-provider
key to write yet; a future item could thread that through `ProviderMetadata` if the UI ends up
wanting per-provider rating badges. `keywords` is wired end-to-end (merge + an additive DB write
path exists) but no concrete provider populates `ProviderMetadata::keywords` yet — added as an
empty-by-default, `Default`-backed field so a provider can start populating it later without another
shape change.

**Wired into the maintenance pass** (`maintenance::run_maintenance_pass`, step (a3)) as an optional,
bounded step: runs only when at least one metadata provider is configured (`state.tmdb` and/or a
freshly-built `metadata::tvdb::TvdbClient::from_config`), right after arr ingest/the wanted worker
and before `embed_stale` (so a freshly-enriched `overview` feeds a richer embedding, not a bare
title/year string). Bounded by `Config::maintenance_enrichment_limit` per `MediaKind` — up to that
many movie rows and that many show rows per pass — via
`repo::media_metadata::find_needing_enrichment` (rows with a known provider id but
`overview IS NULL`, oldest-unsynced-first). Every row's resolve/persist failure is logged and the
pass continues, same error-isolation posture as every other maintenance step; zero providers
configured is a harmless, cheap skip.

### Download-client adapter (`src/download/`, MUSEM-02)

Muse's first *write* to an acquisition substrate: a `DownloadClient` trait (`add`/`list`/
`info`/`delete`, `src/download/mod.rs`) with one live implementation, `QbitClient`
(`src/download/qbit.rs`), against the qBittorrent WebUI **v2** API. It does NOT decide what to
grab — that's a later item (MUSEM-04); this only executes an add/list/delete against
qBittorrent once handed a `GrabRequest`. `MockDownloadClient` (also in `src/download/mod.rs`)
records every `add` for later items' tests and performs no network I/O.

- **Auth**: `POST /api/v2/auth/login` (form `username`/`password`) captures the `SID` session
  cookie from `Set-Cookie`; the cookie is held per-client behind a shared lock (clones of one
  `QbitClient` share a session) and sent back as a plain `Cookie` header on every subsequent
  call, since this crate's `reqwest` dependency doesn't enable the `cookie_store` feature. A
  `403` on any data call triggers exactly one transparent re-auth + retry; a second failure (or
  any other status) surfaces as a typed `MuseError::Upstream`, never a panic.
- **Add**: `POST /api/v2/torrents/add`, sent as `multipart/form-data` (matching qBittorrent's
  own WebUI v2 API docs) via `reqwest`'s `multipart` feature — `urls=` (magnet URI or
  `.torrent` URL, both accepted), optional `category=`/`savepath=` parts (omitted entirely, not
  sent empty, when the caller has no opinion), `paused=`. qBittorrent's response body is a bare
  `"Ok."` with no hash, so the
  returned `GrabReceipt.hash` is resolved client-side from the magnet's `xt=urn:btih:` when
  present, and left `None` for a `.torrent`-URL add (no client-side way to know the infohash in
  that case).
- **List/info**: `GET /api/v2/torrents/info` (optionally `?hashes=`), parsed into
  `TorrentStatus`.
- **Delete**: `POST /api/v2/torrents/delete` (form `hashes=`/`deleteFiles=`).
- **Construction**: `QbitClient::from_config(&QbitConfig)`, where the `QbitConfig` comes from
  `Config::qbit()` (`src/config.rs`) — `MUSE_QBIT_URL`/`MUSE_QBIT_USER`/`MUSE_QBIT_PASS` are
  read only inside the CENTRAL `Config::from_env`, same as every other credential in this
  crate (`api_token`, `plex_token`, `tautulli_api_key`, ...); `download::config` itself does no
  env reading — it's a plain data holder for `QbitConfig`/`QbitPassword`. `Config::qbit()`
  returns `None` (not an error) unless all three vars are set — qBittorrent control is an
  optional, gracefully-degrading dependency, same posture as `PlexClient::from_config`.
- **Not wired into `AppState`/any route or worker yet** — this item ships the adapter only, in
  isolation, covered by its own httpmock test suite (`cargo test --bin muse download::`).
  Wiring it into a caller is a later MUSEM item.

## HTTP API surface (24 routes)

Every route below is real. The `/ingest/*`, `/query/*`, and `/proactive/*` nests each also carry a
`fallback` that answers **`501 Not Implemented`** for any un-mounted sub-path (a documented seam for
future spec items). Error→status mapping lives in `src/error.rs`.

**Health**
- `GET /health` — `{status:"ok", version, db:"up"|"down"}`, never 500s.

**Ingest**
- `POST /ingest/plex-webhook` — native Plex tracker webhook receiver (multipart, `payload` field). Always `200`.

**Query / recall**
- `POST /query/resolve` — free-text → library item, via a vector → trigram → (opt-in) TMDb ladder. `{tier, results}`.
- `POST /query/similar` — "more like this" for a `media_item_id`, vector-first with genre fallback. `{tier, results}`.

**Recommend / curation**
- `POST /recommend` — full ranked list (on-deck + gap + taste + availability-aware). `{items}`.
- `GET /recommend/on_deck?account_id=&limit=` — continue-watching only.
- `GET /recommend/gaps?account_id=&limit=` — gap analysis only.

**Proactive (Lumina's poll surface)**
- `GET /proactive/pending?account_id=&limit=` — eligible undelivered nudges. `{items}`.
- `POST /proactive/{id}/ack` — body `{"outcome":"sent"|"dismissed"}`; other values → `400`. `{item}`.

**Web guide + artwork**
- `GET /` and `GET /guide` — self-contained EPG-style channel-guide HTML page.
- `GET /api/channels` — channel summaries.
- `GET /api/channels/{id}/lineup` — a channel's program lineup (window `now-2h … now+24h`).
- `GET /art/{kind}/{id}?variant=poster` — artwork proxy (Postgres-cached; never leaks the Plex token; serves a 1×1 placeholder rather than 404).

**Request lifecycle (MUSEM-05, auth-gated — `Authorization: Bearer <MUSE_API_TOKEN>`)**
- `POST /requests` — body `{provider_ids, kind, title, quality_profile_id?}`. Classifies the
  request via `arr::request::classify_tier`, using a REAL on-demand Prowlarr search (MUSEM-03) as
  the availability signal rather than a fabricated one (see `src/acquisition`'s module doc).
  `Blocked` (no Prowlarr configured, or no `quality_profile_id`) → `400`, never persisted.
  `AutoApprovable` → persisted, then fulfilled immediately (search → decide → grab). Anything else
  → persisted as `Requested` for manual review. `{request, tier, outcome}`.
- `GET /requests?status=` — list requests, optionally filtered to one `media_requests.status`
  value; omitted lists across every status. Auth-gated so an unauthenticated caller can never
  enumerate request data (the same CAP-SEC-03 lesson `/recommend*` already applies).
- `POST /requests/{id}/approve` — fulfills a `Requested` request now (search → decide → grab).
  Idempotent: approving anything not currently `Requested` (already `Grabbed`/`Failed`/`Denied`/…)
  is a no-op, never a second grab.
- `POST /requests/{id}/deny` — marks `Requested` → `Denied` and records a `history_events` row.
  Idempotent the same way `approve` is.

Gated by TWO independent switches, both of which must be on for a live grab to ever happen:
`ExperienceSettings.acquisition.enabled` (the master gate, `crate::settings::AcquisitionSettings`,
default **off**) AND `Config::arr_request_auto_tier_enabled` (`MUSE_ARR_REQUEST_AUTO_TIER_ENABLED`,
default **off**, the same tiered-safety flag `arr::request::classify_tier` has always used). With
either off, `POST /requests` still persists the request — it just never reaches
`acquisition::fulfill_request`.

**Linear tuner (HDHomeRun-emulation) + streaming**
- `GET /discover.json` — HDHomeRun device descriptor.
- `GET /lineup_status.json` — static scan status.
- `GET /lineup.json` — channel lineup (GuideNumber/GuideName/URL per `mode='linear'` channel).
- `GET /muse.m3u` — M3U playlist alternative.
- `GET /xmltv.xml` — XMLTV EPG generated from `channel_programs`.
- `GET /auto/v{channel_id}` — continuous MPEG-TS stream (join-mid-stream). `501` if ffmpeg is missing, `503` if nothing is scheduled "now".

## Matching-verification: sample-still extraction (`src/matching/`, MUSEL-C1)

MUSEL-C1 is the raw-material primitive for a matching-verification pipeline: rather than
trusting a provider-ID match blindly, Muse can pull a handful of still frames from a media
file and (in a later item, MUSEL-C2) judge whether the file's actual content is consistent
with what it's labeled as.

- `streaming::ffmpeg::build_still_args(file_path, seek_ms)` — pure arg builder, extending the
  existing channel-streaming `ffmpeg` module. Reuses the same fast input-seek (`-ss` *before*
  `-i`) as `build_args`, and decodes exactly one frame (`-frames:v 1 -f image2pipe -vcodec
  mjpeg pipe:1`) per invocation — one still per ffmpeg process, mirroring MUSE-29's
  one-invocation-per-unit discipline.
- `matching::stills::extract_sample_stills(ffmpeg_path, file_path, runtime_ms, n)` — spreads
  `n` sample timestamps across `runtime_ms` (roughly 10%..90%, so a still never lands on a
  black leader/credits frame or seeks past EOF), spawns ffmpeg once per timestamp, and
  captures each resulting JPEG's bytes + timestamp into a `Still { bytes, timestamp_ms }`.
  Read-only on the input; every still lives in memory only (never written to disk beside the
  media). A decode failure on one timestamp is skipped, not fatal to the rest. `ffmpeg_path`
  is threaded through explicitly (matching `Config::ffmpeg_path`, `MUSE_FFMPEG_PATH`) rather
  than assumed — no hardcoded infra. If the ffmpeg binary itself is missing, the whole call
  degrades gracefully to `MuseError::NotImplemented` (the same posture `streaming` uses for a
  missing binary), instead of panicking or repeating the same failure per timestamp.
- MUSEL-C2 (a separate spec item) builds the actual `verify_match` verdict — VLM-via-Chord,
  still-liveness, metadata-consistency — on top of these stills. Not implemented yet.

## Release-decision engine

MUSEM-04 adds `src/decision/` — the pure, deterministic "what to grab" engine that will sit
between a future targeted Prowlarr search (MUSEM-03) and a future qBittorrent grab
(MUSEM-02): given a set of candidate releases, a `quality_profiles` row, and its
`quality_profile_formats` score table, pick the single best *eligible* release, or reject with
reasons. Mirrors `arr::request::classify_tier`'s shape — **no I/O of its own**; every signal is
passed in by the caller, which is what makes it exhaustively unit-testable.

- `decision::decide_release(candidates, profile, format_scores, policy) -> Decision` — the
  entrypoint. `Decision` is `Grab(ReleaseChoice)` or `Reject { reasons: Vec<String> }` (never a
  silent empty result).
- `decision::scoring::ReleaseCandidate` wraps the existing `models::release::Release` row (MUSE-16's
  rolling Prowlarr-report snapshot) plus a `runtime_minutes` hint — the one fact a release row
  can't carry on its own (how long the media item it would satisfy runs), needed for the blueprint
  §2 size-per-minute mis-tag guard. No new candidate/quality types are invented — this consumes
  `models::quality::{QualityDefinition, QualityProfile, CustomFormat, QualityProfileFormat}`
  exactly as MUSE-02 shipped them.
- **Gates** (any failure rejects that candidate with a reason, never a default grab):
  quality-tier resolution (source+resolution → a `quality_definitions` row; unresolvable = fail
  closed), the tier's `allowed` flag in `profile.items`, size-per-minute bounds, and
  `min_format_score`.
- **Custom-format scoring**: `decision::scoring::CustomFormatScorer` evaluates
  `custom_formats.specifications` (blueprint §6/§7.5) against a candidate — `ReleaseTitleSpecification`/
  `EditionSpecification` (case-insensitive substring, deliberately no regex engine — no new
  dependency, same conservative-matching philosophy as `prowlarr::parse`), `SourceSpecification`,
  `ResolutionSpecification`, `LanguageSpecification`, `SizeSpecification`, and
  `IndexerFlagSpecification` (freeleech only, since `Release` doesn't carry a raw indexer-flag
  list). Combining rule mirrors *arr: all `required` specs must match, and at least one
  non-required spec must match if any exist.
- **Scorer registry seam**: matching is behind a `decision::scoring::Scorer` trait +
  `ScorerRegistry` (currently just `CustomFormatScorer::deterministic()`) specifically so a future
  local-LLM/taste scorer (the charter's Phase-1 AI release-selection seam) can register alongside
  the static scorers without `decide_release` itself changing.
- **Ranking** of gate-surviving candidates: quality-tier rank (the profile's `items` preference
  order) → total format score → `proper_repack` (REPACK/PROPER beats an otherwise-equal
  non-repack) → seeders (`None`/unknown is never coerced to `0` — sorted between a known-positive
  count and a known-zero one, so an unreported private-tracker seeder count isn't unfairly sunk)
  → freeleech → smaller size → the release `guid` as a fully deterministic final tiebreak.
- **Upgrade decisions**: `ScoringPolicy::existing` (`Some(ExistingRelease{quality_definition_id,
  total_format_score})`) turns the call into "is anything here worth upgrading to". If the
  existing file already meets the profile's cutoff (tier ≥ `cutoff_quality_id`'s rank AND format
  score ≥ `cutoff_format_score`), or `upgrade_allowed` is `false`, `decide_release` rejects
  outright ("good enough, stop") without even reaching ranking; otherwise the best candidate must
  beat the existing file by tier or by at least `min_upgrade_format_score`.

As of MUSEM-05, `decision::decide_release` IS wired — see the next section.

## Acquisition orchestrator + request lifecycle (MUSEM-05)

`src/acquisition/` is the first thing in this crate that assembles MUSEM-01..04 into a real,
callable path: `acquisition::fulfill_request(deps, request) -> FulfillOutcome` runs, for one
`media_requests` row, a genuine on-demand Prowlarr search (MUSEM-03, narrowed to the request's
`media_kind`'s configured categories) → `decision::decide_release` (MUSEM-04) against the
request's `quality_profile_id` → on `Decision::Grab`, `DownloadClient::add` (MUSEM-02) → persists
a `download_queue` row (MUSEM-01, status `Queued`, `client_hash` from the grab receipt) and a
`Grabbed` `history_events` row; on `Decision::Reject` (or a download-client error), marks the
request `Failed` and records a `DownloadFailed` history row — never a phantom queue row, never a
panic. A request with no `quality_profile_id`, or no Prowlarr configured, degrades to
`FulfillOutcome::Skipped` and is left `Requested` for manual follow-up, rather than erroring.

`acquisition::AcquisitionSink` is the FIRST real (non-`Noop`) implementation of
`arr::request::MediaRequestSink` this crate ships: `submit(draft)` creates the `media_requests` row
for the draft and immediately `fulfill_request`s it. `arr::request::submit_if_appropriate`
guarantees `submit` is only ever reached for `RequestTier::AutoApprovable` — see
`src/acquisition/mod.rs`'s module doc for how `POST /requests` (below) computes a REAL
availability signal (from the on-demand search itself, not a fabrication) so that tier is
actually reachable now, closing the gap `conversational::handle_conversational_request`'s own doc
flagged as "never `AutoApprovable` in practice" and named as the natural follow-up.

`decision::`/`prowlarr::search_releases`/`download::qbit::QbitClient` have no dependency on each
other beyond what `acquisition::fulfill_request` wires — each still merges/tests independently.

## Monitored "wanted" acquisition worker (MUSEM-06)

`src/acquisition/worker.rs` is the Sonarr/Radarr-style background engine: `run_wanted_pass`
scans `repo::acquisition::list_wanted` (MUSEM-01) across every library and, for each item not
searched within `Config::wanted_search_cooldown_secs` (default 6h) of its
`monitored_items.last_search_at`, runs a real on-demand Prowlarr search (MUSEM-03) and turns the
result into an `Availability` signal for `arr::request::classify_tier` — reused verbatim, never a
second tiering rule. Only an `AutoApprovable` item (the operator opted in via
`Config::arr_request_auto_tier_enabled` AND the search actually confirmed it's grabbable now)
reaches `acquisition::fulfill_request` (MUSEM-05) at all; every other item still gets a
`media_requests` row persisted at `RequestStatus::Requested` for manual/operator follow-up —
never a silent grab, including when there's no capability to even search (no Prowlarr configured,
or no `quality_profile_id` on the monitored row).

**Exactly one real Prowlarr search per wanted item.** `fulfill_request` normally runs its own
on-demand search; the worker instead hands it the candidates it already fetched via a new
`FulfillOptions::prefetched_candidates` field, so an `AutoApprovable` item is never searched
twice. This is what makes `Config::wanted_max_searches_per_pass` (default 20) an honest bound on
actual Prowlarr calls, not just on how many items get *considered* — on top of the shared
`ProwlarrClient` `RateLimiter`'s own hourly search budget. `Config::wanted_max_grabs_per_pass`
(default 5) separately caps grabs.

**Idempotent across passes.** A worker-originated grab's `download_queue` row carries BOTH
`request_id` and `monitored_item_id` (`FulfillOptions::monitored_item_id` →
`DownloadSource::Both`, threaded through `fulfill_request`/`grab_and_persist`) — not just
`request_id` the way `POST /requests`' grabs do — so `repo::acquisition::
is_monitored_item_active_in_queue`'s `monitored_item_id`-keyed check actually finds it, and an
item already `queued`/`downloading` is skipped on every subsequent pass, never re-searched or
re-grabbed.

**Create-once pending requests, keyed off real existence, not a timestamp proxy.** Every
worker-created `media_requests` row also carries `monitored_item_id` (migration
`0105_media_requests_monitored_item.sql`, `NULLABLE` — `POST /requests`/`approve`/
`AcquisitionSink` still pass `NULL`, no monitored item involved). Before persisting a `Requested`
row for a non-grabbed outcome (no-capability, `NeedsReview`, `Blocked`), the worker checks
`repo::acquisition::has_open_worker_request_for_monitored_item` — an existing non-terminal
(`requested`/`approved`/`searching`/`grabbed`) request for that monitored item — and only creates
a new one when none exists. An earlier version keyed this off `monitored_items.last_search_at IS
NULL` ("first encounter"); that was wrong, because a FAILED search also sets `last_search_at`
without creating a request, so a pass-1 search failure could permanently suppress the request a
later, successful pass should have created. `last_search_at` is now purely a cooldown timer.

The `AutoApprovable` branch applies the same "at most one worker-created request per monitored
item" invariant, but by REUSE rather than refusal: `repo::acquisition::
get_open_worker_request_for_monitored_item` fetches any existing open request for the item, and
if one exists (e.g. a prior pass left it `Requested` because auto-tier was off, or availability
wasn't confirmed grabbable yet), `fulfill_request` is called against THAT row — transitioning it to
`Grabbed`/`Failed` in place — instead of creating a second request. A fresh request is only created
when none exists yet. This branch's own protection against a double GRAB remains the
`download_queue.monitored_item_id` check; the reuse-vs-create logic here is purely about never
leaving a stale duplicate `Requested` row behind when an item's classification improves.

Non-blocking: an unreachable Prowlarr/qBittorrent, a DB hiccup, or a metadata row deleted mid-pass
is logged and the pass moves on to the next item — never a panic, never an aborted pass.

`run_wanted_pass` is itself gated on `ExperienceSettings.acquisition.enabled` (the same master
gate `fulfill_request` enforces unbypassably as its own first action) — checked again here purely
so a gate-off deployment short-circuits to a cheap no-op without even listing libraries.

**Scheduled inside the maintenance chain** (`crate::maintenance::run_maintenance_pass`,
`src/maintenance/mod.rs`), right after the arr-ingest step and before embed/taste/divergence/
enrichment — dependency order, since arr ingest is what refreshes the `media_items`/`media_files`
rows `list_wanted`'s cutoff comparison reads. `MaintenanceSummary.wanted` carries the pass's own
tally (`grabbed`/`needs_review`/`already_queued_skipped`/`cooldown_skipped`/`no_capability_skipped`/
`metadata_missing_skipped`/`search_failed`/`errors`/`grab_cap_skipped`/`search_cap_skipped`), same
"ran and did nothing" vs "not run" posture every other maintenance-pass step already follows.

## Wiring status: what actually runs

Not every implemented subsystem is triggered in a running deployment. This matters for operators —
some capabilities need a manual invocation or a future worker/route to become live.

**Live HTTP routes:** all 24 listed above.

**Newly wired at MUSEM-05** (previously listed below as unwired seams — each now has a real
caller):
- `download::qbit::QbitClient` — called by `acquisition::fulfill_request` (via `AppState.download`,
  constructed in `main.rs` from `Config::qbit()`) whenever a request's search resolves to a `Grab`.
- `prowlarr::search_releases` — called by `acquisition::search_candidates`, both from
  `POST /requests`' availability check and from `acquisition::fulfill_request`'s search step.
- `decision::decide_release` — called by `acquisition::fulfill_request` against the candidates the
  search above returns.
- `repo::acquisition::*` — `media_requests`/`download_queue`/`history_events` are now written
  end-to-end by `acquisition::fulfill_request` and the `POST /requests*` handlers.

**Newly wired at MUSEL-A2:**
- `metadata::resolve::resolve_and_merge` + `repo::media_metadata::apply_enrichment` — scheduled
  inside the maintenance chain (`maintenance::run_maintenance_pass`, step (a3)) whenever `state.tmdb`
  and/or a config-built `metadata::tvdb::TvdbClient` is configured; see the "Provider-resolution +
  enrichment aggregator" section above.

**Newly wired at MUSEM-06:**
- `monitored_items` (MUSEM-01's "wanted" driver table) — now has a real caller:
  `acquisition::worker::run_wanted_pass`, scheduled inside the maintenance chain (see above).
  `blocklist` still has no caller (no write path adds to it yet).

**Background workers spawned at startup** (`src/workers.rs`):
- Plex session poller (`tracker::poller`) — always spawned; no-ops if Plex unconfigured.
- Prowlarr report-pull worker — spawned **only when Prowlarr is configured**.
- Linear-tuner scheduler (`tuner::scheduler`) — always spawned; no-ops with zero linear channels.
- Proactive generator worker (`proactive::scheduler`) — always spawned; no-ops with zero accounts.

**Implemented but NOT triggered by any worker or route (seams awaiting wiring):**
- `embed::pipeline::embed_stale` — the embedding **write** path. Nothing schedules it, so embeddings are never written in a running deployment unless invoked manually. (The read primitive `embed::nearest` *is* live, used by recall + curation.)
- `taste_model::recompute_taste` — signal→profile recompute. No scheduled caller, so `taste_profile`/`taste_context_centroids` are never populated automatically.
- `radar::divergence::recompute_divergence` — the you-vs-masses radar. No caller, no HTTP surface at all; `taste_divergence` is never computed automatically.
- `arr::ingest::run` — library ingest. Parsed `MUSE_ARR_INSTANCES` is held in state, but no worker or route runs the ingest.
- `tautulli::backfill::run` — one-time history import. No route/CLI/worker; intended to be driven by an orchestrator/ops step.
- `trending::snapshot_trending` — TMDb trending ingest. `main.rs` notes it as a "follow-on wiring item".
- `channels::compose_channel_run` — the on-demand pseudo-TV director. Fully implemented + tested, but no HTTP route mounts it (the *linear* tuner uses its own `tuner::scheduler` grid-filler instead).
- `enrichment::EnrichmentService::enrich_media_item` — external-enrichment cache population. Wired object on `AppState`, but nothing calls it outside tests.
- `plex_control::*` — Plex Companion cast/play-queue client. Declared as a module but **not mounted anywhere** and never called — library-only, and never exercised against a real Plex server.
- `repo::acquisition::blocklist` (MUSEM-01) — `media_requests`/`download_queue`/`history_events`/`monitored_items` are now wired (see "Newly wired at MUSEM-05"/"Newly wired at MUSEM-06" above); `blocklist` still has no write-path caller.

Consequence: in a fresh deployment with a fully populated library and Ollama/Chord configured,
`/recommend`'s taste tier, `proactive`'s `friday_evening`, and `zeitgeist` will silently return
empty until the three recompute write-paths are given a scheduled caller. See
[`docs/runbooks.md`](docs/runbooks.md) for how to drive them and
[`docs/behavior-spec.md`](docs/behavior-spec.md) for the full contract.

## Testing

See [`docs/TESTING.md`](docs/TESTING.md) for the *why*: the four gating phases, the
snapshots-everywhere invariant and its DSN-guard enforcement, the TASTE two-layer model, and the
shadow-parity retirement gate. This section stays the authoritative *how-to-run* reference (env
vars, per-phase commands, CI) — the two are cross-referenced, not duplicated.

The suite runs green with **no live database** — every DB-touching integration test is gated on
`MUSE_TEST_DATABASE_URL` and skips cleanly (does not fail) when it's unset. Point it at a scratch
Postgres 17 (`vector` + `pg_trgm`) database to actually exercise migrations and the repo layer:

```
MUSE_TEST_DATABASE_URL=postgres://user:pass@192.0.2.30/muse_test cargo test
```

Env-var-reading config tests use `serial_test` (process-global env), so run the suite
single-threaded (`cargo test -- --test-threads=1`) to avoid cross-test env races. HTTP-client tests
use `httpmock` and need no live upstream. The integration tests live *inside* the binary crate
(there is no `[lib]` target) so they can reach `crate::repo`/`crate::models`.

### Endpoint contract/integration harness (`src/endpoint_tests.rs`)

A router-level test suite exercises every route mounted in `http::router` through a real axum
`Router` via `tower::ServiceExt::oneshot` (an actual HTTP request/response round trip, not a bare
handler call) — `/health`, `/query/resolve`, `/query/similar`, `/recommend` +
`/recommend/on_deck` + `/recommend/gaps`, `/proactive/pending` + `/proactive/{id}/ack`,
`/api/channels` + `/api/channels/{id}/lineup`, `/art/{kind}/{id}`, `/channels/{id}/compose`,
`/ops/*`, and the `/ingest` \| `/query` \| `/proactive` fallback-501 contract. Each route gets a
contract test (status + response shape), an error-path test, and — gated on
`MUSE_TEST_DATABASE_URL` like the rest of the suite — a happy-path test against real seeded rows.
The gated `db_gated::read_endpoints_never_mutate_the_database` test snapshots every watched
table's row count, exercises every Phase-0 read endpoint back to back, and asserts the counts are
unchanged — the executable form of this service's read-only-until-Phase-1 posture. The two
intentionally-mutating Phase-0 endpoints (`/channels/{id}/compose`, `/proactive/{id}/ack`) get the
opposite assertion in their own tests: that they write exactly the one row they claim to. No test
in this module ever configures a real Plex/Prowlarr/TMDb/Tautulli/arr/Ollama/Chord client, so it
can never reach a live upstream even by accident — same posture as every other test in this crate.
Run with `cargo test endpoint_tests`.

### Golden-response regression baseline (MUSET-02)

On top of the contract/error-path/happy-path coverage above, the `golden`/`golden_support`
modules at the bottom of `src/endpoint_tests.rs` (plus a nested `golden` module inside
`db_gated` for the endpoints whose representative response needs a seeded row) add a
**golden-response baseline**: the exact, canonicalized JSON — or raw bytes, for the artwork
proxy's placeholder PNG — a representative request currently returns, committed under
[`tests/golden/`](tests/golden) and diffed on every `cargo test` run. This catches drift in
response *content*, not just status code. Non-deterministic fields (timestamps, generated ids,
per-run-unique fixture names) are redacted to a stable `"<redacted>"` placeholder via JSON
Pointer before comparison, rather than skipped — the rest of the shape is still asserted
exactly.

- **Regenerate a baseline** after an intentionally changed response:
  `MUSE_UPDATE_GOLDEN=1 cargo test endpoint_tests`. Never hand-edit a `tests/golden/*.json`
  file — golden tests are otherwise strictly read-only/comparison-only.
- **DB-independent goldens** (`health`, `/query/resolve`'s empty-query short-circuit, the
  `/ops/ingest/*` unconfigured 503s, the `/ingest`\|`/query`\|`/proactive` 501 fallback, the two
  `/proactive/{id}/ack` and `/channels/{id}/compose` 400 error bodies, and the artwork proxy's
  placeholder PNG) run in the default `cargo test` invocation, no `MUSE_TEST_DATABASE_URL`
  needed.
- **DB-gated goldens** (the `/recommend` family for a signal-empty account, an `/api/channels`
  + `/api/channels/{id}/lineup` shape for a real seeded channel, and a
  `/proactive/{id}/ack`-dismissed persisted+returned shape) skip cleanly without
  `MUSE_TEST_DATABASE_URL`, same posture as the rest of the suite.
- **Proof the mechanism actually catches drift**:
  `golden::golden_diff_mechanism_detects_a_deliberately_altered_response` exercises the real
  comparison function against a deliberately mutated response (in a self-contained scratch
  file, never a committed golden) and asserts it panics.

### Snapshot ingestion, fixtures, TASTE, reasoning review, shadow, parity (MUSET-03..09)

Everything below reuses the SAME "skip cleanly, never fail" gate as the sections above —
`MUSE_SNAPSHOT_DATABASE_URL` (preferred) or `MUSE_TEST_DATABASE_URL` (fallback), resolved by
`snapshot::load::snapshot_database_url_from_env`. Every DB-gated test in every module below runs
`snapshot::load::connect_snapshot_db`/`connect_snapshot_db_from_env` first, which validates the
DSN through `snapshot::guard::validate_snapshot_dsn` (MUSET-03 AC5) BEFORE connecting: the DSN
must be loopback-hosted and carry an explicit `test`/`snapshot`/`scratch` marker in the database
name, or the guard refuses it outright. This is structurally airtight against ever touching a
live media DB or the production `muse` DB, even from a misconfigured env var. Each helper
(`*_pool_or_skip`) also runs this crate's own migrations against the connected pool, so no
separate migration step is needed — set the env var, run `cargo test`.

- **Snapshot ingestion pipeline** (`src/snapshot/`, MUSET-03): `snapshot::db_gated` round-trips a
  loaded snapshot (normalize → load → provenance) against the isolated Postgres. Guard unit tests
  (`snapshot::guard::tests`) are pure string/IP checks with no DB and always run.
- **Real-data fixtures** (`src/fixtures/`, MUSET-04): reusable seeded-account fixtures
  (`heavy_rewatcher`, `multi_genre`, `cold_start_empty`, `sparse_metadata`) + their
  `ProfileExpectation`s, gated via `fixtures::loader::tests::fixture_pool_or_skip`.
- **TASTE mechanics** (`src/taste_mechanics_tests.rs`, MUSET-05): the deterministic floor
  (embedding/pgvector ops, context-bucketing, scoring/ranking consistency). DB-gated cases go
  through `mechanics_pool_or_skip`; the pure-math determinism assertions run unconditionally.
- **TASTE golden-set regression** (`src/taste_golden_set.rs`, MUSET-06): hand-computed
  known-good taste-lean values a regression must not drift away from. Grader-sanity,
  tolerance-math, and negative-perturbation tests run with no DB; `golden_pool_or_skip`-gated
  cases need the scratch DB.
- **Adversarial reasoning review** (`src/taste_review/`, MUSET-07): trace + panel-critique +
  finding-sink tests all run against `panel::MockReasoningPanel`/`sink::MockFindingSink` — fully
  in-process, no network, no DB, no Terminus client (Muse has no live Terminus integration yet)
  — so this module's suite runs in the FAST phase, not the full/snapshot phase.
- **Shadow runner** (`src/shadow/`, MUSET-08): read-only Tautulli-replacement analytics computed
  from snapshot `play_events`. DB-gated (`shadow::tests::db_gated`) via `snapshot_pool_or_skip`;
  asserts the runner performs zero writes.
- **Parity diff + retirement evidence** (`src/parity/`, MUSET-09): diffs shadow output against
  snapshot Tautulli-origin numbers. The core diff/report-building tests are pure in-memory data
  transforms with no DB; the seeding/overlap tests (`parity::tests::db_gated`, if present) use
  the same `snapshot_pool_or_skip` idiom as `shadow`.

Run any single phase locally the same way as the sections above, e.g.:

```
cargo test snapshot::
cargo test taste_mechanics_tests
cargo test taste_golden_set
cargo test taste_review
cargo test shadow::
cargo test parity::
```

To exercise the DB-gated cases in any of them, point a **local scratch** Postgres 17 with
`vector` + `pg_trgm` available, whose database name carries an explicit
`test`/`snapshot`/`scratch` marker (per the MUSET-03 guard above). Set **both** env vars to the
same DSN: the snapshot-family tests prefer `MUSE_SNAPSHOT_DATABASE_URL` (falling back to
`MUSE_TEST_DATABASE_URL`), while `endpoint_tests.rs`'s `db_gated` module, `integration_tests.rs`,
and `http::ops` read `MUSE_TEST_DATABASE_URL` directly — so exporting both is what exercises
*every* DB-gated test in one pass rather than only the subset keyed off one var:

```
export MUSE_SNAPSHOT_DATABASE_URL=postgres://user:pass@localhost/muse_dev_test
export MUSE_TEST_DATABASE_URL=postgres://user:pass@localhost/muse_dev_test
cargo test -- --test-threads=1
```

Never point either var at a real/shared host. For the **snapshot-family** DB access (everything
routed through `snapshot::load::connect_snapshot_db`, i.e. the snapshot/fixtures/taste/shadow/
parity tests) this is code-enforced, not just a convention — `snapshot::guard::validate_snapshot_dsn`
rejects any non-loopback host (and any db name lacking a `test`/`snapshot`/`scratch` marker)
outright. The older `MUSE_TEST_DATABASE_URL`-direct paths (`endpoint_tests::db_gated`,
`integration_tests.rs`, `http::ops`, channel tests) connect via `PgPoolOptions::connect` and are
**not** routed through that guard, so for those the loopback-only rule is a documented convention;
what structurally keeps them safe is that none of them ever configures a real upstream client. See
[`docs/TESTING.md`](docs/TESTING.md#2-the-cardinal-invariant-snapshots-everywhere-never-a-live-read)
for the full scope of what the guard does and does not enforce.

### CI (`.gitea/workflows/`, MUSET-10)

Two Gitea Actions workflows wire the phases above into CI as the standing proof gate, splitting
on cost the same way the sections above split on DB-gating:

- **`test-fast.yml`** — runs on **every push and every pull request**, any branch.
  `rustfmt --check`, `clippy --all-targets -D warnings`, then `cargo test -- --test-threads=1`
  with `MUSE_SNAPSHOT_DATABASE_URL`/`MUSE_TEST_DATABASE_URL` left **unset** — every DB-gated test
  above skips cleanly (per its own `*_pool_or_skip`), so this job needs no service container and
  covers the endpoint/golden/mechanics/golden-set/reasoning-review fast tests on every change,
  with no live dependency at all.
- **`test-full.yml`** — runs **on demand** (`workflow_dispatch`) and **nightly**
  (`schedule: cron`). Brings up a `pgvector/pgvector:pg17` service container local to the CI
  runner (`localhost:5432`, throwaway `POSTGRES_USER`/`POSTGRES_PASSWORD`/`POSTGRES_DB` values
  scoped to the job's lifetime only — not a fleet secret, never a literal prod DSN), points
  `MUSE_SNAPSHOT_DATABASE_URL`/`MUSE_TEST_DATABASE_URL` at
  `postgres://muse_ci:muse_ci_scratch@localhost:5432/muse_ci_test`, and runs the full suite. That
  DSN is loopback-hosted with an explicit `_test` marker in the db name, so it independently
  satisfies MUSET-03's `snapshot::guard::validate_snapshot_dsn` guard — the same structural
  guarantee a human running the suite locally gets, not a CI-only exception.

Neither workflow embeds a real hostname, credential, or fleet DSN (S1/S7) — the full job's
Postgres exists only inside that job's runner for that job's duration.
