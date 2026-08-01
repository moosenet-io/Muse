-- SUBS-01: which subtitle is active for a media item, where it came from, and
-- what timing offset (if any) an operator confirmed for it.
--
-- One row per subtitle Muse knows about for an item — not just the active one.
-- Keeping the non-active rows is what makes "go back to the one I had before"
-- possible without re-searching a provider, and what lets the UI show an
-- operator the candidates they have already rejected.
--
-- Idempotent throughout (IF NOT EXISTS / DROP ... IF EXISTS before ADD), since
-- Muse applies migrations at startup and a re-run must be a no-op.

CREATE TABLE IF NOT EXISTS subtitle_selections (
    id               bigserial PRIMARY KEY,
    media_item_id    bigint NOT NULL REFERENCES media_items(id) ON DELETE CASCADE,

    -- ISO-639, lowercased, as stored by `subtitles::language_matches`'s
    -- canonicalisation. Nullable: an embedded track whose muxer wrote no
    -- language tag is still a real, selectable subtitle, and recording a
    -- guessed language for it would be worse than recording none.
    language         text,

    -- 'embedded' | 'sidecar' | 'provider'. This is the PREFERENCE TIER, and
    -- the ordering it encodes (embedded < sidecar < provider) is the whole
    -- point of the feature: an embedded track was muxed against this exact
    -- encode and is therefore already in sync, a sidecar was placed for this
    -- release but nothing enforces that it still matches, and a provider
    -- subtitle is the only tier where Muse chose the pairing itself.
    source           text NOT NULL,

    -- Discriminant columns. Exactly one set is populated, enforced by the
    -- CHECK below rather than by convention — a row that claims 'embedded'
    -- but carries no stream index is not a subtitle Muse can ever resolve,
    -- and it must not be storable.
    embedded_stream_index integer,
    -- Persisted ALONGSIDE the index so a stale index is DETECTABLE. The index
    -- alone is only meaningful against one particular file: Foundry's
    -- transcode carries every subtitle stream through (`-map 0:s?` + `-c:s
    -- copy`, verified post-encode), but a file can still be REPLACED by a
    -- quality upgrade whose stream layout differs. See
    -- `subtitles::discover::verify_embedded_selection`, which invalidates a
    -- drifted selection rather than silently pointing it at another track.
    embedded_codec        text,

    sidecar_path          text,

    provider              text,
    provider_subtitle_id  text,
    provider_url          text,
    -- The provider's machine-generated ('ai') flag, carried through to the
    -- operator rather than folded away into a ranking score.
    provider_machine_generated boolean NOT NULL DEFAULT false,

    -- Where Muse's own copy lives, under MUSE_SUBTITLE_STORE_DIR. NULL for an
    -- embedded track (there is no separate file) and for a sidecar Muse has
    -- not re-timed (the sidecar itself is the copy). Never a path inside the
    -- library root: adjusted subtitles are written to Muse's store, and the
    -- library stays read-only.
    storage_path     text,

    -- The offset currently APPLIED, in milliseconds. Zero means unadjusted.
    -- Positive means the subtitle was early and has been pushed later.
    offset_ms        bigint NOT NULL DEFAULT 0,
    -- When an operator confirmed that offset. NOT NULL exactly when offset_ms
    -- is non-zero, enforced below: an offset with no confirmation would mean
    -- something applied a shift without a human, which this feature does not
    -- permit.
    offset_confirmed_at timestamptz,

    -- The most recent DETECTOR PROPOSAL, which is explicitly NOT applied.
    -- Stored separately from offset_ms so a proposal can sit next to the
    -- currently-applied value for the operator to compare. A proposal is a
    -- measurement, not a decision.
    proposed_offset_ms         bigint,
    proposed_confidence        text,
    proposed_at                timestamptz,

    forced           boolean NOT NULL DEFAULT false,
    hearing_impaired boolean NOT NULL DEFAULT false,

    is_active        boolean NOT NULL DEFAULT false,

    created_at       timestamptz NOT NULL DEFAULT now(),
    updated_at       timestamptz NOT NULL DEFAULT now()
);

-- The source discriminant is an INVARIANT, so the database enforces it. A row
-- whose source does not match its populated columns is unresolvable at read
-- time, and the read path would then have to guess — which is exactly the
-- silent-wrong-subtitle failure this feature is built to avoid.
ALTER TABLE subtitle_selections
    DROP CONSTRAINT IF EXISTS subtitle_selections_source_shape;
