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
| `MUSE_LIBRARY_ROOT` | *(none)* | MUSEL-B1: filesystem root of the READ-ONLY library scan (a read-only-mounted share, e.g. the QNAP NAS — see MUSEL-B0). `None`/unset (also treated as unset if blank) makes `library::scan::run_scan` a clean no-op — the scanner code + its fixture-dir tests never need this set. |
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
`genres`, `images` (poster/backdrop URLs), `rating`, `first_aired`/`year`, `network`,
`runtime_minutes` (MUSEL-C2, for `matching::verify::verify_match`'s runtime-consistency check —
`None` from `TvdbClient` today, since this crate's TheTVDB v4 parse doesn't carry it yet) — every
field beyond `provider_ids` is independently nullable, since providers disagree on per-title
coverage.

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

`metadata::resolve::resolve_and_merge(ids, kind, providers) -> MuseResult<Option<ResolvedMetadata>>`
is the fan-out-then-merge step on top of the MUSEL-A1 seam above: given a title's already-known
provider ids (`metadata::resolve::ResolveIds` — a `provider_ids: HashMap<String, String>` keyed by
provider name, e.g. `"tmdb"`/`"tvdb"`/`"imdb"`, plus a fallback `title`) and a set of *named*,
already-configured providers (`metadata::resolve::NamedProvider { name, provider: &dyn
MetadataProvider }`), it calls each provider's own `resolve_by_id`, merges the results into one
`ProviderMetadata`, and never fails just because a provider is absent, down, or has nothing for this
id. `ResolvedMetadata { metadata, confidence }` wraps the merge with a
`metadata::resolve::MatchConfidence` marker (`Id` vs `TitleSearch`, see below) — `resolve_and_merge`
returns `Ok(None)` when nothing resolved at all.

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

**Graceful degrade + the title-search fallback's NARROW scope** (tightened in review, S119b):
`providers` empty -> `Ok(None)` (clean no-op, not an error). A provider present but with no id known
for it is skipped for the id-based pass. A provider that errors or returns `Ok(None)` mid-fan-out is
skipped; the rest still merge. The lowest-confidence title-search fallback (each configured
provider's `search(title, kind)`, first hit only) is attempted **only when `ids` carries no known
provider id at all** — if one or more ids WERE supplied and every one of them failed to resolve,
`resolve_and_merge` returns `Ok(None)` rather than guessing by title, since a title guess could
silently attach an unrelated title's data to a row that carries a specific (if currently
unresolvable) id. When the fallback IS taken (no ids known, title present), the result is wrapped
with `MatchConfidence::TitleSearch` and logged at `warn` — never `MatchConfidence::Id`, and never
silently treated as a confident match by a persistence caller.

