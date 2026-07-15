//! MUSEX-CAP-SEC-01 (Plane TERM #399): outer bearer-token auth for the
//! sensitive/mutating HTTP surface.
//!
//! The S118 Epic capstone flagged that the WIRE-01..06 experience-layer
//! routes (`/discord/respond`, `/conversational`, `/premiere*`,
//! `/channels/director/refresh`, `/friends/opt-in`/`opt-out`), the
//! Constellation-GUI control surface (`/api/settings`, `/api/graph/*`), and
//! the manual ops triggers (`/ops/*`) were reachable with `0.0.0.0` bind and
//! only a `TraceLayer` — no auth at all, a real exposure now that those
//! routes are wired to live, consent-relevant, DB-mutating behavior. See
//! `crate::http::router` for exactly which routes this middleware is
//! attached to (via `Router::route_layer`) and which are deliberately left
//! open (health/liveness + the pre-existing read-only/webhook surface the
//! capstone did not flag).
//!
//! ## Mechanism
//! Muse had no existing inbound-auth convention to reuse (every `*_api_key`
//! field in [`crate::config::Config`] is an OUTBOUND credential to a
//! third-party upstream — Plex/Tautulli/*arr/TMDb/Trakt/etc — never an
//! inbound check on Muse's own HTTP surface; confirmed by inspection before
//! writing this). So this adds the minimal new mechanism: a single shared
//! bearer token, [`crate::config::Config::api_token`]
//! (`MUSE_API_TOKEN`, <secret-manager>-materialized, S1/S7), checked against the
//! request's `Authorization: Bearer <token>` header — the same header
//! convention every other bearer-token integration in this crate already
//! uses on the client side (see e.g. `taste_review::panel`,
//! `taste_review::sink`), so a caller adopting this is consistent with the
//! rest of the fleet's HTTP conventions.
//!
//! **Which caller(s) currently hit these routes was not independently
//! confirmed in this change** — no existing inbound-auth header could be
//! discovered to "match" (there was none), and the callers described in the
//! WIRE-0x specs (Discord bot dispatch, Constellation GUI, the premiere/
//! director schedulers) live outside this repo. The convention chosen
//! (`Authorization: Bearer <MUSE_API_TOKEN>`) is flagged for review —
//! whichever caller(s) are wired up operationally need `MUSE_API_TOKEN`
//! materialized into their own environment and sent on every request to a
//! protected route.
//!
//! ## Fail-closed posture
//! - [`Config::api_token`] configured: a request without a matching
//!   `Authorization: Bearer <token>` header is rejected `401` before the
//!   handler runs.
//! - [`Config::api_token`] **not** configured: every protected route
//!   answers `503` ("auth not configured") rather than either accepting
//!   every caller or silently going open — same fail-closed-over-fail-open
//!   posture the fleet already applies elsewhere (see
//!   `dsn_guard_fail_closed_lesson` in memory: allowlists over denylists,
//!   deny-by-default over accept-by-default).
//! - The ONE documented escape hatch: `MUSE_AUTH_DISABLED=1` (or `true`),
//!   [`Config::auth_disabled`] — an explicit, operator-set dev-mode flag
//!   that reopens the protected surface when no token is configured. It has
//!   NO effect once a token IS configured (a configured token always
//!   enforces, regardless of this flag) — it only changes the
//!   unconfigured-token default from "503" to "open".
//!
//! Constant-time comparison is used for the token check (a bearer token is
//! a secret; short-circuiting string equality on it is a timing side
//! channel) — see [`constant_time_eq`].

use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::HeaderMap;
use axum::middleware::Next;
use axum::response::Response;

use crate::error::MuseError;
use crate::http::AppState;

/// `axum::middleware::from_fn_with_state` handler — attach via
/// `Router::route_layer` to exactly the routes that must be protected (see
/// `crate::http::router`). Runs BEFORE the wrapped handler, so a rejected
/// request never touches the database or any other side effect.
pub async fn require_api_token(
    State(state): State<Arc<AppState>>,
    request: Request,
    next: Next,
) -> Result<Response, MuseError> {
    let Some(configured_token) = state.config.api_token.as_deref() else {
        if state.config.auth_disabled {
            return Ok(next.run(request).await);
        }
        return Err(MuseError::ServiceUnavailable(
            "auth not configured (MUSE_API_TOKEN unset; set MUSE_AUTH_DISABLED=1 to \
             explicitly allow unauthenticated access in dev)"
                .to_string(),
        ));
    };

    match extract_bearer_token(request.headers()) {
        Some(presented) if constant_time_eq(presented.as_bytes(), configured_token.as_bytes()) => {
            Ok(next.run(request).await)
        }
        _ => Err(MuseError::Unauthorized(
            "missing or invalid Authorization: Bearer <token>".to_string(),
        )),
    }
}

/// Pull the token out of `Authorization: Bearer <token>`. Any other scheme,
/// a malformed/missing header, or non-UTF8 bytes all fall through to `None`
/// (rejected) — never panics, never partially matches.
fn extract_bearer_token(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::trim)
        .filter(|v| !v.is_empty())
}

/// Constant-time byte comparison for the token check — deliberately not
/// `==`, which short-circuits on the first mismatched byte and can leak
/// timing information about how much of a secret prefix an attacker has
/// guessed correctly. Length is compared first (an inherent, unavoidable
/// signal even in constant-time-comparison libraries; not itself sensitive
/// here since both sides are opaque tokens, not derived from user input).
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff: u8 = 0;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn headers_with_auth(value: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::header::AUTHORIZATION,
            HeaderValue::from_str(value).unwrap(),
        );
        headers
    }

    #[test]
    fn extract_bearer_token_parses_well_formed_header() {
        let headers = headers_with_auth("Bearer secret-token-value");
        assert_eq!(extract_bearer_token(&headers), Some("secret-token-value"));
    }

    #[test]
    fn extract_bearer_token_rejects_missing_header() {
        let headers = HeaderMap::new();
        assert_eq!(extract_bearer_token(&headers), None);
    }

    #[test]
    fn extract_bearer_token_rejects_wrong_scheme() {
        let headers = headers_with_auth("Basic dXNlcjpwYXNz");
        assert_eq!(extract_bearer_token(&headers), None);
    }

    #[test]
    fn extract_bearer_token_rejects_empty_token() {
        let headers = headers_with_auth("Bearer ");
        assert_eq!(extract_bearer_token(&headers), None);
    }

    #[test]
    fn constant_time_eq_matches_equal_slices() {
        assert!(constant_time_eq(b"same-value", b"same-value"));
    }

    #[test]
    fn constant_time_eq_rejects_different_length() {
        assert!(!constant_time_eq(b"short", b"much-longer-value"));
    }

    #[test]
    fn constant_time_eq_rejects_same_length_different_content() {
        assert!(!constant_time_eq(b"aaaaaaaa", b"aaaaaaab"));
    }
}
