//! Error types for the webhook broker.

use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde_json::json;
use thiserror::Error;

/// Broker error types.
#[derive(Error, Debug)]
pub enum BrokerError {
    #[error("database error: {0}")]
    Database(String),

    #[error("invalid signature")]
    InvalidSignature,

    #[error("unknown tenant or plugin")]
    UnknownTenant,

    #[error("queue full")]
    QueueFull,

    #[error("invalid JSON: {0}")]
    InvalidJson(String),

    #[error("authentication failed")]
    AuthFailed,

    #[error("not found")]
    NotFound,

    #[error("WebSocket error: {0}")]
    WebSocket(String),

    #[error("internal server error: {0}")]
    Internal(String),
}

impl From<rusqlite::Error> for BrokerError {
    fn from(e: rusqlite::Error) -> Self {
        BrokerError::Database(e.to_string())
    }
}

impl From<r2d2::Error> for BrokerError {
    fn from(e: r2d2::Error) -> Self {
        BrokerError::Database(e.to_string())
    }
}

impl IntoResponse for BrokerError {
    fn into_response(self) -> Response {
        let (status, error_message) = match self {
            BrokerError::Database(_) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Database error".to_string(),
            ),
            BrokerError::InvalidSignature => {
                (StatusCode::UNAUTHORIZED, "Invalid signature".to_string())
            }
            BrokerError::UnknownTenant => (
                StatusCode::NOT_FOUND,
                "Unknown tenant or plugin".to_string(),
            ),
            BrokerError::QueueFull => (
                StatusCode::SERVICE_UNAVAILABLE,
                "Queue full, retry later".to_string(),
            ),
            BrokerError::InvalidJson(ref msg) => (StatusCode::BAD_REQUEST, msg.clone()),
            BrokerError::AuthFailed => {
                (StatusCode::UNAUTHORIZED, "Authentication failed".to_string())
            }
            BrokerError::NotFound => (StatusCode::NOT_FOUND, "Not found".to_string()),
            BrokerError::WebSocket(ref msg) => (StatusCode::BAD_GATEWAY, msg.clone()),
            BrokerError::Internal(ref msg) => {
                (StatusCode::INTERNAL_SERVER_ERROR, msg.clone())
            }
        };

        let body = Json(json!({ "error": error_message }));
        (status, body).into_response()
    }
}

pub type Result<T> = std::result::Result<T, BrokerError>;
