//! Repo functions for `personas` / `persona_members` (MUSEX-02, Plane TERM
//! #378): the full-replace upsert a single-account derived/explicit persona
//! uses on every recompute, the insert+membership path a shared
//! (household/couch-group) persona uses instead, addressability (list/get
//! by account + id/name — the seam `MUSEX-03` blending consumes), and the
//! raw `genre_counts_for_media_items` query `crate::persona::derive` folds
//! into a persona's `top_genres` defining signal.

use sqlx::{FromRow, PgPool};

use crate::error::{MuseError, MuseResult};
use crate::models::persona::{NewPersona, Persona};

// --- writes ----------------------------------------------------------------

/// Full-replace upsert for a SINGLE-ACCOUNT persona (`new.account_id` must
/// be `Some`), keyed by `(account_id, name, kind)` — the partial unique
/// index `personas_account_name_kind_uniq` in `migrations/0100_personas.sql`
/// backs this `ON CONFLICT`. This is the re-derivation path
/// `crate::persona::derive`'s callers use: re-running derivation on
/// unchanged inputs re-upserts the same row rather than accumulating
/// duplicates, matching `repo::taste::upsert_profile`'s idempotency
/// posture.
pub async fn upsert_for_account(pool: &PgPool, new: &NewPersona) -> MuseResult<Persona> {
    let account_id = new.account_id.ok_or_else(|| {
        MuseError::Config(
            "upsert_for_account requires NewPersona.account_id to be Some".to_string(),
        )
    })?;
    sqlx::query_as::<_, Persona>(
        r#"
        INSERT INTO personas (account_id, name, kind, centroid, defining_signals, metadata, sample_size)
        VALUES ($1, $2, $3, $4, $5, $6, $7)
        ON CONFLICT (account_id, name, kind) WHERE account_id IS NOT NULL DO UPDATE SET
            centroid = EXCLUDED.centroid,
            defining_signals = EXCLUDED.defining_signals,
            metadata = EXCLUDED.metadata,
            sample_size = EXCLUDED.sample_size,
            updated_at = now()
        RETURNING *
        "#,
    )
    .bind(account_id)
    .bind(&new.name)
    .bind(&new.kind)
    .bind(&new.centroid)
    .bind(&new.defining_signals)
    .bind(&new.metadata)
    .bind(new.sample_size)
    .fetch_one(pool)
    .await
    .map_err(MuseError::Database)
}

/// Insert a SHARED (multi-account, `account_id IS NULL`) persona — a
/// household/couch-group persona spanning several accounts
/// (`docs/MUSEX-experience-layer.md` §1.1: a NEW aggregation on top of N
/// accounts, never a mutation of any one account's own taste). Callers add
/// the spanning accounts via [`add_member`] after. Not an upsert (a shared
/// persona has no natural unique key in v0); a caller re-deriving an
/// existing shared persona should track its `id` and call
/// [`replace_centroid`] instead of inserting a duplicate row.
pub async fn insert_shared(pool: &PgPool, new: &NewPersona) -> MuseResult<Persona> {
    sqlx::query_as::<_, Persona>(
        r#"
        INSERT INTO personas (account_id, name, kind, centroid, defining_signals, metadata, sample_size)
        VALUES (NULL, $1, $2, $3, $4, $5, $6)
        RETURNING *
        "#,
    )
    .bind(&new.name)
    .bind(&new.kind)
    .bind(&new.centroid)
    .bind(&new.defining_signals)
    .bind(&new.metadata)
    .bind(new.sample_size)
    .fetch_one(pool)
    .await
    .map_err(MuseError::Database)
}

/// Replace an existing persona's computed vector/signals/sample size in
/// place, by id — the re-derivation path for a shared persona (which has no
/// upsert key) and, interchangeably, a single-account one addressed by id
/// rather than `(account_id, name, kind)`.
pub async fn replace_centroid(
    pool: &PgPool,
    persona_id: i64,
    centroid: &pgvector::Vector,
    defining_signals: &serde_json::Value,
    sample_size: i32,
) -> MuseResult<Persona> {
    sqlx::query_as::<_, Persona>(
        r#"
        UPDATE personas SET
            centroid = $2,
            defining_signals = $3,
            sample_size = $4,
            updated_at = now()
        WHERE id = $1
        RETURNING *
        "#,
    )
    .bind(persona_id)
    .bind(centroid)
    .bind(defining_signals)
    .bind(sample_size)
    .fetch_one(pool)
    .await
    .map_err(MuseError::Database)
}

pub async fn add_member(pool: &PgPool, persona_id: i64, account_id: i64) -> MuseResult<()> {
    sqlx::query(
        "INSERT INTO persona_members (persona_id, account_id) VALUES ($1, $2) ON CONFLICT DO NOTHING",
    )
    .bind(persona_id)
    .bind(account_id)
    .execute(pool)
    .await
    .map_err(MuseError::Database)?;
    Ok(())
}

