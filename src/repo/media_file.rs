//! Repo functions for `media_files` + the `episode_files` many-to-many join
//! (blueprint §3/§7.3: 1:1 for movies via `media_item_id`, many-to-many for
//! TV season-pack files via `attach_to_episode`).

use sqlx::PgPool;

use crate::error::{MuseError, MuseResult};
use crate::media::doc::{MediaInfoDoc, StoredProbeState};
use crate::media::probe::{MediaProbe, ProbeError};
use crate::models::media_file::{MediaFile, NewMediaFile, ReleaseTypeKind};

/// MUSEL-B1: idempotent upsert for one file the library scanner found —
/// `(media_item_id, relative_path)` is the scanner's own de-dup key (there
/// is no DB-level `UNIQUE` on that pair; `media_files` predates the scanner
/// and its real uniqueness key is the `(id, media_item_id)` superkey used by
/// `episode_files`' composite FK — see `migrations/0009_media_files.sql`).
/// Looks the row up first, then either leaves it untouched (unchanged size
/// — the mtime/size guard the spec asks for; this schema has no
/// `media_files` mtime column, so size is the change signal actually
/// available), updates `size_bytes`/`media_info` when the file changed size
/// since the last scan, or inserts a fresh row. Returns `(row, changed)`
/// so a caller can distinguish "already up to date" from "recorded/updated
/// this pass" for scan-report counters.
pub async fn upsert_scanned(
    pool: &PgPool,
    media_item_id: i64,
    relative_path: &str,
    size_bytes: Option<i64>,
    media_info: Option<serde_json::Value>,
) -> MuseResult<(MediaFile, bool)> {
    let existing = sqlx::query_as::<_, MediaFile>(
        "SELECT * FROM media_files WHERE media_item_id = $1 AND relative_path = $2",
    )
    .bind(media_item_id)
    .bind(relative_path)
    .fetch_optional(pool)
    .await
    .map_err(MuseError::Database)?;

    if let Some(row) = existing {
        if row.size_bytes == size_bytes {
            // Unchanged since the last scan -- clean no-op, no write.
            return Ok((row, false));
        }

        let updated = sqlx::query_as::<_, MediaFile>(
            r#"
            UPDATE media_files SET
                size_bytes = $2,
                media_info = COALESCE($3, media_info),
                date_added = COALESCE(date_added, now())
            WHERE id = $1
            RETURNING *
            "#,
        )
        .bind(row.id)
        .bind(size_bytes)
        .bind(&media_info)
        .fetch_one(pool)
        .await
        .map_err(MuseError::Database)?;
        return Ok((updated, true));
    }

    let created = sqlx::query_as::<_, MediaFile>(
        r#"
        INSERT INTO media_files (
            media_item_id, relative_path, size_bytes, media_info, date_added,
            languages, release_type
        )
        VALUES ($1, $2, $3, $4, now(), '{}', $5)
        RETURNING *
        "#,
    )
    .bind(media_item_id)
    .bind(relative_path)
    .bind(size_bytes)
    .bind(&media_info)
    .bind(ReleaseTypeKind::Single)
    .fetch_one(pool)
    .await
    .map_err(MuseError::Database)?;

    Ok((created, true))
}

pub async fn create(pool: &PgPool, new: &NewMediaFile) -> MuseResult<MediaFile> {
    sqlx::query_as::<_, MediaFile>(
        r#"
        INSERT INTO media_files (
            media_item_id, relative_path, size_bytes, release_group, languages,
            release_type, quality_tier_id, revision_version, revision_real, revision_is_repack
        )
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
        RETURNING *
        "#,
    )
    .bind(new.media_item_id)
    .bind(&new.relative_path)
    .bind(new.size_bytes)
    .bind(&new.release_group)
    .bind(&new.languages)
    .bind(new.release_type)
    .bind(new.quality_tier_id)
    .bind(new.revision.version)
    .bind(new.revision.real)
    .bind(new.revision.is_repack)
    .fetch_one(pool)
    .await
    .map_err(MuseError::Database)
}

pub async fn get(pool: &PgPool, id: i64) -> MuseResult<MediaFile> {
    sqlx::query_as::<_, MediaFile>("SELECT * FROM media_files WHERE id = $1")
        .bind(id)
        .fetch_optional(pool)
        .await
        .map_err(MuseError::Database)?
        .ok_or_else(|| MuseError::NotFound(format!("media_file {id} not found")))
}

