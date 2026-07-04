//! Self-hosted webhook broker for the aish plugin system.
//!
//! Routes webhooks from external services (GitHub, Slack, etc.) to connected
//! aish clients via WebSocket or long-poll. This `lib` target exposes the
//! internals so integration tests (and embedders) can drive the router in-process.

pub mod config;
pub mod db;
pub mod dispatcher;
pub mod error;
pub mod http;
pub mod poll;
pub mod queue;
pub mod signature;
pub mod ws;
