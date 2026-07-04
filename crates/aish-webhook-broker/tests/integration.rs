//! End-to-end HTTP integration tests for the webhook broker.
//!
//! These drive the real axum `Router` in-process via `tower::ServiceExt::oneshot`
//! (no network sockets), backed by a temp-file SQLite database, so they exercise
//! the full request path: routing → handler → db → dispatch hub.

use std::sync::Arc;
use std::time::Instant;

use aish_webhook_broker::config::BrokerConfig;
use aish_webhook_broker::dispatcher::Hub;
use aish_webhook_broker::{db, http, signature};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt; // for `oneshot`

/// Build a router backed by a fresh temp-file DB. Returns the router plus the
/// tempdir guard (kept alive for the test's duration).
fn test_app() -> (axum::Router, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("tempdir");
    let db_path = dir.path().join("broker.db");
    let pool = db::init(db_path.to_str().unwrap()).expect("db init");
    let config = BrokerConfig {
        db: pool,
        hub: Arc::new(Hub::new()),
        start_time: Instant::now(),
        max_queue_size: 100,
        ws_heartbeat_secs: 30,
        poll_timeout_secs: 30,
        msg_ttl_secs: 604_800,
    };
    (http::router(config), dir)
}

async fn body_json(resp: axum::response::Response) -> serde_json::Value {
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    if bytes.is_empty() {
        return serde_json::Value::Null;
    }
    serde_json::from_slice(&bytes).unwrap()
}

async fn register(app: &axum::Router, tenant: &str, plugin: &str, secret: Option<&str>) {
    let mut payload = serde_json::json!({
        "tenant_id": tenant,
        "plugin_id": plugin,
        "session_id": "sess-1",
        "transport": "poll",
    });
    if let Some(s) = secret {
        payload["secret"] = serde_json::json!(s);
    }
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/clients/register")
                .header("content-type", "application/json")
                .body(Body::from(payload.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED, "register should 201");
}

#[tokio::test]
async fn health_reports_ok() {
    let (app, _dir) = test_app();
    let resp = app
        .oneshot(Request::builder().uri("/health").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let j = body_json(resp).await;
    assert_eq!(j["status"], "ok");
    assert_eq!(j["db_health"], "ok");
}

#[tokio::test]
async fn full_lifecycle_register_receive_poll_ack() {
    let (app, _dir) = test_app();
    register(&app, "acme", "github", None).await;

    // Receive a webhook.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/webhooks/acme/github")
                .header("content-type", "application/json")
                .header("x-event-type", "push")
                .body(Body::from(r#"{"ref":"refs/heads/main"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    let j = body_json(resp).await;
    let webhook_id = j["id"].as_str().unwrap().to_string();
    assert_eq!(j["status"], "queued");

    // Poll pending — should return the one message.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/webhooks/acme/github/pending")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let j = body_json(resp).await;
    let messages = j["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0]["event_type"], "push");

    // ACK it.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri(format!("/webhooks/acme/github/messages/{webhook_id}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    // Poll again — queue now empty.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/webhooks/acme/github/pending")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let j = body_json(resp).await;
    assert_eq!(j["messages"].as_array().unwrap().len(), 0);
    assert_eq!(j["remaining_queue_size"], 0);
}

#[tokio::test]
async fn unknown_tenant_plugin_returns_404() {
    let (app, _dir) = test_app();
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/webhooks/nobody/nothing")
                .header("content-type", "application/json")
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn signature_required_when_secret_registered() {
    let (app, _dir) = test_app();
    let secret = "topsecret";
    register(&app, "acme", "secure", Some(secret)).await;
    let body = r#"{"hello":"world"}"#;

    // Missing signature → 401.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/webhooks/acme/secure")
                .header("content-type", "application/json")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    // Correct signature → 202.
    let sig = signature::sign(body.as_bytes(), secret);
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/webhooks/acme/secure")
                .header("content-type", "application/json")
                .header("x-signature", format!("sha256={sig}"))
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
}
