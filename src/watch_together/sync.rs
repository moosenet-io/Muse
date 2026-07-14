//! MUSEX-09 (Plane `TERM #385`): playback-sync delegation + coordinated-start
//! fallback — the piece [`super`]'s module doc calls out as deliberately
//! out of scope ("turning a locked pick into an actual play command against
//! a real media server is a SEPARATE, later concern (MUSEX-09's server
//! adapter)"). This module is that concern's *decision* half: given the
//! group's present clients and their sync capability, decide whether to
//! **delegate** frame-accurate sync to a real server primitive (Jellyfin
//! SyncPlay, where verified available) or fall back to a **coordinated
//! start** (a synchronized countdown + "press play now" + presence ping)
//! that Muse itself drives.
//!
//! ## Muse builds NO low-level sync protocol (the load-bearing AC)
//! There is no frame-timing loop, no seek-drift-correction protocol, no
//! custom wire format anywhere in this module. There are exactly two things
//! Muse can do with a locked pick:
//!
//! 1. **Delegate** — call a [`ServerSyncPrimitive`] (a trait; the only real
//!    candidate today is a Jellyfin SyncPlay adapter, itself an unverified,
//!    config-gated stub — see [`JellyfinSyncPlay`]) and let the SERVER do
//!    the frame-accurate work. Muse never reimplements what SyncPlay does.
//! 2. **Coordinate** — run [`CoordinatedStart`], a pure, deterministic
//!    countdown + presence-ping scheduler with millisecond-resolution
//!    timestamps clients are told to "press play at," and nothing else. No
//!    media protocol, no seek/skip commands, no timeline polling loop lives
//!    here — that's `plex_control::cast::CastController`'s job (starting
//!    playback on ONE target), reused unmodified for the actual "press play
//!    now" trigger by a caller of this module, not duplicated inside it.
//!
//! Per the MUSEX-01 server-abstraction audit (`docs/MUSEX-experience-layer.md`
//! §2.3), frame-accurate sync is **unverified almost everywhere** — Plex has
//! no group-sync primitive Muse can call (`PlexControlClient` only ever
//! targets one `machineIdentifier` at a time), Plex's own Watch Together is
//! an unverified external-API assumption, and Jellyfin SyncPlay itself is
//! flagged `[EXTERNAL-API ASSUMPTION — UNVERIFIED]` — so [`CoordinatedStart`]
//! is the DEFAULT path, not the exception, matching that document's own
//! conclusion.
//!
//! ## Decision logic (server-agnostic)
//! [`decide_sync_mode`] never asks "which server is this," only "does every
//! present client support the SAME server sync primitive." That keeps the
//! decision itself server-agnostic — the server-SPECIFIC part is entirely
//! behind the [`ServerSyncPrimitive`] trait, exactly mirroring how
//! [`super`]'s orchestration stays server-agnostic behind
//! `CastController`-shaped seams elsewhere in this crate.

use std::collections::HashSet;

use async_trait::async_trait;
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use serde::Serialize;

use crate::config::Config;
use crate::error::MuseResult;

// --- capability model -------------------------------------------------------

/// The one server-side sync primitive this module knows how to name today.
/// A real deployment might grow more variants (e.g. a future verified Plex
/// primitive); the point of keeping this as an enum rather than a free-form
/// string is that [`decide_sync_mode`] can cheaply prove "all clients agree
/// on the SAME primitive" with a plain equality/set check.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
pub enum ServerSyncPrimitiveKind {
    /// Jellyfin's SyncPlay group-playback feature. Presumed capable of
    /// frame-accurate multi-client sync per Jellyfin's own docs, but
    /// **[EXTERNAL-API ASSUMPTION — UNVERIFIED]** against a live server —
    /// see [`JellyfinSyncPlay`]'s doc comment. Named here only so the
    /// decision logic can be exercised and tested; it is not claimed to be
    /// production-verified.
    JellyfinSyncPlay,
    /// Plex's own group-playback ("Watch Together") primitive. Per the
    /// MUSEX-01 audit (`docs/MUSEX-experience-layer.md` §2.3) this is an
    /// **[EXTERNAL-API ASSUMPTION — UNVERIFIED]** with a history of limited
    /// scope/investment — Muse does NOT integrate it and there is no
    /// adapter for it here. It exists as a named variant purely so
    /// [`decide_sync_mode`] can represent (and correctly refuse to
    /// delegate to) a group whose clients disagree on which server
    /// primitive to use — a mixed-primitive group must fall back to
    /// coordinated-start, never pick one server's primitive for clients
    /// registered against another.
    PlexWatchTogether,
}

