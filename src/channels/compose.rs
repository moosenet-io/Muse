//! The channel composer (MUSE-24, spec §4d-C) — the agentic director.
//!
//! [`compose_channel_run`] is the entry point: given a `channels` row id and
//! [`ComposeOptions`] (which shows, whose taste, how long, what
//! interstitial cadence/theme), it produces an ordered lineup — a fresh
//! `channel_runs` row plus its ordered `channel_programs` rows forming a
//! contiguous timeline (`end_at[i] == start_at[i+1]`) — and persists both.
//!
//! ## The deterministic core (no LLM required)
//! Each show's *candidate queue* is its remaining unwatched episodes in
//! narrative order (season, then episode number); "unwatched" is computed
//! from `play_sessions.is_finished` for the requesting account (or, with no
//! account, every episode is a candidate). The composer round-robins across
//! the shows in *show-priority order* (see below), taking one episode per
//! show per round, until either the session-length budget is exhausted or
//! every show's queue is empty. An interstitial is inserted after every
//! `interstitial_every_n_items` content items, drawn from a prefetched pool
//! filtered by kind/decade/theme and rotated to avoid picking the same
//! interstitial twice in a row (when the pool has more than one candidate).
//!
//! Show-priority order is either the caller's own show list
//! (`EpisodeOrdering::NextUnwatched`) or a taste-ranked order
//! (`EpisodeOrdering::TasteRanked`, sorted by the account's `ratings` for
//! each show, highest first, unrated shows last, ties preserving input
//! order) — episodes *within* a show always stay in narrative order either
//! way; taste only decides which show gets visited earlier/more often.
//!
//! ## The optional LLM enhancement
//! When `opts.use_llm` and a Chord URL is configured, the composer asks the
//! local model (via Chord's OpenAI-compatible `/v1/chat/completions`) to
//! propose an alternate show-priority permutation plus a human rationale.
//! The response is validated to be an exact permutation of the input show
//! ids; on ANY failure (unreachable, non-success, malformed JSON, or an
//! invalid permutation) the deterministic show order and a templated
//! rationale are used instead — composition itself never fails because the
//! LLM is down.
//!
//! Divergence from the spec's conceptual sketch: `channel_runs.schedule` is
//! persisted as `{"rationale": "...", "items": [...]}` rather than a bare
//! array, so the overall director narrative has a stable home alongside the
//! per-item entries (each `channel_programs` row also gets its own short
//! per-item `rationale`, per that table's column comment).

use std::collections::{HashMap, HashSet, VecDeque};
use std::time::Duration as StdDuration;

use chrono::{DateTime, Duration as ChronoDuration, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value as Json};
use sqlx::PgPool;

use crate::error::{MuseError, MuseResult};
use crate::models::channel::{
    Channel, ChannelProgramItemType, ChannelRun, NewChannelProgram, NewChannelRun,
};
use crate::models::episode::Episode;
use crate::models::interstitial::InterstitialKind;
use crate::repo;

/// A 22-minute sitcom-style default when an episode has no `runtime_minutes`
/// on file — better than a zero-duration timeline entry.
const DEFAULT_EPISODE_DURATION_MS: i64 = 22 * 60_000;
/// A 30-second default for an interstitial with no `duration_ms` on file.
const DEFAULT_INTERSTITIAL_DURATION_MS: i64 = 30_000;
/// Chord chat model, overridable per-call via `ComposeOptions::llm_model`.
/// Deliberately generic (not a literal fleet model name) — Chord resolves
/// aliases per its own routing config.
const DEFAULT_LLM_MODEL: &str = "default";
const LLM_REQUEST_TIMEOUT: StdDuration = StdDuration::from_secs(20);

// --- public options --------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EpisodeOrdering {
    /// Round-robin the shows in the order given; each show's next
    /// unwatched episode, in narrative order.
    NextUnwatched,
    /// Round-robin the shows ordered by the account's rating (highest
    /// first, unrated last); each show's next unwatched episode, in
    /// narrative order.
    TasteRanked,
}

/// Everything the composer needs to build one lineup. Construct via
/// `ComposeOptions { show_media_item_ids: vec![...], ..Default::default() }`
/// or start from a [`super::presets::Preset::apply`] baseline.
#[derive(Debug, Clone)]
pub struct ComposeOptions {
    /// The requesting account, for "next unwatched" resolution and
    /// taste-ranked show ordering. `None` composes as if nothing has been
    /// watched (every episode is a candidate) and taste-ranking degrades to
    /// the input show order.
    pub account_id: Option<i64>,
    /// The shows (media_items.id, TV `library_kind = 'tv'` rows) this
    /// channel round-robins across. Required — composition errors on an
    /// empty list.
    pub show_media_item_ids: Vec<i64>,
    pub ordering: EpisodeOrdering,
    /// The session-length bound in milliseconds. Composition stops adding
    /// items once cumulative duration reaches this bound (or every show is
    /// exhausted, whichever comes first). Must be positive.
    pub target_session_ms: i64,
    /// Insert one interstitial after every N content items (clamped to a
    /// minimum of 1).
    pub interstitial_every_n_items: u32,
    pub interstitial_kind: Option<InterstitialKind>,
    pub interstitial_decade: Option<i32>,
    pub interstitial_theme: Option<String>,
    /// The lineup's first item starts here; every subsequent item starts
    /// exactly where the previous one ended (a contiguous timeline).
    pub start_at: DateTime<Utc>,
    /// Attempt the optional LLM enhancement. Ignored (treated as `false`)
    /// when no Chord URL is configured.
    pub use_llm: bool,
    /// Chord chat-completions model name; defaults to `"default"` when
    /// unset.
    pub llm_model: Option<String>,
}

impl Default for ComposeOptions {
    fn default() -> Self {
        Self {
            account_id: None,
            show_media_item_ids: Vec::new(),
            ordering: EpisodeOrdering::NextUnwatched,
            target_session_ms: 2 * 3_600_000,
            interstitial_every_n_items: 1,
            interstitial_kind: None,
            interstitial_decade: None,
            interstitial_theme: None,
            start_at: Utc::now(),
            use_llm: false,
            llm_model: None,
        }
    }
}

// --- internal candidate/lineup types ---------------------------------------

#[derive(Debug, Clone, PartialEq)]
struct CandidateEpisode {
    episode_id: i64,
    media_item_id: i64,
    title: String,
    subtitle: Option<String>,
    duration_ms: i64,
}

#[derive(Debug, Clone, PartialEq)]
struct CandidateInterstitial {
    id: i64,
    kind: InterstitialKind,
    title: String,
    duration_ms: i64,
}

