//! `GET /art/{kind}/{id}` — the artwork proxy (MUSE-27, spec §3.8/§4d-F).
//!
//! The whole point: the browser only ever requests a same-origin
//! `/art/{kind}/{id}` URL. The Plex token (`PLEX_TOKEN`) lives only in
//! `AppState.plex`'s `reqwest::Client` default headers and is used strictly
//! server-side, in [`crate::plex::PlexClient::fetch_image`] — it is never
//! echoed into the response body or headers here, and no image URL handed to
//! the browser (by the guide page or this handler) ever carries it.
//!
//! Flow per request:
//! 1. Cache hit (`artwork_cache.bytes` present) → serve straight from
//!    Postgres.
//! 2. Cache row exists with a `source_url` but no bytes yet (the guide
//!    registered the upstream path but nothing has fetched it) → fetch from
//!    Plex server-side, cache the bytes, serve.
//! 3. No usable row, or fetch failed, or Plex isn't configured → serve a
//!    tiny built-in placeholder image. Never a 404/500 for a missing cover —
//!    a blank/placeholder tile is strictly better UX in an EPG grid.
//!
//! ## Renditions (`?w=`, MUSE #100)
//! `?w=<ladder width>` serves an indexed, resized rendition instead of the
//! master — the Plex/Jellyfin model: capture once, derive many. The master is
//! still fetched/cached by the flow above (a rendition is derived FROM it, never
//! instead of it), then resized, stored as its own `artwork_cache` row, and
//! served with a strong-ish validator so the browser stops re-asking.
//!
//! Three properties are deliberate:
//! - An **off-ladder width is a 400**, not a clamp — see
//!   `artwork_render`'s module doc for why a free-form `?w=` is an
//!   amplification vector.
//! - Encoding runs under a **bounded semaphore** on a blocking thread. 240 grid
//!   tiles arriving at once must not start 240 multi-megabyte decodes; they
//!   queue. This is bounded concurrency, NOT per-key single-flight — two
//!   requests racing the same missing rendition may both encode it. That is
//!   acceptable because the bytes are a deterministic function of
//!   (master, width, format) and the store is an idempotent upsert; the cost is
//!   one wasted encode, not a corrupt cache.
//! - A rendition failure **falls back to the master**, never to an error: a
//!   too-small poster is better than a broken tile.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Semaphore;

use crate::web::artwork_render::{
    parse_width, render_jpeg, rendition_etag, RENDITION_CONTENT_TYPE, RENDITION_FORMAT,
};

