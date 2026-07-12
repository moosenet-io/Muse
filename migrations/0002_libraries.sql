-- MUSE-02: libraries — first-class multi-instance dimension.
--
-- ARR-BLUEPRINT §1/§7.9: 8 *arr instances observed live (radarr,
-- radarr_foreign, radarr_anime, radarr_uhd, radarr_animated, sonarr,
-- sonarr_anime, sonarr_animated), each an identical schema sharded purely by
-- root-folder/purpose. Muse mirrors this as ONE schema + N library rows
-- (library_id scoping on instance tables), not N Postgres databases/schemas.
CREATE TABLE libraries (
    id              bigserial PRIMARY KEY,
    name            text NOT NULL UNIQUE,        -- e.g. 'radarr', 'radarr_anime', 'sonarr_animated'
    kind            library_kind NOT NULL,
    root_folder     text NOT NULL,               -- e.g. '/media/Movies/'
    source_arr_name text,                        -- the source *arr instance's own name/slug, for migration provenance
    source_arr_url  text,                        -- base URL of the source arr instance (informational; no creds stored)
    enabled         boolean NOT NULL DEFAULT true,
    created_at      timestamptz NOT NULL DEFAULT now(),
    updated_at      timestamptz NOT NULL DEFAULT now()
);
