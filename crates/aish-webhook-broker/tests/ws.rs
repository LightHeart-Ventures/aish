//! TASK-371 — broker-side WebSocket (`GET /ws`) integration tests.
//!
//! Serves the real axum router on a loopback socket (temp-file SQLite) and
//! speaks the raw JSON frame protocol documented in `src/ws.rs` through the
//! `aish-webhook-client` tungstenite transport — no client-side protocol logic,
//! so these pin the BROKER's behaviour: auth rejection, reconnect backlog replay,
//! ack-by-`webhook_id`, and `/stats` `delivered_ws` accounting (live + backlog).
//! The full client⇄broker happy path lives in `tests/client_contract.rs`.

use std::sync::Arc;
use std::time::{Duration, Instant};

use aish_webhook_broker::config::BrokerConfig;
use aish_webhook_broker::dispatcher::Hub;
use aish_webhook_broker::{db, http, signature};

use aish_webhook_client::transport::TungsteniteTransport;
use aish_webhook_client::{Transport, WsMessage};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt; // oneshot

const TENANT: &str = "t-ws";
const PLUGIN: &str = "github";
const SECRET: &str = "ws-secret";

struct Broker {
    router: axum::Router,
    config: BrokerConfig,
    ws_url: String,
    _dir: tempfile::TempDir,
}