use axum::{
    extract::{Path, Query, State},
    http::{header, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
};

/// A minimal 1x1 transparent PNG, embedded so the placeholder never depends
/// on network access or a bundled asset directory.
const PLACEHOLDER_PNG: &[u8] = &[
    137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 1, 0, 0, 0, 1, 8, 4, 0,
    0, 0, 181, 28, 12, 2, 0, 0, 0, 11, 73, 68, 65, 84, 120, 218, 99, 100, 248, 15, 0, 1, 5, 1, 1,
    39, 24, 227, 102, 0, 0, 0, 0, 73, 69, 78, 68, 174, 66, 96, 130,
];

/// The default artwork variant (matches `web::guide::COVER_VARIANT`); callers
/// may request another via `?variant=`.
const DEFAULT_VARIANT: &str = "poster";

/// MUSE #100: how many rendition encodes may run at once. Image decode+encode is
/// CPU-bound and a master can be ~2 MB, so this is the difference between a grid
/// mount costing a few cores briefly and it costing the box. Excess requests
/// WAIT (they do not fail) — the first mount of a cold library is slower than
/// the second, which is the correct trade.
const RENDITION_CONCURRENCY: usize = 4;

static RENDITION_SEMAPHORE: std::sync::LazyLock<Semaphore> =
    std::sync::LazyLock::new(|| Semaphore::new(RENDITION_CONCURRENCY));

pub async fn art_handler(
    State(state): State<Arc<crate::http::AppState>>,
    Path((kind, id)): Path<(String, i64)>,
    Query(params): Query<HashMap<String, String>>,
    headers: axum::http::HeaderMap,
) -> Response {
    let variant = params
        .get("variant")
        .map(String::as_str)
        .unwrap_or(DEFAULT_VARIANT);

    // MUSE #100: validate the rendition width BEFORE any I/O. An off-ladder
    // width is a client error and must not cost a database round-trip, let alone
    // a decode.
    let width = match parse_width(params.get("w").map(String::as_str)) {
        Ok(w) => w,
        Err(()) => return bad_width_response(),
    };

    // Serve an already-cached rendition without ever touching the master.
    if let Some(w) = width {
        match crate::repo::artwork_cache::get_rendition(
            &state.pool,
            &kind,
            id,
            variant,
            w,
            RENDITION_FORMAT,
        )
        .await
        {
            Ok(Some(row)) => {
                if let Some(bytes) = row.bytes {
                    let etag = row.etag.clone().unwrap_or_else(|| {
                        rendition_etag(&format!("{kind}:{id}:{variant}"), w, RENDITION_FORMAT)
                    });
                    if if_none_match_matches(&headers, &etag) {
                        return not_modified_response(&etag);
                    }
                    return rendition_response(bytes, &etag, true);
                }
            }
            Ok(None) => {}
            Err(e) => {
                // A cache-lookup failure must not deny the image — fall through
                // and serve/derive from the master.
                tracing::warn!(error = %e, kind = %kind, id, w, "rendition lookup failed; deriving");
            }
        }
    }

    let master = resolve_master(&state, &kind, id, variant).await;

    if let (Some(w), Some((ref bytes, _))) = (width, master.as_ref().map(|m| (m.0.clone(), m.1.clone()))) {
        if let Some(resp) = derive_rendition(&state, &kind, id, variant, w, bytes, &headers).await {
            return resp;
        }
        // Rendition failed — fall through to the master below rather than erroring.
    }

    match master {
        Some((bytes, content_type)) => image_response(&content_type, bytes, true),
        None => placeholder_response(),
    }
}

/// MUSE #100: encode + store a rendition of `master_bytes`, returning the served
/// response. `None` when the derivation failed, which the caller turns into
/// "serve the master" — never an error page.
async fn derive_rendition(
    state: &crate::http::AppState,
    kind: &str,
    id: i64,
    variant: &str,
    width: i32,
    master_bytes: &[u8],
    headers: &axum::http::HeaderMap,
) -> Option<Response> {
    // Bounded concurrency: queue rather than fan out N decodes. A closed
    // semaphore is unreachable (nothing closes it), but treat it as "no
    // rendition" rather than unwrapping.
    let _permit = RENDITION_SEMAPHORE.acquire().await.ok()?;

    let owned = master_bytes.to_vec();
    // CPU-bound: must not run on the async runtime.
    let rendered = tokio::task::spawn_blocking(move || render_jpeg(&owned, width))
        .await
        .map_err(|e| tracing::warn!(error = %e, "rendition task panicked"))
        .ok()?
        .map_err(|e| tracing::warn!(error = %e, kind, id, width, "rendition encode failed"))
        .ok()?;

    let etag = rendition_etag(&format!("{kind}:{id}:{variant}"), width, RENDITION_FORMAT);

    if let Err(e) = crate::repo::artwork_cache::store_rendition(
        &state.pool,
        kind,
        id,
        variant,
        width,
        RENDITION_FORMAT,
        RENDITION_CONTENT_TYPE,
        &rendered,
        Some(&etag),
    )
    .await
    {
        // Serving it uncached is strictly better than failing; the next request
        // simply re-derives.
        tracing::warn!(error = %e, kind, id, width, "failed to cache rendition; serving uncached");
    }

    if if_none_match_matches(headers, &etag) {
        return Some(not_modified_response(&etag));
    }
    Some(rendition_response(rendered, &etag, false))
}

/// The pre-existing master-resolution flow, unchanged in behaviour and lifted
/// into its own function so the rendition path can reuse it: cached bytes →
/// Plex fetch → provider fallback. `None` means "serve the placeholder".
async fn resolve_master(
    state: &crate::http::AppState,
    kind: &str,
    id: i64,
    variant: &str,
) -> Option<(Vec<u8>, String)> {

    match crate::repo::artwork_cache::get(&state.pool, kind, id, variant).await {
        Ok(Some(row)) => {
            if let (Some(bytes), Some(content_type)) =
                (row.bytes.clone(), row.content_type.clone())
            {
                return Some((bytes, content_type));
            }

            if let Some(source_url) = row.source_url.clone() {
                if let Some(plex) = state.plex.as_ref() {
                    match plex.fetch_image(&source_url).await {
                        Ok((bytes, content_type)) => {
                            if let Err(e) = crate::repo::artwork_cache::store_bytes(
                                &state.pool,
                                kind,
                                id,
                                variant,
                                Some(&source_url),
                                &content_type,
                                &bytes,
                                None,
                            )
                            .await
                            {
                                tracing::warn!(
                                    error = %e,
                                    kind = %kind,
                                    id,
                                    "failed to cache fetched artwork; serving it uncached this time"
                                );
                            }
                            return Some((bytes, content_type));
                        }
                        Err(e) => {
                            tracing::warn!(
                                error = %e,
                                kind = %kind,
                                id,
                                "artwork fetch from plex failed; serving placeholder"
                            );
                        }
                    }
                } else {
                    tracing::debug!(
                        kind = %kind,
                        id,
                        "artwork source registered but plex isn't configured; serving placeholder"
                    );
                }
            }
        }
        Ok(None) => {
            tracing::debug!(kind = %kind, id, "no artwork_cache entry; serving placeholder");
        }
        Err(e) => {
            tracing::warn!(error = %e, kind = %kind, id, "artwork_cache lookup failed; serving placeholder");
        }
    }

    // MWEBX-05 (S126): provider-artwork fallback. When nothing is cached and
    // there's no Plex path (e.g. a QNAP/MUSEL-scanned title, or a Discover
    // title with only TMDb/TVDB metadata), resolve the real poster/backdrop
    // URL from `media_metadata.images` and proxy it server-side (public
    // provider URL, no token), caching the bytes for next time. Any failure
    // degrades to the placeholder — never a 404/500.
    try_provider_artwork(state, kind, id, variant).await
}

/// Extract the first image URL of `cover_type` from a `media_metadata.images`
/// jsonb array (the Radarr/Sonarr shape `[{coverType, url, remoteUrl}]` this
/// crate writes — see `repo::media_metadata::add_image_entry_if_absent`).
/// Prefers `remoteUrl` (the absolute provider URL) over a possibly-relative
/// `url`.
fn image_url_for(images: &serde_json::Value, cover_type: &str) -> Option<String> {
    images.as_array()?.iter().find_map(|entry| {
        let ct = entry.get("coverType")?.as_str()?;
        if ct != cover_type {
            return None;
        }
        entry
            .get("remoteUrl")
            .and_then(|v| v.as_str())
            .or_else(|| entry.get("url").and_then(|v| v.as_str()))
            .map(|s| s.to_string())
    })
}

/// Resolve + proxy a real poster/backdrop from provider metadata, caching it
/// into `artwork_cache` on success. `None` when the entity can't be resolved,
/// carries no provider image URL, or the fetch fails — the caller then serves
/// the placeholder. Read-only and token-free: provider poster/backdrop URLs
/// (TMDb/TVDB) are public.
async fn try_provider_artwork(
    state: &crate::http::AppState,
    kind: &str,
    id: i64,
    variant: &str,
) -> Option<(Vec<u8>, String)> {
    // Resolve the entity to a `media_metadata` id (the images live there).
    let metadata_id = match kind {
        "media_metadata" => id,
        "media_item" => {
            crate::repo::media_item::get(&state.pool, id)
                .await
                .ok()?
                .media_metadata_id
        }
        _ => return None,
    };

    let metadata = crate::repo::media_metadata::get(&state.pool, metadata_id)
        .await
        .ok()?;

    let cover_type = match variant {
        "fanart" | "backdrop" => "fanart",
        _ => "poster",
    };
    let url = image_url_for(&metadata.images, cover_type)?;

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .ok()?;
    let resp = client.get(&url).send().await.ok()?;
    if !resp.status().is_success() {
        tracing::debug!(kind, id, %url, status = resp.status().as_u16(), "provider artwork fetch non-success; placeholder");
        return None;
    }
    let content_type = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("image/jpeg")
        .to_string();
    let bytes = resp.bytes().await.ok()?.to_vec();
    if bytes.is_empty() {
        return None;
    }

    // Best-effort cache under the ORIGINAL (kind,id,variant) key so a repeat
    // request is a cache hit, not another provider round-trip.
    if let Err(e) = crate::repo::artwork_cache::store_bytes(
        &state.pool,
        kind,
        id,
        variant,
        Some(&url),
        &content_type,
        &bytes,
        None,
    )
    .await
    {
        tracing::warn!(error = %e, kind, id, "failed to cache provider artwork; serving it uncached this time");
    }

    Some((bytes, content_type))
}

/// MUSE #100 response helpers. A rendition is content-addressed by
/// (master, width, format) and only changes when one of those changes, so it can
/// carry a long `max-age` plus a validator and answer `304` — the grid was
/// previously re-requesting every poster on every mount.
fn rendition_response(bytes: Vec<u8>, etag: &str, cache_hit: bool) -> Response {
    let mut resp = (StatusCode::OK, bytes).into_response();
    let headers = resp.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(RENDITION_CONTENT_TYPE),
    );
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("public, max-age=604800, immutable"),
    );
    if let Ok(v) = HeaderValue::from_str(etag) {
        headers.insert(header::ETAG, v);
    }
    headers.insert(
        header::HeaderName::from_static("x-muse-artwork-cache"),
        HeaderValue::from_static(if cache_hit { "rendition-hit" } else { "rendition-miss" }),
    );
    resp
}

