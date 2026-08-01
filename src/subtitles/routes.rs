//! SUBS-01 — the subtitle HTTP surface.
//!
//! Five operations, mounted under the Bearer-protected ops router:
//!
//! | Route | What it does |
//! |---|---|
//! | `GET  /subtitles/:media_item_id` | list all three tiers |
//! | `POST /subtitles/:media_item_id/fetch` | search + download from the provider |
//! | `POST /subtitles/selection/:id/active` | make one the active subtitle |
//! | `POST /subtitles/selection/:id/offset/propose` | MEASURE an offset |
//! | `POST /subtitles/selection/:id/offset/apply` | apply an operator-confirmed offset |
//!
//! The last two are deliberately separate routes rather than one route with an
//! `apply: bool` flag. A flag makes "measure" and "change what the viewer
//! sees" the same request with a different body, which is exactly the kind of
//! call that gets made with the wrong body. Two routes means applying an
//! offset is always an explicit, separate act.
//!
//! # Tier status is reported per tier
//!
//! `GET /subtitles/:id` reports each tier's outcome independently. "Provider
//! not configured", "provider errored", and "provider had nothing" are three
//! different facts and are never collapsed into an empty list — the operator's
//! next action differs in each case.

use std::path::Path;
use std::sync::Arc;

use axum::{
    extract::{Path as AxumPath, Query, State},
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::error::{MuseError, MuseResult};
use crate::http::AppState;
use crate::models::subtitle::NewSubtitleSelection;
use crate::repo;

use super::rank::{rank_candidates, RankContext};
use super::wyzie::WyzieClient;
use super::{cues, discover, sync, AvailableSubtitle, HearingImpairedPreference, SubtitleSource};

#[derive(Debug, Deserialize, Default)]
pub struct ListQuery {
    /// ISO-639 language to filter/rank for. Optional: with none supplied,
    /// every discovered subtitle is listed unfiltered.
    pub language: Option<String>,
    /// `prefer` | `avoid` | `indifferent`.
    pub hearing_impaired: Option<String>,
}

fn parse_hi(raw: Option<&str>) -> MuseResult<HearingImpairedPreference> {
    match raw {
        None => Ok(HearingImpairedPreference::Indifferent),
        Some(s) => HearingImpairedPreference::parse(s).ok_or_else(|| {
            MuseError::BadRequest(format!(
                "subtitles: `{s}` is not a hearing-impaired preference (expected prefer, avoid, \
                 or indifferent)"
            ))
        }),
    }
}

/// Resolve a media item to the absolute path of its file on disk.
///
/// Errors rather than returning a default when the item or its path cannot be
/// resolved: every downstream operation here is about a specific file, and
/// operating on a guessed path is how the wrong file gets probed.
async fn media_path(state: &AppState, media_item_id: i64) -> MuseResult<std::path::PathBuf> {
    let item = repo::media_item::get(&state.pool, media_item_id).await?;
    let path = std::path::PathBuf::from(&item.path);
    if path.as_os_str().is_empty() {
        return Err(MuseError::NotFound(format!(
            "media item {media_item_id} has no filesystem path recorded"
        )));
    }
    match resolve_media_file(&path) {
        MediaFileResolution::Resolved(f) => Ok(f),
        // Three DIFFERENT facts, reported as three different errors. Collapsing
        // them into one "not found" would let "the directory could not be read"
        // render as "this title has no media", which is the same
        // absence-vs-ignorance confusion the subtitle tiers are careful about.
        MediaFileResolution::NoMediaPresent => Err(MuseError::NotFound(format!(
            "media item {media_item_id}: {} contains no media file",
            path.display()
        ))),
        MediaFileResolution::Ambiguous { count } => Err(MuseError::BadRequest(format!(
            "media item {media_item_id}: {} holds {count} comparably-sized media files, so \
             which one this row refers to cannot be determined — a split release (CD1/CD2) or \
             a season folder needs one row per file",
            path.display()
        ))),
        MediaFileResolution::CouldNotLook { reason } => Err(MuseError::Internal(anyhow::anyhow!(
            "media item {media_item_id}: could not inspect {} — {reason}. This is NOT a \
             statement that the title has no media",
            path.display()
        ))),
    }
}

/// What resolving an item's recorded path to a media FILE produced.
///
/// Four outcomes rather than `Option`, because "found nothing" and "could not
/// look" are different facts and the caller must be able to say which. An
/// earlier version returned `Option` and the read collapsed a permission error
/// into "contains no media file" — asserting an absence that was never
/// observed. Raised by codex, opus and free at the SUBS-05 gate.
#[derive(Debug, PartialEq)]
pub enum MediaFileResolution {
    Resolved(std::path::PathBuf),
    /// The directory was read successfully and holds no media file.
    NoMediaPresent,
    /// Several media files of comparable size. A release folder normally holds
    /// one feature plus much smaller extras; comparable sizes mean a split
    /// release or a season folder, and picking the largest would silently make
    /// one episode stand in for the row.
    Ambiguous { count: usize },
    /// The path could not be inspected at all.
    CouldNotLook { reason: String },
}

/// Fraction of the largest file's size at which a second file makes the choice
/// ambiguous.
///
/// A sample or trailer is a few percent of a feature; the two halves of a split
/// rip, or two episodes in one folder, are within a factor of two. Half is
/// comfortably between them.
const AMBIGUITY_RATIO: f64 = 0.5;

/// Resolve an item's recorded path to the actual media FILE.
///
/// `media_items.path` is a DIRECTORY for every row in this library — the
/// release folder, e.g. `/srv/media/Movies/1984`, holding `1984.avi` beside
/// artwork, `Thumbs.db` and a readme. SUBS-01 handed that directory straight to
/// ffprobe, which exits 1 on it, so every embedded tier reported "unreadable"
/// for the whole library. The failure was invisible in tests because every
/// fixture used a file path.
///
/// Picks the largest media file, and REFUSES when a second is comparable in
/// size — see [`MediaFileResolution::Ambiguous`]. Non-recursive: a season
/// folder's episodes are separate items with their own rows, and descending
/// would make one episode's subtitles stand in for the season.
///
/// Does not attempt VIDEO_TS/BDMV or `.iso` layouts. Those are not "a file with
/// subtitle streams" in the sense the rest of this module means, and guessing
/// at one would be worse than declining.
///
/// A path that is already a file is returned unchanged, so this is correct for
/// both layouts.
pub fn resolve_media_file(path: &std::path::Path) -> MediaFileResolution {
    match std::fs::metadata(path) {
        Ok(m) if m.is_file() => return MediaFileResolution::Resolved(path.to_path_buf()),
        Ok(m) if !m.is_dir() => {
            return MediaFileResolution::CouldNotLook {
                reason: "path is neither a file nor a directory".to_string(),
            }
        }
        Ok(_) => {}
        Err(e) => return MediaFileResolution::CouldNotLook { reason: e.to_string() },
    }

    let entries = match std::fs::read_dir(path) {
        Ok(e) => e,
        Err(e) => return MediaFileResolution::CouldNotLook { reason: e.to_string() },
    };

    let mut sized: Vec<(u64, std::path::PathBuf)> = Vec::new();
    for entry in entries {
        let entry = match entry {
            Ok(e) => e,
            // One unreadable entry is not the whole directory failing, but it
            // is also not nothing: the file it names might be the feature.
            Err(e) => return MediaFileResolution::CouldNotLook { reason: e.to_string() },
        };
        let p = entry.path();
        if !crate::library::scan::has_media_extension(&p) {
            continue;
        }
        match entry.metadata() {
            Ok(m) if m.is_file() => sized.push((m.len(), p)),
            Ok(_) => {}
            // NOT size 0 — that would rank an unmeasurable file last and let a
            // smaller one win. If a candidate cannot be measured, the
            // comparison cannot be made.
            Err(e) => {
                return MediaFileResolution::CouldNotLook {
                    reason: format!("{}: {e}", p.display()),
                }
            }
        }
    }

    sized.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    match sized.as_slice() {
        [] => MediaFileResolution::NoMediaPresent,
        [(_, only)] => MediaFileResolution::Resolved(only.clone()),
        [(big, first), (second, _), ..] => {
            if *big > 0 && (*second as f64) >= (*big as f64) * AMBIGUITY_RATIO {
                MediaFileResolution::Ambiguous { count: sized.len() }
            } else {
                MediaFileResolution::Resolved(first.clone())
            }
        }
    }
}

/// Probe a media file through Foundry's path guard.
///
/// Deliberately routed through [`crate::foundry::Foundry`] rather than calling
/// ffprobe directly: Foundry owns the default-deny allowed-roots allowlist,
/// and a second, unguarded probe path in this module would be a way to read a
/// file the guard would have refused. When Foundry is unconfigured the
/// embedded tier is UNAVAILABLE, which is reported as such — never as "this
/// file has no embedded subtitles".
fn probe_media(state: &AppState, path: &std::path::Path) -> Result<crate::foundry::MediaProbe, String> {
    let Some(foundry) = crate::foundry::Foundry::from_config(&state.config) else {
        return Err(
            "Foundry is not configured (MUSE_FOUNDRY_ALLOWED_ROOTS unset), so embedded subtitle \
             streams cannot be read from this file — this is NOT a statement that the file has none"
                .to_string(),
        );
    };
    foundry.probe_file(path)
}

/// `GET /subtitles/:media_item_id` — every subtitle Muse can offer, from all
/// three tiers, with each tier's status reported separately.
pub async fn list_subtitles(
    State(state): State<Arc<AppState>>,
    AxumPath(media_item_id): AxumPath<i64>,
    Query(q): Query<ListQuery>,
) -> MuseResult<Json<Value>> {
    let hi = parse_hi(q.hearing_impaired.as_deref())?;
    let path = media_path(&state, media_item_id).await?;

    // --- Tier 1: embedded. ---
    // Probing goes through `Foundry`, not a bare ffprobe call, so the
    // allowed-roots path guard applies here exactly as it does everywhere else
    // that reads a library file.
    let mut probed: Option<crate::foundry::probe::MediaProbe> = None;
    let (embedded, embedded_status) = match probe_media(&state, &path) {
        Ok(probe) => {
            let found = discover::embedded_from_probe(&probe);
            let count = found.len();
            probed = Some(probe);
            (found, json!({ "ok": true, "count": count }))
        }
        Err(reason) => (
            Vec::new(),
            // NOT an empty tier. "ffprobe is missing" and "this file has no
            // embedded subtitles" are different facts, and the first must
            // never render as the second — the whole preference order depends
            // on being able to see the embedded tier.
            json!({ "ok": false, "reason": reason }),
        ),
    };

    // --- Tier 2: sidecar. ---
    let sidecars = discover::detect_sidecars(&path);
    let sidecar_status = json!({ "ok": true, "count": sidecars.len() });
    let sidecar_available: Vec<AvailableSubtitle> = sidecars.iter().map(|s| s.as_available()).collect();

    // --- Tier 3: provider (previously fetched rows only; this route never
    // calls the provider — see the `fetch` route). ---
    let recorded = repo::subtitle::list_for_item(&state.pool, media_item_id).await?;

    // Reconcile persisted EMBEDDED selections against the file as it is NOW.
    //
    // A stream index is only meaningful against one particular file, and a
    // title can be replaced by a quality upgrade whose stream layout differs.
    // Serving the old index then means showing whatever subtitle happens to
    // occupy it — the "Hungarian subtitles you never chose" failure. So a
    // drifted selection is DEACTIVATED here rather than served, and the drift
    // is reported so the operator can see why their choice went away.
    //
    // Only runs when the probe succeeded: if ffprobe is unavailable we cannot
    // distinguish "the stream is gone" from "we could not look", and
    // deactivating an operator's choice on the strength of a failed probe
    // would be its own bug.
    //
    // codex, SUBS-01 gate: verify_embedded_selection existed and was correct
    // but had no production caller, so nothing was ever invalidated.
    let mut invalidated: Vec<Value> = Vec::new();
    if let Some(probe) = probed.as_ref() {
        for row in recorded.iter().filter(|r| r.is_active && r.source == "embedded") {
            let (Some(idx), Some(codec)) = (row.embedded_stream_index, row.embedded_codec.as_deref())
            else {
                continue;
            };
            if let Err(drift) = discover::verify_embedded_selection(
                probe,
                idx as u32,
                codec,
                row.language.as_deref(),
            ) {
                repo::subtitle::invalidate(&state.pool, row.id).await?;
                invalidated.push(json!({
                    "selection_id": row.id,
                    "stream_index": idx,
                    "why": drift.to_string(),
                }));
            }
        }
    }
    // Re-read only if something changed, so the response never shows a
    // selection this request just deactivated.
    let recorded = if invalidated.is_empty() {
        recorded
    } else {
        repo::subtitle::list_for_item(&state.pool, media_item_id).await?
    };

    let mut available: Vec<AvailableSubtitle> = Vec::new();
    available.extend(embedded);
    available.extend(sidecar_available);

    let filtered: Vec<&AvailableSubtitle> = match q.language.as_deref() {
        Some(lang) => available.iter().filter(|s| s.matches_language(lang)).collect(),
        None => available.iter().collect(),
    };

    let preferred = q.language.as_deref().map(|lang| super::SelectionPreference {
        language: lang.to_string(),
        hearing_impaired: hi,
        allow_forced: false,
    });
    let picked = preferred
        .as_ref()
        .and_then(|pref| super::select_preferred(&available, pref));

    Ok(Json(json!({
        "media_item_id": media_item_id,
        // The preference order, stated in the response so a UI does not have
        // to reimplement it (or drift from it).
        "preference_order": super::SubtitleSource::PREFERENCE_ORDER,
        // Selections deactivated by this request because the file no longer
        // has the stream they named. Always present, so a UI can tell "nothing
        // drifted" from "this response does not report drift".
        "invalidated": invalidated,
        "tiers": {
            "embedded": embedded_status,
            "sidecar": sidecar_status,
            "provider": {
                "configured": state.config.wyzie_key.is_some(),
                "recorded_count": recorded.iter().filter(|r| r.source == "provider").count(),
                "note": "this route lists only already-fetched provider subtitles; \
                         POST /subtitles/:id/fetch searches the provider",
            },
        },
        "available": filtered.iter().map(|s| available_json(s)).collect::<Vec<_>>(),
        "recommended": picked.map(available_json),
        "recorded": recorded.iter().map(|r| json!({
            "id": r.id,
            "source": r.source,
            "language": r.language,
            "is_active": r.is_active,
            "offset_ms": r.offset_ms,
            "offset_confirmed_at": r.offset_confirmed_at,
            "proposed_offset_ms": r.proposed_offset_ms,
            "proposed_confidence": r.proposed_confidence,
            "machine_generated": r.provider_machine_generated,
            "storage_path": r.storage_path,
        })).collect::<Vec<_>>(),
    })))
}

fn available_json(s: &AvailableSubtitle) -> Value {
    json!({
        "source": s.source,
        "tier": s.source.kind_str(),
        "preference_rank": s.source.preference_rank(),
        "why_this_tier": s.source.preference_reason(),
        "language": s.language,
        "display": s.display,
        "format": s.format.map(|f| f.as_str()),
        "forced": s.forced,
        "hearing_impaired": s.hearing_impaired,
        // Surfaced so a UI can grey out the timing control instead of letting
        // an operator discover the limitation as a failure.
        "can_adjust_timing": s.is_shiftable(),
    })
}

#[derive(Debug, Deserialize)]
pub struct FetchRequest {
    pub language: String,
    #[serde(default)]
    pub hearing_impaired: Option<String>,
    /// How many ranked candidates to actually download and record. Downloads
    /// are bounded because each is a separate provider request.
    #[serde(default)]
    pub limit: Option<usize>,
}

/// `POST /subtitles/:media_item_id/fetch` — search the provider, rank, and
/// download the top candidates.
///
/// Downloads are recorded INACTIVE. Fetching is not choosing: the operator
/// still picks, having seen the ranking and the machine-generated flags.
pub async fn fetch_from_provider(
    State(state): State<Arc<AppState>>,
    AxumPath(media_item_id): AxumPath<i64>,
    Json(req): Json<FetchRequest>,
) -> MuseResult<Json<Value>> {
    let hi = parse_hi(req.hearing_impaired.as_deref())?;
    let limit = req.limit.unwrap_or(3).clamp(1, 10);

    let Some(client) = WyzieClient::from_config(&state.config) else {
        // Distinct from "nothing found". An unconfigured provider is an ops
        // gap the operator can close; an empty result is not.
        return Err(MuseError::ServiceUnavailable(
            "subtitles: the Wyzie provider is not configured (WYZIE_KEY is unset) — this is \
             not the same as finding no subtitles"
                .into(),
        ));
    };

    let item = repo::media_item::get(&state.pool, media_item_id).await?;
    let metadata = repo::media_metadata::get(&state.pool, item.media_metadata_id).await?;
    let imdb_id = metadata.imdb_id.clone().ok_or_else(|| {
        MuseError::BadRequest(format!(
            "subtitles: media item {media_item_id} has no IMDb id, which the provider needs to \
             search — resolve its metadata first"
        ))
    })?;

    // A provider error propagates as an error. It is never an empty list.
    let results = client.search(&imdb_id, &req.language).await?;

    // The release name is the heaviest ranking signal. When Muse does not know
    // it, the ranker is told `None` rather than being handed the filename as a
    // stand-in — a filename is often not the release name, and a wrong release
    // match is the one error the ranking model cannot absorb.
    let release_name = repo::media_file::list_by_media_item(&state.pool, media_item_id)
        .await
        .ok()
        .and_then(|files| files.into_iter().find_map(|f| f.scene_name));

    let candidates: Vec<super::rank::Candidate> = results.iter().map(|r| r.as_candidate()).collect();
    let ranked = rank_candidates(
        &candidates,
        &RankContext {
            release_name,
            hearing_impaired: hi,
        },
    );

    let mut recorded = Vec::new();
    let mut download_failures = Vec::new();

    for entry in ranked.iter().take(limit) {
        let Some(result) = results.iter().find(|r| r.id == entry.id) else {
            continue;
        };
        let Some(format) = result.subtitle_format() else {
            download_failures.push(json!({
                "id": entry.id,
                "reason": "the provider offered a format Muse cannot read as text",
            }));
            continue;
        };

        let text = match client.download(&result.url).await {
            Ok(text) => text,
            Err(e) => {
                // Recorded and reported, not swallowed. A partial fetch must
                // be visible as a partial fetch.
                download_failures.push(json!({ "id": entry.id, "reason": e.to_string() }));
                continue;
            }
        };

        // Parse before persisting: a body that is not a readable subtitle is
        // rejected here rather than stored and discovered later, mid-viewing.
        if let Err(e) = cues::parse_cue_spans(&text, format) {
            download_failures.push(json!({
                "id": entry.id,
                "reason": format!("the downloaded subtitle could not be read: {e}"),
            }));
            continue;
        }

        let stored = super::adjust::store_original(
            state.config.subtitle_store_dir.as_deref(),
            media_item_id,
            &req.language,
            &format!("wyzie-{}", entry.id),
            format,
            &text,
        );
        let storage_path = match stored {
            Ok(path) => path.to_string_lossy().into_owned(),
            Err(MuseError::Conflict(_)) => {
                // Already fetched previously. Not a failure.
                super::adjust::store_filename(
                    media_item_id,
                    &req.language,
                    &format!("wyzie-{}", entry.id),
                    0,
                    format,
                )
            }
            Err(e) => return Err(e),
        };

        let row = repo::subtitle::record(
            &state.pool,
            NewSubtitleSelection {
                media_item_id,
                language: Some(req.language.to_ascii_lowercase()),
                source: SubtitleSource::Provider {
                    provider: super::wyzie::PROVIDER_NAME.to_string(),
                    provider_id: entry.id.clone(),
                    machine_generated: entry.machine_generated,
                },
                storage_path: Some(storage_path),
                provider_url: Some(result.url.clone()),
                forced: false,
                hearing_impaired: entry.hearing_impaired,
            },
        )
        .await?;

        recorded.push(json!({
            "selection_id": row.id,
            "provider_id": entry.id,
            "score": entry.score,
            "release_agreement": entry.release_agreement,
            "machine_generated": entry.machine_generated,
            "hearing_impaired": entry.hearing_impaired,
            "download_count": entry.download_count,
            "why": entry.reasons,
        }));
    }

    Ok(Json(json!({
        "media_item_id": media_item_id,
        "provider": super::wyzie::PROVIDER_NAME,
        "results_found": results.len(),
        "ranked": ranked.len(),
        "downloaded": recorded,
        // Never hidden: a fetch that partially failed must not look like one
        // that partially succeeded by choice.
        "failures": download_failures,
        "note": "downloaded subtitles are recorded INACTIVE — activate one explicitly",
    })))
}

/// `POST /subtitles/selection/:id/active`
pub async fn set_active(
    State(state): State<Arc<AppState>>,
    AxumPath(id): AxumPath<i64>,
) -> MuseResult<Json<Value>> {
    let row = repo::subtitle::set_active(&state.pool, id).await?;
    Ok(Json(json!({
        "selection_id": row.id,
        "media_item_id": row.media_item_id,
        "source": row.source,
        "language": row.language,
        "is_active": row.is_active,
        "offset_ms": row.offset_ms,
    })))
}

/// `POST /subtitles/selection/:id/offset/propose` — MEASURE an offset.
///
/// **Applies nothing.** The measurement is recorded in the proposal columns
/// and returned with its confidence; the applied offset is untouched.
pub async fn propose_offset(
    State(state): State<Arc<AppState>>,
    AxumPath(id): AxumPath<i64>,
) -> MuseResult<Json<Value>> {
    let selection = repo::subtitle::get(&state.pool, id)
        .await?
        .ok_or_else(|| MuseError::NotFound(format!("subtitle selection {id}")))?;

    let format = selection.format().ok_or_else(|| {
        MuseError::BadRequest(
            "subtitles: this subtitle's timings are not in a text format Muse can measure or \
             shift (image-based subtitles such as PGS/VOBSUB cannot be re-timed)"
                .into(),
        )
    })?;
    let subtitle_path = selection.readable_path().ok_or_else(|| {
        MuseError::BadRequest(
            "subtitles: this selection is an embedded stream with no extracted copy, so there \
             is no cue list to measure — extract or fetch it first"
                .into(),
        )
    })?;

    let text = discover::read_sidecar(Path::new(subtitle_path))?;
    let spans = cues::parse_cue_spans(&text, format)
        .map_err(|e| MuseError::BadRequest(format!("subtitles: {e}")))?;

    let media = media_path(&state, selection.media_item_id).await?;
    let probe = probe_media(&state, &media)
        .map_err(|e| MuseError::ServiceUnavailable(format!("subtitles: could not probe the media file: {e}")))?;
    let duration_ms = probe
        .duration_secs
        .filter(|d| *d > 0.0)
        .map(|d| (d * 1000.0) as i64)
        .ok_or_else(|| {
            MuseError::ServiceUnavailable(
                "subtitles: the media file's duration is unknown, so no timing measurement is \
                 possible (this is not a statement that the timing is correct)"
                    .into(),
            )
        })?;

    let ffmpeg_bin = state.config.ffmpeg_path.clone();
    let media_for_task = media.clone();
    // The ffmpeg scan is minutes of blocking work; it must not sit on an async
    // worker thread.
    let silences = tokio::task::spawn_blocking(move || {
        sync::extract_speech_activity(&ffmpeg_bin, &media_for_task)
    })
    .await
    .map_err(|e| MuseError::Internal(anyhow::anyhow!("subtitles: the audio analysis task failed: {e}")))?
    .map_err(|e| MuseError::ServiceUnavailable(format!("subtitles: {e}")))?;

    let proposal = sync::propose_offset(&silences, &spans, duration_ms)
        .map_err(|e| MuseError::ServiceUnavailable(format!("subtitles: {e}")))?;

    let row = repo::subtitle::record_proposal(
        &state.pool,
        id,
        proposal.offset_ms,
        proposal.confidence.as_str(),
    )
    .await?;

    Ok(Json(json!({
        "selection_id": row.id,
        // Named `proposed_offset_ms`, never `offset_ms`, so a client cannot
        // read this response as a statement about what is applied.
        "proposed_offset_ms": proposal.offset_ms,
        "confidence": proposal.confidence,
        "explanation": proposal.explanation,
        "peak_score": proposal.peak_score,
        "prominence": proposal.prominence,
        "resolution_ms": proposal.resolution_ms,
        "audio_active_bins": proposal.audio_active_bins,
        "subtitle_active_bins": proposal.subtitle_active_bins,
        "applied": false,
        "currently_applied_offset_ms": row.offset_ms,
        "worth_offering": proposal.confidence.worth_offering(),
        "note": "NOTHING was changed. Applying this offset requires an explicit call to \
                 /offset/apply with the value you accept.",
    })))
}

#[derive(Debug, Deserialize)]
pub struct ApplyOffsetRequest {
    /// The offset the operator is confirming, in milliseconds.
    ///
    /// Required and explicit — the route deliberately does NOT default to the
    /// stored proposal. Making the client restate the number means an
    /// operator cannot apply a measurement they never looked at, and a stale
    /// proposal from a previous file cannot be applied by an empty body.
    pub offset_ms: i64,
}

/// `POST /subtitles/selection/:id/offset/apply` — apply an offset the operator
/// has confirmed, writing an adjusted COPY.
pub async fn apply_offset(
    State(state): State<Arc<AppState>>,
    AxumPath(id): AxumPath<i64>,
    Json(req): Json<ApplyOffsetRequest>,
) -> MuseResult<Json<Value>> {
    let selection = repo::subtitle::get(&state.pool, id)
        .await?
        .ok_or_else(|| MuseError::NotFound(format!("subtitle selection {id}")))?;

    let format = selection.format().ok_or_else(|| {
        MuseError::BadRequest("subtitles: this subtitle's format cannot be re-timed".into())
    })?;
    // The IMMUTABLE original, not the currently-serving file. Reading
    // `readable_path()` here meant a second adjustment shifted the output of
    // the first, so +1000ms then +2000ms produced +3000ms of shift while the
    // row recorded 2000. Raised by codex at the SUBS-01 gate.
    let source_path = selection.adjustment_source_path().ok_or_else(|| {
        MuseError::BadRequest("subtitles: this selection has no readable subtitle file to shift".into())
    })?;

    // Read the source, then hand the TEXT to the writer. The writer never
    // opens the source, so it structurally cannot write back over it.
    let source_text = discover::read_sidecar(Path::new(source_path))?;

    let adjusted = super::adjust::write_adjusted(
        state.config.subtitle_store_dir.as_deref(),
        selection.media_item_id,
        selection.language.as_deref().unwrap_or("und"),
        &format!("sel-{id}"),
        format,
        &source_text,
        req.offset_ms,
    )?;

    let path = adjusted.path.to_string_lossy().into_owned();
    let row = repo::subtitle::apply_confirmed_offset(&state.pool, id, req.offset_ms, &path).await?;

    Ok(Json(json!({
        "selection_id": row.id,
        "offset_ms": row.offset_ms,
        "offset_confirmed_at": row.offset_confirmed_at,
        "adjusted_path": path,
        "source_path": source_path,
        "cues_shifted": adjusted.applied.cues_shifted,
        // Reported, not hidden: a large negative shift that pinned cues at
        // zero is usually a sign the offset was wrong.
        "cues_clamped_at_zero": adjusted.applied.clamped_at_zero,
        "note": "the original subtitle was NOT modified — the adjusted copy is a new file",
    })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_typo_in_the_hearing_impaired_preference_is_a_bad_request_not_a_default() {
        assert_eq!(parse_hi(None).unwrap(), HearingImpairedPreference::Indifferent);
        assert_eq!(parse_hi(Some("prefer")).unwrap(), HearingImpairedPreference::Prefer);
        let err = parse_hi(Some("prefered")).unwrap_err();
        assert!(matches!(err, MuseError::BadRequest(_)), "got {err:?}");
    }

    #[test]
    fn the_apply_request_requires_an_explicit_offset_and_does_not_default_to_the_proposal() {
        // An empty body must not silently apply whatever was last measured.
        assert!(serde_json::from_str::<ApplyOffsetRequest>("{}").is_err());
        assert!(serde_json::from_str::<ApplyOffsetRequest>(r#"{"offset_ms":null}"#).is_err());
        let parsed: ApplyOffsetRequest = serde_json::from_str(r#"{"offset_ms":-2500}"#).unwrap();
        assert_eq!(parsed.offset_ms, -2_500);
    }

    #[test]
    fn the_fetch_request_requires_a_language() {
        // Muse never picks a language on the operator's behalf.
        assert!(serde_json::from_str::<FetchRequest>("{}").is_err());
        let parsed: FetchRequest = serde_json::from_str(r#"{"language":"en"}"#).unwrap();
        assert_eq!(parsed.language, "en");
        assert_eq!(parsed.limit, None);
    }

    #[test]
    fn the_available_json_exposes_the_tier_its_rank_and_whether_timing_can_be_adjusted() {
        let sub = AvailableSubtitle {
            source: SubtitleSource::Embedded {
                stream_index: 2,
                codec: "hdmv_pgs_subtitle".into(),
            },
            language: Some("eng".into()),
            display: None,
            format: None,
            forced: false,
            hearing_impaired: false,
        };
        let value = available_json(&sub);
        assert_eq!(value["tier"], "embedded");
        assert_eq!(value["preference_rank"], 0);
        assert_eq!(
            value["can_adjust_timing"], false,
            "an image-based track must advertise that its timing cannot be adjusted"
        );
        assert!(value["why_this_tier"].as_str().unwrap().contains("exact"));
    }
}

#[cfg(test)]
mod media_file_resolution_tests {
    use super::{resolve_media_file, MediaFileResolution};
    use std::fs;

    fn tmp(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("muse-subs05-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).expect("scratch dir");
        d
    }

    fn write(p: &std::path::Path, bytes: usize) {
        fs::write(p, vec![b'x'; bytes]).expect("write fixture");
    }

    fn resolved(r: MediaFileResolution) -> std::path::PathBuf {
        match r {
            MediaFileResolution::Resolved(p) => p,
            other => panic!("expected a resolved file, got {other:?}"),
        }
    }

    /// The bug this fixes: every `media_items.path` is the release DIRECTORY,
    /// and SUBS-01 handed it straight to ffprobe, which exits 1 on a directory.
    /// The whole embedded tier reported "unreadable" for all 1892 items, and no
    /// test caught it because every fixture used a file path.
    ///
    /// The artwork here is deliberately LARGER than the feature. Opus caught
    /// the first version at the gate: with the .avi also the largest file, the
    /// test passed whether or not the extension filter existed, so it did not
    /// test the thing its name claims.
    #[test]
    fn a_release_directory_resolves_to_the_feature_file_not_the_artwork() {
        let d = tmp("dir");
        write(&d.join("1984.avi"), 5_000);
        write(&d.join("fanart.jpg"), 50_000);
        write(&d.join("!!!READ_ME.txt"), 90_000);
        assert_eq!(resolved(resolve_media_file(&d)), d.join("1984.avi"));
        let _ = fs::remove_dir_all(&d);
    }

    /// Release folders ship a sample beside the feature.
    ///
    /// The sample is named to sort BEFORE the feature — codex caught the first
    /// version, where `The.Movie.mkv` sorted before lowercase `sample.mkv` in
    /// byte order, so a lexicographic-first implementation passed too.
    #[test]
    fn the_largest_media_file_wins_over_a_sample_that_sorts_first() {
        let d = tmp("sample");
        write(&d.join("AAA-sample.mkv"), 1_000);
        write(&d.join("The.Movie.2020.mkv"), 900_000);
        assert_eq!(
            resolved(resolve_media_file(&d)),
            d.join("The.Movie.2020.mkv"),
            "must pick by SIZE, not by name order"
        );
        let _ = fs::remove_dir_all(&d);
    }

    /// A season folder of sibling episodes, or a CD1/CD2 split, has no single
    /// feature. Picking the largest would silently make one episode stand in
    /// for the whole row.
    #[test]
    fn comparably_sized_media_files_are_ambiguous_rather_than_guessed_at() {
        let d = tmp("ambig");
        write(&d.join("S01E01.mkv"), 500_000);
        write(&d.join("S01E02.mkv"), 480_000);
        assert_eq!(
            resolve_media_file(&d),
            MediaFileResolution::Ambiguous { count: 2 },
            "two comparable files must refuse, not pick one"
        );
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn a_file_path_is_returned_unchanged() {
        let d = tmp("file");
        let f = d.join("movie.mkv");
        write(&f, 10);
        assert_eq!(resolved(resolve_media_file(&f)), f);
        let _ = fs::remove_dir_all(&d);
    }

    /// "Looked and found none" — distinct from "could not look".
    #[test]
    fn a_directory_with_no_media_reports_absence_not_failure() {
        let d = tmp("nomedia");
        write(&d.join("readme.txt"), 10);
        write(&d.join("poster.jpg"), 10);
        assert_eq!(resolve_media_file(&d), MediaFileResolution::NoMediaPresent);
        let _ = fs::remove_dir_all(&d);
    }

    /// Non-recursive on purpose: a season folder's episodes are separate items
    /// with their own rows.
    #[test]
    fn resolution_does_not_descend_into_subdirectories() {
        let d = tmp("nested");
        fs::create_dir_all(d.join("Season 01")).expect("subdir");
        write(&d.join("Season 01").join("S01E01.mkv"), 900_000);
        assert_eq!(resolve_media_file(&d), MediaFileResolution::NoMediaPresent);
        let _ = fs::remove_dir_all(&d);
    }

    /// The tri-state rule: a path that cannot be inspected must NOT report as
    /// an absence. Both opus and free raised this at the SUBS-05 gate — the
    /// first version collapsed every filesystem error into `None`, which the
    /// caller then rendered as "contains no media file", asserting something
    /// it had never observed.
    #[test]
    fn a_path_that_cannot_be_inspected_is_not_reported_as_an_absence() {
        let r = resolve_media_file(std::path::Path::new("/nonexistent-muse-subs05"));
        assert!(
            matches!(r, MediaFileResolution::CouldNotLook { .. }),
            "expected CouldNotLook, got {r:?}"
        );
        assert_ne!(r, MediaFileResolution::NoMediaPresent);
    }

    /// An unreadable DIRECTORY (mode 000) is the case the criteria named
    /// explicitly. Skipped when running as root, which can read it anyway.
    #[test]
    fn an_unreadable_directory_is_not_reported_as_an_absence() {
        use std::os::unix::fs::PermissionsExt;
        let d = tmp("noperm");
        write(&d.join("movie.mkv"), 100);
        fs::set_permissions(&d, fs::Permissions::from_mode(0o000)).expect("chmod");
        let r = resolve_media_file(&d);
        let readable_anyway = matches!(r, MediaFileResolution::Resolved(_));
        fs::set_permissions(&d, fs::Permissions::from_mode(0o755)).expect("restore");
        let _ = fs::remove_dir_all(&d);
        if readable_anyway {
            return; // running as root; the permission bit does not apply
        }
        assert!(
            matches!(r, MediaFileResolution::CouldNotLook { .. }),
            "expected CouldNotLook, got {r:?}"
        );
    }
}
