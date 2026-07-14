-- MUSEX-18 (Plane TERM #394): the Constellation GUI control + tuning panel.
--
-- A single persisted settings surface for the whole experience layer
-- (channel director, watch_together, adaptation loop, discord bot,
-- what's-hot/trending, KG viz, question frequency, personas, sharing
-- granularity). Stored as one JSONB document under a singleton row
-- (`id = 1`, enforced by the CHECK + primary key) rather than one column
-- per tunable -- the shape is owned by `crate::settings::ExperienceSettings`
-- (serde), not by this schema, so adding a new tunable never needs a new
-- migration. See `src/repo/settings.rs` for the load/save layer this backs.
CREATE TABLE experience_settings (
    id          smallint PRIMARY KEY DEFAULT 1 CHECK (id = 1),
    data        jsonb NOT NULL,
    updated_at  timestamptz NOT NULL DEFAULT now()
);
