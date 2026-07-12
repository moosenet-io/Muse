//! (C) Session reconstruction — spec §4-C.
//!
//! [`fold_events`] is a pure, in-memory fold: given every known
//! `play_events` row for one `session_key`, it always sorts them by
//! `(received_at, id)` before folding, so the result is a deterministic
//! function of the *set* of events, not the order they happen to be
//! passed in or the order they were originally inserted/observed. That is
//! what makes reconstruction both **idempotent** (re-running it over an
//! unchanged event set reproduces the same `play_sessions` row) and
//! **late-event tolerant** (a delayed webhook/poll delivery that shows up
//! after later events were already processed just changes the fold's
//! outcome the next time it runs — it is never "too late" to be folded in
//! correctly).
//!
//! [`reconstruct_and_persist`] is the thin, DB-touching wrapper: it loads
//! every event for a session_key, folds them, resolves the raw
//! `account_ref`/`rating_key` strings to local ids, and upserts
//! `play_sessions` (keyed by the table's `(account_id, media_item_id,
//! episode_id, started_at)` UNIQUE — `started_at` is always the *first*
//! event's `received_at`, which is stable across re-folds, so repeated
//! calls update the same row rather than inserting duplicates).

use chrono::{DateTime, Datelike, Timelike, Utc};
use ipnetwork::IpNetwork;
use sqlx::PgPool;

use crate::error::MuseResult;
use crate::models::account::NewAccount;
use crate::models::play_event::PlayEvent;
use crate::models::play_session::{NewPlaySession, PlaySession};
use crate::repo;

/// Plex fires `media.scrobble` at ~90% watched; a session at or above this
/// fraction of its runtime is finished even without an explicit scrobble.
pub const COMPLETE_THRESHOLD: f32 = 0.90;
/// A session stopped below this fraction of its runtime (and never
/// finished) is a strong negative taste signal.
pub const ABANDON_THRESHOLD: f32 = 0.15;

/// The pure fold result for one session_key's event stream.
#[derive(Debug, Clone, PartialEq)]
pub struct Fold {
    pub started_at: DateTime<Utc>,
    pub stopped_at: Option<DateTime<Utc>>,
    pub duration_ms: Option<i64>,
    pub watched_ms: i64,
    pub view_offset_ms: Option<i64>,
    pub percent_complete: Option<f32>,
    pub paused_counter: i32,
    pub paused_ms: i64,
    pub is_finished: bool,
    pub is_abandoned: bool,
    pub account_ref: Option<String>,
    pub rating_key: Option<String>,
    pub player: Option<String>,
    pub platform: Option<String>,
    pub product: Option<String>,
    pub device: Option<String>,
    pub ip_address: Option<IpNetwork>,
}

