//! MUSE-31: `POST /channels/{id}/compose` — the on-demand director route
//! the spec calls for. `channels::compose::compose_channel_run` (MUSE-24)
//! existed with no HTTP surface at all; this is that surface.
//!
//! Deliberately thin: request-shape validation lives here (so a bad
//! request degrades to `400`, never the `500` a raw `MuseError::Config`
//! from deeper in `compose_channel_run` would map to — see
//! `crate::error::MuseError`'s `IntoResponse`), everything else delegates
//! straight to [`super::compose::compose_channel_run`].

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::Json;
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::json;

use crate::error::{MuseError, MuseResult};
use crate::http::AppState;
use crate::models::interstitial::InterstitialKind;

use super::compose::{compose_channel_run, ComposeOptions, EpisodeOrdering};

/// Request body for `POST /channels/{id}/compose`. Every field beyond
/// `show_media_item_ids` is optional and falls back to
/// [`ComposeOptions::default`] (a 2-hour session, one interstitial per
/// item, no LLM enhancement, `start_at = now`).
#[derive(Debug, Deserialize)]
pub struct ComposeChannelRequest {
    pub account_id: Option<i64>,
    pub show_media_item_ids: Vec<i64>,
    pub ordering: Option<EpisodeOrdering>,
    pub target_session_ms: Option<i64>,
    pub interstitial_every_n_items: Option<u32>,
    pub interstitial_kind: Option<InterstitialKind>,
    pub interstitial_decade: Option<i32>,
    pub interstitial_theme: Option<String>,
    pub start_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub use_llm: bool,
    pub llm_model: Option<String>,
}

