//! MUSE-28's "director": keeps every `mode='linear'` channel's
//! `channel_programs` grid (MUSE-23) filled a rolling
//! [`crate::config::Config::channel_guide_window_hours`] ahead of now.
//!
//! This is a **deterministic** composer — round-robin across the
//! channel's matching content, with an interstitial inserted every N
//! items per `channel.rules`. The agentic, taste-aware composer (MUSE-24)
//! is a separate, later item; this scheduler works whether or not it
//! exists, and never calls it.
//!
//! Idempotency/contiguity: each call resumes from
//! `MAX(channel_programs.end_at)` for the channel (never before "now", so
//! a channel that fell behind catches up to the present rather than
//! backfilling the past) and walks forward with `start_at` of each new row
//! equal to the previous row's `end_at` — by construction this can never
//! violate `channel_programs`' `UNIQUE(channel_id, start_at)` or
//! `end_at > start_at` checks, never overlaps, and never leaves a gap.
//! Re-running when the window is already full is a no-op.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration as StdDuration;

use chrono::{Duration, Utc};
use sqlx::PgPool;

use crate::error::MuseResult;
use crate::http::AppState;
use crate::models::channel::{Channel, ChannelProgramItemType, NewChannelProgram};
use crate::models::interstitial::{Interstitial, InterstitialKind};
use crate::repo;

/// Fallback runtime for an episode with no `runtime_minutes` on file.
const DEFAULT_EPISODE_MINUTES: i64 = 22;
/// Fallback runtime for a movie with no `runtime_minutes` on file.
const DEFAULT_MOVIE_MINUTES: i64 = 100;
/// Fallback duration for an interstitial with no `duration_ms` on file.
const DEFAULT_INTERSTITIAL_MS: i64 = 30_000;
/// Default cadence (content items between interstitials) when
/// `channel.rules.interstitial_every` isn't set or isn't a positive int.
const DEFAULT_INTERSTITIAL_EVERY: u32 = 4;

#[derive(Debug, Clone, sqlx::FromRow)]
struct EpisodeCandidate {
    episode_id: i64,
    media_item_id: i64,
    library_id: i64,
    show_title: String,
    episode_title: Option<String>,
    season_number: i32,
    episode_number: i32,
    runtime_minutes: Option<i32>,
}

#[derive(Debug, Clone, sqlx::FromRow)]
struct MovieCandidate {
    media_item_id: i64,
    library_id: i64,
    title: String,
    runtime_minutes: Option<i32>,
}

/// One thing the round-robin content pool can place onto the grid — an
/// episode or movie. Interstitials are a separate pool, handled inline in
/// [`ensure_rolling_window`] rather than as a `ScheduleItem` variant.
#[derive(Debug, Clone)]
enum ScheduleItem {
    Episode(EpisodeCandidate),
    Movie(MovieCandidate),
}

impl ScheduleItem {
    fn duration_minutes(&self) -> i64 {
        match self {
            ScheduleItem::Episode(e) => e.runtime_minutes.map(i64::from).unwrap_or(DEFAULT_EPISODE_MINUTES),
            ScheduleItem::Movie(m) => m.runtime_minutes.map(i64::from).unwrap_or(DEFAULT_MOVIE_MINUTES),
        }
    }

    fn item_type(&self) -> ChannelProgramItemType {
        match self {
            ScheduleItem::Episode(_) => ChannelProgramItemType::Episode,
            ScheduleItem::Movie(_) => ChannelProgramItemType::Movie,
        }
    }

    fn title(&self) -> String {
        match self {
            ScheduleItem::Episode(e) => e.show_title.clone(),
            ScheduleItem::Movie(m) => m.title.clone(),
        }
    }

    /// The "S2E4" shape `xmltv::render` parses back out for
    /// `<episode-num>` — kept as JUST the code (no episode title) so that
    /// parse stays exact; the human episode title, if any, goes in
    /// `description` instead (see [`Self::description`]).
    fn subtitle(&self) -> Option<String> {
        match self {
            ScheduleItem::Episode(e) => Some(format!("S{}E{}", e.season_number, e.episode_number)),
            ScheduleItem::Movie(_) => None,
        }
    }

