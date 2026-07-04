//! Error type shared across the webhook client.

use thiserror::Error;

/// Errors surfaced by the broker connection manager and dispatcher.
#[derive(Debug, Error)]
pub enum WebhookClientError {
    /// The underlying transport stream ended (server hung up / socket closed).
    #[error("transport closed")]
    TransportClosed,

    /// The auth handshake was rejected or produced an unexpected frame.
    #[error("authentication failed: {0}")]
    Auth(String),

    /// Failure establishing or using the transport.
    #[error("connection error: {0}")]
    Connection(String),

    /// (De)serialization failure on a control/webhook frame.
    #[error("serialization error: {0}")]
    Serde(#[from] serde_json::Error),

    /// I/O failure (config load, handler exec).
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    /// Malformed or missing configuration.
    #[error("config error: {0}")]
    Config(String),

    /// A bounded wait elapsed (auth handshake).
    #[error("timed out after {0:?}")]
    Timeout(std::time::Duration),
}

/// Convenience alias.
pub type Result<T> = std::result::Result<T, WebhookClientError>;
