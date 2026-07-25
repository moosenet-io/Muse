//! MUSE-06: one-time Tautulli watch-history backfill.
//!
//! [`run`] pages through `get_history`, maps each row onto `play_sessions`
//! (+ `play_session_media_info` when stream-data enrichment succeeds), and
//! is safe to run repeatedly:
//!
//! - **Idempotent re-run**: every imported row is looked up first by
//!   Tautulli's `reference_id` (`play_sessions.tautulli_ref_id`) —
//!   `repo::play_session::find_by_tautulli_ref`. If it's already there, the
//!   row is skipped. This covers both resolved and unresolved rows, unlike
//!   the table's own `(account_id, media_item_id, episode_id, started_at)`
//!   UNIQUE, which only dedups resolved rows (Postgres treats distinct NULLs
//!   as non-conflicting — see the caveat on `repo::play_session::upsert`).
//! - **Dedup vs. native capture (MUSE-07)**: before inserting, a row is
//!   checked against `repo::play_session::find_overlapping_native` — a
//!   natively-captured session (`tautulli_ref_id IS NULL`) for the same
//!   account/media/episode within `overlap_tolerance_secs` of `started_at`.
//!   If one exists, native wins: no new row is inserted, and the native row
//!   is stamped with `tautulli_ref_id` for provenance
//!   (`repo::play_session::attach_tautulli_ref`).
//! - **Resumable in the loose sense**: because both guards above key off
//!   data already in Postgres (not an external cursor), running the whole
//!   backfill again from `start=0` after an interruption safely re-derives
//!   the same end state rather than duplicating rows. No `MUSE_TEST_DATABASE_URL`
//!   migration/cursor table was added — the operator-facing entrypoint (a
//!   CLI subcommand or ops script) that would call [`run`] is intentionally
//!   left to the orchestrator (this crate has no `[[bin]]` beyond the axum
//!   service), so a persisted progress cursor isn't wired to anything yet
//!   and was left out per the task's "prefer none" guidance.

use chrono::{DateTime, Datelike, TimeZone, Timelike, Utc};
use ipnetwork::IpNetwork;
use sqlx::PgPool;
use std::collections::HashMap;
use std::str::FromStr;

use crate::error::MuseResult;
use crate::models::account::NewAccount;
use crate::models::media_metadata::MediaKind;
use crate::models::play_session::{NewPlaySession, NewPlaySessionMediaInfo};
use crate::repo;
use crate::repo::play_session::SetMediaRefOutcome;

use super::client::TautulliClient;
use super::models::{parse_decision_kind, HistoryRow, MetadataInfo};

/// `percent_complete >= COMPLETE_THRESHOLD` marks a session finished when
/// Tautulli's own `watched_status` doesn't already say so — spec §3.3/§4-D.
const COMPLETE_THRESHOLD: f32 = 0.90;
/// `percent_complete < ABANDON_THRESHOLD` (and not finished) marks a session
/// abandoned — spec §3.3/§4-D. Backfill has no "no later finish" signal the
/// way the live reconstruction worker does (MUSE-07), so this is a
/// best-effort approximation from percent alone; documented divergence.
const ABANDON_THRESHOLD: f32 = 0.15;

/// Default tolerance for matching a Tautulli history row against a
/// natively-captured session covering the same watch — spec §4-D
/// (`started_at±120s`).
pub const DEFAULT_OVERLAP_TOLERANCE_SECS: i64 = 120;

#[derive(Debug, Clone)]
pub struct BackfillOptions {
    pub page_size: i64,
    /// Fetch `get_metadata` per row to get the item's true media runtime
    /// (more accurate `duration_ms`/`percent_complete` than deriving from
    /// the history row's own play-duration alone). Best-effort: a failure
    /// degrades to the history-row-derived estimate rather than failing the
    /// row.
    pub enrich_metadata: bool,
    /// Fetch `get_stream_data` per row to populate `play_session_media_info`
    /// (quality/transcode detail). Best-effort, same degrade posture as
    /// `enrich_metadata`.
    pub enrich_stream_data: bool,
    pub overlap_tolerance_secs: i64,
}

