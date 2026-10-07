//! TASK-449 — client ⇄ broker wire-contract test.
//!
//! Runs THIS crate's real axum router on a loopback socket (temp-file SQLite)
//! and drives it with the REAL `aish-webhook-client`: HTTP registration
//! (`POST /clients/register` via reqwest), the tokio-tungstenite WebSocket
//! transport, the `WebhookService` read → dispatch → ack loop, and the bundled
//! `plugins/hello-world` `ping` handler. Before TASK-449 the two crates spoke
//! different auth/ack frames and only mock-transport tests existed, so the
//! drift was invisible; this test fails if either side changes the contract.

#![cfg(unix)]

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use aish_webhook_broker::config::BrokerConfig;
use aish_webhook_broker::dispatcher::Hub;
use aish_webhook_broker::{db, http, signature};

use aish_webhook_client::transport::TungsteniteTransport;
use aish_webhook_client::{
    BrokerClient, BrokerConfig as ClientConfig, FlashSink, PluginRegistry, StopReason,
    WebhookClientError, WebhookDispatcher, WebhookService,
};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tokio::sync::watch;
use tower::ServiceExt; // oneshot

const TENANT: &str = "t-contract";
const PLUGIN: &str = "hello-world";
const SECRET: &str = "s3cr3t";

struct Broker {
    router: axum::Router,
    pool: db::DbPool,
    ws_url: String,
    _dir: tempfile::TempDir,
}

/// Serve the real router on 127.0.0.1:<ephemeral>.
async fn start_broker() -> Broker {
    let dir = tempfile::tempdir().expect("tempdir");
    let pool = db::init(dir.path().join("broker.db").to_str().unwrap()).expect("db init");
    let config = BrokerConfig {
        db: pool.clone(),
        hub: Arc::new(Hub::new()),
        start_time: Instant::now(),
        max_queue_size: 100,
        ws_heartbeat_secs: 30,
        poll_timeout_secs: 30,
        msg_ttl_secs: 604_800,
    };
    let router = http::router(config);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let served = router.clone();
    tokio::spawn(async move {
        axum::serve(listener, served).await.unwrap();
    });
    Broker {
        router,
        pool,
        ws_url: format!("ws://{addr}/ws"),
        _dir: dir,
    }
}

fn client_config(ws_url: &str, secret: Option<&str>) -> ClientConfig {
    ClientConfig {
        broker_url: ws_url.to_string(),
        tenant_id: TENANT.into(),
        plugin: Some(PLUGIN.into()),
        transport: "websocket".into(),
        enabled: true,
        secret: secret.map(String::from),
        client_id: Some("aish-contract".into()),
    }
}

/// Plugin dir containing only the bundled hello-world plugin (symlinked, so
/// the test exercises the real plugin.json + handlers/ping.sh).
fn hello_world_plugins_dir() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../plugins/hello-world")
        .canonicalize()
        .expect("plugins/hello-world exists");
    std::os::unix::fs::symlink(src, dir.path().join(PLUGIN)).unwrap();
    dir
}

/// External service → broker: signed `ping` webhook through the real router.
async fn post_ping(router: &axum::Router, message: &str) -> StatusCode {
    let body = serde_json::json!({ "message": message }).to_string();
    let sig = signature::sign(body.as_bytes(), SECRET);
    router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/webhooks/{TENANT}/{PLUGIN}"))
                .header("content-type", "application/json")
                .header("x-event-type", "ping")
                .header("x-signature", format!("sha256={sig}"))
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap()
        .status()
}