    fn description(&self) -> Option<String> {
        match self {
            ScheduleItem::Episode(e) => e.episode_title.clone(),
            ScheduleItem::Movie(_) => None,
        }
    }

    fn episode_id(&self) -> Option<i64> {
        match self {
            ScheduleItem::Episode(e) => Some(e.episode_id),
            ScheduleItem::Movie(_) => None,
        }
    }

    fn media_item_id(&self) -> Option<i64> {
        match self {
            ScheduleItem::Episode(_) => None,
            ScheduleItem::Movie(m) => Some(m.media_item_id),
        }
    }
}

/// Read `channel.rules.interstitial_every` (a positive integer), falling
/// back to [`DEFAULT_INTERSTITIAL_EVERY`] when absent/invalid.
fn interstitial_every(channel: &Channel) -> u32 {
    channel
        .rules
        .get("interstitial_every")
        .and_then(|v| v.as_u64())
        .filter(|&n| n > 0)
        .map(|n| n as u32)
        .unwrap_or(DEFAULT_INTERSTITIAL_EVERY)
}

/// Read `channel.rules.interstitial_kind`, if present and a recognized
/// [`InterstitialKind`] value; `None` means "any kind" for
/// `list_by_kind_decade_theme`.
fn interstitial_kind(channel: &Channel) -> Option<InterstitialKind> {
    let raw = channel.rules.get("interstitial_kind")?.as_str()?;
    serde_json::from_value(serde_json::Value::String(raw.to_string())).ok()
}

/// Read `channel.rules.content_kind` ("episode" | "movie" | "mixed",
/// default "episode").
fn content_kind(channel: &Channel) -> String {
    channel
        .rules
        .get("content_kind")
        .and_then(|v| v.as_str())
        .unwrap_or("episode")
        .to_string()
}

/// Read `channel.rules.library_ids` (an array of library ids), if present
/// — scopes the round-robin content pool to just those libraries (e.g. a
/// themed channel over one show's library). `None` (the field absent)
/// means "the whole catalog", matching a channel like "Discover" that
/// intentionally spans everything.
fn library_scope(channel: &Channel) -> Option<Vec<i64>> {
    let arr = channel.rules.get("library_ids")?.as_array()?;
    let ids: Vec<i64> = arr.iter().filter_map(|v| v.as_i64()).collect();
    if ids.is_empty() {
        None
    } else {
        Some(ids)
    }
}

async fn episode_candidates(pool: &PgPool, scope: Option<&[i64]>) -> MuseResult<Vec<EpisodeCandidate>> {
    let rows = sqlx::query_as::<_, EpisodeCandidate>(
        r#"
        SELECT
            e.id AS episode_id,
            e.media_item_id,
            mi.library_id,
            mm.title AS show_title,
            e.title AS episode_title,
            s.season_number,
            e.episode_number,
            e.runtime_minutes
        FROM episodes e
        JOIN seasons s ON s.id = e.season_id
        JOIN media_items mi ON mi.id = e.media_item_id
        JOIN media_metadata mm ON mm.id = mi.media_metadata_id
        WHERE mi.in_library = true AND e.has_file = true
        ORDER BY e.media_item_id, s.season_number, e.episode_number
        "#,
    )
    .fetch_all(pool)
    .await
    .map_err(crate::error::MuseError::Database)?;

    Ok(match scope {
        Some(ids) => rows.into_iter().filter(|e| ids.contains(&e.library_id)).collect(),
        None => rows,
    })
}