impl Default for BackfillOptions {
    fn default() -> Self {
        Self {
            page_size: super::client::DEFAULT_PAGE_SIZE,
            enrich_metadata: true,
            enrich_stream_data: true,
            overlap_tolerance_secs: DEFAULT_OVERLAP_TOLERANCE_SECS,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BackfillSummary {
    pub pages_fetched: usize,
    pub rows_seen: usize,
    pub imported: usize,
    /// Already present from a prior run (`tautulli_ref_id` matched) — the
    /// idempotency guard.
    pub skipped_already_imported: usize,
    /// A natively-captured session already covered this watch; the native
    /// row was stamped with `tautulli_ref_id` instead of inserting a
    /// duplicate.
    pub skipped_native_overlap: usize,
    /// Row had no `reference_id` at all (Tautulli should always send one,
    /// but the parser is deliberately permissive) — skipped since it can't
    /// be deduped safely.
    pub skipped_missing_reference_id: usize,
    pub resolved_media: usize,
    pub unresolved_media: usize,
}

/// Run the full Tautulli backfill: page through `get_history` from the
/// beginning until Tautulli reports no more rows, importing (or deduping)
/// each one. Never aborts the whole run over one bad row — a single row's
/// enrichment/insert failure is logged and counted implicitly by not
/// incrementing `imported`, and the loop continues with the next row.
pub async fn run(
    pool: &PgPool,
    client: &TautulliClient,
    options: &BackfillOptions,
) -> MuseResult<BackfillSummary> {
    let mut summary = BackfillSummary::default();
    let mut start: i64 = 0;

    loop {
        let page = client.get_history(start, options.page_size).await?;
        summary.pages_fetched += 1;

        if page.rows.is_empty() {
            break;
        }

        for row in &page.rows {
            summary.rows_seen += 1;
            if let Err(e) = import_row(pool, client, row, options, &mut summary).await {
                tracing::warn!(
                    reference_id = ?row.reference_id,
                    error = %e,
                    "failed to import tautulli history row; continuing with the rest of the backfill"
                );
            }
        }

        start += page.rows.len() as i64;
        if start >= page.records_filtered {
            break;
        }
    }

    Ok(summary)
}

async fn import_row(
    pool: &PgPool,
    client: &TautulliClient,
    row: &HistoryRow,
    options: &BackfillOptions,
    summary: &mut BackfillSummary,
) -> MuseResult<()> {
    let Some(reference_id) = row.reference_id else {
        summary.skipped_missing_reference_id += 1;
        return Ok(());
    };

    if repo::play_session::find_by_tautulli_ref(pool, reference_id)
        .await?
        .is_some()
    {
        summary.skipped_already_imported += 1;
        return Ok(());
    }

    let account_id = resolve_account(pool, row).await?;

    let Some(started_at) = row.started.and_then(unix_seconds_to_utc) else {
        // Without a `started` timestamp there is no meaningful session to
        // record (the UNIQUE key and overlap dedup both hinge on it).
        return Ok(());
    };
    let stopped_at = row.stopped.and_then(unix_seconds_to_utc);

    // Fetch get_metadata once (best-effort). It supplies both the item's true
    // runtime AND — for BSEED-1 GUID resolution — the provider `guids` /
    // `grandparent_guid`. A failure degrades to no-metadata (history-row
    // duration + rating-key-only resolution) and never fails the row.
    let metadata = if options.enrich_metadata {
        match row.rating_key_str() {
            Some(rating_key) => match client.get_metadata(&rating_key).await {
                Ok(meta) => meta,
                Err(e) => {
                    tracing::debug!(rating_key, error = %e, "get_metadata enrichment failed; degrading to history-row data");
                    None
                }
            },
            None => None,
        }
    } else {
        None
    };

    let keys = keys_from_history(row, metadata.as_ref());
    let (media_item_id, episode_id) = resolve_media(pool, &keys).await?;

    if media_item_id.is_some() || episode_id.is_some() {
        summary.resolved_media += 1;
    } else {
        summary.unresolved_media += 1;
    }

    // Prefer the true media runtime from get_metadata over the history row's
    // own play-duration, which is watched time, not runtime.
    let duration_ms = metadata
        .as_ref()
        .and_then(|m| m.duration)
        .or_else(|| row.duration.map(|s| s * 1000));

    let watched_ms = row.duration.map(|s| s * 1000);
    let percent_complete = row.percent_complete.map(|p| (p / 100.0) as f32);
    let is_finished = row.watched_status.map(|w| w >= 1.0).unwrap_or(false)
        || percent_complete.map(|p| p >= COMPLETE_THRESHOLD).unwrap_or(false);
    let is_abandoned = !is_finished
        && percent_complete.map(|p| p < ABANDON_THRESHOLD).unwrap_or(false);

    let ip_address = row
        .ip_address
        .as_deref()
        .and_then(|ip| IpNetwork::from_str(ip).ok());

    // Dedup vs. native capture (MUSE-07): a native session covering the same
    // watch wins — attach provenance and skip inserting a duplicate.
    if let Some(native) = repo::play_session::find_overlapping_native(
        pool,
        account_id,
        media_item_id,
        episode_id,
        started_at,
        options.overlap_tolerance_secs,
    )
    .await?
    {
        repo::play_session::attach_tautulli_ref(pool, native.id, reference_id).await?;
        summary.skipped_native_overlap += 1;
        return Ok(());
    }

    let new_session = NewPlaySession {
        account_id,
        media_item_id,
        episode_id,
        session_key: row.session_key.clone(),
        tautulli_ref_id: Some(reference_id),
        started_at,
        stopped_at,
        duration_ms,
        watched_ms,
        // Tautulli's get_history rows don't carry a reliable final
        // view-offset distinct from watched duration in this shape; using
        // watched_ms is the best available approximation for backfilled
        // rows (documented divergence from the live poller, which reads
        // the real `viewOffset`).
        view_offset_ms: watched_ms,
        percent_complete,
        paused_counter: row.paused_counter.unwrap_or(0),
        // Tautulli's history export doesn't expose accumulated pause time,
        // only a pause *count* — left at 0 (unknown) rather than guessed.
        paused_ms: 0,
        is_finished,
        is_abandoned,
        player: row.player.clone(),
        platform: row.platform.clone(),
        product: row.product.clone(),
        device: None,
        ip_address,
        started_hour: Some(started_at.hour() as i32),
        started_dow: Some(started_at.weekday().num_days_from_sunday() as i32),
        // No reliable device-class signal in the history row shape to infer
        // cinema-vs-mobile context from; left unset rather than guessed.
        is_cinema_context: None,
    };

    let session = repo::play_session::upsert(pool, &new_session).await?;
    summary.imported += 1;

    // BSEED-2: persist the Plex identifying keys so a later re-resolution pass
    // can re-match this session offline (no Tautulli round-trip) once its
    // media_item exists. Best-effort — a stamp failure never fails the import.
    if let Err(e) = repo::play_session::set_plex_refs(
        pool,
        session.id,
        keys.rating_key.as_deref(),
        keys.grandparent_rating_key.as_deref(),
        &guids_to_json(&keys.guids),
        keys.grandparent_guid.as_deref(),
    )
    .await
    {
        tracing::debug!(session_id = session.id, error = %e, "failed to persist plex refs for re-resolution; continuing");
    }

    if options.enrich_stream_data {
        if let Some(row_id) = row.row_id {
            match client.get_stream_data(row_id).await {
                Ok(Some(stream)) => {
                    let media_info = NewPlaySessionMediaInfo {
                        video_decision: parse_decision_kind(stream.video_decision.as_deref()),
                        audio_decision: parse_decision_kind(stream.audio_decision.as_deref()),
                        transcode_decision: parse_decision_kind(stream.transcode_decision.as_deref()),
                        container: stream.container,
                        video_codec: stream.video_codec,
                        audio_codec: stream.audio_codec,
                        audio_channels: stream.audio_channels,
                        video_resolution: stream.video_resolution,
                        bitrate: stream.bitrate,
                        width: stream.width,
                        height: stream.height,
                        transcode_reason: stream.transcode_reason,
                    };
                    if let Err(e) = repo::play_session::upsert_media_info(pool, session.id, &media_info).await {
                        tracing::debug!(session_id = session.id, error = %e, "failed to persist stream-data enrichment");
                    }
                }
                Ok(None) => {}
                Err(e) => tracing::debug!(row_id, error = %e, "get_stream_data enrichment failed"),
            }
        }
    }

    Ok(())
}

/// Resolve (and lazily create) the `accounts` row for a history row's
/// Tautulli `user_id` — Tautulli's `user_id` is the underlying Plex account
/// ID, so this keys on the same `plex_account_id` MUSE-04/07 use. Returns
/// `None` (not an error) when the row carries no `user_id` at all.
async fn resolve_account(pool: &PgPool, row: &HistoryRow) -> MuseResult<Option<i64>> {
    let Some(user_id) = row.user_id else {
        return Ok(None);
    };

    let account = repo::account::upsert_by_plex_account_id(
        pool,
        &NewAccount {
            plex_account_id: Some(user_id.to_string()),
            username: row.user.clone(),
            friendly_name: row.friendly_name.clone(),
            is_home_user: true,
            is_primary: false,
        },
    )
    .await?;

    Ok(Some(account.id))
}

/// The identifying keys a session can be resolved from — either derived from a
/// live Tautulli history row + its `get_metadata` ([`keys_from_history`]), or
/// rehydrated from a session's stored `plex_*` columns for offline
/// re-resolution ([`keys_from_unresolved`]).
#[derive(Debug, Clone, Default)]
pub(crate) struct ResolveKeys {
    pub is_episode: bool,
    /// The item's own Plex ratingKey (movie or episode).
    pub rating_key: Option<String>,
    /// The owning show's Plex ratingKey (episodes only).
    pub grandparent_rating_key: Option<String>,
    /// The item's own provider GUIDs (`imdb://`/`tmdb://`/`tvdb://`).
    pub guids: Vec<String>,
    /// The owning show's provider GUID (episodes only).
    pub grandparent_guid: Option<String>,
    /// Title + year for the movie-only title/year resolution fallback.
    pub title: Option<String>,
    pub year: Option<i32>,
}

/// Build [`ResolveKeys`] from a live Tautulli history row plus its
/// (best-effort) `get_metadata`. Guids/grandparent_guid come from
/// `get_metadata` (the history row itself doesn't carry them); year prefers
/// the history row's own value, falling back to metadata.
fn keys_from_history(row: &HistoryRow, metadata: Option<&MetadataInfo>) -> ResolveKeys {
    ResolveKeys {
        is_episode: row.media_type.as_deref() == Some("episode"),
        rating_key: row.rating_key_str(),
        grandparent_rating_key: row.grandparent_rating_key_str(),
        guids: metadata.map(|m| m.guids()).unwrap_or_default(),
        grandparent_guid: metadata.and_then(|m| m.grandparent_guid.clone()),
        title: row.full_title.clone().or_else(|| row.title.clone()),
        year: row
            .year
            .map(|y| y as i32)
            .or_else(|| metadata.and_then(|m| m.year).map(|y| y as i32)),
    }
}

/// Rehydrate [`ResolveKeys`] from a session's stored `plex_*` columns for
/// offline re-resolution (BSEED-2). Episode-ness is inferred from the presence
/// of a stored show-level key, since the session row doesn't persist
/// `media_type` directly.
fn keys_from_unresolved(row: &repo::play_session::UnresolvedSession) -> ResolveKeys {
    let guids = row
        .plex_guids
        .as_array()
        .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
        .unwrap_or_default();
    ResolveKeys {
        is_episode: row.plex_grandparent_guid.is_some() || row.plex_grandparent_rating_key.is_some(),
        rating_key: row.plex_rating_key.clone(),
        grandparent_rating_key: row.plex_grandparent_rating_key.clone(),
        guids,
        grandparent_guid: row.plex_grandparent_guid.clone(),
        // Title/year aren't persisted on the session — offline re-resolution
        // relies on the (more reliable) rating-key + GUID paths.
        title: None,
        year: None,
    }
}

/// A JSON array of the session's provider-GUID strings, for persistence via
/// `repo::play_session::set_plex_refs`.
fn guids_to_json(guids: &[String]) -> serde_json::Value {
    serde_json::Value::Array(guids.iter().cloned().map(serde_json::Value::String).collect())
}

/// Which provider a parsed Plex GUID names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GuidProvider {
    Tmdb,
    Tvdb,
    Imdb,
}

/// Parse a Plex/Tautulli provider GUID into `(provider, id)`. Handles both the
/// modern short scheme (`tmdb://335984`, `tvdb://121361`, `imdb://tt1856101`)
/// and the legacy Plex-agent form (`com.plexapp.agents.imdb://tt…?lang=en`,
/// `com.plexapp.agents.themoviedb://…`, `com.plexapp.agents.thetvdb://…`). Any
/// query string / trailing path is stripped. Returns `None` for a
/// non-provider GUID (`plex://…`, `local://…`) or an empty id — those simply
/// don't participate in matching.
fn parse_provider_guid(raw: &str) -> Option<(GuidProvider, String)> {
    let (scheme, rest) = raw.trim().split_once("://")?;
    let scheme = scheme
        .trim_start_matches("com.plexapp.agents.")
        .to_ascii_lowercase();
    let id = rest.split(['?', '/']).next().unwrap_or(rest).trim();
    if id.is_empty() {
        return None;
    }
    let provider = match scheme.as_str() {
        "tmdb" | "themoviedb" | "moviedb" => GuidProvider::Tmdb,
        "tvdb" | "thetvdb" => GuidProvider::Tvdb,
        "imdb" => GuidProvider::Imdb,
        _ => return None,
    };
    Some((provider, id.to_string()))
}

/// Match `guids` against a cataloged `media_metadata` row of `kind` (via
/// `find_by_{tmdb,tvdb,imdb}_id`), then resolve that to a concrete
/// `media_items.id`. Returns `Ok(None)` when none match — a normal "not in the
/// catalog yet" case.
///
/// **Provider precedence is enforced explicitly (tmdb → tvdb → imdb),
/// independent of the order the GUIDs happened to arrive in** (FIX A): `tmdb_id`
/// and `tvdb_id` are the unique catalog keys, but `imdb_id` is NOT unique in
/// `media_metadata`, so trying imdb first (just because it appeared first in the
/// array) could cross-link the session to a duplicate/inconsistent IMDb record
/// when a correct tmdb/tvdb match existed. All GUIDs are parsed up front, then
/// tried tmdb-first; imdb is the last-resort key.
async fn match_guid_to_media_item(
    pool: &PgPool,
    guids: &[String],
    kind: MediaKind,
) -> MuseResult<Option<i64>> {
    let mut tmdb_ids = Vec::new();
    let mut tvdb_ids = Vec::new();
    let mut imdb_ids = Vec::new();
    for guid in guids {
        if let Some((provider, id)) = parse_provider_guid(guid) {
            match provider {
                GuidProvider::Tmdb => tmdb_ids.push(id),
                GuidProvider::Tvdb => tvdb_ids.push(id),
                GuidProvider::Imdb => imdb_ids.push(id),
            }
        }
    }

    // Try each provider in strict priority order; within a provider, the first
    // id that resolves to a catalog row + media_item wins.
    for id in &tmdb_ids {
        if let Some(mm_id) = repo::media_metadata::find_by_tmdb_id(pool, kind, id).await? {
            if let Some(item_id) = repo::media_item::find_by_media_metadata_id(pool, mm_id).await? {
                return Ok(Some(item_id));
            }
        }
    }
    for id in &tvdb_ids {
        if let Some(mm_id) = repo::media_metadata::find_by_tvdb_id(pool, kind, id).await? {
            if let Some(item_id) = repo::media_item::find_by_media_metadata_id(pool, mm_id).await? {
                return Ok(Some(item_id));
            }
        }
    }
    for id in &imdb_ids {
        if let Some(mm_id) = repo::media_metadata::find_by_imdb_id(pool, kind, id).await? {
            if let Some(item_id) = repo::media_item::find_by_media_metadata_id(pool, mm_id).await? {
                return Ok(Some(item_id));
            }
        }
    }

    Ok(None)
}

/// Resolve a session's [`ResolveKeys`] onto a library `media_item` and/or
/// `episode`, per spec §4-D and BSEED-1. Match order (first hit wins):
///
/// - **Episode:** episode `plex_rating_key` → show `grandparent_rating_key`
///   → show `grandparent_guid` (tmdb/tvdb/imdb → `media_metadata` show →
///   `media_item`). The episode's own GUIDs identify the *episode*, not a show
///   `media_metadata` row, so they're intentionally not used for show matching.
/// - **Movie:** `plex_rating_key` → the item's GUIDs (tmdb/tvdb/imdb →
///   `media_metadata` movie → `media_item`) → exact title+year (real year
///   only, mirroring the scanner's own guard).
///
/// The `plex_rating_key` path stays the first-choice match (unchanged behavior
/// from before BSEED-1); GUID/title matching only augments it, which is what
/// lets arr-ingested items (carrying tmdb/tvdb/imdb ids but `plex_rating_key =
/// NULL`) resolve with no Plex library sync. Nothing matched → `(None, None)`,
/// the session stays unresolved (never an error).
pub(crate) async fn resolve_media(
    pool: &PgPool,
    keys: &ResolveKeys,
) -> MuseResult<(Option<i64>, Option<i64>)> {
    if keys.is_episode {
        // 1. Exact episode by its own ratingKey.
        if let Some(rating_key) = &keys.rating_key {
            if let Some(episode) = repo::episode::find_by_plex_rating_key(pool, rating_key).await? {
                return Ok((Some(episode.media_item_id), Some(episode.id)));
            }
        }
        // 2. Owning show by its ratingKey.
        if let Some(show_rating_key) = &keys.grandparent_rating_key {
            if let Some(show) = repo::media_item::find_by_plex_rating_key(pool, show_rating_key).await? {
                return Ok((Some(show.id), None));
            }
        }
        // 3. Owning show by its provider GUID (arr-ingested shows carry
        //    tvdb/tmdb/imdb ids). Attributes the session to the show-level
        //    media_item even when the specific episode isn't cataloged.
        if let Some(grandparent_guid) = &keys.grandparent_guid {
            let show_guids = [grandparent_guid.clone()];
            if let Some(item_id) = match_guid_to_media_item(pool, &show_guids, MediaKind::Show).await? {
                return Ok((Some(item_id), None));
            }
        }
        return Ok((None, None));
    }

    // Movie (or any non-episode media type).
    // 1. Exact movie by ratingKey.
    if let Some(rating_key) = &keys.rating_key {
        if let Some(item) = repo::media_item::find_by_plex_rating_key(pool, rating_key).await? {
            return Ok((Some(item.id), None));
        }
    }
    // 2. Movie by provider GUID (the arr-ingest path).
    if let Some(item_id) = match_guid_to_media_item(pool, &keys.guids, MediaKind::Movie).await? {
        return Ok((Some(item_id), None));
    }
    // 3. Exact title + year (real year only — never a yearless title match,
    //    mirroring the library scanner's own guard).
    if let (Some(title), Some(year)) = (&keys.title, keys.year) {
        if let Some(mm_id) =
            repo::media_metadata::find_by_title_year(pool, MediaKind::Movie, title, Some(year)).await?
        {
            if let Some(item_id) = repo::media_item::find_by_media_metadata_id(pool, mm_id).await? {
                return Ok((Some(item_id), None));
            }
        }
    }

    Ok((None, None))
}

/// Summary of one [`resolve_existing_unresolved`] pass.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ResolveSummary {
    /// Unresolved sessions examined this pass.
    pub sessions_considered: usize,
    /// Sessions newly resolved (a media/episode ref attached in place).
    pub resolved: usize,
    /// Sessions whose newly-resolved key collided with an already-resolved
    /// session and were deduped away (the redundant duplicate deleted).
    pub deduped_conflicts: usize,
    /// Sessions still unresolvable this pass (no matching catalog row / no
    /// usable keys).
    pub still_unresolved: usize,
    /// Whether a Tautulli client was available to rehydrate keys for sessions
    /// that had none stored (the pre-migration backfill).
    pub tautulli_used: bool,
}

/// BSEED-2: re-resolve already-imported sessions that never matched a library
/// item (`media_item_id IS NULL AND episode_id IS NULL`) against the
/// now-populated catalog — the door that turns the pre-existing imported
/// Tautulli history into taste input once arr ingest has run. Idempotent and
/// safe to re-run: a session that still doesn't match is simply left
/// unresolved.
///
/// Two key sources, in order of preference per session:
/// 1. **Stored keys** (`plex_*` columns) — sessions imported after migration
///    0108 carry their identifying keys and re-resolve fully **offline** (no
///    Tautulli round-trip). This is the path the maintenance pass uses.
/// 2. **Tautulli rehydration** — sessions imported *before* 0108 have no
///    stored keys; when a `tautulli` client is supplied, this re-pages
///    `get_history` (once) to recover each session's ratingKeys, fetches
///    `get_metadata` for its GUIDs, resolves, and — on success — stamps the
///    keys back so subsequent passes can resolve it offline. This is the path
///    `POST /ops/library/resolve` uses to unblock the pre-existing 1544.
///
/// Never returns `Err` for a single bad session — each is error-isolated and
/// logged, matching the crate's graceful-degrade posture.
/// Hard cap on how many Tautulli history records the one-time keyless
/// rehydration paging (`build_ref_map`) will read, so a pathologically large
/// history can't make `POST /ops/library/resolve` unbounded. The pre-existing
/// backfill is ~1.5k rows; this is generous headroom while still bounded.
const MAX_REHYDRATION_RECORDS: i64 = 250_000;

/// Whether an unresolved session carries enough stored Plex identity to be
/// re-resolved **offline** (no Tautulli round-trip). True when ANY stored key
/// is present — crucially including the show-level `grandparent` fields, which
/// alone are enough to resolve an episode's owning show (FIX 3: an
/// episode row that persisted only a `grandparent_guid` must not be skipped
/// forever on the offline path).
fn has_stored_keys(session: &repo::play_session::UnresolvedSession) -> bool {
    session.plex_rating_key.is_some()
        || session.plex_grandparent_rating_key.is_some()
        || session.plex_grandparent_guid.is_some()
        || session
            .plex_guids
            .as_array()
            .map(|a| !a.is_empty())
            .unwrap_or(false)
}

pub async fn resolve_existing_unresolved(
    pool: &PgPool,
    tautulli: Option<&TautulliClient>,
    options: &BackfillOptions,
    limit: i64,
) -> MuseResult<ResolveSummary> {
    let mut summary = ResolveSummary {
        tautulli_used: tautulli.is_some(),
        ..Default::default()
    };

    let unresolved = repo::play_session::list_unresolved(pool, limit).await?;

    // Build the Tautulli `reference_id -> HistoryRow` rehydration map AT MOST
    // ONCE for the whole batch (FIX 1), and only when it's actually needed: a
    // client is available AND at least one row lacks stored keys. Paging is
    // bounded (`MAX_REHYDRATION_RECORDS`). Fail-open: if paging fails, skip
    // keyless rehydration this pass (degrade to offline-only) rather than
    // erroring or — the original bug — re-paging the entire history per row.
    let needs_rehydration = unresolved.iter().any(|s| !has_stored_keys(s));
    let ref_map: Option<HashMap<i64, HistoryRow>> = match tautulli {
        Some(client) if needs_rehydration => match build_ref_map(client, options).await {
            Ok(map) => Some(map),
            Err(e) => {
                tracing::warn!(error = %e, "BSEED-2: could not page Tautulli history for keyless rehydration; skipping it this pass (offline-only)");
                None
            }
        },
        _ => None,
    };

    for session in &unresolved {
        summary.sessions_considered += 1;
        match resolve_one_unresolved(pool, tautulli, session, ref_map.as_ref()).await {
            Ok(Some(SetMediaRefOutcome::Updated)) => summary.resolved += 1,
            Ok(Some(SetMediaRefOutcome::ConflictDeduped)) => summary.deduped_conflicts += 1,
            Ok(None) => summary.still_unresolved += 1,
            Err(e) => {
                summary.still_unresolved += 1;
                tracing::warn!(session_id = session.id, error = %e, "re-resolution failed for this session; continuing");
            }
        }
    }

    tracing::info!(
        sessions_considered = summary.sessions_considered,
        resolved = summary.resolved,
        deduped_conflicts = summary.deduped_conflicts,
        still_unresolved = summary.still_unresolved,
        tautulli_used = summary.tautulli_used,
        "BSEED-2: re-resolution pass complete"
    );

    Ok(summary)
}

/// Resolve a single unresolved session; returns the `set_media_ref` outcome on
/// a hit, or `None` when it still can't be matched. Isolated so
/// [`resolve_existing_unresolved`]'s loop can log-and-continue per session.
/// `ref_map` is the already-built (at-most-once) rehydration map — this
/// function never pages Tautulli itself, only reads the shared map.
async fn resolve_one_unresolved(
    pool: &PgPool,
    tautulli: Option<&TautulliClient>,
    session: &repo::play_session::UnresolvedSession,
    ref_map: Option<&HashMap<i64, HistoryRow>>,
) -> MuseResult<Option<SetMediaRefOutcome>> {
    // Prefer stored keys (offline). Rehydrate from the pre-built ref-map only
    // when the session has none stored and a client + map are available.
    let (keys, stamp) = if has_stored_keys(session) {
        (keys_from_unresolved(session), false)
    } else if let (Some(client), Some(ref_id), Some(map)) =
        (tautulli, session.tautulli_ref_id, ref_map)
    {
        let Some(history_row) = map.get(&ref_id) else {
            return Ok(None);
        };
        // Fetch GUIDs from get_metadata (best-effort) so arr-ingested items
        // can match; degrade to ratingKey-only on failure.
        let metadata = match history_row.rating_key_str() {
            Some(rating_key) => client.get_metadata(&rating_key).await.unwrap_or(None),
            None => None,
        };
        (keys_from_history(history_row, metadata.as_ref()), true)
    } else {
        return Ok(None);
    };

    let (media_item_id, episode_id) = resolve_media(pool, &keys).await?;
    if media_item_id.is_none() && episode_id.is_none() {
        return Ok(None);
    }

    // Persist the keys for future offline re-resolution (only needed when they
    // were rehydrated from Tautulli rather than already stored).
    if stamp {
        if let Err(e) = repo::play_session::set_plex_refs(
            pool,
            session.id,
            keys.rating_key.as_deref(),
            keys.grandparent_rating_key.as_deref(),
            &guids_to_json(&keys.guids),
            keys.grandparent_guid.as_deref(),
        )
        .await
        {
            tracing::debug!(session_id = session.id, error = %e, "failed to stamp rehydrated plex refs; continuing");
        }
    }

    let outcome = repo::play_session::set_media_ref(pool, session.id, media_item_id, episode_id).await?;
    Ok(Some(outcome))
}

/// Page Tautulli's `get_history` into a `reference_id -> HistoryRow` map ONCE,
/// so pre-migration sessions (which stored no keys) can have their ratingKeys
/// recovered for re-resolution. Bounded by [`MAX_REHYDRATION_RECORDS`] — a
/// history larger than that is truncated (logged) rather than paged without
/// limit, keeping the on-demand endpoint's cost bounded.
async fn build_ref_map(
    client: &TautulliClient,
    options: &BackfillOptions,
) -> MuseResult<HashMap<i64, HistoryRow>> {
    let mut map = HashMap::new();
    let mut start: i64 = 0;
    loop {
        if start >= MAX_REHYDRATION_RECORDS {
            tracing::warn!(
                records_read = start,
                cap = MAX_REHYDRATION_RECORDS,
                "BSEED-2: Tautulli history exceeded the rehydration cap; truncating the ref-map"
            );
            break;
        }
        let page = client.get_history(start, options.page_size).await?;
        if page.rows.is_empty() {
            break;
        }
        for row in &page.rows {
            if let Some(ref_id) = row.reference_id {
                map.insert(ref_id, row.clone());
            }
        }
        start += page.rows.len() as i64;
        if start >= page.records_filtered {
            break;
        }
    }
    Ok(map)
}

fn unix_seconds_to_utc(secs: i64) -> Option<DateTime<Utc>> {
    Utc.timestamp_opt(secs, 0).single()
}

#[cfg(test)]
mod tests {
    use super::*;
    use httpmock::prelude::*;
    use sqlx::postgres::PgPoolOptions;
    use uuid::Uuid;