/// Fold every known event for a session into aggregates. Returns `None`
/// only for an empty slice (nothing to reconstruct).
pub fn fold_events(events: &[PlayEvent]) -> Option<Fold> {
    if events.is_empty() {
        return None;
    }

    let mut sorted: Vec<&PlayEvent> = events.iter().collect();
    sorted.sort_by(|a, b| a.received_at.cmp(&b.received_at).then(a.id.cmp(&b.id)));

    let first = sorted[0];
    let mut f = Fold {
        started_at: first.received_at,
        stopped_at: None,
        duration_ms: None,
        watched_ms: 0,
        view_offset_ms: None,
        percent_complete: None,
        paused_counter: 0,
        paused_ms: 0,
        is_finished: false,
        is_abandoned: false,
        account_ref: None,
        rating_key: None,
        player: None,
        platform: None,
        product: None,
        device: None,
        ip_address: None,
    };

    let mut playing = false;
    let mut last_offset: Option<i64> = None;
    let mut last_ts: DateTime<Utc> = first.received_at;
    let mut pause_started_at: Option<DateTime<Utc>> = None;

    for ev in sorted {
        // Context fields: last-write-wins across the stream — later events
        // reflect the most current device/player state.
        if ev.account_ref.is_some() {
            f.account_ref = ev.account_ref.clone();
        }
        if ev.rating_key.is_some() {
            f.rating_key = ev.rating_key.clone();
        }
        if ev.player.is_some() {
            f.player = ev.player.clone();
        }
        if ev.platform.is_some() {
            f.platform = ev.platform.clone();
        }
        if ev.product.is_some() {
            f.product = ev.product.clone();
        }
        if ev.device.is_some() {
            f.device = ev.device.clone();
        }
        if ev.ip_address.is_some() {
            f.ip_address = ev.ip_address;
        }
        if let Some(d) = extract_duration_ms(&ev.raw) {
            f.duration_ms = Some(f.duration_ms.map_or(d, |cur| cur.max(d)));
        }

        // Close out the interval since the last processed event *before*
        // acting on this event's own type — the interval
        // `[last_ts, ev.received_at)` was "playing" iff we were already
        // playing coming into this event. This has to run unconditionally
        // for every event (not just pause/stop/scrobble): the poller emits
        // a fresh `media.play`-mapped snapshot on every tick while a
        // session is active, and each of those ticks closes out one
        // playing interval — if accumulation only happened on
        // pause/stop/scrobble, polled-only sessions (the common case when
        // Plex Pass/webhooks aren't configured) would never accrue
        // `watched_ms` at all between ticks.
        advance(playing, &mut f.watched_ms, &mut last_offset, &mut last_ts, ev);

        match ev.event_type.as_str() {
            "media.play" | "media.resume" => {
                if !playing {
                    if let Some(pause_at) = pause_started_at.take() {
                        f.paused_ms += (ev.received_at - pause_at).num_milliseconds().max(0);
                    }
                    playing = true;
                }
            }
            "media.pause" => {
                if playing {
                    playing = false;
                    f.paused_counter += 1;
                    pause_started_at = Some(ev.received_at);
                }
            }
            "media.stop" => {
                if playing {
                    playing = false;
                } else if let Some(pause_at) = pause_started_at.take() {
                    f.paused_ms += (ev.received_at - pause_at).num_milliseconds().max(0);
                }
                f.stopped_at = Some(ev.received_at);
            }
            "media.scrobble" => {
                f.is_finished = true;
            }
            // `media.rate` and anything unrecognized don't affect playback
            // accounting — ratings are handled by the caller (see
            // `tracker::webhook::handle_rating`); unknown event types are
            // still recorded in `play_events` for forensic replay, just
            // ignored here rather than erroring.
            _ => {}
        }
    }

    f.view_offset_ms = last_offset;

    f.percent_complete = f.duration_ms.filter(|&d| d > 0).map(|d| {
        let progress = f.view_offset_ms.unwrap_or(f.watched_ms);
        (progress as f32 / d as f32).clamp(0.0, 1.0)
    });

    if let Some(pct) = f.percent_complete {
        if pct >= COMPLETE_THRESHOLD {
            f.is_finished = true;
        }
    }

    if f.stopped_at.is_some() && !f.is_finished {
        if let Some(pct) = f.percent_complete {
            f.is_abandoned = pct < ABANDON_THRESHOLD;
        }
    }

    Some(f)
}

/// Close out the interval `[last_ts, ev.received_at)`, crediting it to
/// `watched_ms` only if `playing` was true coming into this event. Prefers
/// the Plex-reported `view_offset_ms` delta (matches spec: "Σ(playing
/// intervals)"); a backward jump (seek/rewind) can't be trusted as negative
/// progress, so that interval falls back to wall-clock elapsed time instead
/// of being discarded outright — a legitimate re-watch of earlier content
/// still accrues watched time. Always advances `last_offset`/`last_ts`
/// regardless of `playing`, so the next interval's delta is measured from
/// the right baseline.
fn advance(
    playing: bool,
    watched_ms: &mut i64,
    last_offset: &mut Option<i64>,
    last_ts: &mut DateTime<Utc>,
    ev: &PlayEvent,
) {
    if playing {
        match (*last_offset, ev.view_offset_ms) {
            (Some(prev), Some(cur)) if cur >= prev => *watched_ms += cur - prev,
            _ => *watched_ms += (ev.received_at - *last_ts).num_milliseconds().max(0),
        }
    }
    if ev.view_offset_ms.is_some() {
        *last_offset = ev.view_offset_ms;
    }
    *last_ts = ev.received_at;
}