async fn movie_candidates(pool: &PgPool, scope: Option<&[i64]>) -> MuseResult<Vec<MovieCandidate>> {
    let rows = sqlx::query_as::<_, MovieCandidate>(
        r#"
        SELECT
            mi.id AS media_item_id,
            mi.library_id,
            mm.title AS title,
            mm.runtime_minutes AS runtime_minutes
        FROM media_items mi
        JOIN media_metadata mm ON mm.id = mi.media_metadata_id
        WHERE mi.in_library = true AND mm.kind = 'movie'
        ORDER BY mi.id
        "#,
    )
    .fetch_all(pool)
    .await
    .map_err(crate::error::MuseError::Database)?;

    Ok(match scope {
        Some(ids) => rows.into_iter().filter(|m| ids.contains(&m.library_id)).collect(),
        None => rows,
    })
}

/// Build round-robin content groups: one group per show (grouped by
/// `media_item_id`, episodes already ordered season/episode ascending) or
/// one group per movie (a group of one). Walking groups in rotation and
/// wrapping each group back to its start on exhaustion gives an endless,
/// endlessly-repeatable (comfort-rewatch) content source.
fn build_groups(episodes: Vec<EpisodeCandidate>, movies: Vec<MovieCandidate>, kind: &str) -> Vec<Vec<ScheduleItem>> {
    let mut groups: Vec<Vec<ScheduleItem>> = Vec::new();

    if kind == "episode" || kind == "mixed" {
        let mut by_show: HashMap<i64, Vec<ScheduleItem>> = HashMap::new();
        let mut order: Vec<i64> = Vec::new();
        for ep in episodes {
            let id = ep.media_item_id;
            if !by_show.contains_key(&id) {
                order.push(id);
            }
            by_show.entry(id).or_default().push(ScheduleItem::Episode(ep));
        }
        for id in order {
            if let Some(items) = by_show.remove(&id) {
                groups.push(items);
            }
        }
    }

    if kind == "movie" || kind == "mixed" {
        for movie in movies {
            groups.push(vec![ScheduleItem::Movie(movie)]);
        }
    }

    groups
}

/// A cursor over [`build_groups`]' round-robin groups: yields the next
/// item in rotation, forever (wrapping each group independently so a
/// shorter group repeats more often than a longer one, rather than
/// stalling once exhausted).
struct RoundRobin {
    groups: Vec<Vec<ScheduleItem>>,
    positions: Vec<usize>,
    next_group: usize,
}

impl RoundRobin {
    fn new(groups: Vec<Vec<ScheduleItem>>) -> Self {
        let positions = vec![0; groups.len()];
        Self {
            groups,
            positions,
            next_group: 0,
        }
    }

    fn is_empty(&self) -> bool {
        self.groups.is_empty() || self.groups.iter().all(|g| g.is_empty())
    }

    fn next(&mut self) -> Option<ScheduleItem> {
        if self.is_empty() {
            return None;
        }
        let n = self.groups.len();
        for _ in 0..n {
            let g = self.next_group;
            self.next_group = (self.next_group + 1) % n;
            if self.groups[g].is_empty() {
                continue;
            }
            let pos = self.positions[g] % self.groups[g].len();
            self.positions[g] += 1;
            return Some(self.groups[g][pos].clone());
        }
        None
    }
}

