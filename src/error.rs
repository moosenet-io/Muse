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

    #[error("internal error: {0}")]
    Internal(#[from] anyhow::Error),

    /// A downstream/external service (Plex, Tautulli, TMDb, …) returned an
    /// error, an unexpected response, or was unreachable. Kept generic
    /// (rather than e.g. `Plex(String)`) so it's reusable across every
    /// upstream integration, not just Plex control.
    #[error("upstream error: {0}")]
    Upstream(String),
}

impl IntoResponse for MuseError {
    fn into_response(self) -> Response {
        let (status, message) = match &self {
            MuseError::Database(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
            MuseError::Config(msg) => (StatusCode::INTERNAL_SERVER_ERROR, msg.clone()),
            MuseError::NotImplemented => (StatusCode::NOT_IMPLEMENTED, self.to_string()),
            MuseError::Internal(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
            MuseError::Upstream(msg) => (StatusCode::BAD_GATEWAY, msg.clone()),
        };

        let body = Json(json!({
            "error": message,
        }));

        (status, body).into_response()
    }
}

pub type MuseResult<T> = Result<T, MuseError>;