/// One present client's sync capability — sourced from the capability map
/// (`docs/MUSEX-experience-layer.md` §2.3), never guessed at runtime. A
/// caller resolving real clients (e.g. from `plex_control`/a future
/// `media_server` adapter's client registry) is responsible for mapping a
/// concrete device/app into one of these two shapes; this module never
/// inspects a client type name or infers capability from a server kind.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum SyncCapability {
    /// This client can be frame-accurately synced via a server-side group
    /// primitive Muse can delegate to.
    FrameSync { primitive: ServerSyncPrimitiveKind },
    /// This client has no known server-side group-sync primitive (Android
    /// TV, Plex Companion targets, bare Chromecast, or any client not
    /// explicitly verified otherwise) — per the capability map, this is the
    /// default assumption, not an edge case.
    CoordinatedStartOnly,
}

/// One present client in a watch-together group, paired with its resolved
/// [`SyncCapability`]. `client_id` is opaque to this module (a
/// `machineIdentifier`, a Jellyfin device id, whatever the caller's client
/// registry uses) — [`decide_sync_mode`] never inspects it beyond using it
/// to report which clients are in which mode.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ClientCapability {
    pub client_id: String,
    pub capability: SyncCapability,
}

// --- the decision ------------------------------------------------------------

/// The chosen sync mode for a group, plus enough detail for the group/UI to
/// know which one is in use and why (the AC's "REPORTS which mode is in
/// use" requirement).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum SyncMode {
    /// Every present client supports the SAME server sync primitive — Muse
    /// delegates frame-accurate sync to it via [`ServerSyncPrimitive`] and
    /// does nothing else.
    Delegated {
        primitive: ServerSyncPrimitiveKind,
        client_ids: Vec<String>,
    },
    /// At least one present client lacks a server sync primitive, or the
    /// present clients don't all agree on the SAME one (a mixed group) —
    /// Muse falls back to [`CoordinatedStart`] for EVERY client, including
    /// ones that individually could have supported delegation. `reason`
    /// is a short, human-readable explanation for the report surface.
    CoordinatedStart {
        reason: String,
        client_ids: Vec<String>,
    },
}

impl SyncMode {
    /// The client ids this mode applies to, regardless of variant — the one
    /// place a caller needs to look to know who's covered.
    pub fn client_ids(&self) -> &[String] {
        match self {
            SyncMode::Delegated { client_ids, .. } => client_ids,
            SyncMode::CoordinatedStart { client_ids, .. } => client_ids,
        }
    }

    /// Short, stable label for logs/UI — not meant to be the full `reason`.
    pub fn label(&self) -> &'static str {
        match self {
            SyncMode::Delegated { .. } => "delegated",
            SyncMode::CoordinatedStart { .. } => "coordinated_start",
        }
    }
}

