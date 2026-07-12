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
            MuseError::Conflict(msg) => (StatusCode::CONFLICT, msg.clone()),
            MuseError::Internal(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
            MuseError::Http(e) => (StatusCode::BAD_GATEWAY, e.to_string()),
            MuseError::Upstream { message, .. } => (StatusCode::BAD_GATEWAY, message.clone()),
        };

        let body = Json(json!({
            "error": message,
        }));

        (status, body).into_response()
    }
}

pub type MuseResult<T> = Result<T, MuseError>;
