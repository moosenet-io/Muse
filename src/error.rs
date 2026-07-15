//! Crate-wide error type + axum `IntoResponse` mapping.

use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde_json::json;

#[derive(Debug, thiserror::Error)]
pub enum MuseError {
    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),

    #[error("configuration error: {0}")]
    Config(String),

    #[error("not implemented yet")]
    NotImplemented,

    #[error("not found: {0}")]
    NotFound(String),

    /// A well-formed request whose *content* is invalid — e.g.
    /// `POST /proactive/{id}/ack`'s `outcome` field holding a value other
    /// than `"sent"`/`"dismissed"`. Distinct from [`MuseError::Config`]
    /// (a server-side misconfiguration, 500) — this is caller error, 400.
    #[error("bad request: {0}")]
    BadRequest(String),

    #[error("conflict: {0}")]
    Conflict(String),

    #[error("internal error: {0}")]
    Internal(#[from] anyhow::Error),

    /// Transport-level failure talking to an upstream HTTP dependency (e.g.
    /// Plex): connection refused, DNS failure, timeout, TLS error, etc.
    #[error("upstream request failed: {0}")]
    Http(#[from] reqwest::Error),

    /// An upstream/external service (Plex, Tautulli, TMDb, …) responded with a
    /// non-success status, an unexpected response, or was otherwise unusable.
    /// Generic across every upstream integration, not just Plex control.
    #[error("upstream error ({status}): {message}")]
    Upstream { status: u16, message: String },

    /// MUSE-29: a request that is well-formed but can't be served *right
    /// now* — the ffmpeg binary spawned but errored transiently, or a
    /// linear channel has no `channel_programs` row covering "now" (grid
    /// not yet filled / channel between programs). Distinct from
    /// [`MuseError::NotImplemented`] (a hard "this feature doesn't exist on
    /// this deployment yet", e.g. ffmpeg binary missing entirely) — both
    /// map to a clean, non-500 response so a tuner client can retry rather
    /// than treating either as a crash.
    #[error("service unavailable: {0}")]
    ServiceUnavailable(String),

    /// MUSEX-CAP-SEC-01 (Plane TERM #399): a protected route was called
    /// without a valid `Authorization: Bearer <token>` — see
    /// `crate::http::auth`. Distinct from [`MuseError::ServiceUnavailable`]
    /// (used when auth is required but not *configured* at all, a server
    /// misconfiguration): this is a genuine caller-auth failure, 401.
    #[error("unauthorized: {0}")]
    Unauthorized(String),
}

impl MuseError {
    /// Convenience constructor for a generic upstream failure with no specific
    /// upstream HTTP status of its own — transport wrappers, malformed bodies,
    /// or preconditions. Records status 502 (the status this maps to in
    /// `IntoResponse`); call sites that know the real upstream status should
    /// build `Upstream { status, message }` directly instead.
    pub fn upstream(message: impl Into<String>) -> Self {
        MuseError::Upstream {
            status: 502,
            message: message.into(),
        }
    }
}

impl IntoResponse for MuseError {
    fn into_response(self) -> Response {
        let (status, message) = match &self {
            MuseError::Database(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
            MuseError::Config(msg) => (StatusCode::INTERNAL_SERVER_ERROR, msg.clone()),
            MuseError::NotImplemented => (StatusCode::NOT_IMPLEMENTED, self.to_string()),
            MuseError::NotFound(msg) => (StatusCode::NOT_FOUND, msg.clone()),
            MuseError::BadRequest(msg) => (StatusCode::BAD_REQUEST, msg.clone()),
            MuseError::Conflict(msg) => (StatusCode::CONFLICT, msg.clone()),
            MuseError::Internal(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
            MuseError::Http(e) => (StatusCode::BAD_GATEWAY, e.to_string()),
            MuseError::Upstream { message, .. } => (StatusCode::BAD_GATEWAY, message.clone()),
            MuseError::ServiceUnavailable(msg) => (StatusCode::SERVICE_UNAVAILABLE, msg.clone()),
            MuseError::Unauthorized(msg) => (StatusCode::UNAUTHORIZED, msg.clone()),
        };

        let body = Json(json!({
            "error": message,
        }));

        (status, body).into_response()
    }
}

pub type MuseResult<T> = Result<T, MuseError>;