/// Decide how to sync a group's present clients. **The load-bearing rule:**
/// delegate to a server primitive ONLY when every present client supports
/// the exact same one; any unsupported client, empty roster, or mixed set
/// of primitives falls back to [`SyncMode::CoordinatedStart`] for the WHOLE
/// group — never a partial delegation that frame-syncs some clients and
/// coordinates others (that would silently leave the coordinated clients
/// out of step with a server-driven timeline no one is telling them about).
///
/// Pure and deterministic: same `clients` (any order) yields the same
/// [`SyncMode`] — `client_ids` is sorted before being stored so caller
/// input order never leaks into the reported mode.
pub fn decide_sync_mode(clients: &[ClientCapability]) -> SyncMode {
    let mut client_ids: Vec<String> = clients.iter().map(|c| c.client_id.clone()).collect();
    client_ids.sort_unstable();

    if clients.is_empty() {
        return SyncMode::CoordinatedStart {
            reason: "no clients present".to_string(),
            client_ids,
        };
    }

    // Collect the set of distinct primitives among clients that support
    // ANY server primitive at all.
    let primitives: HashSet<ServerSyncPrimitiveKind> = clients
        .iter()
        .filter_map(|c| match &c.capability {
            SyncCapability::FrameSync { primitive } => Some(*primitive),
            SyncCapability::CoordinatedStartOnly => None,
        })
        .collect();

    let all_frame_sync = clients
        .iter()
        .all(|c| matches!(c.capability, SyncCapability::FrameSync { .. }));

    if all_frame_sync && primitives.len() == 1 {
        // Every client supports a server primitive, and it's the SAME one.
        let primitive = *primitives.iter().next().expect("len == 1");
        return SyncMode::Delegated {
            primitive,
            client_ids,
        };
    }

    let reason = if !all_frame_sync && primitives.is_empty() {
        "no present client supports a server sync primitive".to_string()
    } else if !all_frame_sync {
        "mixed group: at least one present client has no server sync primitive".to_string()
    } else {
        // all_frame_sync but primitives.len() > 1
        "mixed group: present clients support different server sync primitives".to_string()
    };

    SyncMode::CoordinatedStart { reason, client_ids }
}

// --- server sync primitive (delegation) --------------------------------------

/// The server-side sync primitive Muse can DELEGATE to. Muse only ever
/// CALLS this trait — it never implements frame-accurate sync itself. Real
/// implementations are config-gated (see [`JellyfinSyncPlay::from_config`])
/// and, per the MUSEX-01 audit, currently unverified against a live server;
/// tests use [`MockServerSync`] exclusively (S9: no live server calls).
#[async_trait]
pub trait ServerSyncPrimitive: Send + Sync {
    fn kind(&self) -> ServerSyncPrimitiveKind;

    /// Start a synced group playback of `rating_key` (or the primitive's
    /// own item-id shape) across `client_ids`, each starting at
    /// `offset_ms`. Delegation only — the primitive owns frame-accurate
    /// timing/drift correction entirely; Muse passes the request through
    /// and reports success/failure, nothing more.
    async fn delegate(
        &self,
        client_ids: &[String],
        rating_key: &str,
        offset_ms: i64,
    ) -> MuseResult<()>;
}

/// Jellyfin SyncPlay adapter — config-gated, and, per the MUSEX-01
/// server-abstraction audit, an **[EXTERNAL-API ASSUMPTION — UNVERIFIED]**
/// stub: Muse has zero other Jellyfin integration in this crate, and this
/// adapter has never been exercised against a live Jellyfin server. It
/// exists only to reserve the seam (mirroring `plex_control::cast`'s own
/// `GoogleCastController` placeholder pattern) — every call currently
/// returns [`crate::error::MuseError::NotImplemented`]. A future item that
/// verifies Jellyfin's actual SyncPlay API shape/transport replaces this
/// body, not its signature or its config-gating.
pub struct JellyfinSyncPlay {
    #[allow(dead_code)]
    base_url: String,
    #[allow(dead_code)]
    api_key: String,
}

impl JellyfinSyncPlay {
    /// Build a client from [`Config`]. Returns `None` (graceful degrade,
    /// same posture as `PlexClient::from_config`) when `JELLYFIN_URL` /
    /// `JELLYFIN_TOKEN` aren't set — an unconfigured Jellyfin means
    /// delegation simply isn't available, never a hardcoded fallback
    /// target (S1: no infra literals/secrets).
    pub fn from_config(config: &Config) -> Option<Self> {
        let base_url = config.jellyfin_url.clone()?;
        let api_key = config.jellyfin_token.clone()?;
        Some(Self { base_url, api_key })
    }
}

