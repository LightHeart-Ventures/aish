//! Long-poll fallback helper for clients behind restrictive firewalls.
//!
//! The actual HTTP handler lives in [`crate::http::poll_pending`]; this module
//! provides the wait primitive it uses: block until either a new webhook is
//! announced for the (tenant, plugin) or the timeout elapses.

use std::time::Duration;

use crate::config::BrokerConfig;

/// Wait up to `wait_secs` (clamped to the broker's `poll_timeout_secs`) for a
/// new webhook to be announced for `(tenant_id, plugin_id)`.
///
/// Returns immediately when `wait_secs == 0` (non-blocking probe).
pub async fn wait_for_message(
    config: &BrokerConfig,
    tenant_id: &str,
    plugin_id: &str,
    wait_secs: u64,
) {
    if wait_secs == 0 {
        return;
    }
    let capped = wait_secs.min(config.poll_timeout_secs.max(1));
    let notify = config.hub.notifier(tenant_id, plugin_id);

    // Register interest BEFORE checking, so a webhook arriving during the wait
    // wakes us (notify_waiters only wakes already-parked waiters).
    let notified = notify.notified();
    tokio::select! {
        _ = notified => {}
        _ = tokio::time::sleep(Duration::from_secs(capped)) => {}
    }
}
