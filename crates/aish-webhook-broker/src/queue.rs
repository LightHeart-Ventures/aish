//! Webhook message type + an in-memory FIFO queue with bounded capacity.
//!
//! The [`Webhook`] struct is the on-the-wire message shape. [`MessageQueue`] is
//! a pure in-memory hot cache with FIFO semantics and oldest-drops-first
//! overflow — it is unit-tested here and used as a lightweight buffer; SQLite
//! (see [`crate::db`]) is the durable source of truth.

use std::collections::VecDeque;

/// A webhook message routed through the broker.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct Webhook {
    pub id: String,
    pub tenant_id: String,
    pub plugin_id: String,
    pub event_type: String,
    pub payload: serde_json::Value,
    pub received_at: chrono::DateTime<chrono::Utc>,
}

impl Webhook {
    /// Construct a new webhook with a generated id and `received_at = now`.
    pub fn new(
        tenant_id: impl Into<String>,
        plugin_id: impl Into<String>,
        event_type: impl Into<String>,
        payload: serde_json::Value,
    ) -> Self {
        Self {
            id: format!("wh_{}", uuid::Uuid::new_v4().simple()),
            tenant_id: tenant_id.into(),
            plugin_id: plugin_id.into(),
            event_type: event_type.into(),
            payload,
            received_at: chrono::Utc::now(),
        }
    }

    /// The client-facing push envelope (WebSocket / long-poll message body).
    pub fn to_envelope(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "webhook",
            "id": self.id,
            "tenant_id": self.tenant_id,
            "plugin_id": self.plugin_id,
            "event_type": self.event_type,
            "payload": self.payload,
            "received_at": self.received_at.to_rfc3339(),
        })
    }
}

/// Bounded in-memory FIFO queue. When full, the oldest message is dropped to
/// make room (back-pressure that favours fresh events).
pub struct MessageQueue {
    inner: VecDeque<Webhook>,
    max_size: usize,
}

impl MessageQueue {
    pub fn new(max_size: usize) -> Self {
        Self {
            inner: VecDeque::new(),
            max_size: max_size.max(1),
        }
    }

    pub fn len(&self) -> usize {
        self.inner.len()
    }

    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    /// Enqueue a message. Returns `Some(dropped)` if an old message was evicted.
    pub fn enqueue(&mut self, wh: Webhook) -> Option<Webhook> {
        let dropped = if self.inner.len() >= self.max_size {
            self.inner.pop_front()
        } else {
            None
        };
        self.inner.push_back(wh);
        dropped
    }

    /// Dequeue the oldest message (FIFO).
    pub fn dequeue(&mut self) -> Option<Webhook> {
        self.inner.pop_front()
    }

    /// Remove a message by id (ACK). Returns true if it was present.
    pub fn ack(&mut self, id: &str) -> bool {
        if let Some(pos) = self.inner.iter().position(|w| w.id == id) {
            self.inner.remove(pos);
            true
        } else {
            false
        }
    }

    /// Snapshot the current messages without draining.
    pub fn peek_all(&self) -> Vec<Webhook> {
        self.inner.iter().cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wh(tag: &str) -> Webhook {
        Webhook::new("t1", "github", "push", serde_json::json!({ "tag": tag }))
    }

    #[test]
    fn enqueue_dequeue_fifo() {
        let mut q = MessageQueue::new(10);
        let a = wh("a");
        let b = wh("b");
        let (ida, idb) = (a.id.clone(), b.id.clone());
        q.enqueue(a);
        q.enqueue(b);
        assert_eq!(q.len(), 2);
        assert_eq!(q.dequeue().unwrap().id, ida);
        assert_eq!(q.dequeue().unwrap().id, idb);
        assert!(q.dequeue().is_none());
    }

    #[test]
    fn queue_full_drops_oldest() {
        let mut q = MessageQueue::new(2);
        let a = wh("a");
        let ida = a.id.clone();
        q.enqueue(a);
        q.enqueue(wh("b"));
        let dropped = q.enqueue(wh("c"));
        assert_eq!(dropped.unwrap().id, ida, "oldest should be evicted");
        assert_eq!(q.len(), 2);
    }

    #[test]
    fn ack_removes_from_queue() {
        let mut q = MessageQueue::new(10);
        let a = wh("a");
        let ida = a.id.clone();
        q.enqueue(a);
        q.enqueue(wh("b"));
        assert!(q.ack(&ida));
        assert_eq!(q.len(), 1);
        assert!(!q.ack(&ida), "second ack of same id is a no-op");
    }

    #[test]
    fn zero_max_size_is_clamped_to_one() {
        let mut q = MessageQueue::new(0);
        q.enqueue(wh("a"));
        q.enqueue(wh("b"));
        assert_eq!(q.len(), 1);
    }
}
