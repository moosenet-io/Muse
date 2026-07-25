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

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

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

pub async fn art_handler(
    State(state): State<Arc<crate::http::AppState>>,
    Path((kind, id)): Path<(String, i64)>,
    Query(params): Query<HashMap<String, String>>,
) -> impl IntoResponse {
    let variant = params
        .get("variant")
        .map(String::as_str)
        .unwrap_or(DEFAULT_VARIANT);

    match crate::repo::artwork_cache::get(&state.pool, &kind, id, variant).await {
        Ok(Some(row)) => {
            if let (Some(bytes), Some(content_type)) =
                (row.bytes.clone(), row.content_type.clone())
            {
                return image_response(&content_type, bytes, true);
            }

            if let Some(source_url) = row.source_url.clone() {
                if let Some(plex) = state.plex.as_ref() {
                    match plex.fetch_image(&source_url).await {
                        Ok((bytes, content_type)) => {
                            if let Err(e) = crate::repo::artwork_cache::store_bytes(
                                &state.pool,
                                &kind,
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
                            return image_response(&content_type, bytes, false);
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
    if let Some(resp) = try_provider_artwork(&state, &kind, id, variant).await {
        return resp;
    }

    placeholder_response()
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
) -> Option<Response> {
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

    Some(image_response(&content_type, bytes, false))
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