fn not_modified_response(etag: &str) -> Response {
    let mut resp = StatusCode::NOT_MODIFIED.into_response();
    if let Ok(v) = HeaderValue::from_str(etag) {
        resp.headers_mut().insert(header::ETAG, v);
    }
    resp.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("public, max-age=604800, immutable"),
    );
    resp
}

/// An off-ladder `?w=` is a CLIENT error, answered as such rather than clamped.
/// See `artwork_render`'s module doc: clamping would report success for a size
/// the caller did not get, and a free-form width is an amplification vector.
fn bad_width_response() -> Response {
    let ladder = crate::web::artwork_render::RENDITION_WIDTHS
        .iter()
        .map(|w| w.to_string())
        .collect::<Vec<_>>()
        .join(", ");
    (
        StatusCode::BAD_REQUEST,
        format!("unsupported ?w= — supported rendition widths are: {ladder} (omit ?w= for the original)\n"),
    )
        .into_response()
}

/// `If-None-Match` handling, read from the REQUEST HEADERS (not the query
/// string). A conditional request whose validator list contains our ETag gets a
/// `304`; `*` matches any existing representation per RFC 9110.
fn if_none_match_matches(headers: &axum::http::HeaderMap, etag: &str) -> bool {
    headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|raw| {
            raw.split(',').any(|candidate| {
                let c = candidate.trim();
                // Compare weak validators by their opaque part so `W/"x"` from us
                // matches `W/"x"` echoed back, and tolerate a client that strips
                // the weak prefix.
                c == "*" || c == etag || c.trim_start_matches("W/") == etag.trim_start_matches("W/")
            })
        })
}