pub async fn list_by_media_item(pool: &PgPool, media_item_id: i64) -> MuseResult<Vec<MediaFile>> {
    sqlx::query_as::<_, MediaFile>(
        "SELECT * FROM media_files WHERE media_item_id = $1 ORDER BY date_added DESC NULLS LAST",
    )
    .bind(media_item_id)
    .fetch_all(pool)
    .await
    .map_err(MuseError::Database)
}

/// Link a (possibly season-pack) file to an episode it satisfies. Idempotent.
///
/// The join's `media_item_id` is derived from the episode, so the composite FK
/// to `media_files (id, media_item_id)` rejects any attempt to attach a file
/// from a different show (surfaces as `MuseError::Database`). Attaching to a
/// non-existent episode inserts nothing (the SELECT yields no row).
pub async fn attach_to_episode(pool: &PgPool, episode_id: i64, media_file_id: i64) -> MuseResult<()> {
    sqlx::query(
        r#"
        INSERT INTO episode_files (episode_id, media_file_id, media_item_id)
        SELECT e.id, $2, e.media_item_id
        FROM episodes e
        WHERE e.id = $1
        ON CONFLICT (episode_id, media_file_id) DO NOTHING
        "#,
    )
    .bind(episode_id)
    .bind(media_file_id)
    .execute(pool)
    .await
    .map_err(MuseError::Database)?;
    Ok(())
}

/// All files that (fully or partially, via a season pack) satisfy an episode.
pub async fn list_for_episode(pool: &PgPool, episode_id: i64) -> MuseResult<Vec<MediaFile>> {
    sqlx::query_as::<_, MediaFile>(
        r#"
        SELECT mf.* FROM media_files mf
        JOIN episode_files ef ON ef.media_file_id = mf.id
        WHERE ef.episode_id = $1
        ORDER BY mf.date_added DESC NULLS LAST
        "#,
    )
    .bind(episode_id)
    .fetch_all(pool)
    .await
    .map_err(MuseError::Database)
}

/// All episodes a given (possibly season-pack) file satisfies.
pub async fn list_episode_ids_for_file(pool: &PgPool, media_file_id: i64) -> MuseResult<Vec<i64>> {
    let rows: Vec<(i64,)> = sqlx::query_as(
        "SELECT episode_id FROM episode_files WHERE media_file_id = $1 ORDER BY episode_id",
    )
    .bind(media_file_id)
    .fetch_all(pool)
    .await
    .map_err(MuseError::Database)?;
    Ok(rows.into_iter().map(|(id,)| id).collect())
}

// --- MPRB-05: the probe columns --------------------------------------------

/// Aggregate probe coverage over the whole `media_files` table.
///
/// `suspicious` counts as **probed** for completion and as **needing attention**
/// for the report. Those are two different questions, and conflating them is what
/// makes a backfill look finished when it is not — so both are reported, and
/// neither is derived from the other by the caller.
#[derive(Debug, Clone, Default, PartialEq, Eq, sqlx::FromRow, serde::Serialize)]
pub struct ProbeProgress {
    pub total: i64,
    /// Rows carrying a v1+ document: `ok` + `suspicious`.
    pub probed: i64,
    /// No `media_info_version` — never probed by S130. **Includes `legacy`**,
    /// because a legacy container-only row has not been probed either; it merely
    /// has something in the column.
    pub unprobed: i64,
    /// The pre-S130 `{"container": "<ext>"}` shape: a non-null `media_info` with
    /// no version. A subset of `unprobed`.
    pub legacy: i64,
    pub ok: i64,
    pub suspicious: i64,
    pub unreadable: i64,
    pub probe_failed: i64,
    /// Unprobed AND out of attempts — the backfill will never return to these, so
    /// a sweep that reports "complete" while this is nonzero is telling the truth
    /// only if this number is stated alongside it.
    pub permanently_failed: i64,
}

