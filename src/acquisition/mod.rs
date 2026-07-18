//! MUSEM-05 (Plane MUSE S119 Sprint 1): the acquisition orchestrator —
//! `fulfill_request` wires together every module MUSEM-01..04 shipped
//! (search → decide → grab → persist) behind a single entry point, and
//! [`AcquisitionSink`] is the first REAL (non-`Noop`) implementation of
//! [`crate::arr::request::MediaRequestSink`] this crate ships.
//!
//! ## Why this reuses `classify_tier`/`submit_if_appropriate` honestly
//! [`crate::arr::request`]'s own module doc is explicit that a missing
//! title outside the library "currently classifies as `NeedsReview` or
//! `Blocked` — never `AutoApprovable` in practice" because there was no
//! real-time availability signal to check against (MUSE-16 availability is
//! keyed to an existing `media_metadata_id`, which a not-yet-owned title
//! doesn't have) — and flags "wiring a real-time Prowlarr/availability
//! check for a not-yet-owned TMDb hit" as the natural, separately-
//! reviewable follow-up. This module IS that follow-up: [`fulfill_request`]
//! (and the `POST /requests` handler that calls it, see
//! `crate::http::requests`) runs a genuine on-demand Prowlarr search
//! (MUSEM-03) BEFORE classifying, and turns the real result count into a
//! [`crate::models::availability::Availability`] value — never a fabricated
//! one — so `classify_tier` can legitimately return `AutoApprovable` for a
//! title that is, right now, actually grabbable.
//!
//! ## What "has a matching *arr instance" means here
//! `classify_tier`'s `has_matching_arr_instance` parameter was written for
//! a *arr fleet (a configured Radarr for `Movie`, a configured Sonarr for
//! `Show`). This sprint's grab path is deliberately Prowlarr+qBittorrent-
//! native, not *arr (`crate::arr` stays read-only — see its own module
//! doc). So here that parameter is answered honestly for THIS substrate:
//! "can this request even structurally be fulfilled" — a configured
//! Prowlarr client, a configured download client, and a resolvable quality
//! profile for the requested kind. `false` maps to the same
//! [`crate::arr::request::RequestTier::Blocked`] outcome the original
//! *arr-fleet meaning would have produced for "no capability at all,"
//! which is the property `classify_tier`'s callers actually depend on.

use std::sync::Arc;

use serde_json::json;
use sqlx::PgPool;

use crate::arr::request::{MediaRequestDraft, MediaRequestSink};
use crate::config::Config;
use crate::decision::scoring::{ReleaseCandidate, ScoringPolicy};
use crate::decision::{decide_release, Decision, ReleaseChoice};
use crate::download::{DownloadClient, GrabRequest};
use crate::error::{MuseError, MuseResult};
use crate::models::acquisition::{
    DownloadQueueEntry, DownloadSource, HistoryEventType, MediaRequest, NewDownloadQueueEntry,
    NewHistoryEvent, NewMediaRequest, RequestStatus,
};
use crate::models::media_metadata::MediaKind;
use crate::models::release::Release;
use crate::prowlarr::{search_releases, ProwlarrClient, SearchRelease};
use crate::repo;

/// `media_requests.media_kind`'s string encoding of
/// [`crate::models::media_metadata::MediaKind`] — kept a plain `match`
/// (rather than a `FromStr`/`Display` impl on `MediaKind` itself, which
/// belongs to a different module and already has its own `sqlx`/`serde`
/// encodings) since this is the one place a [`MediaRequestDraft`]'s typed
/// `kind` needs to become the request row's `text` column.
pub fn media_kind_str(kind: MediaKind) -> &'static str {
    match kind {
        MediaKind::Movie => "movie",
        MediaKind::Show => "show",
    }
}

fn categories_for_kind(config: &Config, media_kind: &str) -> Vec<i32> {
    if media_kind.eq_ignore_ascii_case("show") {
        config.prowlarr_tv_categories.clone()
    } else {
        config.prowlarr_movie_categories.clone()
    }
}

