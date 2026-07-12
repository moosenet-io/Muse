# Muse — AI-Native Media Curation & Taste Companion (Founding Spec · Phase 0)
plane_project: MUSE
module: Muse
prefix: MUSE
spec_id: S96-muse-foundation

## Metadata
- **Author:** <operator> (Moose) + Claude (design)
- **Session:** S96
- **Date:** 2026-07-11
- **Muse version:** 0.1.0 (Phase 0 — Foundation, Visibility & Taste)
- **Peer to:** Harmony (build orchestrator), Chord (proxy/inference), Terminus (tool hub), Lumina (assistant)
- **Built by:** Harmony (moosenet-spec pipeline). **Reasoning + embeddings run on:** <host> in-home inference via Chord/Ollama.
- **Context:** Muse is a NEW standalone Rust service — a private, AI-native media **curation + library-taste** engine
  and **companion to Lumina**, built around a **mandatory Postgres + pgvector** database. Plex/self-hosting was the
  origin of moosenet; Muse makes it a first-class, sovereign, in-home-inference experience. It is NOT an *arr reskin
  and NOT a chat wrapper: it owns the **brain** (taste, curation, metadata, release selection, organization) while
  keeping **qBittorrent** (acquisition) and **Plex** (consumption). It is built **strangler-fig** — every phase ships
  independent value; *arr AND Tautulli are retired one function at a time; **import (the only high-blast-radius part)
  is dead last.** The design north star: *never a monolith that only works when finished.*

  This founding spec fully specifies **Phase 0** (read-only, zero blast radius): stand up the DB, migrate the
  library + full watch history, **replace Tautulli's tracking natively**, embed metadata for assistant-speed recall,
  build the behavioral taste model, and produce **proactive content for Lumina**. It also carries the whole-project
  roadmap and the complete reference schemas.

---

## 0. Chosen defaults (the founding decisions — override at ingest if desired)
1. **Name / prefix:** `Muse` / `MUSE` (prefix verified free in the registry).
2. **Phase-0 scope:** READ-ONLY taste + curation + visibility companion. No acquisition/organize/delete in P0.
3. **TERM-226 / TERM-227** (the Terminus media-domain Epic follow-ups: TMDb→TVDb bridge, real release-size for the
   oversize tier): **deferred into Muse** (they are Phase-1/2 acquisition problems, solved properly in Muse's AI
   selection engine — not patched into the Terminus orchestration layer). The shipped Terminus `media_*` domain
   remains the tool/voice surface and re-points to Muse as phases land.
4. **Curation reasoning model:** Chord-routed local model on <host>; **interim default `qwen3-coder:30b`** (already
   resident) for reasoning until a Harmony curation-model sweep picks a chat/instruct-tuned model. Muse must
   coordinate VRAM via Chord (lemonade-coder holds the GPU — never contend blindly).
5. **Taste store:** lives in Muse's Postgres (grown from the MEDIA-06 signal seed) — a dedicated relational + vector model.
6. **Postgres host:** a dedicated **`muse` database** on the fleet PG box (candidate `lumina_intake` @ `LUMINA_INTAKE_PG_HOST`),
   **Postgres 16+ with `pgvector` ≥ 0.7**. (A dedicated instance is a later option if load warrants.)
7. **Embedding model:** **`nomic-embed-text`** on Ollama/<host>, **768-dim** → `vector(768)` columns, HNSW index
   (cosine). (Alternative `bge-large-en-v1.5` 1024-dim; the dim fixes the column width, so this is a build-time pin.)
8. **Telemetry ingest:** (a) **one-time Tautulli API backfill** (all-time history), (b) **ongoing NATIVE capture**
   via **Plex webhook + `/status/sessions` poller** (this is the Tautulli *replacement*), (c) capture Plex **ratings
   + Watchlist** as explicit signals. All strictly read-only against Plex/Tautulli — never write to their DBs.

---

## 1. Whole-project roadmap (strangler-fig; each phase independently shippable)
- **Phase 0 (this spec):** Postgres+pgvector foundation · library+history migration · **native Plex tracker
  (replaces Tautulli)** · **Prowlarr availability report-pull** · **trending/population feed + you-vs-masses radar** ·
  local embeddings + vector recall · behavioral taste model · proactive content for Lumina. *Read-only; zero blast radius.*
- **Phase 0.5 (this spec):** **Channels — the pseudo-TV director** (agentic LLM-composed lineups + interstitials),
  in four independently-useful sub-milestones: (i) **on-demand** cast play-queues to Chromecast/TV, (ii) a **web
  lineup guide** (EPG grid, covers, timelines), (iii) **linear "Muse TV" channels in the Plex guide** via HDHomeRun-
  emulation + M3U + XMLTV (Plex already wired to your HDHomeRun), (iv) the **ffmpeg streaming engine** (continuous
  channels with join-mid-stream). *Benign playback only — plays media, never mutates the library; light-confirm.*
- **Phase 1:** AI **release selection** (local-LLM parses candidate releases, reasons about fit vs stated prefs) →
  drives *arr/qbit to execute. Reversible.
- **Phase 2:** Direct **indexer + qbit** control (acquisition migrates off *arr).
- **Phase 3:** **Import / organize / hardlink** (dry-run-first, atomic, hard-confirm) → *arr fully retires. Last.

Stop after any phase and what exists is useful. Import is last and most-guarded.

---

## 2. Architecture & data flow (Phase 0)
```
   Plex ──(webhook: play/pause/resume/stop/scrobble/rate)──►  Muse /ingest/plex-webhook ─┐
   Plex ──(poll /status/sessions every ~10s)─────────────►  Muse session poller ────────┤
   Tautulli ──(one-time API backfill: get_history/metadata)──►  Muse backfill importer ──┤
   Radarr/Sonarr ──(library/quality/files)──────────────────►  Muse library ingest ──────┤
   Prowlarr ──(RSS report-pull + targeted search: real releases)──►  Muse availability ────┤  ◄── grabbable NOW
                                                                                           ▼
                                          ┌──────────────  Postgres ( muse )  ──────────────┐
                                          │  media_items · media_files · quality_profiles   │
                                          │  play_sessions · play_events · media_info       │  ◄── Tautulli-equivalent
                                          │  watch_stats (derived) · ratings · watchlist    │
                                          │  indexers · releases · availability             │  ◄── Prowlarr report-pull
                                          │  taste_profile · taste_signals · embeddings     │
                                          └───────────────────────┬─────────────────────────┘
   Ollama/<host> (nomic-embed) ──embeddings──►  pgvector columns ────┤
   Chord/<host> (qwen3) ──reasoning──►  taste model + curation ──────┤
                                                                    ▼
   Muse HTTP/tool API ──►  Terminus media_* surface ──►  Lumina  (conversation + PROACTIVE content)
```
Muse is an `axum` HTTP service (health/ingest/query/proactive endpoints) + background workers (poller, embedder,
taste-recompute, proactive-scheduler), all over `sqlx`→Postgres. Secrets via the fleet <secret-manager>→env pattern.

---

## 3. REFERENCE SCHEMA (Postgres DDL — the system of record)
> Migrations live in `migrations/` (sqlx). All timestamps `timestamptz` UTC. All external IDs kept as text to
> survive provider quirks. `pgvector` dim pinned to the embedding model (768 for nomic-embed-text).

### 3.1 Extensions & conventions
```sql
CREATE EXTENSION IF NOT EXISTS vector;      -- pgvector
CREATE EXTENSION IF NOT EXISTS pg_trgm;     -- trigram fuzzy fallback alongside vector
-- enums
CREATE TYPE media_kind    AS ENUM ('movie','show','season','episode');
CREATE TYPE play_state    AS ENUM ('playing','paused','stopped','buffering');
CREATE TYPE decision_kind AS ENUM ('direct_play','direct_stream','transcode','copy');
```

