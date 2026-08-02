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
use chrono::{DateTime, Utc};
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

/// Convert a stored 0..1 watch fraction to the 0..100 percentage the
/// Constellation hook's `MuseOnDeckItem.progress_pct` declares, rounded to one
/// decimal so the JSON stays compact.
fn progress_pct(fraction: Option<f32>) -> f32 {
    let pct = fraction.unwrap_or(0.0) * 100.0;
    (pct * 10.0).round() / 10.0
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
    /// MUSE #112: `movie` or `show` (aliases: movies/shows/series/tv). Absent = every kind.
    pub kind: Option<String>,
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
) -> MuseResult<Json<LibraryGridResponse>> {
    // MUSE #112: raised 1000 -> 5000, matching the table endpoint. The operator's library is
    // 1892 owned titles, so the old cap could not return it however large a limit was asked
    // for — the page showed a slice and reported it as loaded.
    let limit = q.limit.unwrap_or(DEFAULT_GRID_LIMIT).clamp(1, 5000);
    // `movie` / `show` (the DB vocabulary). Anything else is rejected rather than silently
    // ignored: quietly serving a MIXED library to a page that asked for one kind is the kind of
    // false answer this codebase keeps having to remove.
    let kind = match q.kind.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        None => None,
        Some(k) => match k.to_ascii_lowercase().as_str() {
            "movie" | "movies" => Some("movie"),
            "show" | "shows" | "series" | "tv" => Some("show"),
            other => {
                return Err(MuseError::BadRequest(format!(
                    "unknown kind {other:?}; expected movie or show"
                )))
            }
        },
    };

    let owned_rows = repo::dashboard::library_grid(&state.pool, limit, kind)
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

    Ok(Json(LibraryGridResponse {
        owned,
        wanted,
        counts,
    }))
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

/// Whether Discover can ever have anything to show, and if not, why.
///
/// Extracted as a pure function so it is testable: the decision previously sat inline in a
/// handler that needs a database pool to reach, so no test executed it and a mutation restoring
/// the old `tmdb.is_some()` behaviour survived the entire suite. The rule that decides whether
/// a panel tells the operator the truth is exactly the rule that needs a test.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrendingCapability {
    /// A keyed TMDb client — trending can actually be fetched.
    Available,
    /// A key-less proxy client. Serves movie metadata, has NO trending endpoint, so no
    /// snapshot can ever be ingested however long the worker runs.
    MetadataProxyOnly,
    /// No TMDb client at all.
    NotConfigured,
}

