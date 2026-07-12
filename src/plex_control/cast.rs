//! `CastController` — the seam between "how we start playback on a target"
//! and everything upstream of it (the channel composer, transport routes).
//!
//! Today there's exactly one implementation: `PlexControlClient`, which
//! drives targets via the Plex Companion / client-control protocol. Per
//! spec §4d-A, bare Chromecasts that aren't running a Plex receiver (i.e.
//! not discoverable/controllable via Plex Companion) need a **Google Cast**
//! fallback (DIAL discovery + Cast v2 launching the Plex receiver app with
//! the queue). That is out of scope for MUSE-22 — this trait exists so a
//! later `GoogleCastController` can be dropped in without touching callers.

use async_trait::async_trait;

use crate::error::MuseResult;

use super::client::PlexControlClient;
use super::models::TimelinePoll;

/// Common playback-control surface, implementable by any transport that can
/// start/stop/advance media on a target device.
#[async_trait]
pub trait CastController: Send + Sync {
    /// Start playing `rating_key` on `target`, optionally within a
    /// previously built play queue.
    async fn play_media(
        &self,
        target: &str,
        rating_key: &str,
        play_queue_id: Option<i64>,
        offset_ms: i64,
    ) -> MuseResult<()>;

    async fn play(&self, target: &str) -> MuseResult<()>;
    async fn pause(&self, target: &str) -> MuseResult<()>;
    async fn stop(&self, target: &str) -> MuseResult<()>;
    async fn skip_next(&self, target: &str) -> MuseResult<()>;

    /// Poll transport/playback state for `target`.
    async fn poll_timeline(&self, target: &str) -> MuseResult<TimelinePoll>;
}

#[async_trait]
impl CastController for PlexControlClient {
    async fn play_media(
        &self,
        target: &str,
        rating_key: &str,
        play_queue_id: Option<i64>,
        offset_ms: i64,
    ) -> MuseResult<()> {
        PlexControlClient::play_media(self, target, rating_key, play_queue_id, offset_ms).await
    }

    async fn play(&self, target: &str) -> MuseResult<()> {
        PlexControlClient::play(self, target).await
    }

    async fn pause(&self, target: &str) -> MuseResult<()> {
        PlexControlClient::pause(self, target).await
    }

    async fn stop(&self, target: &str) -> MuseResult<()> {
        PlexControlClient::stop(self, target).await
    }

    async fn skip_next(&self, target: &str) -> MuseResult<()> {
        PlexControlClient::skip_next(self, target).await
    }

    async fn poll_timeline(&self, target: &str) -> MuseResult<TimelinePoll> {
        PlexControlClient::timeline_poll(self, target).await
    }
}

/// Placeholder for the Google Cast (DIAL + Cast v2) fallback used for bare
/// Chromecasts that don't have a controllable Plex receiver registered.
///
/// TODO(muse): implement raw Cast v2 (DIAL discovery, `CastSession`, launch
/// the Plex receiver app id with the built play queue). Deliberately not
/// implemented in MUSE-22 — this struct only exists to reserve the seam;
/// every method currently returns `MuseError::NotImplemented`.
pub struct GoogleCastController;

#[async_trait]
impl CastController for GoogleCastController {
    async fn play_media(
        &self,
        _target: &str,
        _rating_key: &str,
        _play_queue_id: Option<i64>,
        _offset_ms: i64,
    ) -> MuseResult<()> {
        Err(crate::error::MuseError::NotImplemented)
    }

    async fn play(&self, _target: &str) -> MuseResult<()> {
        Err(crate::error::MuseError::NotImplemented)
    }

    async fn pause(&self, _target: &str) -> MuseResult<()> {
        Err(crate::error::MuseError::NotImplemented)
    }

    async fn stop(&self, _target: &str) -> MuseResult<()> {
        Err(crate::error::MuseError::NotImplemented)
    }

    async fn skip_next(&self, _target: &str) -> MuseResult<()> {
        Err(crate::error::MuseError::NotImplemented)
    }

    async fn poll_timeline(&self, _target: &str) -> MuseResult<TimelinePoll> {
        Err(crate::error::MuseError::NotImplemented)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn google_cast_controller_is_not_implemented_yet() {
        let ctrl = GoogleCastController;
        let err = ctrl.play("some-chromecast").await.unwrap_err();
        assert!(matches!(err, crate::error::MuseError::NotImplemented));
    }
}