### 3.2 arr-shaped core library (mirrors Radarr/Sonarr so migration is a data-copy)
```sql
-- Accounts (Plex managed/home users) — taste is per-account, never blended.
CREATE TABLE accounts (
  id              bigserial PRIMARY KEY,
  plex_account_id text UNIQUE,               -- Plex accountID
  username        text,
  friendly_name   text,
  is_home_user    boolean NOT NULL DEFAULT false,
  is_primary      boolean NOT NULL DEFAULT false,
  created_at      timestamptz NOT NULL DEFAULT now()
);

-- One row per movie or show (grandparent). Seasons/episodes hang off media_children.
CREATE TABLE media_items (
  id                bigserial PRIMARY KEY,
  kind              media_kind NOT NULL,             -- 'movie' | 'show'
  title             text NOT NULL,
  sort_title        text,
  original_title    text,
  year              int,
  overview          text,
  runtime_minutes   int,
  content_rating    text,                            -- e.g. 'TV-MA','R'
  studio            text,
  tagline           text,
  -- cross-provider IDs (all optional; the migration/enrichment fills what it can)
  tmdb_id           text,
  tvdb_id           text,                            -- Sonarr id-space (TERM-226 lives here)
  imdb_id           text,
  plex_rating_key   text,                            -- Plex grandparent ratingKey
  radarr_id         text,
  sonarr_id         text,
  -- library/acquisition state (arr-parity)
  monitored         boolean NOT NULL DEFAULT false,
  in_library        boolean NOT NULL DEFAULT false,  -- present in Plex/arr
  quality_profile_id bigint REFERENCES quality_profiles(id),
  root_folder_path  text,
  added_at          timestamptz,
  -- scores/ratings (provider + community; user ratings live in ratings table)
  tmdb_rating       real,
  community_rating  real,
  popularity        real,
  metadata_synced_at timestamptz,
  created_at        timestamptz NOT NULL DEFAULT now(),
  updated_at        timestamptz NOT NULL DEFAULT now(),
  UNIQUE (kind, tmdb_id),
  UNIQUE (plex_rating_key)
);
CREATE INDEX ON media_items USING gin (title gin_trgm_ops);
CREATE INDEX ON media_items (tvdb_id);
CREATE INDEX ON media_items (imdb_id);

-- Seasons + episodes (Sonarr parity). parent_id chains episode→season→show.
CREATE TABLE media_children (
  id               bigserial PRIMARY KEY,
  show_id          bigint NOT NULL REFERENCES media_items(id) ON DELETE CASCADE,
  parent_id        bigint REFERENCES media_children(id) ON DELETE CASCADE,   -- episode→season
  kind             media_kind NOT NULL,               -- 'season' | 'episode'
  season_number    int,
  episode_number   int,
  absolute_number  int,                               -- anime absolute numbering
  title            text,
  overview         text,
  air_date         date,
  runtime_minutes  int,
  plex_rating_key  text,
  tvdb_id          text,
  monitored        boolean NOT NULL DEFAULT false,
  has_file         boolean NOT NULL DEFAULT false,
  created_at       timestamptz NOT NULL DEFAULT now(),
  updated_at       timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX ON media_children (show_id, season_number, episode_number);

-- Physical files (MovieFiles/EpisodeFiles parity) — the import layer (Phase 3) will own writes here.
CREATE TABLE media_files (
  id               bigserial PRIMARY KEY,
  media_item_id    bigint REFERENCES media_items(id) ON DELETE CASCADE,
  media_child_id   bigint REFERENCES media_children(id) ON DELETE CASCADE,
  relative_path    text NOT NULL,
  size_bytes       bigint,
  quality          text,                              -- e.g. 'Bluray-2160p'
  resolution       text,                              -- '2160p'
  video_codec      text, audio_codec text, audio_channels real,
  release_group    text,
  edition          text,                              -- 'Director''s Cut'
  languages        text[],
  subtitles        text[],
  date_added       timestamptz,
  created_at       timestamptz NOT NULL DEFAULT now(),
  CHECK (media_item_id IS NOT NULL OR media_child_id IS NOT NULL)
);

-- Quality profiles + custom formats (arr parity; taste-driven NL profiles layer on top in Phase 1).
CREATE TABLE quality_profiles (
  id            bigserial PRIMARY KEY,
  name          text NOT NULL,
  cutoff        text,                                 -- target quality
  items         jsonb NOT NULL DEFAULT '[]',          -- allowed qualities + order
  min_size_mb_per_min real, max_size_mb_per_min real,
  language      text,
  upgrade_allowed boolean NOT NULL DEFAULT true,
  natural_language_intent text,                       -- Phase-1: "small, good-enough, no HDR"
  created_at    timestamptz NOT NULL DEFAULT now()
);

-- People / genres / tags as first-class taste dimensions (embedded).
CREATE TABLE people ( id bigserial PRIMARY KEY, tmdb_person_id text UNIQUE, name text NOT NULL, known_for_department text );
CREATE TABLE media_credits (
  media_item_id bigint REFERENCES media_items(id) ON DELETE CASCADE,
  person_id     bigint REFERENCES people(id) ON DELETE CASCADE,
  role          text NOT NULL,                        -- 'director','actor','writer'
  character     text, cast_order int,
  PRIMARY KEY (media_item_id, person_id, role)
);
CREATE TABLE genres ( id bigserial PRIMARY KEY, name text UNIQUE NOT NULL );
CREATE TABLE media_genres ( media_item_id bigint REFERENCES media_items(id) ON DELETE CASCADE, genre_id bigint REFERENCES genres(id) ON DELETE CASCADE, PRIMARY KEY (media_item_id, genre_id) );
CREATE TABLE tags ( id bigserial PRIMARY KEY, name text UNIQUE NOT NULL, source text );   -- 'plex_label','muse_derived'
CREATE TABLE media_tags ( media_item_id bigint REFERENCES media_items(id) ON DELETE CASCADE, tag_id bigint REFERENCES tags(id) ON DELETE CASCADE, PRIMARY KEY (media_item_id, tag_id) );

-- Collections (Plex + TMDb + Muse-curated).
CREATE TABLE collections ( id bigserial PRIMARY KEY, name text NOT NULL, source text, plex_rating_key text, description text );
CREATE TABLE collection_items ( collection_id bigint REFERENCES collections(id) ON DELETE CASCADE, media_item_id bigint REFERENCES media_items(id) ON DELETE CASCADE, PRIMARY KEY (collection_id, media_item_id) );
```

### 3.3 TELEMETRY — the Tautulli-equivalent tracker (the taste fuel; REPLACES Tautulli)
> These tables reproduce (and extend) Tautulli's `session_history` / `session_history_metadata` /
> `session_history_media_info` so Muse can (a) backfill Tautulli's history and (b) capture new sessions natively,
> then retire Tautulli. `play_events` is the raw webhook/poll stream; `play_sessions` is the reconstructed session.
```sql
-- Raw event stream from Plex webhooks + session polling (append-only, immutable audit).
CREATE TABLE play_events (
  id             bigserial PRIMARY KEY,
  received_at    timestamptz NOT NULL DEFAULT now(),
  source         text NOT NULL,                       -- 'plex_webhook' | 'plex_poll' | 'tautulli_backfill'
  event_type     text NOT NULL,                       -- 'media.play','media.pause','media.resume','media.stop','media.scrobble','media.rate'
  account_ref    text,                                -- Plex accountID
  session_key    text,                                -- Plex Session key (stitches events into a session)
  rating_key     text,                                -- Plex ratingKey of what's playing
  view_offset_ms bigint,                              -- progress at event time
  player         text, platform text, product text, device text, ip_address inet,
  raw            jsonb NOT NULL,                       -- full payload for forensic replay
  UNIQUE (source, event_type, session_key, view_offset_ms, received_at)
);
CREATE INDEX ON play_events (session_key, received_at);

-- Reconstructed watch sessions (Tautulli session_history parity + extensions).
CREATE TABLE play_sessions (
  id                bigserial PRIMARY KEY,
  account_id        bigint REFERENCES accounts(id),
  media_item_id     bigint REFERENCES media_items(id),
  media_child_id    bigint REFERENCES media_children(id),           -- episode-level when applicable
  session_key       text,                                           -- Plex session key (null for backfill)
  tautulli_ref_id   bigint,                                         -- provenance if imported from Tautulli
  started_at        timestamptz NOT NULL,
  stopped_at        timestamptz,
  duration_ms       bigint,                                         -- item runtime
  watched_ms        bigint,                                         -- actual watched (sum of playing intervals)
  view_offset_ms    bigint,                                         -- final progress
  percent_complete  real,                                           -- watched_ms/duration_ms (or offset-based)
  paused_counter    int NOT NULL DEFAULT 0,                         -- # of pauses
  paused_ms         bigint NOT NULL DEFAULT 0,
  is_finished       boolean NOT NULL DEFAULT false,                 -- scrobble OR percent >= COMPLETE_THRESHOLD (0.90)
  is_abandoned      boolean NOT NULL DEFAULT false,                 -- stopped < ABANDON_THRESHOLD (0.15) — strong NEGATIVE signal
  -- context (device/time — taste is contextual)
  player            text, platform text, product text, device text, ip_address inet,
  started_hour      int,                                            -- 0-23 local (time-of-day taste)
  started_dow       int,                                            -- 0-6 (weekend vs weekday)
  is_cinema_context boolean,                                        -- TV/large-screen vs phone/commute
  created_at        timestamptz NOT NULL DEFAULT now(),
  UNIQUE (account_id, media_item_id, media_child_id, started_at)
);
CREATE INDEX ON play_sessions (account_id, started_at DESC);
CREATE INDEX ON play_sessions (media_item_id);

-- Per-session media/quality info (Tautulli session_history_media_info parity) — quality-sensitivity signal + Phase-1 profile learning.
CREATE TABLE play_session_media_info (
  play_session_id  bigint PRIMARY KEY REFERENCES play_sessions(id) ON DELETE CASCADE,
  video_decision   decision_kind, audio_decision decision_kind, transcode_decision decision_kind,
  container text, video_codec text, audio_codec text, audio_channels real,
  video_resolution text, bitrate int, width int, height int,
  transcode_reason text
);

-- Derived per-(account,item) aggregates — recomputed by a worker; the taste model reads these fast.
CREATE TABLE watch_stats (
  account_id       bigint REFERENCES accounts(id) ON DELETE CASCADE,
  media_item_id    bigint REFERENCES media_items(id) ON DELETE CASCADE,
  play_count       int NOT NULL DEFAULT 0,
  finished_count   int NOT NULL DEFAULT 0,
  rewatch_count    int NOT NULL DEFAULT 0,                          -- finishes beyond the first — VERY strong +
  total_watched_ms bigint NOT NULL DEFAULT 0,
  avg_percent      real,
  last_watched_at  timestamptz,
  abandoned        boolean NOT NULL DEFAULT false,                 -- ever abandoned early w/o later finish — NEGATIVE
  first_watched_at timestamptz,
  PRIMARY KEY (account_id, media_item_id)
);

-- Explicit signals (Plex ratings + Watchlist).
CREATE TABLE ratings (
  account_id    bigint REFERENCES accounts(id) ON DELETE CASCADE,
  media_item_id bigint REFERENCES media_items(id) ON DELETE CASCADE,
  rating        real,                                              -- Plex user rating (0-10) / thumbs
  rated_at      timestamptz,
  PRIMARY KEY (account_id, media_item_id)
);
CREATE TABLE watchlist (
  account_id    bigint REFERENCES accounts(id) ON DELETE CASCADE,
  media_item_id bigint REFERENCES media_items(id) ON DELETE CASCADE,
  added_at      timestamptz, removed_at timestamptz,
  fulfilled     boolean NOT NULL DEFAULT false,                    -- later watched — intent→action signal
  PRIMARY KEY (account_id, media_item_id)
);
```