#[derive(Debug, Clone, PartialEq)]
struct LineupEntry {
    item_type: ChannelProgramItemType,
    media_item_id: Option<i64>,
    episode_id: Option<i64>,
    interstitial_id: Option<i64>,
    title: String,
    subtitle: Option<String>,
    duration_ms: i64,
    rationale: String,
}

impl LineupEntry {
    fn from_episode(ep: &CandidateEpisode, round: usize) -> Self {
        LineupEntry {
            item_type: ChannelProgramItemType::Episode,
            media_item_id: Some(ep.media_item_id),
            episode_id: Some(ep.episode_id),
            interstitial_id: None,
            title: ep.title.clone(),
            subtitle: ep.subtitle.clone(),
            duration_ms: ep.duration_ms,
            rationale: format!(
                "Round {}: next unwatched episode for show {}",
                round + 1,
                ep.media_item_id
            ),
        }
    }

    fn from_interstitial(i: &CandidateInterstitial, cadence: u32) -> Self {
        LineupEntry {
            item_type: ChannelProgramItemType::Interstitial,
            media_item_id: None,
            episode_id: None,
            interstitial_id: Some(i.id),
            title: i.title.clone(),
            subtitle: None,
            duration_ms: i.duration_ms,
            rationale: format!("Cadence interstitial ({:?}) after {cadence} item(s)", i.kind),
        }
    }
}

struct TimedEntry {
    entry: LineupEntry,
    start_at: DateTime<Utc>,
    end_at: DateTime<Utc>,
}

impl TimedEntry {
    fn to_json(&self) -> Json {
        json!({
            "type": match self.entry.item_type {
                ChannelProgramItemType::Episode => "episode",
                ChannelProgramItemType::Movie => "movie",
                ChannelProgramItemType::Interstitial => "interstitial",
            },
            "media_item_id": self.entry.media_item_id,
            "episode_id": self.entry.episode_id,
            "interstitial_id": self.entry.interstitial_id,
            "title": self.entry.title,
            "subtitle": self.entry.subtitle,
            "duration_ms": self.entry.duration_ms,
            "start_at": self.start_at.to_rfc3339(),
            "end_at": self.end_at.to_rfc3339(),
            "rationale": self.entry.rationale,
        })
    }
}

// --- the pure, DB-free composition core -------------------------------------

/// Round-robin `show_order` against each show's candidate queue, inserting a
/// cadence-matched interstitial (rotated, avoiding an immediate repeat) after
/// every `opts.interstitial_every_n_items` content items, stopping once
/// `opts.target_session_ms` is reached or every queue is exhausted.
///
/// Pure and DB-free by design — this is what the unit tests below exercise
/// directly with in-memory fixtures.
fn build_lineup(
    show_order: &[i64],
    mut queues: HashMap<i64, VecDeque<CandidateEpisode>>,
    interstitial_pool: &[CandidateInterstitial],
    opts: &ComposeOptions,
) -> Vec<LineupEntry> {
    let mut out = Vec::new();
    if show_order.is_empty() {
        return out;
    }

    let cadence = opts.interstitial_every_n_items.max(1);
    let mut elapsed_ms: i64 = 0;
    let mut content_since_interstitial: u32 = 0;
    let mut last_interstitial_id: Option<i64> = None;
    let mut interstitial_cursor: usize = 0;
    let mut round = 0usize;

    'outer: loop {
        let mut added_this_round = false;
        for show_id in show_order {
            if elapsed_ms >= opts.target_session_ms {
                break 'outer;
            }
            let Some(ep) = queues.get_mut(show_id).and_then(VecDeque::pop_front) else {
                continue;
            };
            added_this_round = true;

            out.push(LineupEntry::from_episode(&ep, round));
            elapsed_ms += ep.duration_ms;
            content_since_interstitial += 1;

            if content_since_interstitial >= cadence {
                if let Some(pick) = pick_interstitial(
                    interstitial_pool,
                    &mut interstitial_cursor,
                    last_interstitial_id,
                ) {
                    out.push(LineupEntry::from_interstitial(pick, cadence));
                    elapsed_ms += pick.duration_ms;
                    last_interstitial_id = Some(pick.id);
                }
                content_since_interstitial = 0;
            }
        }
        if !added_this_round {
            break;
        }
        round += 1;
    }

    out
}

/// Rotate through `pool` starting at `*cursor`, skipping the entry matching
/// `last_id` when the pool has more than one candidate (so back-to-back
/// interstitials never repeat unless the pool is a singleton). Advances
/// `*cursor` to just past the picked entry. Returns `None` for an empty pool.
fn pick_interstitial<'a>(
    pool: &'a [CandidateInterstitial],
    cursor: &mut usize,
    last_id: Option<i64>,
) -> Option<&'a CandidateInterstitial> {
    let n = pool.len();
    if n == 0 {
        return None;
    }
    for step in 0..n {
        let idx = (*cursor + step) % n;
        let candidate = &pool[idx];
        if n == 1 || Some(candidate.id) != last_id {
            *cursor = (idx + 1) % n;
            return Some(candidate);
        }
    }
    // Every candidate matches last_id (n == 1 already handled above, so this
    // is unreachable in practice, but stay total rather than panic).
    *cursor = (*cursor + 1) % n;
    pool.first()
}

/// Stamp a contiguous timeline onto an ordered lineup: item 0 starts at
/// `start_at`; every subsequent item starts exactly where the previous one
/// ended.
fn assign_timeline(entries: Vec<LineupEntry>, start_at: DateTime<Utc>) -> Vec<TimedEntry> {
    let mut cursor = start_at;
    entries
        .into_iter()
        .map(|entry| {
            let item_start = cursor;
            let item_end = item_start + ChronoDuration::milliseconds(entry.duration_ms);
            cursor = item_end;
            TimedEntry {
                entry,
                start_at: item_start,
                end_at: item_end,
            }
        })
        .collect()
}

/// Stable sort of `show_ids` by `ratings` (highest first); unrated shows
/// sort after all rated ones, and ties (including "both unrated") preserve
/// the input's relative order — pure so it's unit-testable without a DB.
fn rank_shows_by_rating(show_ids: &[i64], ratings: &HashMap<i64, f32>) -> Vec<i64> {
    let mut ranked = show_ids.to_vec();
    ranked.sort_by(|a, b| {
        match (ratings.get(a), ratings.get(b)) {
            (Some(x), Some(y)) => y.partial_cmp(x).unwrap_or(std::cmp::Ordering::Equal),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => std::cmp::Ordering::Equal,
        }
    });
    ranked
}