/// `POST /channels/{id}/compose` — compose (and persist) a fresh on-demand
/// lineup for the given channel, right now. Returns the created
/// `channel_runs` row plus its program count. `400` on a structurally
/// invalid request (no shows, non-positive session length); `404` if the
/// channel doesn't exist; never `500` for a caller-input problem.
pub async fn compose_handler(
    State(state): State<Arc<AppState>>,
    Path(channel_id): Path<i64>,
    Json(req): Json<ComposeChannelRequest>,
) -> MuseResult<Json<serde_json::Value>> {
    if req.show_media_item_ids.is_empty() {
        return Err(MuseError::BadRequest(
            "show_media_item_ids must contain at least one show".to_string(),
        ));
    }
    if let Some(ms) = req.target_session_ms {
        if ms <= 0 {
            return Err(MuseError::BadRequest(
                "target_session_ms must be positive".to_string(),
            ));
        }
    }

    let mut opts = ComposeOptions {
        account_id: req.account_id,
        show_media_item_ids: req.show_media_item_ids,
        ordering: req.ordering.unwrap_or(EpisodeOrdering::NextUnwatched),
        use_llm: req.use_llm,
        llm_model: req.llm_model,
        ..Default::default()
    };
    if let Some(ms) = req.target_session_ms {
        opts.target_session_ms = ms;
    }
    if let Some(n) = req.interstitial_every_n_items {
        opts.interstitial_every_n_items = n;
    }
    opts.interstitial_kind = req.interstitial_kind;
    opts.interstitial_decade = req.interstitial_decade;
    opts.interstitial_theme = req.interstitial_theme;
    if let Some(start_at) = req.start_at {
        opts.start_at = start_at;
    }

    let run = compose_channel_run(&state.pool, state.config.chord_url.as_deref(), channel_id, &opts).await?;

    let program_count = run
        .schedule
        .get("items")
        .and_then(|v| v.as_array())
        .map(|items| items.len())
        .unwrap_or(0);

    Ok(Json(json!({
        "run": run,
        "program_count": program_count,
    })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compose_channel_request_deserializes_minimal_body() {
        let req: ComposeChannelRequest = serde_json::from_str(r#"{"show_media_item_ids": [1, 2]}"#)
            .expect("minimal body should deserialize");
        assert_eq!(req.show_media_item_ids, vec![1, 2]);
        assert!(req.account_id.is_none());
        assert!(!req.use_llm);
    }

    /// The empty-`show_media_item_ids` degrade path never touches the DB
    /// (validated before any repo call), so this is a fast unit test rather
    /// than a live-DB one -- proving the route returns `400`, never the
    /// `500` `compose_channel_run`'s own `MuseError::Config` would map to.
    #[tokio::test]
    async fn compose_handler_rejects_empty_show_list_as_bad_request() {
        let pool = sqlx::postgres::PgPoolOptions::new()
            .connect_lazy("postgres://user:pass@127.0.0.1:1/muse_test_lazy")
            .expect("connect_lazy should never fail synchronously");
        let config = crate::config::Config::default();
        let state = Arc::new(AppState {
            pool,
            enrichment: crate::enrichment::EnrichmentService::from_config(&config),
            config,
            plex: None,
            prowlarr: None,
            arr_instances: Vec::new(),
            tmdb: None,
            embed: None,
        });

        let req = ComposeChannelRequest {
            account_id: None,
            show_media_item_ids: Vec::new(),
            ordering: None,
            target_session_ms: None,
            interstitial_every_n_items: None,
            interstitial_kind: None,
            interstitial_decade: None,
            interstitial_theme: None,
            start_at: None,
            use_llm: false,
            llm_model: None,
        };

        let result = compose_handler(State(state), Path(1), Json(req)).await;
        assert!(
            matches!(result, Err(MuseError::BadRequest(_))),
            "an empty show list must degrade to 400, never 500"
        );
    }

    /// Live-DB happy path: seeds a channel + one show with one episode,
    /// composes via the route handler, and asserts the response carries a
    /// real run + a non-zero program count.
    #[tokio::test]
    async fn compose_handler_composes_and_returns_run_and_program_count() {
        let Ok(database_url) = std::env::var("MUSE_TEST_DATABASE_URL") else {
            eprintln!(
                "MUSE_TEST_DATABASE_URL not set — skipping \
                 compose_handler_composes_and_returns_run_and_program_count"
            );
            return;
        };

        use crate::models::channel::{ChannelKind, ChannelMode, NewChannel};
        use crate::models::episode::NewEpisode;
        use crate::models::library::{LibraryKind, NewLibrary};
        use crate::models::media_item::NewMediaItem;
        use crate::models::media_metadata::{MediaKind, NewMediaMetadata};
        use crate::models::season::NewSeason;
        use uuid::Uuid;

        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
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
            &NewLibrary {
                name: format!("muse31-compose-route-tv-{suffix}"),
                kind: LibraryKind::Tv,
                root_folder: "/test/muse31-compose-route".to_string(),
                source_arr_name: None,
                source_arr_url: None,
            },
        )
        .await
        .expect("create library");

        let metadata = crate::repo::media_metadata::upsert_by_tvdb(
            &pool,
            &NewMediaMetadata {
                kind: MediaKind::Show,
                tmdb_id: None,
                tvdb_id: Some(format!("muse31-compose-route-tvdb-{suffix}")),
                imdb_id: None,
                provider_ids: json!({}),
                title: format!("MUSE-31 Compose Route Show {suffix}"),
                sort_title: None,
                original_title: None,
                original_language: None,
                status: None,
                overview: None,
                studio: None,
                network: None,
                runtime_minutes: Some(20),
                year: Some(2022),
                images: json!([]),
            },
        )
        .await
        .expect("upsert media_metadata");

        let show = crate::repo::media_item::upsert(
            &pool,
            &NewMediaItem {
                library_id: library.id,
                media_metadata_id: metadata.id,
                path: format!("/test/muse31-compose-route/show-{suffix}"),
                monitored: true,
                quality_profile_id: None,
                minimum_availability: None,
                plex_rating_key: Some(format!("muse31-compose-route-show-{suffix}")),
                added_at: None,
            },
        )
        .await
        .expect("upsert media_item");

        let season = crate::repo::season::upsert(
            &pool,
            &NewSeason {
                media_item_id: show.id,
                season_number: 1,
                title: None,
                overview: None,
                monitored: true,
                air_date: None,
            },
        )
        .await
        .expect("create season");

        crate::repo::episode::upsert(
            &pool,
            &NewEpisode {
                season_id: season.id,
                media_item_id: show.id,
                episode_number: 1,
                absolute_episode_number: None,
                title: Some("Pilot".to_string()),
                overview: None,
                air_date: None,
                air_date_utc: None,
                runtime_minutes: Some(20),
                monitored: true,
                tvdb_id: None,
            },
        )
        .await
        .expect("create episode");

        let channel = crate::repo::channel::create_channel(
            &pool,
            &NewChannel {
                account_id: None,
                name: format!("MUSE-31 compose route channel {suffix}"),
                kind: ChannelKind::Personal,
                mode: ChannelMode::OnDemand,
                channel_number: None,
                target_client_id: None,
                directive: None,
                rules: json!({}),
                is_preset: false,
            },
        )
        .await
        .expect("create channel");

        let config = crate::config::Config::default();
        let state = Arc::new(AppState {
            pool: pool.clone(),
            enrichment: crate::enrichment::EnrichmentService::from_config(&config),
            config,
            plex: None,
            prowlarr: None,
            arr_instances: Vec::new(),
            tmdb: None,
            embed: None,
        });

        let req = ComposeChannelRequest {
            account_id: None,
            show_media_item_ids: vec![show.id],
            ordering: None,
            target_session_ms: Some(60 * 60_000),
            interstitial_every_n_items: None,
            interstitial_kind: None,
            interstitial_decade: None,
            interstitial_theme: None,
            start_at: None,
            use_llm: false,
            llm_model: None,
        };

        let Json(body) = compose_handler(State(state), Path(channel.id), Json(req))
            .await
            .expect("compose_handler should succeed for a valid request");

        assert!(body["run"]["id"].is_number());
        assert_eq!(body["run"]["channel_id"], json!(channel.id));
        assert!(
            body["program_count"].as_u64().unwrap_or(0) >= 1,
            "expected at least one scheduled program: {body:?}"
        );

        sqlx::query("DELETE FROM channel_runs WHERE channel_id = $1")
            .bind(channel.id)
            .execute(&pool)
            .await
            .ok();
        sqlx::query("DELETE FROM channels WHERE id = $1")
            .bind(channel.id)
            .execute(&pool)
            .await
            .ok();
        sqlx::query("DELETE FROM media_items WHERE id = $1")
            .bind(show.id)
            .execute(&pool)
            .await
            .ok();
        sqlx::query("DELETE FROM media_metadata WHERE id = $1")
            .bind(metadata.id)
            .execute(&pool)
            .await
            .ok();
        sqlx::query("DELETE FROM libraries WHERE id = $1")
            .bind(library.id)
            .execute(&pool)
            .await
            .ok();
    }

    #[test]
    fn compose_channel_request_deserializes_full_body() {
        let body = r#"{
            "account_id": 7,
            "show_media_item_ids": [1, 2],
            "ordering": "taste_ranked",
            "target_session_ms": 3600000,
            "interstitial_every_n_items": 2,
            "interstitial_kind": "bumper",
            "interstitial_decade": 1990,
            "interstitial_theme": "sci-fi",
            "use_llm": true,
            "llm_model": "default"
        }"#;
        let req: ComposeChannelRequest = serde_json::from_str(body).expect("full body should deserialize");
        assert_eq!(req.account_id, Some(7));
        assert_eq!(req.ordering, Some(EpisodeOrdering::TasteRanked));
        assert_eq!(req.interstitial_kind, Some(InterstitialKind::Bumper));
        assert!(req.use_llm);
    }
}