### 3.4 TASTE MODEL + EMBEDDINGS (pgvector — assistant-speed private recall)
```sql
-- One embedding row per embeddable entity; kind lets us search library items, people, or taste centroids uniformly.
CREATE TABLE embeddings (
  id            bigserial PRIMARY KEY,
  entity_kind   text NOT NULL,                        -- 'media_item','person','collection','taste_centroid'
  entity_id     bigint NOT NULL,
  model         text NOT NULL,                        -- 'nomic-embed-text'
  dim           int NOT NULL DEFAULT 768,
  embedding     vector(768) NOT NULL,
  source_text   text,                                 -- what was embedded (title+overview+genres+people)
  embedded_at   timestamptz NOT NULL DEFAULT now(),
  UNIQUE (entity_kind, entity_id, model)
);
CREATE INDEX embeddings_hnsw ON embeddings USING hnsw (embedding vector_cosine_ops);

-- Per-account taste profile (the model): weighted affinities + context-specific centroids.
CREATE TABLE taste_profile (
  account_id        bigint PRIMARY KEY REFERENCES accounts(id) ON DELETE CASCADE,
  genre_affinity    jsonb NOT NULL DEFAULT '{}',       -- {genre: weight} recency-weighted, +finish/−abandon
  person_affinity   jsonb NOT NULL DEFAULT '{}',       -- {person_id: weight}
  keyword_affinity  jsonb NOT NULL DEFAULT '{}',       -- 'slow-burn','one-shot','practical-fx'
  runtime_pref      jsonb,                             -- distribution of finished runtimes (phone vs TV)
  quality_sensitivity jsonb,                           -- from transcode/abandon-on-low-quality signals
  overall_centroid  vector(768),                       -- centroid of loved items
  computed_at       timestamptz NOT NULL DEFAULT now(),
  model_notes       text                               -- LLM-written summary ("you love cerebral, slow-burn sci-fi…")
);
-- Context-specific taste (Friday-night ≠ Sunday-morning ≠ phone-commute).
CREATE TABLE taste_context_centroids (
  account_id  bigint REFERENCES accounts(id) ON DELETE CASCADE,
  context_key text NOT NULL,                           -- 'weekend_evening','weekday_late','phone_short'
  centroid    vector(768) NOT NULL,
  sample_size int NOT NULL,
  PRIMARY KEY (account_id, context_key)
);

-- Raw weighted taste signals (auditable; the profile is derived from these).
CREATE TABLE taste_signals (
  id            bigserial PRIMARY KEY,
  account_id    bigint REFERENCES accounts(id) ON DELETE CASCADE,
  media_item_id bigint REFERENCES media_items(id),
  signal_type   text NOT NULL,                         -- 'finished','abandoned','rewatched','rated','watchlisted','curation_note'
  weight        real NOT NULL,                         -- +1.0 finish, +2.5 rewatch, −1.5 abandon, explicit rating scaled
  context_key   text,
  note          text,                                  -- free-text curation ("loved the pacing")
  observed_at   timestamptz NOT NULL DEFAULT now()
);

-- Proactive-content outbox → Lumina (dedup + cooldown so she isn't spammy).
CREATE TABLE proactive_items (
  id            bigserial PRIMARY KEY,
  account_id    bigint REFERENCES accounts(id),
  kind          text NOT NULL,                         -- 'new_season','finish_nudge','friday_pick','abandon_insight','deal','news'
  media_item_id bigint REFERENCES media_items(id),
  headline      text NOT NULL,                         -- the line Lumina says
  body          jsonb,                                 -- structured payload + rationale
  priority      int NOT NULL DEFAULT 5,
  earliest_at   timestamptz,                           -- don't surface before (e.g. Friday 20:00)
  expires_at    timestamptz,
  delivered_at  timestamptz,
  created_at    timestamptz NOT NULL DEFAULT now()
);
```

### 3.5 Enrichment cache (Terminus-tool-suite signals — §6)
```sql
CREATE TABLE external_enrichment (
  id            bigserial PRIMARY KEY,
  media_item_id bigint REFERENCES media_items(id) ON DELETE CASCADE,
  kind          text NOT NULL,        -- 'forum_sentiment','does_it_get_good','renewal_news','trailer','deal','critic_score'
  source        text NOT NULL,        -- 'reddit','letterboxd','searxng','news','metacritic'
  payload       jsonb NOT NULL,       -- normalized: {score, summary, url, gets_good_at_episode, ...}
  confidence    real,
  fetched_at    timestamptz NOT NULL DEFAULT now(),
  ttl_seconds   int NOT NULL DEFAULT 604800,
  UNIQUE (media_item_id, kind, source)
);
```

### 3.6 AVAILABILITY / release reports (Prowlarr — what's grabbable NOW, vs <media-service>'s catalog)
> The distinction that motivates this: <media-service>/TMDb/TVDb answer *"does this exist?"* (catalog metadata). Prowlarr
> answers *"is there a good release available to download right now, at what quality/size, with how many seeders?"*
> Muse mimics the *arr **report-pull** mechanism (scheduled RSS/latest pulls + targeted search **through Prowlarr**)
> and stores a rolling availability snapshot. Read-only: a search is not a grab. Feeds availability-aware curation +
> proactive "grab-window" content, and is the candidate corpus Phase-1 AI release-selection reasons over.
```sql
CREATE TABLE indexers (
  id             bigserial PRIMARY KEY,
  prowlarr_id    int UNIQUE,                          -- Prowlarr indexer id
  name           text NOT NULL,
  protocol       text,                                -- 'torrent' | 'usenet'
  privacy        text,                                -- 'public' | 'private' | 'semiPrivate'
  enabled        boolean NOT NULL DEFAULT true,
  categories     int[],                               -- Newznab cats supported (2000s movies / 5000s tv)
  last_rss_pull_at timestamptz,
  polite_min_interval_secs int NOT NULL DEFAULT 900,  -- etiquette: don't poll faster than this
  created_at     timestamptz NOT NULL DEFAULT now()
);

-- Rolling availability snapshot. One row per (indexer, release guid). Expired rows are pruned.
CREATE TABLE releases (
  id               bigserial PRIMARY KEY,
  media_item_id    bigint REFERENCES media_items(id) ON DELETE SET NULL,   -- resolved match (may be null pre-parse)
  media_child_id   bigint REFERENCES media_children(id) ON DELETE SET NULL,
  indexer_id       bigint REFERENCES indexers(id) ON DELETE CASCADE,
  guid             text NOT NULL,                     -- indexer-unique release id
  title            text NOT NULL,                     -- raw release name (parsed below)
  info_url         text, download_url text, info_hash text,
  size_bytes       bigint,
  publish_date     timestamptz,                       -- release age (freshness signal)
  seeders          int, leechers int, grabs int,      -- torrent health
  freeleech        boolean, freeleech_pct real,       -- grab-window/economy signal
  categories       int[],
  -- deterministic parse (the arr release-parsing brain, v0 — AI-augmented in Phase 1)
  parsed_title     text, parsed_year int,
  quality          text,                              -- 'Bluray-2160p','WEB-DL-1080p'
  resolution       text, source text,                 -- 'BluRay','WEB','HDTV'
  video_codec      text, audio_codec text, audio_channels real,
  hdr              text[],                             -- ['HDR10','DV']
  edition          text, release_group text,
  proper_repack    boolean NOT NULL DEFAULT false,
  languages        text[], subtitles text[],
  parse_confidence real,
  first_seen_at    timestamptz NOT NULL DEFAULT now(),
  last_seen_at     timestamptz NOT NULL DEFAULT now(),
  expires_at       timestamptz,
  UNIQUE (indexer_id, guid)
);
CREATE INDEX ON releases (media_item_id);
CREATE INDEX ON releases (publish_date DESC);

-- Per-(item) availability rollup for fast curation reads ("is a good release up right now?").
CREATE TABLE availability (
  media_item_id     bigint PRIMARY KEY REFERENCES media_items(id) ON DELETE CASCADE,
  best_quality      text,                             -- highest parsed quality currently available
  best_seeders      int,
  release_count     int NOT NULL DEFAULT 0,
  has_freeleech     boolean NOT NULL DEFAULT false,
  cheapest_size_bytes bigint,                          -- smallest acceptable-quality option
  newest_release_at timestamptz,
  computed_at       timestamptz NOT NULL DEFAULT now()
);
```