/// Plex nests the item runtime under `Metadata.duration` (webhook payloads)
/// or directly on the session entry (poller-constructed raw JSON via
/// `serde_json::to_value` on the typed session) — check both shapes
/// defensively; a raw payload with neither just contributes no duration.
fn extract_duration_ms(raw: &serde_json::Value) -> Option<i64> {
    raw.get("Metadata")
        .and_then(|m| m.get("duration"))
        .or_else(|| raw.get("duration"))
        .and_then(|v| v.as_i64())
}

/// Load every event for `session_key`, fold them, resolve to local ids, and
/// upsert `play_sessions`. Returns `Ok(None)` (not an error) when there's
/// nothing to fold yet, or when the account/media can't be resolved yet —
/// the raw `play_events` rows remain the source of truth until a later
/// call (once *arr ingest has seen the item, or another event supplies the
/// account) can resolve them.
pub async fn reconstruct_and_persist(pool: &PgPool, session_key: &str) -> MuseResult<Option<PlaySession>> {
    let events = repo::play_event::list_for_session(pool, session_key).await?;
    let Some(fold) = fold_events(&events) else {
        return Ok(None);
    };

    let account_id = match &fold.account_ref {
        Some(account_ref) => {
            // First-seen-here-wins account discovery: a new Plex user's
            // very first telemetry event is a legitimate place to create
            // their `accounts` row, same posture as `arr::ingest`'s
            // `ensure_library`. Never blends: keyed strictly on the Plex
            // account id carried by the event itself.
            let account = repo::account::upsert_by_plex_account_id(
                pool,
                &NewAccount {
                    plex_account_id: Some(account_ref.clone()),
                    is_home_user: true,
                    ..Default::default()
                },
            )
            .await?;
            Some(account.id)
        }
        None => None,
    };

    let (media_item_id, episode_id) = match &fold.rating_key {
        Some(rating_key) => resolve_rating_key(pool, rating_key).await?,
        None => (None, None),
    };

    if account_id.is_none() || (media_item_id.is_none() && episode_id.is_none()) {
        tracing::debug!(
            session_key,
            account_resolved = account_id.is_some(),
            media_resolved = media_item_id.is_some() || episode_id.is_some(),
            "session reconstruction: account/media not resolved yet; play_events retains the raw stream"
        );
        return Ok(None);
    }

    let session = repo::play_session::upsert(
        pool,
        &NewPlaySession {
            account_id,
            media_item_id,
            episode_id,
            session_key: Some(session_key.to_string()),
            tautulli_ref_id: None,
            started_at: fold.started_at,
            stopped_at: fold.stopped_at,
            duration_ms: fold.duration_ms,
            watched_ms: Some(fold.watched_ms),
            view_offset_ms: fold.view_offset_ms,
            percent_complete: fold.percent_complete,
            paused_counter: fold.paused_counter,
            paused_ms: fold.paused_ms,
            is_finished: fold.is_finished,
            is_abandoned: fold.is_abandoned,
            player: fold.player.clone(),
            platform: fold.platform.clone(),
            product: fold.product.clone(),
            device: fold.device.clone(),
            ip_address: fold.ip_address,
            started_hour: Some(fold.started_at.hour() as i32),
            started_dow: Some(fold.started_at.weekday().num_days_from_sunday() as i32),
            is_cinema_context: Some(is_cinema_context(&fold)),
        },
    )
    .await?;

    Ok(Some(session))
}

/// Resolve a raw Plex `ratingKey` to local ids: movies (and shows/other
/// top-level items) resolve via `media_items`; TV episode rating keys
/// resolve via `episodes` (which carries its own `media_item_id` pointing
/// at the owning show).
pub(crate) async fn resolve_rating_key(
    pool: &PgPool,
    rating_key: &str,
) -> MuseResult<(Option<i64>, Option<i64>)> {
    if let Some(item) = repo::media_item::get_by_plex_rating_key(pool, rating_key).await? {
        return Ok((Some(item.id), None));
    }
    if let Some(episode) = repo::episode::get_by_plex_rating_key(pool, rating_key).await? {
        return Ok((Some(episode.media_item_id), Some(episode.id)));
    }
    Ok((None, None))
}

