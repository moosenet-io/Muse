//! The channel-guide JSON API + the self-contained EPG-style guide page
//! (MUSE-27, spec §4d-F).
//!
//! `now_marker`/`entity_ref`/the `From<Channel>`/`from_program` shaping below
//! are plain, DB-free functions specifically so the "which program is airing
//! now" logic and the JSON shape are unit-testable without a live Postgres —
//! see the `tests` module at the bottom.

use std::sync::Arc;

use axum::{
    extract::{Path, State},
    response::{Html, IntoResponse},
    Json,
};
use chrono::{DateTime, Duration, Utc};
use serde::Serialize;

use crate::error::MuseResult;
use crate::http::AppState;
use crate::models::channel::{Channel, ChannelKind, ChannelMode, ChannelProgram, ChannelProgramItemType};

/// How far back the lineup window looks (so a viewer tuning in mid-program
/// still sees what just finished airing).
const WINDOW_BEHIND: Duration = Duration::hours(2);
/// How far ahead the lineup window looks (a full day of upcoming programming).
const WINDOW_AHEAD: Duration = Duration::hours(24);

/// Poster/thumb variant requested for every guide cover image. The proxy
/// supports other variants (`?variant=`), but the guide itself always wants
/// the poster.
const COVER_VARIANT: &str = "poster";

#[derive(Debug, Clone, Serialize)]
pub struct ChannelSummary {
    pub id: i64,
    pub name: String,
    pub kind: ChannelKind,
    pub mode: ChannelMode,
    pub channel_number: Option<f32>,
    /// MUSE-27 divergence: the `channels` table (MUSE-23) has no `enabled`/
    /// disabled column yet, so every channel this endpoint returns is
    /// implicitly enabled. Kept as an explicit field so the JSON shape the
    /// spec calls for (§4d-F: "name, kind, mode, channel_number, enabled")
    /// is stable now, and a future disable/archive flag becomes an additive
    /// change rather than a breaking one.
    pub enabled: bool,
}