/// Record a successful probe: the document, its version mirror, the timestamp,
/// the state, and a cleared attempt counter — in ONE statement, so
/// `media_info_version` cannot diverge from the document's `schema_version`.
///
/// `suspicion` is the caller's description of what looks wrong, or `None`. This
/// layer does **not** decide what is suspicious: that rule belongs with the probe
/// extensions and there must be exactly one of it. A suspicious result is still
/// stored — it parsed, and partial data serves the `/why` endpoint better than a
/// null — but stored *labelled*, with `probe_error` carrying the description so
/// one column answers "what is wrong with this file" for every unhappy state.
///
/// `relative_path` is passed rather than re-read because the caller already holds
/// it; it supplies `file_extension`, the one fact `MediaProbe` cannot know.
pub async fn set_probe_result(
    pool: &PgPool,
    id: i64,
    relative_path: &str,
    probe: &MediaProbe,
    suspicion: Option<&str>,
) -> MuseResult<()> {
    let doc = MediaInfoDoc::new(probe.clone(), relative_path);
    let json = doc
        .to_json()
        .map_err(|e| MuseError::Internal(anyhow::anyhow!("serialising the probe document: {e}")))?;
    let state = match suspicion {
        Some(_) => StoredProbeState::Suspicious,
        None => StoredProbeState::Ok,
    };

    sqlx::query(
        r#"
        UPDATE media_files SET
            media_info         = $2,
            media_info_version = $3,
            probed_at          = now(),
            probe_state        = $4,
            probe_error        = $5,
            probe_attempts     = 0
        WHERE id = $1
        "#,
    )
    .bind(id)
    .bind(&json)
    .bind(i32::from(doc.schema_version))
    .bind(state.as_str())
    .bind(suspicion.map(truncate_probe_error))
    .execute(pool)
    .await
    .map_err(MuseError::Database)?;

    Ok(())
}

/// Record a failed probe.
///
/// **`media_info` is deliberately not in the SET list.** A failed re-probe of a
/// file that probed fine last month — an NFS stall, a temporarily missing binary
/// — must never destroy the good result already stored. The failure is recorded
/// beside it, not over it.
///
/// The state comes from [`ProbeError::state`] (MPRB-02) via
/// [`StoredProbeState::from_error`]; this function contains no classification of
/// its own.
pub async fn set_probe_error(pool: &PgPool, id: i64, error: &ProbeError) -> MuseResult<()> {
    sqlx::query(
        r#"
        UPDATE media_files SET
            probed_at      = now(),
            probe_state    = $2,
            probe_error    = $3,
            probe_attempts = probe_attempts + 1
        WHERE id = $1
        "#,
    )
    .bind(id)
    .bind(StoredProbeState::from_error(error).as_str())
    .bind(truncate_probe_error(&error.to_string()))
    .execute(pool)
    .await
    .map_err(MuseError::Database)?;

    Ok(())
}

/// The backfill queue: files with no v1 document that still have attempts left.
///
/// **Keyset pagination on `id > after_id`, never `OFFSET`.** The backfill resumes
/// from a cursor and restarts after a crash; `OFFSET` degrades quadratically over
/// a library of this size (~16,000 titles), and worse, it *shifts* when a row is
/// updated mid-sweep, so a resumed run can skip files entirely.
///
/// The predicate matches the partial index in `0113` exactly. A `suspicious` row
/// carries a v1 document and is therefore NOT returned — it has been probed; it
/// merely needs a human, which is what the audit index is for.
pub async fn list_needing_probe(
    pool: &PgPool,
    after_id: i64,
    limit: i64,
    max_attempts: i32,
) -> MuseResult<Vec<MediaFile>> {
    sqlx::query_as::<_, MediaFile>(
        r#"
        SELECT * FROM media_files
        WHERE id > $1
          AND (media_info_version IS NULL OR media_info_version < 1)
          AND probe_attempts < $2
        ORDER BY id
        LIMIT $3
        "#,
    )
    .bind(after_id)
    .bind(max_attempts)
    .bind(limit)
    .fetch_all(pool)
    .await
    .map_err(MuseError::Database)
}