fn templated_rationale(show_order: &[i64], opts: &ComposeOptions, timed: &[TimedEntry]) -> String {
    let content_count = timed
        .iter()
        .filter(|t| !matches!(t.entry.item_type, ChannelProgramItemType::Interstitial))
        .count();
    let interstitial_count = timed.len() - content_count;
    let mode = match opts.ordering {
        EpisodeOrdering::NextUnwatched => "next-unwatched",
        EpisodeOrdering::TasteRanked => "taste-ranked",
    };
    let minutes = opts.target_session_ms / 60_000;
    format!(
        "Composed a {mode} round-robin lineup across {} show(s): {content_count} episode(s) \
         and {interstitial_count} interstitial(s), targeting a {minutes}-minute session.",
        show_order.len(),
    )
}

// --- LLM enhancement ---------------------------------------------------------

#[derive(Debug, Deserialize)]
struct ChatCompletionResponse {
    #[serde(default)]
    choices: Vec<ChatChoice>,
}

#[derive(Debug, Deserialize)]
struct ChatChoice {
    message: ChatMessage,
}

#[derive(Debug, Deserialize)]
struct ChatMessage {
    content: String,
}

#[derive(Debug, Deserialize)]
struct LlmDecision {
    show_order: Vec<i64>,
    rationale: String,
}

struct LlmEnhancement {
    show_order: Vec<i64>,
    rationale: String,
}

fn build_llm_prompt(channel: &Channel, show_order: &[i64], opts: &ComposeOptions) -> String {
    format!(
        "Channel \"{}\" (directive: {}). Candidate shows in default priority order: {:?}. \
         Session target: {} minutes. Interstitial theme: {:?}, decade: {:?}, every {} item(s). \
         Propose a show-priority order (a permutation of the exact same show ids — do not add, \
         drop, or invent ids) for round-robin scheduling, and a one-paragraph rationale.",
        channel.name,
        channel.directive.as_deref().unwrap_or("(none)"),
        show_order,
        opts.target_session_ms / 60_000,
        opts.interstitial_theme,
        opts.interstitial_decade,
        opts.interstitial_every_n_items.max(1),
    )
}

/// Ask Chord's chat-completions endpoint to propose an alternate
/// show-priority order + rationale. Returns `None` on ANY failure
/// (unconfigured, unreachable, non-success, malformed JSON, or a proposed
/// order that isn't an exact permutation of `show_order`) — the caller falls
/// back to the deterministic order + a templated rationale in every such
/// case. This function never propagates an error.
async fn llm_enhance(
    chord_url: Option<&str>,
    channel: &Channel,
    show_order: &[i64],
    opts: &ComposeOptions,
) -> Option<LlmEnhancement> {
    let base_url = chord_url?;

    let client = match reqwest::Client::builder().timeout(LLM_REQUEST_TIMEOUT).build() {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(error = %e, "channel composer: failed to build LLM http client; falling back");
            return None;
        }
    };

    let model = opts.llm_model.as_deref().unwrap_or(DEFAULT_LLM_MODEL);
    let prompt = build_llm_prompt(channel, show_order, opts);
    let body = json!({
        "model": model,
        "temperature": 0.4,
        "messages": [
            {
                "role": "system",
                "content": "You are Muse's pseudo-TV channel director. Respond with ONLY a \
                    JSON object of the exact shape {\"show_order\": [<show ids, a permutation \
                    of the given set>], \"rationale\": \"<one short paragraph>\"} and nothing \
                    else.",
            },
            { "role": "user", "content": prompt },
        ],
    });

    let url = format!("{}/v1/chat/completions", base_url.trim_end_matches('/'));

    let resp = match client.post(&url).json(&body).send().await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(error = %e, url = %url, "channel composer: chord LLM request failed; falling back to deterministic ordering");
            return None;
        }
    };

    if !resp.status().is_success() {
        tracing::warn!(status = %resp.status(), "channel composer: chord LLM returned non-success; falling back");
        return None;
    }

    let parsed: ChatCompletionResponse = match resp.json().await {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(error = %e, "channel composer: chord LLM response body malformed; falling back");
            return None;
        }
    };

    let Some(content) = parsed.choices.first().map(|c| c.message.content.clone()) else {
        tracing::warn!("channel composer: chord LLM response had no choices; falling back");
        return None;
    };

    let decision: LlmDecision = match serde_json::from_str(&content) {
        Ok(d) => d,
        Err(e) => {
            tracing::warn!(error = %e, "channel composer: chord LLM content wasn't the expected JSON shape; falling back");
            return None;
        }
    };

    let given: HashSet<i64> = show_order.iter().copied().collect();
    let proposed: HashSet<i64> = decision.show_order.iter().copied().collect();
    if given != proposed || decision.show_order.len() != show_order.len() {
        tracing::warn!(
            "channel composer: chord LLM show_order isn't a permutation of the candidate set; falling back"
        );
        return None;
    }

    Some(LlmEnhancement {
        show_order: decision.show_order,
        rationale: decision.rationale,
    })
}

// --- DB-backed candidate gathering -------------------------------------------

/// Every remaining (not `is_finished` for `account_id`, when given)
/// candidate episode for `show_id`, in narrative (season, then episode
/// number) order.
async fn candidate_episodes(
    pool: &PgPool,
    account_id: Option<i64>,
    show_id: i64,
) -> MuseResult<VecDeque<CandidateEpisode>> {
    let seasons = repo::season::list_by_media_item(pool, show_id).await?;

    let mut all: Vec<(Episode, i32)> = Vec::new();
    for season in &seasons {
        let eps = repo::episode::list_by_season(pool, season.id).await?;
        for ep in eps {
            all.push((ep, season.season_number));
        }
    }

    let finished: HashSet<i64> = match account_id {
        Some(account_id) if !all.is_empty() => {
            let ids: Vec<i64> = all.iter().map(|(ep, _)| ep.id).collect();
            sqlx::query_scalar::<_, i64>(
                "SELECT DISTINCT episode_id FROM play_sessions \
                 WHERE account_id = $1 AND episode_id = ANY($2) AND is_finished = true",
            )
            .bind(account_id)
            .bind(&ids)
            .fetch_all(pool)
            .await
            .map_err(MuseError::Database)?
            .into_iter()
            .collect()
        }
        _ => HashSet::new(),
    };

    let queue = all
        .into_iter()
        .filter(|(ep, _)| !finished.contains(&ep.id))
        .map(|(ep, season_number)| CandidateEpisode {
            episode_id: ep.id,
            media_item_id: ep.media_item_id,
            title: ep
                .title
                .clone()
                .unwrap_or_else(|| format!("Episode {}", ep.episode_number)),
            subtitle: Some(format!("S{season_number}E{}", ep.episode_number)),
            duration_ms: ep
                .runtime_minutes
                .map(|m| i64::from(m) * 60_000)
                .unwrap_or(DEFAULT_EPISODE_DURATION_MS),
        })
        .collect::<VecDeque<_>>();

    Ok(queue)
}

