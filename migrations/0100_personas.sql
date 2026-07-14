-- MUSEX-02 (Plane TERM #378): personas — latent taste personas, per account
-- and (for a household/couch-group) spanning several. A persona is a static
-- pgvector taste centroid + jsonb "why this persona" defining-signal
-- provenance, additive on top of the existing `taste_profile`/`embeddings`
-- schema (migrations 0018/0019) — it never mutates either.
--
-- `account_id` is nullable: NOT NULL means a persona owned directly by one
-- account (the common case — "solo-2am" for account 7); NULL means a SHARED
-- persona whose member accounts are listed in `persona_members` (a
-- household/couch-group persona spanning several accounts). This mirrors
-- `docs/MUSEX-experience-layer.md` §1.1's hard invariant that per-account
-- taste (`taste_profile`) is never blended across accounts — a shared
-- persona is a NEW row/aggregation on top of N accounts' own personas, not
-- a mutation of any one account's taste.
CREATE TABLE personas (
    id                bigserial PRIMARY KEY,
    account_id        bigint REFERENCES accounts(id) ON DELETE CASCADE,
    name              text NOT NULL,        -- 'solo-2am','date-night','with-kids',...
    kind              text NOT NULL,        -- 'derived' (context-cluster) | 'explicit' (declared)
    centroid          vector(768) NOT NULL, -- nomic-embed-text, 768-dim — same space as `embeddings`
    defining_signals  jsonb NOT NULL DEFAULT '{}', -- explainability: {"context_key":..,"top_genres":[...],"source_media_item_ids":[...]}
    metadata          jsonb NOT NULL DEFAULT '{}',
    sample_size       int NOT NULL DEFAULT 0,
    created_at        timestamptz NOT NULL DEFAULT now(),
    updated_at        timestamptz NOT NULL DEFAULT now()
);

-- A single-account persona is addressable (and re-derivable via upsert) by
-- (account_id, name, kind); a shared persona (account_id IS NULL) has no
-- such natural key in v0 and is re-derived by id instead (see
-- `repo::persona::replace_centroid`) — hence the partial predicate.
CREATE UNIQUE INDEX personas_account_name_kind_uniq
    ON personas (account_id, name, kind)
    WHERE account_id IS NOT NULL;

CREATE INDEX personas_account_id_idx ON personas (account_id);

-- Same index choice/rationale as `migrations/0018_embeddings.sql`'s
-- `embeddings_hnsw` (cosine distance, no ANALYZE-worthy row count needed).
CREATE INDEX personas_hnsw ON personas USING hnsw (centroid vector_cosine_ops);

-- Membership for SHARED (household/couch-group) personas: which accounts a
-- `personas.account_id IS NULL` row spans. A single-account persona has no
-- rows here (its ownership is `personas.account_id` directly).
CREATE TABLE persona_members (
    persona_id  bigint NOT NULL REFERENCES personas(id) ON DELETE CASCADE,
    account_id  bigint NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    PRIMARY KEY (persona_id, account_id)
);

CREATE INDEX persona_members_account_id_idx ON persona_members (account_id);
