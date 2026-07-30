//! MWEBX-05 (S126): the MUSE web "detail bench" READ API — the JSON the
//! harmony-web `museAdapter` binds its Library / Discover / Requests / Taste /
//! Curation / Subsystems screens to, replacing the fixture reads.
//!
//! ## Posture (why these are all read-only, fail-open)
//! Phase-0 MUSE web is a *browse + inspect* surface — "every screen assumes
//! the assistant did the reaching; the page is where you look closely."
//! Nothing here writes, and nothing here can trigger a grab/download — the
//! acquisition write-path stays behind the dual-safety gate in
//! `crate::http::requests` (MUSEM-05). Every handler degrades to an honest
//! empty/seam `200` rather than a `500`: a missing QNAP mount, an
//! unconfigured Prowlarr/TMDb, or a cold-start account each yield an
//! empty-but-valid body so the UI can render its Live/Worker/Seam/Unmounted
//! states honestly.
//!
//! ## Wiring, not rebuilding
//! These endpoints are thin read projections over subsystems that already
//! exist:
//! - Library reads come from the MUSEL-B1 read-only scanner's persisted
//!   output (`media_items`/`media_files`/`media_metadata`) — see
//!   `crate::library::scan`. `POST /ops/library/scan` (in `crate::http::ops`)
//!   triggers/refreshes a scan on demand; this module only READS what it
//!   recorded.
//! - Artwork URLs point at the same-origin `/art/{kind}/{id}` proxy
//!   (`crate::web::artwork`), which resolves a real poster from the sidecar
//!   cache or the provider (`media_metadata.images`) — the browser never sees
//!   an upstream provider URL or token.
//! - Discover reads the MUSE-19 trending snapshots (`trending_snapshots`).
//! - Requests read the MUSEM-05 acquisition domain (`media_requests`/
//!   `download_queue`/`monitored_items`).
//! - Taste reads the MUSE-10/MUSE-13 taste model + radar
//!   (`taste_profile`/`taste_divergence`).
//! - Curation reuses the MUSE-11 candidate gatherers (`crate::curation`).
//! - Indexers/RSS read the MUSE-16 Prowlarr client (`crate::prowlarr`).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::Json;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::error::{MuseError, MuseResult};
use crate::http::AppState;
use crate::models::media_metadata::MediaKind;
use crate::repo;

const DEFAULT_GRID_LIMIT: i64 = 120;
const DEFAULT_TABLE_LIMIT: i64 = 500;
const DEFAULT_DISCOVER_LIMIT: i64 = 40;
const DEFAULT_CURATION_LIMIT: i64 = 30;
/// MUSE #84: the Constellation dashboard's On-Deck and Gaps cards are
/// fixed-height summary widgets, not browsable lists — a generous cap keeps
/// one dashboard mount from pulling the whole backlog over the proxy. `/gaps`
/// reports its untruncated `total` separately, so the cap is visible, never
/// silent.
const ON_DECK_LIMIT: i64 = 60;
const GAPS_LIMIT: i64 = 60;
/// TMDb trending is region-scoped; US is the neutral default when the caller
/// doesn't pin one (matches the trending worker's own default posture).
const DEFAULT_REGION: &str = "US";
/// Max indexers polled + releases returned by the RSS report endpoint — a
/// manual operator read, not a hot loop, so bounded generously.
const RSS_MAX_INDEXERS: usize = 8;
const RSS_MAX_RELEASES: usize = 100;

/// Same-origin artwork proxy URL for a title's poster (default variant).
fn poster_url(media_metadata_id: i64) -> String {
    format!("/art/media_metadata/{media_metadata_id}")
}

/// Same-origin artwork proxy URL for a title's backdrop/fanart.
fn backdrop_url(media_metadata_id: i64) -> String {
    format!("/art/media_metadata/{media_metadata_id}?variant=fanart")
}

/// Resolve the account a per-account screen (Taste/Curation) should read.
/// An explicit `?account_id=` wins; otherwise the primary account, else the
/// first account, else `None` (cold-start deployment with no accounts yet →
/// the caller returns a seam-empty body). Never errors — a failed lookup
/// degrades to `None`.
async fn resolve_account_id(state: &AppState, requested: Option<i64>) -> Option<i64> {
    if let Some(id) = requested {
        return Some(id);
    }
    let accounts = repo::account::list(&state.pool).await.unwrap_or_default();
    accounts
        .iter()
        .find(|a| a.is_primary)
        .or_else(|| accounts.first())
        .map(|a| a.id)
}

/// Fallible sibling of [`resolve_account_id`] for handlers that must not turn
/// a database failure into a valid-looking empty body.
///
/// `resolve_account_id` folds a failed `repo::account::list` into `None` via
/// `unwrap_or_default`, which makes "the accounts query broke" indistinguishable
/// from "no accounts exist". For the fail-open MWEBX-05 screens that is the
/// intended posture; for the MUSE #84 dashboard endpoints it is not — the
/// Constellation hook renders any 2xx as data, so the two cases must be
/// distinguishable. `Ok(None)` here means the query SUCCEEDED and found no
/// accounts (a true absence); an `Err` propagates.
///
/// (Both round-2 reviewers caught this: the first fix propagated errors from
/// the on-deck query itself but left this resolver's swallowed error in place.)
async fn resolve_account_id_fallible(
    state: &AppState,
    requested: Option<i64>,
) -> MuseResult<Option<i64>> {
    if let Some(id) = requested {
        return Ok(Some(id));
    }
    let accounts = repo::account::list(&state.pool).await?;
    Ok(accounts
        .iter()
        .find(|a| a.is_primary)
        .or_else(|| accounts.first())
        .map(|a| a.id))
}

fn kind_str(kind: MediaKind) -> &'static str {
    match kind {
        MediaKind::Movie => "movie",
        MediaKind::Show => "show",
    }
}

// ===========================================================================
// Library
// ===========================================================================

