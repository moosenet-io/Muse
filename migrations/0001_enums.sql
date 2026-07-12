-- MUSE-02: enum types for the arr-shaped core schema.
--
-- Divergence from spec S96-muse-foundation §3.2: the spec's `media_kind`
-- enum was ('movie','show','season','episode') with a single flat
-- `media_children` table for both seasons and episodes. The ARR-BLUEPRINT
-- recon (docker-301 Sonarr) found a real 3-level TV hierarchy is needed
-- (series -> season -> episode), so seasons and episodes are now their own
-- first-class tables (see 0008_seasons.sql / 0009_episodes.sql) rather than
-- rows distinguished by `kind`. `media_kind` here is narrowed to the two
-- top-level (metadata-bearing) kinds only.
CREATE TYPE media_kind AS ENUM ('movie', 'show');

-- Multi-instance library kind (blueprint §1: 5 Radarr + 3 Sonarr instances
-- observed, sharded by root folder / genre / quality tier).
CREATE TYPE library_kind AS ENUM ('movie', 'tv');

-- File <-> episode satisfaction shape (blueprint §3, Sonarr EpisodeFiles.ReleaseType):
-- a season-pack file can satisfy many episodes at once.
CREATE TYPE release_type_kind AS ENUM ('single', 'multi', 'season_pack');
