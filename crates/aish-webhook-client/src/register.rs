//! Client registration — `POST /clients/register` (TASK-449).
//!
//! The broker authenticates WebSocket clients ONLY by a `session_token` that it
//! issues from its HTTP registration endpoint. So before dialing `/ws` the
//! client registers its `(tenant_id, plugin_id)` route over HTTP(S):
//!
//! ```text
//! POST {http(s)://host}/clients/register
//!   {"tenant_id","plugin_id","session_id","transport":"websocket","secret"?}
//! → 201 {"client_id","session_token":"st_…","ws_path":"/ws", …}
//! ```
//!
//! then sends `{"type":"auth","session_token":"st_…"}` as the first WS frame.
//! The request/response types and URL derivation are always compiled (and unit
//! tested); the actual HTTP call needs the `net` feature.

use serde::{Deserialize, Serialize};

use crate::envelope::BrokerConfig;
use crate::error::{Result, WebhookClientError};

/// Body of `POST /clients/register`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RegisterRequest {
    pub tenant_id: String,
    pub plugin_id: String,
    /// Opaque per-client session id; aish uses its `client_id`.
    pub session_id: String,
    pub transport: String,
    /// Shared secret: the broker then requires an HMAC `X-Signature` on
    /// inbound webhooks for this `(tenant_id, plugin_id)`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub secret: Option<String>,
}

impl RegisterRequest {
    /// Build the registration body from a client config. `plugin` is required
    /// because the broker routes by `(tenant_id, plugin_id)`.
    pub fn from_config(cfg: &BrokerConfig, session_id: &str) -> Result<Self> {
        let plugin_id = cfg
            .plugin
            .as_deref()
            .map(str::trim)
            .filter(|p| !p.is_empty())
            .ok_or_else(|| {
                WebhookClientError::Config(
                    "broker registration needs a plugin id (WEBHOOK_PLUGIN_ID)".into(),
                )
            })?;
        if cfg.tenant_id.trim().is_empty() {
            return Err(WebhookClientError::Config(
                "broker registration needs a tenant id".into(),
            ));
        }
        Ok(Self {
            tenant_id: cfg.tenant_id.clone(),
            plugin_id: plugin_id.to_string(),
            session_id: session_id.to_string(),
            transport: "websocket".to_string(),
            secret: cfg.secret.clone().filter(|s| !s.is_empty()),
        })
    }
}

/// `201 Created` body of `POST /clients/register` (extra fields ignored).
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct RegisterResponse {
    pub client_id: String,
    pub session_token: String,
    #[serde(default)]
    pub ws_path: Option<String>,
}

/// Derive the registration URL from the broker WebSocket URL:
/// `wss://h/ws` → `https://h/clients/register`, `ws://h:p/ws` →
/// `http://h:p/clients/register`. A path prefix before `/ws` is kept
/// (`wss://h/broker/ws` → `https://h/broker/clients/register`); query strings
/// and fragments are dropped. `http(s)://` base URLs are accepted as-is.
pub fn register_url(broker_url: &str) -> Result<String> {
    let url = broker_url.trim();
    let (scheme, rest) = url
        .split_once("://")
        .ok_or_else(|| WebhookClientError::Config(format!("invalid broker URL: {url}")))?;
    let http_scheme = match scheme.to_ascii_lowercase().as_str() {
        "ws" | "http" => "http",
        "wss" | "https" => "https",
        other => {
            return Err(WebhookClientError::Config(format!(
                "unsupported broker URL scheme `{other}` (expected ws/wss/http/https)"
            )))
        }
    };
    let rest = rest.split(['?', '#']).next().unwrap_or_default();
    let (host, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, ""),
    };
    if host.is_empty() {
        return Err(WebhookClientError::Config(format!(
            "broker URL has no host: {url}"
        )));
    }
    let mut prefix = path.trim_end_matches('/');
    if let Some(p) = prefix.strip_suffix("/ws") {
        prefix = p;
    }
    Ok(format!("{http_scheme}://{host}{prefix}/clients/register"))
}

