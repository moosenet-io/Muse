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
        };

        let body = Json(json!({
            "error": message,
        }));

        (status, body).into_response()
    }
}

pub type MuseResult<T> = Result<T, MuseError>;