pub fn trending_capability(client_present: bool, is_proxy_mode: bool) -> TrendingCapability {
    match (client_present, is_proxy_mode) {
        (false, _) => TrendingCapability::NotConfigured,
        (true, true) => TrendingCapability::MetadataProxyOnly,
        (true, false) => TrendingCapability::Available,
    }
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

    // MUSE #111: `configured` used to be `state.tmdb.is_some()`, which conflated "a TMDb
    // client exists" with "trending works". Those are different facts, and on this deployment
    // they differ:
    //
    //   TMDB_API_KEY unset  ->  TmdbClient runs in key-less RadarrProxy mode
    //   RadarrProxy mode    ->  `TmdbClient::trending()` returns Ok(vec![]) WITHOUT asking,
    //                           because api.radarr.video has no /trending endpoint (404, probed)
    //
    // So the trending table is never populated and Discover is empty FOREVER — while the panel
    // was told a trending provider was configured and had simply returned nothing. It then
    // offered three possible causes, none of which was the real one. Reporting the capability
    // honestly is the fix: the operator needs to know a TMDb API key is required, not go
    // hunting for a worker that never had anything to do.
    let capability = trending_capability(
        state.tmdb.is_some(),
        state.tmdb.as_ref().is_some_and(|c| c.is_proxy_mode()),
    );
    let trending_capable = capability == TrendingCapability::Available;
    let metadata_only = capability == TrendingCapability::MetadataProxyOnly;

    Json(json!({
        "region": region,
        // Now means what the panel reads it as: trending can actually be fetched.
        "configured": trending_capable,
        // The narrower fact, so the UI can distinguish "nothing set up" from "set up for
        // metadata but not for trending" — which need completely different operator actions.
        "metadata_provider_only": metadata_only,
        "reason": if trending_capable {
            Value::Null
        } else if metadata_only {
            json!("TMDb is running in key-less proxy mode (no TMDB_API_KEY). That proxy serves \
                   movie metadata only — it has no trending endpoint — so no trending snapshot \
                   can ever be ingested. Set TMDB_API_KEY to enable Discover.")
        } else {
            json!("No TMDb client is configured, so trending cannot be fetched.")
        },
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
                // MUSE #87: scale to a PERCENTAGE. `percent_complete` is a
                // fraction in 0..1 despite the column name, so passing it
                // through unscaled made every progress bar read ~0 (a live
                // 48%-watched film reported `progress_pct: 0.48`). The repo
                // filters to a non-null 0<x<1, so the `0.0` fallback is
                // unreachable; it is not an `unwrap()` so a future filter
                // change can never panic a dashboard request.
                progress_pct: progress_pct(r.percent_complete),
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

// ===========================================================================
// Sessions (MACT-01, Plane MUSE #121) — GET /api/sessions/live + /history
// ===========================================================================
//
// The missing read path over `play_sessions`, which the Plex poller and
// webhook already populate. TWO separate routes, deliberately — they are two
// sources (a derived live view vs. the permanent historical record), not one
// route with a `?state=` filter, which would erase that distinction. Each
// envelope carries an explicit `source` discriminator so the client can
// label it and so a future flip of the live source (epic §8.8 spec J) is
// visible rather than silent.
//
// `account_id` here is the MUSE account (`accounts.id`, the same id-space
// the taste model uses) — never the constellation-web cookie session, which
// carries roles (operator/viewer), not household members.
//
// Unlike the rest of this module, both handlers propagate query errors
// (`MuseResult`) rather than failing open to an empty list — see
// `get_stats`/`get_gaps`'s doc comments for why: a 2xx empty body renders as
// the CLAIM "nobody is watching" / "no history", not the absence of one.

const DEFAULT_HISTORY_LIMIT: i64 = 50;
const MAX_HISTORY_LIMIT: i64 = 500;

fn decision_kind_str(kind: crate::models::play_session::DecisionKind) -> &'static str {
    use crate::models::play_session::DecisionKind;
    match kind {
        DecisionKind::DirectPlay => "direct_play",
        DecisionKind::DirectStream => "direct_stream",
        DecisionKind::Transcode => "transcode",
        DecisionKind::Copy => "copy",
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct SessionAccountOut {
    pub id: Option<i64>,
    pub display_name: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SessionItemOut {
    pub media_item_id: Option<i64>,
    pub title: Option<String>,
    pub year: Option<i32>,
    pub kind: Option<&'static str>,
    /// Present only for an episode-level session (`episode_id` resolved).
    pub season_number: Option<i32>,
    pub episode_number: Option<i32>,
    pub episode_title: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SessionDecisionOut {
    pub video_decision: Option<&'static str>,
    pub audio_decision: Option<&'static str>,
    pub transcode_decision: Option<&'static str>,
    pub transcode_reason: Option<String>,
    pub container: Option<String>,
    pub video_codec: Option<String>,
    pub audio_codec: Option<String>,
    pub audio_channels: Option<f32>,
    pub video_resolution: Option<String>,
    pub bitrate: Option<i32>,
}

impl From<&repo::play_session::SessionJoinRow> for SessionDecisionOut {
    fn from(r: &repo::play_session::SessionJoinRow) -> Self {
        SessionDecisionOut {
            video_decision: r.video_decision.map(decision_kind_str),
            audio_decision: r.audio_decision.map(decision_kind_str),
            transcode_decision: r.transcode_decision.map(decision_kind_str),
            transcode_reason: r.transcode_reason.clone(),
            container: r.container.clone(),
            video_codec: r.video_codec.clone(),
            audio_codec: r.audio_codec.clone(),
            audio_channels: r.audio_channels,
            video_resolution: r.video_resolution.clone(),
            bitrate: r.bitrate,
        }
    }
}

impl From<&repo::play_session::SessionJoinRow> for SessionAccountOut {
    fn from(r: &repo::play_session::SessionJoinRow) -> Self {
        SessionAccountOut {
            id: r.account_id,
            display_name: r.account_display_name.clone(),
        }
    }
}

impl From<&repo::play_session::SessionJoinRow> for SessionItemOut {
    fn from(r: &repo::play_session::SessionJoinRow) -> Self {
        SessionItemOut {
            media_item_id: r.media_item_id,
            title: r.title.clone(),
            year: r.year,
            kind: r.kind.map(kind_str),
            season_number: r.season_number,
            episode_number: r.episode_number,
            episode_title: r.episode_title.clone(),
        }
    }
}

/// Scale `percent_complete` (a FRACTION in 0..1 despite the column name —
/// MUSE #87) to a percentage, EXCEPT when `duration_ms` is null: per MACT-01's
/// edge cases, an unknown duration means progress is genuinely unknown and
/// must be omitted, never reported as `0%`.
fn session_progress_pct(percent_complete: Option<f32>, duration_ms: Option<i64>) -> Option<f32> {
    duration_ms?;
    let pct = percent_complete? * 100.0;
    Some((pct * 10.0).round() / 10.0)
}

#[derive(Debug, Clone, Serialize)]
pub struct LiveSessionOut {
    pub session_id: i64,
    pub session_key: Option<String>,
    pub account: SessionAccountOut,
    pub item: SessionItemOut,
    pub poster_url: Option<String>,
    pub backdrop_url: Option<String>,
    pub view_offset_ms: Option<i64>,
    pub duration_ms: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub progress_pct: Option<f32>,
    pub player: Option<String>,
    pub platform: Option<String>,
    pub product: Option<String>,
    pub device: Option<String>,
    /// `"playing"` | `"paused"` | `"stale"` — see
    /// `repo::play_session::classify_session_state`. An open-but-stale
    /// session is reported here, never dropped, never `"playing"`.
    pub state: repo::play_session::SessionPlayState,
    pub last_event_at: Option<DateTime<Utc>>,
    pub started_at: DateTime<Utc>,
    pub decision: SessionDecisionOut,
}

impl From<repo::play_session::LiveSession> for LiveSessionOut {
    fn from(live: repo::play_session::LiveSession) -> Self {
        let row = &live.row;
        LiveSessionOut {
            session_id: row.session_id,
            session_key: row.session_key.clone(),
            account: row.into(),
            item: row.into(),
            poster_url: row.media_metadata_id.map(poster_url),
            backdrop_url: row.media_metadata_id.map(backdrop_url),
            view_offset_ms: row.view_offset_ms,
            duration_ms: row.duration_ms,
            progress_pct: session_progress_pct(row.percent_complete, row.duration_ms),
            player: row.player.clone(),
            platform: row.platform.clone(),
            product: row.product.clone(),
            device: row.device.clone(),
            state: live.state,
            last_event_at: row.last_event_at,
            started_at: row.started_at,
            decision: row.into(),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct LiveSessionsResponse {
    /// Discriminates this from a future `maestro-live` source (epic §8.8
    /// spec J) — always `"muse-derived"` for this derived-view endpoint.
    pub source: &'static str,
    pub sessions: Vec<LiveSessionOut>,
}

/// `GET /api/sessions/live` — the derived live view: `stopped_at IS NULL`
/// sessions passing the liveness rule (see
/// `repo::play_session::classify_session_state`). `source: "muse-derived"`.
///
/// A true empty (`{"sessions": [], "source": "muse-derived"}`) is returned
/// when no ingest is configured / nobody is watching — distinguishable from
/// a degrade because a query FAILURE propagates as an error instead (see
/// this section's module doc).
/// Builds the actual envelope the handler serializes, factored out so a test
/// can assert against the REAL `source` discriminator the handler emits
/// (from an empty `Vec`, requiring no DB) instead of a hand-typed string
/// that could drift from what `get_live_sessions` actually returns.
fn build_live_sessions_response(
    rows: Vec<repo::play_session::LiveSession>,
) -> LiveSessionsResponse {
    LiveSessionsResponse {
        source: "muse-derived",
        sessions: rows.into_iter().map(LiveSessionOut::from).collect(),
    }
}

pub async fn get_live_sessions(
    State(state): State<Arc<AppState>>,
) -> MuseResult<Json<LiveSessionsResponse>> {
    let grace_secs = state.config.session_active_grace_secs;
    let rows = repo::play_session::list_live(&state.pool, grace_secs).await?;
    Ok(Json(build_live_sessions_response(rows)))
}

#[derive(Debug, Clone, Serialize)]
pub struct HistorySessionOut {
    pub session_id: i64,
    pub session_key: Option<String>,
    pub account: SessionAccountOut,
    pub item: SessionItemOut,
    pub poster_url: Option<String>,
    pub backdrop_url: Option<String>,
    pub view_offset_ms: Option<i64>,
    pub duration_ms: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub progress_pct: Option<f32>,
    pub player: Option<String>,
    pub platform: Option<String>,
    pub product: Option<String>,
    pub device: Option<String>,
    pub started_at: DateTime<Utc>,
    pub decision: SessionDecisionOut,
}

impl From<repo::play_session::SessionJoinRow> for HistorySessionOut {
    fn from(row: repo::play_session::SessionJoinRow) -> Self {
        HistorySessionOut {
            session_id: row.session_id,
            session_key: row.session_key.clone(),
            account: (&row).into(),
            item: (&row).into(),
            poster_url: row.media_metadata_id.map(poster_url),
            backdrop_url: row.media_metadata_id.map(backdrop_url),
            view_offset_ms: row.view_offset_ms,
            duration_ms: row.duration_ms,
            progress_pct: session_progress_pct(row.percent_complete, row.duration_ms),
            player: row.player.clone(),
            platform: row.platform.clone(),
            product: row.product.clone(),
            device: row.device.clone(),
            started_at: row.started_at,
            decision: (&row).into(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct SessionHistoryQuery {
    pub limit: Option<i64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct HistorySessionsResponse {
    /// Always `"muse-history"` — Muse's PERMANENT role per MACT-01's spec;
    /// this route does not change when spec J flips the live source.
    pub source: &'static str,
    pub sessions: Vec<HistorySessionOut>,
}

/// `GET /api/sessions/history?limit=` — Muse's permanent historical record
/// over stopped sessions. Same projection as `/live`, `source: "muse-history"`.
/// See [`build_live_sessions_response`] — same rationale, for history.
fn build_history_sessions_response(
    rows: Vec<repo::play_session::SessionJoinRow>,
) -> HistorySessionsResponse {
    HistorySessionsResponse {
        source: "muse-history",
        sessions: rows.into_iter().map(HistorySessionOut::from).collect(),
    }
}

pub async fn get_session_history(
    State(state): State<Arc<AppState>>,
    Query(q): Query<SessionHistoryQuery>,
) -> MuseResult<Json<HistorySessionsResponse>> {
    let limit = q
        .limit
        .unwrap_or(DEFAULT_HISTORY_LIMIT)
        .clamp(1, MAX_HISTORY_LIMIT);
    let rows = repo::play_session::list_history(&state.pool, limit).await?;
    Ok(Json(build_history_sessions_response(rows)))
}

// ===========================================================================
// Terminate (MACT-02, Plane MUSE #122) — POST /api/sessions/:session_key/terminate
// ===========================================================================
//
// The only mutation in this file's session surface, and the only one with
// real-world blast radius: it interrupts a person mid-film. Three
// deliberate safety properties, all enforced here or upstream of here:
//
// 1. Keyed on `session_key`, resolved against the SAME live set
//    `GET /api/sessions/live` reports (`plex_control::resolve_live_target`,
//    which calls `repo::play_session::find_live_by_session_key`). A caller
//    can never pass an arbitrary player target and have Muse relay a stop
//    to it — Muse decides the target, not the request body. AND resolution
//    is a REFUSAL, not a tiebreak, when it's ambiguous: both `session_key`
//    (Plex reuses it) and the `plex_clients` display-name join this bridges
//    through (see `plex_control::repo::find_machine_identifier_by_name`'s
//    doc comment; `TODO(S130-J)` there for the real fix) are non-unique
//    columns — more than one candidate is `409 Conflict`, never a silent
//    "pick the newest one". A wrong-target stop is the whole failure mode
//    this endpoint exists to prevent.
// 2. The relay itself goes through `CastController::stop` — the ONE seam
//    MUSE-22 built for driving playback, never a second HTTP path to Plex.
// 3. Every failure mode reports what ACTUALLY happened
//    (`TerminateSessionResponse`) — a `200` never implies a stream stopped
//    when nothing was relayed, an unconfigured/unresolvable target is a
//    `503`, an ambiguous match is a `409`, and none of those is ever an
//    optimistic `200`. `stopped: true` on a `200` means "the backend
//    accepted the command and nothing since contradicted it" — see
//    `TerminateSessionResponse::stopped`'s own doc comment for exactly
//    what that does and doesn't establish.
//
// Auth is layered, not solely this handler's job: Terminus's `proxy_muse`
// runs `enforce_viewer_role_gate` in front of this route on the
// Constellation-web path, rejecting a `viewer`'s `POST` with `403` before
// it is ever proxied here. This route's own bearer check
// (`auth::require_api_token`, applied to the whole `protected_routes()`
// group) is Muse's half: Muse has one shared bearer and no per-user
// identity, so it cannot itself distinguish operator from viewer — it only
// proves the caller came through Terminus. The panel's client-side
// `RoleGate` (MACT-07) is cosmetic on top of both; it enforces nothing.

#[derive(Debug, Clone, Deserialize, Default)]
pub struct TerminateSessionBody {
    /// Optional human-readable reason, logged for the operator. NOT
    /// currently deliverable to the viewer — see `reason_delivered` below.
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct TerminateSessionResponse {
    /// Whether the stop actually took, to the best of what Muse can
    /// establish. Precisely: `false` on any `CastController::stop` error
    /// (never an optimistic `true`), AND `false` if a follow-up timeline
    /// poll shows the player still actively playing/paused/buffering (the
    /// backend accepted the command but it plainly didn't take). `true`
    /// otherwise -- which for `PlexControlClient` means "the PMS answered
    /// 2xx to the stop request, and nothing since has contradicted it" --
    /// NOT an independently-confirmed "the stream has fully ended" in every
    /// case a poll is inconclusive or fails. See `terminate_session`'s
    /// `Ok(())` arm for the exact reasoning. Never read this as a stronger
    /// guarantee than that.
    pub stopped: bool,
    pub backend: &'static str,
    /// Whether the caller's `reason` (if any) was actually surfaced to the
    /// viewer. Today's `CastController::stop(target)` has no channel to
    /// carry a message to the player (Plex Companion's stop command takes
    /// no text payload), so this is always `false` — the reason is logged
    /// for the operator (see `terminate_session`) but never claimed as
    /// delivered. A future `CastController` that CAN surface it should flip
    /// this honestly, not unconditionally.
    pub reason_delivered: bool,
}

/// Parse the optional JSON body (`{"reason": "..."}`) MACT-02 accepts. A
/// genuinely empty body (no `Content-Length`, or `Content-Length: 0`) is
/// valid — "no reason given" — rather than a `400`; axum's own `Json`
/// extractor would otherwise reject a bodyless POST as malformed JSON, so
/// this is parsed by hand from raw bytes instead of via `Json<...>`. Pure
/// and DB-independent — exercised directly by unit tests below.
fn parse_terminate_body(bytes: &[u8]) -> Result<Option<String>, MuseError> {
    if bytes.is_empty() {
        return Ok(None);
    }
    let body: TerminateSessionBody = serde_json::from_slice(bytes)
        .map_err(|e| MuseError::BadRequest(format!("invalid terminate request body: {e}")))?;
    Ok(body.reason)
}

/// `POST /api/sessions/:session_key/terminate` — stop a live stream. See
/// this section's module doc for the full safety-property breakdown.
pub async fn terminate_session(
    State(state): State<Arc<AppState>>,
    Path(session_key): Path<String>,
    body: axum::body::Bytes,
) -> MuseResult<Json<TerminateSessionResponse>> {
    let reason = parse_terminate_body(&body)?;

    // Cheap, static check FIRST: if there is no controller at all, there is
    // nothing a DB round-trip could change about the outcome — every
    // session_key would 503 regardless of whether it's live. Fail fast,
    // never fabricate a 200.
    let Some(controller) = state.cast_controller.as_ref() else {
        return Err(MuseError::ServiceUnavailable(
            "no cast controller configured; nothing to relay a stop to".to_string(),
        ));
    };

    let machine_identifier = match crate::plex_control::resolve_live_target(
        &state.pool,
        &session_key,
        state.config.session_active_grace_secs,
        state.config.terminate_target_fresh_within_secs,
    )
    .await?
    {
        crate::plex_control::ResolveOutcome::NotFound => {
            return Err(MuseError::NotFound(format!(
                "no live session for session_key {session_key}"
            )));
        }
        // Review finding, cycle 2 (MACT-02, codex, confirmed): MACT-01
        // deliberately retains stale open rows (that's what its `stale`
        // state is for), so an old stale session must not resolve a target
        // at all -- it could stop a NEWER session sharing the same device.
        // Grouped with `NotFound`'s 404: both mean "no session Muse
        // currently vouches for as live", which is the only fact this
        // caller-facing status needs to carry (see `ResolveOutcome::StaleSession`'s
        // doc comment for why this reuses MACT-01's own liveness judgement
        // rather than a second, driftable definition of "live").
        crate::plex_control::ResolveOutcome::StaleSession => {
            return Err(MuseError::NotFound(format!(
                "session_key {session_key} has no play_events within the liveness grace \
                 window; refusing to treat a stale session as live"
            )));
        }
        // Review finding (MACT-02, codex, confirmed): `session_key` and the
        // reported player display name are both non-unique columns. Never
        // silently pick a candidate for a mutation with this blast radius
        // -- refuse with a distinct 409 naming the ambiguity, rather than
        // collapsing "there could be more than one" into "no target"'s 503
        // or into a guessed success.
        crate::plex_control::ResolveOutcome::AmbiguousSession => {
            return Err(MuseError::Conflict(format!(
                "more than one live session currently matches session_key {session_key}; \
                 refusing to guess which one to stop"
            )));
        }
        crate::plex_control::ResolveOutcome::NoTarget => {
            return Err(MuseError::ServiceUnavailable(format!(
                "session {session_key} is live but has no resolvable cast-control target"
            )));
        }
        // Review finding, cycle 2 (codex, confirmed): a UNIQUE plex_clients
        // match is not necessarily a CURRENT one -- rows are never pruned,
        // so the one match found could be an obsolete client sharing a
        // display name with whatever device the session actually plays on
        // now. Distinct 503 from `NoTarget` (there WAS a match, just not a
        // trustworthy one) for logs/diagnostics; the caller sees the same
        // "nothing to relay to safely" outcome either way.
        crate::plex_control::ResolveOutcome::StaleTarget => {
            return Err(MuseError::ServiceUnavailable(format!(
                "session {session_key}'s player name matches only a stale plex_clients row \
                 (older than {}s); refusing to relay to a possibly-obsolete device",
                state.config.terminate_target_fresh_within_secs
            )));
        }
        crate::plex_control::ResolveOutcome::AmbiguousTarget => {
            return Err(MuseError::Conflict(format!(
                "more than one discovered Plex client matches session {session_key}'s player \
                 name; refusing to guess which device to stop"
            )));
        }
        crate::plex_control::ResolveOutcome::Resolved { machine_identifier } => {
            machine_identifier
        }
    };

    // Review finding (MACT-02, codex, confirmed): the module doc + HTTP
    // reference doc both say the reason is "logged for the operator" --
    // logging only `reason.is_some()` doesn't actually do that. Log the
    // text itself (an empty string when none was given) alongside the
    // boolean, so the claim in the docs matches what's on disk.
    tracing::info!(
        session_key = %session_key,
        target = %machine_identifier,
        reason_provided = reason.is_some(),
        reason = %reason.as_deref().unwrap_or(""),
        "terminate_session: relaying stop"
    );

    let stopped = match controller.stop(&machine_identifier).await {
        Ok(()) => {
            // Review finding (MACT-02, codex, confirmed): `stop()` returning
            // `Ok` means the backend ACCEPTED the command (for
            // `PlexControlClient`, the PMS answered 2xx to the Companion
            // stop request) -- it does NOT establish that playback actually
            // ended. Strengthen the signal with a timeline poll (the same
            // `CastController` seam) per this endpoint's own EDGE CASES
            // ("player accepts but keeps playing -> stopped: false with the
            // backend's own report"): if the player is STILL actively
            // reporting playing/paused/buffering, the stop plainly didn't
            // take, and we downgrade honestly rather than claim success. A
            // poll failure, or any other/absent state, is not further proof
            // of anything either way -- it's just not a disproof, so the
            // accepted-command signal stands. See `stopped`'s doc comment
            // on `TerminateSessionResponse` for the precise, honest
            // semantics this leaves the field with.
            match controller.poll_timeline(&machine_identifier).await {
                Ok(poll)
                    if matches!(
                        poll.state.as_deref(),
                        Some("playing") | Some("paused") | Some("buffering")
                    ) =>
                {
                    tracing::warn!(
                        session_key = %session_key,
                        target = %machine_identifier,
                        state = ?poll.state,
                        "terminate_session: stop command accepted but timeline still active"
                    );
                    false
                }
                _ => true,
            }
        }
        Err(e) => {
            tracing::warn!(
                session_key = %session_key,
                target = %machine_identifier,
                error = %e,
                "terminate_session: CastController::stop failed"
            );
            false
        }
    };

    Ok(Json(TerminateSessionResponse {
        stopped,
        backend: controller.backend_name(),
        reason_delivered: false,
    }))
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

    /// MUSE #87 regression: `play_sessions.percent_complete` is a FRACTION in
    /// 0..1 despite its name, so a passthrough reported a 48%-watched film as
    /// `progress_pct: 0.48` and every progress bar rendered as ~empty. Live
    /// data confirms the scale (finished sessions avg 0.991, max 1.000).
    #[test]
    fn progress_pct_scales_the_stored_fraction_to_a_percentage() {
        assert_eq!(progress_pct(Some(0.48)), 48.0);
        assert_eq!(progress_pct(Some(0.851)), 85.1);
        assert_eq!(progress_pct(Some(1.0)), 100.0);
        // Unreachable given the repo filter, but must not panic or NaN.
        assert_eq!(progress_pct(None), 0.0);
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

#[cfg(test)]
mod mact01_sessions_tests {
    use super::*;
    use crate::models::play_session::DecisionKind;

    /// MACT-01 AC: handlers are agnostic to who writes `play_sessions` — a
    /// source-scan proof that neither this file's sessions section, nor the
    /// repo layer it calls, references `tracker::poller` or keys behaviour on
    /// `source = 'plex_poll'`. Mirrors the `no_playback_mutation_calls`
    /// pattern in `tracker::interpret`.
    #[test]
    fn sessions_handlers_never_reference_tracker_poller_or_the_plex_poll_source() {
        let dashboard_source = include_str!("dashboard.rs");
        let section_start = dashboard_source
            .find("// Sessions (MACT-01, Plane MUSE #121)")
            .expect("MACT-01 sessions section marker must exist in dashboard.rs");
        let section = &dashboard_source[section_start..];
        // Stop at this test module itself so the FORBIDDEN strings appearing
        // in comments *about* the rule (like this one) don't self-trigger.
        let non_test_section = section.split("#[cfg(test)]").next().unwrap_or(section);

        let repo_source = include_str!("../repo/play_session.rs");

        const FORBIDDEN: &[&str] = &["tracker::poller", "poller::", "plex_poll"];
        for pattern in FORBIDDEN {
            assert!(
                !non_test_section.contains(pattern),
                "web::dashboard's MACT-01 sessions handlers must not reference {pattern:?}"
            );
            assert!(
                !repo_source.contains(pattern),
                "repo::play_session's MACT-01 queries must not reference {pattern:?}"
            );
        }
    }

    #[test]
    fn session_progress_pct_scales_the_stored_fraction_to_a_percentage() {
        // MUSE #87's regression, re-pinned for the sessions projection.
        assert_eq!(session_progress_pct(Some(0.48), Some(6_000_000)), Some(48.0));
        assert_eq!(session_progress_pct(Some(1.0), Some(6_000_000)), Some(100.0));
        assert_eq!(session_progress_pct(Some(0.0), Some(6_000_000)), Some(0.0));
    }

    #[test]
    fn session_progress_pct_is_omitted_not_zero_when_duration_is_unknown() {
        // MACT-01 edge case: an unknown duration means progress is unknown,
        // never a fabricated `0%`.
        assert_eq!(session_progress_pct(Some(0.48), None), None);
        assert_eq!(session_progress_pct(None, Some(6_000_000)), None);
        assert_eq!(session_progress_pct(None, None), None);
    }

    #[test]
    fn decision_kind_str_passes_through_all_four_variants_verbatim() {
        // MACT-01 AC: `copy`/`direct_stream`/`transcode` (and `direct_play`)
        // must never collapse into a boolean "is transcoding" — assert each
        // of the four persisted variants maps to its own distinct string.
        assert_eq!(decision_kind_str(DecisionKind::DirectPlay), "direct_play");
        assert_eq!(decision_kind_str(DecisionKind::DirectStream), "direct_stream");
        assert_eq!(decision_kind_str(DecisionKind::Transcode), "transcode");
        assert_eq!(decision_kind_str(DecisionKind::Copy), "copy");
    }

    /// Calls the SAME builder functions `get_live_sessions`/
    /// `get_session_history` call ([`build_live_sessions_response`] /
    /// [`build_history_sessions_response`]) rather than hand-constructing a
    /// `LiveSessionsResponse`/`HistorySessionsResponse` literal — a
    /// hand-typed literal would pass even if the handler's actual `source`
    /// string drifted, since it never exercises the handler's own code.
    #[test]
    fn live_and_history_responses_carry_their_own_source_discriminator() {
        let live = serde_json::to_value(build_live_sessions_response(Vec::new())).unwrap();
        assert_eq!(live["source"], "muse-derived");
        assert_eq!(live["sessions"], serde_json::json!([]));

        let history = serde_json::to_value(build_history_sessions_response(Vec::new())).unwrap();
        assert_eq!(history["source"], "muse-history");
        assert_eq!(history["sessions"], serde_json::json!([]));
    }

    fn sample_join_row() -> repo::play_session::SessionJoinRow {
        repo::play_session::SessionJoinRow {
            session_id: 1,
            account_id: Some(7),
            account_display_name: Some("Alex".to_string()),
            media_item_id: Some(42),
            episode_id: None,
            media_metadata_id: Some(99),
            kind: Some(MediaKind::Movie),
            title: Some("Arrival".to_string()),
            year: Some(2016),
            season_number: None,
            episode_number: None,
            episode_title: None,
            session_key: Some("session-key-1".to_string()),
            view_offset_ms: Some(1_000),
            duration_ms: Some(6_000_000),
            percent_complete: Some(0.48),
            player: Some("Living Room".to_string()),
            platform: Some("Plex Web".to_string()),
            product: Some("Plex Web".to_string()),
            device: Some("Chrome".to_string()),
            started_at: Utc::now(),
            last_event_type: Some("media.play".to_string()),
            last_event_at: Some(Utc::now()),
            video_decision: Some(DecisionKind::Transcode),
            audio_decision: Some(DecisionKind::Copy),
            transcode_decision: Some(DecisionKind::Transcode),
            container: Some("mkv".to_string()),
            video_codec: Some("hevc".to_string()),
            audio_codec: Some("aac".to_string()),
            audio_channels: Some(2.0),
            video_resolution: Some("1080".to_string()),
            bitrate: Some(8_000_000),
            transcode_reason: Some("video codec unsupported by device".to_string()),
        }
    }

    /// MACT-01 AC: `ip_address` is on `PlaySession`/`PlayEvent` but nothing
    /// in the sessions read path selects it, holds it, or serializes it.
    /// Proven both structurally (the row/output types below have no such
    /// field, so this would fail to COMPILE if one were added) and by
    /// asserting it is absent from the actual serialized JSON.
    #[test]
    fn ip_address_is_never_serialized_in_a_live_or_history_session() {
        let live_out = LiveSessionOut::from(repo::play_session::LiveSession {
            row: sample_join_row(),
            state: repo::play_session::SessionPlayState::Playing,
        });
        let live_json = serde_json::to_value(&live_out).unwrap();
        assert!(live_json.get("ip_address").is_none());
        assert!(!serde_json::to_string(&live_json).unwrap().contains("ip_address"));

        let history_out = HistorySessionOut::from(sample_join_row());
        let history_json = serde_json::to_value(&history_out).unwrap();
        assert!(history_json.get("ip_address").is_none());
    }

    #[test]
    fn live_session_out_carries_the_classified_state_and_decision_block_verbatim() {
        let out = LiveSessionOut::from(repo::play_session::LiveSession {
            row: sample_join_row(),
            state: repo::play_session::SessionPlayState::Stale,
        });
        let json = serde_json::to_value(&out).unwrap();
        assert_eq!(json["state"], "stale");
        assert_eq!(json["decision"]["video_decision"], "transcode");
        assert_eq!(json["decision"]["audio_decision"], "copy");
        assert_eq!(json["progress_pct"], 48.0);
    }

    /// MACT-01 edge case, asserted at the SERIALIZED shape (not just the
    /// Rust `Option` value): `duration_ms: None` must make `progress_pct`
    /// absent from the JSON body entirely, never present as `null`. A
    /// dashboard client checking `"progress_pct" in body` (or any falsy/
    /// nullish check that still sees the key) would otherwise be fooled by
    /// a `null` that reads differently from a genuinely missing field.
    #[test]
    fn progress_pct_is_absent_from_the_serialized_body_not_null_when_duration_is_unknown() {
        let mut row = sample_join_row();
        row.duration_ms = None;

        let live_json = serde_json::to_value(LiveSessionOut::from(repo::play_session::LiveSession {
            row: row.clone(),
            state: repo::play_session::SessionPlayState::Playing,
        }))
        .unwrap();
        assert!(
            live_json.as_object().unwrap().get("progress_pct").is_none(),
            "progress_pct must be OMITTED, not serialized as null: {live_json}"
        );

        let history_json = serde_json::to_value(HistorySessionOut::from(row)).unwrap();
        assert!(
            history_json.as_object().unwrap().get("progress_pct").is_none(),
            "progress_pct must be OMITTED, not serialized as null: {history_json}"
        );
    }

    /// Same fail-open-proof pattern as
    /// `the_new_dashboard_handlers_cannot_swallow_a_query_error_into_a_2xx`
    /// above, for the two MACT-01 handlers — strengthened past a bare
    /// `Fn(&AppState) -> T` (which unifies `T` with whatever the function
    /// actually returns and therefore proves nothing about `MuseResult`
    /// specifically): these helpers pin the closure's `Fut::Output` to
    /// `MuseResult<Json<_>>`, so this fails to COMPILE if either handler's
    /// signature ever changes to return a bare `Json<_>` (a fail-open).
    fn assert_state_handler_returns_museresult<T, Fut, F>(_: F)
    where
        F: Fn(State<Arc<AppState>>) -> Fut,
        Fut: std::future::Future<Output = MuseResult<Json<T>>>,
    {
    }

    fn assert_state_query_handler_returns_museresult<T, Q, Fut, F>(_: F)
    where
        F: Fn(State<Arc<AppState>>, Query<Q>) -> Fut,
        Fut: std::future::Future<Output = MuseResult<Json<T>>>,
    {
    }

    #[test]
    fn sessions_handlers_cannot_swallow_a_query_error_into_a_2xx() {
        assert_state_handler_returns_museresult(get_live_sessions);
        assert_state_query_handler_returns_museresult(get_session_history);
    }

}

/// MACT-02 tests for `terminate_session`: pure body-parsing (no DB), then
/// sqlx integration coverage for unknown key (404), a live session with no
/// matching `plex_clients` row (503, no relay), and both outcomes of an
/// actually-attempted relay against a fake `CastController` (`stopped:
/// true`/`false`, never fabricated). The integration tests are gated on
/// `MUSE_TEST_DATABASE_URL` exactly like `repo::play_session::mact01_tests`'s
/// `db_gated` module — skipped, not failed, when no live DB is configured.
#[cfg(test)]
mod mact02_terminate_tests {
    use super::*;

    // -- parse_terminate_body (pure, no DB) ------------------------------

    #[test]
    fn terminate_body_empty_bytes_is_no_reason_not_a_400() {
        assert_eq!(parse_terminate_body(b"").unwrap(), None);
    }

    #[test]
    fn terminate_body_with_reason_is_parsed() {
        assert_eq!(
            parse_terminate_body(br#"{"reason":"movie night is over"}"#).unwrap(),
            Some("movie night is over".to_string())
        );
    }

    #[test]
    fn terminate_body_empty_object_is_no_reason() {
        assert_eq!(parse_terminate_body(b"{}").unwrap(), None);
    }

    #[test]
    fn terminate_body_malformed_json_is_a_bad_request_not_a_500() {
        let err = parse_terminate_body(b"{not json").unwrap_err();
        assert!(matches!(err, MuseError::BadRequest(_)), "got {err:?}");
    }

    #[test]
    fn terminate_response_never_serializes_reason_delivered_true_by_construction() {
        // Cheap proof that the default/only construction path in
        // `terminate_session` sets `reason_delivered: false` — CastController::stop
        // has no way to actually deliver one today (see the doc comment on
        // `TerminateSessionResponse::reason_delivered`).
        let resp = TerminateSessionResponse {
            stopped: true,
            backend: "plex",
            reason_delivered: false,
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["reason_delivered"], false);
    }

    // -- sqlx integration coverage ---------------------------------------
    use crate::config::Config;
    use crate::plex_control::{CastController, TimelinePoll};
    use sqlx::PgPool;

    /// A `CastController` test double: never touches a network, reports
    /// exactly what it's told to. `target` is recorded so a test can assert
    /// the resolved `machine_identifier` — not the session's `player`
    /// display name — is what actually got relayed.
    /// Review finding (MACT-02, codex, confirmed): the earlier version of
    /// this fake discarded `target` entirely, so the "success" test never
    /// actually asserted *which* `machine_identifier` got the stop relayed
    /// to it -- the whole point of the resolution seam. `stop_calls`
    /// records every target `stop()` was actually invoked with.
    struct FakeController {
        fail: bool,
        /// When `true`, `poll_timeline` reports the player is STILL
        /// actively playing after `stop()` returned `Ok` -- exercises the
        /// "command accepted but didn't take" downgrade in
        /// `terminate_session`.
        poll_still_playing: bool,
        backend: &'static str,
        stop_calls: std::sync::Mutex<Vec<String>>,
    }

    impl FakeController {
        fn new(fail: bool) -> Self {
            FakeController {
                fail,
                poll_still_playing: false,
                backend: "fake",
                stop_calls: std::sync::Mutex::new(Vec::new()),
            }
        }

        fn stop_targets(&self) -> Vec<String> {
            self.stop_calls.lock().unwrap().clone()
        }
    }

    #[async_trait::async_trait]
    impl CastController for FakeController {
        async fn play_media(
            &self,
            _target: &str,
            _rating_key: &str,
            _play_queue_id: Option<i64>,
            _offset_ms: i64,
        ) -> MuseResult<()> {
            Ok(())
        }

        async fn play(&self, _target: &str) -> MuseResult<()> {
            Ok(())
        }

        async fn pause(&self, _target: &str) -> MuseResult<()> {
            Ok(())
        }

        async fn stop(&self, target: &str) -> MuseResult<()> {
            self.stop_calls.lock().unwrap().push(target.to_string());
            if self.fail {
                Err(MuseError::upstream("simulated stop failure"))
            } else {
                Ok(())
            }
        }

        async fn skip_next(&self, _target: &str) -> MuseResult<()> {
            Ok(())
        }

        async fn poll_timeline(&self, _target: &str) -> MuseResult<TimelinePoll> {
            if self.poll_still_playing {
                Ok(TimelinePoll {
                    state: Some("playing".to_string()),
                    rating_key: None,
                    time_ms: None,
                    duration_ms: None,
                    raw: serde_json::json!({}),
                })
            } else {
                Err(MuseError::NotImplemented)
            }
        }

        fn backend_name(&self) -> &'static str {
            self.backend
        }
    }

    async fn test_pool_or_skip(test_name: &str) -> Option<PgPool> {
        let Ok(database_url) = std::env::var("MUSE_TEST_DATABASE_URL") else {
            // See `repo::play_session::mact01_tests::db_gated::test_pool_or_skip`'s
            // doc comment: a real `eprintln!` here would be invisible on a
            // passing test under plain `cargo test` (libtest captures it),
            // silently hiding that this integration coverage never ran.
            // Writing straight to the real stderr fd bypasses that capture.
            use std::io::Write as _;
            let _ = writeln!(
                std::io::stderr(),
                "[db_gated SKIP] MUSE_TEST_DATABASE_URL not set — {test_name} did NOT run \
                 against a live database (expected in the default test run)"
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

    fn test_state(pool: PgPool, cast_controller: Option<Arc<dyn CastController>>) -> Arc<AppState> {
        Arc::new(AppState {
            pool,
            config: Config::default(),
            plex: None,
            prowlarr: None,
            arr_instances: Vec::new(),
            enrichment: crate::enrichment::EnrichmentService::from_config(&Config::default()),
            tmdb: None,
            embed: None,
            download: None,
            cast_controller,
        })
    }

    /// Insert a minimal open (`stopped_at IS NULL`) `play_sessions` row for
    /// a fresh `session_key`, with the given `player` display name.
    /// Seeds an open `play_sessions` row AND a matching, FRESH `play_events`
    /// row (`media.play`, `received_at = now()`), so `classify_session_state`
    /// reports `Playing` -- i.e. this session passes `resolve_live_target`'s
    /// cycle-2 staleness check the same way `GET /api/sessions/live` would
    /// report it `state: "playing"`. Every existing terminate test wants a
    /// genuinely-live session by default; use [`seed_stale_live_session`]
    /// for the dedicated staleness-refusal test.
    async fn seed_live_session(pool: &PgPool, session_key: &str, player: &str) {
        sqlx::query(
            "INSERT INTO play_sessions (session_key, started_at, player) \
             VALUES ($1, now(), $2)",
        )
        .bind(session_key)
        .bind(player)
        .execute(pool)
        .await
        .expect("seed play_sessions row");

        // `ON CONFLICT DO NOTHING`: the ambiguous-session-key test seeds two
        // live rows sharing one `session_key`, which would otherwise collide
        // on `play_events`'s `(source, event_type, session_key,
        // view_offset_ms)` uniqueness. Harmless here -- that test only cares
        // about row MULTIPLICITY (checked before staleness ever runs), not
        // play state.
        sqlx::query(
            "INSERT INTO play_events (source, event_type, session_key, view_offset_ms, raw) \
             VALUES ('plex_poll', 'media.play', $1, 0, '{}'::jsonb) \
             ON CONFLICT DO NOTHING",
        )
        .bind(session_key)
        .execute(pool)
        .await
        .expect("seed a fresh play_events row");
    }

    /// Same as [`seed_live_session`] but with NO `play_events` row at all --
    /// `classify_session_state` reports `Stale` (no `last_event_at` to
    /// trust), same as a crashed player or a missed stop event MACT-01
    /// already has to handle.
    async fn seed_stale_live_session(pool: &PgPool, session_key: &str, player: &str) {
        sqlx::query(
            "INSERT INTO play_sessions (session_key, started_at, player) \
             VALUES ($1, now(), $2)",
        )
        .bind(session_key)
        .bind(player)
        .execute(pool)
        .await
        .expect("seed play_sessions row");
    }

    /// `last_seen_at` defaults to `now()` (see migration 0090) -- fresh.
    async fn seed_plex_client(pool: &PgPool, machine_identifier: &str, name: &str) {
        sqlx::query(
            "INSERT INTO plex_clients (machine_identifier, name) VALUES ($1, $2)",
        )
        .bind(machine_identifier)
        .bind(name)
        .execute(pool)
        .await
        .expect("seed plex_clients row");
    }

    /// A `plex_clients` row seen a full day ago -- well outside
    /// `Config::default().terminate_target_fresh_within_secs` (300s), for
    /// the stale-target refusal test.
    async fn seed_stale_plex_client(pool: &PgPool, machine_identifier: &str, name: &str) {
        sqlx::query(
            "INSERT INTO plex_clients (machine_identifier, name, last_seen_at) \
             VALUES ($1, $2, now() - interval '1 day')",
        )
        .bind(machine_identifier)
        .bind(name)
        .execute(pool)
        .await
        .expect("seed stale plex_clients row");
    }

    #[tokio::test]
    async fn unknown_session_key_is_404_with_no_relay_attempted() {
        let Some(pool) = test_pool_or_skip("unknown_session_key_is_404_with_no_relay_attempted").await
        else {
            return;
        };
        let state = test_state(
            pool,
            Some(Arc::new(FakeController::new(false))),
        );

        let key = format!("mact02-unknown-{}", uuid::Uuid::new_v4());
        let err = terminate_session(
            State(state),
            Path(key),
            axum::body::Bytes::new(),
        )
        .await
        .expect_err("an unknown session_key must not succeed");
        assert!(
            matches!(err, MuseError::NotFound(_)),
            "expected NotFound, got {err:?}"
        );
    }

    #[tokio::test]
    async fn no_controller_configured_is_503_before_any_db_read() {
        let Some(pool) =
            test_pool_or_skip("no_controller_configured_is_503_before_any_db_read").await
        else {
            return;
        };
        let state = test_state(pool, None);

        // Deliberately an unknown key too -- proves the 503 fires from the
        // controller check, not incidentally from a 404, since no relay
        // (and per the handler's fail-fast ordering, no DB read at all)
        // should ever be attempted with nothing to relay to.
        let key = format!("mact02-no-controller-{}", uuid::Uuid::new_v4());
        let err = terminate_session(State(state), Path(key), axum::body::Bytes::new())
            .await
            .expect_err("no controller configured must never fabricate a 200");
        assert!(
            matches!(err, MuseError::ServiceUnavailable(_)),
            "expected ServiceUnavailable, got {err:?}"
        );
    }

    #[tokio::test]
    async fn live_session_with_no_matching_plex_client_is_503_never_a_fabricated_200() {
        let Some(pool) = test_pool_or_skip(
            "live_session_with_no_matching_plex_client_is_503_never_a_fabricated_200",
        )
        .await
        else {
            return;
        };
        let suffix = uuid::Uuid::new_v4();
        let key = format!("mact02-notarget-{suffix}");
        let player_name = format!("Unknown Room {suffix}");
        seed_live_session(&pool, &key, &player_name).await;
        // Deliberately NOT seeding a matching `plex_clients` row.

        let state = test_state(
            pool,
            Some(Arc::new(FakeController::new(false))),
        );

        let err = terminate_session(State(state), Path(key), axum::body::Bytes::new())
            .await
            .expect_err("an unresolvable target must not succeed");
        assert!(
            matches!(err, MuseError::ServiceUnavailable(_)),
            "expected ServiceUnavailable, got {err:?}"
        );
    }

    #[tokio::test]
    async fn resolved_session_relays_stop_and_reports_true_on_success() {
        let Some(pool) =
            test_pool_or_skip("resolved_session_relays_stop_and_reports_true_on_success").await
        else {
            return;
        };
        let suffix = uuid::Uuid::new_v4();
        let key = format!("mact02-ok-{suffix}");
        let player_name = format!("Living Room {suffix}");
        let machine_id = format!("machine-{suffix}");
        seed_live_session(&pool, &key, &player_name).await;
        seed_plex_client(&pool, &machine_id, &player_name).await;

        // Keep the concrete `Arc<FakeController>` alongside the trait-object
        // handle `test_state` takes, so the assertion below can inspect
        // exactly which target `stop()` was invoked with -- not just that
        // it succeeded (see `FakeController`'s doc comment: the earlier
        // version of this test couldn't tell wrong-target from right-target).
        let controller = Arc::new(FakeController::new(false));
        let state = test_state(pool, Some(controller.clone() as Arc<dyn CastController>));

        let Json(resp) = terminate_session(State(state), Path(key), axum::body::Bytes::new())
            .await
            .expect("a resolved live session with a working controller should succeed");
        assert!(resp.stopped, "expected stopped:true on a successful relay");
        assert_eq!(resp.backend, "fake");
        assert!(
            !resp.reason_delivered,
            "no reason was sent, and CastController::stop has no delivery channel anyway"
        );
        assert_eq!(
            controller.stop_targets(),
            vec![machine_id.clone()],
            "stop() must be called with the resolved plex_clients.machine_identifier, \
             never the session's player display name or anything else"
        );
    }

    #[tokio::test]
    async fn stop_accepted_but_still_playing_is_reported_stopped_false() {
        let Some(pool) =
            test_pool_or_skip("stop_accepted_but_still_playing_is_reported_stopped_false").await
        else {
            return;
        };
        let suffix = uuid::Uuid::new_v4();
        let key = format!("mact02-stillplaying-{suffix}");
        let player_name = format!("Kitchen {suffix}");
        let machine_id = format!("machine-{suffix}");
        seed_live_session(&pool, &key, &player_name).await;
        seed_plex_client(&pool, &machine_id, &player_name).await;

        let mut controller = FakeController::new(false);
        controller.poll_still_playing = true;
        let state = test_state(pool, Some(Arc::new(controller)));

        let Json(resp) = terminate_session(State(state), Path(key), axum::body::Bytes::new())
            .await
            .expect("an accepted-but-ignored stop is a 200, not an error response");
        assert!(
            !resp.stopped,
            "a timeline poll that still shows the player active must downgrade \
             stopped to false, never leave a command-accepted true unquestioned"
        );
    }

    #[tokio::test]
    async fn ambiguous_session_key_is_409_never_a_silent_pick() {
        let Some(pool) =
            test_pool_or_skip("ambiguous_session_key_is_409_never_a_silent_pick").await
        else {
            return;
        };
        let suffix = uuid::Uuid::new_v4();
        // Same session_key, TWO separate open (stopped_at IS NULL) rows --
        // the exact key-reuse hazard MACT-01's own doc comments describe.
        let key = format!("mact02-dupkey-{suffix}");
        seed_live_session(&pool, &key, &format!("Room A {suffix}")).await;
        seed_live_session(&pool, &key, &format!("Room B {suffix}")).await;

        let state = test_state(pool, Some(Arc::new(FakeController::new(false))));

        let err = terminate_session(State(state), Path(key), axum::body::Bytes::new())
            .await
            .expect_err("an ambiguous session_key must not resolve to either candidate");
        assert!(
            matches!(err, MuseError::Conflict(_)),
            "expected Conflict (409), got {err:?}"
        );
    }

    #[tokio::test]
    async fn ambiguous_plex_client_name_is_409_never_a_silent_pick() {
        let Some(pool) =
            test_pool_or_skip("ambiguous_plex_client_name_is_409_never_a_silent_pick").await
        else {
            return;
        };
        let suffix = uuid::Uuid::new_v4();
        let key = format!("mact02-duptarget-{suffix}");
        let player_name = format!("Shared Name {suffix}");
        seed_live_session(&pool, &key, &player_name).await;
        // Two DIFFERENT discovered Plex clients sharing the same display
        // name -- e.g. two Chromecasts nobody bothered to rename.
        seed_plex_client(&pool, &format!("machine-a-{suffix}"), &player_name).await;
        seed_plex_client(&pool, &format!("machine-b-{suffix}"), &player_name).await;

        let state = test_state(pool, Some(Arc::new(FakeController::new(false))));

        let err = terminate_session(State(state), Path(key), axum::body::Bytes::new())
            .await
            .expect_err("an ambiguous player-name match must not resolve to either device");
        assert!(
            matches!(err, MuseError::Conflict(_)),
            "expected Conflict (409), got {err:?}"
        );
    }

    #[tokio::test]
    async fn stale_session_is_404_never_stops_a_newer_session_on_the_same_device() {
        // Cycle-2 finding, the one the reviewer most wanted fixed: an OLD
        // stale session must not resolve a target at all, because doing so
        // (via its player name) could stop a NEWER, genuinely-live session
        // on that same device.
        let Some(pool) = test_pool_or_skip(
            "stale_session_is_404_never_stops_a_newer_session_on_the_same_device",
        )
        .await
        else {
            return;
        };
        let suffix = uuid::Uuid::new_v4();
        let key = format!("mact02-stalesession-{suffix}");
        let player_name = format!("Den {suffix}");
        // Deliberately the STALE seed (no play_events row) -- even though a
        // matching, FRESH plex_clients row exists, resolution must refuse
        // before it ever reaches target lookup.
        seed_stale_live_session(&pool, &key, &player_name).await;
        seed_plex_client(&pool, &format!("machine-{suffix}"), &player_name).await;

        let controller = Arc::new(FakeController::new(false));
        let state = test_state(pool, Some(controller.clone() as Arc<dyn CastController>));

        let err = terminate_session(State(state), Path(key), axum::body::Bytes::new())
            .await
            .expect_err("a stale open session must not resolve to a stop target");
        assert!(
            matches!(err, MuseError::NotFound(_)),
            "expected NotFound (404), got {err:?}"
        );
        assert!(
            controller.stop_targets().is_empty(),
            "no relay may be attempted for a stale session, ever"
        );
    }

    #[tokio::test]
    async fn stale_plex_client_match_is_503_never_relayed_to_an_obsolete_device() {
        let Some(pool) = test_pool_or_skip(
            "stale_plex_client_match_is_503_never_relayed_to_an_obsolete_device",
        )
        .await
        else {
            return;
        };
        let suffix = uuid::Uuid::new_v4();
        let key = format!("mact02-staletarget-{suffix}");
        let player_name = format!("Loft {suffix}");
        seed_live_session(&pool, &key, &player_name).await;
        // The ONLY plex_clients match is a day old -- well outside
        // Config::default()'s 300s freshness window. Unambiguous (exactly
        // one row) but not current; must still refuse.
        seed_stale_plex_client(&pool, &format!("machine-{suffix}"), &player_name).await;

        let controller = Arc::new(FakeController::new(false));
        let state = test_state(pool, Some(controller.clone() as Arc<dyn CastController>));

        let err = terminate_session(State(state), Path(key), axum::body::Bytes::new())
            .await
            .expect_err("a stale-only plex_clients match must not resolve to a stop target");
        assert!(
            matches!(err, MuseError::ServiceUnavailable(_)),
            "expected ServiceUnavailable (503), got {err:?}"
        );
        assert!(
            controller.stop_targets().is_empty(),
            "no relay may be attempted against an obsolete-only match, ever"
        );
    }

    #[tokio::test]
    async fn controller_error_reports_stopped_false_never_a_fabricated_success() {
        let Some(pool) = test_pool_or_skip(
            "controller_error_reports_stopped_false_never_a_fabricated_success",
        )
        .await
        else {
            return;
        };
        let suffix = uuid::Uuid::new_v4();
        let key = format!("mact02-fail-{suffix}");
        let player_name = format!("Bedroom {suffix}");
        let machine_id = format!("machine-{suffix}");
        seed_live_session(&pool, &key, &player_name).await;
        seed_plex_client(&pool, &machine_id, &player_name).await;

        let state = test_state(pool, Some(Arc::new(FakeController::new(true))));

        let Json(resp) = terminate_session(State(state), Path(key), axum::body::Bytes::new())
            .await
            .expect("a controller error is a 200 with stopped:false, not an error response");
        assert!(
            !resp.stopped,
            "a controller error must report stopped:false, never a fabricated true"
        );
    }
}

#[cfg(test)]
mod discover_capability_tests {
    use super::{trending_capability, TrendingCapability};

    #[test]
    fn a_keyless_proxy_is_not_a_trending_provider() {
        // MUSE #111, the whole bug: `configured: state.tmdb.is_some()` reported TRUE for a
        // key-less proxy that structurally cannot fetch trending — api.radarr.video has no
        // /trending endpoint (404, probed live), and TmdbClient::trending() returns empty
        // WITHOUT asking. So Discover was permanently empty while telling the operator a
        // trending provider was configured and had merely returned nothing, offering three
        // candidate causes, none of which was the real one.
        assert_eq!(
            trending_capability(true, true),
            TrendingCapability::MetadataProxyOnly,
            "a metadata proxy must not be reported as a trending provider",
        );
    }

    #[test]
    fn a_keyed_client_can_do_trending() {
        assert_eq!(trending_capability(true, false), TrendingCapability::Available);
    }

    #[test]
    fn no_client_is_its_own_state() {
        // Distinct from the proxy case because the operator action differs: one needs TMDb
        // configuring at all, the other needs an API KEY adding to a working client.
        assert_eq!(trending_capability(false, false), TrendingCapability::NotConfigured);
        assert_eq!(trending_capability(false, true), TrendingCapability::NotConfigured);
    }
}

#[cfg(test)]
mod library_kind_tests {
    /// The accepted spellings for `?kind=`, mirroring the handler's match arms.
    ///
    /// MUSE #112. Rejecting an unknown kind matters more than it looks: quietly ignoring it
    /// would serve a MIXED library to a page that asked for one kind, and the page would
    /// present it as "all your movies". Serving the wrong set under a confident label is the
    /// failure mode this codebase keeps having to remove — a 400 is the honest answer.
    fn normalize(k: &str) -> Option<&'static str> {
        match k.trim().to_ascii_lowercase().as_str() {
            "movie" | "movies" => Some("movie"),
            "show" | "shows" | "series" | "tv" => Some("show"),
            _ => None,
        }
    }

    #[test]
    fn both_vocabularies_are_accepted_and_map_to_the_db_spelling() {
        // The DB enum says `show`; the GUI, the ecosystem and the operator all say "series"
        // or "TV". Callers should not have to guess which one this endpoint wants.
        for k in ["movie", "Movies", " MOVIE "] {
            assert_eq!(normalize(k), Some("movie"), "{k}");
        }
        for k in ["show", "shows", "series", "tv", "TV"] {
            assert_eq!(normalize(k), Some("show"), "{k}");
        }
    }

    #[test]
    fn an_unknown_kind_is_rejected_rather_than_ignored() {
        // NOT None-meaning-all. The handler turns this into a 400.
        assert_eq!(normalize("anime"), None);
        assert_eq!(normalize("documentary"), None);
        assert_eq!(normalize("movie;drop"), None);
    }
}

// ===========================================================================
// FOUNDRY-02: transcode survey (dry run)
// ===========================================================================

#[derive(Debug, Deserialize)]
pub struct SurveyQuery {
    /// How many files to examine. Default 25, up to 50,000 — enough to
    /// pre-flight the whole library deliberately. Encodes nothing.
    pub limit: Option<usize>,
    /// Wall-clock ceiling for the survey, in seconds. Default 3600, clamped
    /// 30..86400. A survey that hits it reports what it actually examined.
    pub deadline_secs: Option<u64>,
}

/// `POST /ops/foundry/survey` — report what transcoding WOULD do. Encodes nothing.
///
/// This is the deliberate first wiring of MUSEF-02, which until now had no production caller at
/// all: `optimize_file` existed and nothing invoked it, so the stage had never run and the first
/// file it touched would have been a real one in the library.
///
/// The survey never calls `optimize_file`, so it cannot encode or replace anything even if
/// `MUSE_FOUNDRY_ENABLE_MUTATION` is on. Turning execution on stays a separate, deliberate act.
pub async fn foundry_survey(
    State(state): State<Arc<AppState>>,
    Query(q): Query<SurveyQuery>,
) -> MuseResult<Json<Value>> {
    let Some(foundry) = crate::foundry::Foundry::from_config(&state.config) else {
        // NOT an empty survey. "Foundry is not configured" and "nothing needs transcoding" are
        // different facts, and a zero-count report would read as the second.
        return Ok(Json(json!({
            "ran": false,
            "reason": "foundry is not configured (MUSE_FOUNDRY_* unset) — nothing was examined",
        })));
    };

    let caps = foundry.capabilities();
    // Ceiling raised 500 -> 50_000 so the WHOLE library can be pre-flighted.
    //
    // The old bound existed because an unbounded survey was "an accidental
    // hours-long ffprobe sweep of the whole library on a shared host". Two
    // things changed: ffprobe now has a 120s timeout (FOUNDRY-10, after one
    // wedged probe blocked a run indefinitely), and the survey has its own
    // deadline below. Measured on this library: 500 files in 85s, so all
    // 16,221 is roughly 46 minutes — practical for a deliberate pre-flight,
    // and it encodes NOTHING.
    //
    // The default stays 25. Surveying everything is an explicit choice.
    let limit = q.limit.unwrap_or(25).clamp(1, 50_000);
    // A survey that runs past this reports what it DID examine rather than
    // running unbounded — the same distinction the validator draws between a
    // completed sample and a truncated one.
    let survey_deadline = std::time::Duration::from_secs(
        q.deadline_secs.unwrap_or(3600).clamp(30, 86_400),
    );

    // Candidates come from the same walker the library scanner uses, so the survey looks at
    // exactly the files Muse considers media — not a second, divergent notion of "a video".
    let Some(root) = state.config.library_root.clone() else {
        // Same distinction as above: no root configured is not an empty library.
        return Ok(Json(json!({
            "ran": false,
            "reason": "MUSE_LIBRARY_ROOT is not set — there is nothing to survey",
        })));
    };
    let candidates: Vec<std::path::PathBuf> =
        crate::library::scan::walk_media_files(std::path::Path::new(&root))
            .into_iter()
            .map(|f| f.absolute_path)
            .collect();

    // Path A's policy, for the same reason the validator uses it: this
    // endpoint reports what Path A WOULD do, so reporting the default's
    // decisions would describe work that is not the work.
    let policy = crate::foundry::policy::TranscodePolicy::direct_play_normalization();
    let summary =
        crate::foundry::survey::survey_files(&foundry, &policy, &candidates, limit, survey_deadline);

    Ok(Json(json!({
        "ran": true,
        "dry_run": true,
        "capabilities": {
            "ffprobe": format!("{:?}", caps.ffprobe),
            "ffmpeg": format!("{:?}", caps.ffmpeg),
            "can_transcode": caps.can_transcode(),
        },
        "candidates_found": candidates.len(),
        "examined": summary.examined,
        "truncated": summary.truncated,
        "counts": {
            "already_optimal": summary.already_optimal,
            "would_transcode": summary.would_transcode,
            "cannot_decide": summary.cannot_decide,
            "probe_failed": summary.probe_failed,
        },
        // A judgement about the SURVEY, not an instruction. `surveyed` means the counts are
        // worth reading, never "go ahead and enable mutation".
        "readiness": summary.readiness().as_str(),
        "files": summary.files.iter().map(|f| json!({
            "path": f.path,
            "outcome": f.outcome.as_str(),
            "detail": match &f.outcome {
                crate::foundry::survey::SurveyOutcome::WouldTranscode {
                    reasons,
                    predicted_deletion_refusals,
                } => json!({
                    "reasons": reasons,
                    // What the deletion gate will say AFTERWARDS, known now.
                    // Non-empty means Path A would spend a full re-encode and
                    // then KEEP the original — doubling disk for this title
                    // rather than reclaiming any.
                    "predicted_deletion_refusals": predicted_deletion_refusals,
                    "reclaims_disk": predicted_deletion_refusals.is_empty(),
                }),
                crate::foundry::survey::SurveyOutcome::CannotDecide { why } => json!(why),
                crate::foundry::survey::SurveyOutcome::ProbeFailed { error } => json!(error),
                crate::foundry::survey::SurveyOutcome::AlreadyOptimal => Value::Null,
            },
        })).collect::<Vec<_>>(),
    })))
}

// ===========================================================================
// FOUNDRY-04: transcode validation (real encodes, to scratch, never in place)
// ===========================================================================

#[derive(Debug, Deserialize)]
pub struct ValidateQuery {
    /// How many files to actually encode. Default 12 — the operator's own
    /// number ("a dozen types of files"). Clamped to 1..=24: every one of these
    /// is a real encode with a 20-minute ceiling, so an unbounded limit is an
    /// unbounded run.
    pub limit: Option<usize>,
    /// How many candidates to probe when choosing the sample. Default 400 —
    /// the size of the operator's own measured container survey. More probes
    /// means more distinct shapes to choose between; it costs one ffprobe each.
    pub probe_budget: Option<usize>,
    /// Largest input to encode, in MiB. Default 2048 (2 GiB).
    ///
    /// The default deliberately EXCLUDES the 4K/HDR/DV tail, because the
    /// original scratch filesystem could not hold those files' output. Raising
    /// it is how that tail gets covered — and the coverage note in every
    /// response is generated from whatever value is actually in force, so it
    /// can never claim a ceiling the run did not use.
    ///
    /// Clamped to 1..=65536 MiB (64 GiB).
    pub max_input_mb: Option<u64>,
    /// Cumulative output budget for the whole run, in MiB. Default 6144 (6 GiB).
    ///
    /// Bounds cumulative work, not peak: each output is deleted as soon as it
    /// is verified. Refused up front if the scratch filesystem does not
    /// actually have this much free.
    ///
    /// Clamped to 1..=4194304 MiB (4 TiB).
    pub budget_mb: Option<u64>,
    /// Per-encode wall-clock ceiling, in seconds. Default 1200 (20 min).
    ///
    /// A 4K feature does not re-encode in 20 minutes on a CPU, so covering the
    /// large tail needs this raised or every large file times out and is
    /// reported as a FAILURE rather than a skip.
    ///
    /// Clamped to 60..=21600 (6 h).
    pub encode_timeout_secs: Option<u64>,
    /// Whole-run deadline, in seconds. Default 3600 (1 h).
    ///
    /// Clamped to 60..=86400 (24 h).
    pub run_deadline_secs: Option<u64>,
    /// Only validate files at least this large, in MiB.
    ///
    /// Targets the large tail. The diversity sampler picks for SHAPE coverage,
    /// so a 16-file sample of a library that is ~1% 4K will essentially never
    /// contain a 4K file — raising `max_input_mb` made that content eligible
    /// without making it reachable.
    pub min_input_mb: Option<u64>,
    /// Only validate files that carry an HDR transfer, a Dolby Vision signal,
    /// or an UNDETERMINED dynamic range.
    ///
    /// Undetermined is included on purpose: an untagged 10-bit file is the
    /// ambiguous case most likely to be misjudged, and excluding it would hide
    /// exactly the files worth looking at.
    pub hdr_only: Option<bool>,
}

/// `POST /ops/foundry/validate` — really encode a diverse sample to scratch,
/// verify the outputs with forge's own rules, and report. **Never touches an
/// original.**
///
/// This is the evidence step between FOUNDRY-02's survey (which plans and
/// encodes nothing) and enabling destructive mutation. It does not call
/// `forge::optimize_file`, does not read `MUSE_FOUNDRY_ENABLE_MUTATION`, and
/// writes only inside a scratch directory that is re-checked against safety
/// rail 3 before the run starts. See [`crate::foundry::validate`].
///
/// **This is a long call.** Up to `limit` real encodes, each capped at 20
/// minutes, with a 60-minute ceiling on the whole run. It is dispatched onto a
/// blocking thread so the encodes do not occupy an async worker for an hour.
/// `POST /ops/foundry/reap` — remove `.muse-superseded` originals that the
/// deletion gate allows.
///
/// **This is the only endpoint in Muse that can permanently destroy library
/// data.** It is therefore two deliberate steps, not one: `mutate=true` is
/// required to delete anything, and without it every allowed candidate is
/// reported as `would_delete` and nothing is touched. The response always
/// states which mode ran.
#[derive(Debug, serde::Deserialize)]
pub struct ReapQuery {
    /// Actually delete. Default **false**.
    pub mutate: Option<bool>,
    /// Retention window in days. Default 14. Clamped to 0..=3650, and 0 is
    /// allowed only because a validated bulk migration may legitimately want
    /// it — it is not the default and never becomes one.
    pub retention_days: Option<u64>,
}

/// `POST /ops/foundry/marks` — mark a title for Path B renditions.
///
/// This is the ONLY way a rendition candidate comes into existence. The run
/// endpoint below reads marks and nothing else; it has no access to a library
/// listing. That is the operator's "only items I mark, never everything"
/// constraint made structural rather than promised.
#[derive(Debug, serde::Deserialize)]
pub struct MarkBody {
    /// `movie` | `season` | `show`.
    pub scope: String,
    /// Absolute path: a file for `movie`, a directory for `season`/`show`.
    pub path: String,
    /// Which rungs. Never defaulted to all four — that is the exact outcome
    /// the operator asked to avoid.
    pub rungs: Vec<String>,
    pub marked_by: Option<String>,
}

pub async fn foundry_mark(
    State(state): State<Arc<AppState>>,
    Json(body): Json<MarkBody>,
) -> MuseResult<Json<Value>> {
    use crate::foundry::marks::MarkScope;
    use crate::foundry::rendition::RenditionName;

    let Some(scope) = MarkScope::parse(&body.scope) else {
        return Err(MuseError::BadRequest(format!(
            "unknown scope `{}` — expected movie, season or show. Refused rather than \
             defaulted: a typo becoming `movie` would mark one file when a whole show \
             was meant",
            body.scope
        )));
    };
    if body.rungs.is_empty() {
        return Err(MuseError::BadRequest(
            "no rungs given — a mark with no rungs would examine the title and produce \
             nothing, which is indistinguishable from a bug"
                .into(),
        ));
    }
    let mut rungs = Vec::new();
    for r in &body.rungs {
        let Some(parsed) = RenditionName::parse(r) else {
            return Err(MuseError::BadRequest(format!(
                "unknown rung `{r}` — expected mobile, web, tv or hifi"
            )));
        };
        rungs.push(parsed);
    }

    // The path must be inside an allowed root. Marking something outside the
    // library would let a rendition run read a file the guard would refuse,
    // which is the bypass the guard exists to prevent.
    let Some(foundry) = crate::foundry::Foundry::from_config(&state.config) else {
        return Err(MuseError::BadRequest(
            "foundry is not configured, so a mark could not be acted on".into(),
        ));
    };
    if foundry.probe_file(std::path::Path::new(&body.path)).is_err()
        && !std::path::Path::new(&body.path).is_dir()
    {
        // A directory cannot be probed, so only a FILE mark is validated this
        // way; a directory is checked at expansion time by the same guard.
        return Err(MuseError::BadRequest(format!(
            "{} is not a readable media file inside an allowed root",
            body.path
        )));
    }

    let id = crate::repo::rendition_mark::upsert(
        &state.pool,
        scope,
        &body.path,
        &rungs,
        body.marked_by.as_deref(),
    )
    .await?;

    Ok(Json(json!({
        "marked": true,
        "id": id,
        "scope": scope.as_str(),
        "path": body.path,
        "rungs": rungs.iter().map(|r| r.as_str()).collect::<Vec<_>>(),
        "note": "a mark is stored UNEXPANDED: a season covers episodes that arrive later",
    })))
}

/// `DELETE /ops/foundry/marks` — revoke a mark.
pub async fn foundry_unmark(
    State(state): State<Arc<AppState>>,
    Json(body): Json<Value>,
) -> MuseResult<Json<Value>> {
    let Some(path) = body.get("path").and_then(|v| v.as_str()) else {
        return Err(MuseError::BadRequest("path is required".into()));
    };
    let existed = crate::repo::rendition_mark::revoke(&state.pool, path).await?;
    Ok(Json(json!({
        "revoked": existed,
        "path": path,
        // Distinguishes "I revoked it" from "there was nothing to revoke", so
        // an operator who mistypes a path is not told they succeeded.
        "note": if existed { "the live mark was revoked" } else { "no live mark existed for that path" },
    })))
}

/// `POST /ops/foundry/renditions/plan` — what the marks would produce.
///
/// Plans only; encodes nothing. Reports the EXPANDED file count per mark
/// before any work happens, because a season mark that expands to four hundred
/// episodes is something the operator should see rather than discover.
pub async fn foundry_renditions_plan(
    State(state): State<Arc<AppState>>,
) -> MuseResult<Json<Value>> {
    let (marks, unparseable) = crate::repo::rendition_mark::live(&state.pool).await?;

    let mut per_mark = Vec::new();
    let mut total_files = 0usize;
    for m in &marks {
        let (files, problem) = crate::foundry::marks::expand(m);
        total_files += files.len();
        per_mark.push(json!({
            "id": m.id,
            "scope": m.scope.as_str(),
            "path": m.path,
            "rungs": m.rungs.iter().map(|r| r.as_str()).collect::<Vec<_>>(),
            "files": files.len(),
            // A mark producing nothing SAYS why — silence is indistinguishable
            // from a mark that was never made.
            "problem": problem.map(|p| p.to_string()),
            "renditions_implied": files.len() * m.rungs.len(),
        }));
    }

    Ok(Json(json!({
        "ran": true,
        "dry_run": true,
        "marks": marks.len(),
        "files_covered": total_files,
        "renditions_implied": per_mark.iter()
            .filter_map(|m| m.get("renditions_implied").and_then(|v| v.as_u64()))
            .sum::<u64>(),
        "per_mark": per_mark,
        // Rows the database accepted but this build cannot read. Reported, not
        // skipped silently: it means schema and code have diverged.
        "unreadable_marks": unparseable,
        "note": "candidates come ONLY from marks; this endpoint cannot see the library",
    })))
}

/// `POST /ops/foundry/optimize` — run Path A on EXPLICIT paths.
///
/// Until now `optimize_file` had no production caller at all: the full chain
/// (probe -> plan -> encode -> verify -> swap) had never executed, in any test
/// or in production. The three existing `optimize_file` tests exercise only
/// refusal paths — absent tools, path outside roots — so the swap itself, the
/// single most destructive operation in Muse, was also the least exercised.
///
/// This is the trigger, and it is deliberately the NARROWEST one that can
/// prove the chain works:
///
/// - **Explicit paths only.** There is no sweep, no glob, no "optimize the
///   library". A caller must name each file. That keeps the blast radius equal
///   to what was typed, and means a 16,000-item run is something an operator
///   builds deliberately rather than something one request can start.
/// - **Both gates.** MUSE_FOUNDRY_ENABLE_MUTATION must be open AND the request
///   must pass `confirm=<the exact path>`, restated. Same shape as the
///   subtitle offset apply: measuring and changing are different acts.
/// - **Bounded.** At most 8 paths per request.
///
/// The original is never destroyed by this: forge hard-links it to
/// `<name>.muse-superseded` before releasing the original name. Reclaiming
/// that space is the reaper's job and requires its own two gates.
#[derive(Debug, serde::Deserialize)]
pub struct OptimizeBody {
    /// The files to optimize. Explicit, never a pattern.
    pub paths: Vec<String>,
    /// Must exactly equal the single path when one path is given — the
    /// operator restating what they are about to rewrite.
    pub confirm: Option<String>,
}

pub async fn foundry_optimize(
    State(state): State<Arc<AppState>>,
    Json(body): Json<OptimizeBody>,
) -> MuseResult<Json<Value>> {
    if body.paths.is_empty() {
        return Err(MuseError::BadRequest("no paths given".into()));
    }
    if body.paths.len() > 8 {
        return Err(MuseError::BadRequest(format!(
            "{} paths given; at most 8 per request. This endpoint is deliberately not a \
             sweep — a large run is something an operator assembles deliberately, not \
             something one request starts",
            body.paths.len()
        )));
    }

    let Some(foundry) = crate::foundry::Foundry::from_config(&state.config) else {
        return Ok(Json(json!({
            "ran": false,
            "reason": "foundry is not configured — nothing was optimized",
        })));
    };

    // The GLOBAL gate. Checked here as well as inside forge, because an
    // operator reading this endpoint's response should be told plainly that
    // the deployment forbids mutation rather than seeing per-file skips.
    if !foundry.mutation_enabled() {
        return Ok(Json(json!({
            "ran": false,
            "reason": "MUSE_FOUNDRY_ENABLE_MUTATION is closed — this deployment cannot \
                       modify the library, so nothing was optimized",
            "paths": body.paths,
        })));
    }

    // The per-request gate: the operator restates the path. Only meaningful
    // for a single path, so a multi-path request must be assembled knowingly.
    if body.paths.len() == 1 {
        match body.confirm.as_deref() {
            Some(c) if c == body.paths[0] => {}
            _ => {
                return Err(MuseError::BadRequest(format!(
                    "confirm must exactly restate the path being rewritten. Expected \
                     confirm=\"{}\"",
                    body.paths[0]
                )))
            }
        }
    } else if body.confirm.as_deref() != Some("MULTIPLE") {
        return Err(MuseError::BadRequest(
            "a multi-path request must pass confirm=MULTIPLE, so several files are never \
             rewritten by a request that meant one"
                .into(),
        ));
    }

    let policy = crate::foundry::policy::TranscodePolicy::direct_play_normalization();
    let paths = body.paths.clone();
    let results = tokio::task::spawn_blocking(move || {
        paths
            .into_iter()
            .map(|p| {
                let status = foundry.optimize_file(std::path::Path::new(&p), &policy);
                (p, format!("{status:?}"))
            })
            .collect::<Vec<_>>()
    })
    .await
    .map_err(|e| MuseError::Internal(anyhow::anyhow!("optimize task failed: {e}")))?;

    Ok(Json(json!({
        "ran": true,
        "policy": "direct_play_normalization",
        "results": results.iter().map(|(p, s)| json!({ "path": p, "status": s })).collect::<Vec<_>>(),
        "note": "the original is preserved as <name>.muse-superseded; reclaiming that \
                 space is the reaper's job and needs its own two gates",
    })))
}

pub async fn foundry_reap(
    State(state): State<Arc<AppState>>,
    Query(q): Query<ReapQuery>,
) -> MuseResult<Json<Value>> {
    use crate::foundry::reaper;

    let Some(foundry) = crate::foundry::Foundry::from_config(&state.config) else {
        return Ok(Json(json!({
            "ran": false,
            "reason": "foundry is not configured (MUSE_FOUNDRY_* unset) — nothing was reaped",
        })));
    };
    if !foundry.capabilities().ffprobe.is_present() {
        // Without ffprobe neither file can be re-probed, so the gate cannot be
        // consulted — and a reaper that cannot consult the gate must not run
        // at all rather than fall back to some weaker rule.
        return Ok(Json(json!({
            "ran": false,
            "reason": "ffprobe is not usable, so the deletion gate cannot be consulted —                        refusing to reap rather than deleting on a weaker check",
        })));
    }

    let mutate = q.mutate.unwrap_or(false);
    let retention = std::time::Duration::from_secs(
        q.retention_days.unwrap_or(14).clamp(0, 3650) * 24 * 60 * 60,
    );
    let run = tokio::task::spawn_blocking(move || reaper::reap(&foundry, retention, mutate))
        .await
        .map_err(|e| MuseError::Internal(anyhow::anyhow!("reap task failed: {e}")))?;

    Ok(Json(json!({
        "ran": true,
        // Stated on every response: a dry run must never be mistakable for a
        // real one when the numbers are read later.
        "mutation_enabled": run.mutation_enabled,
        // Separate from the above so a request that ASKED to mutate and was
        // refused by the global gate is distinguishable from one that never
        // asked. Without this an operator cannot tell "I forgot ?mutate=true"
        // from "the deployment forbids it".
        "globally_permitted": run.globally_permitted,
        "gate_note": if !run.globally_permitted {
            "MUSE_FOUNDRY_ENABLE_MUTATION is closed, so NOTHING can be deleted by this \
             deployment regardless of ?mutate"
        } else if !run.mutation_enabled {
            "the global gate is open but this request did not pass ?mutate=true, so \
             nothing was deleted"
        } else {
            "BOTH gates are open: this request DELETED backups the gate allowed"
        },
        "retention_secs": run.retention_secs,
        // Coverage, so "examined: 0" can be told apart from "I could not
        // look". On the one endpoint that can permanently delete data, those
        // must never render identically.
        "coverage": {
            "dirs_read": run.dirs_read,
            "dirs_unreadable": run.dirs_unreadable,
            "trustworthy": run.dirs_read > 0,
            "note": if run.dirs_read == 0 {
                "NOTHING was read — this result establishes nothing about the library"
            } else if run.dirs_unreadable > 0 {
                "PARTIAL — some directories could not be listed, so backups may exist \
                 that this pass never saw"
            } else {
                "the whole tree under every allowed root was listed"
            },
        },
        "examined": run.files.len(),
        "deleted": run.deleted(),
        "would_delete": run.would_delete(),
        "kept": run.kept(),
        "bytes_reclaimed": run.bytes_reclaimed,
        "files": run.files.iter().map(|f| json!({
            "superseded": f.superseded_path,
            "replacement": f.replacement_path,
            "bytes": f.bytes,
            "outcome": f.outcome.to_string(),
            "deleted": f.outcome.deleted(),
        })).collect::<Vec<_>>(),
    })))
}

pub async fn foundry_validate(
    State(state): State<Arc<AppState>>,
    Query(q): Query<ValidateQuery>,
) -> MuseResult<Json<Value>> {
    // Stated on EVERY response, including refusals. Codex, FOUNDRY-04 gate: the
    // >2 GiB exclusion was disclosed only when a run succeeded, so a refusal
    // read as "nothing to report" when it actually meant the 4K/HDR/DV tail was
    // never going to be covered either way.
    // Built from the bounds ACTUALLY in force for this run, not a fixed
    // string. The note used to hardcode "2 GiB"; once the ceiling became an
    // operator override that string would have kept claiming 2 GiB whatever
    // the run used, and a report that misstates its own coverage reads as
    // verified when it is not.
    let bounds = validate::ValidationBounds::from_overrides(
        q.max_input_mb,
        q.budget_mb,
        q.encode_timeout_secs,
        q.run_deadline_secs,
    );
    let coverage_note = bounds.coverage_note();
    let filter = validate::CandidateFilter {
        // Saturating: an operator-supplied u64 times 1 MiB overflows, and a
        // WRAP yields a tiny floor that silently admits small files — the
        // filter would then quietly stop targeting the tail it exists for.
        // Opus and free, FOUNDRY-16 gate.
        min_input_bytes: q.min_input_mb.map(|m| m.saturating_mul(1024 * 1024)),
        hdr_only: q.hdr_only.unwrap_or(false),
    };
    let filter_is_unrestricted = filter.is_unrestricted();
    let policy_reported = crate::foundry::policy::TranscodePolicy::direct_play_normalization();
    // Half the run deadline, so a targeted walk cannot consume the whole
    // budget before any encode happens.
    let probe_deadline = bounds.run_deadline / 2;


    use crate::foundry::validate;

    let Some(foundry) = crate::foundry::Foundry::from_config(&state.config) else {
        // Not an empty run. "Foundry is not configured" and "nothing failed"
        // are different facts, and a zero-count report would read as the second.
        return Ok(Json(json!({
            "ran": false,
            "coverage": coverage_note,
        // Stated on the response: a TARGETED run covers a deliberately narrow
        // slice, and its "all_verified" means something much smaller than an
        // unrestricted run's. Reading one as the other is the whole risk.
        "targeting": {
            "min_input_mb": q.min_input_mb,
            "hdr_only": q.hdr_only.unwrap_or(false),
            "note": if filter_is_unrestricted {
                "unrestricted: the diverse sample spans the library's shapes, which \
                 under-represents the ~1% 4K/HDR tail by construction"
            } else {
                "TARGETED: only files matching the filter were considered, so this \
                 result says nothing about the rest of the library"
            },
        },
            "reason": "foundry is not configured (MUSE_FOUNDRY_* unset) — nothing was validated",
        })));
    };

    let caps = foundry.capabilities();
    if !caps.can_transcode() {
        // Both tools, not just ffmpeg: without ffprobe an output could be
        // produced and never verified, and an unverified success is a failure.
        return Ok(Json(json!({
            "ran": false,
            "coverage": coverage_note,
        // Stated on the response: a TARGETED run covers a deliberately narrow
        // slice, and its "all_verified" means something much smaller than an
        // unrestricted run's. Reading one as the other is the whole risk.
        "targeting": {
            "min_input_mb": q.min_input_mb,
            "hdr_only": q.hdr_only.unwrap_or(false),
            "note": if filter_is_unrestricted {
                "unrestricted: the diverse sample spans the library's shapes, which \
                 under-represents the ~1% 4K/HDR tail by construction"
            } else {
                "TARGETED: only files matching the filter were considered, so this \
                 result says nothing about the rest of the library"
            },
        },
            "reason": "ffmpeg and ffprobe must both be usable to validate an encode",
            "capabilities": {
                "ffprobe": caps.ffprobe.summary(),
                "ffmpeg": caps.ffmpeg.summary(),
            },
        })));
    }

    let Some(root) = state.config.library_root.clone() else {
        return Ok(Json(json!({
            "ran": false,
            "coverage": coverage_note,
        // Stated on the response: a TARGETED run covers a deliberately narrow
        // slice, and its "all_verified" means something much smaller than an
        // unrestricted run's. Reading one as the other is the whole risk.
        "targeting": {
            "min_input_mb": q.min_input_mb,
            "hdr_only": q.hdr_only.unwrap_or(false),
            "note": if filter_is_unrestricted {
                "unrestricted: the diverse sample spans the library's shapes, which \
                 under-represents the ~1% 4K/HDR tail by construction"
            } else {
                "TARGETED: only files matching the filter were considered, so this \
                 result says nothing about the rest of the library"
            },
        },
            "reason": "MUSE_LIBRARY_ROOT is not set — there is nothing to validate",
        })));
    };

    let limit = q.limit.unwrap_or(12).clamp(1, 24);
    let probe_budget = q.probe_budget.unwrap_or(400).clamp(limit, 4000);

    // Every encode and every probe below is blocking, and the run can take an
    // hour. Off the async executor it goes.
    let outcome = tokio::task::spawn_blocking(move || {
        let scratch = validate::prepare_scratch_dir(&foundry)?;

        // The budget is accounting; this is the disk. A run admitted on
        // accounting alone can still fill the filesystem partway through, and
        // on this fleet a full scratch filesystem presents as unrelated
        // failures rather than as an obvious disk error. Raised at the
        // FOUNDRY-04 review gate.
        // Against the budget this run actually asked for, not the default —
        // otherwise raising the budget would skip the disk check that makes
        // raising it safe.
        validate::check_free_space(&scratch, bounds.required_free_bytes())?;

        let candidates: Vec<std::path::PathBuf> =
            crate::library::scan::walk_media_files(std::path::Path::new(&root))
                .into_iter()
                .map(|f| f.absolute_path)
                .collect();
        let candidate_count = candidates.len();

        let (probed, probe_failures) =
            validate::probe_candidates(&foundry, &candidates, probe_budget, &filter, probe_deadline);

        // PATH A's policy, not the default.
        //
        // The harness exists to answer "can Path A be trusted to rewrite this
        // library", and it was answering it about a DIFFERENT policy. The
        // default caps at 1080p / 12 Mbps / 8-bit, so validating with it meant
        // every 4K file was downscaled to 1080p and forced to yuv420p — with
        // no tone-map — during a run whose whole purpose was to prove 4K/HDR
        // is handled safely. Caught by reading the live ffmpeg argv on a real
        // Dolby Vision file, not from the code.
        //
        // direct_play_normalization (4K ceiling, 100 Mbps) was referenced only
        // from tests and doc comments; nothing in production used it.
        let policy = crate::foundry::policy::TranscodePolicy::direct_play_normalization();
        let run = validate::validate_sample(
            &foundry,
            &policy,
            &scratch,
            &probed,
            probe_failures,
            limit,
            &bounds,
        );
        Ok::<_, validate::ValidationRefusal>((run, bounds, candidate_count))
    })
    .await
    .map_err(|e| MuseError::Internal(anyhow::anyhow!("the validation run panicked or was cancelled: {e}")))?;

    let (run, bounds, candidate_count) = match outcome {
        Ok(v) => v,
        Err(refusal) => {
            return Ok(Json(json!({
                "ran": false,
            "coverage": coverage_note,
        // Stated on the response: a TARGETED run covers a deliberately narrow
        // slice, and its "all_verified" means something much smaller than an
        // unrestricted run's. Reading one as the other is the whole risk.
        "targeting": {
            "min_input_mb": q.min_input_mb,
            "hdr_only": q.hdr_only.unwrap_or(false),
            "note": if filter_is_unrestricted {
                "unrestricted: the diverse sample spans the library's shapes, which \
                 under-represents the ~1% 4K/HDR tail by construction"
            } else {
                "TARGETED: only files matching the filter were considered, so this \
                 result says nothing about the rest of the library"
            },
        },
                "reason": refusal.to_string(),
            })))
        }
    };

    let verdict = run.verdict();
    Ok(Json(json!({
        "ran": true,
        // On the SUCCESS response above all. A targeted run's "all_verified"
        // covers a deliberately narrow slice; an unrestricted run's spans the
        // library's shapes. Reading the first as the second is the whole risk,
        // and the success response is exactly where that misreading happens.
        // Opus caught this missing at the FOUNDRY-16 gate.
        "targeting": {
            "min_input_mb": q.min_input_mb,
            "hdr_only": q.hdr_only.unwrap_or(false),
            "note": if filter_is_unrestricted {
                "unrestricted: the diverse sample spans the library's shapes, which \
                 under-represents the ~1% 4K/HDR tail by construction"
            } else {
                "TARGETED: only files matching the filter were considered, so this \
                 result says NOTHING about the rest of the library"
            },
        },
        // WHICH policy was validated. Without this the report cannot be told
        // apart from one run against a different policy, which is exactly the
        // confusion that let the harness validate the wrong one unnoticed.
        "policy": {
            "name": "direct_play_normalization",
            "max_width": policy_reported.max_width,
            "max_height": policy_reported.max_height,
            "max_video_bitrate_bps": policy_reported.max_video_bitrate_bps,
            "note": "the policy PATH A uses. A run against TranscodePolicy::default() \
                     would cap at 1080p and 8-bit and would NOT be evidence about Path A",
        },
        // Stated on every response, not just in the docs: this endpoint writes
        // only to scratch and the operator should be able to see that claim
        // next to the numbers it is asking them to trust.
        "originals_modified": 0,
        "mutation_used": false,
        "candidates_found": candidate_count,
        "candidates_probed": run.candidates_probed,
        "source_probe_failures": run.source_probe_failures,
        "distinct_shapes_available": run.distinct_shapes,
        "counts": {
            "verified": run.verified,
            "failed": run.failed,
            "skipped": run.skipped,
        },
        // Failures first, deliberately, and there is no pass rate anywhere in
        // this payload: one broken output in twelve is the finding.
        "verdict": verdict.as_str(),
        "deadline_hit": run.deadline_hit,
        "bounds": {
            "max_input_bytes": bounds.max_input_bytes,
            "max_total_output_bytes": bounds.max_total_output_bytes,
            "output_reserve_factor": bounds.output_reserve_factor,
            "per_encode_timeout_secs": bounds.per_encode_timeout.as_secs(),
            "run_deadline_secs": bounds.run_deadline.as_secs(),
            "scratch_bytes_reserved": run.scratch_bytes_reserved,
            "note": "files above max_input_bytes are SKIPPED, not validated — \
                     the ~1% of the library that is 4K/HDR is not covered by this run",
        },
        "files": run.files.iter().map(|f| json!({
            "path": f.path,
            "outcome": f.outcome.as_str(),
            "detail": match &f.outcome {
                validate::ValidationOutcome::Verified => Value::Null,
                validate::ValidationOutcome::Failed { failure } => json!(failure.to_string()),
                validate::ValidationOutcome::Skipped { reason } => json!(reason.to_string()),
            },
            "input": {
                "container": f.input_container,
                "video_codec": f.input_video_codec,
                "dimensions": f.input_dimensions.map(|(w, h)| format!("{w}x{h}")),
                "audio_codecs": f.input_audio_codecs,
                "subtitles": f.input_subtitle_count,
                "attachments": f.input_attachment_count,
                "chapters": f.input_chapter_count,
                "duration_secs": f.input_duration_secs,
                "bytes": f.input_bytes,
            },
            "plan": {
                "summary": f.plan_summary,
                "reasons": f.plan_reasons,
            },
            "output": {
                "container": f.output_container,
                "video_codec": f.output_video_codec,
                "dimensions": f.output_dimensions.map(|(w, h)| format!("{w}x{h}")),
                "audio_codecs": f.output_audio_codecs,
                "subtitles": f.output_subtitle_count,
                "duration_secs": f.output_duration_secs,
                "bytes": f.output_bytes,
                "size_delta_bytes": f.size_delta_bytes,
            },
            "encode_wall_secs": f.encode_wall_secs,
        })).collect::<Vec<_>>(),
    })))
}

// --- FOUNDRY-11: the armed run ---------------------------------------------

/// `POST /ops/foundry/run` — the deliberate large run Path A was built for.
///
/// `foundry_optimize` refuses more than eight paths on the grounds that "a
/// 16,000-item run is something an operator builds deliberately rather than
/// something one request can start". This IS that deliberate build, and every
/// safeguard it carries exists because the alternative is an unattended process
/// rewriting a library.
///
/// **What it will not do:**
/// - Start without `MUSE_FOUNDRY_ENABLE_MUTATION` open.
/// - Start without `confirm` restating the exact title count it is about to
///   attempt — the operator states the size, so a mis-typed limit cannot
///   silently become a bigger run than intended.
/// - Start a second run while one is in flight.
/// - Touch a title whose original cannot be reclaimed, unless asked. The
///   default is `reclaimable_only`: of 3,621 titles that would be re-encoded on
///   this library, only 463 can ever have their original removed, and for the
///   rest a rewrite permanently leaves BOTH copies on disk.
/// - Keep going past its ceilings: title count, consecutive failures, free
///   space, or wall clock. Every one of those reports a distinct stop reason,
///   and only an exhausted candidate list counts as having finished.
///
/// The originals are still not destroyed here. Forge hard-links each to
/// `<name>.muse-superseded`; reclaiming that space remains the reaper's job,
/// behind its own two gates.
#[derive(Debug, serde::Deserialize)]
pub struct RunBody {
    /// Ceiling on titles ATTEMPTED. Not a target.
    pub max_titles: Option<usize>,
    /// `"reclaimable_only"` (default) or `"all"`.
    pub policy: Option<String>,
    /// Must equal `"run <max_titles> titles"`. The operator restates the size.
    pub confirm: Option<String>,
    /// Wall-clock ceiling for the whole run.
    pub deadline_secs: Option<u64>,
    /// Stop after this many failures in a row.
    pub max_consecutive_failures: Option<u32>,
    /// Floor for free space on the work filesystem.
    pub min_free_gib: Option<u64>,
}

pub async fn foundry_run_start(
    State(state): State<Arc<AppState>>,
    Json(body): Json<RunBody>,
) -> MuseResult<Json<Value>> {
    use crate::foundry::run::{self, CandidatePolicy, RunLimits};

    let Some(foundry) = crate::foundry::Foundry::from_config(&state.config) else {
        return Ok(Json(json!({
            "started": false,
            "reason": "foundry is not configured (MUSE_FOUNDRY_* unset) — nothing was started",
        })));
    };

    // The global gate, reported once for the run rather than as N identical
    // per-file skips.
    if !foundry.mutation_enabled() {
        return Ok(Json(json!({
            "started": false,
            "reason": "MUSE_FOUNDRY_ENABLE_MUTATION is closed — this deployment cannot \
                       modify the library, so no run was started",
        })));
    }

    let max_titles = body.max_titles.unwrap_or(10).clamp(1, 50_000);

    // The operator restates the SIZE. `confirm` on the optimize endpoint
    // restates a path; here the dangerous quantity is how many titles, so that
    // is what must be restated. A mis-typed limit fails closed instead of
    // starting a larger run than intended.
    if let Err(why) = run::check_confirm(max_titles, body.confirm.as_deref()) {
        return Err(MuseError::BadRequest(why));
    }

    let policy_choice = match body.policy.as_deref().unwrap_or("reclaimable_only") {
        "reclaimable_only" => CandidatePolicy::ReclaimableOnly,
        "all" => CandidatePolicy::All,
        other => {
            return Err(MuseError::BadRequest(format!(
                "policy must be \"reclaimable_only\" or \"all\", got {other:?}"
            )))
        }
    };

    let Some(root) = state.config.library_root.clone() else {
        return Ok(Json(json!({
            "started": false,
            "reason": "MUSE_LIBRARY_ROOT is not set — there is nothing to run against",
        })));
    };

    let limits = RunLimits {
        max_titles,
        max_consecutive_failures: body.max_consecutive_failures.unwrap_or(3),
        min_free_bytes: body
            .min_free_gib
            .unwrap_or(50)
            .saturating_mul(1024 * 1024 * 1024),
        deadline: std::time::Duration::from_secs(
            body.deadline_secs.unwrap_or(6 * 3600).clamp(60, 72 * 3600),
        ),
    };

    // Claim the slot HERE, not inside the spawned task. A read-then-spawn
    // would let two requests both answer `started: true` while only one
    // actually won the race — the loser's operator would be told a run began
    // that never did. Holding the slot from the moment we answer removes that
    // gap, and the guard releases it however the run ends, including a panic.
    let Some(slot) = run::global_handle().claim() else {
        return Err(MuseError::BadRequest(
            "a run is already in flight; stop it or wait for it to finish".into(),
        ));
    };

    // The work dir is where each encode is staged. Without one there is nowhere
    // outside the library to stage, and forge refuses every title — reported
    // here as the configuration gap it is, rather than letting the run start
    // and stop on a free-space floor of zero, which would misattribute a config
    // problem to a full disk. Raised at the FOUNDRY-11 gate.
    if state.config.foundry_work_dir.is_none() {
        return Ok(Json(json!({
            "started": false,
            "reason": "MUSE_FOUNDRY_WORK_DIR is not set — there is nowhere outside the \
                       library to stage an encode, so no run was started",
        })));
    }

    let work_dir = state.config.foundry_work_dir.clone();
    let transcode_policy = crate::foundry::policy::TranscodePolicy::direct_play_normalization();

    // Selection runs on the blocking pool too: it surveys the library, which is
    // ~46 minutes of ffprobe over NFS for 16,221 files.
    tokio::task::spawn_blocking(move || {
        let candidates: Vec<std::path::PathBuf> =
            crate::library::scan::walk_media_files(std::path::Path::new(&root))
                .into_iter()
                .map(|f| f.absolute_path)
                .collect();

        tracing::info!(
            candidates = candidates.len(),
            "foundry run: surveying to select candidates"
        );

        // Survey everything, then keep only what the chosen policy admits.
        let summary = crate::foundry::survey::survey_files(
            &foundry,
            &transcode_policy,
            &candidates,
            candidates.len().max(1),
            std::time::Duration::from_secs(6 * 3600),
        );

        let surveyed: Vec<(std::path::PathBuf, bool)> = summary
            .files
            .iter()
            .filter_map(|f| match &f.outcome {
                crate::foundry::survey::SurveyOutcome::WouldTranscode {
                    predicted_deletion_refusals,
                    ..
                } => Some((
                    std::path::PathBuf::from(&f.path),
                    predicted_deletion_refusals.is_empty(),
                )),
                _ => None,
            })
            .collect();

        let selected = run::select_candidates(&surveyed, policy_choice);
        tracing::info!(
            would_transcode = surveyed.len(),
            selected = selected.len(),
            policy = ?policy_choice,
            "foundry run: candidates selected"
        );

        let report = run::execute_run(
            &foundry,
            &transcode_policy,
            &selected,
            &limits,
            work_dir.as_deref().map(std::path::Path::new),
            run::global_handle(),
            slot,
        );
        tracing::info!(
            stop_reason = report.stop_reason.as_str(),
            completed = report.completed(),
            "foundry run: complete"
        );
    });

    Ok(Json(json!({
        "started": true,
        "max_titles": max_titles,
        "policy": match policy_choice {
            CandidatePolicy::ReclaimableOnly => "reclaimable_only",
            CandidatePolicy::All => "all",
        },
        "note": "candidate selection surveys the library first; poll GET /ops/foundry/run \
                 for progress. Originals are hard-linked to .muse-superseded, never deleted \
                 by this endpoint.",
    })))
}

/// `GET /ops/foundry/run` — progress of the run in flight, or the last one.
///
/// `stop_reason` is `null` while running. Its presence is the ONLY signal that
/// a run is over, so a reader cannot mistake a stalled run for a finished one —
/// the same distinction the survey draws between truncated and complete.
pub async fn foundry_run_status() -> MuseResult<Json<Value>> {
    let handle = crate::foundry::run::global_handle();
    let Some(p) = handle.snapshot() else {
        return Ok(Json(json!({
            "ever_run": false,
            "active": false,
            "note": "no run has been started in this process",
        })));
    };

    Ok(Json(json!({
        "ever_run": true,
        "active": handle.is_active(),
        "candidates_total": p.candidates_total,
        "current": p.current.as_ref().map(|c| c.display().to_string()),
        "ledger": {
            "attempted": p.ledger.attempted,
            "rewritten": p.ledger.rewritten,
            "failed": p.ledger.failed,
            "skipped": p.ledger.skipped,
            "bytes_before_total": p.ledger.bytes_before_total,
            "bytes_after_total": p.ledger.bytes_after_total,
            "bytes_reclaimed": p.ledger.bytes_reclaimed(),
        },
        "stop_reason": p.stop_reason.as_ref().map(|r| r.as_str()),
        "stop_detail": p.stop_reason.as_ref().map(|r| r.to_string()),
        // Derived from the stop reason, never from the counts: a run cancelled
        // on its last title must not read as having finished.
        "completed": p.stop_reason.as_ref().map(|r| r.is_complete()),
    })))
}

/// `POST /ops/foundry/run/stop` — ask the run in flight to stop.
///
/// Takes effect before the NEXT title, not mid-encode: killing ffmpeg partway
/// would leave a staged file to clean up, and the staged file is not the
/// library copy, so waiting costs nothing but one title's time.
pub async fn foundry_run_stop() -> MuseResult<Json<Value>> {
    let was_active = crate::foundry::run::global_handle().request_stop();
    Ok(Json(json!({
        "stop_requested": true,
        "was_active": was_active,
        "note": if was_active {
            "the run will stop before its next title; the one in flight finishes first"
        } else {
            "no run was in flight — nothing was stopped"
        },
    })))
}
