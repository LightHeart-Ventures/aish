//! Dispatch hub: routes freshly-received webhooks to connected WebSocket
//! clients in real time and wakes any parked long-pollers.
//!
//! The hub holds only *transient* in-memory state. Durability lives in SQLite;
//! the hub is a best-effort fast path. If a client is offline, the webhook is
//! simply left in the DB queue for the next poll/reconnect to drain.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::Notify;

/// A connected WebSocket client's push channel + routing key.
struct WsClient {
    tenant_id: String,
    plugin_id: String,
    tx: UnboundedSender<String>,
}

/// In-memory dispatch hub shared across all requests via `Arc`.
#[derive(Default)]
pub struct Hub {
    /// Per-(tenant,plugin) notifier used to wake long-pollers.
    notifiers: Mutex<HashMap<(String, String), Arc<Notify>>>,
    /// Connected WebSocket clients keyed by session token.
    ws_clients: Mutex<HashMap<String, WsClient>>,
    /// Cached count of connected clients (cheap `/health` read).
    connected: AtomicUsize,
}

impl Hub {
    pub fn new() -> Self {
        Self::default()
    }

    /// Get (or create) the notifier for a (tenant, plugin) pair.
    pub fn notifier(&self, tenant_id: &str, plugin_id: &str) -> Arc<Notify> {
        let key = (tenant_id.to_string(), plugin_id.to_string());
        let mut map = self.notifiers.lock().unwrap();
        map.entry(key).or_insert_with(|| Arc::new(Notify::new())).clone()
    }

    /// Announce a new webhook: push to matching WS clients and wake pollers.
    /// Returns the number of WebSocket clients the envelope was delivered to.
    pub fn dispatch(&self, tenant_id: &str, plugin_id: &str, envelope: &str) -> usize {
        let mut delivered = 0usize;
        {
            let clients = self.ws_clients.lock().unwrap();
            for c in clients.values() {
                if c.tenant_id == tenant_id && c.plugin_id == plugin_id {
                    if c.tx.send(envelope.to_string()).is_ok() {
                        delivered += 1;
                    }
                }
            }
        }
        // Wake long-pollers regardless.
        self.notifier(tenant_id, plugin_id).notify_waiters();
        delivered
    }

    /// Register a connected WebSocket client.
    pub fn register_ws(
        &self,
        session_token: &str,
        tenant_id: &str,
        plugin_id: &str,
        tx: UnboundedSender<String>,
    ) {
        let mut clients = self.ws_clients.lock().unwrap();
        let existed = clients
            .insert(
                session_token.to_string(),
                WsClient {
                    tenant_id: tenant_id.to_string(),
                    plugin_id: plugin_id.to_string(),
                    tx,
                },
            )
            .is_some();
        if !existed {
            self.connected.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Remove a WebSocket client on disconnect.
    pub fn unregister_ws(&self, session_token: &str) {
        let mut clients = self.ws_clients.lock().unwrap();
        if clients.remove(session_token).is_some() {
            self.connected.fetch_sub(1, Ordering::Relaxed);
        }
    }

    /// Number of currently-connected WebSocket clients.
    pub fn connected_count(&self) -> usize {
        self.connected.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dispatch_targets_only_matching_clients() {
        let hub = Hub::new();
        let (tx_a, mut rx_a) = tokio::sync::mpsc::unbounded_channel();
        let (tx_b, mut rx_b) = tokio::sync::mpsc::unbounded_channel();
        hub.register_ws("st_a", "t1", "github", tx_a);
        hub.register_ws("st_b", "t1", "slack", tx_b);
        assert_eq!(hub.connected_count(), 2);

        let n = hub.dispatch("t1", "github", "envelope");
        assert_eq!(n, 1);
        assert_eq!(rx_a.try_recv().unwrap(), "envelope");
        assert!(rx_b.try_recv().is_err());

        hub.unregister_ws("st_a");
        assert_eq!(hub.connected_count(), 1);
    }
}
