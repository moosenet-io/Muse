//! MUSEM-05: the <media-service>-style request lifecycle HTTP surface —
//! `POST /requests`, `GET /requests`, `POST /requests/:id/approve`,
//! `POST /requests/:id/deny`. Mounted on the auth-gated `protected` router
//! (see `crate::http::router`'s doc comment for the full authed/open
//! breakdown) — these persist and can trigger a real download-client grab,
//! so they are sensitive by the same CAP-SEC-01/03 posture every other
//! mutating/account-scoped route on this crate already follows.
//!
//! ## The master acquisition gate
//! [`crate::settings::ExperienceSettings`] carries a dedicated
//! `acquisition.enabled` toggle (default `false`, see
//! [`crate::settings::AcquisitionSettings`]) — distinct from
//! [`crate::config::Config::arr_request_auto_tier_enabled`], which only
//! controls whether [`crate::arr::request::classify_tier`] is even ALLOWED
//! to return `AutoApprovable`. Both must be true for a `POST /requests`
//! call to ever reach a live grab: the master gate is checked first and,
//! when off, short-circuits straight to "persist as Requested," so an
//! operator can flip acquisition off crate-wide without touching the
//! tiered-safety config at all.

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::Json;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::acquisition::{fulfill_request, media_kind_str, AcquisitionDeps, FulfillOutcome};
use crate::arr::request::{classify_tier, RequestTier};
use crate::download::DownloadClient;
use crate::error::{MuseError, MuseResult};
use crate::http::AppState;
use crate::models::acquisition::{MediaRequest, NewMediaRequest, RequestStatus};
use crate::models::availability::Availability;
use crate::models::media_metadata::MediaKind;
use crate::repo;

/// `POST /requests` body.
#[derive(Debug, Clone, Deserialize)]
pub struct CreateRequestBody {
    /// Free-form provider-id bag (e.g. `{"tmdb": "603"}`) — stored as-is on
    /// `media_requests.provider_ids`. A `tmdb` string key, when present, is
    /// also used as `MediaRequestDraft::tmdb_id` for the tiered-safety
    /// classification below.
    #[serde(default)]
    pub provider_ids: serde_json::Value,
    pub kind: MediaKind,
    pub title: String,
    pub quality_profile_id: Option<i64>,
}

