-- MUSEM-01 (Plane MUSE, S119 Sprint 1): the acquisition-domain write-path
-- foundation -- monitoring ("wanted"), requests (<media-service>-lifecycle
-- parity), the download queue, typed history, and the blocklist.
--
-- ## Reuse note -- why this migration does NOT add all nine tables the
-- spec's APPROACH section lists
-- `ARR-BLUEPRINT.md` (and this spec) call for
-- `quality_definitions`/`quality_profiles`/`custom_formats`/
-- `quality_profile_format_scores`. Those four already exist as of MUSE-02
-- (`migrations/0003_quality_definitions.sql`,
-- `migrations/0004_custom_formats.sql`) -- `quality_definitions`,
-- `quality_profiles`, `custom_formats`, and `quality_profile_formats`
-- (the score join; same `(quality_profile_id, custom_format_id) -> score`
-- shape the spec names `quality_profile_format_scores`). Recreating them
-- here would either collide (duplicate table) or fork the schema into two
-- competing quality models. This migration only adds the five acquisition
-- tables that do NOT already exist anywhere in the schema: `monitored_items`,
-- `media_requests`, `download_queue`, `history_events`, `blocklist`. See
-- `src/repo/quality.rs` / `src/models/quality.rs` for the pre-existing
-- quality-domain repo/model layer this migration's tables reference by FK.
--
-- `media_kind` (on `media_requests`) is intentionally `text`, NOT the
-- existing `media_kind` Postgres enum type (`'movie'|'show'`, see
-- `migrations/0006_media_items.sql` / `src/models/media_metadata.rs`) --
-- per this spec item's edge case, a future `'music'` value must be
-- accepted without a schema change (`ALTER TYPE`), which a fixed enum type
-- would require. Same reasoning for the `status`/`event_type` columns
-- below: plain `text`, validated at the application layer
-- (`src/models/acquisition.rs`), so an unrecognized value is a decodable
-- (if unmapped) string, never a failed row fetch.

-- The "wanted" driver: monitoring a title within a library, independent of
-- whether a `media_items` row/file exists yet. Deliberately decoupled from
-- `media_items` (`media_item_id` is nullable) so a title can be monitored
-- before it has ever been grabbed -- `media_items.path` is NOT NULL, so a
-- `media_items` row alone cannot represent "wanted, not yet acquired".
CREATE TABLE monitored_items (
    id                  bigserial PRIMARY KEY,
    media_metadata_id   bigint NOT NULL REFERENCES media_metadata(id) ON DELETE CASCADE,
    media_item_id       bigint REFERENCES media_items(id) ON DELETE SET NULL,
    library_id          bigint NOT NULL REFERENCES libraries(id) ON DELETE CASCADE,
    monitored           boolean NOT NULL DEFAULT true,
    quality_profile_id  bigint REFERENCES quality_profiles(id),
    min_availability    text,
    last_search_at      timestamptz,
    created_at          timestamptz NOT NULL DEFAULT now(),
    updated_at          timestamptz NOT NULL DEFAULT now(),
    UNIQUE (media_metadata_id, library_id)
);
CREATE INDEX ON monitored_items (library_id, monitored);
CREATE INDEX ON monitored_items (media_item_id);

-- <media-service>-lifecycle request tracking. `media_kind` is `text` (see header
-- note); `provider_ids` mirrors `media_metadata.provider_ids`'s keyed-map
-- shape (blueprint: provider identity as a map, not denormalized columns)
-- for a request that may not resolve to a `media_metadata` row yet.
CREATE TABLE media_requests (
    id                  bigserial PRIMARY KEY,
    provider_ids        jsonb NOT NULL DEFAULT '{}',
    media_kind          text NOT NULL,
    title               text NOT NULL,
    requested_by        text,
    status              text NOT NULL DEFAULT 'requested',
    tier                text,
    quality_profile_id  bigint REFERENCES quality_profiles(id),
    note                text,
    created_at          timestamptz NOT NULL DEFAULT now(),
    updated_at          timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX ON media_requests (status);

-- The download queue: one row per in-flight (or terminal) grab. A queue
-- entry must trace back to *something* that asked for it -- either a
-- `media_requests` row (<media-service>-style ask) or a `monitored_items` row
-- (autonomous wanted-list grab) -- so the CHECK below requires at least one
-- non-null ref; both may be set (a request that also created/updated a
-- monitor).
CREATE TABLE download_queue (
    id                  bigserial PRIMARY KEY,
    request_id          bigint REFERENCES media_requests(id) ON DELETE SET NULL,
    monitored_item_id   bigint REFERENCES monitored_items(id) ON DELETE SET NULL,
    release_guid        text NOT NULL,
    release_title       text NOT NULL,
    indexer             text,
    download_client     text,
    client_hash         text,
    protocol            text,
    status              text NOT NULL DEFAULT 'queued',
    size_bytes          bigint,
    added_at            timestamptz NOT NULL DEFAULT now(),
    updated_at          timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT download_queue_has_source
        CHECK (request_id IS NOT NULL OR monitored_item_id IS NOT NULL)
);
CREATE INDEX ON download_queue (status);
CREATE INDEX ON download_queue (client_hash);
CREATE INDEX ON download_queue (request_id);
CREATE INDEX ON download_queue (monitored_item_id);

-- Typed history (blueprint rec #6: typed jsonb, not a loose bag). `quality`
-- carries the `{quality,revision}` compound (see
-- `src/models/acquisition.rs::QualityStamp`) for grab/import events;
-- `data`/`languages` are typed-by-`event_type` jsonb payload + language
-- list, mirroring `media_files.languages`'s array-of-code shape.
CREATE TABLE history_events (
    id                  bigserial PRIMARY KEY,
    event_type          text NOT NULL,
    media_metadata_id   bigint REFERENCES media_metadata(id) ON DELETE SET NULL,
    monitored_item_id   bigint REFERENCES monitored_items(id) ON DELETE SET NULL,
    download_id         text,
    source_title        text,
    quality             jsonb,
    data                jsonb NOT NULL DEFAULT '{}',
    languages           jsonb NOT NULL DEFAULT '[]',
    created_at          timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX ON history_events (download_id);
CREATE INDEX ON history_events (media_metadata_id);
CREATE INDEX ON history_events (event_type);

-- Releases/hashes the decision engine (MUSEM-04, out of scope here) must
-- never re-grab -- a failed/rejected download or a manually-blocked
-- release.
CREATE TABLE blocklist (
    id                  bigserial PRIMARY KEY,
    source_title        text NOT NULL,
    torrent_hash        text,
    media_metadata_id   bigint REFERENCES media_metadata(id) ON DELETE SET NULL,
    indexer             text,
    message             text,
    size_bytes          bigint,
    created_at          timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX ON blocklist (torrent_hash);
CREATE INDEX ON blocklist (media_metadata_id);