### 3.7 POPULATION CONSUMPTION — "what's trending on streaming" + you-vs-the-masses (radar data)
> A second axis for taste: not just *what you watch*, but *what everyone else is watching*. Read-only public feeds
> (TMDb trending/popular day-one; Trakt/FlixPatrol/JustWatch as richer optional streaming sources). Powers "popular
> vs you" radar graphs, mainstream blind-spots, and the taste-maker "you were early" signal.
```sql
-- Rolling population-level trending/popular snapshots ("the masses").
CREATE TABLE trending_snapshots (
  id            bigserial PRIMARY KEY,
  source        text NOT NULL,                        -- 'tmdb','trakt','flixpatrol','justwatch','netflix_top10'
  scope         text NOT NULL,                        -- 'trending','popular','most_watched','most_played','top10'
  platform      text,                                 -- 'netflix','prime','disney','hbo',… or NULL = aggregate
  region        text NOT NULL DEFAULT 'US',
  window        text NOT NULL,                        -- 'day' | 'week'
  rank          int,
  media_item_id bigint REFERENCES media_items(id) ON DELETE SET NULL,  -- resolved to library/catalog id
  external_ref  jsonb,                                -- {tmdb_id,imdb_id,title,year} when not in our items
  popularity    real,                                 -- source popularity/score
  captured_at   timestamptz NOT NULL DEFAULT now(),
  UNIQUE (source, scope, platform, region, window, rank, captured_at)
);
CREATE INDEX ON trending_snapshots (captured_at DESC);
CREATE INDEX ON trending_snapshots (media_item_id);

-- Where a title streams (TMDb /watch/providers) — the "on streaming" grounding.
CREATE TABLE streaming_availability (
  media_item_id bigint REFERENCES media_items(id) ON DELETE CASCADE,
  provider      text NOT NULL,                        -- 'netflix','prime_video',…
  region        text NOT NULL DEFAULT 'US',
  offer_type    text NOT NULL,                        -- 'flatrate','ads','rent','buy'
  link          text,
  seen_at       timestamptz NOT NULL DEFAULT now(),
  PRIMARY KEY (media_item_id, provider, region, offer_type)
);

-- Aggregate "mainstream" taste of the trending set (counterpart to per-account taste_profile).
CREATE TABLE population_profile (
  id                  bigserial PRIMARY KEY,
  window              text NOT NULL,                   -- 'week'
  region              text NOT NULL DEFAULT 'US',
  genre_distribution  jsonb NOT NULL,                  -- {genre: share}
  decade_distribution jsonb,                           -- {decade: share}
  runtime_distribution jsonb,
  mainstream_centroid vector(768),                     -- centroid of the trending set's embeddings
  sample_size         int,
  computed_at         timestamptz NOT NULL DEFAULT now()
);

-- The radar-graph payload: per-account divergence from the mainstream, tracked over time.
CREATE TABLE taste_divergence (
  id                bigserial PRIMARY KEY,
  account_id        bigint REFERENCES accounts(id) ON DELETE CASCADE,
  computed_at       timestamptz NOT NULL DEFAULT now(),
  -- radar dimensions (you vs population, shared axes)
  genre_index       jsonb NOT NULL,                    -- {genre: your_share/pop_share}  (>1 over-index, <1 under)
  decade_index      jsonb,
  mainstream_score  real,                              -- 0..1 cosine(your centroid, mainstream centroid)
  adventurousness   real,                              -- niche/long-tail consumption vs top-ranked
  contrarian_index  real,                              -- how far your top items sit from the zeitgeist
  -- interesting derived data points
  were_early        jsonb,                             -- [{media, watched_at, trended_at, lead_days}] taste-maker signal
  blind_spots       jsonb,                             -- hugely popular, never touched by you
  guilty_pleasures  jsonb                              -- you rewatch but it under-indexes the mainstream (or vice-versa)
);
CREATE INDEX ON taste_divergence (account_id, computed_at DESC);
```

### 3.8 CHANNELS — the pseudo-TV director (agentic linear programming + playback)
> Plex has no native "personal TV channel." Muse composes one on demand — an ordered play queue interleaving
> **an episode of each show with bumpers/shorts/commercials/music-videos/idents between** — themed/genre/era/personal,
> then **casts it to a Chromecast/TV** via the Plex client-control (Plex Companion) API. Playback is a benign control
> action (plays media; never mutates the library) → light-confirm, not a gated mutation.
```sql
-- Discovered Plex players / cast targets (Chromecast, AppleTV, TV apps, Plex web/mobile).
CREATE TABLE plex_clients (
  id                 bigserial PRIMARY KEY,
  machine_identifier text UNIQUE NOT NULL,            -- Plex client id (the control target)
  name               text, product text, device text, platform text,
  address            text, port int,
  protocol_caps      text[],                          -- 'playback','timeline','navigation'
  is_cast_target     boolean NOT NULL DEFAULT false,  -- Chromecast/receiver
  last_seen_at       timestamptz NOT NULL DEFAULT now()
);

-- Interstitial pool (bumpers/commercials/music-videos/idents/shorts) — the "between shows" glue.
CREATE TABLE interstitials (
  id               bigserial PRIMARY KEY,
  plex_rating_key  text UNIQUE,                        -- lives in a Plex "Bumpers"/"Commercials" library
  kind             text NOT NULL,                      -- 'bumper','commercial','music_video','ident','short','trailer'
  title            text,
  decade           int,                                -- 1980,1990,2000… (era-matching)
  theme            text,                               -- 'saturday_morning','horror','holiday','retro_tech'
  genre            text, mood text,
  duration_ms      bigint,
  tags             text[],
  source           text,                               -- 'plex_library','user'
  created_at       timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX ON interstitials (kind, decade, theme);

-- Channel definitions / presets (reusable directives).
CREATE TABLE channels (
  id            bigserial PRIMARY KEY,
  account_id    bigint REFERENCES accounts(id) ON DELETE CASCADE,
  name          text NOT NULL,                         -- 'Saturday Morning','90s Chaos','Comfort Rewatch','Discover'
  kind          text NOT NULL,                         -- 'personal','theme','genre','era','preset'
  mode          text NOT NULL DEFAULT 'on_demand',     -- 'on_demand' (cast a play-queue) | 'linear' (a persistent tuner channel in Plex guide)
  channel_number real,                                 -- guide channel number for linear mode (e.g. 101.1)
  directive     text,                                  -- the NL brief ("an ep of each sitcom + retro ads, 2 hrs")
  rules         jsonb NOT NULL DEFAULT '{}',           -- episode-selection policy, interstitial cadence/ratio,
                                                       --   era/theme constraints, session length, shuffle vs order
  is_preset     boolean NOT NULL DEFAULT false,
  created_at    timestamptz NOT NULL DEFAULT now()
);

-- A composed + (optionally) played schedule instance.
CREATE TABLE channel_runs (
  id             bigserial PRIMARY KEY,
  channel_id     bigint REFERENCES channels(id) ON DELETE SET NULL,
  account_id     bigint REFERENCES accounts(id),
  target_client  text,                                 -- plex_clients.machine_identifier
  plex_play_queue_id text,                             -- the Plex playQueue we built
  schedule       jsonb NOT NULL,                       -- ordered [{type:episode|interstitial, ref, title, dur, rationale}]
  total_duration_ms bigint,
  composed_at    timestamptz NOT NULL DEFAULT now(),
  started_at     timestamptz, ended_at timestamptz,
  status         text NOT NULL DEFAULT 'composed'      -- 'composed','playing','paused','stopped','completed'
);

-- LINEAR channels: a time-anchored programming grid (drives the XMLTV guide Plex reads, + the web lineup UI).
CREATE TABLE channel_programs (
  id             bigserial PRIMARY KEY,
  channel_id     bigint REFERENCES channels(id) ON DELETE CASCADE,
  item_type      text NOT NULL,                        -- 'episode' | 'movie' | 'interstitial'
  media_item_id  bigint REFERENCES media_items(id) ON DELETE SET NULL,
  media_child_id bigint REFERENCES media_children(id) ON DELETE SET NULL,
  interstitial_id bigint REFERENCES interstitials(id) ON DELETE SET NULL,
  title          text NOT NULL,
  subtitle       text,                                 -- episode title / "S2E4"
  description    text,
  artwork_url    text,                                 -- poster/cover (Muse-proxied)
  start_at       timestamptz NOT NULL,                 -- guide start (linear timeline)
  end_at         timestamptz NOT NULL,
  duration_ms    bigint NOT NULL,
  rationale      text,                                 -- why the director scheduled it here
  UNIQUE (channel_id, start_at)
);
CREATE INDEX ON channel_programs (channel_id, start_at);

-- Local artwork cache/proxy (never expose the Plex token to the browser; Muse proxies posters/covers).
CREATE TABLE artwork_cache (
  id            bigserial PRIMARY KEY,
  entity_kind   text NOT NULL,                          -- 'media_item','interstitial','person'
  entity_id     bigint NOT NULL,
  variant       text NOT NULL DEFAULT 'poster',         -- 'poster','thumb','art','banner'
  source_url    text,                                   -- upstream (plex/tmdb)
  local_path    text,                                   -- cached bytes on disk (served by Muse)
  width int, height int,
  cached_at     timestamptz NOT NULL DEFAULT now(),
  UNIQUE (entity_kind, entity_id, variant)
);
```

