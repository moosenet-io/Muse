//! (B) Session poller — spec §4-B.
//!
//! Every `MUSE_PLEX_POLL_SECS` (default 10s), polls `/status/sessions` and
//! folds each active session into `play_events` (source `plex_poll`) the
//! same way the webhook does, then triggers the same reconstruction. This
//! is what fills the gaps a missed/dropped webhook delivery leaves, and —
//! since Plex webhooks require Plex Pass — it's the *only* ingestion path
//! at all for a server without it, so it must never depend on the webhook
//! having run first.

use std::sync::Arc;
use std::time::Duration;

use crate::error::MuseResult;
use crate::http::AppState;
use crate::models::play_event::NewPlayEvent;
use crate::models::play_session::{DecisionKind, NewPlaySessionMediaInfo};
use crate::plex::{MediaItem as PlexSession, PlexClient};
use crate::repo;

use super::reconstruct;

/// Default poll cadence when `MUSE_PLEX_POLL_SECS` is unset/unparseable.
const DEFAULT_POLL_SECS: u64 = 10;

/// Spawn the poller as a background task. A no-op (logs once, returns
/// immediately, spawns nothing) when Plex isn't configured — same
/// graceful-degrade posture as every other Plex-backed feature in this
/// crate; there is nothing to poll without `PLEX_URL`/`PLEX_TOKEN`.
pub fn spawn(state: Arc<AppState>) {
    let Some(plex) = state.plex.clone() else {
        tracing::info!("plex session poller: PLEX_URL/PLEX_TOKEN not configured; poller disabled");
        return;
    };

    let interval_secs = state.config.plex_poll_secs.unwrap_or(DEFAULT_POLL_SECS).max(1);
    tracing::info!(interval_secs, "plex session poller: starting");

    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(Duration::from_secs(interval_secs));
        loop {
            ticker.tick().await;
            // A tick failure (Plex unreachable, a transient DB error) must
            // never kill the loop — log and simply try again next tick.
            if let Err(e) = poll_once(&state, &plex).await {
                tracing::warn!(error = %e, "plex session poller: tick failed; will retry next interval");
            }
        }
    });
}

async fn poll_once(state: &AppState, plex: &PlexClient) -> MuseResult<()> {
    let sessions = match plex.sessions().await {
        Ok(sessions) => sessions,
        Err(e) => {
            tracing::warn!(error = %e, "plex session poller: /status/sessions unreachable this tick; will retry");
            return Ok(());
        }
    };

    for session in &sessions {
        if let Err(e) = ingest_one_session(state, session).await {
            tracing::warn!(
                error = %e,
                session_key = session.session_key.as_deref().unwrap_or("?"),
                "plex session poller: failed to ingest one active session; continuing with the rest"
            );
        }
    }

    Ok(())
}

async fn ingest_one_session(state: &AppState, session: &PlexSession) -> MuseResult<()> {
    // Nothing to stitch a snapshot without a session key onto; `/status/
    // sessions` always includes one in practice, but stay defensive.
    let Some(session_key) = session.session_key.clone() else {
        return Ok(());
    };

    // Poll snapshots are folded through the exact same reconstruction as
    // webhook events (see `reconstruct::fold_events`), so they're mapped
    // onto the same small event-type vocabulary: a "paused" player state
    // becomes a `media.pause` event; anything else (playing, buffering, or
    // simply unspecified) is treated as advancing playback, i.e.
    // `media.play`. Every tick emits one of these — that's what lets a
    // polled-only deployment (no Plex Pass) accrue `watched_ms` at all.
    let event_type = match session.player.as_ref().and_then(|p| p.state.as_deref()) {
        Some("paused") => "media.pause",
        _ => "media.play",
    };

    let account_ref = session.resolved_account_id();
    let raw = serde_json::to_value(session).unwrap_or_else(|e| {
        tracing::warn!(error = %e, "plex session poller: failed to serialize session snapshot to raw jsonb");
        serde_json::json!({})
    });

    let new_event = NewPlayEvent {
        source: "plex_poll".to_string(),
        event_type: event_type.to_string(),
        account_ref,
        session_key: Some(session_key.clone()),
        rating_key: session.rating_key.clone(),
        view_offset_ms: session.view_offset,
        player: session.player.as_ref().and_then(|p| p.title.clone()),
        platform: session.player.as_ref().and_then(|p| p.platform.clone()),
        product: session.player.as_ref().and_then(|p| p.product.clone()),
        device: session.player.as_ref().and_then(|p| p.device.clone()),
        ip_address: None,
        raw,
    };

    repo::play_event::insert(&state.pool, &new_event).await?;

    let persisted = reconstruct::reconstruct_and_persist(&state.pool, &session_key).await?;

    if let Some(play_session) = persisted {
        if let Some(media_info) = plex_media_info(session) {
            repo::play_session::upsert_media_info(&state.pool, play_session.id, &media_info).await?;
        }
    }

    Ok(())
}

