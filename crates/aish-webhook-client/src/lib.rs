//! # aish-webhook-client
//!
//! Client-side integration for the aish webhook broker (SPR-059 Phase 4.8 / 4.9
//! / 5). Three cooperating pieces:
//!
//! * [`BrokerClient`] — **TASK-264** connection manager: auth handshake,
//!   exponential-backoff reconnect, heartbeat, graceful shutdown.
//! * [`WebhookService`] — **TASK-265** background message loop: read → dispatch
//!   → ack, cancellable via a `watch` shutdown signal.
//! * [`WebhookDispatcher`] — **TASK-268** handler dispatch: load handlers from
//!   `plugin.json`, match event types, apply filters, fork/exec with a timeout
//!   and full error isolation.
//!
//! All three are written against the [`Transport`] trait, so the entire flow is
//! exercised without sockets via [`transport::mock_transport`]. The real
//! WebSocket transport ([`transport::TungsteniteTransport`]) is behind the
//! default `net` feature.
//!
//! ```no_run
//! # #[cfg(feature = "net")]
//! # async fn demo() -> aish_webhook_client::Result<()> {
//! use std::sync::Arc;
//! use aish_webhook_client::{
//!     BrokerClient, BrokerConfig, PluginRegistry, WebhookDispatcher, WebhookService,
//!     transport::TungsteniteTransport,
//! };
//! use tokio::sync::watch;
//!
//! let config = BrokerConfig::load("~/.aish/config/broker.json")?;
//! let mut client = BrokerClient::new(config.clone());
//! let transport = TungsteniteTransport::connect(&config.broker_url).await?;
//! client.connect(transport).await?;
//!
//! let registry = Arc::new(PluginRegistry::load_dir("~/.aish/plugins")?);
//! let dispatcher = Arc::new(WebhookDispatcher::new(registry));
//! let (shutdown_tx, shutdown_rx) = watch::channel(false);
//!
//! let mut service = WebhookService::new(client, dispatcher);
//! tokio::spawn(async move { service.run(shutdown_rx).await });
//! // … on `:quit`: shutdown_tx.send(true).ok();
//! # Ok(())
//! # }
//! ```

pub mod audit;
pub mod backoff;
pub mod client;
pub mod dispatcher;
pub mod envelope;
pub mod error;
pub mod service;
pub mod transport;

pub use audit::{AuditRecord, AuditSink, JsonlAuditSink, MemoryAuditSink, NoopAuditSink};
pub use backoff::ExponentialBackoff;
pub use client::{BrokerClient, ConnState};
pub use dispatcher::{
    FlashSink, HandlerOutcome, PluginManifest, PluginRegistry, WebhookDispatcher, WebhookHandler,
    DEFAULT_HANDLER_TIMEOUT,
};
pub use envelope::{BrokerConfig, ClientFrame, ServerFrame, Webhook};
pub use error::{Result, WebhookClientError};
pub use service::{StopReason, WebhookService};
pub use transport::{Transport, WsMessage};