/// Heuristic "big screen" signal (spec §3.3: "TV/large-screen vs
/// phone/commute") from whatever device/player/product/platform strings the
/// session carried. Best-effort — Plex doesn't expose a canonical
/// screen-size field.
fn is_cinema_context(fold: &Fold) -> bool {
    let haystack = [&fold.player, &fold.product, &fold.platform, &fold.device]
        .into_iter()
        .flatten()
        .map(|s| s.to_ascii_lowercase())
        .collect::<Vec<_>>()
        .join(" ");

    const TV_KEYWORDS: &[&str] = &[
        "tv",
        "living room",
        "roku",
        "chromecast",
        "appletv",
        "apple tv",
        "shield",
        "firetv",
        "fire tv",
    ];
    TV_KEYWORDS.iter().any(|kw| haystack.contains(kw))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ev(
        id: i64,
        received_at: DateTime<Utc>,
        event_type: &str,
        view_offset_ms: Option<i64>,
        duration_ms: Option<i64>,
    ) -> PlayEvent {
        let mut raw = json!({"event": event_type});
        if let Some(d) = duration_ms {
            raw["duration"] = json!(d);
        }
        PlayEvent {
            id,
            received_at,
            source: "plex_webhook".to_string(),
            event_type: event_type.to_string(),
            account_ref: Some("1".to_string()),
            session_key: Some("sess-1".to_string()),
            rating_key: Some("rk-1".to_string()),
            view_offset_ms,
            player: Some("Living Room TV".to_string()),
            platform: Some("Plex for Android (TV)".to_string()),
            product: None,
            device: None,
            ip_address: None,
            raw,
        }
    }

    #[test]
    fn fold_events_empty_returns_none() {
        assert!(fold_events(&[]).is_none());
    }

    #[test]
    fn finishes_at_or_above_complete_threshold_even_without_scrobble() {
        let t0 = Utc::now();
        let events = vec![
            ev(1, t0, "media.play", Some(0), Some(100_000)),
            ev(2, t0 + chrono::Duration::seconds(1), "media.stop", Some(92_000), None),
        ];
        let fold = fold_events(&events).expect("should fold");
        assert!(fold.is_finished, "92% watched must count as finished even with no scrobble");
        assert!(!fold.is_abandoned);
    }

    #[test]
    fn abandons_when_stopped_well_before_completion() {
        let t0 = Utc::now();
        let events = vec![
            ev(1, t0, "media.play", Some(0), Some(100_000)),
            ev(2, t0 + chrono::Duration::seconds(1), "media.stop", Some(5_000), None),
        ];
        let fold = fold_events(&events).expect("should fold");
        assert!(!fold.is_finished);
        assert!(fold.is_abandoned, "stopping at 5% must be flagged abandoned");
    }

    #[test]
    fn neither_finished_nor_abandoned_in_the_middle() {
        let t0 = Utc::now();
        let events = vec![
            ev(1, t0, "media.play", Some(0), Some(100_000)),
            ev(2, t0 + chrono::Duration::seconds(1), "media.stop", Some(50_000), None),
        ];
        let fold = fold_events(&events).expect("should fold");
        assert!(!fold.is_finished);
        assert!(!fold.is_abandoned, "stopping mid-way is neither a finish nor an abandon");
    }

    #[test]
    fn scrobble_marks_finished_regardless_of_percent() {
        let t0 = Utc::now();
        let events = vec![
            ev(1, t0, "media.play", Some(0), Some(100_000)),
            // Scrobble fires ~90% in but before the final stop event; percent
            // here is deliberately below threshold to isolate the scrobble path.
            ev(2, t0 + chrono::Duration::seconds(1), "media.scrobble", Some(50_000), None),
        ];
        let fold = fold_events(&events).expect("should fold");
        assert!(fold.is_finished, "an explicit scrobble must mark finished regardless of percent");
    }

    #[test]
    fn pause_counting_and_paused_ms() {
        let t0 = Utc::now();
        let events = vec![
            ev(1, t0, "media.play", Some(0), Some(600_000)),
            ev(2, t0 + chrono::Duration::seconds(10), "media.pause", Some(10_000), None),
            ev(3, t0 + chrono::Duration::seconds(40), "media.resume", Some(10_000), None),
            ev(4, t0 + chrono::Duration::seconds(50), "media.stop", Some(20_000), None),
        ];
        let fold = fold_events(&events).expect("should fold");
        assert_eq!(fold.paused_counter, 1);
        assert_eq!(fold.paused_ms, 30_000, "paused from t+10s to t+40s == 30s");
        // watched_ms: 0->10000 while playing (10s), then 10000->20000 after resume (10s) = 20000ms
        assert_eq!(fold.watched_ms, 20_000);
    }

    #[test]
    fn multiple_pauses_increment_counter_each_time() {
        let t0 = Utc::now();
        let events = vec![
            ev(1, t0, "media.play", Some(0), Some(600_000)),
            ev(2, t0 + chrono::Duration::seconds(5), "media.pause", Some(5_000), None),
            ev(3, t0 + chrono::Duration::seconds(10), "media.resume", Some(5_000), None),
            ev(4, t0 + chrono::Duration::seconds(15), "media.pause", Some(10_000), None),
            ev(5, t0 + chrono::Duration::seconds(20), "media.resume", Some(10_000), None),
            ev(6, t0 + chrono::Duration::seconds(25), "media.stop", Some(15_000), None),
        ];
        let fold = fold_events(&events).expect("should fold");
        assert_eq!(fold.paused_counter, 2);
    }

    #[test]
    fn late_event_convergence_is_order_independent() {
        let t0 = Utc::now();
        let events = vec![
            ev(1, t0, "media.play", Some(0), Some(100_000)),
            ev(2, t0 + chrono::Duration::seconds(5), "media.pause", Some(30_000), None),
            ev(3, t0 + chrono::Duration::seconds(10), "media.resume", Some(30_000), None),
            ev(4, t0 + chrono::Duration::seconds(20), "media.stop", Some(95_000), None),
        ];

        let forward = fold_events(&events).expect("should fold forward order");

        // Simulate the "late event" case: the events arrive/are stored in a
        // scrambled order (e.g. the pause was persisted after the stop due
        // to network jitter) — reconstruction is always handed the full,
        // current set and must converge to the identical result because it
        // sorts internally by (received_at, id), not input order.
        let mut scrambled = events.clone();
        scrambled.swap(1, 3);
        scrambled.swap(0, 2);
        let reordered = fold_events(&scrambled).expect("should fold scrambled order");

        assert_eq!(forward, reordered, "fold must be a function of the event set, not input order");
    }

    #[test]
    fn rewind_does_not_produce_negative_watched_time() {
        let t0 = Utc::now();
        let events = vec![
            ev(1, t0, "media.play", Some(50_000), Some(200_000)),
            // User rewinds from 60s to 10s, then keeps watching.
            ev(2, t0 + chrono::Duration::seconds(10), "media.play", Some(60_000), None),
            ev(3, t0 + chrono::Duration::seconds(11), "media.play", Some(10_000), None),
            ev(4, t0 + chrono::Duration::seconds(21), "media.stop", Some(20_000), None),
        ];
        let fold = fold_events(&events).expect("should fold");
        assert!(fold.watched_ms >= 0, "watched_ms must never go negative on a rewind");
    }

    #[test]
    fn unknown_event_type_is_ignored_not_fatal() {
        let t0 = Utc::now();
        let events = vec![
            ev(1, t0, "media.play", Some(0), Some(100_000)),
            ev(2, t0 + chrono::Duration::seconds(1), "media.unknown-future-event", Some(1_000), None),
            ev(3, t0 + chrono::Duration::seconds(2), "media.stop", Some(2_000), None),
        ];
        let fold = fold_events(&events).expect("should fold despite an unrecognized event type");
        assert_eq!(fold.stopped_at, Some(events[2].received_at));
    }

    #[test]
    fn extract_duration_ms_checks_both_shapes() {
        assert_eq!(extract_duration_ms(&json!({"duration": 123})), Some(123));
        assert_eq!(extract_duration_ms(&json!({"Metadata": {"duration": 456}})), Some(456));
        assert_eq!(extract_duration_ms(&json!({"nothing": true})), None);
    }
}
