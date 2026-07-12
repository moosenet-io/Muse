# Muse — Data Model

The Postgres database is Muse's system of record. This document describes the shipped schema
(`migrations/0000`–`0098`, applied by sqlx in filename order) grouped by concern, and calls out the
**structural divergences from the founding spec** that shipped — these are deliberate and
documented in the migration headers themselves. All timestamps are `timestamptz` (UTC). Rust row
structs live in `src/models/*.rs` and match the SQL column names 1:1 (snake_case preserved).

Requires **PostgreSQL 17** with extensions `vector` (pgvector) and `pg_trgm`. Enums:
`media_kind('movie','show')`, `library_kind('movie','tv')`, `release_type_kind('single','multi','season_pack')`,
`decision_kind('direct_play','direct_stream','transcode','copy')`, `interstitial_kind`,
`channel_kind`, `channel_mode`, `channel_run_status`, `channel_program_item_type`.

## Key divergences from the spec (read this first)

The shipped schema is more *arr-faithful than the spec's simplified sketch. Six divergences matter
for anyone reasoning about the data:

1. **metadata / instance split.** The spec had one flat `media_items` table. Shipped: **`media_metadata`**
   (`0005`) is the shared, provider-keyed title record (one row per real-world title: tmdb/tvdb/imdb
   ids, title, overview, ratings, airing dates); **`media_items`** (`0006`) is a thin *per-library
   instance* row (`library_id`, `media_metadata_id`, `path`, `monitored`, `quality_profile_id`,
   `plex_rating_key`) with `UNIQUE(library_id, media_metadata_id)`. The same title present in two
   libraries (e.g. `radarr` and `radarr_uhd`) gets two `media_items` rows sharing one `media_metadata`.
   Title-level facts (people, genres, collections, `releases`, `availability`, `trending_snapshots`,
   `streaming_availability`) key off `media_metadata_id`; per-instance / telemetry facts (`watch_stats`,
   `ratings`, `watchlist`, `taste_signals`, `play_sessions`, `proactive_items`) key off `media_items.id`.

2. **3-level TV hierarchy.** The spec used a flat `media_children` table. Shipped: **`media_items`
   (show) → `seasons` (`0007`) → `episodes` (`0008`)** as three separate tables. The `media_kind`
   enum was narrowed to `('movie','show')` only — season/episode are first-class tables, not
   enum-discriminated rows.

3. **compound quality model.** Beyond the spec's `quality_profiles.items jsonb`, shipped adds
   `quality_definitions` (`0003`, semantic quality tiers with size limits + `sort_order`),
   `custom_formats` (`0004`, named scored matcher rules), and a real join `quality_profile_formats`
   (`0004`, `(quality_profile_id, custom_format_id, score)`) — Sonarr/Radarr FormatItems parity.
   `media_files` (`0009`) carries compound quality as `quality_tier_id` (FK) + flattened
   `revision_version`/`revision_real`/`revision_is_repack` (PROPER/REPACK/REAL), reassembled in Rust
   via `MediaFile::revision()`. **The custom-format scorer is a documented seam** — the schema stores
   and round-trips these rules, nothing evaluates releases against them yet.

4. **composite-FK same-show integrity.** `seasons`, `episodes`, and `media_files` each carry a
   `UNIQUE(id, media_item_id)` superkey. `episodes` declares `FOREIGN KEY (season_id, media_item_id)
   REFERENCES seasons(id, media_item_id)`, and the `episode_files` join (`0010`) declares two composite
   FKs — `(episode_id, media_item_id) → episodes` and `(media_file_id, media_item_id) → media_files`.
   This structurally prevents an episode or a season-pack file from ever being linked across shows.
   `episode_files.media_item_id` is derived at insert time (`repo::media_file::attach_to_episode`),
   never caller-supplied.

5. **`NULLS NOT DISTINCT` dedup on `play_sessions`.** `0015` created
   `UNIQUE(account_id, media_item_id, episode_id, started_at)`. Under default Postgres NULL semantics
   a movie play (`episode_id` NULL) or an unresolved play (both NULL) never conflicts with itself, so
   the reconstruction upsert would INSERT duplicates on every re-run. `0023` re-adds the constraint as
   `play_sessions_natural_key_uniq UNIQUE NULLS NOT DISTINCT (...)` (PG15+) so those NULLs compare
   equal and the tuple is a true idempotency key.