/// Every account id a shared persona spans, ordered ascending (deterministic
/// — never relies on insertion order).
pub async fn list_members(pool: &PgPool, persona_id: i64) -> MuseResult<Vec<i64>> {
    sqlx::query_scalar::<_, i64>(
        "SELECT account_id FROM persona_members WHERE persona_id = $1 ORDER BY account_id",
    )
    .bind(persona_id)
    .fetch_all(pool)
    .await
    .map_err(MuseError::Database)
}

// --- addressability: list/get by account + id/name --------------------------
//
// The seam MUSEX-03 (persona blending) selects/blends personas through: a
// persona is reachable either because an account owns it directly
// (`personas.account_id = account_id`) or because it's a shared persona the
// account is a member of (`persona_members`).

/// Every persona addressable by `account_id` — owned directly or shared via
/// membership. Ordered by the fully-deterministic `(name, kind, id)` — `id`
/// is the unique tiebreak, so two personas with the same name (e.g. a
/// direct and a shared persona, or two kinds under one name) always come
/// back in a totally-ordered, stable sequence rather than whatever order
/// the planner happens to emit.
pub async fn list_for_account(pool: &PgPool, account_id: i64) -> MuseResult<Vec<Persona>> {
    sqlx::query_as::<_, Persona>(
        r#"
        SELECT * FROM (
            SELECT p.* FROM personas p WHERE p.account_id = $1
            UNION
            SELECT p.* FROM personas p
            JOIN persona_members pm ON pm.persona_id = p.id
            WHERE pm.account_id = $1
        ) addressable
        ORDER BY name, kind, id
        "#,
    )
    .bind(account_id)
    .fetch_all(pool)
    .await
    .map_err(MuseError::Database)
}

pub async fn get_by_id(pool: &PgPool, persona_id: i64) -> MuseResult<Option<Persona>> {
    sqlx::query_as::<_, Persona>("SELECT * FROM personas WHERE id = $1")
        .bind(persona_id)
        .fetch_optional(pool)
        .await
        .map_err(MuseError::Database)
}

/// The persona named `name` addressable by `account_id` (owned directly or
/// via a shared membership) — the name-based half of the addressability
/// seam, alongside [`get_by_id`].
///
/// A single account CAN legitimately hold more than one persona under a
/// given `name`: the `personas_account_name_kind_uniq` index only forbids a
/// duplicate `(account_id, name, kind)`, so a `derived` and an `explicit`
/// persona can share a name, and a directly-owned and a shared persona can
/// too. Rather than forbid that (a name is a human label, not a key), this
/// selects deterministically: the `ORDER BY name, kind, id` is applied to
/// the UNION *before* `LIMIT 1`, so the persona returned for an ambiguous
/// name is always the same one (lowest `(kind, id)`), never planner-order
/// roulette. Callers needing a specific one of several same-named personas
/// should address it by id via [`get_by_id`] (or enumerate with
/// [`list_for_account`]).
pub async fn get_by_name_for_account(
    pool: &PgPool,
    account_id: i64,
    name: &str,
) -> MuseResult<Option<Persona>> {
    sqlx::query_as::<_, Persona>(
        r#"
        SELECT * FROM (
            SELECT p.* FROM personas p WHERE p.account_id = $1 AND p.name = $2
            UNION
            SELECT p.* FROM personas p
            JOIN persona_members pm ON pm.persona_id = p.id
            WHERE pm.account_id = $1 AND p.name = $2
        ) addressable
        ORDER BY name, kind, id
        LIMIT 1
        "#,
    )
    .bind(account_id)
    .bind(name)
    .fetch_optional(pool)
    .await
    .map_err(MuseError::Database)
}

// --- explainability input: genre frequency over an arbitrary media set -----

/// One `(genre, count)` row from [`genre_counts_for_media_items`].
#[derive(Debug, Clone, FromRow)]
pub struct GenreCount {
    pub genre: String,
    pub count: i64,
}

/// Deterministically-ordered genre frequency over `media_item_ids`
/// (`ORDER BY count DESC, genre ASC` — ties break alphabetically, never on
/// whatever order Postgres happens to return matching rows first). The raw
/// input `crate::persona::derive` folds into a persona's `top_genres`
/// defining signal. Returns an empty `Vec` for an empty input rather than
/// issuing a query with an empty `ANY($1)` array.
pub async fn genre_counts_for_media_items(
    pool: &PgPool,
    media_item_ids: &[i64],
) -> MuseResult<Vec<GenreCount>> {
    if media_item_ids.is_empty() {
        return Ok(Vec::new());
    }
    sqlx::query_as::<_, GenreCount>(
        r#"
        SELECT g.name AS genre, count(*)::bigint AS count
        FROM media_items mi
        JOIN media_metadata_genres mmg ON mmg.media_metadata_id = mi.media_metadata_id
        JOIN genres g ON g.id = mmg.genre_id
        WHERE mi.id = ANY($1)
        GROUP BY g.name
        ORDER BY count(*) DESC, g.name ASC
        "#,
    )
    .bind(media_item_ids)
    .fetch_all(pool)
    .await
    .map_err(MuseError::Database)
}
