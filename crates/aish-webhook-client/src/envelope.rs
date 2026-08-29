//! Wire types: broker config, the webhook envelope, and control frames.

use serde::{Deserialize, Serialize};

/// Client-side broker configuration. Loadable from a JSON file via
/// [`BrokerConfig::load`] (a library convenience for embedders). aish's own
/// integration does not use `load()` — it builds this struct from environment
/// variables instead; see `src/webhook.rs::config_from_env` in the parent
/// `aish` crate and the broker's `docs/CLIENT.md`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct BrokerConfig {
    /// e.g. `wss://webhook-broker.example.com/ws`.
    pub broker_url: String,
    /// Tenant this client authenticates as.
    pub tenant_id: String,
    /// Optional plugin scope (broker may fan out per-plugin).
    #[serde(default)]
    pub plugin: Option<String>,
    /// Transport hint; only `"websocket"` is implemented.
    #[serde(default = "default_transport")]
    pub transport: String,
    /// Master enable switch — aish skips broker init when false.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Optional shared secret echoed in the auth frame.
    #[serde(default)]
    pub secret: Option<String>,
    /// Stable client id; generated when absent.
    #[serde(default)]
    pub client_id: Option<String>,
}

fn default_transport() -> String {
    "websocket".to_string()
}
fn default_true() -> bool {
    true
}

impl BrokerConfig {
    /// Load + parse a broker config JSON file. Returns
    /// `WebhookClientError::Config` on a missing/enabled-false file so callers
    /// can treat "no broker" as a soft no-op.
    pub fn load(path: impl AsRef<std::path::Path>) -> crate::error::Result<Self> {
        let raw = std::fs::read_to_string(path)?;
        let cfg: BrokerConfig = serde_json::from_str(&raw)?;
        Ok(cfg)
    }
}

/// A webhook event delivered by the broker to the client.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct Webhook {
    /// Unique delivery id — echoed back in the ack.
    pub id: String,
    #[serde(default)]
    pub tenant_id: String,
    #[serde(default)]
    pub plugin_id: String,
    /// e.g. `"pull_request"`, `"issues"`.
    pub event_type: String,
    /// Raw provider payload.
    #[serde(default)]
    pub payload: serde_json::Value,
}

/// Frames the client sends to the broker.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientFrame<'a> {
    /// Auth/registration handshake.
    Auth {
        tenant_id: &'a str,
        client_id: &'a str,
        #[serde(skip_serializing_if = "Option::is_none")]
        plugin: Option<&'a str>,
        #[serde(skip_serializing_if = "Option::is_none")]
        secret: Option<&'a str>,
    },
    /// Acknowledge a delivered webhook so the broker drops it from its queue.
    Ack { id: &'a str },
    /// Application-level heartbeat response.
    Pong,
}

/// Broker → client control frame classification (best-effort).
#[derive(Debug, Clone, PartialEq)]
pub enum ServerFrame {
    /// A webhook to dispatch.
    Webhook(Webhook),
    /// Auth accepted; carries the assigned session token + client id.
    AuthOk {
        session_token: Option<String>,
        client_id: Option<String>,
    },
    /// Application-level heartbeat request.
    Ping,
    /// Anything else we safely ignore.
    Other,
}

impl ServerFrame {
    /// Parse a text frame. The broker sends bare webhook envelopes (no `type`
    /// tag) as well as tagged control frames, so we sniff both shapes.
    pub fn parse(text: &str) -> crate::error::Result<ServerFrame> {
        let v: serde_json::Value = serde_json::from_str(text)?;
        let ty = v.get("type").and_then(|x| x.as_str());
        match ty {
            Some("webhook") | Some("event") => {
                let w: Webhook = serde_json::from_value(v)?;
                Ok(ServerFrame::Webhook(w))
            }
            Some("ping") => Ok(ServerFrame::Ping),
            Some("auth_ok") | Some("registered") | Some("ack") => Ok(ServerFrame::AuthOk {
                session_token: v
                    .get("session_token")
                    .and_then(|x| x.as_str())
                    .map(String::from),
                client_id: v.get("client_id").and_then(|x| x.as_str()).map(String::from),
            }),
            Some("pong") => Ok(ServerFrame::Other),
            _ => {
                // Untyped: if it looks like a webhook envelope, treat it as one.
                if v.get("event_type").is_some() && v.get("id").is_some() {
                    let w: Webhook = serde_json::from_value(v)?;
                    Ok(ServerFrame::Webhook(w))
                } else {
                    Ok(ServerFrame::Other)
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn webhook_roundtrip() {
        let w = Webhook {
            id: "d1".into(),
            tenant_id: "t1".into(),
            plugin_id: "github".into(),
            event_type: "pull_request".into(),
            payload: serde_json::json!({"action": "opened"}),
        };
        let s = serde_json::to_string(&w).unwrap();
        let back: Webhook = serde_json::from_str(&s).unwrap();
        assert_eq!(w, back);
    }

    #[test]
    fn parse_untyped_webhook_envelope() {
        let txt = r#"{"id":"abc","tenant_id":"t","plugin_id":"github","event_type":"issues","payload":{}}"#;
        match ServerFrame::parse(txt).unwrap() {
            ServerFrame::Webhook(w) => assert_eq!(w.event_type, "issues"),
            other => panic!("expected webhook, got {other:?}"),
        }
    }

    #[test]
    fn parse_auth_ok() {
        let txt = r#"{"type":"auth_ok","session_token":"s123","client_id":"c1"}"#;
        match ServerFrame::parse(txt).unwrap() {
            ServerFrame::AuthOk { session_token, client_id } => {
                assert_eq!(session_token.as_deref(), Some("s123"));
                assert_eq!(client_id.as_deref(), Some("c1"));
            }
            other => panic!("expected auth_ok, got {other:?}"),
        }
    }

    #[test]
    fn parse_ping() {
        assert_eq!(ServerFrame::parse(r#"{"type":"ping"}"#).unwrap(), ServerFrame::Ping);
    }

    #[test]
    fn config_defaults() {
        let cfg: BrokerConfig =
            serde_json::from_str(r#"{"broker_url":"wss://x","tenant_id":"t"}"#).unwrap();
        assert_eq!(cfg.transport, "websocket");
        assert!(cfg.enabled);
        assert!(cfg.plugin.is_none());
    }
}