    use crate::models::library::{LibraryKind, NewLibrary};
    use crate::models::media_item::NewMediaItem;
    use crate::models::media_metadata::{MediaKind, NewMediaMetadata};

    fn history_page_body(rows_json: &str) -> String {
        format!(
            r#"{{"response": {{"result": "success", "data": {{"recordsFiltered": {count}, "recordsTotal": {count}, "data": [{rows_json}]}}}}}}"#,
            count = if rows_json.trim().is_empty() { 0 } else { 1 },
        )
    }

    /// Pure mapping/parsing check: no network, no DB — constructs a
    /// `HistoryRow` directly (as `get_history` parsing would produce) and
    /// asserts the finished/abandoned/percent mapping spec §4-D describes.
    #[test]
    fn watched_status_and_percent_map_to_finished_and_abandoned() {
        let finished_row = HistoryRow {
            reference_id: Some(1),
            watched_status: Some(1.0),
            percent_complete: Some(96.0),
            ..Default::default()
        };
        let percent = finished_row.percent_complete.map(|p| (p / 100.0) as f32);
        assert!(percent.unwrap() >= COMPLETE_THRESHOLD);

        let abandoned_row = HistoryRow {
            reference_id: Some(2),
            watched_status: Some(0.0),
            percent_complete: Some(8.0),
            ..Default::default()
        };
        let percent = abandoned_row.percent_complete.map(|p| (p / 100.0) as f32);
        assert!(percent.unwrap() < ABANDON_THRESHOLD);
    }

