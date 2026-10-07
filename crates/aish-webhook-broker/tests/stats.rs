//! TASK-314 (Phase 5D hardening): `GET /stats` integration tests.
//!
//! Drives the real axum `Router` in-process (no sockets) over a temp-file
//! SQLite DB, like `tests/integration.rs`. Kept in its own file so it does not
//! collide with protocol/contract tests landing in `integration.rs`.

use std::sync::Arc;
use std::time::Instant;

use aish_webhook_broker::config::BrokerConfig;
use aish_webhook_broker::dispatcher::Hub;
use aish_webhook_broker::{db, http, signature};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt; // for `oneshot`

/// Router + shared config (hub / DB reachable by the test) over a fresh DB.
fn test_app(max_queue_size: usize) -> (axum::Router, BrokerConfig, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("tempdir");
    let db_path = dir.path().join("broker.db");
    let pool = db::init(db_path.to_str().unwrap()).expect("db init");
    let config = BrokerConfig {
        db: pool,
        hub: Arc::new(Hub::new()),
        start_time: Instant::now(),
        max_queue_size,
        ws_heartbeat_secs: 30,
        poll_timeout_secs: 30,
        msg_ttl_secs: 604_800,
    };
    (http::router(config.clone()), config, dir)
}

async fn send(app: &axum::Router, req: Request<Body>) -> axum::response::Response {
    app.clone().oneshot(req).await.unwrap()
}

async fn body_string(resp: axum::response::Response) -> String {
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    String::from_utf8(bytes.to_vec()).unwrap()
}

async fn body_json(resp: axum::response::Response) -> serde_json::Value {
    serde_json::from_str(&body_string(resp).await).unwrap()
}

/// Register a client; returns the registration response JSON.
async fn register(
    app: &axum::Router,
    tenant: &str,
    plugin: &str,
    session: &str,
    secret: Option<&str>,
) -> serde_json::Value {
    let mut payload = serde_json::json!({
        "tenant_id": tenant,
        "plugin_id": plugin,
        "session_id": session,
        "transport": "poll",
    });
    if let Some(s) = secret {
        payload["secret"] = serde_json::json!(s);
    }
    let resp = send(
        app,
        Request::builder()
            .method("POST")
            .uri("/clients/register")
            .header("content-type", "application/json")
            .body(Body::from(payload.to_string()))
            .unwrap(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED, "register should 201");
    body_json(resp).await
}

async fn post_webhook(app: &axum::Router, tenant: &str, plugin: &str, body: &str) -> StatusCode {
    send(
        app,
        Request::builder()
            .method("POST")
            .uri(format!("/webhooks/{tenant}/{plugin}"))
            .header("content-type", "application/json")
            .header("x-event-type", "push")
            .body(Body::from(body.to_string()))
            .unwrap(),
    )
    .await
    .status()
}

async fn get_stats(app: &axum::Router) -> serde_json::Value {
    let resp = send(
        app,
        Request::builder()
            .uri("/stats")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    body_json(resp).await
}

/// The `plugins[]` entry for (tenant, plugin); panics if absent.
fn entry<'a>(stats: &'a serde_json::Value, tenant: &str, plugin: &str) -> &'a serde_json::Value {
    stats["plugins"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["tenant_id"] == tenant && p["plugin_id"] == plugin)
        .unwrap_or_else(|| panic!("no stats entry for {tenant}/{plugin}: {stats}"))
}

const COUNT_FIELDS: [&str; 8] = [
    "queued",
    "acked",
    "delivered",
    "delivered_ws",
    "delivered_poll",
    "received",
    "dropped",
    "expired",
];

fn assert_totals_are_sum(stats: &serde_json::Value) {
    for f in COUNT_FIELDS {
        let sum: u64 = stats["plugins"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| p[f].as_u64().unwrap())
            .sum();
        assert_eq!(stats["totals"][f].as_u64().unwrap(), sum, "totals.{f}");
    }
}

#[tokio::test]
async fn stats_empty_broker() {
    let (app, _cfg, _dir) = test_app(100);
    let s = get_stats(&app).await;
    assert_eq!(s["plugins"], serde_json::json!([]));
    for f in COUNT_FIELDS {
        assert_eq!(s["totals"][f], 0, "totals.{f}");
    }
    assert!(s["generated_at"].is_string());
    assert!(s["counters_since"].is_string());
    assert!(s["uptime_secs"].is_u64());
}