6. **embeddings `vector(768)` + HNSW; `taste_divergence` overlap-based, not centroid-based.**
   `embeddings` (`0018`) pins `vector(768)` (nomic-embed-text, build-time width) with
   `CREATE INDEX ... USING hnsw (embedding vector_cosine_ops)`. `0013` and `0040` both
   `CREATE EXTENSION IF NOT EXISTS vector` — `0040` is a *defensive duplicate* guarding branch-merge
   order for `population_profile.mainstream_centroid`, not a second feature. `taste_divergence` (`0044`)
   matches the spec's column shape, but `mainstream_score` is computed from genre/decade **distribution
   overlap** rather than the spec's literal "cosine of centroids", because no embeddings pipeline feeds
   the radar (see `src/radar/divergence.rs`); same 0..1 range, no embeddings dependency.

## arr-shaped core library

| Table | Keyed by | Notes |
|---|---|---|
| `libraries` | `id` (`name` UNIQUE) | First-class multi-instance dimension: one row per *arr instance (`kind`, `root_folder`, `source_arr_name`/`url`, `enabled`). |
| `media_metadata` | `id`; `UNIQUE(kind,tmdb_id)`, `UNIQUE(kind,tvdb_id)` | Shared provider-keyed title: ids, title/sort/clean titles, overview, tagline, studio/network, certification, runtime, year, airing dates, `images`/`keywords`/`ratings`/`recommendations` jsonb, popularity, collection ids. GIN trigram index on `title`. |
| `media_items` | `id`; `UNIQUE(library_id, media_metadata_id)`, `UNIQUE(plex_rating_key)` | Per-library instance: `path`, `monitored`, `in_library`, `quality_profile_id`, `minimum_availability`, `plex_rating_key`, `added_at`, `last_search_time`. |
| `seasons` | `id`; `UNIQUE(media_item_id, season_number)`, superkey `UNIQUE(id, media_item_id)` | `season_number`, `title`, `overview`, `monitored`, `air_date`. |
| `episodes` | `id`; `UNIQUE(season_id, episode_number)`, `UNIQUE(plex_rating_key)`, superkey `UNIQUE(id, media_item_id)` | Composite FK to `seasons(id, media_item_id)`. Scene numbering columns, `absolute_episode_number`, `air_date`/`air_date_utc`, `monitored`, `has_file`, `tvdb_id`. |
| `media_files` | `id`; superkey `UNIQUE(id, media_item_id)` | `relative_path`, `size_bytes`, `media_info` jsonb, `release_group`, `edition`, `languages[]`/`subtitles[]`, `indexer_flags` bitmask, `release_type` enum, compound quality (`quality_tier_id` + `revision_*`). Movies: 1:1 via `media_item_id`. TV: many-to-many via `episode_files`. |
| `episode_files` | PK `(episode_id, media_file_id)` | Join with two composite same-show FKs (see divergence 4). Carries `media_item_id`. |
| `quality_definitions` | `id` (`quality_key` UNIQUE) | Semantic quality tier: `title`, `source`, `resolution`, `modifier`, size limits (`*_mb_per_min`), `sort_order`. |
| `quality_profiles` | `id` (`name` UNIQUE) | `cutoff_quality_id` FK, `items` jsonb (ordered allowed qualities), format-score gates (`min_format_score`/`cutoff_format_score`/`min_upgrade_format_score`), `natural_language_intent` (Phase-1 seam). |
| `custom_formats` | `id` (`name` UNIQUE) | `specifications` jsonb (matcher rules — **stored, not evaluated: scorer seam**), `include_when_renaming`. |
| `quality_profile_formats` | PK `(quality_profile_id, custom_format_id)` | Per-profile custom-format `score`. |
| `people` / `media_metadata_credits` | — | `people(tmdb_person_id UNIQUE, name, known_for_department)`; credits keyed off `media_metadata_id` (title-level), `role`/`character`/`cast_order`. |
| `genres` / `media_metadata_genres` | — | Genres keyed off `media_metadata_id`. |
| `collections` / `media_metadata_collections` | — | Plex/TMDb/Muse collections keyed off `media_metadata_id`. |
| `tags` / `media_item_tags` | — | Tags stay on `media_items` (operator-set per instance, not provider metadata). |
| `accounts` | `id` (`plex_account_id` UNIQUE) | Plex managed/home users. Taste is **per-account, never blended**. `is_home_user`, `is_primary`. |