async fn candidate_interstitials(
    pool: &PgPool,
    opts: &ComposeOptions,
) -> MuseResult<Vec<CandidateInterstitial>> {
    let rows = repo::interstitial::list_by_kind_decade_theme(
        pool,
        opts.interstitial_kind,
        opts.interstitial_decade,
        opts.interstitial_theme.as_deref(),
    )
    .await?;

    Ok(rows
        .into_iter()
        .map(|i| CandidateInterstitial {
            id: i.id,
            kind: i.kind,
            title: i.title.clone().unwrap_or_else(|| format!("{:?}", i.kind)),
            duration_ms: i.duration_ms.unwrap_or(DEFAULT_INTERSTITIAL_DURATION_MS),
        })
        .collect())
}

async fn taste_ranked_show_order(
    pool: &PgPool,
    account_id: Option<i64>,
    show_ids: &[i64],
) -> MuseResult<Vec<i64>> {
    let Some(account_id) = account_id else {
        return Ok(show_ids.to_vec());
    };

    let ratings = repo::watch_stats::list_ratings_for_account(pool, account_id).await?;
    let rating_map: HashMap<i64, f32> = ratings
        .into_iter()
        .filter_map(|r| r.rating.map(|v| (r.media_item_id, v)))
        .collect();

    Ok(rank_shows_by_rating(show_ids, &rating_map))
}

// --- entry points -------------------------------------------------------------

/// Compose (and persist) a fresh ordered lineup for `channel_id`: a new
/// `channel_runs` row plus its ordered `channel_programs` rows forming a
/// contiguous timeline. Never mutates a prior run — composing is
/// intentionally generative, so the previous run (if any) is left intact as
/// history.
///
/// `chord_url` is `Config::chord_url` (or `None` to force the deterministic
/// path regardless of `opts.use_llm`). Errors only on a structurally invalid
/// request (no shows, non-positive session length) or a database failure —
/// never because the LLM enhancement is unavailable.
pub async fn compose_channel_run(
    pool: &PgPool,
    chord_url: Option<&str>,
    channel_id: i64,
    opts: &ComposeOptions,
) -> MuseResult<ChannelRun> {
    if opts.show_media_item_ids.is_empty() {
        return Err(MuseError::Config(
            "channel composer requires at least one show in show_media_item_ids".to_string(),
        ));
    }
    if opts.target_session_ms <= 0 {
        return Err(MuseError::Config(
            "channel composer requires a positive target_session_ms".to_string(),
        ));
    }

    let channel = repo::channel::get_channel(pool, channel_id).await?;

    let mut queues: HashMap<i64, VecDeque<CandidateEpisode>> = HashMap::new();
    for &show_id in &opts.show_media_item_ids {
        queues.insert(show_id, candidate_episodes(pool, opts.account_id, show_id).await?);
    }
    let interstitial_pool = candidate_interstitials(pool, opts).await?;

    let mut show_order = match opts.ordering {
        EpisodeOrdering::NextUnwatched => opts.show_media_item_ids.clone(),
        EpisodeOrdering::TasteRanked => {
            taste_ranked_show_order(pool, opts.account_id, &opts.show_media_item_ids).await?
        }
    };

    let mut llm_rationale: Option<String> = None;
    if opts.use_llm {
        if let Some(enhancement) = llm_enhance(chord_url, &channel, &show_order, opts).await {
            show_order = enhancement.show_order;
            llm_rationale = Some(enhancement.rationale);
        }
    }

    let entries = build_lineup(&show_order, queues, &interstitial_pool, opts);
    let mut timed = assign_timeline(entries, opts.start_at);

    let rationale = llm_rationale.unwrap_or_else(|| templated_rationale(&show_order, opts, &timed));

    // The overall director narrative rides on the first item's per-row
    // rationale (spec: "emit a programming schedule with rationale"); later
    // rows keep their own short structural note.
    if let Some(first) = timed.first_mut() {
        first.entry.rationale = rationale.clone();
    }

    let total_duration_ms: i64 = timed.iter().map(|t| t.entry.duration_ms).sum();
    let schedule_json = json!({
        "rationale": rationale,
        "items": timed.iter().map(TimedEntry::to_json).collect::<Vec<_>>(),
    });

    let run = repo::channel::create_run(
        pool,
        &NewChannelRun {
            channel_id: Some(channel_id),
            account_id: opts.account_id,
            target_client_id: None,
            plex_play_queue_id: None,
            schedule: schedule_json,
            total_duration_ms: Some(total_duration_ms),
        },
    )
    .await?;

    // `channel_programs` has `UNIQUE (channel_id, start_at)`; this
    // composer's own contiguous-timeline construction (`assign_timeline`)
    // never produces duplicate `start_at`s within a single call, so each
    // insert below is expected to succeed — a real collision here would
    // mean a genuine concurrent-compose race for the same channel, which
    // surfaces as a normal `MuseError::Database` (not swallowed).
    for timed_entry in &timed {
        let entry = &timed_entry.entry;
        let new_program = NewChannelProgram {
            channel_id,
            item_type: entry.item_type,
            media_item_id: entry.media_item_id,
            episode_id: entry.episode_id,
            interstitial_id: entry.interstitial_id,
            title: entry.title.clone(),
            subtitle: entry.subtitle.clone(),
            description: None,
            artwork_url: None,
            start_at: timed_entry.start_at,
            end_at: timed_entry.end_at,
            duration_ms: entry.duration_ms,
            rationale: Some(entry.rationale.clone()),
        };
        repo::channel::create_program(pool, &new_program).await?;
    }

    Ok(run)
}

/// Alias for [`compose_channel_run`] naming the "regenerate" operation from
/// spec §4d-C ("On-demand, regenerable, adjustable"). Always inserts a new
/// `channel_runs` row; the prior run is left as history.
pub async fn regenerate_channel_run(
    pool: &PgPool,
    chord_url: Option<&str>,
    channel_id: i64,
    opts: &ComposeOptions,
) -> MuseResult<ChannelRun> {
    compose_channel_run(pool, chord_url, channel_id, opts).await
}