/// Register with the broker over HTTP(S) and return the issued session token.
#[cfg(feature = "net")]
pub async fn register(cfg: &BrokerConfig, session_id: &str) -> Result<RegisterResponse> {
    let url = register_url(&cfg.broker_url)?;
    let body = RegisterRequest::from_config(cfg, session_id)?;
    let http = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .map_err(|e| WebhookClientError::Registration(e.to_string()))?;
    let resp = http
        .post(&url)
        .json(&body)
        .send()
        .await
        .map_err(|e| WebhookClientError::Registration(format!("POST {url}: {e}")))?;
    let status = resp.status();
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        return Err(WebhookClientError::Registration(format!(
            "POST {url}: HTTP {status}: {text}"
        )));
    }
    resp.json::<RegisterResponse>()
        .await
        .map_err(|e| WebhookClientError::Registration(format!("bad register response: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(plugin: Option<&str>, secret: Option<&str>) -> BrokerConfig {
        BrokerConfig {
            broker_url: "wss://broker.example/ws".into(),
            tenant_id: "acme".into(),
            plugin: plugin.map(String::from),
            transport: "websocket".into(),
            enabled: true,
            secret: secret.map(String::from),
            client_id: None,
        }
    }

    #[test]
    fn register_url_maps_ws_schemes_to_http() {
        assert_eq!(
            register_url("wss://aish-webhook-broker.fly.dev/ws").unwrap(),
            "https://aish-webhook-broker.fly.dev/clients/register"
        );
        assert_eq!(
            register_url("ws://127.0.0.1:8080/ws").unwrap(),
            "http://127.0.0.1:8080/clients/register"
        );
        assert_eq!(
            register_url("ws://127.0.0.1:8080").unwrap(),
            "http://127.0.0.1:8080/clients/register"
        );
        assert_eq!(
            register_url("https://h.example/").unwrap(),
            "https://h.example/clients/register"
        );
    }

    #[test]
    fn register_url_keeps_prefix_and_drops_query() {
        assert_eq!(
            register_url("wss://h.example/broker/ws/?x=1#f").unwrap(),
            "https://h.example/broker/clients/register"
        );
    }

    #[test]
    fn register_url_rejects_garbage() {
        assert!(register_url("broker.example/ws").is_err());
        assert!(register_url("ftp://h/ws").is_err());
        assert!(register_url("wss:///ws").is_err());
    }

    #[test]
    fn request_requires_plugin_and_maps_secret() {
        assert!(matches!(
            RegisterRequest::from_config(&cfg(None, None), "s"),
            Err(WebhookClientError::Config(_))
        ));
        assert!(RegisterRequest::from_config(&cfg(Some("  "), None), "s").is_err());

        let req =
            RegisterRequest::from_config(&cfg(Some("hello-world"), Some("k")), "sess").unwrap();
        assert_eq!(
            serde_json::to_value(&req).unwrap(),
            serde_json::json!({
                "tenant_id": "acme",
                "plugin_id": "hello-world",
                "session_id": "sess",
                "transport": "websocket",
                "secret": "k",
            })
        );
        // No secret ⇒ key omitted (broker then accepts unsigned webhooks).
        let req = RegisterRequest::from_config(&cfg(Some("hello-world"), None), "s").unwrap();
        assert!(serde_json::to_value(&req).unwrap().get("secret").is_none());
    }

    #[test]
    fn response_parses_broker_201_body() {
        let body = r#"{"client_id":"c_1","session_token":"st_x","ws_path":"/ws","poll_path":"/p","transport":"websocket","registered_at":"t"}"#;
        let r: RegisterResponse = serde_json::from_str(body).unwrap();
        assert_eq!(r.session_token, "st_x");
        assert_eq!(r.client_id, "c_1");
        assert_eq!(r.ws_path.as_deref(), Some("/ws"));
    }
}