/// Ensure `channel`'s `channel_programs` grid is filled from wherever it
/// currently ends (or from `now` if empty/behind) out to `now + window`.
/// Returns the number of newly-inserted rows (0 if already full, or if the
/// channel has no matching content to schedule).
pub async fn ensure_rolling_window(pool: &PgPool, channel: &Channel, window: Duration) -> MuseResult<usize> {
    let now = Utc::now();
    let target_end = now + window;

    let existing_end = repo::channel::latest_program_end(pool, channel.id).await?;
    let mut cursor = match existing_end {
        Some(end) if end > now => end,
        _ => now,
    };

    if cursor >= target_end {
        return Ok(0);
    }

    let kind = content_kind(channel);
    let scope = library_scope(channel);
    let episodes = if kind != "movie" {
        episode_candidates(pool, scope.as_deref()).await?
    } else {
        Vec::new()
    };
    let movies = if kind != "episode" {
        movie_candidates(pool, scope.as_deref()).await?
    } else {
        Vec::new()
    };
    let groups = build_groups(episodes, movies, &kind);
    let mut content = RoundRobin::new(groups);

    if content.is_empty() {
        tracing::warn!(channel_id = channel.id, "no schedulable content for linear channel; grid not filled");
        return Ok(0);
    }

    let interstitials = repo::interstitial::list_by_kind_decade_theme(pool, interstitial_kind(channel), None, None).await?;
    let every = interstitial_every(channel);

    let mut inserted = 0usize;
    let mut since_interstitial = 0u32;
    let mut interstitial_idx = 0usize;

    while cursor < target_end {
        // Interstitial slot?
        if every > 0 && since_interstitial >= every && !interstitials.is_empty() {
            let inter: &Interstitial = &interstitials[interstitial_idx % interstitials.len()];
            interstitial_idx += 1;
            let dur_ms = inter.duration_ms.unwrap_or(DEFAULT_INTERSTITIAL_MS).max(1);
            let start = cursor;
            let end = start + Duration::milliseconds(dur_ms);
            repo::channel::create_program(
                pool,
                &NewChannelProgram {
                    channel_id: channel.id,
                    item_type: ChannelProgramItemType::Interstitial,
                    media_item_id: None,
                    episode_id: None,
                    interstitial_id: Some(inter.id),
                    title: inter.title.clone().unwrap_or_else(|| "Interstitial".to_string()),
                    subtitle: None,
                    description: None,
                    artwork_url: None,
                    start_at: start,
                    end_at: end,
                    duration_ms: dur_ms,
                    rationale: Some("scheduled interstitial cadence (MUSE-28 director)".to_string()),
                },
            )
            .await?;
            cursor = end;
            inserted += 1;
            since_interstitial = 0;
            continue;
        }

        let Some(item) = content.next() else {
            break;
        };
        let dur_ms = item.duration_minutes() * 60_000;
        let start = cursor;
        let end = start + Duration::milliseconds(dur_ms);
        repo::channel::create_program(
            pool,
            &NewChannelProgram {
                channel_id: channel.id,
                item_type: item.item_type(),
                media_item_id: item.media_item_id(),
                episode_id: item.episode_id(),
                interstitial_id: None,
                title: item.title(),
                subtitle: item.subtitle(),
                description: item.description(),
                artwork_url: None,
                start_at: start,
                end_at: end,
                duration_ms: dur_ms,
                rationale: Some("round-robin director placement (MUSE-28)".to_string()),
            },
        )
        .await?;
        cursor = end;
        inserted += 1;
        since_interstitial += 1;
    }

    Ok(inserted)
}

/// Top off every linear channel's rolling window once. Errors on one
/// channel are logged and skipped rather than aborting the rest (same
/// graceful-degrade posture as the rest of the crate's background
/// workers).
pub async fn fill_all_channels(pool: &PgPool, window_hours: i64) {
    let channels = match repo::channel::list_linear_channels(pool).await {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(error = %e, "failed to list linear channels for tuner scheduler");
            return;
        }
    };
    let window = Duration::hours(window_hours);
    for channel in channels {
        match ensure_rolling_window(pool, &channel, window).await {
            Ok(n) if n > 0 => tracing::info!(channel_id = channel.id, inserted = n, "extended linear guide grid"),
            Ok(_) => {}
            Err(e) => tracing::error!(channel_id = channel.id, error = %e, "failed to extend linear guide grid"),
        }
    }
}