/// Turn one on-demand [`SearchRelease`] (MUSEM-03) into the
/// [`Release`] shape [`crate::decision::decide_release`] (MUSEM-04)
/// consumes — an in-memory adaptation only, never persisted to the
/// `releases` table (that rolling snapshot belongs to the report-pull
/// worker, MUSE-16).
fn search_release_to_release(sr: &SearchRelease) -> Release {
    Release {
        id: 0,
        media_metadata_id: None,
        episode_id: None,
        indexer_id: sr.indexer_id as i64,
        guid: sr.guid.clone(),
        title: sr.title.clone(),
        info_url: sr.info_url.clone(),
        download_url: sr.download_url.clone(),
        info_hash: sr.info_hash.clone(),
        size_bytes: sr.size,
        publish_date: sr.publish_date,
        seeders: sr.seeders,
        leechers: sr.leechers,
        grabs: sr.grabs,
        freeleech: sr.freeleech,
        freeleech_pct: None,
        categories: sr.categories.clone(),
        parsed_title: sr.parsed.title.clone(),
        parsed_year: sr.parsed.year,
        quality: sr.parsed.quality.clone(),
        resolution: sr.parsed.resolution.clone(),
        source: sr.parsed.source.clone(),
        video_codec: sr.parsed.video_codec.clone(),
        audio_codec: sr.parsed.audio_codec.clone(),
        audio_channels: None,
        hdr: sr.parsed.hdr.clone(),
        edition: sr.parsed.edition.clone(),
        release_group: sr.parsed.release_group.clone(),
        proper_repack: sr.parsed.proper_repack,
        languages: Vec::new(),
        subtitles: Vec::new(),
        parse_confidence: Some(sr.parsed.confidence),
        first_seen_at: chrono::Utc::now(),
        last_seen_at: chrono::Utc::now(),
        expires_at: None,
    }
}

/// Run an on-demand targeted Prowlarr search (MUSEM-03) for `title`,
/// narrowed to `media_kind`'s categories via [`Config`].
pub async fn search_candidates(
    prowlarr: &ProwlarrClient,
    config: &Config,
    title: &str,
    media_kind: &str,
) -> MuseResult<Vec<SearchRelease>> {
    let categories = categories_for_kind(config, media_kind);
    search_releases(prowlarr, config, Some(title), None, &categories, &[]).await
}

/// Everything [`fulfill_request`] needs beyond the request row itself,
/// grouped so call sites (the HTTP handlers in `crate::http::requests`, the
/// future MUSEM-06 wanted-worker) thread one value instead of four.
pub struct AcquisitionDeps<'a> {
    pub pool: &'a PgPool,
    pub config: &'a Config,
    /// `None` when Prowlarr isn't configured — [`fulfill_request`] degrades
    /// to `FulfillOutcome::Skipped` rather than panicking or hanging.
    pub prowlarr: Option<&'a ProwlarrClient>,
    /// `None` when no download client is configured — same degrade
    /// posture, surfaced only once a `Decision::Grab` is actually reached
    /// (a request that resolves to `Decision::Reject` never needs one).
    pub download: Option<&'a dyn DownloadClient>,
}

/// [`fulfill_request`]'s result — always `Ok` unless a genuinely
/// unexpected (e.g. database) failure occurred; a request that fails to
/// grab for an ordinary reason (no candidates, decision-reject, download
/// error) is `Ok(FulfillOutcome::Rejected { .. })`/`Skipped { .. }`, never
/// a propagated error, since the request row itself has already been
/// updated to reflect that outcome.
#[derive(Debug, Clone, PartialEq)]
pub enum FulfillOutcome {
    Grabbed {
        queue_id: i64,
        hash: Option<String>,
    },
    /// `decide_release` returned `Decision::Reject`, or the download client
    /// itself errored on an otherwise-chosen release. Either way the
    /// request row is marked [`RequestStatus::Failed`] and a
    /// `history_events` row is written; never a queue row.
    Rejected {
        reasons: Vec<String>,
    },
    /// Fulfillment could not even be attempted (no Prowlarr client
    /// configured, or the request has no `quality_profile_id`). The
    /// request row is left as-is (still [`RequestStatus::Requested`] /
    /// whatever status it had) for manual operator follow-up — this is the
    /// "missing quality profile → NeedsReview" edge case from the item
    /// spec, expressed via Muse's real (no separate `NeedsReview` value)
    /// [`RequestStatus`] enum by simply not advancing the status.
    Skipped {
        reason: String,
    },
}

