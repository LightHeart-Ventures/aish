//! WebSocket server for real-time webhook delivery.
//!
//! Protocol (JSON text frames):
//!   client → server, first frame:  {"type":"auth","session_token":"st_..."}
//!   server → client, on success:   {"type":"auth_ok","client_id":"..."}
//!   server → client, per webhook:  {"type":"webhook","id":"wh_...", ...}
//!   client → server, ack:          {"type":"ack","webhook_id":"wh_..."}
//!   either direction:              ping / pong (WebSocket control frames)

use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        State,
    },
    response::IntoResponse,
};
use futures_util::{SinkExt, StreamExt};
use serde_json::json;
use tracing::{debug, info, warn};

use crate::config::BrokerConfig;
use crate::db;

/// Axum handler: upgrade the connection and hand off to [`handle_socket`].
pub async fn ws_handler(
    ws: WebSocketUpgrade,
    State(config): State<BrokerConfig>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_socket(socket, config))
}

async fn handle_socket(socket: WebSocket, config: BrokerConfig) {
    let (mut sender, mut receiver) = socket.split();

    // 1. Authenticate: the first frame must be an `auth` message.
    let client = match authenticate(&mut receiver, &config).await {
        Some(c) => c,
        None => {
            let _ = sender
                .send(Message::Text(
                    json!({"type":"auth_error","error":"authentication failed"}).to_string(),
                ))
                .await;
            let _ = sender.send(Message::Close(None)).await;
            return;
        }
    };

    let _ = sender
        .send(Message::Text(
            json!({"type":"auth_ok","client_id": client.client_id}).to_string(),
        ))
        .await;

    // 2. Register a push channel with the hub.
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    config
        .hub
        .register_ws(&client.session_token, &client.tenant_id, &client.plugin_id, tx);
    info!(
        client = %client.client_id,
        tenant = %client.tenant_id,
        plugin = %client.plugin_id,
        "WebSocket client connected"
    );

    // 3. Drain any already-queued webhooks for this client.
    if let Ok(pending) = db::fetch_pending(&config.db, &client.tenant_id, &client.plugin_id, 500) {
        for wh in pending {
            if sender
                .send(Message::Text(wh.to_envelope().to_string()))
                .await
                .is_err()
            {
                config.hub.unregister_ws(&client.session_token);
                return;
            }
        }
    }

    // 4. Concurrent loop: forward pushes, handle incoming acks/pings, heartbeat.
    let mut heartbeat =
        tokio::time::interval(std::time::Duration::from_secs(config.ws_heartbeat_secs.max(1)));
    heartbeat.tick().await; // consume the immediate first tick

    loop {
        tokio::select! {
            // Push a dispatched webhook to the client.
            maybe_msg = rx.recv() => {
                match maybe_msg {
                    Some(text) => {
                        if sender.send(Message::Text(text)).await.is_err() {
                            break;
                        }
                    }
                    None => break,
                }
            }

            // Handle an inbound frame from the client.
            maybe_frame = receiver.next() => {
                match maybe_frame {
                    Some(Ok(Message::Text(text))) => {
                        handle_client_frame(&text, &config, &client).await;
                    }
                    Some(Ok(Message::Ping(p))) => {
                        let _ = sender.send(Message::Pong(p)).await;
                    }
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Err(e)) => {
                        warn!(client = %client.client_id, "ws error: {}", e);
                        break;
                    }
                    _ => {}
                }
            }

            // Server-initiated heartbeat.
            _ = heartbeat.tick() => {
                if sender.send(Message::Ping(Vec::new())).await.is_err() {
                    break;
                }
            }
        }
    }

    config.hub.unregister_ws(&client.session_token);
    info!(client = %client.client_id, "WebSocket client disconnected");
}

/// Read + validate the opening `auth` frame. Returns the authenticated client.
async fn authenticate(
    receiver: &mut futures_util::stream::SplitStream<WebSocket>,
    config: &BrokerConfig,
) -> Option<db::ClientRow> {
    // Wait (bounded) for the first text frame.
    let frame = tokio::time::timeout(std::time::Duration::from_secs(10), receiver.next())
        .await
        .ok()??;
    let text = match frame.ok()? {
        Message::Text(t) => t,
        _ => return None,
    };
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    if v.get("type").and_then(|t| t.as_str()) != Some("auth") {
        return None;
    }
    let token = v.get("session_token").and_then(|t| t.as_str())?;
    match db::validate_session(&config.db, token) {
        Ok(Some(client)) => Some(client),
        _ => None,
    }
}

/// Handle a text frame from a connected client (currently: `ack`).
async fn handle_client_frame(text: &str, config: &BrokerConfig, client: &db::ClientRow) {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(text) else {
        return;
    };
    match v.get("type").and_then(|t| t.as_str()) {
        Some("ack") => {
            if let Some(webhook_id) = v.get("webhook_id").and_then(|w| w.as_str()) {
                let _ = db::mark_delivered(
                    &config.db,
                    &client.tenant_id,
                    &client.plugin_id,
                    webhook_id,
                    Some(&client.client_id),
                );
                debug!(client = %client.client_id, webhook = webhook_id, "ack");
            }
        }
        Some("ping") => {} // application-level ping; heartbeat covers liveness
        _ => {}
    }
}