/// Probe coverage, one row, one round trip.
///
/// `max_attempts` must be the same value the sweep passes to
/// [`list_needing_probe`], or `permanently_failed` describes a queue that is not
/// the one actually running.
pub async fn probe_progress(pool: &PgPool, max_attempts: i32) -> MuseResult<ProbeProgress> {
    sqlx::query_as::<_, ProbeProgress>(
        r#"
        SELECT
            count(*)                                                         AS total,
            count(*) FILTER (WHERE media_info_version >= 1)                  AS probed,
            count(*) FILTER (WHERE media_info_version IS NULL)               AS unprobed,
            count(*) FILTER (WHERE media_info_version IS NULL
                               AND media_info IS NOT NULL)                   AS legacy,
            count(*) FILTER (WHERE probe_state = 'ok')                       AS ok,
            count(*) FILTER (WHERE probe_state = 'suspicious')               AS suspicious,
            count(*) FILTER (WHERE probe_state = 'unreadable')               AS unreadable,
            count(*) FILTER (WHERE probe_state = 'probe_failed')             AS probe_failed,
            count(*) FILTER (WHERE media_info_version IS NULL
                               AND probe_attempts >= $1)                     AS permanently_failed
        FROM media_files
        "#,
    )
    .bind(max_attempts)
    .fetch_one(pool)
    .await
    .map_err(MuseError::Database)
}