fn image_response(content_type: &str, bytes: Vec<u8>, cache_hit: bool) -> Response {
    let mut resp = (StatusCode::OK, bytes).into_response();
    let headers = resp.headers_mut();
    if let Ok(v) = HeaderValue::from_str(content_type) {
        headers.insert(header::CONTENT_TYPE, v);
    }
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("public, max-age=86400"),
    );
    headers.insert(
        header::HeaderName::from_static("x-muse-artwork-cache"),
        HeaderValue::from_static(if cache_hit { "hit" } else { "miss" }),
    );
    resp
}

fn placeholder_response() -> Response {
    let mut resp = (StatusCode::OK, PLACEHOLDER_PNG).into_response();
    let headers = resp.headers_mut();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static("image/png"));
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("no-cache"),
    );
    headers.insert(
        header::HeaderName::from_static("x-muse-artwork-cache"),
        HeaderValue::from_static("placeholder"),
    );
    resp
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn placeholder_bytes_are_a_valid_png_signature() {
        assert_eq!(&PLACEHOLDER_PNG[0..8], &[137, 80, 78, 71, 13, 10, 26, 10]);
    }

    /// Exercises the full miss → fetch → store → hit-from-cache flow against
    /// a real Postgres if `MUSE_TEST_DATABASE_URL` is set (skips cleanly
    /// otherwise), with the upstream "Plex" server mocked via httpmock. Also
    /// asserts the fleet's own `PLEX_TOKEN` value never appears anywhere in
    /// the proxy's response (headers or body) — it is used only in the
    /// server-side request to the mock, exactly as `PlexClient` sends it to
    /// a real Plex server.
    #[tokio::test]
    async fn artwork_proxy_caches_on_miss_and_serves_from_cache_on_hit() {
        use httpmock::prelude::*;
        use sqlx::postgres::PgPoolOptions;

        let Ok(database_url) = std::env::var("MUSE_TEST_DATABASE_URL") else {
            eprintln!(
                "skipping artwork_proxy_caches_on_miss_and_serves_from_cache_on_hit: MUSE_TEST_DATABASE_URL not set"
            );
            return;
        };

        let pool = PgPoolOptions::new()
            .max_connections(2)
            .connect(&database_url)
            .await
            .expect("connect to MUSE_TEST_DATABASE_URL");
        sqlx::migrate!("./migrations")
            .run(&pool)
            .await
            .expect("run migrations");

        const SECRET_TOKEN: &str = "super-secret-plex-token-should-never-leak";
        let image_bytes: Vec<u8> = vec![0xDE, 0xAD, 0xBE, 0xEF];

        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(GET)
                .path("/library/metadata/8888/thumb/1")
                .header("X-Plex-Token", SECRET_TOKEN);
            then.status(200)
                .header("content-type", "image/jpeg")
                .body(image_bytes.clone());
        });

        let plex = crate::plex::PlexClient::new(server.base_url(), SECRET_TOKEN)
            .expect("plex client should construct");

        let entity_kind = "media_item";
        let entity_id: i64 = 8_888_001; // distinct from any seeded fixture data
        crate::repo::artwork_cache::upsert_source(
            &pool,
            entity_kind,
            entity_id,
            "poster",
            "/library/metadata/8888/thumb/1",
        )
        .await
        .expect("register artwork source");

        let config = crate::config::Config::default();
        let state = Arc::new(crate::http::AppState {
            pool: pool.clone(),
            config: config.clone(),
            plex: Some(plex),
            prowlarr: None,
            arr_instances: Vec::new(),
            enrichment: crate::enrichment::EnrichmentService::from_config(&config),
            tmdb: None,
            embed: None,
            download: None,
        });

        // First request: cache miss, fetches from (mocked) Plex.
        let resp1 = art_handler(
            State(state.clone()),
            Path((entity_kind.to_string(), entity_id)),
            Query(HashMap::new()),
            axum::http::HeaderMap::new(),
        )
        .await
        .into_response();

        assert_eq!(resp1.status(), StatusCode::OK);
        assert_eq!(
            resp1.headers().get("x-muse-artwork-cache").unwrap(),
            "miss"
        );
        let body1 = axum::body::to_bytes(resp1.into_body(), usize::MAX)
            .await
            .expect("read body");
        assert_eq!(body1.as_ref(), image_bytes.as_slice());
        assert!(
            !String::from_utf8_lossy(&body1).contains(SECRET_TOKEN),
            "response body must never contain the Plex token"
        );
        mock.assert_hits(1);

        // Second request: served straight from artwork_cache — the mock is
        // NOT hit again.
        let resp2 = art_handler(
            State(state.clone()),
            Path((entity_kind.to_string(), entity_id)),
            Query(HashMap::new()),
            axum::http::HeaderMap::new(),
        )
        .await
        .into_response();

        assert_eq!(resp2.status(), StatusCode::OK);
        assert_eq!(resp2.headers().get("x-muse-artwork-cache").unwrap(), "hit");
        let body2 = axum::body::to_bytes(resp2.into_body(), usize::MAX)
            .await
            .expect("read body");
        assert_eq!(body2.as_ref(), image_bytes.as_slice());
        mock.assert_hits(1); // still just once — cache hit made no new request

        // Neither response's headers leak the token either.
        for resp in [
            art_handler(
                State(state.clone()),
                Path((entity_kind.to_string(), entity_id)),
                Query(HashMap::new()),
                axum::http::HeaderMap::new(),
            )
            .await
            .into_response(),
        ] {
            for value in resp.headers().values() {
                assert!(!value.to_str().unwrap_or("").contains(SECRET_TOKEN));
            }
        }

        sqlx::query("DELETE FROM artwork_cache WHERE entity_kind = $1 AND entity_id = $2")
            .bind(entity_kind)
            .bind(entity_id)
            .execute(&pool)
            .await
            .ok();
    }

    /// An artwork request for an entity with no registered source and no
    /// configured Plex client degrades to the placeholder rather than
    /// erroring.
    #[tokio::test]
    async fn artwork_proxy_serves_placeholder_when_unconfigured() {
        use sqlx::postgres::PgPoolOptions;

        let Ok(database_url) = std::env::var("MUSE_TEST_DATABASE_URL") else {
            eprintln!(
                "skipping artwork_proxy_serves_placeholder_when_unconfigured: MUSE_TEST_DATABASE_URL not set"
            );
            return;
        };

        let pool = PgPoolOptions::new()
            .max_connections(2)
            .connect(&database_url)
            .await
            .expect("connect to MUSE_TEST_DATABASE_URL");
        sqlx::migrate!("./migrations")
            .run(&pool)
            .await
            .expect("run migrations");

        let config = crate::config::Config::default();
        let state = Arc::new(crate::http::AppState {
            pool: pool.clone(),
            config: config.clone(),
            plex: None,
            prowlarr: None,
            arr_instances: Vec::new(),
            enrichment: crate::enrichment::EnrichmentService::from_config(&config),
            tmdb: None,
            embed: None,
            download: None,
        });

        let resp = art_handler(
            State(state),
            Path(("media_item".to_string(), 99_999_999_i64)),
            Query(HashMap::new()),
            axum::http::HeaderMap::new(),
        )
        .await
        .into_response();

        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers().get("x-muse-artwork-cache").unwrap(),
            "placeholder"
        );
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .expect("read body");
        assert_eq!(body.as_ref(), PLACEHOLDER_PNG);
    }
}