/// Best-effort mapping of Plex's session `Media`/`TranscodeSession` blocks
/// (only present on `/status/sessions`, not on webhook payloads) onto
/// `play_session_media_info` — spec §4-B: "capture transcode decision,
/// codecs, resolution." Returns `None` when the session carries no media
/// info at all rather than persisting an empty row.
fn plex_media_info(session: &PlexSession) -> Option<NewPlaySessionMediaInfo> {
    let media = session.media.first();
    if media.is_none() && session.transcode_session.is_none() {
        return None;
    }

    let decision_of = |raw: Option<&str>| -> Option<DecisionKind> {
        match raw?.to_ascii_lowercase().as_str() {
            "transcode" => Some(DecisionKind::Transcode),
            "directstream" => Some(DecisionKind::DirectStream),
            "copy" => Some(DecisionKind::Copy),
            "directplay" => Some(DecisionKind::DirectPlay),
            _ => None,
        }
    };

    let ts = session.transcode_session.as_ref();
    // No `TranscodeSession` block at all is Plex's own signal that
    // playback is direct-play end to end.
    let video_decision = ts
        .and_then(|t| decision_of(t.video_decision.as_deref()))
        .or(Some(DecisionKind::DirectPlay));
    let audio_decision = ts
        .and_then(|t| decision_of(t.audio_decision.as_deref()))
        .or(Some(DecisionKind::DirectPlay));
    let transcode_decision = ts.and_then(|t| decision_of(t.video_decision.as_deref()));

    Some(NewPlaySessionMediaInfo {
        video_decision,
        audio_decision,
        transcode_decision,
        container: ts
            .and_then(|t| t.container.clone())
            .or_else(|| media.and_then(|m| m.container.clone())),
        video_codec: media.and_then(|m| m.video_codec.clone()),
        audio_codec: media.and_then(|m| m.audio_codec.clone()),
        audio_channels: media.and_then(|m| m.audio_channels).map(|c| c as f32),
        video_resolution: media.and_then(|m| m.video_resolution.clone()),
        bitrate: media.and_then(|m| m.bitrate).map(|b| b as i32),
        width: media.and_then(|m| m.width).map(|w| w as i32),
        height: media.and_then(|m| m.height).map(|h| h as i32),
        transcode_reason: ts.and_then(|t| t.reason.clone()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plex::{MediaInfo, SessionPlayer, TranscodeSession};

    fn base_session() -> PlexSession {
        PlexSession {
            session_key: Some("5".to_string()),
            rating_key: Some("100".to_string()),
            ..Default::default()
        }
    }

    #[test]
    fn plex_media_info_none_when_no_media_or_transcode() {
        assert!(plex_media_info(&base_session()).is_none());
    }

    #[test]
    fn plex_media_info_defaults_to_direct_play_without_transcode_session() {
        let mut session = base_session();
        session.media = vec![MediaInfo {
            video_resolution: Some("1080".to_string()),
            video_codec: Some("h264".to_string()),
            ..Default::default()
        }];

        let info = plex_media_info(&session).expect("should build media info");
        assert_eq!(info.video_decision, Some(DecisionKind::DirectPlay));
        assert_eq!(info.audio_decision, Some(DecisionKind::DirectPlay));
        assert!(info.transcode_decision.is_none());
        assert_eq!(info.video_resolution.as_deref(), Some("1080"));
    }

    #[test]
    fn plex_media_info_maps_transcode_decision() {
        let mut session = base_session();
        session.media = vec![MediaInfo::default()];
        session.transcode_session = Some(TranscodeSession {
            video_decision: Some("transcode".to_string()),
            audio_decision: Some("copy".to_string()),
            reason: Some("videoCodecNotSupported".to_string()),
            ..Default::default()
        });

        let info = plex_media_info(&session).expect("should build media info");
        assert_eq!(info.video_decision, Some(DecisionKind::Transcode));
        assert_eq!(info.audio_decision, Some(DecisionKind::Copy));
        assert_eq!(info.transcode_decision, Some(DecisionKind::Transcode));
        assert_eq!(info.transcode_reason.as_deref(), Some("videoCodecNotSupported"));
    }

    #[test]
    fn poll_event_type_maps_paused_state() {
        let mut session = base_session();
        session.player = Some(SessionPlayer {
            state: Some("paused".to_string()),
            ..Default::default()
        });
        let event_type = match session.player.as_ref().and_then(|p| p.state.as_deref()) {
            Some("paused") => "media.pause",
            _ => "media.play",
        };
        assert_eq!(event_type, "media.pause");
    }
}