/// `probe_error` is a diagnostic, not a log sink. ffprobe can emit kilobytes on a
/// damaged file, and an unbounded splice into a text column is how one bad file
/// makes a row unreadable in every listing that renders it.
fn truncate_probe_error(message: &str) -> String {
    const MAX_BYTES: usize = 1024;
    if message.len() <= MAX_BYTES {
        return message.to_string();
    }
    // Byte-bounded but char-boundary-safe: slicing mid-UTF-8 panics, and a
    // stderr blob is not guaranteed to be ASCII.
    let mut end = MAX_BYTES;
    while end > 0 && !message.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}… (truncated)", &message[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_short_probe_error_is_stored_verbatim() {
        let message = "ffprobe exited with 1 — the file is unreadable";
        assert_eq!(truncate_probe_error(message), message);
    }

    #[test]
    fn an_enormous_probe_error_is_bounded_and_says_so() {
        let message = "x".repeat(10_000);
        let truncated = truncate_probe_error(&message);
        assert!(truncated.len() <= 1024 + "… (truncated)".len());
        assert!(truncated.ends_with("… (truncated)"));
    }

    #[test]
    fn truncation_never_splits_a_utf8_character() {
        // The character must genuinely STRADDLE the 1024-byte bound, or the test
        // is blind: `é` is 2 bytes and 1024 is even, so a naive `&s[..1024]`
        // lands on a boundary and never panics — this test PASSED against a
        // deliberately-broken implementation until the fixture was fixed. `€` is
        // 3 bytes and 1024 % 3 == 1, so byte 1024 is mid-character.
        let message = "€".repeat(1000);
        assert_eq!(message.len(), 3000);
        assert!(!message.is_char_boundary(1024), "the fixture must straddle");
        let truncated = truncate_probe_error(&message);
        assert!(truncated.starts_with('€'));
        assert!(truncated.ends_with("… (truncated)"));

        // And the even-width case still behaves.
        assert!(truncate_probe_error(&"é".repeat(2000)).ends_with("… (truncated)"));
    }

    #[test]
    fn the_state_written_for_a_failure_is_mprb_02s_classification_not_a_new_one() {
        let error = ProbeError::Timeout { secs: 30 };
        assert_eq!(
            StoredProbeState::from_error(&error).as_str(),
            error.state().as_str()
        );
    }

    /// MPRB-05: the probe columns against a real Postgres.
    ///
    /// **These do not run in this environment.** There is no
    /// `MUSE_TEST_DATABASE_URL` provisioned (MUSE #130), so every assertion below
    /// — the `0113` DDL applying at all, the `CHECK` constraint, both partial
    /// indexes, and all four repo queries — is SKIPPED, not passed. MPRB-10's
    /// live backfill is their first real execution. A green suite here proves the
    /// pure logic above and nothing about this.
    #[cfg(test)]
    mod db_gated {
        use super::*;
        use crate::media::probe::{MediaProbe, ProbeError, VideoStream};

        async fn test_pool_or_skip(test_name: &str) -> Option<sqlx::PgPool> {
            let Ok(database_url) = std::env::var("MUSE_TEST_DATABASE_URL") else {
                eprintln!(
                    "MUSE_TEST_DATABASE_URL not set — SKIPPING {test_name}. This test did \
                     NOT pass; it did not run."
                );
                return None;
            };
            let pool = sqlx::postgres::PgPoolOptions::new()
                .max_connections(5)
                .connect(&database_url)
                .await
                .expect("connect to MUSE_TEST_DATABASE_URL");
            sqlx::migrate!("./migrations")
                .run(&pool)
                .await
                .expect("migrations should apply cleanly");
            Some(pool)
        }

        fn a_probe() -> MediaProbe {
            MediaProbe {
                container: "matroska,webm".to_string(),
                duration_secs: Some(120.0),
                format_bitrate_bps: Some(1_000_000),
                size_bytes: Some(2000),
                video: vec![VideoStream {
                    index: 0,
                    codec: "hevc".to_string(),
                    width: Some(1920),
                    height: Some(1080),
                    ..Default::default()
                }],
                audio: vec![],
                subtitles: vec![],
                attachments: vec![],
                data_stream_count: 0,
                unindexed_stream_count: 0,
                chapter_count: 0,
                title: None,
                other_stream_count: 0,
                notes: Vec::new(),
            }
        }

        /// Seed one `media_files` row and return it, with a unique suffix so
        /// concurrent runs cannot collide.
        async fn seed_file(pool: &sqlx::PgPool, suffix: &str) -> MediaFile {
            let library = crate::repo::library::create(
                pool,
                &crate::models::library::NewLibrary {
                    name: format!("mprb05-{suffix}"),
                    kind: crate::models::library::LibraryKind::Movie,
                    root_folder: format!("/tmp/muse-mprb05-{suffix}"),
                    source_arr_name: None,
                    source_arr_url: None,
                },
            )
            .await
            .expect("seed library");

            let metadata = crate::repo::media_metadata::upsert_by_tmdb(
                pool,
                &crate::models::media_metadata::NewMediaMetadata {
                    kind: crate::models::media_metadata::MediaKind::Movie,
                    tmdb_id: Some(format!("mprb05-{suffix}")),
                    tvdb_id: None,
                    imdb_id: None,
                    provider_ids: serde_json::json!({}),
                    title: format!("MPRB05 {suffix}"),
                    sort_title: None,
                    original_title: None,
                    original_language: None,
                    status: None,
                    overview: None,
                    studio: None,
                    network: None,
                    runtime_minutes: None,
                    year: None,
                    images: serde_json::json!([]),
                },
            )
            .await
            .expect("seed media_metadata");

            let item = crate::repo::media_item::upsert(
                pool,
                &crate::models::media_item::NewMediaItem {
                    library_id: library.id,
                    media_metadata_id: metadata.id,
                    path: format!("{suffix}.mkv"),
                    monitored: false,
                    quality_profile_id: None,
                    minimum_availability: None,
                    plex_rating_key: None,
                    added_at: None,
                },
            )
            .await
            .expect("seed media_item");

            let (file, _) = upsert_scanned(
                pool,
                item.id,
                &format!("{suffix}.mkv"),
                Some(2000),
                Some(serde_json::json!({ "container": "mkv" })),
            )
            .await
            .expect("seed media_file");
            file
        }

        /// The acceptance criterion with the sharpest teeth: `set_probe_error`
        /// must NEVER overwrite a previously-good `media_info`.
        #[tokio::test]
        async fn a_failed_reprobe_never_destroys_a_good_result() {
            let Some(pool) =
                test_pool_or_skip("a_failed_reprobe_never_destroys_a_good_result").await
            else {
                return;
            };
            let suffix = uuid::Uuid::new_v4().simple().to_string();
            let file = seed_file(&pool, &suffix).await;

            set_probe_result(&pool, file.id, &file.relative_path, &a_probe(), None)
                .await
                .expect("set_probe_result");
            let probed = get(&pool, file.id).await.expect("re-read");
            assert_eq!(probed.media_info_version, Some(1));
            assert_eq!(probed.probe_state_parsed(), Some(StoredProbeState::Ok));
            assert_eq!(probed.probe_attempts, 0);
            assert!(probed.probed_at.is_some());
            let good_doc = probed.media_info.clone();
            assert!(matches!(
                probed.stored_media_info(),
                crate::media::doc::StoredMediaInfo::V1(_)
            ));

            set_probe_error(&pool, file.id, &ProbeError::Timeout { secs: 30 })
                .await
                .expect("set_probe_error");
            let failed = get(&pool, file.id).await.expect("re-read after failure");
            assert_eq!(
                failed.media_info, good_doc,
                "a failed re-probe must leave the good document untouched"
            );
            assert_eq!(failed.media_info_version, Some(1));
            assert_eq!(failed.probe_attempts, 1);
            assert_eq!(
                failed.probe_state.as_deref(),
                Some(ProbeError::Timeout { secs: 30 }.state().as_str())
            );
        }

        /// The `CHECK` constraint accepts every state the code can write — and
        /// rejects one it cannot. Without the negative half, a constraint that
        /// accepted everything would pass.
        #[tokio::test]
        async fn the_check_constraint_accepts_the_taxonomy_and_rejects_anything_else() {
            let Some(pool) = test_pool_or_skip(
                "the_check_constraint_accepts_the_taxonomy_and_rejects_anything_else",
            )
            .await
            else {
                return;
            };
            let suffix = uuid::Uuid::new_v4().simple().to_string();
            let file = seed_file(&pool, &suffix).await;

            for state in [
                StoredProbeState::Ok,
                StoredProbeState::Suspicious,
                StoredProbeState::Failed(crate::media::probe::ProbeState::Unreadable),
                StoredProbeState::Failed(crate::media::probe::ProbeState::ProbeFailed),
            ] {
                sqlx::query("UPDATE media_files SET probe_state = $2 WHERE id = $1")
                    .bind(file.id)
                    .bind(state.as_str())
                    .execute(&pool)
                    .await
                    .unwrap_or_else(|e| panic!("the CHECK rejected {}: {e}", state.as_str()));
            }

            let rejected = sqlx::query("UPDATE media_files SET probe_state = $2 WHERE id = $1")
                .bind(file.id)
                .bind("not_a_state")
                .execute(&pool)
                .await;
            assert!(
                rejected.is_err(),
                "the CHECK constraint accepted a state the code can never write"
            );
        }

        /// A suspicious result is stored, labelled, and is NOT re-queued — it has
        /// been probed; it needs a human, not another probe.
        #[tokio::test]
        async fn a_suspicious_result_is_stored_labelled_and_not_requeued() {
            let Some(pool) =
                test_pool_or_skip("a_suspicious_result_is_stored_labelled_and_not_requeued").await
            else {
                return;
            };
            let suffix = uuid::Uuid::new_v4().simple().to_string();
            let file = seed_file(&pool, &suffix).await;

            set_probe_result(
                &pool,
                file.id,
                &file.relative_path,
                &a_probe(),
                Some("duration is zero on a 4 GB file"),
            )
            .await
            .expect("set_probe_result");

            let row = get(&pool, file.id).await.expect("re-read");
            assert_eq!(
                row.probe_state_parsed(),
                Some(StoredProbeState::Suspicious)
            );
            assert!(row.media_info.is_some(), "a suspicious result is still stored");
            assert_eq!(
                row.probe_error.as_deref(),
                Some("duration is zero on a 4 GB file")
            );

            let queue = list_needing_probe(&pool, 0, 1000, 3)
                .await
                .expect("list_needing_probe");
            assert!(
                !queue.iter().any(|f| f.id == file.id),
                "a suspicious row carries a v1 document and must not be re-queued"
            );
        }

        /// Keyset pagination resumes from a cursor, and a file out of attempts
        /// leaves the queue.
        #[tokio::test]
        async fn the_queue_is_keyset_paginated_and_attempt_bounded() {
            let Some(pool) =
                test_pool_or_skip("the_queue_is_keyset_paginated_and_attempt_bounded").await
            else {
                return;
            };
            let suffix = uuid::Uuid::new_v4().simple().to_string();
            let file = seed_file(&pool, &suffix).await;

            // A legacy container-only row is unprobed and therefore queued.
            let queue = list_needing_probe(&pool, file.id - 1, 1000, 3)
                .await
                .expect("list_needing_probe");
            assert!(queue.iter().any(|f| f.id == file.id));

            // The cursor excludes it.
            let after = list_needing_probe(&pool, file.id, 1000, 3)
                .await
                .expect("list_needing_probe after cursor");
            assert!(!after.iter().any(|f| f.id == file.id));

            // Burn the attempts; it leaves the queue rather than looping forever.
            for _ in 0..3 {
                set_probe_error(&pool, file.id, &ProbeError::NoStreams)
                    .await
                    .expect("set_probe_error");
            }
            let exhausted = list_needing_probe(&pool, file.id - 1, 1000, 3)
                .await
                .expect("list_needing_probe when out of attempts");
            assert!(!exhausted.iter().any(|f| f.id == file.id));

            let progress = probe_progress(&pool, 3).await.expect("probe_progress");
            assert!(progress.total >= 1);
            assert!(progress.permanently_failed >= 1);
            assert!(progress.probe_failed >= 1);
        }
    }

    /// MUSEL-B1: `upsert_scanned`'s idempotency/size-guard behavior,
    /// isolated from the full scanner. Gated on `MUSE_TEST_DATABASE_URL`,
    /// same skip-cleanly-when-unset posture as every other live-DB test in
    /// this crate.
    #[tokio::test]
    async fn upsert_scanned_is_idempotent_and_updates_only_on_a_size_change() {
        let Ok(database_url) = std::env::var("MUSE_TEST_DATABASE_URL") else {
            eprintln!(
                "MUSE_TEST_DATABASE_URL not set -- skipping \
                 upsert_scanned_is_idempotent_and_updates_only_on_a_size_change \
                 (expected in the default test run; the crate does not require a live DB)"
            );
            return;
        };

        use uuid::Uuid;

        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(5)
            .connect(&database_url)
            .await
            .expect("connect to MUSE_TEST_DATABASE_URL");
        sqlx::migrate!("./migrations")
            .run(&pool)
            .await
            .expect("migrations should apply cleanly");

        let suffix = Uuid::new_v4().simple().to_string();

        let library = crate::repo::library::create(
            &pool,
            &crate::models::library::NewLibrary {
                name: format!("media-file-upsert-test-{suffix}"),
                kind: crate::models::library::LibraryKind::Movie,
                root_folder: format!("/tmp/muse-media-file-upsert-test-{suffix}"),
                source_arr_name: None,
                source_arr_url: None,
            },
        )
        .await
        .expect("seed library");

        let metadata = crate::repo::media_metadata::upsert_by_tmdb(
            &pool,
            &crate::models::media_metadata::NewMediaMetadata {
                kind: crate::models::media_metadata::MediaKind::Movie,
                tmdb_id: Some(format!("mediafile-upsert-tmdb-{suffix}")),
                tvdb_id: None,
                imdb_id: None,
                provider_ids: serde_json::json!({}),
                title: format!("Media File Upsert Test {suffix}"),
                sort_title: None,
                original_title: None,
                original_language: None,
                status: None,
                overview: None,
                studio: None,
                network: None,
                runtime_minutes: None,
                year: None,
                images: serde_json::json!([]),
            },
        )
        .await
        .expect("seed media_metadata");

        let media_item = crate::repo::media_item::upsert(
            &pool,
            &crate::models::media_item::NewMediaItem {
                library_id: library.id,
                media_metadata_id: metadata.id,
                path: "Movie.mkv".to_string(),
                monitored: false,
                quality_profile_id: None,
                minimum_availability: None,
                plex_rating_key: None,
                added_at: None,
            },
        )
        .await
        .expect("seed media_item");

        let (first, first_changed) = upsert_scanned(&pool, media_item.id, "Movie.mkv", Some(1000), None)
            .await
            .expect("first upsert_scanned");
        assert!(first_changed, "the first scan of a file must always be recorded as a change");
        assert_eq!(first.size_bytes, Some(1000));

        let (second, second_changed) = upsert_scanned(&pool, media_item.id, "Movie.mkv", Some(1000), None)
            .await
            .expect("second upsert_scanned with an unchanged size");
        assert!(!second_changed, "an unchanged size must be a clean no-op, not a write");
        assert_eq!(second.id, first.id);

        let rows = list_by_media_item(&pool, media_item.id).await.expect("list_by_media_item");
        assert_eq!(rows.len(), 1, "no duplicate row from the two upserts above");

        let (third, third_changed) = upsert_scanned(
            &pool,
            media_item.id,
            "Movie.mkv",
            Some(2000),
            Some(serde_json::json!({"container": "mkv"})),
        )
        .await
        .expect("third upsert_scanned with a changed size");
        assert!(third_changed, "a changed size must be recorded as an update");
        assert_eq!(third.id, first.id, "a size change updates the existing row, never inserts a duplicate");
        assert_eq!(third.size_bytes, Some(2000));

        let rows_again = list_by_media_item(&pool, media_item.id).await.expect("list_by_media_item again");
        assert_eq!(rows_again.len(), 1, "a size-changed update must still be exactly one row");
    }
}