/// THE orchestrator: search (MUSEM-03) → decide (MUSEM-04) → grab
/// (MUSEM-02) → persist `download_queue` + `history_events` (MUSEM-01), for
/// one already-created [`MediaRequest`] row. Called both from the
/// `POST /requests`/`approve` HTTP handlers (`crate::http::requests`) and
/// from [`AcquisitionSink::submit`] below.
pub async fn fulfill_request(
    deps: &AcquisitionDeps<'_>,
    request: &MediaRequest,
) -> MuseResult<FulfillOutcome> {
    // Review finding 1 (codex, MUSEM-05 REQUEST_CHANGES): the master
    // acquisition gate (`ExperienceSettings.acquisition.enabled`) MUST be
    // enforced HERE — the single chokepoint every grab path (`POST
    // /requests`, `approve`, `AcquisitionSink`, and any future MUSEM-06
    // worker) funnels through — rather than merely being a caller-side
    // convention that a new call site could forget to check. Fail closed:
    // with the gate off, this is a no-op that leaves the request row
    // exactly as it was (`Requested`, for manual/operator follow-up), never
    // a grab, regardless of who called `fulfill_request`.
    let settings = repo::settings::load(deps.pool).await?;
    if !settings.is_acquisition_enabled() {
        return Ok(FulfillOutcome::Skipped {
            reason: "acquisition is disabled (master gate off)".to_string(),
        });
    }

    let Some(prowlarr) = deps.prowlarr else {
        return Ok(FulfillOutcome::Skipped {
            reason: "prowlarr is not configured; cannot search for releases".to_string(),
        });
    };

    let Some(quality_profile_id) = request.quality_profile_id else {
        return Ok(FulfillOutcome::Skipped {
            reason: "no quality profile set on this request".to_string(),
        });
    };

    // Review finding 2 (codex): a request is not genuinely fulfillable
    // without a configured download client — checked as a precondition
    // (Skipped, request stays `Requested`) rather than discovering it only
    // after a `Decision::Grab`, which would wrongly mark the request
    // `Failed` for a system-capability gap rather than a genuine
    // release-decision rejection.
    let Some(download) = deps.download else {
        return Ok(FulfillOutcome::Skipped {
            reason: "no download client is configured".to_string(),
        });
    };

    let profile = repo::quality::get_profile(deps.pool, quality_profile_id).await?;
    let definitions = repo::quality::list_definitions(deps.pool).await?;
    let custom_formats = repo::quality::list_custom_formats(deps.pool).await?;
    let format_scores =
        repo::quality::list_profile_format_scores(deps.pool, quality_profile_id).await?;

    let candidates = search_candidates(prowlarr, deps.config, &request.title, &request.media_kind).await?;

    let scored: Vec<(SearchRelease, Release)> = candidates
        .iter()
        .map(|sr| (sr.clone(), search_release_to_release(sr)))
        .collect();
    let release_candidates: Vec<ReleaseCandidate> = scored
        .iter()
        .map(|(_, r)| ReleaseCandidate {
            release: r.clone(),
            runtime_minutes: None,
        })
        .collect();
    let policy = ScoringPolicy {
        definitions: &definitions,
        custom_formats: &custom_formats,
        existing: None,
    };
    let decision = decide_release(&release_candidates, &profile, &format_scores, &policy);

    match decision {
        Decision::Grab(choice) => {
            let matched = scored.iter().find(|(_, r)| r.guid == choice.release.guid);
            match grab_and_persist(deps.pool, download, request.id, &choice, matched.map(|(sr, _)| sr))
                .await
            {
                Ok(entry) => {
                    repo::acquisition::update_request_status(
                        deps.pool,
                        request.id,
                        RequestStatus::Grabbed.as_str(),
                    )
                    .await?;
                    record_history_grabbed(deps.pool, request, &entry, matched.map(|(sr, _)| sr))
                        .await?;
                    Ok(FulfillOutcome::Grabbed {
                        queue_id: entry.id,
                        hash: entry.client_hash,
                    })
                }
                Err(e) => {
                    // The download client (or a malformed choice) errored —
                    // surfaced as a Failed request, never a crash and never
                    // a phantom download_queue row (enqueue only happens
                    // AFTER a successful `DownloadClient::add`).
                    let reason = format!("download client error: {e}");
                    repo::acquisition::update_request_status(
                        deps.pool,
                        request.id,
                        RequestStatus::Failed.as_str(),
                    )
                    .await?;
                    record_failed_history(deps.pool, request, &reason).await?;
                    Ok(FulfillOutcome::Rejected {
                        reasons: vec![reason],
                    })
                }
            }
        }
        Decision::Reject { reasons } => {
            repo::acquisition::update_request_status(
                deps.pool,
                request.id,
                RequestStatus::Failed.as_str(),
            )
            .await?;
            record_failed_history(deps.pool, request, &reasons.join("; ")).await?;
            Ok(FulfillOutcome::Rejected { reasons })
        }
    }
}

/// The submitted url a [`DownloadClient::add`] call needs: the release's
/// Prowlarr-proxied `download_url` when present, else a magnet URI built
/// from its `info_hash`. `Err` only when a chosen release carries neither —
/// a malformed candidate that should never have survived
/// [`crate::decision::decide_release`]'s gating, but handled as a typed
/// error here rather than an `unwrap`/panic regardless.
fn resolve_grab_url(choice: &ReleaseChoice) -> MuseResult<String> {
    choice
        .release
        .download_url
        .clone()
        .or_else(|| {
            choice
                .release
                .info_hash
                .as_ref()
                .map(|hash| format!("magnet:?xt=urn:btih:{hash}"))
        })
        .ok_or_else(|| {
            MuseError::Conflict(format!(
                "release {} has neither a download_url nor an info_hash to grab",
                choice.release.guid
            ))
        })
}