impl From<Channel> for ChannelSummary {
    fn from(c: Channel) -> Self {
        Self {
            id: c.id,
            name: c.name,
            kind: c.kind,
            mode: c.mode,
            channel_number: c.channel_number,
            enabled: true,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct LineupProgram {
    pub id: i64,
    pub item_type: ChannelProgramItemType,
    pub title: String,
    pub subtitle: Option<String>,
    pub description: Option<String>,
    pub start_at: DateTime<Utc>,
    pub end_at: DateTime<Utc>,
    pub duration_ms: i64,
    pub rationale: Option<String>,
    /// Same-origin artwork-proxy URL (`/art/{kind}/{id}`) — never a direct
    /// Plex URL, so the browser never needs (or sees) the Plex token.
    pub cover_url: String,
    pub is_interstitial: bool,
    /// True for the single program the "now" line points at.
    pub is_now: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct LineupResponse {
    pub channel: ChannelSummary,
    pub now_program_id: Option<i64>,
    pub generated_at: DateTime<Utc>,
    pub window_start: DateTime<Utc>,
    pub window_end: DateTime<Utc>,
    pub programs: Vec<LineupProgram>,
}

/// Map a program onto the artwork-proxy's `(kind, id)` addressing. `None`
/// when the program carries none of `media_item_id`/`episode_id`/
/// `interstitial_id` (shouldn't happen given the table's `CHECK` constraint,
/// but the guide degrades to a placeholder cover rather than erroring if it
/// ever does).
pub fn entity_ref(program: &ChannelProgram) -> Option<(&'static str, i64)> {
    match program.item_type {
        ChannelProgramItemType::Episode => program.episode_id.map(|id| ("episode", id)),
        ChannelProgramItemType::Movie => program.media_item_id.map(|id| ("media_item", id)),
        ChannelProgramItemType::Interstitial => {
            program.interstitial_id.map(|id| ("interstitial", id))
        }
    }
}

fn cover_url_for(program: &ChannelProgram) -> String {
    match entity_ref(program) {
        Some((kind, id)) => format!("/art/{kind}/{id}"),
        // No resolvable entity id — the artwork proxy treats any unknown
        // (kind, id) pair as a cache miss with no source and serves its
        // placeholder image, so this never 404s in the guide.
        None => "/art/unknown/0".to_string(),
    }
}

/// The id of the program airing at `now` (`start_at <= now < end_at`), if
/// any. Pure and DB-free by design — see the unit tests below.
pub fn now_marker(programs: &[ChannelProgram], now: DateTime<Utc>) -> Option<i64> {
    programs
        .iter()
        .find(|p| p.start_at <= now && now < p.end_at)
        .map(|p| p.id)
}

fn to_lineup_program(program: &ChannelProgram, now_id: Option<i64>) -> LineupProgram {
    LineupProgram {
        id: program.id,
        item_type: program.item_type,
        title: program.title.clone(),
        subtitle: program.subtitle.clone(),
        description: program.description.clone(),
        start_at: program.start_at,
        end_at: program.end_at,
        duration_ms: program.duration_ms,
        rationale: program.rationale.clone(),
        cover_url: cover_url_for(program),
        is_interstitial: matches!(program.item_type, ChannelProgramItemType::Interstitial),
        is_now: now_id == Some(program.id),
    }
}

/// `GET /api/channels` — basic metadata for the guide's channel rail.
pub async fn list_channels_handler(
    State(state): State<Arc<AppState>>,
) -> MuseResult<Json<Vec<ChannelSummary>>> {
    let channels = crate::repo::channel::list_channels(&state.pool, None).await?;
    Ok(Json(channels.into_iter().map(ChannelSummary::from).collect()))
}

/// `GET /api/channels/{id}/lineup` — the current/upcoming lineup for a
/// channel, with a "now" marker computed at request time (a real axum
/// handler may use wall-clock time; it's workflow-composer scripts that
/// can't). A channel with no scheduled programs in the window returns an
/// empty `programs` array, not an error.
pub async fn lineup_handler(
    State(state): State<Arc<AppState>>,
    Path(channel_id): Path<i64>,
) -> MuseResult<Json<LineupResponse>> {
    let channel = crate::repo::channel::get_channel(&state.pool, channel_id).await?;

    let now = Utc::now();
    let window_start = now - WINDOW_BEHIND;
    let window_end = now + WINDOW_AHEAD;

    let programs = crate::repo::channel::list_programs_in_window(
        &state.pool,
        channel_id,
        window_start,
        window_end,
    )
    .await?;

    // Best-effort: register each program's upstream artwork source so a
    // later `/art/{kind}/{id}` request has something to fetch from. Never
    // fails the lineup response — a registration failure just means that
    // program's cover falls back to the placeholder until it succeeds on a
    // future render.
    for program in &programs {
        if let (Some(source_url), Some((kind, id))) =
            (program.artwork_url.as_deref(), entity_ref(program))
        {
            if let Err(e) = crate::repo::artwork_cache::upsert_source(
                &state.pool,
                kind,
                id,
                COVER_VARIANT,
                source_url,
            )
            .await
            {
                tracing::warn!(
                    error = %e,
                    channel_id,
                    program_id = program.id,
                    "failed to register artwork source; cover art may fall back to placeholder"
                );
            }
        }
    }

    let now_id = now_marker(&programs, now);
    let lineup_programs = programs
        .iter()
        .map(|p| to_lineup_program(p, now_id))
        .collect();

    Ok(Json(LineupResponse {
        channel: ChannelSummary::from(channel),
        now_program_id: now_id,
        generated_at: now,
        window_start,
        window_end,
        programs: lineup_programs,
    }))
}

/// `GET /` and `/guide` — a self-contained EPG-style grid/timeline page.
/// Inline CSS + JS only, no external CDN/font/script dependencies (it must
/// render offline against a local Muse instance). Consumes
/// `/api/channels`/`/api/channels/{id}/lineup` client-side.
pub async fn guide_page() -> impl IntoResponse {
    Html(GUIDE_HTML)
}

const GUIDE_HTML: &str = r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Muse — Channel Guide</title>
<style>
  :root {
    color-scheme: dark;
    --bg: #0d1016;
    --panel: #161b24;
    --border: #262c38;
    --text: #e6e9ef;
    --muted: #8a92a3;
    --accent: #6fa8ff;
    --interstitial: #3a3020;
    --now-line: #ff5a5a;
  }
  * { box-sizing: border-box; }
  body {
    margin: 0;
    background: var(--bg);
    color: var(--text);
    font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, sans-serif;
  }
  header {
    padding: 16px 20px;
    border-bottom: 1px solid var(--border);
  }
  header h1 { margin: 0; font-size: 1.25rem; }
  header p { margin: 4px 0 0; color: var(--muted); font-size: 0.85rem; }
  #guide {
    position: relative;
    padding: 12px;
  }
  .channel-row {
    display: grid;
    grid-template-columns: 160px 1fr;
    gap: 10px;
    margin-bottom: 10px;
  }
  .channel-label {
    background: var(--panel);
    border: 1px solid var(--border);
    border-radius: 6px;
    padding: 8px 10px;
    display: flex;
    flex-direction: column;
    justify-content: center;
  }
  .channel-label .name { font-weight: 600; font-size: 0.9rem; }
  .channel-label .meta { color: var(--muted); font-size: 0.72rem; margin-top: 2px; }
  .timeline {
    position: relative;
    background: var(--panel);
    border: 1px solid var(--border);
    border-radius: 6px;
    height: 74px;
    overflow: hidden;
  }
  .program {
    position: absolute;
    top: 4px;
    bottom: 4px;
    border-radius: 4px;
    background: #202836;
    border: 1px solid var(--border);
    padding: 4px 6px;
    overflow: hidden;
    white-space: nowrap;
    text-overflow: ellipsis;
    font-size: 0.72rem;
    display: flex;
    gap: 6px;
    align-items: center;
  }
  .program.now { outline: 2px solid var(--now-line); }
  .program.interstitial {
    background: var(--interstitial);
    font-style: italic;
  }
  .program img {
    height: 100%;
    width: auto;
    max-width: 40px;
    object-fit: cover;
    border-radius: 3px;
    flex-shrink: 0;
  }
  .program .info { overflow: hidden; }
  .program .title { overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
  .program .time { color: var(--muted); font-size: 0.65rem; }
  .now-line {
    position: absolute;
    top: 0;
    bottom: 0;
    width: 2px;
    background: var(--now-line);
    z-index: 5;
    pointer-events: none;
  }
  #empty {
    color: var(--muted);
    padding: 24px;
    text-align: center;
  }
</style>
</head>
<body>
<header>
  <h1>Muse — Channel Guide</h1>
  <p>Live EPG-style lineup, refreshed every 60s. Hover a program for its rationale.</p>
</header>
<div id="guide"><div id="empty">Loading channels…</div></div>
<script>
(function () {
  var GUIDE = document.getElementById('guide');
  var EMPTY = document.getElementById('empty');

  function fmtTime(iso) {
    var d = new Date(iso);
    return d.toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' });
  }

  async function fetchJSON(url) {
    var res = await fetch(url, { headers: { Accept: 'application/json' } });
    if (!res.ok) throw new Error(url + ': HTTP ' + res.status);
    return res.json();
  }

  function renderChannel(channel, lineup) {
    var row = document.createElement('div');
    row.className = 'channel-row';

    var label = document.createElement('div');
    label.className = 'channel-label';
    label.innerHTML =
      '<div class="name">' + escapeHtml(channel.name) + '</div>' +
      '<div class="meta">' + escapeHtml(channel.kind) + ' · ' + escapeHtml(channel.mode) +
      (channel.channel_number ? ' · ch ' + channel.channel_number : '') + '</div>';
    row.appendChild(label);

    var timeline = document.createElement('div');
    timeline.className = 'timeline';

    var winStart = new Date(lineup.window_start).getTime();
    var winEnd = new Date(lineup.window_end).getTime();
    var winSpan = Math.max(1, winEnd - winStart);

    if (!lineup.programs.length) {
      var placeholder = document.createElement('div');
      placeholder.className = 'program';
      placeholder.style.left = '0';
      placeholder.style.width = '100%';
      placeholder.textContent = 'Nothing scheduled';
      timeline.appendChild(placeholder);
    }

    lineup.programs.forEach(function (p) {
      var start = new Date(p.start_at).getTime();
      var end = new Date(p.end_at).getTime();
      var leftPct = Math.max(0, (start - winStart) / winSpan * 100);
      var widthPct = Math.max(1, (end - start) / winSpan * 100);

      var el = document.createElement('div');
      el.className = 'program' + (p.is_interstitial ? ' interstitial' : '') + (p.is_now ? ' now' : '');
      el.style.left = leftPct + '%';
      el.style.width = widthPct + '%';
      var rationale = p.rationale ? p.rationale : '';
      el.title = p.title + (p.subtitle ? ' — ' + p.subtitle : '') +
        '\n' + fmtTime(p.start_at) + '–' + fmtTime(p.end_at) +
        (rationale ? '\n' + rationale : '');

      var img = document.createElement('img');
      img.src = p.cover_url;
      img.alt = '';
      img.loading = 'lazy';
      el.appendChild(img);

      var info = document.createElement('div');
      info.className = 'info';
      info.innerHTML =
        '<div class="title">' + escapeHtml(p.title) + (p.subtitle ? ' — ' + escapeHtml(p.subtitle) : '') + '</div>' +
        '<div class="time">' + fmtTime(p.start_at) + '–' + fmtTime(p.end_at) + '</div>';
      el.appendChild(info);

      timeline.appendChild(el);
    });

    var nowPct = Math.min(100, Math.max(0, (Date.now() - winStart) / winSpan * 100));
    var nowLine = document.createElement('div');
    nowLine.className = 'now-line';
    nowLine.style.left = nowPct + '%';
    timeline.appendChild(nowLine);

    row.appendChild(timeline);
    GUIDE.appendChild(row);
  }

  function escapeHtml(s) {
    return String(s == null ? '' : s).replace(/[&<>"']/g, function (c) {
      return { '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' }[c];
    });
  }

  async function render() {
    try {
      var channels = await fetchJSON('/api/channels');
      GUIDE.innerHTML = '';
      if (!channels.length) {
        var empty = document.createElement('div');
        empty.id = 'empty';
        empty.textContent = 'No channels yet.';
        GUIDE.appendChild(empty);
        return;
      }
      for (var i = 0; i < channels.length; i++) {
        var channel = channels[i];
        try {
          var lineup = await fetchJSON('/api/channels/' + channel.id + '/lineup');
          renderChannel(channel, lineup);
        } catch (e) {
          console.error('lineup failed for channel', channel.id, e);
        }
      }
    } catch (e) {
      GUIDE.innerHTML = '';
      var err = document.createElement('div');
      err.id = 'empty';
      err.textContent = 'Failed to load guide: ' + e.message;
      GUIDE.appendChild(err);
    }
  }

  render();
  setInterval(render, 60000);
})();
</script>
</body>
</html>
"#;

#[cfg(test)]
mod tests {
    use super::*;

    fn program(
        id: i64,
        item_type: ChannelProgramItemType,
        start_offset_min: i64,
        duration_min: i64,
    ) -> ChannelProgram {
        let start_at = Utc::now() + Duration::minutes(start_offset_min);
        ChannelProgram {
            id,
            channel_id: 1,
            item_type,
            media_item_id: matches!(item_type, ChannelProgramItemType::Movie).then_some(100),
            episode_id: matches!(item_type, ChannelProgramItemType::Episode).then_some(200),
            interstitial_id: matches!(item_type, ChannelProgramItemType::Interstitial)
                .then_some(300),
            title: format!("Program {id}"),
            subtitle: None,
            description: None,
            artwork_url: Some(format!("/library/metadata/{id}/thumb/1")),
            start_at,
            end_at: start_at + Duration::minutes(duration_min),
            duration_ms: duration_min * 60_000,
            rationale: Some("because it fit the slot".to_string()),
            play_event_id: None,
            created_at: Utc::now(),
        }
    }

    #[test]
    fn now_marker_finds_the_currently_airing_program() {
        let now = Utc::now();
        let earlier = ChannelProgram {
            start_at: now - Duration::minutes(30),
            end_at: now - Duration::minutes(1),
            ..program(1, ChannelProgramItemType::Episode, -30, 29)
        };
        let current = ChannelProgram {
            start_at: now - Duration::minutes(5),
            end_at: now + Duration::minutes(25),
            ..program(2, ChannelProgramItemType::Episode, -5, 30)
        };
        let later = ChannelProgram {
            start_at: now + Duration::minutes(30),
            end_at: now + Duration::minutes(60),
            ..program(3, ChannelProgramItemType::Episode, 30, 30)
        };

        let programs = vec![earlier, current, later];
        assert_eq!(now_marker(&programs, now), Some(2));
    }

    #[test]
    fn now_marker_returns_none_when_nothing_is_airing() {
        let now = Utc::now();
        let past = ChannelProgram {
            start_at: now - Duration::hours(2),
            end_at: now - Duration::hours(1),
            ..program(1, ChannelProgramItemType::Episode, -120, 60)
        };
        let future = ChannelProgram {
            start_at: now + Duration::hours(1),
            end_at: now + Duration::hours(2),
            ..program(2, ChannelProgramItemType::Episode, 60, 60)
        };

        assert_eq!(now_marker(&[past, future], now), None);
    }

    #[test]
    fn now_marker_returns_none_for_empty_schedule() {
        assert_eq!(now_marker(&[], Utc::now()), None);
    }

    #[test]
    fn entity_ref_maps_each_item_type_to_its_own_id() {
        let episode = program(1, ChannelProgramItemType::Episode, 0, 30);
        let movie = program(2, ChannelProgramItemType::Movie, 0, 30);
        let interstitial = program(3, ChannelProgramItemType::Interstitial, 0, 2);

        assert_eq!(entity_ref(&episode), Some(("episode", 200)));
        assert_eq!(entity_ref(&movie), Some(("media_item", 100)));
        assert_eq!(entity_ref(&interstitial), Some(("interstitial", 300)));
    }

    #[test]
    fn entity_ref_is_none_when_the_matching_id_column_is_missing() {
        let mut episode = program(1, ChannelProgramItemType::Episode, 0, 30);
        episode.episode_id = None;
        assert_eq!(entity_ref(&episode), None);
    }

    #[test]
    fn cover_url_falls_back_to_placeholder_when_no_entity_id() {
        let mut episode = program(1, ChannelProgramItemType::Episode, 0, 30);
        episode.episode_id = None;
        assert_eq!(cover_url_for(&episode), "/art/unknown/0");
    }

    #[test]
    fn to_lineup_program_marks_only_the_current_program() {
        let current = program(1, ChannelProgramItemType::Episode, -5, 30);
        let other = program(2, ChannelProgramItemType::Movie, 60, 90);

        let now_id = Some(1i64);
        let a = to_lineup_program(&current, now_id);
        let b = to_lineup_program(&other, now_id);

        assert!(a.is_now);
        assert!(!b.is_now);
        assert_eq!(a.cover_url, "/art/episode/200");
        assert_eq!(b.cover_url, "/art/media_item/100");
        assert!(!a.is_interstitial);
    }

    #[test]
    fn to_lineup_program_flags_interstitials() {
        let bumper = program(9, ChannelProgramItemType::Interstitial, 0, 1);
        let lp = to_lineup_program(&bumper, None);
        assert!(lp.is_interstitial);
        assert!(!lp.is_now);
    }

    /// Exercises `/api/channels` and `/api/channels/{id}/lineup` against a
    /// real Postgres if `MUSE_TEST_DATABASE_URL` is set; skips cleanly
    /// otherwise so the suite never requires a live DB. Seeds an
    /// interstitial + a channel + a channel_programs row that overlaps
    /// "now", then calls the handlers directly (not through the full HTTP
    /// stack — same posture as the rest of this crate's repo-level live-DB
    /// tests) and asserts the seeded data round-trips through the JSON
    /// shape, including the "now" marker.
    #[tokio::test]
    async fn lineup_and_channels_endpoints_reflect_seeded_data() {
        use sqlx::postgres::PgPoolOptions;

        let Ok(database_url) = std::env::var("MUSE_TEST_DATABASE_URL") else {
            eprintln!(
                "skipping lineup_and_channels_endpoints_reflect_seeded_data: MUSE_TEST_DATABASE_URL not set"
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

        let interstitial = crate::repo::interstitial::upsert(
            &pool,
            &crate::models::interstitial::NewInterstitial {
                plex_rating_key: Some("muse27-test-bumper".to_string()),
                kind: crate::models::interstitial::InterstitialKind::Bumper,
                title: Some("Test Bumper".to_string()),
                decade: Some(1990),
                theme: Some("saturday_morning".to_string()),
                genre: None,
                mood: None,
                duration_ms: Some(30_000),
                tags: vec![],
                source: Some("user".to_string()),
            },
        )
        .await
        .expect("seed interstitial");

        let channel = crate::repo::channel::create_channel(
            &pool,
            &crate::models::channel::NewChannel {
                account_id: None,
                name: "MUSE-27 Test Channel".to_string(),
                kind: ChannelKind::Preset,
                mode: ChannelMode::Linear,
                channel_number: None,
                target_client_id: None,
                directive: Some("test".to_string()),
                rules: serde_json::json!({}),
                is_preset: false,
            },
        )
        .await
        .expect("seed channel");

        let now = Utc::now();
        let program = crate::repo::channel::create_program(
            &pool,
            &crate::models::channel::NewChannelProgram {
                channel_id: channel.id,
                item_type: ChannelProgramItemType::Interstitial,
                media_item_id: None,
                episode_id: None,
                interstitial_id: Some(interstitial.id),
                title: "Test Bumper".to_string(),
                subtitle: None,
                description: None,
                artwork_url: Some("/library/metadata/9999/thumb/1".to_string()),
                start_at: now - Duration::minutes(1),
                end_at: now + Duration::minutes(1),
                duration_ms: 120_000,
                rationale: Some("seeded for MUSE-27 live-db test".to_string()),
            },
        )
        .await
        .expect("seed program");

        let config = crate::config::Config::default();
        let state = Arc::new(AppState {
            pool: pool.clone(),
            config: config.clone(),
            plex: None,
            prowlarr: None,
            arr_instances: Vec::new(),
            enrichment: crate::enrichment::EnrichmentService::from_config(&config),
            tmdb: None,
            embed: None,
        });

        let channels = list_channels_handler(State(state.clone()))
            .await
            .expect("list_channels_handler should succeed")
            .0;
        assert!(
            channels.iter().any(|c| c.id == channel.id && c.enabled),
            "seeded channel should appear in /api/channels"
        );

        let lineup = lineup_handler(State(state.clone()), Path(channel.id))
            .await
            .expect("lineup_handler should succeed")
            .0;
        assert_eq!(lineup.channel.id, channel.id);
        assert_eq!(lineup.now_program_id, Some(program.id));
        assert_eq!(lineup.programs.len(), 1);
        let lp = &lineup.programs[0];
        assert_eq!(lp.id, program.id);
        assert!(lp.is_now);
        assert!(lp.is_interstitial);
        assert_eq!(lp.cover_url, format!("/art/interstitial/{}", interstitial.id));

        // A channel that exists but has no programs in-window degrades to an
        // empty lineup, not an error.
        let empty_channel = crate::repo::channel::create_channel(
            &pool,
            &crate::models::channel::NewChannel {
                account_id: None,
                name: "MUSE-27 Empty Channel".to_string(),
                kind: ChannelKind::Preset,
                mode: ChannelMode::Linear,
                channel_number: None,
                target_client_id: None,
                directive: None,
                rules: serde_json::json!({}),
                is_preset: false,
            },
        )
        .await
        .expect("seed empty channel");
        let empty_lineup = lineup_handler(State(state.clone()), Path(empty_channel.id))
            .await
            .expect("lineup_handler should succeed for an empty channel")
            .0;
        assert!(empty_lineup.programs.is_empty());
        assert!(empty_lineup.now_program_id.is_none());

        // Cleanup.
        sqlx::query("DELETE FROM channel_programs WHERE channel_id = $1")
            .bind(channel.id)
            .execute(&pool)
            .await
            .ok();
        sqlx::query("DELETE FROM channels WHERE id = ANY($1)")
            .bind(vec![channel.id, empty_channel.id])
            .execute(&pool)
            .await
            .ok();
        sqlx::query("DELETE FROM interstitials WHERE id = $1")
            .bind(interstitial.id)
            .execute(&pool)
            .await
            .ok();
    }

    #[test]
    fn channel_summary_reports_enabled_true_for_any_channel() {
        let channel = Channel {
            id: 1,
            account_id: None,
            name: "Test".to_string(),
            kind: ChannelKind::Preset,
            mode: ChannelMode::Linear,
            channel_number: Some(101.1),
            target_client_id: None,
            directive: None,
            rules: serde_json::json!({}),
            is_preset: true,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        let summary = ChannelSummary::from(channel);
        assert!(summary.enabled);
        assert_eq!(summary.channel_number, Some(101.1));
    }
}