## Telemetry — the Tautulli-equivalent tracker (the taste fuel)

| Table | Keyed by | Notes |
|---|---|---|
| `play_events` | `id`; `UNIQUE(source, event_type, session_key, view_offset_ms)` | Append-only raw event stream. `source` = `plex_webhook`/`plex_poll`/`tautulli_backfill`; `event_type` = `media.play`/`pause`/`resume`/`stop`/`scrobble`/`rate`; `account_ref`, `session_key`, `rating_key`, `view_offset_ms`, player/platform/product/device, `ip_address`, full `raw` jsonb. (Spec's dedup key included `received_at`; dropped so retries actually dedup.) |
| `play_sessions` | `id`; `play_sessions_natural_key_uniq NULLS NOT DISTINCT (account_id, media_item_id, episode_id, started_at)` | Reconstructed sessions. `session_key`, `tautulli_ref_id` (provenance), timing (`started_at`/`stopped_at`/`duration_ms`/`watched_ms`/`view_offset_ms`/`percent_complete`), `paused_counter`/`paused_ms`, `is_finished`, `is_abandoned`, context (`started_hour`/`started_dow`/`is_cinema_context`). FKs `ON DELETE SET NULL` to preserve history. |
| `play_session_media_info` | PK `play_session_id` | 1:1 quality/decision detail: `video`/`audio`/`transcode_decision` enums, container/codecs/channels, resolution, bitrate, dimensions, `transcode_reason`. |
| `watch_stats` | PK `(account_id, media_item_id)` | Derived aggregates: `play_count`, `finished_count`, `rewatch_count`, `total_watched_ms`, `avg_percent`, `last_watched_at`, `abandoned`, `first_watched_at`. |
| `ratings` | PK `(account_id, media_item_id)` | Plex user rating (0-10), `rated_at`. |
| `watchlist` | PK `(account_id, media_item_id)` | `added_at`/`removed_at`, `fulfilled` (intent→action). |

## Taste model + embeddings (pgvector)

| Table | Keyed by | Notes |
|---|---|---|
| `embeddings` | `id`; `UNIQUE(entity_kind, entity_id, model)` | `entity_kind`/`entity_id` (keyed to `media_item.id` in practice), `model`, `dim` (default 768), `embedding vector(768)`, `source_text` (also the change-detection key — no hash column). HNSW cosine index `embeddings_hnsw`. |
| `taste_profile` | PK `account_id` | `genre_affinity`/`person_affinity`/`keyword_affinity` jsonb (keyword also nests a `decades` sibling — no dedicated decade column), `runtime_pref`, `quality_sensitivity` (deferred, always NULL in v0), `overall_centroid vector(768)`, `model_notes` (LLM prose). |
| `taste_context_centroids` | PK `(account_id, context_key)` | Per-context centroid (`{weekend|weekday}_{morning|daytime|evening|late_night}`) + `sample_size`. |
| `taste_signals` | `id` | Auditable weighted atoms: `signal_type` (`finished`/`abandoned`/`rewatched`/`rated`/`watchlisted`/`curation_note`), `weight`, `context_key`, `note`, `observed_at`. |
| `proactive_items` | `id` | Outbox → Lumina. `kind`, `media_item_id`, `headline`, `body` jsonb, `priority`, `earliest_at`/`expires_at`/`delivered_at`, plus (`0036`) `dedup_key`, `status` (default `pending`), `dismissed_at`. |
| `external_enrichment` | `id`; `UNIQUE(media_item_id, kind, source)` | Enrichment cache: `kind`/`source`, `payload` jsonb, `confidence`, `fetched_at`, `ttl_seconds` (default 604800). |

## Availability / release reports (Prowlarr)

| Table | Keyed by | Notes |
|---|---|---|
| `indexers` | `id` (`prowlarr_id` UNIQUE) | `protocol`/`privacy`/`enabled`, `categories int[]`, `last_rss_pull_at`, `polite_min_interval_secs` (default 900). |
| `releases` | `id`; `UNIQUE(indexer_id, guid)` | Rolling snapshot **keyed to `media_metadata_id`** (title-level grabbability) + `episode_id`. Raw `title`, urls/hash, size, `publish_date`, torrent health (`seeders`/`leechers`/`grabs`), `freeleech`/`freeleech_pct`, `categories`, and the deterministic parse columns (`parsed_title`/`parsed_year`/`quality`/`resolution`/`source`/codecs/`hdr[]`/`edition`/`release_group`/`proper_repack`/`languages`/`subtitles`/`parse_confidence`), `first_seen_at`/`last_seen_at`/`expires_at`. |
| `availability` | PK `media_metadata_id` | Per-title rollup: `best_quality`, `best_seeders`, `release_count`, `has_freeleech`, `cheapest_size_bytes`, `newest_release_at`, `computed_at`. |

## Population consumption + you-vs-masses radar

| Table | Keyed by | Notes |
|---|---|---|
| `trending_snapshots` | `id`; `UNIQUE(source,scope,platform,region,window,rank,captured_at)` | "The masses." `source`/`scope`/`platform`/`region`/`window`/`rank`, resolved `media_metadata_id` or `external_ref` jsonb, `popularity`, `captured_at`. |
| `streaming_availability` | PK `(media_metadata_id, provider, region, offer_type)` | Where a title streams (TMDb `/watch/providers`): `flatrate`/`ads`/`rent`/`buy`, `link`, `seen_at`. |
| `population_profile` | `id` (append-only, no unique key) | Aggregate mainstream rollup: `genre_distribution`/`decade_distribution`/`runtime_distribution` jsonb, `mainstream_centroid vector(768)`, `sample_size`. (MUSE-19 ships storage only; `radar::compute_population_distributions` fills the genre/decade distributions, the rest stay NULL/placeholder.) |
| `taste_divergence` | `id` (append-only, no unique key) | Per-account radar snapshot, tracked over time: `genre_index`/`decade_index` jsonb (>1 over-index), `mainstream_score`/`adventurousness`/`contrarian_index` (all 0..1), `were_early`/`blind_spots`/`guilty_pleasures` jsonb arrays. |

## Channels / interstitials / programs / artwork

| Table | Keyed by | Notes |
|---|---|---|
| `plex_clients` | `id` (`machine_identifier` UNIQUE) | Discovered cast targets (name/product/device/platform/address/port, `protocol_caps[]`, `is_cast_target`, `last_seen_at`). Written only by the unwired `plex_control` module. |
| `interstitials` | `id` (`plex_rating_key` UNIQUE) | Bumpers/commercials/idents pool: `kind` enum, `decade`, `theme`/`genre`/`mood`, `duration_ms`, `tags[]`, `source`, plus (`0098`) `file_path` (nullable — for the ffmpeg engine, which needs a local path not just a rating key). |
| `channels` | `id` (`channel_number` UNIQUE) | `account_id` (nullable, no FK), `kind` enum, `mode` enum (`on_demand`/`linear`), `channel_number`, `target_client_id` FK, `directive` (NL brief), `rules` jsonb, `is_preset`. |
| `channel_runs` | `id` | A composed/played schedule instance: `channel_id`, `target_client_id`, `plex_play_queue_id`, `schedule` jsonb (`{rationale, items:[...]}`), `total_duration_ms`, timing, `status` enum. |
| `channel_programs` | `id`; `UNIQUE(channel_id, start_at)` | Linear time-anchored grid (drives XMLTV + web guide): `item_type` enum, one of `media_item_id`/`episode_id`/`interstitial_id` (CHECK: at least one), `title`/`subtitle`/`description`/`artwork_url`, `start_at`/`end_at` (CHECK end>start)/`duration_ms`, `rationale`, `play_event_id` (seam). |
| `artwork_cache` | `id`; `UNIQUE(entity_kind, entity_id, variant)` | Muse-proxied artwork: `source_url`, `content_type`, `bytes bytea` (cached **directly in Postgres**, not on disk as the spec sketched), `etag`, `fetched_at`. Keeps the Plex token out of the browser. |

For how these tables are populated and read at runtime, see [`runbooks.md`](runbooks.md) and
[`behavior-spec.md`](behavior-spec.md).