#[derive(Debug, Clone, Deserialize)]
pub struct LibraryQuery {
    pub limit: Option<i64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct GridItem {
    pub media_item_id: i64,
    pub media_metadata_id: i64,
    pub kind: &'static str,
    pub title: String,
    pub year: Option<i32>,
    /// `"on_disk"` when at least one file exists, else `"monitored"` (owned
    /// row, nothing on disk yet).
    pub availability: &'static str,
    pub monitored: bool,
    pub poster_url: String,
    pub backdrop_url: String,
    pub tmdb_id: Option<String>,
    pub tvdb_id: Option<String>,
    pub imdb_id: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct WantedGridItem {
    pub monitored_item_id: i64,
    pub media_metadata_id: i64,
    pub library_id: i64,
    pub kind: &'static str,
    pub title: String,
    pub year: Option<i32>,
    pub availability: &'static str,
    pub poster_url: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct LibraryGridResponse {
    pub owned: Vec<GridItem>,
    pub wanted: Vec<WantedGridItem>,
    pub counts: Value,
}

/// `GET /api/library` — the poster-wall grid: owned titles (with real
/// on-disk availability) plus the wanted set. Fail-open: an empty/unmounted
/// library returns empty lists, never a 500.
pub async fn get_library(
    State(state): State<Arc<AppState>>,
    Query(q): Query<LibraryQuery>,
) -> Json<LibraryGridResponse> {
    let limit = q.limit.unwrap_or(DEFAULT_GRID_LIMIT).clamp(1, 1000);

    let owned_rows = repo::dashboard::library_grid(&state.pool, limit)
        .await
        .unwrap_or_else(|e| {
            tracing::warn!(error = %e, "get_library: owned grid query failed; serving empty");
            Vec::new()
        });
    let wanted_rows = repo::dashboard::wanted_titles(&state.pool, limit)
        .await
        .unwrap_or_else(|e| {
            tracing::warn!(error = %e, "get_library: wanted query failed; serving empty");
            Vec::new()
        });
    let counts = repo::dashboard::library_counts(&state.pool)
        .await
        .map(|c| json!({ "owned": c.owned, "wanted": c.wanted, "on_disk": c.on_disk }))
        .unwrap_or_else(|_| json!({ "owned": 0, "wanted": 0, "on_disk": 0 }));

    let owned = owned_rows
        .into_iter()
        .map(|r| GridItem {
            media_item_id: r.media_item_id,
            media_metadata_id: r.media_metadata_id,
            kind: kind_str(r.kind),
            title: r.title,
            year: r.year,
            availability: if r.has_file { "on_disk" } else { "monitored" },
            monitored: r.monitored,
            poster_url: poster_url(r.media_metadata_id),
            backdrop_url: backdrop_url(r.media_metadata_id),
            tmdb_id: r.tmdb_id,
            tvdb_id: r.tvdb_id,
            imdb_id: r.imdb_id,
        })
        .collect();

    let wanted = wanted_rows
        .into_iter()
        .map(|r| WantedGridItem {
            monitored_item_id: r.monitored_item_id,
            media_metadata_id: r.media_metadata_id,
            library_id: r.library_id,
            kind: kind_str(r.kind),
            title: r.title,
            year: r.year,
            availability: "wanted",
            poster_url: poster_url(r.media_metadata_id),
        })
        .collect();

    Json(LibraryGridResponse {
        owned,
        wanted,
        counts,
    })
}

#[derive(Debug, Clone, Serialize)]
pub struct TableRow {
    pub media_item_id: i64,
    pub media_metadata_id: i64,
    pub kind: &'static str,
    pub title: String,
    pub year: Option<i32>,
    pub monitored: bool,
    pub quality_profile_id: Option<i64>,
    pub quality_profile_name: Option<String>,
    pub size_bytes: i64,
    pub file_count: i64,
    /// On-disk yes/no — the dense table's at-a-glance availability column.
    pub on_disk: bool,
    /// SEAM: cutoff-met requires a quality sort-order comparison the read
    /// layer doesn't compute yet (needs the profile's cutoff tier vs. the
    /// best on-disk file's tier) — surfaced as `null` (honest unknown), not
    /// a fabricated bool.
    pub cutoff_met: Option<bool>,
    pub poster_url: String,
}

/// `GET /api/library/table` — the dense management table (on-disk footprint,
/// file count, quality profile). Fail-open.
pub async fn get_library_table(
    State(state): State<Arc<AppState>>,
    Query(q): Query<LibraryQuery>,
) -> Json<Vec<TableRow>> {
    let limit = q.limit.unwrap_or(DEFAULT_TABLE_LIMIT).clamp(1, 5000);
    let rows = repo::dashboard::library_table(&state.pool, limit)
        .await
        .unwrap_or_else(|e| {
            tracing::warn!(error = %e, "get_library_table: query failed; serving empty");
            Vec::new()
        });

    let out = rows
        .into_iter()
        .map(|r| TableRow {
            media_item_id: r.media_item_id,
            media_metadata_id: r.media_metadata_id,
            kind: kind_str(r.kind),
            title: r.title,
            year: r.year,
            monitored: r.monitored,
            quality_profile_id: r.quality_profile_id,
            quality_profile_name: r.quality_profile_name,
            size_bytes: r.size_bytes,
            file_count: r.file_count,
            on_disk: r.file_count > 0,
            cutoff_met: None,
            poster_url: poster_url(r.media_metadata_id),
        })
        .collect();

    Json(out)
}

/// `GET /api/library/:id` — full media detail for one `media_item`: shared
/// metadata, on-disk files, and external enrichment. `:id` is the
/// `media_item` id. A not-found id degrades to a seam body (found=false)
/// rather than a 404, so the UI can render an honest empty detail pane.
pub async fn get_library_detail(
    State(state): State<Arc<AppState>>,
    Path(id): Path<i64>,
) -> Json<Value> {
    let item = match repo::media_item::get(&state.pool, id).await {
        Ok(item) => item,
        Err(_) => {
            return Json(json!({ "found": false, "media_item_id": id }));
        }
    };

    let metadata = repo::media_metadata::get(&state.pool, item.media_metadata_id)
        .await
        .ok();
    let files = repo::media_file::list_by_media_item(&state.pool, id)
        .await
        .unwrap_or_default();
    let enrichment = repo::external_enrichment::list_for_media_item(&state.pool, id)
        .await
        .unwrap_or_default();

    Json(json!({
        "found": true,
        "media_item": item,
        "metadata": metadata,
        "poster_url": poster_url(item.media_metadata_id),
        "backdrop_url": backdrop_url(item.media_metadata_id),
        "files": files,
        "enrichment": enrichment,
        // SEAM: the MUSEL still-frame matching verification is verdict-only
        // (not persisted to a table) — surfaced as null until a match-verdict
        // store exists, never a fabricated verdict.
        "match_verdict": Value::Null,
    }))
}

// ===========================================================================
// Discover
// ===========================================================================

#[derive(Debug, Clone, Deserialize)]
pub struct DiscoverQuery {
    pub region: Option<String>,
    pub limit: Option<i64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DiscoverItem {
    pub media_metadata_id: i64,
    pub kind: &'static str,
    pub title: String,
    pub year: Option<i32>,
    pub popularity: Option<f32>,
    pub poster_url: String,
    pub backdrop_url: String,
}

/// `GET /api/discover` — TMDb trending/popular titles not already in the
/// library (read-only browse; the request/grab action stays gated in
/// `crate::http::requests`). Seam-empty when TMDb isn't configured (no
/// trending snapshots) — `configured` tells the UI which it is.
pub async fn get_discover(
    State(state): State<Arc<AppState>>,
    Query(q): Query<DiscoverQuery>,
) -> Json<Value> {
    let region = q.region.unwrap_or_else(|| DEFAULT_REGION.to_string());
    let limit = q.limit.unwrap_or(DEFAULT_DISCOVER_LIMIT).clamp(1, 200);

    let rows = repo::trending::list_trending_not_in_library(&state.pool, &region, limit)
        .await
        .unwrap_or_else(|e| {
            tracing::warn!(error = %e, "get_discover: trending query failed; serving empty");
            Vec::new()
        });

    let items: Vec<DiscoverItem> = rows
        .into_iter()
        .map(|r| DiscoverItem {
            media_metadata_id: r.media_metadata_id,
            kind: kind_str(r.kind),
            title: r.title,
            year: r.year,
            popularity: r.popularity,
            poster_url: poster_url(r.media_metadata_id),
            backdrop_url: backdrop_url(r.media_metadata_id),
        })
        .collect();

    Json(json!({
        "region": region,
        "configured": state.tmdb.is_some(),
        "items": items,
    }))
}

// ===========================================================================
// Requests
// ===========================================================================

/// `GET /api/requests` — the request list with lifecycle status + safety
/// tier. Read-only (the write/approve/grab path is `crate::http::requests`).
pub async fn get_requests(State(state): State<Arc<AppState>>) -> Json<Value> {
    let statuses = [
        "requested",
        "approved",
        "denied",
        "searching",
        "grabbed",
        "available",
        "failed",
    ];
    let mut all = Vec::new();
    for status in statuses {
        match repo::acquisition::list_requests_by_status(&state.pool, status).await {
            Ok(rows) => all.extend(rows),
            Err(e) => {
                tracing::warn!(error = %e, status, "get_requests: list failed; skipping status")
            }
        }
    }
    all.sort_by(|a, b| b.created_at.cmp(&a.created_at));

    // Tier rollup (the "tiers" the UI groups by) — honest counts from the
    // persisted `tier` column.
    let mut tier_counts = serde_json::Map::new();
    for r in &all {
        let key = r.tier.clone().unwrap_or_else(|| "unclassified".to_string());
        let entry = tier_counts.entry(key).or_insert_with(|| json!(0));
        if let Some(n) = entry.as_i64() {
            *entry = json!(n + 1);
        }
    }

    Json(json!({
        "requests": all,
        "tiers": tier_counts,
        "total": all.len(),
    }))
}

/// `GET /api/requests/:id` — one request's lifecycle-stepper state: the
/// current status plus the ordered steps a <media-service>-style request moves
/// through, each marked reached/current/pending from the row's real status.
pub async fn get_request_detail(
    State(state): State<Arc<AppState>>,
    Path(id): Path<i64>,
) -> Json<Value> {
    let request = match repo::acquisition::get_request(&state.pool, id).await {
        Ok(r) => r,
        Err(_) => return Json(json!({ "found": false, "request_id": id })),
    };

    // The canonical happy-path order; a terminal Denied/Failed is surfaced
    // as the current step without inventing intermediate ones.
    let order = ["requested", "approved", "searching", "grabbed", "available"];
    let status = request.status.as_str();
    let current_idx = order.iter().position(|s| *s == status);

    let steps: Vec<Value> = order
        .iter()
        .enumerate()
        .map(|(i, label)| {
            let state = match current_idx {
                Some(ci) if i < ci => "reached",
                Some(ci) if i == ci => "current",
                _ => "pending",
            };
            json!({ "label": label, "state": state })
        })
        .collect();

    let terminal = match status {
        "denied" => Some("denied"),
        "failed" => Some("failed"),
        _ => None,
    };

    Json(json!({
        "found": true,
        "request": request,
        "status": status,
        "steps": steps,
        "terminal": terminal,
    }))
}

/// `GET /api/requests/queue` — the wanted set + the active download queue.
/// Download **progress %** is a SEAM (qBittorrent per-torrent progress isn't
/// persisted on `download_queue`), so each queue entry carries its real
/// lifecycle status + size but a `null` progress, honestly.
pub async fn get_requests_queue(State(state): State<Arc<AppState>>) -> Json<Value> {
    let wanted = repo::dashboard::wanted_titles(&state.pool, 500)
        .await
        .unwrap_or_default();

    let mut queue = Vec::new();
    for status in ["queued", "downloading", "completed", "importing"] {
        match repo::acquisition::list_download_queue_by_status(&state.pool, status).await {
            Ok(rows) => queue.extend(rows),
            Err(e) => tracing::warn!(error = %e, status, "get_requests_queue: queue list failed"),
        }
    }

    let wanted_json: Vec<Value> = wanted
        .into_iter()
        .map(|w| {
            json!({
                "monitored_item_id": w.monitored_item_id,
                "media_metadata_id": w.media_metadata_id,
                "library_id": w.library_id,
                "kind": kind_str(w.kind),
                "title": w.title,
                "year": w.year,
                "poster_url": poster_url(w.media_metadata_id),
            })
        })
        .collect();

    let queue_json: Vec<Value> = queue
        .into_iter()
        .map(|e| {
            json!({
                "id": e.id,
                "request_id": e.request_id,
                "monitored_item_id": e.monitored_item_id,
                "release_title": e.release_title,
                "indexer": e.indexer,
                "protocol": e.protocol,
                "status": e.status,
                "size_bytes": e.size_bytes,
                "added_at": e.added_at,
                // SEAM: real download %/ETA not persisted (see doc comment).
                "progress": Value::Null,
            })
        })
        .collect();

    Json(json!({
        "wanted": wanted_json,
        "queue": queue_json,
    }))
}

// ===========================================================================
// Taste
// ===========================================================================

#[derive(Debug, Clone, Deserialize)]
pub struct AccountQuery {
    pub account_id: Option<i64>,
    pub limit: Option<i64>,
    pub region: Option<String>,
}

/// `GET /api/taste` — the taste snapshot: genre-lean + decade-lean weights,
/// context centroids, and the MUSE-13 divergence/radar signals. Seam-empty
/// (`has_data: false`) for an account with no computed profile/radar yet.
pub async fn get_taste(
    State(state): State<Arc<AppState>>,
    Query(q): Query<AccountQuery>,
) -> Json<Value> {
    let Some(account_id) = resolve_account_id(&state, q.account_id).await else {
        return Json(json!({ "has_data": false, "account_id": Value::Null }));
    };

    let profile = repo::taste::get_profile(&state.pool, account_id)
        .await
        .ok()
        .flatten();
    let divergence = repo::taste_divergence::latest_divergence(&state.pool, account_id)
        .await
        .ok()
        .flatten();
    let genre_weights = repo::taste_divergence::account_genre_weights(&state.pool, account_id)
        .await
        .unwrap_or_default();
    let decade_weights = repo::taste_divergence::account_decade_weights(&state.pool, account_id)
        .await
        .unwrap_or_default();
    let centroids = repo::taste::list_context_centroids(&state.pool, account_id)
        .await
        .unwrap_or_default();

    let has_data = profile.is_some()
        || divergence.is_some()
        || !genre_weights.is_empty()
        || !decade_weights.is_empty();

    let genre_lean: Vec<Value> = genre_weights
        .into_iter()
        .map(|g| json!({ "genre": g.genre, "weight": g.weight }))
        .collect();
    let decade_lean: Vec<Value> = decade_weights
        .into_iter()
        .map(|d| json!({ "decade": d.decade, "weight": d.weight }))
        .collect();
    let centroid_summary: Vec<Value> = centroids
        .into_iter()
        .map(|c| json!({ "context_key": c.context_key, "sample_size": c.sample_size }))
        .collect();

    let divergence_json = divergence.map(|d| {
        json!({
            "mainstream_score": d.mainstream_score,
            "adventurousness": d.adventurousness,
            "contrarian_index": d.contrarian_index,
            "blind_spots": d.blind_spots,
            "guilty_pleasures": d.guilty_pleasures,
            "computed_at": d.computed_at,
        })
    });

    let profile_json = profile.map(|p| {
        json!({
            "genre_affinity": p.genre_affinity,
            "person_affinity": p.person_affinity,
            "keyword_affinity": p.keyword_affinity,
            "computed_at": p.computed_at,
        })
    });

    Json(json!({
        "has_data": has_data,
        "account_id": account_id,
        "genre_lean": genre_lean,
        "decade_lean": decade_lean,
        "centroids": centroid_summary,
        "divergence": divergence_json,
        "profile": profile_json,
    }))
}

// ===========================================================================
// Curation
// ===========================================================================

/// `GET /api/curation` — ranked recommendations for an account, reusing the
/// MUSE-11 candidate gatherers (taste-fit + gap + trending-available),
/// de-duped and sorted by taste fit. Every rec carries its GROUNDED facts
/// (never invented). Seam-empty for a cold-start account.
pub async fn get_curation(
    State(state): State<Arc<AppState>>,
    Query(q): Query<AccountQuery>,
) -> Json<Value> {
    let limit = q.limit.unwrap_or(DEFAULT_CURATION_LIMIT).clamp(1, 100);
    let region = q.region.unwrap_or_else(|| DEFAULT_REGION.to_string());

    let Some(account_id) = resolve_account_id(&state, q.account_id).await else {
        return Json(json!({ "account_id": Value::Null, "recommendations": [] }));
    };

    let mut candidates = Vec::new();
    match crate::curation::candidates::gather_taste_candidates(&state.pool, account_id, limit).await
    {
        Ok(c) => candidates.extend(c),
        Err(e) => tracing::warn!(error = %e, "get_curation: taste candidates failed"),
    }
    match crate::curation::candidates::gather_gap_candidates(&state.pool, account_id, limit).await {
        Ok(c) => candidates.extend(c),
        Err(e) => tracing::warn!(error = %e, "get_curation: gap candidates failed"),
    }
    match crate::curation::candidates::gather_available_now_candidates(&state.pool, &region, limit)
        .await
    {
        Ok(c) => candidates.extend(c),
        Err(e) => tracing::warn!(error = %e, "get_curation: available-now candidates failed"),
    }

    let mut deduped = crate::curation::candidates::dedup_candidates(candidates);
    deduped.sort_by(|a, b| {
        b.taste_fit
            .partial_cmp(&a.taste_fit)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    deduped.truncate(limit as usize);

    let recommendations: Vec<Value> = deduped
        .into_iter()
        .map(|c| {
            let source = match c.source {
                crate::curation::candidates::CandidateSource::Taste => "taste",
                crate::curation::candidates::CandidateSource::OnDeck => "on_deck",
                crate::curation::candidates::CandidateSource::Gap => "gap",
                crate::curation::candidates::CandidateSource::AvailableNow => "available_now",
            };
            let grabbable = c
                .availability
                .as_ref()
                .map(|a| a.release_count > 0)
                .unwrap_or(false);
            json!({
                "media_metadata_id": c.media_metadata_id,
                "media_item_id": c.media_item_id,
                "title": c.title,
                "year": c.year,
                "kind": kind_str(c.kind),
                "source": source,
                "taste_fit": c.taste_fit,
                "facts": c.facts,
                "grabbable": grabbable,
                "poster_url": poster_url(c.media_metadata_id),
            })
        })
        .collect();

    Json(json!({
        "account_id": account_id,
        "recommendations": recommendations,
    }))
}

// ===========================================================================
// Subsystems (the dashboard health grid)
// ===========================================================================

/// `GET /api/subsystems` — the module registry that drives the dashboard
/// health grid. State per subsystem is derived HONESTLY from what's actually
/// wired at runtime (config/`AppState` presence + a cheap data probe for the
/// library), mapping to the UI's four `WiringStatusPill` states:
/// `live` (wired + has data), `worker` (wired, background/on-demand),
/// `seam` (implemented, not yet producing data), `unmounted` (not configured).
pub async fn get_subsystems(State(state): State<Arc<AppState>>) -> Json<Value> {
    // Cheap data probe: does the library have any scanned content?
    let counts = repo::dashboard::library_counts(&state.pool).await.ok();
    let has_library = counts.as_ref().map(|c| c.owned > 0).unwrap_or(false);
    let has_on_disk = counts.as_ref().map(|c| c.on_disk > 0).unwrap_or(false);

    let library_state = match (&state.config.library_root, has_on_disk, has_library) {
        (None, _, _) => "unmounted",
        (Some(_), true, _) => "live",
        (Some(_), false, true) => "worker",
        (Some(_), false, false) => "seam",
    };

    let tvdb_configured = crate::metadata::tvdb::TvdbClient::from_config(&state.config).is_some();
    let providers_state = if state.tmdb.is_some() || tvdb_configured {
        "live"
    } else {
        "unmounted"
    };

    let prowlarr_state = if state.prowlarr.is_some() {
        "worker"
    } else {
        "unmounted"
    };
    let acquisition_state = if state.prowlarr.is_some() && state.download.is_some() {
        "worker"
    } else if state.prowlarr.is_some() {
        "seam"
    } else {
        "unmounted"
    };
    let discover_state = if state.tmdb.is_some() {
        "worker"
    } else {
        "unmounted"
    };
    let taste_state = if state.embed.is_some() {
        "worker"
    } else {
        "seam"
    };

    let subsystems = json!([
        {
            "key": "library_scan",
            "label": "Library Scan",
            "state": library_state,
            "concern": "Read-only QNAP scan → media_items/media_files. Needs MUSE_LIBRARY_ROOT + a mounted library to go Live."
        },
        {
            "key": "metadata_providers",
            "label": "Metadata Providers",
            "state": providers_state,
            "concern": "TVDB v4 / TMDb / IMDb enrichment feeding titles + artwork."
        },
        {
            "key": "artwork",
            "label": "Artwork Proxy",
            "state": if has_library { "live" } else { "seam" },
            "concern": "/art proxy serves posters from sidecar cache or provider images; caches on-disk."
        },
        {
            "key": "discover",
            "label": "Discover / Trending",
            "state": discover_state,
            "concern": "TMDb trending browse (read-only). Request/grab stays gated."
        },
        {
            "key": "prowlarr",
            "label": "Prowlarr Indexers",
            "state": prowlarr_state,
            "concern": "Indexer registry + RSS release reports (read-only). Needs PROWLARR_URL/API key."
        },
        {
            "key": "acquisition",
            "label": "Acquisition",
            "state": acquisition_state,
            "concern": "Grab path behind the dual-safety gate (default OFF). Needs Prowlarr + a download client."
        },
        {
            "key": "taste_model",
            "label": "Taste Model",
            "state": taste_state,
            "concern": "Embeddings + centroid taste profile. Needs a Chord embed backend for Live."
        },
        {
            "key": "curation",
            "label": "Curation",
            "state": if has_library { "worker" } else { "seam" },
            "concern": "Ranked recs from taste/gap/trending candidates, grounded in real signals."
        },
        {
            "key": "requests",
            "label": "Requests",
            "state": acquisition_state,
            "concern": "<media-service>-style request lifecycle. Read here; write/approve gated."
        }
    ]);

    Json(json!({ "subsystems": subsystems }))
}

// ===========================================================================
// Prowlarr indexers + RSS
// ===========================================================================

/// `GET /api/indexers` — the Prowlarr indexer registry + per-indexer health
/// (the way Sonarr/Radarr surface Prowlarr). Read-only. Seam-empty when
/// Prowlarr isn't configured (`configured: false`) or is unreachable
/// (`reachable: false`) — never a 500.
pub async fn get_indexers(State(state): State<Arc<AppState>>) -> Json<Value> {
    let Some(client) = state.prowlarr.as_ref() else {
        return Json(json!({ "configured": false, "reachable": false, "indexers": [] }));
    };

    match client.indexers().await {
        Ok(indexers) => {
            let list: Vec<Value> = indexers
                .into_iter()
                .map(|i| {
                    json!({
                        "id": i.id,
                        "name": i.name,
                        "protocol": i.protocol,
                        "privacy": i.privacy,
                        "enabled": i.enable,
                        "categories": i.category_ids(),
                    })
                })
                .collect();
            Json(json!({ "configured": true, "reachable": true, "indexers": list }))
        }
        Err(e) => {
            tracing::warn!(error = %e, "get_indexers: prowlarr unreachable; serving seam");
            Json(json!({ "configured": true, "reachable": false, "indexers": [] }))
        }
    }
}

/// `GET /api/indexers/rss` (alias `GET /api/rss`) — recent RSS release
/// reports pulled across enabled indexers (the *arr "RSS sync" analog),
/// newest first. READ-ONLY: reporting only, never a grab. Seam-empty when
/// Prowlarr isn't configured. Per-indexer rate-limit conflicts are skipped
/// (not retried), never surfaced as errors.
pub async fn get_rss(State(state): State<Arc<AppState>>) -> Json<Value> {
    let Some(client) = state.prowlarr.as_ref() else {
        return Json(json!({ "configured": false, "indexers_polled": 0, "releases": [] }));
    };

    let indexers = match client.indexers().await {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(error = %e, "get_rss: prowlarr indexer list unreachable; serving seam");
            return Json(json!({ "configured": true, "indexers_polled": 0, "releases": [] }));
        }
    };

    let mut releases = Vec::new();
    let mut polled = 0usize;
    for indexer in indexers
        .into_iter()
        .filter(|i| i.enable)
        .take(RSS_MAX_INDEXERS)
    {
        // No category filter → the indexer's default recent feed. A short
        // min-interval; a Conflict (polled too recently) is a clean skip.
        match client
            .rss_pull(indexer.id, &[], Duration::from_secs(2))
            .await
        {
            Ok(rows) => {
                polled += 1;
                for r in rows {
                    releases.push(json!({
                        "guid": r.guid,
                        "title": r.title,
                        "indexer_id": r.indexer_id,
                        "indexer": r.indexer,
                        "protocol": r.protocol,
                        "size": r.size,
                        "publish_date": r.publish_date,
                        "seeders": r.seeders,
                        "leechers": r.leechers,
                        "grabs": r.grabs,
                        "info_url": r.info_url,
                    }));
                }
            }
            Err(e) => {
                tracing::debug!(error = %e, indexer = %indexer.name, "get_rss: skipping indexer (rate-limited or errored)");
            }
        }
    }

    // Newest first, then cap.
    releases.sort_by(|a, b| {
        let ka = a.get("publish_date").and_then(|v| v.as_str()).unwrap_or("");
        let kb = b.get("publish_date").and_then(|v| v.as_str()).unwrap_or("");
        kb.cmp(ka)
    });
    releases.truncate(RSS_MAX_RELEASES);

    Json(json!({
        "configured": true,
        "indexers_polled": polled,
        "releases": releases,
    }))
}

/// Process-global in-flight flag for the library scan (MWEBX-05 review,
/// codex High): a scan is a bounded, resource-heavy pass (walks the RO mount,
/// upserts catalog rows, caches artwork) — two of them stacking wastes work
/// and races the same rows. This flag makes the trigger single-flight: a
/// second call while one is running is rejected `409` rather than launching a
/// parallel scan. Process-global (single-process service), reset on the
/// [`ScanInFlightGuard`]'s `Drop` so it clears even if `run_scan` errors or
/// panics.
static SCAN_IN_FLIGHT: AtomicBool = AtomicBool::new(false);

/// RAII release for [`SCAN_IN_FLIGHT`] — clears the flag on drop (normal
/// return, `?`-propagated error, or unwind), so a failed scan never wedges
/// the trigger permanently.
struct ScanInFlightGuard;

impl Drop for ScanInFlightGuard {
    fn drop(&mut self) {
        SCAN_IN_FLIGHT.store(false, Ordering::Release);
    }
}

/// `POST /ops/library/scan` — trigger a library scan on demand (MUSEL-B1
/// `run_scan`), building the same metadata providers the maintenance pass
/// uses. Clean no-op (empty reports) when `MUSE_LIBRARY_ROOT` is unset / the
/// mount is absent. Returns the aggregated scan counts. This is the "refresh
/// the library" door the web Library screen calls before re-reading
/// `/api/library`.
///
/// ## Posture (MWEBX-05 review, codex High): guarded ops action, not a public POST
/// A library SCAN is a **read-only-of-MEDIA cataloging** operation: it reads
/// the read-only QNAP mount and writes ONLY Muse's own internal catalog
/// tables (`media_items`/`media_files`/`media_metadata`) + the artwork cache.
/// It is NOT an acquisition/grab write, so it is correctly NOT behind the
/// dual-safety gate — that gate governs downloads/grabs, a different and
/// dangerous class of write. The right guard for a cataloging write is
/// **Bearer auth + single-flight**, which is exactly what protects it:
/// - Bearer auth: this route is nested under `/ops`, which
///   `crate::http::router` mounts on the `protected` sub-router behind
///   `auth::require_api_token` — there is no public trigger (a tokenless call
///   is rejected before this handler runs).
/// - Single-flight: [`SCAN_IN_FLIGHT`] rejects a concurrent call with `409`
///   so scans can't stack/spam (the DoS-via-repeated-scan vector the review
///   flagged).
pub async fn trigger_library_scan(State(state): State<Arc<AppState>>) -> MuseResult<Json<Value>> {
    // Acquire the single-flight slot; a second concurrent scan is a clean
    // 409, never a parallel launch.
    if SCAN_IN_FLIGHT
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return Err(MuseError::Conflict(
            "a library scan is already running; try again once it finishes".to_string(),
        ));
    }
    let _in_flight = ScanInFlightGuard;

    let tvdb = crate::metadata::tvdb::TvdbClient::from_config(&state.config);
    let mut providers: Vec<crate::metadata::resolve::NamedProvider<'_>> = Vec::new();
    if let Some(tmdb) = state.tmdb.as_ref() {
        providers.push(crate::metadata::resolve::NamedProvider::new(
            crate::metadata::resolve::TMDB,
            tmdb,
        ));
    }
    if let Some(tvdb) = tvdb.as_ref() {
        providers.push(crate::metadata::resolve::NamedProvider::new(
            crate::metadata::resolve::TVDB,
            tvdb,
        ));
    }

    let reports = crate::library::scan::run_scan(&state.pool, &state.config, &providers).await?;

    let mut scanned = 0usize;
    let mut matched = 0usize;
    let mut tentative = 0usize;
    let mut unmatched = 0usize;
    let mut errors = 0usize;
    let mut art_cached = 0usize;
    for r in &reports {
        scanned += r.scanned;
        matched += r.matched;
        tentative += r.tentative;
        unmatched += r.unmatched;
        errors += r.errors;
        art_cached += r.art_cached;
    }

    Ok(Json(json!({
        "libraries_scanned": reports.len(),
        "library_root_configured": state.config.library_root.is_some(),
        "scanned": scanned,
        "matched": matched,
        "tentative": tentative,
        "unmatched": unmatched,
        "errors": errors,
        "art_cached": art_cached,
    })))
}

// ===========================================================================
// Constellation web GUI dashboard (MUSE #84)
// ===========================================================================
//
// The four endpoints `Terminus/constellation-web/src/hooks/useMuse.ts` binds
// its Muse dashboard sections to. That hook file is the CONTRACT: every field
// name and JSON type below is copied from its `MuseStats`/`MuseOnDeck`/
// `MuseGaps`/`MusePremiere` interfaces, including the detail that every `id`
// is a STRING there, not a number — hence the `.to_string()`s.
//
// ## Why these degrade differently from the rest of this module
// `useMuseSection` treats a `404`/`501` as "not yet wired" and a `null` body
// as the same thing, but renders any 2xx body AS DATA. So an honest empty
// list is NOT interchangeable with an error here: returning `{"items":[]}`
// asserts "nothing is on deck", which must only be said when that is true.
// Each handler below therefore states which of the two it does and why.

#[derive(Debug, Clone, Serialize)]
pub struct MuseStatsResponse {
    pub library_size: i64,
    pub active_channels: i64,
    pub pending_items: i64,
    pub last_ingest_at: Option<String>,
}

/// `GET /stats` — the dashboard header scalars (`useMuseStats`).
///
/// PUBLIC (unauthenticated) by deliberate choice: these are four
/// whole-library aggregates with no per-account component, the same class of
/// data `/api/library`'s `counts` block already serves publicly. Nothing here
/// discloses who watched what.
///
/// ## Query errors propagate — this does NOT fail open
/// The rest of this module fails open to an empty/zero `200`, a posture built
/// for *seam* conditions (an unmounted library, an unconfigured provider) —
/// cases where "nothing" is the true answer. A failed COUNT is not that. And
/// because `useMuseSection` renders any 2xx body AS DATA, serving zeros here
/// would put "0 items in library" on the operator's dashboard whenever a query
/// transiently failed — the exact category of confident-but-false claim that
/// `get_premiere`'s `501` exists to avoid. Returning the error lets the card
/// degrade instead. (Raised by the MUSE #84 review panel: the fail-open paths
/// contradicted the reasoning used to justify that `501`.)
pub async fn get_stats(State(state): State<Arc<AppState>>) -> MuseResult<Json<MuseStatsResponse>> {
    let s = repo::dashboard::constellation_stats(&state.pool).await?;
    Ok(Json(MuseStatsResponse {
        library_size: s.library_size,
        active_channels: s.active_channels,
        pending_items: s.pending_items,
        last_ingest_at: s.last_ingest_at.map(|t| t.to_rfc3339()),
    }))
}

#[derive(Debug, Clone, Serialize)]
pub struct MuseOnDeckItem {
    pub id: String,
    pub title: String,
    pub kind: &'static str,
    pub progress_pct: f32,
    pub poster_path: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct MuseOnDeckResponse {
    pub items: Vec<MuseOnDeckItem>,
}

/// `GET /on_deck` — continue-watching (`useMuseOnDeck`).
///
/// PROTECTED: this is per-account viewing history (MUSEX-CAP-SEC-03), the
/// exact data `/api/taste` and `/api/curation` are gated for. It must never
/// move to the public router.
///
/// Account selection reuses this module's existing `resolve_account_id`
/// convention (explicit `?account_id=` → primary → first → none), so the
/// Taste, Curation and On-Deck screens all agree about whose data an
/// un-parameterised request means. A cold-start deployment with no accounts
/// yields an empty list, which is honest: with no account there is no queue.
///
/// ## On `?account_id=` not being authorization-checked
/// The MUSE #84 review panel flagged that a caller past the protected router
/// can name any `account_id`. That is accurate, and it is a property of the
/// auth model rather than of this handler: `http::auth::require_api_token`
/// compares a single shared `MUSE_API_TOKEN` in constant time and establishes
/// **no per-user identity at all**. There is exactly one principal — the
/// operator — and they are already authorized for every household account
/// (`/api/taste?account_id=N` and `/api/curation?account_id=N` have had the
/// same shape since MWEBX-05). So there is no per-user boundary here to
/// cross, and adding an `account_id`-vs-caller check would be theatre: there
/// is no caller identity to compare against.
///
/// Introducing real per-account authorization means giving Muse per-user
/// principals (sessions/JWT scoped to a `muse_account_id`) — a genuine auth
/// change across every per-account endpoint, not something to bolt onto one
/// dashboard read. Until then, the enforcement boundary is "holds the
/// operator token", which is what the protected router expresses.
pub async fn get_on_deck(
    State(state): State<Arc<AppState>>,
    Query(q): Query<AccountQuery>,
) -> MuseResult<Json<MuseOnDeckResponse>> {
    // `resolve_account_id_fallible`, NOT `resolve_account_id`: the latter
    // folds a failed accounts query into `None`, which would land in the
    // empty-list branch below and re-create the exact false-2xx this endpoint
    // was fixed to avoid.
    let Some(account_id) = resolve_account_id_fallible(&state, q.account_id).await? else {
        // A genuinely-true empty: the query succeeded and there are no
        // accounts, so there is no queue.
        return Ok(Json(MuseOnDeckResponse { items: Vec::new() }));
    };
    let rows = repo::dashboard::on_deck(&state.pool, account_id, ON_DECK_LIMIT).await?;
    Ok(Json(MuseOnDeckResponse {
        items: rows
            .into_iter()
            .map(|r| MuseOnDeckItem {
                id: r.media_item_id.to_string(),
                title: r.title,
                kind: kind_str(r.kind),
                // The repo filters `percent_complete` to a non-null 0<x<100,
                // so the fallback is unreachable; it is a `0.0` rather than
                // an `unwrap()` so a future filter change can never panic a
                // dashboard request.
                progress_pct: r.percent_complete.unwrap_or(0.0),
                // `/art/media_item/{id}` — deliberately keyed on the SAME id
                // this item reports, so a client can build the art URL from
                // `id` alone. (`poster_url()`'s `/art/media_metadata/{id}`
                // form is equally valid but keyed on a different id, which
                // would silently invite an `id`/art-id mix-up.)
                poster_path: format!("/art/media_item/{}", r.media_item_id),
            })
            .collect(),
    }))
}

#[derive(Debug, Clone, Serialize)]
pub struct MuseGapItem {
    pub id: String,
    pub title: String,
    pub kind: &'static str,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct MuseGapsResponse {
    pub gaps: Vec<MuseGapItem>,
    pub total: i64,
}

/// `GET /gaps` — monitored-but-absent titles (`useMuseGaps`).
///
/// PUBLIC, for the same reason as `/stats`: "this title is monitored and has
/// no file" is a property of the library, not of a person. It is the same
/// `wanted_titles` projection `/api/library` already returns publicly.
///
/// `total` is the UNTRUNCATED count from `constellation_stats`, not
/// `gaps.len()` — the list is capped at `GAPS_LIMIT`, and reporting the
/// capped length as the total would silently under-report the backlog.
///
/// Query errors propagate rather than failing open to an empty list, for the
/// reason spelled out on `get_stats`: a 2xx empty body renders as "no gaps",
/// which is a claim, not an absence of one.
pub async fn get_gaps(State(state): State<Arc<AppState>>) -> MuseResult<Json<MuseGapsResponse>> {
    let rows = repo::dashboard::wanted_titles(&state.pool, GAPS_LIMIT).await?;
    // `total` comes from a second query; a failure there is still a failure —
    // the previous fallback silently substituted the truncated visible length
    // for the real backlog size, which is exactly the quiet-wrong-number
    // problem an untruncated `total` exists to prevent.
    let total = repo::dashboard::constellation_stats(&state.pool)
        .await?
        .pending_items;
    Ok(Json(MuseGapsResponse {
        gaps: rows
            .into_iter()
            .map(|r| MuseGapItem {
                id: r.monitored_item_id.to_string(),
                detail: match r.year {
                    Some(y) => format!("{y} · monitored, no file on disk"),
                    None => "monitored, no file on disk".to_string(),
                },
                title: r.title,
                kind: kind_str(r.kind),
            })
            .collect(),
        total,
    }))
}

/// The shape `/premiere` will return once premiere events are persisted.
/// Retained (unconstructed) as the machine-checked record of the contract
/// `useMusePremiere` expects, so MUSE #86 has a target rather than having to
/// re-derive it from the hook file.
#[allow(dead_code)]
#[derive(Debug, Clone, Serialize)]
pub struct MusePremiereItem {
    pub id: String,
    pub title: String,
    pub release_date: String,
    pub rsvp_count: i64,
}

/// See [`MusePremiereItem`] — retained as the MUSE #86 target shape.
#[allow(dead_code)]
#[derive(Debug, Clone, Serialize)]
pub struct MusePremiereResponse {
    pub items: Vec<MusePremiereItem>,
}

/// `GET /premiere` — scheduled premiere events (`useMusePremiere`).
///
/// ## This returns `501 Not Implemented`, and that is the honest answer
/// `premiere::schedule::PremiereEvent` is an **in-memory** value. There is no
/// premiere-events table and no RSVP table: migration
/// `0101_premiere_discussion_threads.sql` persists discussion threads and
/// posts ONLY, with no `scheduled_at` and no RSVP rows. So a scheduled
/// premiere does not survive the process that scheduled it, and there is
/// nothing durable to enumerate.
///
/// Returning `{"items":[]}` here would be a lie of exactly the kind the
/// module doc warns about — `useMuseSection` renders a 2xx body as data, so
/// an empty list would assert "no premieres are scheduled" when the truth is
/// "premieres cannot be recorded yet". A `501` is the one status the hook
/// already classifies as `not yet wired`, which is precisely the fact. The
/// GUI renders its standard degraded card and no one is misled.
///
/// Wiring this for real needs a persistence layer (premiere events + RSVPs)
/// — tracked separately as MUSE #86, deliberately NOT bolted on here.
/// The `501` is returned directly rather than via `MuseError::NotImplemented`
/// (a unit variant whose message is the fixed string "not implemented yet")
/// so an operator running `curl` gets the actual reason, without widening a
/// shared error enum for one endpoint.
pub async fn get_premiere() -> (StatusCode, Json<Value>) {
    (
        StatusCode::NOT_IMPLEMENTED,
        Json(json!({
            "error": "premiere events are not persisted yet",
            "detail": "PremiereEvent is in-memory only; there are no premiere-event or RSVP \
                       tables (0101 persists discussion threads/posts only), so there is \
                       nothing durable to list. Returning an empty list would falsely assert \
                       that no premieres are scheduled.",
            "tracked_as": "MUSE #86",
        })),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn poster_and_backdrop_urls_are_same_origin_proxy_paths() {
        assert_eq!(poster_url(42), "/art/media_metadata/42");
        assert_eq!(backdrop_url(42), "/art/media_metadata/42?variant=fanart");
    }

    #[test]
    fn kind_str_maps_both_kinds() {
        assert_eq!(kind_str(MediaKind::Movie), "movie");
        assert_eq!(kind_str(MediaKind::Show), "show");
    }

    // ── MUSE #84: Constellation dashboard contract ──────────────────────────
    //
    // These pin the SERIALIZED KEY NAMES AND JSON TYPES against
    // `Terminus/constellation-web/src/hooks/useMuse.ts`. This is not
    // ceremony: a field-name or number-vs-string mismatch here does not fail
    // any request — `useMuseSection` degrades only on a 404/501/null, so a
    // 200 carrying the wrong keys renders as a populated-looking card with
    // blank values. That silent-wrong-shape failure is exactly how the Muse
    // panel came to show empty cards, so the shape is asserted, not assumed.

    #[test]
    fn stats_response_matches_the_use_muse_stats_interface() {
        let json = serde_json::to_value(MuseStatsResponse {
            library_size: 1892,
            active_channels: 0,
            pending_items: 3,
            last_ingest_at: Some("2026-07-25T17:55:53+00:00".to_string()),
        })
        .unwrap();
        // Exactly these four keys, no more (an extra key is harmless to the
        // GUI but signals the contract drifted without the hook being updated).
        let obj = json.as_object().unwrap();
        let mut keys: Vec<&str> = obj.keys().map(String::as_str).collect();
        keys.sort();
        assert_eq!(
            keys,
            ["active_channels", "last_ingest_at", "library_size", "pending_items"]
        );
        assert!(json["library_size"].is_i64(), "MuseStats.library_size is number");
        assert!(json["last_ingest_at"].is_string());
    }

    #[test]
    fn stats_last_ingest_at_is_null_not_absent_when_the_library_is_empty() {
        // `MuseStats.last_ingest_at` is `string | null` — a MISSING key would
        // deserialize as `undefined` in TS and print "undefined" in the card.
        let json = serde_json::to_value(MuseStatsResponse {
            library_size: 0,
            active_channels: 0,
            pending_items: 0,
            last_ingest_at: None,
        })
        .unwrap();
        assert!(json.as_object().unwrap().contains_key("last_ingest_at"));
        assert!(json["last_ingest_at"].is_null());
    }

    #[test]
    fn on_deck_item_ids_serialize_as_strings_and_progress_as_a_number() {
        let json = serde_json::to_value(MuseOnDeckResponse {
            items: vec![MuseOnDeckItem {
                id: "77".to_string(),
                title: "Example Feature Film".to_string(),
                kind: "movie",
                progress_pct: 41.5,
                poster_path: "/art/media_item/77".to_string(),
            }],
        })
        .unwrap();
        let item = &json["items"][0];
        // `MuseOnDeckItem.id: string` — a bare number here would break
        // `museArtUrl(..., item.id)`'s encodeURIComponent contract.
        assert!(item["id"].is_string(), "MuseOnDeckItem.id is a string");
        assert!(item["progress_pct"].is_number());
        // The art id in `poster_path` must be the SAME id the item reports,
        // so a client can build the art URL from `id` alone.
        assert_eq!(item["poster_path"], "/art/media_item/77");
    }

    #[test]
    fn gaps_response_reports_the_untruncated_total_separately_from_the_list() {
        let json = serde_json::to_value(MuseGapsResponse {
            gaps: vec![MuseGapItem {
                id: "5".to_string(),
                title: "Example Series".to_string(),
                kind: "show",
                detail: "1999 · monitored, no file on disk".to_string(),
            }],
            // Deliberately larger than `gaps.len()`: the list is capped at
            // GAPS_LIMIT and `total` must stay the real backlog size.
            total: 412,
        })
        .unwrap();
        assert!(json["gaps"][0]["id"].is_string(), "MuseGapItem.id is a string");
        assert_eq!(json["gaps"].as_array().unwrap().len(), 1);
        assert_eq!(json["total"], 412);
    }

    /// Regression guard for the MUSE #84 review's sharpest finding: the four
    /// new handlers must not convert a query FAILURE into a valid-looking 2xx
    /// body, because `useMuseSection` renders any 2xx as data. This is asserted
    /// structurally (on the signatures) rather than by faking a broken pool:
    /// a `MuseResult` return type cannot silently swallow a `sqlx` error, while
    /// the previous `Json<T>` returns could and did.
    #[test]
    fn the_new_dashboard_handlers_cannot_swallow_a_query_error_into_a_2xx() {
        // Compile-time proof: each handler's output is a fallible `MuseResult`,
        // so the only way to a 2xx is a successful query. If someone
        // reintroduces a `.unwrap_or_else(|_| empty)` fail-open, its return
        // type stops being `MuseResult` and these bindings stop compiling.
        fn assert_fallible<T, F: Fn(&AppState) -> T>(_: F) {}
        let _ = assert_fallible::<_, _>(|_s: &AppState| {
            // `get_stats`/`get_gaps` take only `State`; `get_on_deck` also
            // takes `Query`. Referencing them as values is enough to pin the
            // signature — calling them would need a live pool.
            let _stats: fn(State<Arc<AppState>>) -> _ = get_stats;
            let _gaps: fn(State<Arc<AppState>>) -> _ = get_gaps;
            let _on_deck: fn(State<Arc<AppState>>, Query<AccountQuery>) -> _ = get_on_deck;
        });
    }

    #[tokio::test]
    async fn premiere_returns_501_so_the_gui_degrades_instead_of_showing_a_false_empty() {
        let (status, Json(body)) = get_premiere().await;
        // 501 is one of `useMuseSection`'s NOT_WIRED_STATUS values, so the card
        // renders "not yet wired" rather than asserting "no premieres booked".
        assert_eq!(status, StatusCode::NOT_IMPLEMENTED);
        assert_eq!(body["tracked_as"], "MUSE #86");
        // Must NOT look like a successful empty payload.
        assert!(body.get("items").is_none());
    }
}
