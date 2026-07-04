//! HTTP REST API endpoints.

use axum::{
    body::Bytes,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    routing::{delete, get, post},
    Json, Router,
};
use serde::Deserialize;
use serde_json::json;
use tracing::{info, warn};

use crate::config::BrokerConfig;
use crate::db;
use crate::error::{BrokerError, Result};
use crate::queue::Webhook;
use crate::signature;
use crate::ws;

/// Build the HTTP router with all endpoints.
pub fn router(config: BrokerConfig) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/ws", get(ws::ws_handler))
        .route("/clients/register", post(register_client))
        .route("/webhooks/:tenant_id/:plugin_id", post(receive_webhook))
        .route(
            "/webhooks/:tenant_id/:plugin_id/pending",
            get(poll_pending),
        )
        .route(
            "/webhooks/:tenant_id/:plugin_id/messages/:webhook_id",
            delete(ack_webhook),
        )
        .with_state(config)
}

/// Health check endpoint.
async fn health(State(config): State<BrokerConfig>) -> impl IntoResponse {
    let (queued, db_health) = match db::total_pending(&config.db) {
        Ok(n) => (n, "ok"),
        Err(_) => (0, "error"),
    };
    let response = json!({
        "status": "ok",
        "uptime_secs": config.start_time.elapsed().as_secs(),
        "connected_clients": config.hub.connected_count(),
        "queued_messages": queued,
        "db_health": db_health,
    });
    (StatusCode::OK, Json(response))
}

#[derive(Debug, Deserialize)]
struct RegisterRequest {
    tenant_id: String,
    plugin_id: String,
    session_id: String,
    #[serde(default)]
    transport: Option<String>,
    #[serde(default)]
    secret: Option<String>,
}

/// Register a new aish client.
async fn register_client(
    State(config): State<BrokerConfig>,
    Json(req): Json<RegisterRequest>,
) -> Result<impl IntoResponse> {
    if req.tenant_id.is_empty() || req.plugin_id.is_empty() || req.session_id.is_empty() {
        return Err(BrokerError::InvalidJson(
            "tenant_id, plugin_id, and session_id are required".to_string(),
        ));
    }
    let transport = req.transport.as_deref().unwrap_or("websocket");
    let client = db::register_client(
        &config.db,
        &req.tenant_id,
        &req.plugin_id,
        &req.session_id,
        transport,
        req.secret.as_deref(),
    )?;

    info!(
        tenant = %req.tenant_id,
        plugin = %req.plugin_id,
        "client registered: {}",
        client.client_id
    );

    let response = json!({
        "client_id": client.client_id,
        "session_token": client.session_token,
        "ws_path": "/ws",
        "poll_path": format!("/webhooks/{}/{}/pending", req.tenant_id, req.plugin_id),
        "transport": transport,
        "registered_at": chrono::Utc::now().to_rfc3339(),
    });
    Ok((StatusCode::CREATED, Json(response)))
}

/// Receive a webhook from an external service.
async fn receive_webhook(
    State(config): State<BrokerConfig>,
    Path((tenant_id, plugin_id)): Path<(String, String)>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<impl IntoResponse> {
    // 1. Reject unknown (tenant, plugin) with 404.
    if !db::tenant_plugin_exists(&config.db, &tenant_id, &plugin_id)? {
        warn!(tenant = %tenant_id, plugin = %plugin_id, "webhook for unknown tenant/plugin");
        return Err(BrokerError::UnknownTenant);
    }

    // 2. Verify signature if a secret was registered for this (tenant, plugin).
    if let Some(secret) = db::get_secret(&config.db, &tenant_id, &plugin_id)? {
        let sig = header_value(&headers, &["x-signature", "x-hub-signature-256"])
            .ok_or(BrokerError::InvalidSignature)?;
        signature::verify_signature(&body, &sig, &secret)?;
    }

    // 3. Determine the event type from a header, falling back to the payload.
    let event_type = header_value(
        &headers,
        &["x-event-type", "x-github-event", "x-gitlab-event"],
    )
    .unwrap_or_default();

    // 4. Parse the JSON payload.
    let payload: serde_json::Value = if body.is_empty() {
        json!({})
    } else {
        serde_json::from_slice(&body).map_err(|e| BrokerError::InvalidJson(e.to_string()))?
    };

    let event_type = if !event_type.is_empty() {
        event_type
    } else {
        payload
            .get("action")
            .and_then(|v| v.as_str())
            .or_else(|| payload.get("event").and_then(|v| v.as_str()))
            .unwrap_or("unknown")
            .to_string()
    };

    // 5. Persist (durable) — enforces the per-tenant queue cap.
    let webhook = Webhook::new(tenant_id.clone(), plugin_id.clone(), event_type, payload);
    db::insert_webhook(
        &config.db,
        &webhook,
        config.msg_ttl_secs,
        config.max_queue_size,
    )?;

    // 6. Fast-path dispatch to connected WS clients + wake long-pollers.
    let envelope = webhook.to_envelope().to_string();
    let delivered = config.hub.dispatch(&tenant_id, &plugin_id, &envelope);

    info!(
        tenant = %tenant_id,
        plugin = %plugin_id,
        webhook = %webhook.id,
        ws_delivered = delivered,
        "webhook received"
    );

    Ok((
        StatusCode::ACCEPTED,
        Json(json!({ "id": webhook.id, "status": "queued" })),
    ))
}

#[derive(Debug, Deserialize)]
struct PollParams {
    #[serde(default)]
    #[allow(dead_code)]
    session_id: Option<String>,
    #[serde(default)]
    wait_secs: Option<u64>,
    #[serde(default)]
    limit: Option<usize>,
}

/// Poll for pending messages (long-poll fallback).
async fn poll_pending(
    State(config): State<BrokerConfig>,
    Path((tenant_id, plugin_id)): Path<(String, String)>,
    Query(params): Query<PollParams>,
) -> Result<impl IntoResponse> {
    let limit = params.limit.unwrap_or(100).clamp(1, 500);
    let wait_secs = params.wait_secs.unwrap_or(0);

    // First read.
    let mut pending = db::fetch_pending(&config.db, &tenant_id, &plugin_id, limit)?;

    // Park for new messages if empty and the client asked to wait.
    if pending.is_empty() && wait_secs > 0 {
        crate::poll::wait_for_message(&config, &tenant_id, &plugin_id, wait_secs).await;
        pending = db::fetch_pending(&config.db, &tenant_id, &plugin_id, limit)?;
    }

    let messages: Vec<serde_json::Value> = pending.iter().map(|w| w.to_envelope()).collect();
    let remaining = db::count_pending(&config.db, &tenant_id, &plugin_id)?;

    Ok((
        StatusCode::OK,
        Json(json!({
            "messages": messages,
            "remaining_queue_size": remaining,
        })),
    ))
}

/// ACK a webhook (mark as delivered).
async fn ack_webhook(
    State(config): State<BrokerConfig>,
    Path((tenant_id, plugin_id, webhook_id)): Path<(String, String, String)>,
) -> Result<impl IntoResponse> {
    let updated = db::mark_delivered(&config.db, &tenant_id, &plugin_id, &webhook_id, None)?;
    if updated {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(BrokerError::NotFound)
    }
}

/// Return the first present header from `names`, lowercased match, as a String.
fn header_value(headers: &HeaderMap, names: &[&str]) -> Option<String> {
    for name in names {
        if let Some(v) = headers.get(*name) {
            if let Ok(s) = v.to_str() {
                return Some(s.to_string());
            }
        }
    }
    None
}