#[tokio::test]
async fn stats_lifecycle_counts() {
    let (app, _cfg, _dir) = test_app(100);
    register(&app, "acme", "github", "sess-1", None).await;
    for i in 0..3 {
        assert_eq!(
            post_webhook(&app, "acme", "github", &format!(r#"{{"n":{i}}}"#)).await,
            StatusCode::ACCEPTED
        );
    }
    let s = get_stats(&app).await;
    let e = entry(&s, "acme", "github");
    assert_eq!(e["received"], 3);
    assert_eq!(e["queued"], 3);
    assert_eq!(e["delivered"], 0);

    // Poll → 3 envelopes handed out.
    let resp = send(
        &app,
        Request::builder()
            .uri("/webhooks/acme/github/pending")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    let j = body_json(resp).await;
    assert_eq!(j["messages"].as_array().unwrap().len(), 3);
    let id = j["messages"][0]["id"].as_str().unwrap().to_string();

    // ACK one.
    let resp = send(
        &app,
        Request::builder()
            .method("DELETE")
            .uri(format!("/webhooks/acme/github/messages/{id}"))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    let s = get_stats(&app).await;
    let e = entry(&s, "acme", "github");
    assert_eq!(e["received"], 3);
    assert_eq!(e["delivered_poll"], 3);
    assert_eq!(e["delivered_ws"], 0);
    assert_eq!(e["delivered"], 3);
    assert_eq!(e["queued"], 2);
    assert_eq!(e["acked"], 1);
    assert_eq!(e["dropped"], 0);
    assert_eq!(e["expired"], 0);
    assert_totals_are_sum(&s);
}

#[tokio::test]
async fn stats_ws_fast_path_counts_delivered_ws() {
    let (app, cfg, _dir) = test_app(100);
    register(&app, "acme", "github", "sess-1", None).await;
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    cfg.hub.register_ws("st_test", "acme", "github", tx);

    assert_eq!(
        post_webhook(&app, "acme", "github", "{}").await,
        StatusCode::ACCEPTED
    );
    assert!(rx.try_recv().is_ok(), "envelope pushed to the live client");

    let s = get_stats(&app).await;
    let e = entry(&s, "acme", "github");
    assert_eq!(e["delivered_ws"], 1);
    assert_eq!(e["delivered"], 1);
    // Pushed but not acked → still durably queued.
    assert_eq!(e["queued"], 1);
    assert_eq!(e["acked"], 0);
}

#[tokio::test]
async fn stats_overflow_counts_dropped() {
    let (app, _cfg, _dir) = test_app(2);
    register(&app, "acme", "github", "sess-1", None).await;
    for _ in 0..5 {
        assert_eq!(
            post_webhook(&app, "acme", "github", "{}").await,
            StatusCode::ACCEPTED
        );
    }
    let s = get_stats(&app).await;
    let e = entry(&s, "acme", "github");
    assert_eq!(e["received"], 5);
    assert_eq!(e["queued"], 2);
    assert_eq!(e["dropped"], 3);
    assert_totals_are_sum(&s);
}

#[tokio::test]
async fn stats_ttl_sweep_counts_expired_undelivered_only() {
    let (app, cfg, _dir) = test_app(100);
    register(&app, "acme", "github", "sess-1", None).await;
    for _ in 0..3 {
        post_webhook(&app, "acme", "github", "{}").await;
    }
    // Ack one so the sweep removes 2 undelivered + 1 acked row.
    let pending = db::fetch_pending(&cfg.db, "acme", "github", 10).unwrap();
    assert!(db::mark_delivered(&cfg.db, "acme", "github", &pending[0].id, None).unwrap());

    // A sweep "now" removes nothing (7-day TTL).
    assert!(db::ttl_cleanup(&cfg.db).unwrap().is_empty());

    // Sweep as if the TTL had elapsed; record exactly as main.rs's sweep does.
    let far_future = chrono::Utc::now() + chrono::Duration::days(30);
    let expired = db::ttl_cleanup_at(&cfg.db, far_future).unwrap();
    assert_eq!(expired.len(), 1);
    assert_eq!(expired[0].undelivered, 2);
    assert_eq!(expired[0].acked, 1);
    for e in &expired {
        cfg.hub
            .stats()
            .record_expired(&e.tenant_id, &e.plugin_id, e.undelivered);
    }

    let s = get_stats(&app).await;
    let e = entry(&s, "acme", "github");
    assert_eq!(e["expired"], 2, "acked rows ageing out are not 'expired'");
    assert_eq!(e["queued"], 0);
    assert_eq!(e["acked"], 0);
    assert_eq!(e["received"], 3);
}

#[tokio::test]
async fn stats_isolated_per_key_and_rejections_not_counted() {
    let (app, _cfg, _dir) = test_app(100);
    register(&app, "acme", "github", "sess-1", None).await;
    register(&app, "acme", "secure", "sess-2", Some("s3cret")).await;
    register(&app, "globex", "github", "sess-3", None).await;

    post_webhook(&app, "acme", "github", "{}").await;
    post_webhook(&app, "acme", "github", "{}").await;
    post_webhook(&app, "globex", "github", "{}").await;

    // Rejections: unknown key (404), missing / bad signature (401), bad JSON (400).
    assert_eq!(
        post_webhook(&app, "nobody", "nothing", "{}").await,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        post_webhook(&app, "acme", "secure", "{}").await,
        StatusCode::UNAUTHORIZED
    );
    let resp = send(
        &app,
        Request::builder()
            .method("POST")
            .uri("/webhooks/acme/secure")
            .header("x-signature", "sha256=deadbeef")
            .body(Body::from("{}"))
            .unwrap(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(
        post_webhook(&app, "acme", "github", "not json").await,
        StatusCode::BAD_REQUEST
    );

    let s = get_stats(&app).await;
    assert_eq!(entry(&s, "acme", "github")["received"], 2);
    assert_eq!(entry(&s, "acme", "github")["queued"], 2);
    assert_eq!(entry(&s, "globex", "github")["received"], 1);
    assert_eq!(entry(&s, "globex", "github")["queued"], 1);
    assert_eq!(entry(&s, "acme", "secure")["received"], 0);
    assert_eq!(entry(&s, "acme", "secure")["queued"], 0);
    assert!(
        !s["plugins"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p["tenant_id"] == "nobody"),
        "rejected unknown key must not appear"
    );
    assert_eq!(s["totals"]["received"], 3);
    assert_totals_are_sum(&s);

    // Sorted by (tenant_id, plugin_id).
    let keys: Vec<(String, String)> = s["plugins"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| {
            (
                p["tenant_id"].as_str().unwrap().to_string(),
                p["plugin_id"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    let mut sorted = keys.clone();
    sorted.sort();
    assert_eq!(keys, sorted);
}

#[tokio::test]
async fn stats_registered_idle_key_listed_with_zeros() {
    let (app, _cfg, _dir) = test_app(100);
    register(&app, "acme", "idle", "sess-1", None).await;
    let s = get_stats(&app).await;
    let e = entry(&s, "acme", "idle");
    for f in COUNT_FIELDS {
        assert_eq!(e[f], 0, "{f}");
    }
}

#[tokio::test]
async fn stats_never_leaks_payload_or_secrets() {
    let (app, _cfg, _dir) = test_app(100);
    let reg = register(
        &app,
        "acme",
        "secure",
        "sess-leakcheck",
        Some("SUPER-SECRET-VALUE"),
    )
    .await;
    let client_id = reg["client_id"].as_str().unwrap().to_string();
    let token = reg["session_token"].as_str().unwrap().to_string();

    let body = r#"{"marker":"PAYLOAD-MARKER-42"}"#;
    let sig = signature::sign(body.as_bytes(), "SUPER-SECRET-VALUE");
    let resp = send(
        &app,
        Request::builder()
            .method("POST")
            .uri("/webhooks/acme/secure")
            .header("x-signature", format!("sha256={sig}"))
            .header("x-event-type", "EVENT-TYPE-MARKER")
            .body(Body::from(body))
            .unwrap(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    let webhook_id = body_json(resp).await["id"].as_str().unwrap().to_string();

    let resp = send(
        &app,
        Request::builder()
            .uri("/stats")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let raw = body_string(resp).await;
    for needle in [
        "PAYLOAD-MARKER-42",
        "EVENT-TYPE-MARKER",
        "SUPER-SECRET-VALUE",
        "sess-leakcheck",
        client_id.as_str(),
        token.as_str(),
        webhook_id.as_str(),
        "secret",
        "session",
        "payload",
    ] {
        assert!(!raw.contains(needle), "/stats leaked {needle:?}: {raw}");
    }
    // ...but the count is there.
    let s: serde_json::Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(entry(&s, "acme", "secure")["received"], 1);
}

#[tokio::test]
async fn stats_is_unauthenticated_and_get_only() {
    let (app, _cfg, _dir) = test_app(100);
    // No auth header needed, like /health.
    get_stats(&app).await;
    let resp = send(
        &app,
        Request::builder()
            .method("POST")
            .uri("/stats")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::METHOD_NOT_ALLOWED);
}
