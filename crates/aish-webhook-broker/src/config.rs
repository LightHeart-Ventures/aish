//! Broker configuration and runtime state.

use std::sync::Arc;
use std::time::Instant;

use crate::db::DbPool;
use crate::dispatcher::Hub;

/// Runtime configuration + shared state for the webhook broker.
///
/// This is the axum `State` — it is cloned per request, so every field is
/// cheap to clone (pool handles, `Arc`, `Copy` scalars).
#[derive(Clone)]
pub struct BrokerConfig {
    /// SQLite connection pool (source of truth for queued webhooks).
    pub db: DbPool,

    /// In-memory dispatch hub: long-poll wakeups + connected WebSocket clients.
    pub hub: Arc<Hub>,

    /// Process start time — used to report uptime on `/health`.
    pub start_time: Instant,

    /// Maximum number of undelivered messages to retain per (tenant_id, plugin_id).
    pub max_queue_size: usize,

    /// WebSocket heartbeat interval in seconds.
    pub ws_heartbeat_secs: u64,

    /// Long-poll timeout in seconds (upper bound on `wait_secs`).
    pub poll_timeout_secs: u64,

    /// Message time-to-live in seconds (default 7 days).
    pub msg_ttl_secs: u64,
}