    #[test]
    fn unix_seconds_to_utc_round_trips() {
        let dt = unix_seconds_to_utc(1_700_000_000).expect("valid unix timestamp should parse");
        assert_eq!(dt.timestamp(), 1_700_000_000);
    }

    /// BSEED-1: the GUID parser must handle both the modern short scheme and
    /// the legacy Plex-agent form, strip query strings, and reject
    /// non-provider GUIDs.
    #[test]
    fn parse_provider_guid_handles_all_forms() {
        assert_eq!(
            parse_provider_guid("tmdb://335984"),
            Some((GuidProvider::Tmdb, "335984".to_string()))
        );
        assert_eq!(
            parse_provider_guid("tvdb://121361"),
            Some((GuidProvider::Tvdb, "121361".to_string()))
        );
        assert_eq!(
            parse_provider_guid("imdb://tt1856101"),
            Some((GuidProvider::Imdb, "tt1856101".to_string()))
        );
        // Legacy Plex-agent forms + query string stripping.
        assert_eq!(
            parse_provider_guid("com.plexapp.agents.imdb://tt1856101?lang=en"),
            Some((GuidProvider::Imdb, "tt1856101".to_string()))
        );
        assert_eq!(
            parse_provider_guid("com.plexapp.agents.themoviedb://335984?lang=en"),
            Some((GuidProvider::Tmdb, "335984".to_string()))
        );
        assert_eq!(
            parse_provider_guid("com.plexapp.agents.thetvdb://121361/2/3?lang=en"),
            Some((GuidProvider::Tvdb, "121361".to_string()))
        );
        // Non-provider GUIDs / empties are ignored.
        assert_eq!(parse_provider_guid("plex://movie/5d776b59ad5437001f79c6f8"), None);
        assert_eq!(parse_provider_guid("local://12345"), None);
        assert_eq!(parse_provider_guid("tmdb://"), None);
        assert_eq!(parse_provider_guid("not-a-guid"), None);
    }