---

## 4. Tautulli-replacement subsystem (detailed artifacts)
Muse must produce Tautulli-equivalent tracking natively so Tautulli can be retired early. Three artifacts:

**(A) Plex webhook receiver** — `POST /ingest/plex-webhook` (multipart; Plex posts JSON in the `payload` field).
Handle events → append to `play_events`:
- `media.play` → open/attach session; `media.pause`/`media.resume` → pause accounting (increment `paused_counter`,
  accumulate `paused_ms`); `media.stop` → close session; `media.scrobble` (Plex fires at ~90% watched) → mark
  `is_finished`; `media.rate` → upsert `ratings`. Webhook `Account.id`, `Player`, `Metadata.ratingKey`,
  `Metadata.viewOffset` map directly onto the columns. (Plex webhooks require Plex Pass; the poller is the fallback/complement.)

**(B) Session poller** — every `MUSE_PLEX_POLL_SECS` (default 10s) GET `/status/sessions`. For each active session:
upsert an open `play_sessions` row keyed by Plex `Session.key`, advance `view_offset_ms`/`watched_ms`, capture
`play_session_media_info` (transcode decision, codecs, resolution). Poller fills gaps the webhook misses and is the
primary path when Plex Pass/webhooks aren't available.

**(C) Session reconstruction** — a worker stitches `play_events` + poll snapshots into finalized `play_sessions`:
`watched_ms` = Σ(playing intervals); `percent_complete` = watched_ms/duration_ms; `is_finished` = scrobble OR
percent ≥ `COMPLETE_THRESHOLD` (0.90); `is_abandoned` = stopped with percent < `ABANDON_THRESHOLD` (0.15) and no
later finish. Populate `started_hour`/`started_dow`/`is_cinema_context`. Idempotent + late-event tolerant.

**(D) Tautulli backfill importer** (one-time) — Tautulli API `get_history` (paged) + `get_metadata` +
`get_stream_data` → map onto `play_sessions`(+`_media_info`) with `source='tautulli_backfill'`,
`tautulli_ref_id=reference_id`. Field mapping: Tautulli `watched_status`→`is_finished`, `percent_complete`→
`percent_complete`, `paused_counter`→`paused_counter`, `stopped-started`→duration, `platform/player`→context.
**Dedup:** during the overlap window, native capture and backfill can both see a session — dedup on
`(account, rating_key, started_at±120s)`; prefer native (higher fidelity), keep `tautulli_ref_id` for provenance.

**Retiring Tautulli:** once (D) has run and (A)/(B)/(C) are green for a soak period, Tautulli is redundant for
Muse's purposes — the operator can leave it running (harmless) or decommission it. Muse never depended on Tautulli
staying up; it only mined its history once.

---

## 4b. Availability intelligence — Prowlarr report-pull (day-one, read-only)
Muse mimics the *arr availability mechanism so curation knows *grabbability*, not just catalog existence. **All
through Prowlarr** (it owns indexer credentials + rate-limits — Muse never talks to trackers directly), **read-only**.

**(A) Indexer sync** — `GET /api/v1/indexer` → upsert `indexers` (protocol/privacy/categories/enabled). Refreshed daily.

**(B) Report pull (the *arr "RSS sync" analog) — the primary, polite mechanism.** A scheduled worker calls Prowlarr
search with **no query** (latest/RSS mode) per enabled indexer + category (movies 2000s / tv 5000s), on a
per-indexer interval ≥ `polite_min_interval_secs` (default 900s), and upserts results into `releases`. This mirrors
how Radarr/Sonarr keep a fresh view of what's newly posted **without per-item hammering**. Results are matched to
`media_items` by parsed title/year/IDs; unmatched releases are still stored (useful for negative-space discovery).

**(C) Targeted search (sparingly).** For a specific curation/answer need ("is a good release of X up?"), a bounded
`GET /api/v1/search?query=…|tmdbid=…&categories=…&indexerIds=…` — **cached** (`releases.last_seen_at`), rate-limited,
and preferred by ID (tmdb/imdb/tvdb) over free-text. Never fan a text search across all private indexers on a whim.

**(D) Parse + rollup.** A deterministic **release-name parser** (v0 — the *arr parsing brain: quality/resolution/
source/codec/HDR/group/edition/proper-repack, size, seeders, age) populates the parsed_* columns + `parse_confidence`;
a rollup worker maintains `availability` per item (best quality/seeders/freeleech/newest). Phase-1 AI selection reasons
over `releases`; P0 only *reports* — it never grabs.

**Etiquette / safety (part of "don't damage anything"):** through-Prowlarr only; RSS-pull-first; per-indexer polite
intervals; aggressive caching; ID-based targeted search; hard cap on searches/hour. Respecting tracker rules protects
your account standing — treated as a first-class constraint, tested (rate-limit guard).

**Why this beats <media-service>-style discovery:** <media-service> surfaces IMDb/TMDb *catalog* entries (what exists, for
request creation). Muse's release reports surface **what's actually downloadable now, in what quality, how healthy** —
so recommendations become *"you'd love X and there's a 45 GB 2160p remux with 300 seeders up, freeleech ends tonight"*
instead of *"X exists."* That is the availability layer a taste companion needs and a discovery catalog can't give.

---

## 4c. Population consumption feed — "what's trending on streaming" (read-only)
A scheduled worker snapshots population-level consumption into `trending_snapshots` and derives the `population_profile`:
- **Day-one (have creds):** TMDb `/trending/{movie,tv}/{day,week}`, `/movie|tv/popular`, and `/watch/providers`
  (→ `streaming_availability`, the "on streaming where" grounding). US default, region-configurable.
- **Richer streaming sources (optional, add creds):** **Trakt** most-watched/most-played (the best "actually being
  watched" signal from community scrobbles), **FlixPatrol / JustWatch** per-platform streaming Top-10s (Netflix/
  Prime/Disney+/…). Flagged as enrichment; Muse degrades to TMDb-only if absent.
- **Resolve** each trending entry to a `media_item` (or keep `external_ref` if not in your catalog), **embed** the
  trending set, and compute the **mainstream centroid** + genre/decade distributions.
- **You-vs-the-masses (`taste_divergence`, the radar payload):** per account, compute genre/decade **over/under-index**,
  a **mainstream_score** (cosine of your centroid vs the mainstream centroid), **adventurousness/contrarian** indices,
  and the interesting bits — **`were_early`** (you watched it before it trended = taste-maker), **`blind_spots`**
  (huge but untouched), **`guilty_pleasures`**. Tracked over time so the radar *moves* (are you drifting mainstream
  or more niche?). The radar **visualization** itself is a downstream consumer (Lumina/Soma dashboard) — P0 captures
  and computes the data; rendering can follow.

