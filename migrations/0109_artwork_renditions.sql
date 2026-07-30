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

-- Swap the unique key. Dropping only Postgres'' DEFAULT constraint name would
-- leave a renamed constraint — or a bare UNIQUE INDEX — in place, and that
-- surviving 3-column uniqueness would REJECT every rendition insert while the
-- new 5-column index looked correct. So discover it instead of guessing its
-- name: drop any unique CONSTRAINT and any unique INDEX whose key is exactly
-- (entity_kind, entity_id, variant).
DO $$
DECLARE
    obj text;
BEGIN
    -- Unique/PK constraints on exactly those three columns.
    FOR obj IN
        SELECT c.conname
        FROM pg_constraint c
        WHERE c.conrelid = 'artwork_cache'::regclass
          AND c.contype = 'u'
          AND (
                SELECT array_agg(a.attname::text ORDER BY a.attname)
                FROM unnest(c.conkey) k
                JOIN pg_attribute a ON a.attrelid = c.conrelid AND a.attnum = k
              ) = ARRAY['entity_id','entity_kind','variant']
    LOOP
        EXECUTE format('ALTER TABLE artwork_cache DROP CONSTRAINT %I', obj);
        RAISE NOTICE 'dropped 3-column unique constraint %', obj;
    END LOOP;

    -- A bare unique index (no backing constraint) on the same three columns.
    FOR obj IN
        SELECT i.indexrelid::regclass::text
        FROM pg_index i
        WHERE i.indrelid = 'artwork_cache'::regclass
          AND i.indisunique
          AND NOT EXISTS (SELECT 1 FROM pg_constraint c WHERE c.conindid = i.indexrelid)
          AND (
                SELECT array_agg(a.attname::text ORDER BY a.attname)
                FROM unnest(i.indkey) k
                JOIN pg_attribute a ON a.attrelid = i.indrelid AND a.attnum = k
              ) = ARRAY['entity_id','entity_kind','variant']
    LOOP
        EXECUTE format('DROP INDEX %s', obj);
        RAISE NOTICE 'dropped 3-column unique index %', obj;
    END LOOP;
END $$;

-- The sentinel relationship is an INVARIANT, so the database enforces it rather
-- than trusting every caller: width = 0 if and only if format = 'original'.
-- Without this, `store_rendition(width=0, format='jpeg')` would mint a row that
-- is neither a valid master nor a valid rendition, and a width=0 master lookup
-- would then be ambiguous.
ALTER TABLE artwork_cache
    DROP CONSTRAINT IF EXISTS artwork_cache_original_sentinel;
ALTER TABLE artwork_cache
    ADD CONSTRAINT artwork_cache_original_sentinel
    CHECK ((width = 0 AND format = 'original') OR (width > 0 AND format <> 'original'));

-- Partial-free, total unique key over the rendition identity.
CREATE UNIQUE INDEX IF NOT EXISTS artwork_cache_rendition_key
    ON artwork_cache (entity_kind, entity_id, variant, width, format);

-- Lookups for "every rendition of this entity" (cache invalidation + GC).
CREATE INDEX IF NOT EXISTS artwork_cache_entity_renditions_idx
    ON artwork_cache (entity_kind, entity_id, variant);