/// Spawn the background director loop: on `Config::channel_scheduler_tick_secs`
/// cadence, top off every linear channel's rolling
/// `Config::channel_guide_window_hours` window.
pub fn spawn(state: Arc<AppState>) {
    let tick = StdDuration::from_secs(state.config.channel_scheduler_tick_secs.max(1));
    let window_hours = state.config.channel_guide_window_hours;
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(tick);
        loop {
            interval.tick().await;
            fill_all_channels(&state.pool, window_hours).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ep(media_item_id: i64, season: i32, episode: i32) -> EpisodeCandidate {
        EpisodeCandidate {
            episode_id: media_item_id * 100 + episode as i64,
            media_item_id,
            library_id: 1,
            show_title: format!("Show {media_item_id}"),
            episode_title: Some(format!("Ep {episode}")),
            season_number: season,
            episode_number: episode,
            runtime_minutes: Some(22),
        }
    }

    #[test]
    fn round_robin_alternates_across_groups_and_wraps() {
        let groups = build_groups(
            vec![ep(1, 1, 1), ep(1, 1, 2), ep(2, 1, 1)],
            vec![],
            "episode",
        );
        assert_eq!(groups.len(), 2);
        let mut rr = RoundRobin::new(groups);

        let a = rr.next().unwrap();
        let b = rr.next().unwrap();
        let c = rr.next().unwrap();
        let d = rr.next().unwrap();

        // group 1 (2 eps) then group 2 (1 ep, wraps) alternate
        assert_eq!(a.title(), "Show 1");
        assert_eq!(b.title(), "Show 2");
        assert_eq!(c.title(), "Show 1");
        assert_eq!(d.title(), "Show 2"); // wrapped back to its only episode
    }

    #[test]
    fn round_robin_on_empty_groups_yields_none() {
        let mut rr = RoundRobin::new(vec![]);
        assert!(rr.is_empty());
        assert!(rr.next().is_none());
    }

    #[test]
    fn interstitial_every_reads_rules_with_fallback() {
        let mk = |rules: serde_json::Value| Channel {
            id: 1,
            account_id: None,
            name: "x".to_string(),
            kind: crate::models::ChannelKind::Preset,
            mode: crate::models::ChannelMode::Linear,
            channel_number: None,
            target_client_id: None,
            directive: None,
            rules,
            is_preset: false,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };

        assert_eq!(interstitial_every(&mk(serde_json::json!({}))), DEFAULT_INTERSTITIAL_EVERY);
        assert_eq!(interstitial_every(&mk(serde_json::json!({"interstitial_every": 2}))), 2);
        assert_eq!(interstitial_every(&mk(serde_json::json!({"interstitial_every": 0}))), DEFAULT_INTERSTITIAL_EVERY);
        assert_eq!(interstitial_every(&mk(serde_json::json!({"interstitial_every": "bogus"}))), DEFAULT_INTERSTITIAL_EVERY);
    }

    #[test]
    fn content_kind_defaults_to_episode() {
        let ch = Channel {
            id: 1,
            account_id: None,
            name: "x".to_string(),
            kind: crate::models::ChannelKind::Preset,
            mode: crate::models::ChannelMode::Linear,
            channel_number: None,
            target_client_id: None,
            directive: None,
            rules: serde_json::json!({}),
            is_preset: false,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        assert_eq!(content_kind(&ch), "episode");
    }

    #[test]
    fn library_scope_reads_rules_array_and_defaults_to_none() {
        let mk = |rules: serde_json::Value| Channel {
            id: 1,
            account_id: None,
            name: "x".to_string(),
            kind: crate::models::ChannelKind::Preset,
            mode: crate::models::ChannelMode::Linear,
            channel_number: None,
            target_client_id: None,
            directive: None,
            rules,
            is_preset: false,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };

        assert_eq!(library_scope(&mk(serde_json::json!({}))), None);
        assert_eq!(library_scope(&mk(serde_json::json!({"library_ids": []}))), None);
        assert_eq!(
            library_scope(&mk(serde_json::json!({"library_ids": [3, 7]}))),
            Some(vec![3, 7])
        );
    }

    #[test]
    fn content_subtitle_is_the_exact_s_e_shape_xmltv_parses() {
        let item = ScheduleItem::Episode(ep(1, 2, 4));
        assert_eq!(item.subtitle(), Some("S2E4".to_string()));
        assert_eq!(item.description(), Some("Ep 4".to_string()));
    }

    // --- live-DB test (MUSE-28) ------------------------------------------
    //
    // Gated on MUSE_TEST_DATABASE_URL per the crate-wide convention (see
    // src/integration_tests.rs) — skips cleanly (does not fail) when unset.
    // Seeds one library/show/episode/interstitial and a linear channel
    // scoped to that library (via rules.library_ids) so this test is safe
    // to run concurrently with any other live-DB test sharing the same
    // database: the round-robin content pool can never pick up unrelated
    // rows from another test's fixtures.
    #[tokio::test]
    async fn ensure_rolling_window_fills_contiguously_and_is_idempotent() {
        let Ok(database_url) = std::env::var("MUSE_TEST_DATABASE_URL") else {
            eprintln!(
                "MUSE_TEST_DATABASE_URL not set — skipping \
                 ensure_rolling_window_fills_contiguously_and_is_idempotent \
                 (this is expected in the default test run; the crate does not require a live DB)"
            );
            return;
        };

        use sqlx::postgres::PgPoolOptions;
        use uuid::Uuid;

        use crate::models::channel::{ChannelKind, ChannelMode, NewChannel};
        use crate::models::episode::NewEpisode;
        use crate::models::library::{LibraryKind, NewLibrary};
        use crate::models::media_item::NewMediaItem;
        use crate::models::media_metadata::{MediaKind, NewMediaMetadata};
        use crate::models::season::NewSeason;

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

        let library = repo::library::create(
            &pool,
            &NewLibrary {
                name: format!("muse28_tuner_{suffix}"),
                kind: LibraryKind::Tv,
                root_folder: "/media/TV/".to_string(),
                source_arr_name: Some("sonarr".to_string()),
                source_arr_url: None,
            },
        )
        .await
        .expect("create library");

        let show_metadata = repo::media_metadata::upsert_by_tvdb(
            &pool,
            &NewMediaMetadata {
                kind: MediaKind::Show,
                tmdb_id: None,
                tvdb_id: Some(format!("tvdb-muse28-{suffix}")),
                imdb_id: None,
                provider_ids: serde_json::json!({}),
                title: format!("Muse28 Test Show {suffix}"),
                sort_title: None,
                original_title: None,
                original_language: Some("en".to_string()),
                status: Some("continuing".to_string()),
                overview: None,
                studio: None,
                network: None,
                runtime_minutes: Some(22),
                year: Some(1994),
                images: serde_json::json!([]),
            },
        )
        .await
        .expect("upsert show media_metadata");

        let show_item = repo::media_item::upsert(
            &pool,
            &NewMediaItem {
                library_id: library.id,
                media_metadata_id: show_metadata.id,
                path: format!("/media/TV/Muse28 Test Show {suffix}"),
                monitored: true,
                quality_profile_id: None,
                minimum_availability: None,
                plex_rating_key: None,
                added_at: None,
            },
        )
        .await
        .expect("upsert show media_item");

        let season = repo::season::upsert(
            &pool,
            &NewSeason {
                media_item_id: show_item.id,
                season_number: 1,
                title: None,
                overview: None,
                monitored: true,
                air_date: None,
            },
        )
        .await
        .expect("upsert season");

        // Two episodes with a file present — enough content for the
        // round-robin to place multiple items without exhausting instantly,
        // while relying on wraparound (rewatch) to fill the whole window.
        for n in 1..=2 {
            let new_ep = NewEpisode {
                season_id: season.id,
                media_item_id: show_item.id,
                episode_number: n,
                absolute_episode_number: Some(n),
                title: Some(format!("Ep {n}")),
                overview: None,
                air_date: None,
                air_date_utc: None,
                runtime_minutes: Some(22),
                monitored: true,
                tvdb_id: None,
            };
            let created = repo::episode::upsert(&pool, &new_ep).await.expect("upsert episode");
            // upsert doesn't accept has_file directly — flip it after,
            // mirroring how the tracker would after a real scan.
            repo::episode::set_has_file(&pool, created.id, true)
                .await
                .expect("mark episode has_file");
        }

        let interstitial = repo::interstitial::upsert(
            &pool,
            &crate::models::interstitial::NewInterstitial {
                plex_rating_key: Some(format!("plex-bumper-muse28-{suffix}")),
                kind: InterstitialKind::Bumper,
                title: Some("Muse28 Test Bumper".to_string()),
                decade: None,
                theme: None,
                genre: None,
                mood: None,
                duration_ms: Some(10_000),
                tags: vec![],
                source: Some("test".to_string()),
            },
        )
        .await
        .expect("upsert interstitial");
        assert!(interstitial.duration_ms.is_some());

        let channel = repo::channel::create_channel(
            &pool,
            &NewChannel {
                account_id: None,
                name: format!("Muse28 Tuner Test {suffix}"),
                kind: ChannelKind::Preset,
                mode: ChannelMode::Linear,
                channel_number: None,
                target_client_id: None,
                directive: None,
                rules: serde_json::json!({
                    "library_ids": [library.id],
                    "interstitial_every": 1,
                }),
                is_preset: false,
            },
        )
        .await
        .expect("create linear channel");

        // A short window keeps the test fast while still exercising
        // multiple content+interstitial insertions.
        let window = Duration::minutes(90);

        let inserted_first = ensure_rolling_window(&pool, &channel, window)
            .await
            .expect("fill rolling window");
        assert!(inserted_first > 0, "expected the director to schedule something");

        let now = Utc::now();
        let programs = repo::channel::list_programs_in_window(&pool, channel.id, now - Duration::minutes(1), now + window + Duration::hours(1))
            .await
            .expect("list scheduled programs");
        assert_eq!(programs.len(), inserted_first);

        // Contiguity + no overlap: sorted by start_at, each row's end_at
        // equals the next row's start_at exactly.
        let mut sorted = programs.clone();
        sorted.sort_by_key(|p| p.start_at);
        for pair in sorted.windows(2) {
            assert_eq!(
                pair[0].end_at, pair[1].start_at,
                "grid must be gap-free and non-overlapping"
            );
        }
        // Every content row scheduled here is either the fixture
        // interstitial or an episode of the fixture show (library-scoped
        // rules keep other concurrent tests' rows out of this channel).
        for p in &sorted {
            match p.item_type {
                crate::models::ChannelProgramItemType::Interstitial => {
                    assert_eq!(p.interstitial_id, Some(interstitial.id));
                }
                crate::models::ChannelProgramItemType::Episode => {
                    assert!(p.episode_id.is_some());
                }
                crate::models::ChannelProgramItemType::Movie => {
                    panic!("channel rules.content_kind defaults to episode; no movie rows expected");
                }
            }
        }

        // Idempotency: re-running with the SAME window is now a no-op
        // (the grid already reaches target_end).
        let inserted_second = ensure_rolling_window(&pool, &channel, window)
            .await
            .expect("re-run fill rolling window");
        assert_eq!(inserted_second, 0, "re-running an already-full window must not duplicate rows");

        // Extending the window extends the grid forward with no gap at the
        // seam between the old end and the newly-added rows.
        let bigger_window = Duration::minutes(150);
        let inserted_third = ensure_rolling_window(&pool, &channel, bigger_window)
            .await
            .expect("extend rolling window");
        assert!(inserted_third > 0, "extending the window should schedule more rows");

        let extended = repo::channel::list_programs_in_window(
            &pool,
            channel.id,
            now - Duration::minutes(1),
            now + bigger_window + Duration::hours(1),
        )
        .await
        .expect("list extended programs");
        let mut extended_sorted = extended.clone();
        extended_sorted.sort_by_key(|p| p.start_at);
        for pair in extended_sorted.windows(2) {
            assert_eq!(
                pair[0].end_at, pair[1].start_at,
                "extended grid must still be gap-free and non-overlapping"
            );
        }

        // The lineup/xmltv-facing surfaces reflect the seeded channel +
        // schedule.
        let linear_channels = repo::channel::list_linear_channels(&pool)
            .await
            .expect("list linear channels");
        assert!(linear_channels.iter().any(|c| c.id == channel.id));

        let guide_number = crate::tuner::hdhr::guide_number(&channel);
        assert!(!guide_number.is_empty());
    }
}