**Why it's worth it:** it turns taste from an absolute into a *relative* — "you're 3× over-indexed on cerebral sci-fi
and you were 6 weeks early on the show everyone's now bingeing" — which is exactly the kind of personal, slightly
flattering, socially-useful data point that makes Lumina feel like she *knows* you, and it's a genuinely novel graph.

---

## 4d. Channels — the pseudo-TV director (Phase 0.5 — benign playback, high-delight)
The killer "living-room" feature: *"start me a 90s Saturday-morning channel on the living-room TV"* → Muse composes
a lineup and casts it. Sits just after the P0 library+taste foundation (it needs them); ships as its own delightful,
self-contained capability. **Benign** (plays media; no library mutation) → light-confirm.

**(A) Playback-control client (how the Chromecast starts).** Plex **client-control / Companion** API: discover
players/cast targets (`GET /clients`, plex.tv `/resources` for remote/Chromecast) → `plex_clients`; build a **Plex
play queue** (`POST /playQueues` from an ordered item list) — the native primitive for a sequenced "channel"; issue
`/player/playback/playMedia?...&machineIdentifier=<target>` to start it on the chosen client; drive `play/pause/
skipNext/stop` + `/player/timeline/poll` for state. **Fallback** for a bare Chromecast: Google Cast (DIAL + Cast v2)
launching the Plex receiver with the queue. Target discovery covers Chromecast, AppleTV, TV Plex apps, web/mobile.

**(B) Interstitial catalog.** Ingest a Plex "Bumpers/Commercials/Music-Videos/Idents" library section → `interstitials`,
and **auto-tag** (kind/decade/theme/genre/mood/duration) via metadata + a local-LLM pass, so the director can pick
era- and theme-appropriate glue. (User curates the pool; Muse organizes + tags it. Sourcing new interstitials is
out of scope for MVP — rights/egress.)