async fn wait_until(what: &str, timeout: Duration, mut cond: impl FnMut() -> bool) {
    let deadline = Instant::now() + timeout;
    while !cond() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn client_registers_authenticates_receives_flashes_and_acks() {
    let broker = start_broker().await;

    // 1. Register over HTTP → broker-issued session token.
    let mut client = BrokerClient::new(client_config(&broker.ws_url, Some(SECRET)));
    let reg = client.register().await.expect("POST /clients/register");
    assert!(reg.session_token.starts_with("st_"), "{reg:?}");
    assert_eq!(client.session_token(), Some(reg.session_token.as_str()));
    assert_eq!(client.client_id(), reg.client_id);

    // 2. Real WS dial + `{"type":"auth","session_token"}` → auth_ok.
    let transport = TungsteniteTransport::connect(&broker.ws_url)
        .await
        .expect("ws connect");
    client.connect(transport).await.expect("broker auth_ok");
    assert!(client.is_connected());

    // 3. Message loop with the real hello-world plugin + a flash sink.
    let plugins = hello_world_plugins_dir();
    let registry = PluginRegistry::load_dir(plugins.path()).unwrap();
    assert_eq!(
        registry.matching("ping").len(),
        1,
        "hello-world ping handler"
    );
    let flashes: Arc<Mutex<Vec<String>>> = Arc::default();
    let sink_store = flashes.clone();
    let sink: FlashSink = Arc::new(move |s: String| sink_store.lock().unwrap().push(s));
    let dispatcher = Arc::new(WebhookDispatcher::new(Arc::new(registry)).with_flash_sink(sink));
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let mut service = WebhookService::new(client, dispatcher);
    let svc = tokio::spawn(async move { service.run(shutdown_rx).await });

    // 4. Signed webhook in → delivered over WS → ping.sh → flash.
    assert_eq!(
        post_ping(&broker.router, "contract ok").await,
        StatusCode::ACCEPTED
    );
    wait_until("flash from ping.sh", Duration::from_secs(15), || {
        flashes
            .lock()
            .unwrap()
            .iter()
            .any(|s| s.contains("👋 contract ok"))
    })
    .await;

    // 5. `{"type":"ack","webhook_id"}` → broker marks it delivered.
    let pool = broker.pool.clone();
    wait_until("broker to record the ack", Duration::from_secs(10), || {
        db::count_pending(&pool, TENANT, PLUGIN).unwrap() == 0
    })
    .await;

    shutdown_tx.send(true).unwrap();
    let reason = tokio::time::timeout(Duration::from_secs(5), svc)
        .await
        .expect("service stops")
        .unwrap();
    assert_eq!(reason, StopReason::Shutdown);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn queued_webhook_is_drained_on_connect_and_acked() {
    // Webhook arrives while the client is offline → delivered on (re)connect.
    let broker = start_broker().await;
    let mut client = BrokerClient::new(client_config(&broker.ws_url, Some(SECRET)));
    client.register().await.unwrap();
    assert_eq!(
        post_ping(&broker.router, "while offline").await,
        StatusCode::ACCEPTED
    );
    assert_eq!(db::count_pending(&broker.pool, TENANT, PLUGIN).unwrap(), 1);

    let transport = TungsteniteTransport::connect(&broker.ws_url).await.unwrap();
    client.connect(transport).await.unwrap();
    let wh = tokio::time::timeout(Duration::from_secs(5), client.poll_next())
        .await
        .expect("drained webhook")
        .unwrap()
        .expect("webhook frame");
    assert_eq!(wh.event_type, "ping");
    assert_eq!(wh.plugin_id, PLUGIN);
    assert_eq!(wh.payload["message"], "while offline");

    client.ack(&wh.id).await.unwrap();
    let pool = broker.pool.clone();
    wait_until("ack", Duration::from_secs(5), || {
        db::count_pending(&pool, TENANT, PLUGIN).unwrap() == 0
    })
    .await;
    client.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bogus_session_token_is_rejected_as_auth_error() {
    let broker = start_broker().await;
    let mut client =
        BrokerClient::new(client_config(&broker.ws_url, None)).with_session_token("st_bogus");
    let transport = TungsteniteTransport::connect(&broker.ws_url).await.unwrap();
    let started = Instant::now();
    let err = client.connect(transport).await.unwrap_err();
    assert!(
        matches!(err, WebhookClientError::Auth(_)),
        "expected Auth error, got {err:?}"
    );
    // Fails on the broker's auth_error frame, not the client's 10 s timeout.
    assert!(started.elapsed() < Duration::from_secs(5));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn register_without_plugin_id_is_a_config_error() {
    let broker = start_broker().await;
    let mut cfg = client_config(&broker.ws_url, None);
    cfg.plugin = None;
    let err = BrokerClient::<TungsteniteTransport>::new(cfg)
        .register()
        .await
        .unwrap_err();
    assert!(matches!(err, WebhookClientError::Config(_)), "{err:?}");
}
