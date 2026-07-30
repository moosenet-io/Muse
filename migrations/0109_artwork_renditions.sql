-- MUSE #100: index optimized poster renditions alongside the original, the way
-- Plex/Jellyfin do — capture the provider's image ONCE, then derive and cache
-- sized/encoded variants so a grid tile costs ~15KB instead of ~1.9MB while a
-- detail view still gets a high-quality render.
--
-- The existing UNIQUE (entity_kind, entity_id, variant) is widened to include
-- the rendition dimensions. `width = 0` means THE ORIGINAL (the master), which
-- is why the column is NOT NULL with a 0 default rather than nullable: in
-- Postgres a NULL in a unique key does not conflict with another NULL, so a
-- nullable width would silently permit duplicate originals.
--
-- `format` is the encoded container of THIS row's bytes. Only 'jpeg' is
-- produced today (the `image` crate's webp support is decode-only), but the
-- column is part of the key from day one so adding WebP/AVIF later is a new
-- row rather than a migration + cache wipe.

ALTER TABLE artwork_cache
    ADD COLUMN IF NOT EXISTS width  integer NOT NULL DEFAULT 0,
    ADD COLUMN IF NOT EXISTS format text    NOT NULL DEFAULT 'original';

COMMENT ON COLUMN artwork_cache.width IS
    'Rendition width in px; 0 = the original master image, never a derivative.';
COMMENT ON COLUMN artwork_cache.format IS
    'For a rendition, the encoded container of its bytes (jpeg today; webp/avif reserved). '
    'For the ORIGINAL (width = 0) it is always the literal ''original'' — NOT the master''s real '
    'container — so exactly one original row can exist per (entity_kind, entity_id, variant) '
    'whatever the provider served. Keying the original on its real container would let a provider '
    'that switched from JPEG to PNG mint a SECOND "original" row, and a width=0 lookup would then '
    'return one of them arbitrarily.';

-- Swap the unique key. The old constraint name is Postgres'' default for a
-- table-level UNIQUE(...) on these columns; drop it defensively by name and
-- fall back to nothing if a deployment already renamed it.
ALTER TABLE artwork_cache
    DROP CONSTRAINT IF EXISTS artwork_cache_entity_kind_entity_id_variant_key;

-- Partial-free, total unique key over the rendition identity.
CREATE UNIQUE INDEX IF NOT EXISTS artwork_cache_rendition_key
    ON artwork_cache (entity_kind, entity_id, variant, width, format);

-- Lookups for "every rendition of this entity" (cache invalidation + GC).
CREATE INDEX IF NOT EXISTS artwork_cache_entity_renditions_idx
    ON artwork_cache (entity_kind, entity_id, variant);