#[async_trait]
impl ServerSyncPrimitive for JellyfinSyncPlay {
    fn kind(&self) -> ServerSyncPrimitiveKind {
        ServerSyncPrimitiveKind::JellyfinSyncPlay
    }

    async fn delegate(
        &self,
        _client_ids: &[String],
        _rating_key: &str,
        _offset_ms: i64,
    ) -> MuseResult<()> {
        Err(crate::error::MuseError::NotImplemented)
    }
}

// --- coordinated-start fallback -----------------------------------------------

/// One client's coordinated-start instruction: when to press play, and a
/// presence-ping cadence so the group UI can show who's still "there" during
/// the countdown. Deterministic given the coordinator's inputs — no clock
/// sampling, no randomness.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CoordinatedStartSignal {
    pub client_id: String,
    /// The instant every client is told to press play. Identical across
    /// all clients in one coordinated start — that's the entire mechanism.
    pub play_at: DateTime<Utc>,
    /// Presence-ping instants between `now` and `play_at`, inclusive of
    /// neither endpoint, spaced by the coordinator's configured interval.
    /// Purely informational (UI "still here?" heartbeats) — never a media
    /// command.
    pub presence_pings: Vec<DateTime<Utc>>,
}

/// A pure, deterministic countdown + presence-ping scheduler. This is the
/// ENTIRE coordinated-start mechanism — no seek loop, no drift correction,
/// no media protocol. `play_at` is computed once and handed to every
/// client identically; turning `play_at` into an actual `CastController`
/// `play_media`/`play` call per target is the caller's job (reusing the
/// existing seam, never duplicated here).
#[derive(Debug, Clone)]
pub struct CoordinatedStart {
    /// How long the countdown runs before `play_at`.
    pub countdown: ChronoDuration,
    /// Spacing between presence pings during the countdown. Must be
    /// positive and no larger than `countdown`, or [`CoordinatedStart::schedule`]
    /// returns a `play_at` with zero presence pings rather than dividing by
    /// a degenerate interval.
    pub presence_ping_interval: ChronoDuration,
}

impl CoordinatedStart {
    /// A reasonable default: 5-second countdown, presence pings every
    /// second. Not a magic infra value — purely a UX timing choice, safe to
    /// hardcode (S1 covers infra literals/secrets, not UI timing).
    pub fn default_timing() -> Self {
        Self {
            countdown: ChronoDuration::seconds(5),
            presence_ping_interval: ChronoDuration::seconds(1),
        }
    }

    /// Build one [`CoordinatedStartSignal`] per `client_id`, all sharing the
    /// same `play_at` (`now + self.countdown`). Deterministic given `now` —
    /// no internal clock reads.
    pub fn schedule(
        &self,
        client_ids: &[String],
        now: DateTime<Utc>,
    ) -> Vec<CoordinatedStartSignal> {
        let play_at = now + self.countdown;
        let presence_pings = self.presence_pings(now, play_at);

        let mut ids: Vec<String> = client_ids.to_vec();
        ids.sort_unstable();
        ids.into_iter()
            .map(|client_id| CoordinatedStartSignal {
                client_id,
                play_at,
                presence_pings: presence_pings.clone(),
            })
            .collect()
    }

    fn presence_pings(&self, now: DateTime<Utc>, play_at: DateTime<Utc>) -> Vec<DateTime<Utc>> {
        if self.presence_ping_interval <= ChronoDuration::zero() {
            return Vec::new();
        }
        let mut pings = Vec::new();
        let mut next = now + self.presence_ping_interval;
        while next < play_at {
            pings.push(next);
            next += self.presence_ping_interval;
        }
        pings
    }
}