    /// `keys_from_history` pulls guids/grandparent_guid from `get_metadata`
    /// (the history row alone doesn't carry them) and prefers the history
    /// row's own year.
    #[test]
    fn keys_from_history_merges_row_and_metadata() {
        use crate::tautulli::MetadataInfo;

        let row = HistoryRow {
            media_type: Some("episode".to_string()),
            rating_key: Some(990001),
            grandparent_rating_key: Some(500),
            title: Some("The Long Night".to_string()),
            year: Some(2019),
            ..Default::default()
        };
        let meta: MetadataInfo = serde_json::from_str(
            r#"{"guids": ["tvdb://7366144"], "grandparent_guid": "tvdb://121361", "year": 1999}"#,
        )
        .unwrap();

        let keys = keys_from_history(&row, Some(&meta));
        assert!(keys.is_episode);
        assert_eq!(keys.rating_key.as_deref(), Some("990001"));
        assert_eq!(keys.grandparent_rating_key.as_deref(), Some("500"));
        assert_eq!(keys.guids, vec!["tvdb://7366144".to_string()]);
        assert_eq!(keys.grandparent_guid.as_deref(), Some("tvdb://121361"));
        assert_eq!(keys.year, Some(2019), "history-row year wins over metadata year");
    }

    /// FIX 3: an episode that persisted ONLY its owning show's `grandparent_guid`
    /// (no rating keys, no item guids) still has enough to resolve offline —
    /// `has_stored_keys` must return `true`, and `keys_from_unresolved` must
    /// surface the grandparent guid as an episode key. A session with nothing
    /// stored is (correctly) not offline-resolvable.
    #[test]
    fn has_stored_keys_true_for_grandparent_guid_only_episode() {
        use crate::repo::play_session::UnresolvedSession;

        let grandparent_only = UnresolvedSession {
            id: 1,
            account_id: Some(5),
            tautulli_ref_id: Some(99),
            plex_rating_key: None,
            plex_grandparent_rating_key: None,
            plex_grandparent_guid: Some("tvdb://121361".to_string()),
            plex_guids: serde_json::json!([]),
        };
        assert!(
            has_stored_keys(&grandparent_only),
            "a grandparent-guid-only episode must be treated as offline-resolvable"
        );
        let keys = keys_from_unresolved(&grandparent_only);
        assert!(keys.is_episode, "grandparent presence implies an episode");
        assert_eq!(keys.grandparent_guid.as_deref(), Some("tvdb://121361"));

        // Also true when only the grandparent RATING KEY is stored.
        let grandparent_rk_only = UnresolvedSession {
            plex_grandparent_guid: None,
            plex_grandparent_rating_key: Some("500".to_string()),
            ..grandparent_only.clone()
        };
        assert!(has_stored_keys(&grandparent_rk_only));

        // Nothing stored -> not offline-resolvable.
        let empty = UnresolvedSession {
            plex_grandparent_guid: None,
            plex_grandparent_rating_key: None,
            ..grandparent_only.clone()
        };
        assert!(!has_stored_keys(&empty));
    }