async fn grab_and_persist(
    pool: &PgPool,
    download: &dyn DownloadClient,
    request_id: i64,
    choice: &ReleaseChoice,
    matched: Option<&SearchRelease>,
) -> MuseResult<DownloadQueueEntry> {
    let url = resolve_grab_url(choice)?;
    let receipt = download.add(GrabRequest::new(url)).await?;

    repo::acquisition::enqueue_download(
        pool,
        &NewDownloadQueueEntry {
            source: DownloadSource::Request(request_id),
            release_guid: choice.release.guid.clone(),
            release_title: choice.release.title.clone(),
            indexer: matched.and_then(|sr| sr.indexer.clone()),
            download_client: Some("qbittorrent".to_string()),
            client_hash: receipt.hash,
            protocol: matched.and_then(|sr| sr.protocol.clone()),
            size_bytes: choice.release.size_bytes,
        },
    )
    .await
}

async fn record_history_grabbed(
    pool: &PgPool,
    request: &MediaRequest,
    entry: &DownloadQueueEntry,
    matched: Option<&SearchRelease>,
) -> MuseResult<()> {
    repo::acquisition::record_history_event(
        pool,
        &NewHistoryEvent {
            event_type: HistoryEventType::Grabbed.as_str().to_string(),
            media_metadata_id: None,
            monitored_item_id: None,
            download_id: entry.client_hash.clone(),
            source_title: Some(entry.release_title.clone()),
            quality: None,
            data: json!({
                "request_id": request.id,
                "indexer": matched.and_then(|sr| sr.indexer.clone()),
            }),
            languages: json!([]),
        },
    )
    .await?;
    Ok(())
}

async fn record_failed_history(pool: &PgPool, request: &MediaRequest, reason: &str) -> MuseResult<()> {
    repo::acquisition::record_history_event(
        pool,
        &NewHistoryEvent {
            event_type: HistoryEventType::DownloadFailed.as_str().to_string(),
            media_metadata_id: None,
            monitored_item_id: None,
            download_id: None,
            source_title: Some(request.title.clone()),
            quality: None,
            data: json!({"request_id": request.id, "reason": reason}),
            languages: json!([]),
        },
    )
    .await?;
    Ok(())
}

/// The FIRST real (non-`Noop`) [`MediaRequestSink`] this crate ships:
/// creates the `media_requests` row for `draft` and immediately
/// [`fulfill_request`]s it. [`crate::arr::request::submit_if_appropriate`]
/// guarantees `submit` is only ever called for
/// [`crate::arr::request::RequestTier::AutoApprovable`] — see this module's
/// doc for how `POST /requests` (`crate::http::requests`) computes a real
/// availability signal so that tier is actually reachable now.
pub struct AcquisitionSink {
    pub pool: PgPool,
    pub config: Arc<Config>,
    pub prowlarr: Option<Arc<ProwlarrClient>>,
    pub download: Option<Arc<dyn DownloadClient>>,
    /// The quality profile the auto-approved draft is fulfilled against.
    /// `None` degrades to [`FulfillOutcome::Skipped`] (see
    /// [`fulfill_request`]) rather than a blind grab.
    pub quality_profile_id: Option<i64>,
}