// --- mock for tests ------------------------------------------------------------

/// Test-only [`ServerSyncPrimitive`] that records every `delegate` call it
/// receives — used exclusively to PROVE the negative test: when
/// [`decide_sync_mode`] returns [`SyncMode::CoordinatedStart`], nothing
/// calls `delegate` on any primitive. Never used outside `#[cfg(test)]`
/// (S9: no live server calls anywhere, mocked or otherwise, outside tests).
#[cfg(test)]
pub struct MockServerSync {
    pub calls: std::sync::Mutex<Vec<(Vec<String>, String, i64)>>,
}

#[cfg(test)]
impl MockServerSync {
    pub fn new() -> Self {
        Self {
            calls: std::sync::Mutex::new(Vec::new()),
        }
    }

    pub fn call_count(&self) -> usize {
        self.calls.lock().unwrap().len()
    }
}

#[cfg(test)]
#[async_trait]
impl ServerSyncPrimitive for MockServerSync {
    fn kind(&self) -> ServerSyncPrimitiveKind {
        ServerSyncPrimitiveKind::JellyfinSyncPlay
    }

    async fn delegate(
        &self,
        client_ids: &[String],
        rating_key: &str,
        offset_ms: i64,
    ) -> MuseResult<()> {
        self.calls
            .lock()
            .unwrap()
            .push((client_ids.to_vec(), rating_key.to_string(), offset_ms));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame_sync(id: &str) -> ClientCapability {
        ClientCapability {
            client_id: id.to_string(),
            capability: SyncCapability::FrameSync {
                primitive: ServerSyncPrimitiveKind::JellyfinSyncPlay,
            },
        }
    }

    fn frame_sync_with(id: &str, primitive: ServerSyncPrimitiveKind) -> ClientCapability {
        ClientCapability {
            client_id: id.to_string(),
            capability: SyncCapability::FrameSync { primitive },
        }
    }

    fn coordinated_only(id: &str) -> ClientCapability {
        ClientCapability {
            client_id: id.to_string(),
            capability: SyncCapability::CoordinatedStartOnly,
        }
    }

    // ------------------------------------------------------------------
    // Delegates where the capability map allows
    // ------------------------------------------------------------------

    #[test]
    fn all_syncplay_capable_clients_delegate_to_the_shared_primitive() {
        let clients = vec![
            frame_sync("tv-1"),
            frame_sync("tv-2"),
            frame_sync("phone-1"),
        ];
        let mode = decide_sync_mode(&clients);
        match mode {
            SyncMode::Delegated {
                primitive,
                client_ids,
            } => {
                assert_eq!(primitive, ServerSyncPrimitiveKind::JellyfinSyncPlay);
                assert_eq!(client_ids, vec!["phone-1", "tv-1", "tv-2"]);
            }
            other => panic!("expected Delegated for an all-SyncPlay group, got {other:?}"),
        }
    }

    #[test]
    fn decide_sync_mode_is_order_independent() {
        let forward = vec![frame_sync("a"), frame_sync("b"), frame_sync("c")];
        let shuffled = vec![frame_sync("c"), frame_sync("a"), frame_sync("b")];
        assert_eq!(decide_sync_mode(&forward), decide_sync_mode(&shuffled));
    }

    // ------------------------------------------------------------------
    // Coordinated-start fallback where not (single unsupported client)
    // ------------------------------------------------------------------

    #[test]
    fn a_single_android_tv_style_unsupported_client_forces_coordinated_start() {
        let clients = vec![coordinated_only("android-tv-1")];
        let mode = decide_sync_mode(&clients);
        assert!(
            matches!(mode, SyncMode::CoordinatedStart { .. }),
            "an unsupported client must fall back to coordinated start: {mode:?}"
        );
    }

    // ------------------------------------------------------------------
    // Mixed-capability group: coordinated-start for ALL
    // ------------------------------------------------------------------

    #[test]
    fn mixed_group_falls_back_to_coordinated_start_for_every_client() {
        let clients = vec![
            frame_sync("jellyfin-tv"),
            coordinated_only("plex-android-tv"),
        ];
        let mode = decide_sync_mode(&clients);
        match mode {
            SyncMode::CoordinatedStart { client_ids, reason } => {
                assert_eq!(
                    client_ids,
                    vec!["jellyfin-tv", "plex-android-tv"],
                    "coordinated start must cover EVERY present client, including the \
                     SyncPlay-capable one — never a partial delegation"
                );
                assert!(!reason.is_empty());
            }
            other => panic!("expected CoordinatedStart for a mixed group, got {other:?}"),
        }
    }

    // ------------------------------------------------------------------
    // All FrameSync-capable but on DIFFERENT primitives: still
    // coordinated-start (no partial/ambiguous delegation)
    // ------------------------------------------------------------------

    #[test]
    fn all_frame_sync_but_disagreeing_primitives_falls_back_to_coordinated_start() {
        // Every client CAN frame-sync, but they're registered against two
        // different server primitives (one Jellyfin SyncPlay, one Plex
        // Watch Together). Delegating would mean picking ONE server's
        // primitive for a client registered against the other -- exactly
        // the ambiguity `decide_sync_mode` must refuse. This closes the one
        // untested branch of the load-bearing decision:
        // all-FrameSync-but-mismatched -> CoordinatedStart, never Delegated.
        let clients = vec![
            frame_sync_with("jellyfin-tv", ServerSyncPrimitiveKind::JellyfinSyncPlay),
            frame_sync_with("plex-tv", ServerSyncPrimitiveKind::PlexWatchTogether),
        ];
        let mode = decide_sync_mode(&clients);
        match mode {
            SyncMode::CoordinatedStart { client_ids, reason } => {
                assert_eq!(
                    client_ids,
                    vec!["jellyfin-tv", "plex-tv"],
                    "coordinated start must cover EVERY client even when all are \
                     frame-sync-capable -- disagreeing primitives means no partial delegation"
                );
                assert!(
                    reason.contains("different server sync primitives"),
                    "the reason must make the disagreement explicit for the report surface: \
                     {reason}"
                );
            }
            other => panic!(
                "all-FrameSync-but-mismatched-primitives must surface as CoordinatedStart, \
                 never Delegated to one server's primitive: {other:?}"
            ),
        }
    }

    #[tokio::test]
    async fn disagreeing_primitives_never_trigger_a_frame_sync_attempt() {
        // Complement to the decision test above: prove (via the same
        // drive_mode helper the other negative tests use) that NO server
        // sync primitive is invoked when the clients disagree on which
        // primitive to use.
        let mock = MockServerSync::new();
        let clients = vec![
            frame_sync_with("jellyfin-tv", ServerSyncPrimitiveKind::JellyfinSyncPlay),
            frame_sync_with("plex-tv", ServerSyncPrimitiveKind::PlexWatchTogether),
        ];
        let mode = decide_sync_mode(&clients);
        drive_mode(&mode, &mock).await;
        assert_eq!(
            mock.call_count(),
            0,
            "disagreeing primitives must not delegate to ANY primitive -- coordinated start \
             covers the whole group"
        );
    }

    #[test]
    fn empty_roster_degrades_to_coordinated_start_not_a_panic() {
        let mode = decide_sync_mode(&[]);
        assert!(matches!(mode, SyncMode::CoordinatedStart { .. }));
        assert!(mode.client_ids().is_empty());
    }

    // ------------------------------------------------------------------
    // REPORTS which mode is in use
    // ------------------------------------------------------------------

    #[test]
    fn sync_mode_reports_a_stable_label_for_ui_logging() {
        let delegated = decide_sync_mode(&[frame_sync("a")]);
        let coordinated = decide_sync_mode(&[coordinated_only("a")]);
        assert_eq!(delegated.label(), "delegated");
        assert_eq!(coordinated.label(), "coordinated_start");
    }

    // ------------------------------------------------------------------
    // NEGATIVE TEST (load-bearing): no frame-sync attempt on an
    // unsupported/mixed client — the primitive is NEVER invoked when the
    // mode is CoordinatedStart.
    // ------------------------------------------------------------------

    /// Simulates what a real caller does with the decision: only invoke
    /// `ServerSyncPrimitive::delegate` when `decide_sync_mode` returned
    /// `Delegated`. Using `MockServerSync` proves the primitive is never
    /// touched for a CoordinatedStart outcome, for both the single-
    /// unsupported-client case and the mixed-group case.
    async fn drive_mode(mode: &SyncMode, primitive: &dyn ServerSyncPrimitive) {
        if let SyncMode::Delegated { client_ids, .. } = mode {
            primitive
                .delegate(client_ids, "some-rating-key", 0)
                .await
                .ok();
        }
        // SyncMode::CoordinatedStart: deliberately does NOT touch `primitive`
        // at all -- that omission is the entire point of this helper.
    }

    #[tokio::test]
    async fn unsupported_client_never_triggers_a_frame_sync_attempt() {
        let mock = MockServerSync::new();
        let clients = vec![coordinated_only("android-tv-1")];
        let mode = decide_sync_mode(&clients);
        drive_mode(&mode, &mock).await;
        assert_eq!(
            mock.call_count(),
            0,
            "no server sync primitive may be invoked when the mode is CoordinatedStart"
        );
    }

    #[tokio::test]
    async fn mixed_group_never_triggers_a_frame_sync_attempt_on_any_client() {
        let mock = MockServerSync::new();
        let clients = vec![
            frame_sync("jellyfin-tv"),
            coordinated_only("plex-android-tv"),
        ];
        let mode = decide_sync_mode(&clients);
        drive_mode(&mode, &mock).await;
        assert_eq!(
            mock.call_count(),
            0,
            "a mixed group must not delegate for ANY client, including the \
             SyncPlay-capable one -- coordinated start covers the whole group"
        );
    }

    #[tokio::test]
    async fn all_capable_group_does_invoke_the_primitive_exactly_once() {
        // Sanity complement to the negative tests above: prove the mock
        // actually detects a call when delegation SHOULD happen, so the
        // negative tests' zero-calls assertion is meaningful and not just
        // trivially true because the mock is unreachable.
        let mock = MockServerSync::new();
        let clients = vec![frame_sync("tv-1"), frame_sync("tv-2")];
        let mode = decide_sync_mode(&clients);
        drive_mode(&mode, &mock).await;
        assert_eq!(mock.call_count(), 1);
    }

    // ------------------------------------------------------------------
    // ServerSyncPrimitive: real impl is config-gated, unverified stub
    // ------------------------------------------------------------------

    #[test]
    fn jellyfin_syncplay_is_none_when_unconfigured() {
        let config = Config::default();
        assert!(JellyfinSyncPlay::from_config(&config).is_none());
    }

    #[test]
    fn jellyfin_syncplay_builds_when_configured() {
        let config = Config {
            jellyfin_url: Some("http://example.invalid:8096".to_string()),
            jellyfin_token: Some("test-token".to_string()),
            ..Config::default()
        };
        assert!(JellyfinSyncPlay::from_config(&config).is_some());
    }

    #[tokio::test]
    async fn jellyfin_syncplay_delegate_is_an_unverified_stub_not_a_live_call() {
        let config = Config {
            jellyfin_url: Some("http://example.invalid:8096".to_string()),
            jellyfin_token: Some("test-token".to_string()),
            ..Config::default()
        };
        let primitive = JellyfinSyncPlay::from_config(&config).expect("configured jellyfin client");
        assert_eq!(primitive.kind(), ServerSyncPrimitiveKind::JellyfinSyncPlay);
        let err = primitive
            .delegate(&["tv-1".to_string()], "rk-1", 0)
            .await
            .unwrap_err();
        assert!(
            matches!(err, crate::error::MuseError::NotImplemented),
            "the real Jellyfin adapter must stay an explicit NotImplemented stub until its \
             API shape is verified against a live server -- never a silent live call"
        );
    }

    // ------------------------------------------------------------------
    // Coordinated-start coordinator: pure, deterministic
    // ------------------------------------------------------------------

    #[test]
    fn coordinated_start_gives_every_client_the_same_play_at() {
        let coordinator = CoordinatedStart::default_timing();
        let now = DateTime::parse_from_rfc3339("2026-07-14T18:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let signals = coordinator.schedule(
            &[
                "tv-1".to_string(),
                "tv-2".to_string(),
                "phone-1".to_string(),
            ],
            now,
        );
        assert_eq!(signals.len(), 3);
        let play_at = signals[0].play_at;
        assert!(signals.iter().all(|s| s.play_at == play_at));
        assert_eq!(play_at, now + ChronoDuration::seconds(5));
    }

    #[test]
    fn coordinated_start_is_deterministic_and_order_independent() {
        let coordinator = CoordinatedStart::default_timing();
        let now = DateTime::parse_from_rfc3339("2026-07-14T18:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let forward = coordinator.schedule(&["a".to_string(), "b".to_string()], now);
        let reversed = coordinator.schedule(&["b".to_string(), "a".to_string()], now);
        assert_eq!(forward, reversed);
    }

    #[test]
    fn coordinated_start_presence_pings_are_spaced_within_the_countdown() {
        let coordinator = CoordinatedStart {
            countdown: ChronoDuration::seconds(5),
            presence_ping_interval: ChronoDuration::seconds(1),
        };
        let now = DateTime::parse_from_rfc3339("2026-07-14T18:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let signals = coordinator.schedule(&["tv-1".to_string()], now);
        let pings = &signals[0].presence_pings;
        // 1s, 2s, 3s, 4s -- strictly before the 5s play_at.
        assert_eq!(pings.len(), 4);
        for p in pings {
            assert!(*p > now && *p < signals[0].play_at);
        }
    }

    #[test]
    fn coordinated_start_zero_ping_interval_yields_no_pings_not_a_panic() {
        let coordinator = CoordinatedStart {
            countdown: ChronoDuration::seconds(5),
            presence_ping_interval: ChronoDuration::zero(),
        };
        let now = Utc::now();
        let signals = coordinator.schedule(&["tv-1".to_string()], now);
        assert!(signals[0].presence_pings.is_empty());
    }

    // ------------------------------------------------------------------
    // Muse builds NO low-level sync protocol (source-scan negative test,
    // same idiom as watch_together::mod's own server-agnostic scan)
    // ------------------------------------------------------------------

    /// Scans this module's own non-test source for vocabulary that would
    /// indicate a custom low-level sync/media protocol (frame timing, seek
    /// loops, RTP/RTCP-style jargon, a bespoke wire format) creeping in.
    /// This module is only ever allowed to (a) call `ServerSyncPrimitive`
    /// or (b) schedule plain timestamps -- if this test ever fails, a
    /// low-level protocol has been reimplemented here, violating the AC.
    #[test]
    fn sync_module_has_zero_low_level_sync_protocol_vocabulary() {
        let source = include_str!("sync.rs");
        let (production_code, _test_code) = source
            .split_once("#[cfg(test)]")
            .expect("this file has a #[cfg(test)] marker before its test module");

        let forbidden = [
            "seek_loop",
            "frame_timing",
            "drift_correct",
            "rtcp",
            "RTCP",
            "rtp_",
            "socket",
            "Socket",
            "TcpStream",
            "UdpSocket",
        ];
        for needle in forbidden {
            assert!(
                !production_code.contains(needle),
                "watch_together::sync's production code must never grow a low-level sync \
                 protocol implementation -- found forbidden reference {needle:?}"
            );
        }
    }
}
