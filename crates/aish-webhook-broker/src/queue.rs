//! The on-the-wire webhook message shape.
//!
//! Queuing (bounded FIFO with oldest-drops-first overflow) is implemented
//! directly against SQLite in [`crate::db::insert_webhook`], which is the
//! sole source of truth — there is no separate in-memory queue.

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn to_envelope_includes_all_fields() {
        let wh = Webhook::new("t1", "github", "push", serde_json::json!({ "tag": "a" }));
        let env = wh.to_envelope();
        assert_eq!(env["type"], "webhook");
        assert_eq!(env["tenant_id"], "t1");
        assert_eq!(env["plugin_id"], "github");
        assert_eq!(env["event_type"], "push");
        assert_eq!(env["payload"]["tag"], "a");
        assert_eq!(env["id"], wh.id);
    }
}
