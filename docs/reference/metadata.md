# metadata

The normalized metadata-provider seam (88 KG nodes, MUSEL-A1). Muse's original TMDb
integration (`trending::client::TmdbClient`) is a single hardwired source; this module
defines a provider-agnostic `MetadataProvider` trait plus a normalized
`ProviderMetadata` shape that any concrete provider (TMDb, TheTVDB, …) resolves into, so
the resolver/aggregator can fan out across several configured providers rather than
being TMDb-specific. **Read-only**: a provider is only ever looked up, never written to.

## Key types and functions

| Symbol | Kind | File | What it does |
|---|---|---|---|
| `metadata::MetadataProvider` | trait | `src/metadata/mod.rs` | The provider seam: lookup/search returning normalized `ProviderMetadata` |
| `metadata::MediaKind` | enum | `src/metadata/mod.rs` | Movie vs Series, generalized across providers |
| `metadata::tvdb::TvdbClient::new` | fn | `src/metadata/tvdb.rs` | Constructs the TheTVDB v4 client |
| `metadata::tvdb::TvdbClient::login` | fn | `src/metadata/tvdb.rs` | v4 login: API key (+ optional subscriber PIN) → bearer token |
| `metadata::tvdb::TvdbClient::get_with_reauth` | fn | `src/metadata/tvdb.rs` | GET with automatic one-shot re-login on auth expiry — the client's central request path |
| `metadata::tvdb::TvdbClient::get_data` | fn | `src/metadata/tvdb.rs` | Typed deserialization over `get_with_reauth` (TVDB's `{"data": …}` envelope) |
| `metadata::resolve::NamedProvider` | struct | `src/metadata/resolve.rs` | A provider tagged with its name, for multi-provider resolution |
| `metadata::MockMetadataProvider` | struct | `src/metadata/mod.rs` | Test double for the trait |

## How it connects

`Config::tvdb()` assembles `metadata::config::TvdbConfig` from the `MUSE_TVDB_*` fields
— the client never reads env vars itself; `TvdbClient::from_config` returns `None` when
the key is unset, the same graceful-degrade posture as every other optional integration.
Resolved metadata flows into `repo::media_metadata` (`upsert_by_tmdb` remains the
title-level convergence point; TVDB/IMDb ids bridge through TMDb's find surface).
The library scanner and matching-verification use resolved metadata to confirm what a
scanned file actually is.

## Configuration

- `MUSE_TVDB_API_KEY` — TheTVDB v4 API key (secret-wrapped in `QbitPassword` so a stray
  `Debug` of `Config` can't print it).
- `MUSE_TVDB_PIN` — optional subscriber PIN for subscription-model keys.
- `MUSE_TVDB_BASE_URL` — base-URL override for tests/proxies; unset means TheTVDB's real
  host (`DEFAULT_BASE_URL` in `src/metadata/tvdb.rs`).
- `TMDB_API_KEY` — the TMDb side (read by `trending::client::TmdbClient`, which predates
  this seam and is being folded in behind it).

## Notes and gaps

- TVDB v4 `/extended` responses are what the client targets; the TMDb/IMDb id bridge was
  built in the same sprint (S119b) — see the sprint build report in the repo root.
- `trending::TmdbClient` is not yet re-homed under this module; the trait exists so that
  migration is additive.
- Not covered here: artwork caching (`repo::artwork_cache` + the `/art` proxy in `web`).