async fn start_broker() -> Broker {
    let dir = tempfile::tempdir().expect("tempdir");
    let pool = db::init(dir.path().join("broker.db").to_str().unwrap()).expect("db init");
    let config = BrokerConfig {
        db: pool,
        hub: Arc::new(Hub::new()),
        start_time: Instant::now(),
        max_queue_size: 100,
        ws_heartbeat_secs: 30,
        poll_timeout_secs: 30,
        msg_ttl_secs: 604_800,
    };
    let router = http::router(config.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let served = router.clone();
    tokio::spawn(async move {
        axum::serve(listener, served).await.unwrap();
    });
    Broker {
        router,
        config,
        ws_url: format!("ws://{addr}/ws"),
        _dir: dir,
    }
}

/// `POST /clients/register` → session token.
async fn register(router: &axum::Router) -> String {
    let body = json!({
        "tenant_id": TENANT,
        "plugin_id": PLUGIN,
        "session_id": "sess-ws",
        "transport": "websocket",
        "secret": SECRET,
    });
    let resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/clients/register")
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let v: Value = serde_json::from_slice(&bytes).unwrap();
    v["session_token"].as_str().unwrap().to_string()
}

/// Signed webhook in through the real router; returns the broker webhook id.
async fn post_webhook(router: &axum::Router, n: u64) -> String {
    let body = json!({ "n": n }).to_string();
    let sig = signature::sign(body.as_bytes(), SECRET);
    let resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/webhooks/{TENANT}/{PLUGIN}"))
                .header("content-type", "application/json")
                .header("x-event-type", "push")
                .header("x-signature", format!("sha256={sig}"))
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let v: Value = serde_json::from_slice(&bytes).unwrap();
    v["id"].as_str().unwrap().to_string()
}

async fn dial(broker: &Broker) -> TungsteniteTransport {
    TungsteniteTransport::connect(&broker.ws_url)
        .await
        .expect("ws connect")
}

async fn send_json(ws: &mut TungsteniteTransport, v: Value) {
    ws.send(WsMessage::Text(v.to_string())).await.unwrap();
}

/// Next JSON text frame (skipping ping/pong), or `None` on close/EOF.
async fn next_json(ws: &mut TungsteniteTransport) -> Option<Value> {
    let deadline = Duration::from_secs(5);
    loop {
        let msg = tokio::time::timeout(deadline, ws.recv())
            .await
            .expect("timed out waiting for a frame");
        match msg {
            Ok(Some(WsMessage::Text(t))) => return Some(serde_json::from_str(&t).unwrap()),
            Ok(Some(WsMessage::Ping(_))) | Ok(Some(WsMessage::Pong(_))) => continue,
            Ok(Some(WsMessage::Close)) | Ok(None) | Err(_) => return None,
        }
    }
}

/// Dial + `{"type":"auth"}` with a valid token; asserts `auth_ok`.
async fn connect_authed(broker: &Broker, token: &str) -> TungsteniteTransport {
    let mut ws = dial(broker).await;
    send_json(&mut ws, json!({"type":"auth","session_token":token})).await;
    let ok = next_json(&mut ws).await.expect("auth reply");
    assert_eq!(ok["type"], "auth_ok", "{ok}");
    assert!(ok["client_id"].as_str().is_some_and(|s| !s.is_empty()));
    ws
}

/// Read `n` webhook envelopes, returning their ids in arrival order.
async fn read_webhooks(ws: &mut TungsteniteTransport, n: usize) -> Vec<String> {
    let mut ids = Vec::with_capacity(n);
    while ids.len() < n {
        let f = next_json(ws).await.expect("webhook frame");
        assert_eq!(f["type"], "webhook", "{f}");
        assert_eq!(f["tenant_id"], TENANT);
        assert_eq!(f["plugin_id"], PLUGIN);
        ids.push(f["id"].as_str().unwrap().to_string());
    }
    ids
}

fn delivered_ws(broker: &Broker) -> u64 {
    broker
        .config
        .hub
        .stats()
        .counters()
        .get(&(TENANT.to_string(), PLUGIN.to_string()))
        .map(|c| c.delivered_ws)
        .unwrap_or(0)
}

fn pending(broker: &Broker) -> i64 {
    db::count_pending(&broker.config.db, TENANT, PLUGIN).unwrap()
}

async fn wait_until(what: &str, mut cond: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !cond() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Expect `auth_error` followed by the server closing the socket.
async fn assert_auth_error(ws: &mut TungsteniteTransport) {
    let f = next_json(ws).await.expect("auth_error frame");
    assert_eq!(f["type"], "auth_error", "{f}");
    assert!(
        next_json(ws).await.is_none(),
        "socket closed after auth_error"
    );
}

// ---------------------------------------------------------------------------
// Auth
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn invalid_session_token_gets_auth_error_and_close() {
    let broker = start_broker().await;
    register(&broker.router).await;
    let mut ws = dial(&broker).await;
    send_json(&mut ws, json!({"type":"auth","session_token":"st_bogus"})).await;
    assert_auth_error(&mut ws).await;
    assert_eq!(broker.config.hub.connected_count(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn missing_session_token_gets_auth_error_and_close() {
    let broker = start_broker().await;
    let mut ws = dial(&broker).await;
    send_json(&mut ws, json!({"type":"auth"})).await;
    assert_auth_error(&mut ws).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn non_auth_first_frame_gets_auth_error() {
    let broker = start_broker().await;
    let token = register(&broker.router).await;
    let mut ws = dial(&broker).await;
    // A valid token in the wrong frame type is still rejected.
    send_json(&mut ws, json!({"type":"ack","session_token":token})).await;
    assert_auth_error(&mut ws).await;
}

// ---------------------------------------------------------------------------
// Delivery: live push, backlog replay, ack
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn backlog_is_replayed_in_order_on_reconnect_and_counted_in_stats() {
    let broker = start_broker().await;
    let token = register(&broker.router).await;

    // Offline: three webhooks queue up; nothing delivered yet.
    let mut sent = Vec::new();
    for n in 0..3 {
        sent.push(post_webhook(&broker.router, n).await);
    }
    assert_eq!(pending(&broker), 3);
    assert_eq!(delivered_ws(&broker), 0);

    // Connect → whole backlog replayed, oldest first, and counted.
    let mut ws = connect_authed(&broker, &token).await;
    assert_eq!(read_webhooks(&mut ws, 3).await, sent);
    wait_until("backlog counted in delivered_ws", || {
        delivered_ws(&broker) == 3
    })
    .await;

    // Drop without acking → still pending → replayed again (at-least-once).
    ws.close().await.unwrap();
    wait_until("disconnect", || broker.config.hub.connected_count() == 0).await;
    assert_eq!(pending(&broker), 3);
    let mut ws = connect_authed(&broker, &token).await;
    assert_eq!(read_webhooks(&mut ws, 3).await, sent);
    wait_until("second replay counted", || delivered_ws(&broker) == 6).await;
    ws.close().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ack_by_webhook_id_removes_only_that_webhook_from_pending() {
    let broker = start_broker().await;
    let token = register(&broker.router).await;
    let mut ws = connect_authed(&broker, &token).await;
    wait_until("ws registered", || broker.config.hub.connected_count() == 1).await;

    // Live push (dispatch fast path).
    let a = post_webhook(&broker.router, 1).await;
    let b = post_webhook(&broker.router, 2).await;
    assert_eq!(read_webhooks(&mut ws, 2).await, vec![a.clone(), b.clone()]);
    assert_eq!(delivered_ws(&broker), 2);
    assert_eq!(pending(&broker), 2);

    // Unknown id and a malformed ack are ignored; connection stays up.
    send_json(&mut ws, json!({"type":"ack","webhook_id":"wh_nope"})).await;
    send_json(&mut ws, json!({"type":"ack"})).await;
    send_json(&mut ws, json!({"type":"ack","webhook_id":a})).await;
    wait_until("ack of a", || pending(&broker) == 1).await;
    assert_eq!(broker.config.hub.connected_count(), 1);

    // Reconnect → only the un-acked webhook is replayed.
    ws.close().await.unwrap();
    wait_until("disconnect", || broker.config.hub.connected_count() == 0).await;
    let mut ws = connect_authed(&broker, &token).await;
    assert_eq!(read_webhooks(&mut ws, 1).await, vec![b.clone()]);
    wait_until("replay counted", || delivered_ws(&broker) == 3).await;

    send_json(&mut ws, json!({"type":"ack","webhook_id":b})).await;
    wait_until("ack of b", || pending(&broker) == 0).await;
    ws.close().await.unwrap();
}