**Persistence** (`repo::media_metadata::apply_enrichment`, Muse's DB only — never a provider, never
the library) writes a `ResolvedMetadata.metadata` onto an *existing* `media_metadata` row (never
creates one; row creation stays `arr::ingest`'s job) — but callers MUST check `confidence` first:
`maintenance::run_metadata_resolve_pass` (the only wired caller) skips persisting a
`MatchConfidence::TitleSearch` result entirely rather than writing it as if it were authoritative.
`apply_enrichment` itself is strictly **fill-only / add-only**, never a refresh-and-replace:
- `overview`/`year`/`tmdb_id`/`tvdb_id`/`imdb_id` only fill in when the row's own value is currently
  NULL — an existing (possibly curated) value always wins over whatever this pass resolved, even on
  a re-run that resolved something different.
- `provider_ids` is a union with what the row already has (existing keys win on overlap).
- `images` and `keywords` are **add-only**: a new `coverType`/keyword not already present is
  appended; an existing `coverType` entry's URL is left untouched even when the merge produced a
  different URL for the same `coverType` (this is intentionally not a "refresh art from the
  provider" operation — that would be a separate, explicit item). `keywords` persists into the
  `media_metadata.keywords` jsonb array, deduplicated.
- `genres` are additively linked via `genres`/`media_metadata_genres` (find-or-create by name, link
  if not already linked) — never unlinked, so a single provider's genre call never silently removes
  a curator's or another provider's tag.
- `ratings` gets a coarse v1 shape, fill-only: `{"resolved": {"value": <merged rating>}}` is only
  set if the row has no `"resolved"` key yet. `ProviderMetadata::rating` is already a single merged
  scalar by the time it reaches persistence (the per-provider breakdown was collapsed during the
  precedence merge), so there's no richer per-provider key to write yet; a future item could thread
  that through `ProviderMetadata` if the UI ends up wanting per-provider rating badges.

**Wired into the maintenance pass** (`maintenance::run_maintenance_pass`, step (a3)) as an optional,
bounded step: runs only when at least one metadata provider is configured (`state.tmdb` and/or a
freshly-built `metadata::tvdb::TvdbClient::from_config`), right after arr ingest/the wanted worker
and before `embed_stale` (so a freshly-enriched `overview` feeds a richer embedding, not a bare
title/year string). Bounded by `Config::maintenance_enrichment_limit` per `MediaKind` — up to that
many movie rows and that many show rows per pass — via
`repo::media_metadata::find_needing_enrichment` (rows with a known provider id but
`overview IS NULL`, oldest-unsynced-first). Every row's resolve/persist failure is logged and the
pass continues, same error-isolation posture as every other maintenance step; zero providers
configured is a harmless, cheap skip; a `MatchConfidence::TitleSearch` result is logged and skipped
(not persisted), per the persistence rule above.

### Read-only library scan + sidecar art (`src/library/`, MUSEL-B1)

Walks `MUSE_LIBRARY_ROOT` (a read-only-mounted media library — the QNAP NAS share, per the
MUSEL-B0 ops prerequisite), finds media files, matches each to an *already-cataloged*
`media_metadata` row, records the on-disk file, and pulls sidecar art beside it.

**READ-ONLY is a hard, structurally-enforced constraint.** Every filesystem call the scanner
makes into the library is one of `read_dir` / `symlink_metadata` / `metadata` / `File::open`
with `OpenOptions::new().read(true)` — never `.write(true)`/`.create(true)`/`.append(true)`,
and never `fs::remove_*`/`fs::rename`/`fs::write`. This is proven two ways in
`src/library/scan.rs`'s test suite:
- **Structural**: `no_write_create_remove_calls_in_the_scan_and_sidecar_source` `read_dir`s the
  whole `src/library/` production source directory (`mod.rs`/`scan.rs`/`sidecar.rs`, everything
  above each file's own `#[cfg(test)]` block) and greps every `.rs` file for a comprehensive
  banned-pattern set: `File::create`, `.write(true)`, `.create(true)`, `.create_new(true)`,
  `.append(true)`, `.truncate(true)`, `fs::write(`/`std::fs::write`, `fs::remove_file`,
  `fs::remove_dir`/`fs::remove_dir_all`, `fs::rename`, `fs::copy(`, `fs::hard_link`,
  `fs::soft_link`, `fs::symlink(` (kept paren-suffixed so it doesn't false-positive against this
  module's own legitimate, read-only `symlink_metadata` calls), `fs::set_permissions`,
  `fs::create_dir`/`fs::create_dir_all` — failing the build if a future edit (anywhere in the
  module) introduces any write-shaped filesystem call into the library.
- **Behavioral**: `fixture_scan_leaves_the_library_byte_for_byte_unchanged` checksums (relative
  path + size + full bytes) an entire fixture library tree, runs a full walk + sidecar-detect +
  sidecar-read pass over it, checksums again, and asserts the two checksums are identical.

**It never creates a new `media_metadata` row.** The scanner only ever links a file it finds on
disk to a title *already* in Muse's catalog (from `arr::ingest` or a curator) — same posture as
MUSEL-A2's `apply_enrichment` (additive-onto-an-existing-row only). Matching, in order
(`library::scan::DbLibraryResolver`):
1. **Explicit id candidates**, checked in priority order, each resolved against the catalog via
   `repo::media_metadata::find_by_tmdb_id`/`find_by_tvdb_id` (the latter added by this item,
   symmetric with the former):
   - An `{tmdb-NNNN}`/`{tvdb-NNNN}`/`{imdb-ttNNN}` id tag in the path (the Radarr/Sonarr
     folder-naming convention, `library::scan::extract_id_tag`).
   - The `.nfo` sidecar's own embedded id (`sidecar::extract_provider_id_from_nfo` — see "Sidecar
     art + `.nfo`" below), read READ-ONLY during the walk itself so it's available as a matching
     signal, not just something discovered after the fact for caching. This is the *arr suite's
     own identification, arguably the single strongest signal available to the scanner.
   
   **If ANY explicit id candidate is present but NONE of them resolve to a local catalog row, the
   file is `Unmatched` — it never falls through to the title/year match below.** A path tag or a
   `.nfo` id is a specific, caller/tooling-asserted identity claim; falling back to a title/year
   guess after one fails could confidently attach the file to a *different* title's row that
   happens to share a title+year. This mirrors MUSEL-A2's `resolve_and_merge` rule that
   known-but-unresolvable ids never fall back to a title guess.
2. An exact, case-insensitive title+**year** match via `repo::media_metadata::find_by_title_year`
   (already existed, from the Prowlarr report-pull worker) — deterministic, not a fuzzy guess, so
   treated as a confident match. **Reached only when the file carried no explicit id candidate at
   all** (see the refusal rule above), **and only when the filename's parse actually produced a
   year** (codex review): `find_by_title_year` runs a TITLE-ONLY query (no year filter at all) and
   still returns a confident id when its `year` argument is `None` — a behavior other callers of
   that shared helper may legitimately want, so it isn't changed there — but this scanner call site
   deliberately never invokes it that way. A file whose filename carries a title with no parseable
   year (`ParsedRelease.year == None`) skips this branch entirely rather than confidently attaching
   to any same-title catalog row (a remake vs. the original, a different edition, …); it falls
   through to the tentative path below / ends up unmatched.
3. If neither found a local row and metadata providers are configured, a
   `metadata::resolve::resolve_and_merge` title search runs purely to leave a discoverable log
   trail ("resolvable externally, not yet in the local catalog") — its result is always recorded
   as **tentative**, never matched, mirroring `maintenance::run_metadata_resolve_pass`'s own rule
   that a `MatchConfidence::TitleSearch` hit is never auto-persisted as authoritative. There is no
   local row to attach it to regardless of the provider's own confidence.

A file that resolves none of the above is **unmatched**. Because `media_files.media_item_id` and
`media_items.media_metadata_id` are both `NOT NULL` (`migrations/0009_media_files.sql`,
`migrations/0006_media_items.sql`), there is no schema-level "unmatched file" row to attach an
unmatched/tentative file to — extending that schema is out of this item's scope. Unmatched and
tentative files are instead surfaced via `ScanReport::unmatched_paths` and a `tracing` log line,
satisfying the spec's "recorded as unmatched (visible), never a wrong-confident match" — just via
the scan report rather than a `media_files` row specifically. A matched file's release-name parse
(title/year/season/episode) reuses `prowlarr::parse::parse_release_name` — the same deterministic,
no-network parser the Prowlarr report-pull worker already uses; a season marker in the parse picks
`MediaKind::Show`, its absence picks `MediaKind::Movie`.

**Recording**: a confidently-matched file gets a `media_items` row (`repo::media_item::upsert`,
keyed `(library_id, media_metadata_id)`, already idempotent) and a `media_files` row via the new
`repo::media_file::upsert_scanned` — idempotent on `(media_item_id, relative_path)`: an unchanged
`size_bytes` is a clean no-op, a changed one updates the existing row in place, never inserting a
duplicate. (The spec asks for an "mtime/size guard"; `media_files` has no mtime column, so size is
the change signal actually available — documented on `upsert_scanned`.) The file's container
extension is recorded into the existing `media_info` jsonb column as `{"container": "mkv"}` — no
schema change needed, since that column already exists for exactly this kind of file-probing data.

**Sidecar art + `.nfo`** (`src/library/sidecar.rs`): detects `movie.nfo`/`tvshow.nfo`/
`<basename>.nfo`, `poster.jpg`/`.png` (falling back to `folder.jpg`/`.png`), `fanart.jpg`/`.png`
(`backdrop.jpg`/`.png`), and per-season `season##-poster.*` beside the media file —
`sidecar::detect` (a directory listing only, no content read) runs once per file during the walk
and is cached on `ScannedFile::sidecar_art` so both matching and recording reuse the same pass.
**Poster/fanart selection is deterministic by declared priority order, not `read_dir` order**
(codex review): when a directory carries more than one candidate for the same slot (e.g. both
`poster.jpg` and `folder.jpg`), `detect` collects every present candidate first, then selects by
walking `POSTER_NAMES`/`FANART_NAMES` in their own declared array order — never by whichever one
`std::fs::read_dir` (an unspecified iteration order) happened to yield first. Without this, the
same, unchanged directory could select a different file across scans, and `cache_if_changed`
below would then needlessly churn `artwork_cache` even though nothing on disk actually changed.
**Every candidate is symlink-checked via `std::fs::symlink_metadata` (never `Path::is_file()`,
which follows symlinks) and rejected unless it's a genuine regular file** (codex review) — the
same read-only-root-boundary posture `walk_dir` already enforces for media files, now applied to
sidecars too: a `poster.jpg`/`.nfo` symlinked to somewhere outside `MUSE_LIBRARY_ROOT` is never
detected, read, or attached. `sidecar::read_bytes` re-checks this itself as defense-in-depth for a
caller that invokes it directly.

- **`.nfo` — used for matching, not just cached.** `sidecar::extract_provider_id_from_nfo` reads a
  detected `.nfo`'s bytes READ-ONLY and extracts an embedded provider id: `<uniqueid
  type="tmdb"/"imdb"/"tvdb">…</uniqueid>` (current Kodi/`*arr` schema, any attribute order/quoting)
  or the legacy flat `<tmdbid>`/`<imdbid>`/`<tvdbid>`/bare `<id>` tags (older Kodi `movie.nfo`,
  `<id>` conventionally the TMDb id). Deliberately a plain substring scan, not a real XML parser —
  matches the crate's no-new-heavy-dependency posture for a handful of well-known tags — but
  **tag-scoped, not a whole-document search** (codex review): the `type` attribute is only ever
  read from within a genuine `<uniqueid ...>` element's own opening-tag bounds, and the element
  text only from that same element's own `</uniqueid>` close. A naive whole-document search for
  `type="tmdb"` would also match an unrelated element like `<rating type="tmdb">603</rating>` and
  mis-read it as a confident provider id — which, since an explicit id suppresses the title/year
  fallback, would produce a WRONG confident match against an unrelated catalog row. Feeds straight
  into `DbLibraryResolver` as an explicit id candidate (see "Matching" above) — subject to the same
  "fails to resolve -> unmatched, never a title/year guess" rule as a path id tag.
- **`.nfo` — attached, not just detected.** A matched file's `.nfo` bytes are also read READ-ONLY
  and cached into `artwork_cache` under `entity_kind = "media_item"`, `variant = "nfo"`,
  `content_type = "application/xml"` — the same table/key convention poster/fanart use — and
  counted in `ScanReport::nfo_attached`, so `.nfo` detection→attachment is complete and observable,
  not silently dropped (a prior revision detected the `.nfo` but never read/used/reported it).
- **Poster/fanart** bytes are read READ-ONLY (`sidecar::read_bytes`) and cached into
  `artwork_cache` the same way, `entity_kind = "media_item"` — the same key convention
  `web::artwork::art_handler` already reads from, so a scanned file's art (and now its `.nfo`) is
  immediately retrievable with no further wiring (`GET /art/media_item/{id}?variant=nfo` etc.).
- **Idempotent per-sidecar, decoupled from the media file's own change status.** Sidecar
  detection/attachment always runs for a matched file — it is deliberately NOT gated on
  `upsert_scanned`'s media-file size guard (an earlier revision skipped all sidecar work whenever
  the media file was unchanged, which meant a poster/fanart/`.nfo` added or edited *after* the
  first scan — a curator drops in a better poster, `*arr` rewrites the `.nfo` — was never picked
  up on a later rescan of that same, byte-identical file). Instead, `cache_if_changed` (shared by
  `cache_art`/`cache_nfo`) looks up whatever is already cached for `(media_item_id, variant)` and
  skips the write only when the bytes are byte-for-byte identical to what's already there;
  otherwise it (re-)writes. Net effect: a rescan where nothing changed (media file AND every
  sidecar identical) still does zero writes and reports `art_cached`/`nfo_attached` as `0`, but a
  rescan that finds a newly-added or edited sidecar attaches it — even when the media file next to
  it hasn't moved at all.
- No local sidecar art/`.nfo` found is not an error — the existing artwork-proxy path (Plex fetch,
  then the built-in placeholder) already covers "fall back to a provider re-fetch."

**Entry point**: `library::scan::run_scan(pool, config, providers)` is a clean no-op when
`config.library_root` (`MUSE_LIBRARY_ROOT`) is unset — logged at `debug`, no filesystem or DB
touch at all. When set, it scans every enabled `libraries` row whose `root_folder` falls under
`MUSE_LIBRARY_ROOT`, using `library::scan::path_is_within_root` for that containment check —
**path-component-aware** (`Path::starts_with`, canonicalizing when both sides exist on disk and
falling back to a lexical-but-still-component-aware comparison otherwise), not a raw
`str::starts_with`: `/mnt/library2` is correctly rejected as "inside" `/mnt/library` even though
the *string* `"library2"` starts with `"library"`. **Not wired into a worker/route yet** (out of
this item's scope, matching the "ships the primitive, wiring is a later item" posture MUSEM-02's
download adapter also documents above) — call it from an operator CLI subcommand or the
maintenance pass in a follow-up item. Per-file failures (a bad file, a resolver error, a DB write
failure) increment `ScanReport::errors` and the pass continues; a directory that can't be listed
mid-walk is logged and that subtree is skipped, not the whole pass. Symlinks under the library root
are detected via `symlink_metadata` and never followed — a deliberate, documented skip (a symlink
out of the read-only root would otherwise let the walk escape it; a symlink loop would otherwise
hang it).

Tests: `cargo test --bin muse library::` — walk/parse/match(mocked resolver)/sidecar-detect all
run against a fixture directory tree (`std::env::temp_dir()` + a unique per-test subdir, no
`tempfile` dependency needed), no live QNAP mount or DB required for those. The full record path
(`scan_library_end_to_end_matches_records_and_caches_art_idempotently`) and
`repo::media_file`'s `upsert_scanned_is_idempotent_and_updates_only_on_a_size_change` are gated on
`MUSE_TEST_DATABASE_URL`, same skip-cleanly-when-unset posture as every other live-DB test in this
crate.

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