/// "Adjust and recompose" (spec §4d-C: e.g. "more music videos," "swap the
/// drama for a comedy") — apply a caller-supplied tweak to a base set of
/// options, then compose a fresh run from the tweaked options. Like
/// [`compose_channel_run`], always inserts a new run rather than mutating
/// the base run.
pub async fn adjust_channel_run(
    pool: &PgPool,
    chord_url: Option<&str>,
    channel_id: i64,
    mut opts: ComposeOptions,
    adjust: impl FnOnce(&mut ComposeOptions),
) -> MuseResult<ChannelRun> {
    adjust(&mut opts);
    compose_channel_run(pool, chord_url, channel_id, &opts).await
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use httpmock::prelude::*;

    fn ep(episode_id: i64, media_item_id: i64, duration_ms: i64) -> CandidateEpisode {
        CandidateEpisode {
            episode_id,
            media_item_id,
            title: format!("Episode {episode_id}"),
            subtitle: Some(format!("S1E{episode_id}")),
            duration_ms,
        }
    }

    fn interstitial(id: i64, kind: InterstitialKind, duration_ms: i64) -> CandidateInterstitial {
        CandidateInterstitial {
            id,
            kind,
            title: format!("Interstitial {id}"),
            duration_ms,
        }
    }

    fn base_opts() -> ComposeOptions {
        ComposeOptions {
            show_media_item_ids: vec![1, 2],
            target_session_ms: 10 * 60_000, // 10 minutes
            interstitial_every_n_items: 1,
            ..Default::default()
        }
    }

    // --- build_lineup: round-robin fairness ---------------------------------

    #[test]
    fn round_robin_alternates_shows_evenly() {
        let mut queues = HashMap::new();
        queues.insert(1, VecDeque::from([ep(101, 1, 60_000), ep(102, 1, 60_000)]));
        queues.insert(2, VecDeque::from([ep(201, 2, 60_000), ep(202, 2, 60_000)]));

        let opts = ComposeOptions {
            show_media_item_ids: vec![1, 2],
            target_session_ms: 1_000_000, // effectively unbounded for this fixture
            interstitial_every_n_items: 100, // no interstitials for this test
            ..Default::default()
        };

        let entries = build_lineup(&[1, 2], queues, &[], &opts);
        let show_sequence: Vec<i64> = entries.iter().filter_map(|e| e.media_item_id).collect();
        assert_eq!(show_sequence, vec![1, 2, 1, 2], "must alternate strictly between shows");
    }

    #[test]
    fn round_robin_skips_exhausted_show_without_stalling() {
        let mut queues = HashMap::new();
        queues.insert(1, VecDeque::from([ep(101, 1, 60_000)])); // only 1 episode
        queues.insert(2, VecDeque::from([ep(201, 2, 60_000), ep(202, 2, 60_000)]));

        let opts = ComposeOptions {
            show_media_item_ids: vec![1, 2],
            target_session_ms: 1_000_000,
            interstitial_every_n_items: 100,
            ..Default::default()
        };

        let entries = build_lineup(&[1, 2], queues, &[], &opts);
        let show_sequence: Vec<i64> = entries.iter().filter_map(|e| e.media_item_id).collect();
        assert_eq!(show_sequence, vec![1, 2, 2], "show 1 exhausts after round 1, show 2 keeps going");
    }

    #[test]
    fn empty_show_order_produces_empty_lineup() {
        let entries = build_lineup(&[], HashMap::new(), &[], &base_opts());
        assert!(entries.is_empty());
    }

    // --- build_lineup: interstitial cadence ---------------------------------

    #[test]
    fn interstitial_inserted_every_n_items() {
        let mut queues = HashMap::new();
        queues.insert(
            1,
            VecDeque::from([
                ep(101, 1, 60_000),
                ep(102, 1, 60_000),
                ep(103, 1, 60_000),
                ep(104, 1, 60_000),
            ]),
        );
        let pool = vec![interstitial(1, InterstitialKind::Bumper, 10_000)];

        let opts = ComposeOptions {
            show_media_item_ids: vec![1],
            target_session_ms: 1_000_000,
            interstitial_every_n_items: 2,
            ..Default::default()
        };

        let entries = build_lineup(&[1], queues, &pool, &opts);
        let kinds: Vec<ChannelProgramItemType> = entries.iter().map(|e| e.item_type).collect();
        assert_eq!(
            kinds,
            vec![
                ChannelProgramItemType::Episode,
                ChannelProgramItemType::Episode,
                ChannelProgramItemType::Interstitial,
                ChannelProgramItemType::Episode,
                ChannelProgramItemType::Episode,
                ChannelProgramItemType::Interstitial,
            ]
        );
    }

    #[test]
    fn interstitial_cadence_zero_is_clamped_to_one() {
        let mut queues = HashMap::new();
        queues.insert(1, VecDeque::from([ep(101, 1, 60_000), ep(102, 1, 60_000)]));
        let pool = vec![interstitial(1, InterstitialKind::Bumper, 10_000)];

        let opts = ComposeOptions {
            show_media_item_ids: vec![1],
            target_session_ms: 1_000_000,
            interstitial_every_n_items: 0, // must clamp to 1, not divide-by-zero or loop forever
            ..Default::default()
        };

        let entries = build_lineup(&[1], queues, &pool, &opts);
        let kinds: Vec<ChannelProgramItemType> = entries.iter().map(|e| e.item_type).collect();
        assert_eq!(
            kinds,
            vec![
                ChannelProgramItemType::Episode,
                ChannelProgramItemType::Interstitial,
                ChannelProgramItemType::Episode,
                ChannelProgramItemType::Interstitial,
            ]
        );
    }

    #[test]
    fn no_interstitial_pool_means_no_interstitials_inserted() {
        let mut queues = HashMap::new();
        queues.insert(1, VecDeque::from([ep(101, 1, 60_000), ep(102, 1, 60_000)]));

        let opts = ComposeOptions {
            show_media_item_ids: vec![1],
            target_session_ms: 1_000_000,
            interstitial_every_n_items: 1,
            ..Default::default()
        };

        let entries = build_lineup(&[1], queues, &[], &opts);
        assert!(entries
            .iter()
            .all(|e| e.item_type != ChannelProgramItemType::Interstitial));
        assert_eq!(entries.len(), 2, "content still gets scheduled even with no interstitial pool");
    }

    #[test]
    fn pick_interstitial_avoids_immediate_repeat_with_multiple_candidates() {
        let pool = vec![
            interstitial(1, InterstitialKind::Bumper, 10_000),
            interstitial(2, InterstitialKind::Bumper, 10_000),
        ];
        let mut cursor = 0;
        let first = pick_interstitial(&pool, &mut cursor, None).unwrap();
        let second = pick_interstitial(&pool, &mut cursor, Some(first.id)).unwrap();
        assert_ne!(first.id, second.id, "must not repeat immediately when >1 candidate exists");
    }

    #[test]
    fn pick_interstitial_singleton_pool_repeats_by_necessity() {
        let pool = vec![interstitial(1, InterstitialKind::Bumper, 10_000)];
        let mut cursor = 0;
        let first = pick_interstitial(&pool, &mut cursor, None).unwrap();
        let second = pick_interstitial(&pool, &mut cursor, Some(first.id)).unwrap();
        assert_eq!(first.id, second.id, "a singleton pool has no alternative to repeat");
    }

    #[test]
    fn pick_interstitial_empty_pool_returns_none() {
        let mut cursor = 0;
        assert!(pick_interstitial(&[], &mut cursor, None).is_none());
    }

    // --- build_lineup: session-length bound ---------------------------------

    #[test]
    fn session_length_bound_stops_scheduling_once_reached() {
        let mut queues = HashMap::new();
        queues.insert(
            1,
            VecDeque::from(
                (0..20)
                    .map(|i| ep(100 + i, 1, 5 * 60_000)) // 5 min each
                    .collect::<VecDeque<_>>(),
            ),
        );

        let opts = ComposeOptions {
            show_media_item_ids: vec![1],
            target_session_ms: 22 * 60_000, // ~22 minutes -> should fit ~5 episodes then stop
            interstitial_every_n_items: 1000, // no interstitials, isolate the bound
            ..Default::default()
        };

        let entries = build_lineup(&[1], queues, &[], &opts);
        let total: i64 = entries.iter().map(|e| e.duration_ms).sum();
        assert!(total >= opts.target_session_ms, "must reach the target, not stop short");
        // Never wildly overshoot: at most one extra item's worth past target.
        assert!(
            total < opts.target_session_ms + 5 * 60_000,
            "must not keep scheduling long past the session bound"
        );
        assert!(entries.len() < 20, "must have stopped before exhausting the queue");
    }

    #[test]
    fn session_length_bound_of_zero_is_rejected_by_compose_not_build_lineup() {
        // build_lineup itself has no opinion on <=0 targets (it just never
        // enters the loop body usefully); compose_channel_run is where this
        // is validated and rejected — see `session_length_must_be_positive`.
        let mut queues = HashMap::new();
        queues.insert(1, VecDeque::from([ep(101, 1, 60_000)]));
        let opts = ComposeOptions {
            show_media_item_ids: vec![1],
            target_session_ms: 0,
            ..Default::default()
        };
        let entries = build_lineup(&[1], queues, &[], &opts);
        assert!(entries.is_empty(), "a zero budget schedules nothing");
    }

    // --- assign_timeline: contiguous timeline -------------------------------

    #[test]
    fn assign_timeline_produces_contiguous_timeline() {
        let start = DateTime::parse_from_rfc3339("2026-07-12T10:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let entries = vec![
            LineupEntry::from_episode(&ep(101, 1, 60_000), 0),
            LineupEntry::from_episode(&ep(102, 1, 90_000), 0),
            LineupEntry::from_interstitial(&interstitial(1, InterstitialKind::Bumper, 15_000), 1),
        ];

        let timed = assign_timeline(entries, start);
        assert_eq!(timed[0].start_at, start);
        for w in timed.windows(2) {
            assert_eq!(w[0].end_at, w[1].start_at, "end of item i must equal start of item i+1");
        }
        assert_eq!(timed.last().unwrap().end_at, start + ChronoDuration::milliseconds(60_000 + 90_000 + 15_000));
    }

    #[test]
    fn assign_timeline_empty_input_is_empty_output() {
        let timed = assign_timeline(vec![], Utc::now());
        assert!(timed.is_empty());
    }

    // --- taste-ranked show ordering (pure) ----------------------------------

    #[test]
    fn rank_shows_by_rating_orders_highest_first() {
        let mut ratings = HashMap::new();
        ratings.insert(1, 6.0);
        ratings.insert(2, 9.0);
        ratings.insert(3, 3.0);
        let ranked = rank_shows_by_rating(&[1, 2, 3], &ratings);
        assert_eq!(ranked, vec![2, 1, 3]);
    }

    #[test]
    fn rank_shows_by_rating_unrated_sort_last_preserving_order() {
        let mut ratings = HashMap::new();
        ratings.insert(2, 5.0);
        // show 1 and show 3 are both unrated
        let ranked = rank_shows_by_rating(&[1, 2, 3], &ratings);
        assert_eq!(ranked, vec![2, 1, 3], "unrated shows keep their relative input order, after the rated one");
    }

    #[test]
    fn rank_shows_by_rating_no_ratings_preserves_input_order() {
        let ranked = rank_shows_by_rating(&[3, 1, 2], &HashMap::new());
        assert_eq!(ranked, vec![3, 1, 2]);
    }

    // --- templated rationale --------------------------------------------------

    #[test]
    fn templated_rationale_mentions_counts_and_session_length() {
        let start = Utc::now();
        let entries = vec![
            LineupEntry::from_episode(&ep(101, 1, 60_000), 0),
            LineupEntry::from_interstitial(&interstitial(1, InterstitialKind::Bumper, 10_000), 1),
        ];
        let timed = assign_timeline(entries, start);
        let opts = ComposeOptions {
            target_session_ms: 30 * 60_000,
            ordering: EpisodeOrdering::TasteRanked,
            ..base_opts()
        };
        let text = templated_rationale(&[1, 2], &opts, &timed);
        assert!(text.contains("taste-ranked"));
        assert!(text.contains("1 episode"));
        assert!(text.contains("1 interstitial"));
        assert!(text.contains("30-minute"));
    }

    // --- LLM enhancement: success + validation + fallback -------------------

    fn test_channel() -> Channel {
        Channel {
            id: 1,
            account_id: None,
            name: "Test Channel".to_string(),
            kind: crate::models::channel::ChannelKind::Preset,
            mode: crate::models::channel::ChannelMode::OnDemand,
            channel_number: None,
            target_client_id: None,
            directive: Some("an ep of each show".to_string()),
            rules: json!({}),
            is_preset: true,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    #[tokio::test]
    async fn llm_enhance_returns_none_when_chord_unconfigured() {
        let channel = test_channel();
        let result = llm_enhance(None, &channel, &[1, 2], &base_opts()).await;
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn llm_enhance_parses_valid_permutation_and_rationale() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(POST).path("/v1/chat/completions");
            then.status(200).header("content-type", "application/json").body(
                json!({
                    "choices": [
                        { "message": { "content": json!({
                            "show_order": [2, 1],
                            "rationale": "Opened with show 2 since it's on a streak, then show 1."
                        }).to_string() } }
                    ]
                })
                .to_string(),
            );
        });

        let channel = test_channel();
        let result = llm_enhance(Some(&server.base_url()), &channel, &[1, 2], &base_opts())
            .await
            .expect("should parse a valid enhancement");

        assert_eq!(result.show_order, vec![2, 1]);
        assert!(result.rationale.contains("streak"));
    }

    #[tokio::test]
    async fn llm_enhance_falls_back_on_invalid_permutation() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(POST).path("/v1/chat/completions");
            then.status(200).header("content-type", "application/json").body(
                json!({
                    "choices": [
                        { "message": { "content": json!({
                            "show_order": [1, 1], // NOT a valid permutation of [1, 2]
                            "rationale": "bogus"
                        }).to_string() } }
                    ]
                })
                .to_string(),
            );
        });

        let channel = test_channel();
        let result = llm_enhance(Some(&server.base_url()), &channel, &[1, 2], &base_opts()).await;
        assert!(result.is_none(), "an invalid permutation must fall back, not be trusted");
    }

    #[tokio::test]
    async fn llm_enhance_falls_back_on_malformed_json_content() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(POST).path("/v1/chat/completions");
            then.status(200).header("content-type", "application/json").body(
                json!({
                    "choices": [ { "message": { "content": "not valid json at all" } } ]
                })
                .to_string(),
            );
        });

        let channel = test_channel();
        let result = llm_enhance(Some(&server.base_url()), &channel, &[1, 2], &base_opts()).await;
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn llm_enhance_falls_back_on_non_success_status() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(POST).path("/v1/chat/completions");
            then.status(500).body("internal error");
        });

        let channel = test_channel();
        let result = llm_enhance(Some(&server.base_url()), &channel, &[1, 2], &base_opts()).await;
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn llm_enhance_falls_back_on_unreachable_host() {
        let channel = test_channel();
        // Loopback with nothing listening — connection refused immediately
        // (unlike an RFC 5737 test-net address, which can hang until the OS
        // routing/ARP timeout instead of failing fast).
        let result = llm_enhance(
            Some("http://127.0.0.1:1"),
            &channel,
            &[1, 2],
            &ComposeOptions {
                llm_model: None,
                ..base_opts()
            },
        )
        .await;
        assert!(result.is_none());
    }
}