ALTER TABLE subtitle_selections
    ADD CONSTRAINT subtitle_selections_source_shape CHECK (
        (source = 'embedded'
            AND embedded_stream_index IS NOT NULL
            AND embedded_codec IS NOT NULL
            AND sidecar_path IS NULL
            AND provider_subtitle_id IS NULL)
     OR (source = 'sidecar'
            AND sidecar_path IS NOT NULL
            AND embedded_stream_index IS NULL
            AND provider_subtitle_id IS NULL)
     OR (source = 'provider'
            AND provider IS NOT NULL
            AND provider_subtitle_id IS NOT NULL
            AND embedded_stream_index IS NULL
            AND sidecar_path IS NULL)
    );

-- An applied offset must have a confirmation, and an unapplied one must not
-- claim to. This is the database half of "the detector proposes, a human
-- applies": there is no way to record a non-zero applied offset that nobody
-- confirmed.
ALTER TABLE subtitle_selections
    DROP CONSTRAINT IF EXISTS subtitle_selections_offset_confirmed;
ALTER TABLE subtitle_selections
    ADD CONSTRAINT subtitle_selections_offset_confirmed CHECK (
        (offset_ms = 0 AND offset_confirmed_at IS NULL)
     OR (offset_ms <> 0 AND offset_confirmed_at IS NOT NULL)
    );

-- Confidence, when recorded, must be one of the three the detector can emit.
-- A free-text column would let a typo'd 'hgh' read as an unknown-but-plausible
-- confidence at the UI layer.
ALTER TABLE subtitle_selections
    DROP CONSTRAINT IF EXISTS subtitle_selections_confidence_values;
ALTER TABLE subtitle_selections
    ADD CONSTRAINT subtitle_selections_confidence_values CHECK (
        proposed_confidence IS NULL
     OR proposed_confidence IN ('high', 'low', 'inconclusive')
    );

ALTER TABLE subtitle_selections
    DROP CONSTRAINT IF EXISTS subtitle_selections_source_values;
ALTER TABLE subtitle_selections
    ADD CONSTRAINT subtitle_selections_source_values CHECK (
        source IN ('embedded', 'sidecar', 'provider')
    );

-- At most ONE active subtitle per item per language. A partial unique index
-- rather than a plain one: only active rows participate, so an item can keep
-- any number of inactive candidates for the same language.
--
-- COALESCE on language because an untagged subtitle has NULL there, and in
-- Postgres two NULLs do not conflict in a unique key — without the coalesce,
-- an item could end up with several simultaneously-active untagged subtitles
-- and the read path would return an arbitrary one.
CREATE UNIQUE INDEX IF NOT EXISTS subtitle_selections_one_active_per_language
    ON subtitle_selections (media_item_id, COALESCE(language, ''))
    WHERE is_active;

CREATE INDEX IF NOT EXISTS subtitle_selections_item_idx
    ON subtitle_selections (media_item_id);

-- Re-selecting a provider subtitle already fetched for this item must find the
-- existing row rather than minting a duplicate.
CREATE UNIQUE INDEX IF NOT EXISTS subtitle_selections_provider_key
    ON subtitle_selections (media_item_id, provider, provider_subtitle_id)
    WHERE provider_subtitle_id IS NOT NULL;

COMMENT ON COLUMN subtitle_selections.source IS
    'Preference tier: embedded < sidecar < provider. Embedded is preferred because it was '
    'muxed against this exact encode and therefore cannot be out of sync with the video it '
    'shipped inside; provider is last because it is the only tier where Muse chose the pairing.';
COMMENT ON COLUMN subtitle_selections.offset_ms IS
    'The APPLIED offset in ms, positive = subtitle was early and has been pushed later. Only '
    'ever set from an operator-confirmed proposal; the detector never writes here.';
COMMENT ON COLUMN subtitle_selections.proposed_offset_ms IS
    'The detector''s most recent measurement. NOT applied. Sits beside offset_ms so an '
    'operator can compare a proposal against what is currently in force.';
COMMENT ON COLUMN subtitle_selections.embedded_codec IS
    'Recorded with embedded_stream_index so a stale index is detectable after the file is '
    'replaced — a selection that no longer matches is invalidated, never re-pointed.';