**(C) The channel composer (the agentic director — the novel LLM bit).** Given a directive (personal / theme /
genre / era / mood / duration) + the taste model + library + watch-state, the local LLM composes an ordered
schedule: pick episodes (**next-unwatched per show** by default, or taste-ranked; round-robin across shows for the
"one of each" feel), interleave interstitials at the configured cadence/ratio that match the era/theme, respect the
requested session length, and emit a **programming schedule with rationale** ("opened with the Animaniacs intro,
then…"). On-demand, regenerable, adjustable ("more music videos," "swap the drama for a comedy"). Presets:
*Saturday Morning · Prestige Drama Night · 90s Chaos · Comfort Rewatch · Discover (things you'd love) · Household
Movie Night.* Personal channels are learned from taste.

**(D) Execution + the taste loop.** Build the Plex play queue from the schedule, cast to the target, sequence it,
handle next/skip-bumper/stop; **log what actually played back into `play_events`/`play_sessions`** — so the channel
*feeds* the taste model (what you let ride vs skipped is signal). `channel_runs` records the composed schedule + status.

**(E) Linear / virtual-tuner mode — persistent "Muse TV" channels in the Plex guide (you already run HDHomeRun).**
Since Plex is already wired to an HDHomeRun for live-TV/guide, Muse presents its `mode='linear'` channels as an
**additional tuner Plex ingests as Live TV** — so Muse channels appear in the same guide you surf, alongside the real
tuner. Two artifacts, the proven ErsatzTV/dizqueTV pattern:
- **Tuner emulation + guide:** an **HDHomeRun-emulation** surface (`/discover.json`, `/lineup.json`, `/lineup_status.json`)
  **and** an **M3U playlist** + **XMLTV EPG** (`/muse.m3u`, `/xmltv.xml`) — Plex accepts either an HDHomeRun device or
  an M3U-custom-tuner + XMLTV. The XMLTV is generated from `channel_programs` (now/next/later with titles, art, descriptions).
- **Streaming engine:** each linear channel serves a **continuous MPEG-TS/HLS stream** at a tune URL. An **ffmpeg**
  pipeline concatenates the scheduled library files + interstitials into one seamless transport stream, with correct
  **join-mid-stream** semantics (a viewer tuning in at 8:47 lands in the middle of what's "on now", like real TV).
  The **director schedules a rolling window** (e.g. the next 24–48h of `channel_programs`) so the guide always has
  upcoming programming; a worker extends it. Re-uses the same composer as on-demand — linear is just "play-queue
  anchored to a clock + streamed."

**(F) Channel-guide web UI (stub in this spec).** Muse serves a lightweight web page (axum static + JSON API) showing
the channels and their lineups as an **EPG-style grid/timeline** — each program with **cover art, title/episode
metadata, start–end times, and the director's rationale**, interstitials visually marked, a "now" line. Endpoints:
`GET /api/channels`, `GET /api/channels/{id}/lineup`, and `GET /art/{kind}/{id}` (artwork proxied via `artwork_cache`
so the Plex token never reaches the browser). Ships as a functional stub — a real, viewable lineup guide — and is the
seed of the fuller Muse dashboard (taste views, the you-vs-masses radar) later.

**Sub-milestones within Phase 0.5 (never a monolith):** (i) on-demand cast play-queues → (ii) the web lineup guide →
(iii) linear tuner + XMLTV in the Plex guide → (iv) the ffmpeg streaming engine. Each is independently useful.

---

## 5. Phase-0 work items

### Human-action / provisioning (Agent: <operator>)
- **MUSE-00a:** Create the `moosenet/Muse` gitea repo + `moosenet-io/muse` github mirror shell (`mirror_ready:false`)
  + the **MUSE Plane project** (claim prefix `MUSE`, register in `data/prefix_registry.toml`).
- **MUSE-00b:** Provision the `muse` Postgres database (PG16+, `CREATE EXTENSION vector, pg_trgm`) on the fleet PG
  host; add `MUSE_DATABASE_URL` to <secret-manager>. Pull `nomic-embed-text` on the <host> Ollama.
- **MUSE-00c:** Provision read-only creds in <secret-manager>: `PLEX_URL`+`PLEX_TOKEN`, `TAUTULLI_URL`+`TAUTULLI_API_KEY`,
  `RADARR_URL`+`RADARR_API_KEY`, `SONARR_URL`+`SONARR_API_KEY`, **`PROWLARR_URL`+`PROWLARR_API_KEY`**, `TMDB_API_KEY`.
  Register a **Plex webhook** → `MUSE_URL/ingest/plex-webhook`.

### Code items (Agent: claude)
- **MUSE-01 — Service scaffold.** New Rust crate: `axum` HTTP (`/health`, `/ingest/*`, `/query/*`, `/proactive/*`),
  `sqlx` PG pool, config (env-materialized secrets), tracing/audit, background-worker harness. Consumed by Terminus
  via the gitea cargo registry. *FILES:* `src/{main,config,db,http,workers}.rs`, `Cargo.toml`. *TEST:* health +
  config-from-env + a migration smoke test; no hardcoded infra.
- **MUSE-02 — Core schema migrations.** §3.1–3.2 DDL as versioned `migrations/`; sqlx models + repository layer.
  *TEST:* migrate up/down on a throwaway PG; round-trip a media_item + child + file + credit.
- **MUSE-03 — Telemetry + taste + embedding schema.** §3.3–3.5 DDL + models. *TEST:* insert a play_session +
  media_info + watch_stats recompute; pgvector column + HNSW index build.
- **MUSE-04 — Plex read client.** libraries/metadata/`/status/sessions`/ratings/watchlist/managed-users; typed,
  graceful-degrade, no writes. *TEST:* mocked (httpmock) parsing + multi-user separation.
- **MUSE-05 — Library ingest.** *arr (Radarr/Sonarr) + Plex → `media_items`/`media_children`/`media_files`/
  `quality_profiles`/`people`/`genres`/`collections`. Idempotent upsert; provider-ID reconciliation (tmdb/tvdb/imdb/
  plex_rating_key). *TEST:* mocked arr+plex → populated rows; re-run = no dupes.
- **MUSE-06 — Tautulli backfill importer** (§4-D). Paged history import + metadata/media-info + dedup vs native.
  *TEST:* mocked Tautulli API → play_sessions with correct finished/abandoned/paused mapping; overlap dedup.
- **MUSE-07 — Native Plex tracker** (§4-A/B/C): webhook receiver + poller + session reconstruction. THE Tautulli
  replacement. *TEST:* synthetic event streams (play→pause→resume→stop, scrobble, mid-session poll) reconstruct
  correct watched_ms/percent/paused_counter/finished/abandoned; idempotent + late-event tolerant.
- **MUSE-08 — Embedding pipeline.** Compose source_text (title+overview+genres+top people+tags) → local
  `nomic-embed-text` via Ollama → `embeddings` + HNSW; incremental (only changed items); VRAM-polite via Chord.
  *TEST:* mocked embedder → vector rows; cosine query returns nearest.
- **MUSE-09 — Vector recall + search API/tools.** `/query/resolve?q=` → library-vector-first ANN (fallback pg_trgm,
  then TMDb beyond the library); `/query/similar`. This is the assistant-speed lookup. *TEST:* "dark sci-fi with the
  AI" resolves to the right item over embeddings; degrades gracefully.
- **MUSE-10 — Taste model v0.** Recompute `taste_signals`→`taste_profile`(+context centroids) from watch_stats +
  ratings + watchlist: recency-weighted, finish=+, abandon=−, rewatch=++; LLM `model_notes` summary via Chord.
  *TEST:* fixture history → expected affinities; abandonment lowers a genre; rewatch dominates.
- **MUSE-11 — Curation/recommend engine v0.** Local-LLM reasoning over taste_profile + library + context **+
  availability** (§3.6) → ranked suggestions with rationale ("because you finished + rewatched X") that factor
  *grabbability* for not-in-library picks ("you'd love X and a great release is up"); on-deck/continue-watching; gap
  analysis (owns S1-3, S4 exists AND a 1080p release is available). Read-only. *TEST:* mocked model + fixtures →
  rationale cites real signals; availability influences ranking; multi-user isolated.
- **MUSE-12 — Proactive content generator → Lumina.** Event-driven (`is_finished` on a show w/ a next season/gap;
  Friday-evening context; abandonment-pattern insight; **grab-window** — a taste-match just got a high-seeder or
  freeleech release; **zeitgeist** — a mainstream blind-spot worth a heads-up, or a "you were early on the show
  everyone's now bingeing" flex) → `proactive_items` with cooldown/dedup; `/proactive/pending`
  consumed by the Terminus surface + Lumina reminders/engagement scheduler. *TEST:* "finished S3, S4 exists" emits a
  new_season item; cooldown prevents spam; expired items drop.
- **MUSE-13 — Terminus surface integration.** Re-point the shipped `media_*` read tools (search/status/recommend/
  on_deck/recently_added) at Muse (library-vector-first), and add `muse_proactive`/`muse_taste_summary` tools. Keep
  the tiered-safety design for later mutation phases. *TEST:* media_search routes to Muse vector recall; parity
  responses; no live ARR writes.
- **MUSE-14 — Enrichment via Terminus tool suite (first cut).** §6 — start with (a) forum/critic **sentiment** +
  **"does it get good / best watch order"** via `searxng_search`/`lumina_web_fetch` (directly de-risks abandonment),
  (b) **renewal/trailer news** via `news_search`, cached in `external_enrichment` and folded into curation + proactive
  content. *TEST:* mocked web/news → normalized enrichment rows; "gets good at ep 4" surfaced on a slow-starter.
- **MUSE-16 — Prowlarr client + availability schema.** Read-only Prowlarr client (`/api/v1/indexer`, `/api/v1/search`
  RSS + targeted) + §3.6 migrations (`indexers`/`releases`/`availability`). Through-Prowlarr only; typed; graceful
  degrade. *FILES:* `src/prowlarr/{client,parse}.rs`, `migrations/…availability.sql`. *TEST:* mocked Prowlarr →
  parsed releases; indexer sync; **rate-limit/etiquette guard** (never poll an indexer under its polite interval;
  targeted-search cap). No grabs.
- **MUSE-17 — Report-pull worker + release parser + rollup.** Scheduled RSS/latest pull per enabled indexer+category
  (§4b-B), deterministic release-name parser v0 (§4b-D: quality/resolution/source/codec/HDR/group/edition/proper-
  repack/size/seeders/age → parsed_* + confidence), match to `media_items`, maintain `availability` rollup; prune
  expired. *TEST:* fixture release names parse correctly (incl. remux/HDR/proper/anime); rollup picks best quality +
  freeleech; unmatched releases retained; polite-interval respected.
- **MUSE-19 — Trending/streaming feed + population profile.** §3.7 migrations + scheduled ingest: TMDb trending/
  popular + `/watch/providers` (→ `streaming_availability`); optional Trakt/FlixPatrol/JustWatch behind flags;
  resolve + embed the trending set; compute `population_profile` (mainstream centroid + distributions). Read-only.
  *FILES:* `src/trending/{tmdb,trakt,flixpatrol}.rs`, migrations. *TEST:* mocked TMDb trending → snapshots +
  streaming rows; population centroid computed; degrades to TMDb-only when optional sources absent.
- **MUSE-20 — Taste-vs-population divergence (radar data).** Compute per-account `taste_divergence`: genre/decade
  over/under-index, mainstream_score, adventurousness/contrarian, `were_early`, `blind_spots`, `guilty_pleasures`;
  recompute on a schedule so it trends over time; expose `/query/radar` + a `muse_taste_radar` tool. (Radar *render*
  is downstream Lumina/Soma.) *TEST:* fixture (your history + a trending set) → correct over-index + a were-early hit
  + a blind-spot; multi-user isolated.
#### Phase 0.5 — Channels / pseudo-TV director (benign playback; needs the P0 library+taste foundation)
- **MUSE-22 — Plex playback-control client + player discovery.** Plex Companion/client-control + `/playQueues`
  (§4d-A): discover players/cast targets → `plex_clients`; build a play queue; `playMedia`/`play`/`pause`/`skipNext`/
  `stop`/timeline-poll to a target `machineIdentifier`; Google-Cast fallback for bare Chromecasts. §3.8 `plex_clients`
  migration. *FILES:* `src/plex_control/{companion,playqueue,cast}.rs`. *TEST:* mocked Plex control → correct
  playMedia + queue ordering + transport; target selection; graceful "client offline."
- **MUSE-23 — Interstitial catalog + channel schema.** Ingest a Plex bumpers/commercials/music-video/idents library
  → `interstitials` with local-LLM auto-tagging (kind/decade/theme/mood/duration); §3.8 `interstitials`/`channels`/
  `channel_runs` migrations. Human-action prereq (<operator>): create/point a Plex "Bumpers" library + register cast
  targets. *TEST:* mocked Plex → tagged interstitials; era/theme queryable.
- **MUSE-24 — Channel composer (the agentic director) + presets.** Local-LLM schedule composition (§4d-C):
  round-robin next-unwatched (or taste-ranked) episodes + cadence-matched interstitials, session-length aware, with
  rationale; presets (Saturday Morning / Prestige Night / 90s Chaos / Comfort Rewatch / Discover / Household); on-demand
  + regenerate/adjust. *TEST:* fixture library+taste+interstitials → a valid interleaved schedule honoring the directive
  (ratio, era match, duration); "more music videos" regenerates correctly; multi-user isolated.
- **MUSE-25 — Terminus channel tools + Lumina + taste loop.** `muse_channel_start` (directive+target → compose+cast+
  play, LIGHT-confirm — "start the 90s channel on the living-room TV?"), `muse_channel_next`/`skip`/`stop`,
  `muse_players`, `muse_channel_presets`; log played items → `play_events` (channel feeds taste). *TEST:* end-to-end
  (mocked) compose→confirm→cast→sequence; skip-bumper advances; stop halts; a played episode records a play_session.
- **MUSE-27 — Channel-guide web UI stub + artwork proxy.** axum-served page + JSON API (`/api/channels`,
  `/api/channels/{id}/lineup`, `/art/{kind}/{id}`) rendering an **EPG-style grid/timeline** — covers, title/episode
  metadata, start–end times, rationale, interstitials marked, a "now" line. `artwork_cache` proxy (no Plex token in
  the browser). Functional stub, seed of the fuller dashboard. *FILES:* `src/web/{routes,art}.rs`, `web/` static.
  *TEST:* lineup JSON shape; artwork proxied (no token leak); renders composed + linear schedules.
- **MUSE-28 — Linear tuner: HDHomeRun-emulation + M3U + XMLTV + program grid.** §4d-E artifacts: `/discover.json`,
  `/lineup.json`, `/lineup_status.json`, `/muse.m3u`, `/xmltv.xml`; the director schedules a rolling 24–48h of
  `channel_programs` per `mode='linear'` channel; XMLTV generated from the grid. Human-action prereq (<operator>): add Muse
  to Plex Live TV as a custom tuner (HDHomeRun URL or M3U+XMLTV) next to the existing HDHomeRun. *TEST:* valid
  discover/lineup JSON + well-formed XMLTV from a fixture grid; rolling-window extension; channel numbering.
- **MUSE-29 — Channel streaming engine (ffmpeg).** Per linear channel, a continuous **MPEG-TS/HLS** stream at a tune
  URL: ffmpeg concat of scheduled library files + interstitials into one seamless stream, **join-mid-stream** (tune-in
  lands at the current position of what's "on now"). *FILES:* `src/stream/{engine,ffmpeg,mux}.rs`. *TEST:* schedule →
  continuous TS; a mid-window tune-in starts at the right offset; interstitial→episode transitions seamless (mocked/
  short fixtures). NOTE: benign playback-only; no library writes.
- **MUSE-30 — Docs + behavior-spec.** README + `docs/` (architecture, schema, **Tautulli-replacement + Prowlarr
  report-pull + trending-feed + channels (on-demand cast + linear HDHomeRun/M3U/XMLTV + ffmpeg streaming) + web-guide
  runbooks incl. tracker etiquette + API rate/etiquette for TMDb/Trakt + Plex control/cast targeting + adding Muse as a
  Plex custom tuner**, the taste model, availability + population layers, the you-vs-masses radar, the pseudo-TV
  director, the lineup web UI, proactive contract) + `behavior-spec.md` (ingest/track/report-pull/trending/taste/radar/
  channel/tuner/stream/web states + degradation + rate-limit + playback-confirm invariants).

---

## 6. Leveraging the Terminus tool suite for curation & taste (design + roadmap)
Muse's reasoning gets dramatically richer by pulling the fleet's existing tools as **taste/enrichment signals** — all
cached in `external_enrichment`, all optional (Muse degrades to local-only if a tool is down):
- **Web / forums (`searxng_search`, `lumina_web_fetch`, `lumina_clawhub_*`):** community sentiment (Reddit r/movies,
  r/television, Letterboxd, Trakt, MyAnimeList), **"does it get good after episode N"** (pairs perfectly with the
  abandonment signal — Muse can say *"it picks up at ep 4, want to give it another shot?"* instead of dropping it),
  **best watch/release order**, "if you liked X" threads → candidate recommendations Muse can't derive from your
  library alone.
- **News (`news_search`, `news_topic`):** renewals/cancellations, trailer drops, release dates, cast/awards → timely
  **proactive** content ("your show got renewed"; "the sequel's trailer dropped").
- **Shopping / deals (`odyssey_deals` today; a future `muse_deals`/commerce hook):** *unique commerce tied to what
  you're bingeing* — a 4K box-set of the trilogy you're rewatching on sale, the **novel the show is based on**,
  soundtrack on vinyl, tickets to see the composer/relevant con, streaming-rental deals for something not worth
  owning. A genuinely novel "media-adjacent commerce" surface for the assistant. (New tool candidate — flagged below.)
- **Calendar (`google_calendar_*`):** schedule-aware suggestions — *"free Saturday night — here's a movie-night
  lineup"*; avoid suggesting a 3-hour film on a work night.
- **Weather (`weather`):** context flavor — *"rainy weekend, cozy-binge weather"* lineups.
- **Council / Wizard (`council_convene`, `wizard_consult`):** multi-model deliberation for hard curation calls or to
  "settle which cut to watch."
- **Crucible (`crucible_track_create`):** cross-domain — *"you're deep in WWII docs; want a structured deep-dive
  track?"* turning binge into learning.
- **Reminders/engagement (`reminder_set`) + Nexus (`nexus_send`):** the delivery rails for proactive content.
- **Meridian/Ledger:** budget-aware "worth buying vs renting" for the commerce angle.

---

## 7. NEW suggestions (data to capture + system improvements — keep iterating)
**Richer capture (add to telemetry as we go):**
1. **Abandonment fingerprinting** — correlate abandons with metadata (runtime? subtitles? slow-burn tag? specific
   genre at specific time-of-day?) so Muse learns *why* you bail, not just that you did.
2. **Binge velocity** — gap between episodes (autoplay-through vs one-a-night) as an engagement-intensity signal.
3. **Transcode/quality pain** — repeated transcodes or abandons on a device → quality-profile guidance in Phase 1.
4. **Co-viewing / household** — overlapping multi-user sessions → shared-taste "movie night" recommendations.
5. **Seasonal/temporal patterns** — holiday films in December, comfort-rewatches when stressed (infer from cadence).
6. **Skip behavior** (intro/credits) if exposed — signal of investment.
7. **Watchlist→watch latency** — how long intent takes to convert; nudge stale watchlist items.
8. **Time-of-day taste vectors** — already modeled via context centroids; expand to mood inference.

**System improvements:**
9. **Explainability first-class** — every recommendation/proactive line stores its rationale + the signals it used
   (already in schema) so Lumina can always answer "why did you suggest that?" honestly.
10. **Feedback loop** — capture the outcome of every proactive nudge (accepted/ignored/dismissed) as a `taste_signal`
    so Muse learns which *kinds* of nudges you welcome (ties to the reminders/engagement rules).
11. **Cold-start** — for a fresh account, lean on watchlist + ratings + external "if you liked" until behavior accrues.
12. **Privacy posture** — all taste/behavior stays in the `muse` PG on-LAN; only non-personal metadata lookups (TMDb,
    web sentiment) egress; embeddings are local. Make this an explicit, testable guarantee.
13. **Local model sweep (Harmony)** — pick the curation/instruct model empirically (latency vs quality on curation
    prompts) and the embedding model (recall quality) before pinning; keep both swappable behind config.

14. **Popular-vs-you over time** — snapshot `taste_divergence` on a cadence so the radar animates; detect drift
    ("you've been trending more mainstream since summer") and taste-maker streaks (consistently early).
15. **Cross-account household zeitgeist** — whose taste in the house tracks the mainstream vs who's the contrarian;
    fuel for movie-night picks that satisfy the room.
16. **"Worth the hype?" verdicts** — pair a blind-spot (trending) with forum sentiment (§6) + your taste vector →
    an honest *"everyone loves it but it's not your thing — skip"* vs *"this one's actually up your alley."*

**New-tool candidates (future, worth their own specs):**
- `muse_deals` / a media-commerce tool (box sets, source novels, soundtracks, tickets) — the shopping angle.
- A `muse_watch_party` planner (calendar + co-viewing + household taste intersection).
- A `muse_radar` / taste-dashboard surface (Soma) rendering the you-vs-masses graphs over time.

---

## Pre-flight
- Repository: new `moosenet/Muse` on gitea (create — MUSE-00a). Cargo consumed via the gitea registry.
- DB: `muse` Postgres 16+ with `vector`+`pg_trgm` (MUSE-00b). Ollama `nomic-embed-text` on <host>.
- Secrets (<secret-manager>→env, read-only): `MUSE_DATABASE_URL`, `PLEX_URL/PLEX_TOKEN`, `TAUTULLI_URL/TAUTULLI_API_KEY`,
  `RADARR_URL/RADARR_API_KEY`, `SONARR_URL/SONARR_API_KEY`, `TMDB_API_KEY`, `MUSE_OLLAMA_URL`, `CHORD_URL`.
- Behavior flags (plain env): `MUSE_PLEX_POLL_SECS=10`, `MUSE_COMPLETE_THRESHOLD=0.90`, `MUSE_ABANDON_THRESHOLD=0.15`,
  `MUSE_EMBED_MODEL=nomic-embed-text`, `MUSE_EMBED_DIM=768`. Channels: `MUSE_CHANNEL_GUIDE_WINDOW_HOURS=48`, `MUSE_HDHR_DEVICE_ID`.
- Phase-0.5 deps: **ffmpeg** on the Muse host (streaming engine, MUSE-29); **HDHomeRun already on the network + Plex
  Live-TV connected** (Muse registers as an additional custom tuner via HDHomeRun-emulation or M3U+XMLTV — MUSE-28
  human-action). Muse serves the tuner/stream/guide on a stable LAN URL Plex can reach.
- Plane: via the Terminus Plane tool (new MUSE project). Baseline tests: 0 (new repo).
- **SAFETY:** Phase 0 is READ-ONLY (never write to Plex/Tautulli/*arr/Prowlarr; all service calls read-only; all tests
  mocked). The ONE exception is **Phase 0.5 playback control** — a *benign* write (it starts playback on a device;
  it never modifies, downloads, deletes, or organizes anything), gated by a **light-confirm**. No acquisition/organize/
  delete tools exist until Phase 1+. Tracker etiquette (Prowlarr) + API rate-limits (TMDb/Trakt) are tested invariants.

## Notes for the executing agent
1. **Never a monolith** — MUSE-01→03 (DB) then 04→07 (ingest+tracker) then 08→12 (embed/taste/proactive) each land
   working; even MUSE-07 alone (native tracker) + MUSE-06 (backfill) already replaces Tautulli.
2. **Local-inference-first** — embeddings + reasoning on <host> via Ollama/Chord; coordinate VRAM (don't fight the
   serving model). Taste/behavior never egress.
3. **Read-only, mocked tests** — same discipline as the S94 media domain; do not touch the live stack beyond reads.
4. **Full moosenet-spec pipeline** (worktree → test → dual review → merge), Plane project MUSE, built by Harmony.