/// Live-DB test: seeds a channel + 2 shows (each with 2 episodes) + 1
/// interstitial against a real Postgres, then asserts `compose_channel_run`
/// produces the expected round-robin interleaving with a contiguous
/// timeline. Skips cleanly when `MUSE_TEST_DATABASE_URL` is unset — never
/// requires a live DB for the rest of the suite. Mirrors the pattern in
/// `src/plex_control/repo.rs`.
#[cfg(test)]
mod live_db_tests {
    use super::*;
    use crate::models::channel::{ChannelKind, ChannelMode};
    use crate::models::episode::NewEpisode;
    use crate::models::interstitial::NewInterstitial;
    use crate::models::library::{LibraryKind, NewLibrary};
    use crate::models::media_item::NewMediaItem;
    use crate::models::media_metadata::{MediaKind, NewMediaMetadata};
    use crate::models::season::NewSeason;
    use sqlx::postgres::PgPoolOptions;

    struct Seed {
        library_id: i64,
        show1_media_item_id: i64,
        show2_media_item_id: i64,
        show1_metadata_id: i64,
        show2_metadata_id: i64,
        interstitial_id: i64,
        channel_id: i64,
    }

    async fn seed(pool: &PgPool) -> Seed {
        // Idempotent: drop any leftovers from a previous failed run under
        // the same fixed test identifiers, then insert fresh.
        sqlx::query("DELETE FROM channel_runs WHERE channel_id IN (SELECT id FROM channels WHERE name = 'MUSE-24 test channel')")
            .execute(pool)
            .await
            .ok();
        sqlx::query("DELETE FROM channels WHERE name = 'MUSE-24 test channel'")
            .execute(pool)
            .await
            .ok();
        sqlx::query("DELETE FROM interstitials WHERE plex_rating_key = 'muse24-test-bumper-1'")
            .execute(pool)
            .await
            .ok();
        sqlx::query("DELETE FROM media_items WHERE plex_rating_key IN ('muse24-test-show-1', 'muse24-test-show-2')")
            .execute(pool)
            .await
            .ok();
        sqlx::query("DELETE FROM media_metadata WHERE tvdb_id IN ('muse24-test-tvdb-1', 'muse24-test-tvdb-2')")
            .execute(pool)
            .await
            .ok();
        sqlx::query("DELETE FROM libraries WHERE name = 'muse24-test-tv'")
            .execute(pool)
            .await
            .ok();

        let library = repo::library::create(
            pool,
            &NewLibrary {
                name: "muse24-test-tv".to_string(),
                kind: LibraryKind::Tv,
                root_folder: "/test/tv".to_string(),
                source_arr_name: None,
                source_arr_url: None,
            },
        )
        .await
        .expect("create library");

        let mut show_ids = Vec::new();
        let mut metadata_ids = Vec::new();
        for (n, tvdb) in [(1, "muse24-test-tvdb-1"), (2, "muse24-test-tvdb-2")] {
            let metadata = repo::media_metadata::upsert_by_tvdb(
                pool,
                &NewMediaMetadata {
                    kind: MediaKind::Show,
                    tmdb_id: None,
                    tvdb_id: Some(tvdb.to_string()),
                    imdb_id: None,
                    provider_ids: json!({}),
                    title: format!("MUSE-24 Test Show {n}"),
                    sort_title: None,
                    original_title: None,
                    original_language: None,
                    status: None,
                    overview: None,
                    studio: None,
                    network: None,
                    runtime_minutes: Some(20),
                    year: Some(2020),
                    images: json!([]),
                },
            )
            .await
            .expect("create media_metadata");
            metadata_ids.push(metadata.id);

            let media_item = repo::media_item::upsert(
                pool,
                &NewMediaItem {
                    library_id: library.id,
                    media_metadata_id: metadata.id,
                    path: format!("/test/tv/show-{n}"),
                    monitored: true,
                    quality_profile_id: None,
                    minimum_availability: None,
                    plex_rating_key: Some(format!("muse24-test-show-{n}")),
                    added_at: None,
                },
            )
            .await
            .expect("create media_item");
            show_ids.push(media_item.id);

            let season = repo::season::upsert(
                pool,
                &NewSeason {
                    media_item_id: media_item.id,
                    season_number: 1,
                    title: None,
                    overview: None,
                    monitored: true,
                    air_date: None,
                },
            )
            .await
            .expect("create season");

            for episode_number in 1..=2 {
                repo::episode::upsert(
                    pool,
                    &NewEpisode {
                        season_id: season.id,
                        media_item_id: media_item.id,
                        episode_number,
                        absolute_episode_number: None,
                        title: Some(format!("Show {n} Episode {episode_number}")),
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
            }
        }

        let interstitial = repo::interstitial::upsert(
            pool,
            &NewInterstitial {
                plex_rating_key: Some("muse24-test-bumper-1".to_string()),
                kind: InterstitialKind::Bumper,
                title: Some("Test Bumper".to_string()),
                decade: None,
                theme: None,
                genre: None,
                mood: None,
                duration_ms: Some(15_000),
                tags: vec![],
                source: Some("user".to_string()),
            },
        )
        .await
        .expect("create interstitial");

        let channel = repo::channel::create_channel(
            pool,
            &crate::models::channel::NewChannel {
                account_id: None,
                name: "MUSE-24 test channel".to_string(),
                kind: ChannelKind::Personal,
                mode: ChannelMode::OnDemand,
                channel_number: None,
                target_client_id: None,
                directive: Some("an ep of each test show".to_string()),
                rules: json!({}),
                is_preset: false,
            },
        )
        .await
        .expect("create channel");

        Seed {
            library_id: library.id,
            show1_media_item_id: show_ids[0],
            show2_media_item_id: show_ids[1],
            show1_metadata_id: metadata_ids[0],
            show2_metadata_id: metadata_ids[1],
            interstitial_id: interstitial.id,
            channel_id: channel.id,
        }
    }

    async fn cleanup(pool: &PgPool, seed: &Seed) {
        sqlx::query("DELETE FROM channel_runs WHERE channel_id = $1")
            .bind(seed.channel_id)
            .execute(pool)
            .await
            .ok();
        sqlx::query("DELETE FROM channels WHERE id = $1")
            .bind(seed.channel_id)
            .execute(pool)
            .await
            .ok();
        sqlx::query("DELETE FROM interstitials WHERE id = $1")
            .bind(seed.interstitial_id)
            .execute(pool)
            .await
            .ok();
        // Cascades to seasons -> episodes.
        sqlx::query("DELETE FROM media_items WHERE id = ANY($1)")
            .bind(vec![seed.show1_media_item_id, seed.show2_media_item_id])
            .execute(pool)
            .await
            .ok();
        sqlx::query("DELETE FROM media_metadata WHERE id = ANY($1)")
            .bind(vec![seed.show1_metadata_id, seed.show2_metadata_id])
            .execute(pool)
            .await
            .ok();
        sqlx::query("DELETE FROM libraries WHERE id = $1")
            .bind(seed.library_id)
            .execute(pool)
            .await
            .ok();
    }

    #[tokio::test]
    async fn compose_channel_run_produces_contiguous_interleaved_lineup() {
        let Ok(database_url) = std::env::var("MUSE_TEST_DATABASE_URL") else {
            eprintln!(
                "skipping compose_channel_run_produces_contiguous_interleaved_lineup: \
                 MUSE_TEST_DATABASE_URL not set"
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

        let seed = seed(&pool).await;

        let start_at = DateTime::parse_from_rfc3339("2026-07-12T18:00:00Z")
            .unwrap()
            .with_timezone(&Utc);

        let opts = ComposeOptions {
            account_id: None,
            show_media_item_ids: vec![seed.show1_media_item_id, seed.show2_media_item_id],
            ordering: EpisodeOrdering::NextUnwatched,
            // 2 episodes/show * 2 shows * 20min + 4 interstitials * 15s,
            // with headroom so the session bound never cuts the lineup
            // short (this test is about interleaving/contiguity, not the
            // session-length bound — that's covered by the unit tests).
            target_session_ms: 10 * 3_600_000,
            interstitial_every_n_items: 1,
            interstitial_kind: Some(InterstitialKind::Bumper),
            interstitial_decade: None,
            interstitial_theme: None,
            start_at,
            use_llm: false,
            llm_model: None,
        };

        let run = compose_channel_run(&pool, None, seed.channel_id, &opts)
            .await
            .expect("compose_channel_run should succeed");

        assert_eq!(run.channel_id, Some(seed.channel_id));
        assert!(run.total_duration_ms.unwrap_or(0) > 0);

        let programs = sqlx::query_as::<_, crate::models::channel::ChannelProgram>(
            "SELECT * FROM channel_programs WHERE channel_id = $1 ORDER BY start_at",
        )
        .bind(seed.channel_id)
        .fetch_all(&pool)
        .await
        .expect("fetch channel_programs");

        // Both shows have exactly 2 unwatched episodes and the interstitial
        // pool has exactly 1 candidate, so the expected exhaustion lineup is
        // ep(show1) -> bumper -> ep(show2) -> bumper -> ep(show1) -> bumper
        // -> ep(show2) -> bumper (8 rows total).
        assert_eq!(programs.len(), 8, "expected 4 episodes + 4 interstitials");

        let expected_types = [
            ChannelProgramItemType::Episode,
            ChannelProgramItemType::Interstitial,
            ChannelProgramItemType::Episode,
            ChannelProgramItemType::Interstitial,
            ChannelProgramItemType::Episode,
            ChannelProgramItemType::Interstitial,
            ChannelProgramItemType::Episode,
            ChannelProgramItemType::Interstitial,
        ];
        let actual_types: Vec<ChannelProgramItemType> =
            programs.iter().map(|p| p.item_type).collect();
        assert_eq!(actual_types, expected_types);

        let expected_shows = [
            seed.show1_media_item_id,
            seed.show2_media_item_id,
            seed.show1_media_item_id,
            seed.show2_media_item_id,
        ];
        let actual_shows: Vec<i64> = programs
            .iter()
            .filter(|p| p.item_type == ChannelProgramItemType::Episode)
            .filter_map(|p| p.media_item_id)
            .collect();
        assert_eq!(actual_shows, expected_shows, "must round-robin strictly between the two shows");

        // Contiguous timeline: item 0 starts at start_at, and every
        // subsequent item starts exactly where the previous one ended.
        assert_eq!(programs[0].start_at, start_at);
        for w in programs.windows(2) {
            assert_eq!(
                w[0].end_at, w[1].start_at,
                "channel_programs must form a contiguous timeline (schema CHECK end_at > start_at \
                 is necessary but not sufficient — this composer additionally guarantees no gaps)"
            );
            assert!(w[0].end_at > w[0].start_at, "each program must satisfy end_at > start_at");
        }

        let total_from_programs: i64 = programs.iter().map(|p| p.duration_ms).sum();
        assert_eq!(run.total_duration_ms, Some(total_from_programs));

        // Composing again must insert a fresh run, leaving the first intact.
        let second_start = programs.last().unwrap().end_at + ChronoDuration::seconds(1);
        let second_opts = ComposeOptions {
            start_at: second_start,
            ..opts
        };
        let second_run = compose_channel_run(&pool, None, seed.channel_id, &second_opts)
            .await
            .expect("second compose_channel_run should also succeed");
        assert_ne!(second_run.id, run.id, "regenerating must create a new run, not overwrite the old one");

        let run_count: i64 = sqlx::query_scalar("SELECT count(*) FROM channel_runs WHERE channel_id = $1")
            .bind(seed.channel_id)
            .fetch_one(&pool)
            .await
            .expect("count runs");
        assert_eq!(run_count, 2, "the prior run must still exist as history");

        cleanup(&pool, &seed).await;
    }
}
