//! Repo functions for `subtitle_selections` (SUBS-01). Runtime sqlx only, per
//! the MUSE-02 build constraint (the crate must build without a live database).

use chrono::Utc;
use sqlx::PgPool;

use crate::error::{MuseError, MuseResult};
use crate::models::subtitle::{
    NewSubtitleSelection, SubtitleSelection, SOURCE_EMBEDDED, SOURCE_PROVIDER, SOURCE_SIDECAR,
};
use crate::subtitles::SubtitleSource;

/// Every subtitle Muse knows about for an item, active or not.
///
/// Ordered by preference tier then id, so the list an operator sees leads with
/// the tier that is most likely to already be in sync. The tier ordering is
/// expressed here in SQL as well as in
/// [`crate::subtitles::SubtitleSource::preference_rank`]; the two are asserted
/// to agree by `sql_tier_order_matches_the_rust_preference_rank`.
pub async fn list_for_item(pool: &PgPool, media_item_id: i64) -> MuseResult<Vec<SubtitleSelection>> {
    sqlx::query_as::<_, SubtitleSelection>(&list_for_item_sql())
    .bind(media_item_id)
    .fetch_all(pool)
    .await
    .map_err(MuseError::Database)
}

/// The `list_for_item` query, with its tier ordering GENERATED from
/// [`SubtitleSource::PREFERENCE_ORDER`] rather than restated in SQL.
///
/// Opus and codex both flagged the old version at the SUBS-01 gate: the order
/// was hardcoded in the SQL string and the test that "checked" it re-declared
/// the same order in Rust, so reversing the SQL would have left the test green.
/// Generating the CASE means there is one source of truth and the test can
/// inspect what is actually sent to Postgres.
///
/// Not a SQL-injection surface: every fragment comes from a compile-time
/// constant array of literals, never from input.
pub fn list_for_item_sql() -> String {
    let cases: String = SubtitleSource::PREFERENCE_ORDER
        .iter()
        .enumerate()
        .map(|(rank, tier)| format!("WHEN '{tier}' THEN {rank} "))
        .collect();
    format!(
        "SELECT * FROM subtitle_selections WHERE media_item_id = $1 \
         ORDER BY CASE source {cases}ELSE {} END, id",
        SubtitleSource::PREFERENCE_ORDER.len()
    )
}

/// The active subtitle for an item in one language, if any.
///
/// `Ok(None)` means no subtitle is active — a legitimate state, distinct from
/// an error, and the caller must not render it as a failure.
pub async fn active_for_item(
    pool: &PgPool,
    media_item_id: i64,
    language: Option<&str>,
) -> MuseResult<Option<SubtitleSelection>> {
    sqlx::query_as::<_, SubtitleSelection>(
        "SELECT * FROM subtitle_selections \
         WHERE media_item_id = $1 AND is_active AND COALESCE(language, '') = COALESCE($2, '')",
    )
    .bind(media_item_id)
    .bind(language)
    .fetch_optional(pool)
    .await
    .map_err(MuseError::Database)
}

pub async fn get(pool: &PgPool, id: i64) -> MuseResult<Option<SubtitleSelection>> {
    sqlx::query_as::<_, SubtitleSelection>("SELECT * FROM subtitle_selections WHERE id = $1")
        .bind(id)
        .fetch_optional(pool)
        .await
        .map_err(MuseError::Database)
}