/// The response shape for every one of these endpoints — the persisted (or
/// just-updated) request row plus a human-readable summary of what
/// happened, never raw internal reasoning a caller shouldn't see.
#[derive(Debug, Clone, Serialize)]
pub struct RequestResponse {
    pub request: MediaRequest,
    pub tier: Option<String>,
    pub outcome: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ListRequestsQuery {
    pub status: Option<String>,
}

fn download_client_ref(state: &AppState) -> Option<&dyn DownloadClient> {
    state.download.as_ref().map(|c| c as &dyn DownloadClient)
}

fn outcome_label(outcome: &FulfillOutcome) -> String {
    match outcome {
        FulfillOutcome::Grabbed { queue_id, .. } => format!("grabbed (queue #{queue_id})"),
        FulfillOutcome::Rejected { reasons } => format!("rejected: {}", reasons.join("; ")),
        FulfillOutcome::Skipped { reason } => format!("skipped: {reason}"),
    }
}

/// `POST /requests`. Classifies the request via
/// [`crate::arr::request::classify_tier`] using a REAL availability signal
/// (an on-demand Prowlarr search, when Prowlarr is configured — see
/// `crate::acquisition`'s module doc for why this is honest rather than
/// fabricated), persists it, and — only when the tier comes back
/// `AutoApprovable` AND the master acquisition gate AND
/// `Config::arr_request_auto_tier_enabled` are both on — fulfills it
/// immediately. `Blocked` (no capability at all to fulfill this kind of
/// request) is rejected with `400` and never persisted at all.
pub async fn create_request_handler(
    State(state): State<Arc<AppState>>,
    Json(body): Json<CreateRequestBody>,
) -> MuseResult<Json<RequestResponse>> {
    if body.title.trim().is_empty() {
        return Err(MuseError::BadRequest("title must not be empty".to_string()));
    }

    let settings = repo::settings::load(&state.pool).await?;
    let acquisition_enabled = settings.is_acquisition_enabled();
    let auto_tier_enabled = acquisition_enabled && state.config.arr_request_auto_tier_enabled;

    // "Has capability at all": a configured Prowlarr client and a resolvable
    // quality profile for this kind — see the module doc on why this is the
    // honest re-reading of `has_matching_arr_instance` for a Prowlarr-native
    // (not *arr-fleet) grab path.
    let has_matching_capability = state.prowlarr.is_some() && body.quality_profile_id.is_some();
    if !has_matching_capability {
        return Err(MuseError::BadRequest(
            "no capability configured to fulfill this request (Prowlarr not configured, or no \
             quality_profile_id supplied) — request was not persisted"
                .to_string(),
        ));
    }

    // A real, on-demand availability signal (never fabricated) so
    // `classify_tier` can legitimately return `AutoApprovable` — see the
    // `crate::acquisition` module doc.
    let availability = if auto_tier_enabled {
        if let Some(prowlarr) = state.prowlarr.as_ref() {
            let candidates = crate::acquisition::search_candidates(
                prowlarr,
                &state.config,
                &body.title,
                media_kind_str(body.kind),
            )
            .await?;
            Some(Availability {
                media_metadata_id: 0,
                best_quality: None,
                best_seeders: candidates.iter().filter_map(|c| c.seeders).max(),
                release_count: candidates.len() as i32,
                has_freeleech: candidates.iter().any(|c| c.freeleech),
                cheapest_size_bytes: candidates.iter().filter_map(|c| c.size).min(),
                newest_release_at: candidates.iter().filter_map(|c| c.publish_date).max(),
                computed_at: chrono::Utc::now(),
            })
        } else {
            None
        }
    } else {
        None
    };

    let tier = classify_tier(auto_tier_enabled, availability.as_ref(), true);

    let new = NewMediaRequest {
        provider_ids: body.provider_ids.clone(),
        media_kind: media_kind_str(body.kind).to_string(),
        title: body.title.clone(),
        requested_by: None,
        tier: Some(format!("{tier:?}")),
        quality_profile_id: body.quality_profile_id,
        note: None,
    };
    let request = repo::acquisition::create_request(&state.pool, &new).await?;

    let outcome = if tier == RequestTier::AutoApprovable {
        let deps = AcquisitionDeps {
            pool: &state.pool,
            config: &state.config,
            prowlarr: state.prowlarr.as_ref(),
            download: download_client_ref(&state),
        };
        Some(fulfill_request(&deps, &request).await?)
    } else {
        None
    };

    let final_request = if outcome.is_some() {
        repo::acquisition::get_request(&state.pool, request.id).await?
    } else {
        request
    };

    Ok(Json(RequestResponse {
        request: final_request,
        tier: Some(format!("{tier:?}")),
        outcome: outcome.as_ref().map(outcome_label),
    }))
}

/// `GET /requests`. Auth-gated (see this module's doc) — never reachable
/// unauthenticated, so an anonymous caller cannot enumerate request/account
/// data (the CAP-SEC-03 lesson this crate already applies to `/recommend*`).
/// `?status=<status>` filters to one lifecycle status; omitted lists across
/// every known status.
pub async fn list_requests_handler(
    State(state): State<Arc<AppState>>,
    Query(query): Query<ListRequestsQuery>,
) -> MuseResult<Json<Vec<MediaRequest>>> {
    let statuses: Vec<RequestStatus> = match &query.status {
        Some(raw) => {
            let parsed: RequestStatus = raw
                .parse()
                .map_err(|e| MuseError::BadRequest(format!("invalid status {raw:?}: {e}")))?;
            vec![parsed]
        }
        None => vec![
            RequestStatus::Requested,
            RequestStatus::Approved,
            RequestStatus::Denied,
            RequestStatus::Searching,
            RequestStatus::Grabbed,
            RequestStatus::Available,
            RequestStatus::Failed,
        ],
    };

    let mut all = Vec::new();
    for status in statuses {
        all.extend(repo::acquisition::list_requests_by_status(&state.pool, status.as_str()).await?);
    }
    all.sort_by(|a, b| a.created_at.cmp(&b.created_at));
    Ok(Json(all))
}

/// `POST /requests/:id/approve` — fulfills a `Requested` request now.
/// Idempotent: approving a request that has already moved past `Requested`
/// (already `Grabbed`/`Failed`/`Denied`/etc.) is a no-op that returns the
/// current row rather than a second grab.
pub async fn approve_request_handler(
    State(state): State<Arc<AppState>>,
    Path(id): Path<i64>,
) -> MuseResult<Json<RequestResponse>> {
    let request = repo::acquisition::get_request(&state.pool, id).await?;

    if request.status != RequestStatus::Requested.as_str() {
        return Ok(Json(RequestResponse {
            request,
            tier: None,
            outcome: Some("no-op: request is not in Requested state".to_string()),
        }));
    }

    let settings = repo::settings::load(&state.pool).await?;
    if !settings.is_acquisition_enabled() {
        return Ok(Json(RequestResponse {
            request,
            tier: None,
            outcome: Some("skipped: acquisition is disabled (master gate off)".to_string()),
        }));
    }

    let approved = repo::acquisition::update_request_status(
        &state.pool,
        id,
        RequestStatus::Approved.as_str(),
    )
    .await?;

    let deps = AcquisitionDeps {
        pool: &state.pool,
        config: &state.config,
        prowlarr: state.prowlarr.as_ref(),
        download: download_client_ref(&state),
    };
    let outcome = fulfill_request(&deps, &approved).await?;
    let final_request = repo::acquisition::get_request(&state.pool, id).await?;

    Ok(Json(RequestResponse {
        request: final_request,
        tier: None,
        outcome: Some(outcome_label(&outcome)),
    }))
}

/// `POST /requests/:id/deny` — idempotent: denying an already-terminal
/// request (already `Denied`/`Grabbed`/etc.) leaves it as-is rather than
/// overwriting a real outcome with `Denied`.
pub async fn deny_request_handler(
    State(state): State<Arc<AppState>>,
    Path(id): Path<i64>,
) -> MuseResult<Json<RequestResponse>> {
    let request = repo::acquisition::get_request(&state.pool, id).await?;

    if request.status != RequestStatus::Requested.as_str() {
        return Ok(Json(RequestResponse {
            request,
            tier: None,
            outcome: Some("no-op: request is not in Requested state".to_string()),
        }));
    }

    let denied = repo::acquisition::update_request_status(&state.pool, id, RequestStatus::Denied.as_str())
        .await?;
    repo::acquisition::record_history_event(
        &state.pool,
        &crate::models::acquisition::NewHistoryEvent {
            event_type: "denied".to_string(),
            media_metadata_id: None,
            monitored_item_id: None,
            download_id: None,
            source_title: Some(denied.title.clone()),
            quality: None,
            data: json!({"request_id": denied.id}),
            languages: json!([]),
        },
    )
    .await?;

    Ok(Json(RequestResponse {
        request: denied,
        tier: None,
        outcome: Some("denied".to_string()),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outcome_label_summarizes_each_variant() {
        assert!(outcome_label(&FulfillOutcome::Grabbed { queue_id: 5, hash: None }).contains('5'));
        assert!(outcome_label(&FulfillOutcome::Rejected {
            reasons: vec!["no candidates".to_string()]
        })
        .contains("no candidates"));
        assert!(outcome_label(&FulfillOutcome::Skipped {
            reason: "no profile".to_string()
        })
        .contains("no profile"));
    }
}

/// DB-backed handler-level coverage — `MUSE_TEST_DATABASE_URL`-gated, same
/// convention as `crate::acquisition::db_gated`. Calls handlers directly
/// (State + extractors), matching `http::ops`'s/`channels::routes`'s own
/// existing unit-test style (`crate::endpoint_tests` is the separate
/// router-level complement, and owns the tokenless-401 gate tests for these
/// routes).
#[cfg(test)]
mod db_gated {
    use super::*;
    use crate::config::Config;
    use crate::download::config::{QbitConfig, QbitPassword};
    use crate::download::qbit::QbitClient;
    use crate::enrichment::EnrichmentService;
    use crate::models::acquisition::QueueStatus;
    use crate::models::quality::{NewQualityDefinition, NewQualityProfile};
    use crate::settings::{AcquisitionSettings, ExperienceSettings};
    use httpmock::prelude::*;

    async fn test_pool_or_skip(test_name: &str) -> Option<sqlx::PgPool> {
        let Ok(database_url) = std::env::var("MUSE_TEST_DATABASE_URL") else {
            eprintln!(
                "MUSE_TEST_DATABASE_URL not set — skipping {test_name} \
                 (expected in the default test run; this harness does not \
                 require a live DB)"
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

    fn unique_suffix() -> u128 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock")
            .as_nanos()
    }

    async fn seed_profile_allowing_web_1080p(pool: &sqlx::PgPool) -> i64 {
        let def = crate::repo::quality::create_definition(
            pool,
            &NewQualityDefinition {
                quality_key: format!("web-1080p-{}", unique_suffix()),
                title: "WEB 1080p".to_string(),
                source: "WEB-DL".to_string(),
                resolution: Some("1080p".to_string()),
                modifier: "none".to_string(),
                sort_order: 1,
            },
        )
        .await
        .expect("seed quality_definition");

        crate::repo::quality::create_profile(
            pool,
            &NewQualityProfile {
                name: format!("musem05-http-profile-{}", unique_suffix()),
                cutoff_quality_id: None,
                items: serde_json::json!([{ "quality": { "id": def.id }, "allowed": true }]),
                upgrade_allowed: true,
                natural_language_intent: None,
            },
        )
        .await
        .expect("seed quality_profile")
        .id
    }

    fn qbit_login_and_add_mock(server: &MockServer) {
        server.mock(|when, then| {
            when.method(POST).path("/api/v2/auth/login");
            then.status(200)
                .header("set-cookie", "SID=testsid; path=/; HttpOnly")
                .body("Ok.");
        });
        server.mock(|when, then| {
            when.method(POST).path("/api/v2/torrents/add");
            then.status(200).body("Ok.");
        });
    }

    async fn state_for(
        pool: sqlx::PgPool,
        config: Config,
        prowlarr: Option<crate::prowlarr::ProwlarrClient>,
        download: Option<QbitClient>,
    ) -> Arc<AppState> {
        Arc::new(AppState {
            enrichment: EnrichmentService::from_config(&config),
            pool,
            config,
            plex: None,
            prowlarr,
            arr_instances: Vec::new(),
            tmdb: None,
            embed: None,
            download,
        })
    }

    async fn save_settings(pool: &sqlx::PgPool, acquisition_enabled: bool) {
        let mut settings = ExperienceSettings::default();
        settings.acquisition = AcquisitionSettings {
            enabled: acquisition_enabled,
        };
        crate::repo::settings::save(pool, &settings)
            .await
            .expect("save settings");
    }

    fn search_response_body(guid: &str, title: &str) -> String {
        format!(
            r#"[{{"guid": "{guid}", "title": "{title}", "indexerId": 1,
                  "indexer": "TestIndexer", "protocol": "torrent",
                  "downloadUrl": "http://example.invalid/dl/{guid}",
                  "categories": [{{"id": 2000, "name": "Movies"}}]}}]"#
        )
    }

    /// Master gate OFF: even with `arr_request_auto_tier_enabled` true and a
    /// genuinely grabbable candidate available, `POST /requests` must never
    /// reach a grab — the request is persisted (Requested) and left there.
    #[tokio::test]
    async fn master_gate_off_never_grabs_even_for_an_otherwise_grabbable_title() {
        let Some(pool) =
            test_pool_or_skip("master_gate_off_never_grabs_even_for_an_otherwise_grabbable_title").await
        else {
            return;
        };
        save_settings(&pool, false).await;
        let profile_id = seed_profile_allowing_web_1080p(&pool).await;
        let title = format!("Musem05 Http Gate Off Movie {}", unique_suffix());

        let prowlarr_server = MockServer::start();
        prowlarr_server.mock(|when, then| {
            when.method(GET).path("/api/v1/search");
            then.status(200)
                .header("content-type", "application/json")
                .body(search_response_body("gate-off-guid", &format!("{title}.2020.1080p.WEB-DL")));
        });
        let prowlarr = crate::prowlarr::ProwlarrClient::new(prowlarr_server.base_url(), "test-key")
            .expect("prowlarr client");

        let qbit_server = MockServer::start();
        qbit_login_and_add_mock(&qbit_server);
        let download = QbitClient::from_config(&QbitConfig {
            url: qbit_server.base_url(),
            user: "admin".to_string(),
            pass: QbitPassword::from("hunter2".to_string()),
        })
        .expect("qbit client");

        let config = Config {
            arr_request_auto_tier_enabled: true,
            ..Config::default()
        };
        let state = state_for(pool.clone(), config, Some(prowlarr), Some(download)).await;

        let body = CreateRequestBody {
            provider_ids: json!({"tmdb": "603"}),
            kind: MediaKind::Movie,
            title: title.clone(),
            quality_profile_id: Some(profile_id),
        };
        let response = create_request_handler(State(state), Json(body))
            .await
            .expect("create_request_handler should succeed");

        assert_eq!(response.request.status, RequestStatus::Requested.as_str());
        assert_eq!(response.tier.as_deref(), Some("NeedsReview"));

        let queue = crate::repo::acquisition::list_download_queue_by_status(
            &pool,
            QueueStatus::Queued.as_str(),
        )
        .await
        .expect("list queue");
        assert!(
            !queue.iter().any(|q| q.request_id == Some(response.request.id)),
            "the master gate being off must prevent any grab"
        );
    }

    /// Full happy path: master gate ON, auto-tier ON, Prowlarr finds a real
    /// candidate -> `POST /requests` classifies `AutoApprovable` and grabs
    /// immediately in the same call.
    #[tokio::test]
    async fn create_request_autoapprovable_grabs_immediately() {
        let Some(pool) = test_pool_or_skip("create_request_autoapprovable_grabs_immediately").await
        else {
            return;
        };
        save_settings(&pool, true).await;
        let profile_id = seed_profile_allowing_web_1080p(&pool).await;
        let title = format!("Musem05 Http Autoapprove Movie {}", unique_suffix());

        let prowlarr_server = MockServer::start();
        prowlarr_server.mock(|when, then| {
            when.method(GET).path("/api/v1/search");
            then.status(200)
                .header("content-type", "application/json")
                .body(search_response_body(
                    "autoapprove-guid",
                    &format!("{title}.2020.1080p.WEB-DL"),
                ));
        });
        let prowlarr = crate::prowlarr::ProwlarrClient::new(prowlarr_server.base_url(), "test-key")
            .expect("prowlarr client");

        let qbit_server = MockServer::start();
        qbit_login_and_add_mock(&qbit_server);
        let download = QbitClient::from_config(&QbitConfig {
            url: qbit_server.base_url(),
            user: "admin".to_string(),
            pass: QbitPassword::from("hunter2".to_string()),
        })
        .expect("qbit client");

        let config = Config {
            arr_request_auto_tier_enabled: true,
            ..Config::default()
        };
        let state = state_for(pool.clone(), config, Some(prowlarr), Some(download)).await;

        let body = CreateRequestBody {
            provider_ids: json!({"tmdb": "603"}),
            kind: MediaKind::Movie,
            title: title.clone(),
            quality_profile_id: Some(profile_id),
        };
        let response = create_request_handler(State(state), Json(body))
            .await
            .expect("create_request_handler should succeed");

        assert_eq!(response.tier.as_deref(), Some("AutoApprovable"));
        assert_eq!(response.request.status, RequestStatus::Grabbed.as_str());
    }

    /// Approve is idempotent: approving an already-`Grabbed` request never
    /// grabs a second time.
    #[tokio::test]
    async fn approve_on_an_already_grabbed_request_is_a_no_op() {
        let Some(pool) = test_pool_or_skip("approve_on_an_already_grabbed_request_is_a_no_op").await
        else {
            return;
        };
        let request = crate::repo::acquisition::create_request(
            &pool,
            &crate::models::acquisition::NewMediaRequest {
                provider_ids: json!({}),
                media_kind: "movie".to_string(),
                title: format!("Musem05 Already Grabbed {}", unique_suffix()),
                requested_by: None,
                tier: None,
                quality_profile_id: None,
                note: None,
            },
        )
        .await
        .expect("seed request");
        crate::repo::acquisition::update_request_status(&pool, request.id, RequestStatus::Grabbed.as_str())
            .await
            .expect("mark grabbed");

        let config = Config::default();
        let state = state_for(pool.clone(), config, None, None).await;

        let response = approve_request_handler(State(state), Path(request.id))
            .await
            .expect("approve_request_handler should succeed");

        assert_eq!(response.request.status, RequestStatus::Grabbed.as_str());
        assert_eq!(response.outcome.as_deref(), Some("no-op: request is not in Requested state"));
    }

    /// Deny persists `Denied` and writes a `history_events` row.
    #[tokio::test]
    async fn deny_marks_denied_and_records_history() {
        let Some(pool) = test_pool_or_skip("deny_marks_denied_and_records_history").await else {
            return;
        };
        let request = crate::repo::acquisition::create_request(
            &pool,
            &crate::models::acquisition::NewMediaRequest {
                provider_ids: json!({}),
                media_kind: "movie".to_string(),
                title: format!("Musem05 Deny Me {}", unique_suffix()),
                requested_by: None,
                tier: None,
                quality_profile_id: None,
                note: None,
            },
        )
        .await
        .expect("seed request");

        let config = Config::default();
        let state = state_for(pool.clone(), config, None, None).await;

        let response = deny_request_handler(State(state), Path(request.id))
            .await
            .expect("deny_request_handler should succeed");

        assert_eq!(response.request.status, RequestStatus::Denied.as_str());

        let count: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM history_events WHERE event_type = 'denied' AND data->>'request_id' = $1",
        )
        .bind(request.id.to_string())
        .fetch_one(&pool)
        .await
        .expect("count denied history rows");
        assert_eq!(count, 1);
    }

    /// `GET /requests` lists across every status by default and filters by
    /// `?status=` when supplied.
    #[tokio::test]
    async fn list_requests_filters_by_status_query_param() {
        let Some(pool) = test_pool_or_skip("list_requests_filters_by_status_query_param").await
        else {
            return;
        };
        let title = format!("Musem05 List Me {}", unique_suffix());
        crate::repo::acquisition::create_request(
            &pool,
            &crate::models::acquisition::NewMediaRequest {
                provider_ids: json!({}),
                media_kind: "movie".to_string(),
                title: title.clone(),
                requested_by: None,
                tier: None,
                quality_profile_id: None,
                note: None,
            },
        )
        .await
        .expect("seed request");

        let config = Config::default();
        let state = state_for(pool.clone(), config, None, None).await;

        let all = list_requests_handler(
            State(state.clone()),
            Query(ListRequestsQuery { status: None }),
        )
        .await
        .expect("list all");
        assert!(all.iter().any(|r| r.title == title));

        let requested_only = list_requests_handler(
            State(state.clone()),
            Query(ListRequestsQuery {
                status: Some("requested".to_string()),
            }),
        )
        .await
        .expect("list requested");
        assert!(requested_only.iter().any(|r| r.title == title));

        let grabbed_only = list_requests_handler(
            State(state),
            Query(ListRequestsQuery {
                status: Some("grabbed".to_string()),
            }),
        )
        .await
        .expect("list grabbed");
        assert!(!grabbed_only.iter().any(|r| r.title == title));
    }
}