#[async_trait::async_trait]
impl MediaRequestSink for AcquisitionSink {
    async fn submit(&self, draft: &MediaRequestDraft) -> MuseResult<()> {
        let new = NewMediaRequest {
            provider_ids: json!({"tmdb": draft.tmdb_id}),
            media_kind: media_kind_str(draft.kind).to_string(),
            title: draft.title.clone(),
            requested_by: None,
            tier: Some("auto_approvable".to_string()),
            quality_profile_id: self.quality_profile_id,
            note: None,
        };
        let request = repo::acquisition::create_request(&self.pool, &new).await?;

        let deps = AcquisitionDeps {
            pool: &self.pool,
            config: &self.config,
            prowlarr: self.prowlarr.as_deref(),
            download: self.download.as_deref(),
        };
        // `fulfill_request` never returns `Err` for an ordinary grab
        // failure (see its own doc) — it always leaves the request row in
        // a terminal, accurate state either way, so `submit`'s only
        // failure mode here is a genuine infra error (already `?`-
        // propagated by `fulfill_request` for the DB calls it makes).
        fulfill_request(&deps, &request).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::download::MockDownloadClient;

    fn release_choice(guid: &str, download_url: Option<&str>, info_hash: Option<&str>) -> ReleaseChoice {
        ReleaseChoice {
            release: Release {
                id: 0,
                media_metadata_id: None,
                episode_id: None,
                indexer_id: 1,
                guid: guid.to_string(),
                title: "Some.Title.2020.1080p.WEB-DL".to_string(),
                info_url: None,
                download_url: download_url.map(str::to_string),
                info_hash: info_hash.map(str::to_string),
                size_bytes: None,
                publish_date: None,
                seeders: None,
                leechers: None,
                grabs: None,
                freeleech: false,
                freeleech_pct: None,
                categories: vec![],
                parsed_title: None,
                parsed_year: None,
                quality: None,
                resolution: Some("1080p".to_string()),
                source: Some("WEB-DL".to_string()),
                video_codec: None,
                audio_codec: None,
                audio_channels: None,
                hdr: vec![],
                edition: None,
                release_group: None,
                proper_repack: false,
                languages: vec![],
                subtitles: vec![],
                parse_confidence: None,
                first_seen_at: chrono::Utc::now(),
                last_seen_at: chrono::Utc::now(),
                expires_at: None,
            },
            total_score: 0,
            quality_tier: "web-1080p".to_string(),
            reason: "test".to_string(),
        }
    }

    #[test]
    fn resolve_grab_url_prefers_download_url() {
        let choice = release_choice("g1", Some("http://example.invalid/dl/g1"), Some("deadbeef"));
        assert_eq!(
            resolve_grab_url(&choice).unwrap(),
            "http://example.invalid/dl/g1"
        );
    }

    #[test]
    fn resolve_grab_url_falls_back_to_a_magnet_built_from_info_hash() {
        let choice = release_choice("g2", None, Some("deadbeef"));
        assert_eq!(
            resolve_grab_url(&choice).unwrap(),
            "magnet:?xt=urn:btih:deadbeef"
        );
    }

    #[test]
    fn resolve_grab_url_errors_when_neither_is_present() {
        let choice = release_choice("g3", None, None);
        assert!(resolve_grab_url(&choice).is_err());
    }

    #[test]
    fn media_kind_str_matches_the_media_requests_column_convention() {
        assert_eq!(media_kind_str(MediaKind::Movie), "movie");
        assert_eq!(media_kind_str(MediaKind::Show), "show");
    }

    #[test]
    fn categories_for_kind_picks_tv_categories_for_show() {
        let config = Config {
            prowlarr_movie_categories: vec![2000],
            prowlarr_tv_categories: vec![5000],
            ..Default::default()
        };
        assert_eq!(categories_for_kind(&config, "show"), vec![5000]);
        assert_eq!(categories_for_kind(&config, "movie"), vec![2000]);
    }

    /// A `MockDownloadClient` is a real trait object usable as
    /// `AcquisitionDeps::download` — proves the trait-object seam compiles
    /// and behaves as expected end-to-end without a database (the
    /// DB-backed `fulfill_request` scenarios live in `db_gated` below).
    #[tokio::test]
    async fn mock_download_client_add_is_reachable_through_the_trait_object() {
        let mock = MockDownloadClient::new();
        let download: &dyn DownloadClient = &mock;
        let receipt = download
            .add(GrabRequest::new("magnet:?xt=urn:btih:AA"))
            .await
            .expect("mock add should succeed");
        assert_eq!(receipt.raw_response, "Ok.");
        assert_eq!(mock.added_count(), 1);
    }
}

/// DB-backed coverage for the full `fulfill_request` orchestration —
/// `MUSE_TEST_DATABASE_URL`-gated, same convention as
/// `repo::acquisition::db_gated` / `repo::settings::db_gated`. Skips
/// cleanly, never fails, when no test database is configured.
#[cfg(test)]
mod db_gated {
    use super::*;
    use crate::download::MockDownloadClient;
    use crate::models::acquisition::{NewMediaRequest, QueueStatus};
    use crate::models::quality::{NewQualityDefinition, NewQualityProfile};
    use httpmock::prelude::*;

    async fn test_pool_or_skip(test_name: &str) -> Option<PgPool> {
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

    async fn seed_profile_allowing_web_1080p(pool: &PgPool) -> i64 {
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
                name: format!("musem05-profile-{}", unique_suffix()),
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

    async fn seed_request(pool: &PgPool, title: &str, quality_profile_id: Option<i64>) -> MediaRequest {
        repo::acquisition::create_request(
            pool,
            &NewMediaRequest {
                provider_ids: serde_json::json!({}),
                media_kind: "movie".to_string(),
                title: title.to_string(),
                requested_by: None,
                tier: None,
                quality_profile_id,
                note: None,
            },
        )
        .await
        .expect("seed media_request")
    }

    fn search_response_body(guid: &str, title: &str) -> String {
        format!(
            r#"[{{"guid": "{guid}", "title": "{title}", "indexerId": 1,
                  "indexer": "TestIndexer", "protocol": "torrent",
                  "downloadUrl": "http://example.invalid/dl/{guid}",
                  "categories": [{{"id": 2000, "name": "Movies"}}]}}]"#
        )
    }

    /// Review finding 1 (codex): `fulfill_request` now enforces the master
    /// acquisition gate itself, loading `ExperienceSettings` from `pool` on
    /// every call — so every DB-gated scenario below that expects a grab to
    /// actually happen must explicitly turn the gate ON first (it defaults
    /// OFF, see `crate::settings::AcquisitionSettings`). Scenarios that
    /// expect NO grab regardless (missing quality profile, decision-reject,
    /// download-client error) still turn it on too, so those tests keep
    /// isolating the ORIGINAL reason they're testing rather than silently
    /// becoming "skipped because the gate is off" — see the dedicated
    /// `gate_off_*` tests below for the gate itself.
    async fn save_settings(pool: &PgPool, acquisition_enabled: bool) {
        let mut settings = crate::settings::ExperienceSettings::default();
        settings.acquisition = crate::settings::AcquisitionSettings {
            enabled: acquisition_enabled,
        };
        repo::settings::save(pool, &settings)
            .await
            .expect("save settings");
    }