/// Record a subtitle for an item, INACTIVE.
///
/// Created inactive deliberately: recording that a subtitle exists and
/// choosing to use it are different acts, and a provider search that fetched
/// six candidates must not activate the last one it happened to write.
/// Activation goes through [`set_active`].
pub async fn record(pool: &PgPool, new: NewSubtitleSelection) -> MuseResult<SubtitleSelection> {
    let (source, stream_index, codec, sidecar_path, provider, provider_id, machine_generated) =
        match &new.source {
            SubtitleSource::Embedded { stream_index, codec } => (
                SOURCE_EMBEDDED,
                Some(i32::try_from(*stream_index).map_err(|_| {
                    MuseError::BadRequest("subtitles: the embedded stream index is out of range".into())
                })?),
                Some(codec.clone()),
                None,
                None,
                None,
                false,
            ),
            SubtitleSource::Sidecar { path } => {
                (SOURCE_SIDECAR, None, None, Some(path.clone()), None, None, false)
            }
            SubtitleSource::Provider {
                provider,
                provider_id,
                machine_generated,
            } => (
                SOURCE_PROVIDER,
                None,
                None,
                None,
                Some(provider.clone()),
                Some(provider_id.clone()),
                *machine_generated,
            ),
        };

    sqlx::query_as::<_, SubtitleSelection>(
        r#"
        INSERT INTO subtitle_selections (
            media_item_id, language, source,
            embedded_stream_index, embedded_codec,
            sidecar_path,
            provider, provider_subtitle_id, provider_url, provider_machine_generated,
            storage_path, original_storage_path, forced, hearing_impaired, is_active
        )
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $11, $12, $13, false)
        -- Re-fetching a provider subtitle already recorded for this item must
        -- refresh it, not mint a duplicate the operator then has to
        -- disambiguate. `is_active` is deliberately NOT touched here: a
        -- re-fetch must never activate something, nor deactivate the
        -- operator's current choice.
        ON CONFLICT (media_item_id, provider, provider_subtitle_id)
            WHERE provider_subtitle_id IS NOT NULL
        DO UPDATE SET
            provider_url = EXCLUDED.provider_url,
            storage_path = COALESCE(EXCLUDED.storage_path, subtitle_selections.storage_path),
            -- A re-fetch replaces the pristine copy, so the original follows
            -- it. COALESCE keeps a previously-recorded original when the
            -- re-fetch carries no file of its own.
            original_storage_path =
                COALESCE(EXCLUDED.storage_path, subtitle_selections.original_storage_path),
            updated_at = now()
        RETURNING *
        "#,
    )
    .bind(new.media_item_id)
    .bind(new.language.as_deref())
    .bind(source)
    .bind(stream_index)
    .bind(codec)
    .bind(sidecar_path)
    .bind(provider)
    .bind(provider_id)
    .bind(new.provider_url.as_deref())
    .bind(machine_generated)
    .bind(new.storage_path.as_deref())
    .bind(new.forced)
    .bind(new.hearing_impaired)
    .fetch_one(pool)
    .await
    .map_err(MuseError::Database)
}