    /// Mocked-Tautulli, live-DB round trip: `run()` against an httpmock
    /// Tautulli server, asserting the imported `play_sessions` row's mapped
    /// fields, then re-running the exact same mocked history to assert
    /// idempotency (no new row, no duplicate).
    ///
    /// Gated on `MUSE_TEST_DATABASE_URL` — skips cleanly (does not fail)
    /// when unset, same pattern as `src/integration_tests.rs`.
    #[tokio::test]
    async fn backfill_imports_and_is_idempotent_on_rerun() {
        let Ok(database_url) = std::env::var("MUSE_TEST_DATABASE_URL") else {
            eprintln!(
                "MUSE_TEST_DATABASE_URL not set — skipping backfill_imports_and_is_idempotent_on_rerun \
                 (this is expected in the default test run; the crate does not require a live DB)"
            );
            return;
        };

        let pool = PgPoolOptions::new()
            .max_connections(5)
            .connect(&database_url)
            .await
            .expect("connect to MUSE_TEST_DATABASE_URL");

        sqlx::migrate!("./migrations")
            .run(&pool)
            .await
            .expect("migrations should apply cleanly");

        let suffix = Uuid::new_v4().simple().to_string();
        let rating_key: i64 = (Uuid::new_v4().as_u128() % 1_000_000) as i64;
        let user_id: i64 = (Uuid::new_v4().as_u128() % 1_000_000) as i64;

        // A movie in the library for the history row to resolve onto.
        let library = repo::library::create(
            &pool,
            &NewLibrary {
                name: format!("radarr_muse06_{suffix}"),
                kind: LibraryKind::Movie,
                root_folder: "/media/Movies/".to_string(),
                source_arr_name: Some("radarr".to_string()),
                source_arr_url: None,
            },
        )
        .await
        .expect("create library");

        let metadata = repo::media_metadata::upsert_by_tmdb(
            &pool,
            &NewMediaMetadata {
                kind: MediaKind::Movie,
                tmdb_id: Some(format!("tmdb-muse06-{suffix}")),
                tvdb_id: None,
                imdb_id: None,
                provider_ids: serde_json::json!({}),
                title: "Backfill Test Movie".to_string(),
                sort_title: None,
                original_title: None,
                original_language: None,
                status: None,
                overview: None,
                studio: None,
                network: None,
                runtime_minutes: Some(105),
                year: Some(2019),
                images: serde_json::json!([]),
            },
        )
        .await
        .expect("upsert media_metadata");

        let item = repo::media_item::upsert(
            &pool,
            &NewMediaItem {
                library_id: library.id,
                media_metadata_id: metadata.id,
                path: "/media/Movies/Backfill Test Movie (2019)".to_string(),
                monitored: true,
                quality_profile_id: None,
                minimum_availability: None,
                plex_rating_key: Some(rating_key.to_string()),
                added_at: None,
            },
        )
        .await
        .expect("upsert media_item");

        let reference_id: i64 = (Uuid::new_v4().as_u128() % 1_000_000) as i64;
        let row_json = format!(
            r#"{{
                "reference_id": {reference_id},
                "row_id": {reference_id},
                "started": 1700000000,
                "stopped": 1700006300,
                "duration": 6300,
                "paused_counter": 2,
                "user_id": {user_id},
                "user": "backfill_test_user_{suffix}",
                "friendly_name": "Backfill Test User",
                "player": "Living Room",
                "platform": "Roku",
                "product": "Plex for Roku",
                "ip_address": "192.0.2.7",
                "rating_key": {rating_key},
                "full_title": "Backfill Test Movie",
                "media_type": "movie",
                "percent_complete": 97,
                "watched_status": 1
            }}"#
        );

        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/api/v2").query_param("cmd", "get_history");
            then.status(200)
                .header("content-type", "application/json")
                .body(history_page_body(&row_json));
        });
        server.mock(|when, then| {
            when.method(GET).path("/api/v2").query_param("cmd", "get_metadata");
            then.status(200)
                .header("content-type", "application/json")
                .body(r#"{"response": {"result": "success", "data": {"media_type": "movie", "duration": 6300000}}}"#);
        });
        server.mock(|when, then| {
            when.method(GET).path("/api/v2").query_param("cmd", "get_stream_data");
            then.status(200)
                .header("content-type", "application/json")
                .body(
                    r#"{"response": {"result": "success", "data": {
                        "video_decision": "direct play", "audio_decision": "direct play",
                        "transcode_decision": "direct play", "container": "mkv"
                    }}}"#,
                );
        });

        let client = TautulliClient::new(server.base_url(), "test-key").expect("client should construct");
        let options = BackfillOptions {
            page_size: 250,
            ..Default::default()
        };

        let summary = run(&pool, &client, &options).await.expect("backfill run should succeed");
        assert_eq!(summary.imported, 1);
        assert_eq!(summary.resolved_media, 1);
        assert_eq!(summary.skipped_already_imported, 0);

        let imported = repo::play_session::find_by_tautulli_ref(&pool, reference_id)
            .await
            .expect("find by tautulli ref")
            .expect("session should have been imported");
        assert_eq!(imported.media_item_id, Some(item.id));
        assert!(imported.is_finished);
        assert!(!imported.is_abandoned);
        assert!((imported.percent_complete.unwrap() - 0.97).abs() < 0.001);
        assert_eq!(imported.paused_counter, 2);

        let media_info = repo::play_session::get_media_info(&pool, imported.id)
            .await
            .expect("get media info")
            .expect("media info should have been enriched");
        assert_eq!(media_info.container.as_deref(), Some("mkv"));

        // Re-running the exact same history must import nothing new — the
        // tautulli_ref_id idempotency guard.
        let second_summary = run(&pool, &client, &options).await.expect("second backfill run should succeed");
        assert_eq!(second_summary.imported, 0);
        assert_eq!(second_summary.skipped_already_imported, 1);

        let sessions_for_item = repo::play_session::list_for_media_item(&pool, item.id)
            .await
            .expect("list sessions for media item");
        assert_eq!(sessions_for_item.len(), 1, "re-running the backfill must not duplicate the session");
    }

    /// Dedup-vs-native: a natively-captured session (no `tautulli_ref_id`,
    /// as MUSE-07 would write) already covers the same watch — the backfill
    /// must not insert a second row, only stamp provenance onto the native
    /// one.
    #[tokio::test]
    async fn backfill_prefers_native_session_over_duplicate_insert() {
        let Ok(database_url) = std::env::var("MUSE_TEST_DATABASE_URL") else {
            eprintln!(
                "MUSE_TEST_DATABASE_URL not set — skipping backfill_prefers_native_session_over_duplicate_insert \
                 (this is expected in the default test run; the crate does not require a live DB)"
            );
            return;
        };

        let pool = PgPoolOptions::new()
            .max_connections(5)
            .connect(&database_url)
            .await
            .expect("connect to MUSE_TEST_DATABASE_URL");

        sqlx::migrate!("./migrations")
            .run(&pool)
            .await
            .expect("migrations should apply cleanly");

        let suffix = Uuid::new_v4().simple().to_string();
        let rating_key: i64 = (Uuid::new_v4().as_u128() % 1_000_000) as i64;
        let user_id: i64 = (Uuid::new_v4().as_u128() % 1_000_000) as i64;

        let library = repo::library::create(
            &pool,
            &NewLibrary {
                name: format!("radarr_muse06b_{suffix}"),
                kind: LibraryKind::Movie,
                root_folder: "/media/Movies/".to_string(),
                source_arr_name: Some("radarr".to_string()),
                source_arr_url: None,
            },
        )
        .await
        .expect("create library");

        let metadata = repo::media_metadata::upsert_by_tmdb(
            &pool,
            &NewMediaMetadata {
                kind: MediaKind::Movie,
                tmdb_id: Some(format!("tmdb-muse06b-{suffix}")),
                tvdb_id: None,
                imdb_id: None,
                provider_ids: serde_json::json!({}),
                title: "Native Overlap Test Movie".to_string(),
                sort_title: None,
                original_title: None,
                original_language: None,
                status: None,
                overview: None,
                studio: None,
                network: None,
                runtime_minutes: Some(100),
                year: Some(2018),
                images: serde_json::json!([]),
            },
        )
        .await
        .expect("upsert media_metadata");

        let item = repo::media_item::upsert(
            &pool,
            &NewMediaItem {
                library_id: library.id,
                media_metadata_id: metadata.id,
                path: "/media/Movies/Native Overlap Test Movie (2018)".to_string(),
                monitored: true,
                quality_profile_id: None,
                minimum_availability: None,
                plex_rating_key: Some(rating_key.to_string()),
                added_at: None,
            },
        )
        .await
        .expect("upsert media_item");

        let account = repo::account::upsert_by_plex_account_id(
            &pool,
            &NewAccount {
                plex_account_id: Some(user_id.to_string()),
                username: Some(format!("native_user_{suffix}")),
                friendly_name: None,
                is_home_user: true,
                is_primary: false,
            },
        )
        .await
        .expect("upsert account");

        let started_at = unix_seconds_to_utc(1_700_100_000).expect("valid timestamp");

        // Simulate a session MUSE-07 already captured natively (no
        // tautulli_ref_id) for the exact same account/media/time.
        let native_session = repo::play_session::upsert(
            &pool,
            &NewPlaySession {
                account_id: Some(account.id),
                media_item_id: Some(item.id),
                episode_id: None,
                session_key: Some(format!("native-session-{suffix}")),
                tautulli_ref_id: None,
                started_at,
                stopped_at: Some(started_at + chrono::Duration::minutes(95)),
                duration_ms: Some(100 * 60 * 1000),
                watched_ms: Some(95 * 60 * 1000),
                view_offset_ms: Some(95 * 60 * 1000),
                percent_complete: Some(0.95),
                paused_counter: 0,
                paused_ms: 0,
                is_finished: true,
                is_abandoned: false,
                player: Some("Native Capture".to_string()),
                platform: None,
                product: None,
                device: None,
                ip_address: None,
                started_hour: Some(started_at.hour() as i32),
                started_dow: Some(started_at.weekday().num_days_from_sunday() as i32),
                is_cinema_context: None,
            },
        )
        .await
        .expect("upsert native play_session");

        let reference_id: i64 = (Uuid::new_v4().as_u128() % 1_000_000) as i64;
        let row_json = format!(
            r#"{{
                "reference_id": {reference_id},
                "row_id": {reference_id},
                "started": 1700100000,
                "stopped": 1700105700,
                "duration": 5700,
                "user_id": {user_id},
                "rating_key": {rating_key},
                "media_type": "movie",
                "percent_complete": 95,
                "watched_status": 1
            }}"#
        );

        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/api/v2").query_param("cmd", "get_history");
            then.status(200)
                .header("content-type", "application/json")
                .body(history_page_body(&row_json));
        });
        server.mock(|when, then| {
            when.method(GET).path("/api/v2").query_param("cmd", "get_metadata");
            then.status(200)
                .header("content-type", "application/json")
                .body(r#"{"response": {"result": "success", "data": {}}}"#);
        });
        server.mock(|when, then| {
            when.method(GET).path("/api/v2").query_param("cmd", "get_stream_data");
            then.status(200)
                .header("content-type", "application/json")
                .body(r#"{"response": {"result": "success", "data": {}}}"#);
        });

        let client = TautulliClient::new(server.base_url(), "test-key").expect("client should construct");
        let summary = run(&pool, &client, &BackfillOptions::default())
            .await
            .expect("backfill run should succeed");

        assert_eq!(summary.imported, 0, "the overlapping native session must win, not a new insert");
        assert_eq!(summary.skipped_native_overlap, 1);

        let sessions_for_item = repo::play_session::list_for_media_item(&pool, item.id)
            .await
            .expect("list sessions for media item");
        assert_eq!(sessions_for_item.len(), 1, "no duplicate session should exist");
        assert_eq!(sessions_for_item[0].id, native_session.id);

        let updated_native = repo::play_session::get(&pool, native_session.id)
            .await
            .expect("get native session");
        assert_eq!(
            updated_native.tautulli_ref_id,
            Some(reference_id),
            "the native row should be stamped with tautulli provenance"
        );
    }

    /// BSEED-1 (live-DB, gated on `MUSE_TEST_DATABASE_URL`): an arr-ingested
    /// movie carries a `tmdb_id` but `plex_rating_key = NULL`. A session whose
    /// `plex_rating_key` misses must still resolve via its `tmdb://` GUID
    /// (`resolve_media` → `find_by_tmdb_id` → `find_by_media_metadata_id`) —
    /// the whole point of BSEED-1 (no Plex library sync needed).
    #[tokio::test]
    async fn resolve_media_matches_movie_by_tmdb_guid_without_plex_rating_key() {
        let Ok(database_url) = std::env::var("MUSE_TEST_DATABASE_URL") else {
            eprintln!("MUSE_TEST_DATABASE_URL not set — skipping resolve_media_matches_movie_by_tmdb_guid_without_plex_rating_key");
            return;
        };

        let pool = PgPoolOptions::new()
            .max_connections(5)
            .connect(&database_url)
            .await
            .expect("connect");
        sqlx::migrate!("./migrations").run(&pool).await.expect("migrate");

        let suffix = Uuid::new_v4().simple().to_string();
        let tmdb_id = format!("bseed1-tmdb-{suffix}");

        let library = repo::library::create(
            &pool,
            &NewLibrary {
                name: format!("bseed1-lib-{suffix}"),
                kind: LibraryKind::Movie,
                root_folder: "/media/bseed1/".to_string(),
                source_arr_name: Some("radarr".to_string()),
                source_arr_url: None,
            },
        )
        .await
        .expect("create library");

        let metadata = repo::media_metadata::upsert_by_tmdb(
            &pool,
            &NewMediaMetadata {
                kind: MediaKind::Movie,
                tmdb_id: Some(tmdb_id.clone()),
                tvdb_id: None,
                imdb_id: Some(format!("tt{suffix}")),
                provider_ids: serde_json::json!({}),
                title: format!("BSEED-1 GUID Movie {suffix}"),
                sort_title: None,
                original_title: None,
                original_language: None,
                status: None,
                overview: None,
                studio: None,
                network: None,
                runtime_minutes: Some(110),
                year: Some(2017),
                images: serde_json::json!([]),
            },
        )
        .await
        .expect("upsert media_metadata");

        // arr-ingested: NO plex_rating_key.
        let item = repo::media_item::upsert(
            &pool,
            &NewMediaItem {
                library_id: library.id,
                media_metadata_id: metadata.id,
                path: format!("/media/bseed1/movie-{suffix}"),
                monitored: true,
                quality_profile_id: None,
                minimum_availability: None,
                plex_rating_key: None,
                added_at: None,
            },
        )
        .await
        .expect("upsert media_item");

        // A session whose rating_key doesn't match anything, but whose tmdb
        // GUID does.
        let keys = ResolveKeys {
            is_episode: false,
            rating_key: Some("does-not-exist-99999".to_string()),
            grandparent_rating_key: None,
            guids: vec![format!("tmdb://{tmdb_id}"), "imdb://tt-nope".to_string()],
            grandparent_guid: None,
            title: None,
            year: None,
        };

        let (mi, ep) = resolve_media(&pool, &keys).await.expect("resolve");
        assert_eq!(mi, Some(item.id), "must resolve via tmdb GUID");
        assert_eq!(ep, None);

        // And a bare imdb GUID resolves too (find_by_imdb_id path).
        let keys_imdb = ResolveKeys {
            guids: vec![format!("imdb://tt{suffix}")],
            ..ResolveKeys::default()
        };
        let (mi2, _) = resolve_media(&pool, &keys_imdb).await.expect("resolve imdb");
        assert_eq!(mi2, Some(item.id), "must resolve via imdb GUID");
    }

    /// FIX A (live-DB, gated): provider precedence (tmdb → tvdb → imdb) is
    /// enforced regardless of GUID array order. A session whose guids list
    /// `imdb://` BEFORE `tmdb://`, where the imdb id belongs to a DIFFERENT
    /// (wrong) movie and the tmdb id to the correct one, must resolve to the
    /// TMDB match — never the earlier-in-array imdb one (imdb_id isn't unique).
    #[tokio::test]
    async fn match_guid_enforces_tmdb_precedence_over_earlier_imdb() {
        let Ok(database_url) = std::env::var("MUSE_TEST_DATABASE_URL") else {
            eprintln!("MUSE_TEST_DATABASE_URL not set — skipping match_guid_enforces_tmdb_precedence_over_earlier_imdb");
            return;
        };
        let pool = PgPoolOptions::new()
            .max_connections(5)
            .connect(&database_url)
            .await
            .expect("connect");
        sqlx::migrate!("./migrations").run(&pool).await.expect("migrate");

        let suffix = Uuid::new_v4().simple().to_string();
        let correct_tmdb = format!("fixA-tmdb-correct-{suffix}");
        let wrong_imdb = format!("tt-fixA-wrong-{suffix}");

        let library = repo::library::create(
            &pool,
            &NewLibrary {
                name: format!("fixA-lib-{suffix}"),
                kind: LibraryKind::Movie,
                root_folder: "/media/fixA/".to_string(),
                source_arr_name: Some("radarr".to_string()),
                source_arr_url: None,
            },
        )
        .await
        .expect("library");

        // WRONG movie: carries the imdb id the session lists first.
        let wrong_md = repo::media_metadata::upsert_by_tmdb(
            &pool,
            &NewMediaMetadata {
                kind: MediaKind::Movie,
                tmdb_id: Some(format!("fixA-tmdb-wrong-{suffix}")),
                tvdb_id: None,
                imdb_id: Some(wrong_imdb.clone()),
                provider_ids: serde_json::json!({}),
                title: format!("Wrong Movie {suffix}"),
                sort_title: None,
                original_title: None,
                original_language: None,
                status: None,
                overview: None,
                studio: None,
                network: None,
                runtime_minutes: Some(90),
                year: Some(2001),
                images: serde_json::json!([]),
            },
        )
        .await
        .expect("wrong md");
        let wrong_item = repo::media_item::upsert(
            &pool,
            &NewMediaItem {
                library_id: library.id,
                media_metadata_id: wrong_md.id,
                path: format!("/media/fixA/wrong-{suffix}"),
                monitored: true,
                quality_profile_id: None,
                minimum_availability: None,
                plex_rating_key: None,
                added_at: None,
            },
        )
        .await
        .expect("wrong item");

        // CORRECT movie: carries the tmdb id the session lists second.
        let correct_md = repo::media_metadata::upsert_by_tmdb(
            &pool,
            &NewMediaMetadata {
                kind: MediaKind::Movie,
                tmdb_id: Some(correct_tmdb.clone()),
                tvdb_id: None,
                imdb_id: None,
                provider_ids: serde_json::json!({}),
                title: format!("Correct Movie {suffix}"),
                sort_title: None,
                original_title: None,
                original_language: None,
                status: None,
                overview: None,
                studio: None,
                network: None,
                runtime_minutes: Some(100),
                year: Some(2002),
                images: serde_json::json!([]),
            },
        )
        .await
        .expect("correct md");
        let correct_item = repo::media_item::upsert(
            &pool,
            &NewMediaItem {
                library_id: library.id,
                media_metadata_id: correct_md.id,
                path: format!("/media/fixA/correct-{suffix}"),
                monitored: true,
                quality_profile_id: None,
                minimum_availability: None,
                plex_rating_key: None,
                added_at: None,
            },
        )
        .await
        .expect("correct item");

        // imdb (wrong) listed BEFORE tmdb (correct) in the array.
        let keys = ResolveKeys {
            is_episode: false,
            guids: vec![format!("imdb://{wrong_imdb}"), format!("tmdb://{correct_tmdb}")],
            ..ResolveKeys::default()
        };
        let (mi, _) = resolve_media(&pool, &keys).await.expect("resolve");
        assert_eq!(
            mi,
            Some(correct_item.id),
            "tmdb precedence must win over an earlier-in-array imdb match"
        );
        assert_ne!(mi, Some(wrong_item.id), "must NOT cross-link via the non-unique imdb id");
    }

    /// BSEED-2 (live-DB, gated): a previously-imported unresolved session with
    /// stored `plex_guids` re-resolves offline once the matching `media_item`
    /// exists, AND the `(account, item, episode, started_at)` UNIQUE collision
    /// is handled — a second unresolved session that resolves to the same key
    /// is deduped, not a hard error.
    #[tokio::test]
    async fn resolve_existing_unresolved_resolves_offline_and_handles_unique_collision() {
        let Ok(database_url) = std::env::var("MUSE_TEST_DATABASE_URL") else {
            eprintln!("MUSE_TEST_DATABASE_URL not set — skipping resolve_existing_unresolved_resolves_offline_and_handles_unique_collision");
            return;
        };

        let pool = PgPoolOptions::new()
            .max_connections(5)
            .connect(&database_url)
            .await
            .expect("connect");
        sqlx::migrate!("./migrations").run(&pool).await.expect("migrate");

        let suffix = Uuid::new_v4().simple().to_string();
        let tmdb_id = format!("bseed2-tmdb-{suffix}");

        let account = repo::account::upsert_by_plex_account_id(
            &pool,
            &NewAccount {
                plex_account_id: Some(format!("bseed2-acct-{suffix}")),
                username: Some(format!("bseed2_{suffix}")),
                friendly_name: None,
                is_home_user: true,
                is_primary: false,
            },
        )
        .await
        .expect("account");

        let library = repo::library::create(
            &pool,
            &NewLibrary {
                name: format!("bseed2-lib-{suffix}"),
                kind: LibraryKind::Movie,
                root_folder: "/media/bseed2/".to_string(),
                source_arr_name: Some("radarr".to_string()),
                source_arr_url: None,
            },
        )
        .await
        .expect("library");

        let metadata = repo::media_metadata::upsert_by_tmdb(
            &pool,
            &NewMediaMetadata {
                kind: MediaKind::Movie,
                tmdb_id: Some(tmdb_id.clone()),
                tvdb_id: None,
                imdb_id: None,
                provider_ids: serde_json::json!({}),
                title: format!("BSEED-2 Movie {suffix}"),
                sort_title: None,
                original_title: None,
                original_language: None,
                status: None,
                overview: None,
                studio: None,
                network: None,
                runtime_minutes: Some(100),
                year: Some(2019),
                images: serde_json::json!([]),
            },
        )
        .await
        .expect("metadata");

        let item = repo::media_item::upsert(
            &pool,
            &NewMediaItem {
                library_id: library.id,
                media_metadata_id: metadata.id,
                path: format!("/media/bseed2/movie-{suffix}"),
                monitored: true,
                quality_profile_id: None,
                minimum_availability: None,
                plex_rating_key: None,
                added_at: None,
            },
        )
        .await
        .expect("item");

        // Two unresolved sessions at the SAME started_at (distinct only because
        // their media refs were NULL) with the same stored tmdb GUID — after
        // resolution both would key on (account, item, NULL, started_at),
        // colliding on the UNIQUE. The collision must be deduped, not fatal.
        let started_at = unix_seconds_to_utc(1_700_500_000).expect("ts");
        let guids = guids_to_json(&[format!("tmdb://{tmdb_id}")]);
        let mut session_ids = Vec::new();
        for i in 0..2 {
            let s = repo::play_session::upsert(
                &pool,
                &NewPlaySession {
                    account_id: Some(account.id),
                    media_item_id: None,
                    episode_id: None,
                    session_key: Some(format!("bseed2-{suffix}-{i}")),
                    tautulli_ref_id: Some((Uuid::new_v4().as_u128() % 1_000_000) as i64),
                    started_at,
                    stopped_at: None,
                    duration_ms: Some(100 * 60 * 1000),
                    watched_ms: Some(90 * 60 * 1000),
                    view_offset_ms: Some(90 * 60 * 1000),
                    percent_complete: Some(0.9),
                    paused_counter: 0,
                    paused_ms: 0,
                    is_finished: true,
                    is_abandoned: false,
                    player: None,
                    platform: None,
                    product: None,
                    device: None,
                    ip_address: None,
                    started_hour: Some(20),
                    started_dow: Some(5),
                    is_cinema_context: None,
                },
            )
            .await
            .expect("insert unresolved session");
            repo::play_session::set_plex_refs(&pool, s.id, None, None, &guids, None)
                .await
                .expect("stamp guids");
            session_ids.push(s.id);
        }

        // Offline re-resolution (no Tautulli client).
        let summary = resolve_existing_unresolved(&pool, None, &BackfillOptions::default(), 1000)
            .await
            .expect("re-resolution pass");

        assert!(summary.sessions_considered >= 2);
        assert_eq!(summary.resolved, 1, "first session resolves in place");
        assert_eq!(summary.deduped_conflicts, 1, "the colliding duplicate is deduped");

        // Exactly one surviving resolved session for this (account, item).
        let survivors = repo::play_session::list_for_media_item(&pool, item.id)
            .await
            .expect("list");
        let mine: Vec<_> = survivors.iter().filter(|s| s.account_id == Some(account.id)).collect();
        assert_eq!(mine.len(), 1, "collision must leave exactly one resolved session");
        assert_eq!(mine[0].media_item_id, Some(item.id));
    }

    /// FIX 1 (live-DB, gated): keyless unresolved sessions must trigger the
    /// Tautulli rehydration ref-map to be paged AT MOST ONCE per resolve call —
    /// not once per keyless row (the original unbounded-per-row bug). Two
    /// keyless sessions ⇒ exactly one `get_history` paging.
    #[tokio::test]
    async fn resolve_existing_unresolved_builds_ref_map_at_most_once() {
        let Ok(database_url) = std::env::var("MUSE_TEST_DATABASE_URL") else {
            eprintln!("MUSE_TEST_DATABASE_URL not set — skipping resolve_existing_unresolved_builds_ref_map_at_most_once");
            return;
        };
        let pool = PgPoolOptions::new()
            .max_connections(5)
            .connect(&database_url)
            .await
            .expect("connect");
        sqlx::migrate!("./migrations").run(&pool).await.expect("migrate");

        let suffix = Uuid::new_v4().simple().to_string();
        let account = repo::account::upsert_by_plex_account_id(
            &pool,
            &NewAccount {
                plex_account_id: Some(format!("fix1-acct-{suffix}")),
                username: Some(format!("fix1_{suffix}")),
                friendly_name: None,
                is_home_user: true,
                is_primary: false,
            },
        )
        .await
        .expect("account");

        // Two keyless unresolved sessions (no stored plex keys), each with a
        // tautulli_ref_id present in the mocked history.
        let r1 = (Uuid::new_v4().as_u128() % 1_000_000) as i64;
        let r2 = (Uuid::new_v4().as_u128() % 1_000_000) as i64 + 1_000_000;
        for (i, r) in [r1, r2].into_iter().enumerate() {
            repo::play_session::upsert(
                &pool,
                &NewPlaySession {
                    account_id: Some(account.id),
                    media_item_id: None,
                    episode_id: None,
                    session_key: Some(format!("fix1-{suffix}-{i}")),
                    tautulli_ref_id: Some(r),
                    started_at: unix_seconds_to_utc(1_700_600_000 + i as i64).unwrap(),
                    stopped_at: None,
                    duration_ms: None,
                    watched_ms: None,
                    view_offset_ms: None,
                    percent_complete: None,
                    paused_counter: 0,
                    paused_ms: 0,
                    is_finished: false,
                    is_abandoned: false,
                    player: None,
                    platform: None,
                    product: None,
                    device: None,
                    ip_address: None,
                    started_hour: None,
                    started_dow: None,
                    is_cinema_context: None,
                },
            )
            .await
            .expect("insert keyless session");
        }

        let server = MockServer::start();
        let history_mock = server.mock(|when, then| {
            when.method(GET).path("/api/v2").query_param("cmd", "get_history");
            then.status(200).header("content-type", "application/json").body(format!(
                r#"{{"response":{{"result":"success","data":{{"recordsFiltered":2,"recordsTotal":2,"data":[
                    {{"reference_id":{r1},"rating_key":111,"media_type":"movie"}},
                    {{"reference_id":{r2},"rating_key":222,"media_type":"movie"}}
                ]}}}}}}"#
            ));
        });
        // get_metadata returns nothing useful -> the sessions stay unresolved;
        // this test only asserts the history was paged exactly once.
        server.mock(|when, then| {
            when.method(GET).path("/api/v2").query_param("cmd", "get_metadata");
            then.status(200)
                .header("content-type", "application/json")
                .body(r#"{"response":{"result":"success","data":{}}}"#);
        });

        let client = TautulliClient::new(server.base_url(), "k").expect("client");
        let _ = resolve_existing_unresolved(&pool, Some(&client), &BackfillOptions::default(), 10_000)
            .await
            .expect("re-resolution");

        // Exactly one paging of get_history, regardless of how many keyless
        // rows were processed — the ref-map is built once and reused.
        history_mock.assert_hits(1);
    }
}