    #[tokio::test]
    async fn autoapprovable_grab_writes_queue_and_history_rows() {
        let Some(pool) = test_pool_or_skip("autoapprovable_grab_writes_queue_and_history_rows").await
        else {
            return;
        };
        save_settings(&pool, true).await;
        let profile_id = seed_profile_allowing_web_1080p(&pool).await;
        let title = format!("Musem05 Grab Movie {}", unique_suffix());
        let request = seed_request(&pool, &title, Some(profile_id)).await;

        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/api/v1/search");
            then.status(200)
                .header("content-type", "application/json")
                .body(search_response_body(
                    "musem05-guid-1",
                    &format!("{title}.2020.1080p.WEB-DL"),
                ));
        });
        let prowlarr = ProwlarrClient::new(server.base_url(), "test-key").expect("client");
        let config = Config::default();
        let download = MockDownloadClient::new();

        let deps = AcquisitionDeps {
            pool: &pool,
            config: &config,
            prowlarr: Some(&prowlarr),
            download: Some(&download),
        };

        let outcome = fulfill_request(&deps, &request).await.expect("fulfill_request");
        match outcome {
            FulfillOutcome::Grabbed { queue_id, .. } => {
                let entry = repo::acquisition::get_download_queue_entry(&pool, queue_id)
                    .await
                    .expect("queue entry should exist");
                assert_eq!(entry.status, QueueStatus::Queued.as_str());
                assert_eq!(entry.request_id, Some(request.id));
            }
            other => panic!("expected Grabbed, got {other:?}"),
        }
        assert_eq!(download.added_count(), 1, "grab must call the download client exactly once");

        let reloaded = repo::acquisition::get_request(&pool, request.id).await.expect("reload request");
        assert_eq!(reloaded.status, RequestStatus::Grabbed.as_str());

        // MockDownloadClient never resolves a hash for a non-magnet
        // download_url, so `download_id` is None on the written history
        // row -- assert via a direct query on `data->>'request_id'` rather
        // than the by-hash lookup helper (`list_history_for_download`).
        let count: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM history_events WHERE event_type = 'grabbed' AND data->>'request_id' = $1",
        )
        .bind(request.id.to_string())
        .fetch_one(&pool)
        .await
        .expect("count grabbed history rows");
        assert_eq!(count, 1);
    }

    #[tokio::test]
    async fn decision_reject_marks_failed_with_no_queue_row() {
        let Some(pool) = test_pool_or_skip("decision_reject_marks_failed_with_no_queue_row").await
        else {
            return;
        };
        save_settings(&pool, true).await;
        // A profile with zero allowed tiers -- every candidate is rejected.
        let profile_id = crate::repo::quality::create_profile(
            &pool,
            &NewQualityProfile {
                name: format!("musem05-empty-profile-{}", unique_suffix()),
                cutoff_quality_id: None,
                items: serde_json::json!([]),
                upgrade_allowed: true,
                natural_language_intent: None,
            },
        )
        .await
        .expect("seed empty quality_profile")
        .id;
        let title = format!("Musem05 Reject Movie {}", unique_suffix());
        let request = seed_request(&pool, &title, Some(profile_id)).await;

        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/api/v1/search");
            then.status(200)
                .header("content-type", "application/json")
                .body(search_response_body(
                    "musem05-guid-reject",
                    &format!("{title}.2020.1080p.WEB-DL"),
                ));
        });
        let prowlarr = ProwlarrClient::new(server.base_url(), "test-key").expect("client");
        let config = Config::default();
        let download = MockDownloadClient::new();

        let deps = AcquisitionDeps {
            pool: &pool,
            config: &config,
            prowlarr: Some(&prowlarr),
            download: Some(&download),
        };

        let outcome = fulfill_request(&deps, &request).await.expect("fulfill_request");
        assert!(matches!(outcome, FulfillOutcome::Rejected { .. }));
        assert_eq!(download.added_count(), 0, "a rejected decision must never grab");

        let reloaded = repo::acquisition::get_request(&pool, request.id).await.expect("reload request");
        assert_eq!(reloaded.status, RequestStatus::Failed.as_str());

        let queue = repo::acquisition::list_download_queue_by_status(&pool, QueueStatus::Queued.as_str())
            .await
            .expect("list queue");
        assert!(
            !queue.iter().any(|q| q.request_id == Some(request.id)),
            "a rejected decision must never leave a download_queue row"
        );
    }

    #[tokio::test]
    async fn download_client_error_surfaces_failed_not_a_crash() {
        let Some(pool) = test_pool_or_skip("download_client_error_surfaces_failed_not_a_crash").await
        else {
            return;
        };
        save_settings(&pool, true).await;
        let profile_id = seed_profile_allowing_web_1080p(&pool).await;
        let title = format!("Musem05 Qbit Error Movie {}", unique_suffix());
        let request = seed_request(&pool, &title, Some(profile_id)).await;

        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/api/v1/search");
            then.status(200)
                .header("content-type", "application/json")
                .body(search_response_body(
                    "musem05-guid-err",
                    &format!("{title}.2020.1080p.WEB-DL"),
                ));
        });
        let prowlarr = ProwlarrClient::new(server.base_url(), "test-key").expect("client");
        let config = Config::default();

        struct AlwaysFailsDownloadClient;
        #[async_trait::async_trait]
        impl DownloadClient for AlwaysFailsDownloadClient {
            async fn add(&self, _req: GrabRequest) -> MuseResult<crate::download::GrabReceipt> {
                Err(MuseError::Upstream {
                    status: 500,
                    message: "simulated qbittorrent failure".to_string(),
                })
            }
            async fn list(&self) -> MuseResult<Vec<crate::download::TorrentStatus>> {
                Ok(Vec::new())
            }
            async fn info(&self, _hash: &str) -> MuseResult<Option<crate::download::TorrentStatus>> {
                Ok(None)
            }
            async fn delete(&self, _hash: &str, _delete_files: bool) -> MuseResult<()> {
                Ok(())
            }
        }
        let download = AlwaysFailsDownloadClient;

        let deps = AcquisitionDeps {
            pool: &pool,
            config: &config,
            prowlarr: Some(&prowlarr),
            download: Some(&download),
        };

        let outcome = fulfill_request(&deps, &request).await.expect("fulfill_request must not error");
        assert!(matches!(outcome, FulfillOutcome::Rejected { .. }));

        let reloaded = repo::acquisition::get_request(&pool, request.id).await.expect("reload request");
        assert_eq!(reloaded.status, RequestStatus::Failed.as_str());

        let queue = repo::acquisition::list_download_queue_by_status(&pool, QueueStatus::Queued.as_str())
            .await
            .expect("list queue");
        assert!(
            !queue.iter().any(|q| q.request_id == Some(request.id)),
            "a download-client error must never leave a download_queue row"
        );
    }

    #[tokio::test]
    async fn missing_quality_profile_skips_without_grabbing() {
        let Some(pool) = test_pool_or_skip("missing_quality_profile_skips_without_grabbing").await
        else {
            return;
        };
        save_settings(&pool, true).await;
        let title = format!("Musem05 No Profile Movie {}", unique_suffix());
        let request = seed_request(&pool, &title, None).await;

        let server = MockServer::start();
        let prowlarr = ProwlarrClient::new(server.base_url(), "test-key").expect("client");
        let config = Config::default();
        let download = MockDownloadClient::new();

        let deps = AcquisitionDeps {
            pool: &pool,
            config: &config,
            prowlarr: Some(&prowlarr),
            download: Some(&download),
        };

        let outcome = fulfill_request(&deps, &request).await.expect("fulfill_request");
        assert!(matches!(outcome, FulfillOutcome::Skipped { .. }));
        assert_eq!(download.added_count(), 0);

        // Status is untouched -- still Requested, for manual review.
        let reloaded = repo::acquisition::get_request(&pool, request.id).await.expect("reload request");
        assert_eq!(reloaded.status, RequestStatus::Requested.as_str());
    }

    /// Review finding 1 (codex): with the master gate OFF, `fulfill_request`
    /// must never reach `DownloadClient::add` even for an otherwise
    /// perfectly grabbable candidate (configured Prowlarr + download client
    /// + quality profile, and a search result that would decide `Grab`).
    #[tokio::test]
    async fn gate_off_prevents_a_grab_even_for_an_otherwise_grabbable_request() {
        let Some(pool) =
            test_pool_or_skip("gate_off_prevents_a_grab_even_for_an_otherwise_grabbable_request").await
        else {
            return;
        };
        save_settings(&pool, false).await;
        let profile_id = seed_profile_allowing_web_1080p(&pool).await;
        let title = format!("Musem05 Gate Off Movie {}", unique_suffix());
        let request = seed_request(&pool, &title, Some(profile_id)).await;

        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/api/v1/search");
            then.status(200)
                .header("content-type", "application/json")
                .body(search_response_body(
                    "gate-off-fulfill-guid",
                    &format!("{title}.2020.1080p.WEB-DL"),
                ));
        });
        let prowlarr = ProwlarrClient::new(server.base_url(), "test-key").expect("client");
        let config = Config::default();
        let download = MockDownloadClient::new();

        let deps = AcquisitionDeps {
            pool: &pool,
            config: &config,
            prowlarr: Some(&prowlarr),
            download: Some(&download),
        };

        let outcome = fulfill_request(&deps, &request).await.expect("fulfill_request");
        assert!(
            matches!(outcome, FulfillOutcome::Skipped { .. }),
            "expected Skipped with the master gate off, got {outcome:?}"
        );
        assert_eq!(download.added_count(), 0, "the master gate being off must prevent any grab");

        let reloaded = repo::acquisition::get_request(&pool, request.id).await.expect("reload request");
        assert_eq!(
            reloaded.status,
            RequestStatus::Requested.as_str(),
            "a gate-off skip must leave the request Requested, not Failed or Grabbed"
        );

        let queue = repo::acquisition::list_download_queue_by_status(&pool, QueueStatus::Queued.as_str())
            .await
            .expect("list queue");
        assert!(!queue.iter().any(|q| q.request_id == Some(request.id)));
    }

    #[tokio::test]
    async fn acquisition_sink_creates_and_fulfills_a_request() {
        let Some(pool) = test_pool_or_skip("acquisition_sink_creates_and_fulfills_a_request").await
        else {
            return;
        };
        save_settings(&pool, true).await;
        let profile_id = seed_profile_allowing_web_1080p(&pool).await;

        let title = format!("Musem05 Sink Movie {}", unique_suffix());
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/api/v1/search");
            then.status(200)
                .header("content-type", "application/json")
                .body(search_response_body(
                    "musem05-sink-guid",
                    &format!("{title}.2020.1080p.WEB-DL"),
                ));
        });
        let prowlarr = Arc::new(ProwlarrClient::new(server.base_url(), "test-key").expect("client"));
        let download: Arc<dyn DownloadClient> = Arc::new(MockDownloadClient::new());

        let sink = AcquisitionSink {
            pool: pool.clone(),
            config: Arc::new(Config::default()),
            prowlarr: Some(prowlarr),
            download: Some(download),
            quality_profile_id: Some(profile_id),
        };

        let draft = MediaRequestDraft {
            tmdb_id: "tmdb-musem05".to_string(),
            title: title.clone(),
            kind: MediaKind::Movie,
        };
        sink.submit(&draft).await.expect("sink submit should succeed");

        let requests = repo::acquisition::list_requests_by_status(&pool, RequestStatus::Grabbed.as_str())
            .await
            .expect("list grabbed requests");
        assert!(
            requests.iter().any(|r| r.title == title),
            "the sink must have created and grabbed a request for the draft's title"
        );
    }

    /// Review finding 1 (codex): the master gate is enforced INSIDE
    /// `fulfill_request`, so `AcquisitionSink::submit` — the sink
    /// `submit_if_appropriate` calls ONLY for `RequestTier::AutoApprovable`
    /// — must still perform NO grab when the gate is off, even though it's
    /// the one path that's structurally already past the tiered-safety
    /// check. Proves the gate is a real, unbypassable property of the grab
    /// chokepoint, not just a `POST /requests`-side convention.
    #[tokio::test]
    async fn acquisition_sink_performs_no_grab_when_the_master_gate_is_off() {
        let Some(pool) =
            test_pool_or_skip("acquisition_sink_performs_no_grab_when_the_master_gate_is_off").await
        else {
            return;
        };
        save_settings(&pool, false).await;
        let profile_id = seed_profile_allowing_web_1080p(&pool).await;

        let title = format!("Musem05 Sink Gate Off Movie {}", unique_suffix());
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/api/v1/search");
            then.status(200)
                .header("content-type", "application/json")
                .body(search_response_body(
                    "musem05-sink-gate-off-guid",
                    &format!("{title}.2020.1080p.WEB-DL"),
                ));
        });
        let prowlarr = Arc::new(ProwlarrClient::new(server.base_url(), "test-key").expect("client"));
        let mock_download = Arc::new(MockDownloadClient::new());
        let download: Arc<dyn DownloadClient> = mock_download.clone();

        let sink = AcquisitionSink {
            pool: pool.clone(),
            config: Arc::new(Config::default()),
            prowlarr: Some(prowlarr),
            download: Some(download),
            quality_profile_id: Some(profile_id),
        };

        let draft = MediaRequestDraft {
            tmdb_id: "tmdb-musem05-gate-off".to_string(),
            title: title.clone(),
            kind: MediaKind::Movie,
        };
        sink.submit(&draft).await.expect("sink submit should succeed even when gated off");

        assert_eq!(
            mock_download.added_count(),
            0,
            "the master gate being off must prevent the sink from ever grabbing"
        );

        let requests = repo::acquisition::list_requests_by_status(&pool, RequestStatus::Requested.as_str())
            .await
            .expect("list requested requests");
        assert!(
            requests.iter().any(|r| r.title == title),
            "the sink must still have created the request row (persist-but-never-act)"
        );

        let queue = repo::acquisition::list_download_queue_by_status(&pool, QueueStatus::Queued.as_str())
            .await
            .expect("list queue");
        assert!(!queue.iter().any(|q| q.release_title.contains(title.as_str())));
    }
}