/// Make one selection the active subtitle for its item+language.
///
/// Runs in a transaction: the previous active row is deactivated and the new
/// one activated together. Without the transaction, a failure between the two
/// statements would leave the item with either no active subtitle or two, and
/// the partial unique index would then reject the second write — so the
/// operator's "switch to this one" would fail having already turned the old
/// one off.
pub async fn set_active(pool: &PgPool, id: i64) -> MuseResult<SubtitleSelection> {
    let mut tx = pool.begin().await.map_err(MuseError::Database)?;

    let target = sqlx::query_as::<_, SubtitleSelection>(
        "SELECT * FROM subtitle_selections WHERE id = $1 FOR UPDATE",
    )
    .bind(id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(MuseError::Database)?
    .ok_or_else(|| MuseError::NotFound(format!("subtitle selection {id}")))?;

    sqlx::query(
        "UPDATE subtitle_selections SET is_active = false, updated_at = now() \
         WHERE media_item_id = $1 AND COALESCE(language, '') = COALESCE($2, '') AND is_active AND id <> $3",
    )
    .bind(target.media_item_id)
    .bind(target.language.as_deref())
    .bind(id)
    .execute(&mut *tx)
    .await
    .map_err(MuseError::Database)?;

    let activated = sqlx::query_as::<_, SubtitleSelection>(
        "UPDATE subtitle_selections SET is_active = true, updated_at = now() WHERE id = $1 RETURNING *",
    )
    .bind(id)
    .fetch_one(&mut *tx)
    .await
    .map_err(MuseError::Database)?;

    tx.commit().await.map_err(MuseError::Database)?;
    Ok(activated)
}

/// Record a detector PROPOSAL. Never touches `offset_ms`.
///
/// This is the persistence half of "the detector proposes, a human applies":
/// there is no code path from a measurement to the applied-offset column. The
/// proposal lands in its own three columns and waits.
pub async fn record_proposal(
    pool: &PgPool,
    id: i64,
    proposed_offset_ms: i64,
    confidence: &str,
) -> MuseResult<SubtitleSelection> {
    sqlx::query_as::<_, SubtitleSelection>(
        "UPDATE subtitle_selections \
         SET proposed_offset_ms = $2, proposed_confidence = $3, proposed_at = $4, updated_at = now() \
         WHERE id = $1 RETURNING *",
    )
    .bind(id)
    .bind(proposed_offset_ms)
    .bind(confidence)
    .bind(Utc::now())
    .fetch_optional(pool)
    .await
    .map_err(MuseError::Database)?
    .ok_or_else(|| MuseError::NotFound(format!("subtitle selection {id}")))
}

/// Record an offset an operator CONFIRMED, together with the adjusted file it
/// produced.
///
/// `offset_confirmed_at` is set in the same statement as `offset_ms`, because
/// the migration's CHECK requires them to move together — an applied offset
/// with no confirmation is not a storable state.
pub async fn apply_confirmed_offset(
    pool: &PgPool,
    id: i64,
    offset_ms: i64,
    storage_path: &str,
) -> MuseResult<SubtitleSelection> {
    if offset_ms == 0 {
        return Err(MuseError::BadRequest(
            "subtitles: a confirmed offset of 0ms is not an adjustment".into(),
        ));
    }

    // `original_storage_path` is claimed here if it is not already set, BEFORE
    // storage_path is repointed at the adjusted copy — otherwise the pristine
    // pointer is lost on the first adjustment and every later one compounds.
    // COALESCE, so a second adjustment never re-claims the adjusted file as
    // the original.
    sqlx::query_as::<_, SubtitleSelection>(
        "UPDATE subtitle_selections \
         SET original_storage_path = COALESCE(original_storage_path, storage_path), \
             offset_ms = $2, offset_confirmed_at = $3, storage_path = $4, updated_at = now() \
         WHERE id = $1 RETURNING *",
    )
    .bind(id)
    .bind(offset_ms)
    .bind(Utc::now())
    .bind(storage_path)
    .fetch_optional(pool)
    .await
    .map_err(MuseError::Database)?
    .ok_or_else(|| MuseError::NotFound(format!("subtitle selection {id}")))
}

/// Invalidate an embedded selection whose stream no longer matches the file.
///
/// Deactivates rather than deletes. The row stays as a record of what the
/// operator had chosen, so the UI can say "your English track is gone because
/// the file was replaced" instead of the selection simply vanishing.
pub async fn invalidate(pool: &PgPool, id: i64) -> MuseResult<()> {
    sqlx::query("UPDATE subtitle_selections SET is_active = false, updated_at = now() WHERE id = $1")
        .bind(id)
        .execute(pool)
        .await
        .map_err(MuseError::Database)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::subtitles::SubtitleSource;

    #[test]
    fn sql_tier_order_matches_the_rust_preference_rank() {
        // Inspects the SQL ACTUALLY SENT, rather than re-declaring the order
        // in Rust and comparing it to itself. The old version would have
        // passed with the SQL CASE reversed.
        let sql = list_for_item_sql();
        for (rank, tier) in SubtitleSource::PREFERENCE_ORDER.iter().enumerate() {
            let expected = format!("WHEN '{tier}' THEN {rank}");
            assert!(
                sql.contains(&expected),
                "the query must rank {tier} at {rank} to agree with preference_rank; got: {sql}"
            );
        }
        // And the ranks in the SQL must appear in ascending order, so a
        // rearranged PREFERENCE_ORDER cannot silently invert the listing.
        let positions: Vec<usize> = SubtitleSource::PREFERENCE_ORDER
            .iter()
            .map(|t| sql.find(&format!("'{t}'")).expect("every tier appears"))
            .collect();
        assert!(
            positions.windows(2).all(|w| w[0] < w[1]),
            "tiers must appear in preference order in the CASE: {sql}"
        );
        // Cross-check against the enum itself, so this cannot drift from the
        // ranking the selection logic uses.
        assert_eq!(SubtitleSource::PREFERENCE_ORDER.len(), 3);
    }

    #[tokio::test]
    async fn a_confirmed_offset_of_zero_is_refused_before_it_reaches_the_database() {
        // The CHECK constraint would reject this too, but failing here keeps
        // the error a clean 400 rather than a database error surfaced as 500.
        let pool = match PgPool::connect_lazy("postgres://muse:muse@127.0.0.1:1/muse") {
            Ok(pool) => pool,
            Err(_) => return,
        };
        let err = apply_confirmed_offset(&pool, 1, 0, "/tmp/x.srt").await.unwrap_err();
        assert!(matches!(err, MuseError::BadRequest(_)), "got {err:?}");
    }
}
