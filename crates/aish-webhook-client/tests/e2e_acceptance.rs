//! TASK-389 (SPR-069) — End-to-end acceptance for the webhook client.
//!
//! Exercises the full client-side delivery path with the crate's hermetic
//! [`mock_transport`] (no sockets): broker ingress → message-loop dispatch →
//! fork/exec of a real argv handler → stdout/stderr capture → durable audit
//! record. Failure isolation is proven by dispatching a succeeding *and* a
//! failing handler for the same event and asserting the batch survives.
//!
//! The broker-side legs of the SPR-069 acceptance criteria — HMAC signature
//! rejection and the `/health` endpoint — are covered by the broker crate's
//! own unit tests (`aish-webhook-broker::signature`:
//! `invalid_signature_rejected`, `wrong_secret_rejected`, `tamper_detection`;
//! and the `http::health` handler). This suite owns the client half.
//!
//! Hermetic: builds and runs under `--no-default-features` (no `net`, no
//! sockets). The only external dependency is `/bin/cat`, present on every
//! Linux/macOS runner.

use std::sync::Arc;
use std::time::Duration;

use aish_webhook_client::transport::mock_transport;
use aish_webhook_client::{
    BrokerClient, BrokerConfig, MemoryAuditSink, PluginManifest, PluginRegistry, StopReason,
    Webhook, WebhookDispatcher, WebhookHandler, WebhookService, WsMessage,
};
use tokio::sync::watch;

/// A manifest with two subscribers to `pull_request`:
/// * `echo`   — `/bin/cat` streams the JSON payload from stdin back to stdout.
/// * `broken` — `/bin/cat <missing-file>` writes to stderr and exits non-zero.
fn registry() -> PluginRegistry {
    PluginRegistry::from_plugins(vec![
        PluginManifest {
            id: "echo".into(),
            name: "Echo".into(),
            version: "0.0.0".into(),
            webhooks: vec![WebhookHandler {
                event_type: "pull_request".into(),
                command: vec!["/bin/cat".into()],
                filters: Default::default(),
                timeout_secs: Some(10),
            }],
        },
        PluginManifest {
            id: "broken".into(),
            name: "Broken".into(),
            version: "0.0.0".into(),
            webhooks: vec![WebhookHandler {
                event_type: "pull_request".into(),
                command: vec![
                    "/bin/cat".into(),
                    "/nonexistent/aish-e2e-does-not-exist".into(),
                ],
                filters: Default::default(),
                timeout_secs: Some(10),
            }],
        },
    ])
}

fn webhook() -> Webhook {
    Webhook {
        id: "w-e2e-1".into(),
        tenant_id: "acme".into(),
        plugin_id: String::new(),
        event_type: "pull_request".into(),
        payload: serde_json::json!({"action": "opened", "number": 42}),
    }
}

fn broker_config() -> BrokerConfig {
    BrokerConfig {
        broker_url: "wss://broker.invalid/ws".into(),
        tenant_id: "acme".into(),
        plugin: None,
        transport: "websocket".into(),
        enabled: true,
        secret: None,
        client_id: Some("svc".into()),
    }
}

/// Core acceptance: dispatch fork/execs real handlers, captures stdout+stderr,
/// isolates a failing handler from a succeeding one, and durably audits both.
#[tokio::test]
async fn e2e_dispatch_forkexec_captures_output_and_audits() {
    let sink = Arc::new(MemoryAuditSink::new());
    let dispatcher = WebhookDispatcher::new(Arc::new(registry())).with_audit_sink(sink.clone());

    let outcomes = dispatcher.dispatch(&webhook()).await;

    assert_eq!(outcomes.len(), 2, "both subscribers should run");

    let echo = outcomes
        .iter()
        .find(|o| o.plugin_id == "echo")
        .expect("echo outcome present");
    assert!(echo.executed && echo.success, "echo handler should exit 0");
    assert_eq!(echo.exit_code, Some(0));
    // /bin/cat streamed the JSON payload (delivered on stdin) back out.
    assert!(
        echo.stdout.contains("\"opened\""),
        "stdout should echo the payload, got: {:?}",
        echo.stdout
    );

    let broken = outcomes
        .iter()
        .find(|o| o.plugin_id == "broken")
        .expect("broken outcome present");
    assert!(broken.executed, "broken handler should have been exec'd");
    assert!(!broken.success, "broken handler should exit non-zero");
    assert_ne!(broken.exit_code, Some(0));
    assert!(
        !broken.stderr.is_empty(),
        "stderr should be captured, got: {:?}",
        broken.stderr
    );

    // Failure isolation: the failing handler did not sink the succeeding one.
    assert!(echo.success && !broken.success);

    // Durable audit trail: one record per outcome, identity threaded through.
    let records = sink.records();
    assert_eq!(records.len(), 2, "one audit record per handler");
    assert!(records
        .iter()
        .all(|r| r.webhook_id == "w-e2e-1" && r.tenant_id == "acme"));
    assert!(records.iter().any(|r| r.plugin_id == "echo" && r.success));
    assert!(records.iter().any(|r| r.plugin_id == "broken" && !r.success));
}

/// Full ingress path over the mock transport: the message loop reads a webhook
/// off the broker stream, dispatches it (fork/exec + audit), acks it, and
/// shuts down cleanly on signal.
#[tokio::test]
async fn e2e_ingress_via_mock_transport_dispatches_and_acks() {
    let sink = Arc::new(MemoryAuditSink::new());
    let dispatcher =
        Arc::new(WebhookDispatcher::new(Arc::new(registry())).with_audit_sink(sink.clone()));

    let (mock, mut handle) = mock_transport();
    let mut client = BrokerClient::new(broker_config());

    // Broker side: consume the client's auth frame, accept it, deliver one webhook.
    let drive = tokio::spawn(async move {
        let _auth = handle.next_sent().await; // client auth frame
        handle.push_text(r#"{"type":"auth_ok"}"#);
        handle.push_text(
            r#"{"id":"w-e2e-2","tenant_id":"acme","event_type":"pull_request","payload":{"action":"opened"}}"#,
        );
        handle
    });
    client.connect(mock).await.unwrap();
    let mut handle = drive.await.unwrap();

    let (tx, rx) = watch::channel(false);
    let mut svc = WebhookService::new(client, dispatcher);
    let jh = tokio::spawn(async move { svc.run(rx).await });

    // The service must ack the delivered webhook (ingress → dispatch → ack).
    let mut acked = false;
    while let Ok(Some(msg)) =
        tokio::time::timeout(Duration::from_millis(1000), handle.next_sent()).await
    {
        if let WsMessage::Text(t) = msg {
            if t.contains("\"type\":\"ack\"") && t.contains("w-e2e-2") {
                acked = true;
                break;
            }
        }
    }
    assert!(acked, "service should ack the dispatched webhook");

    // Dispatch (including audit) completes before the ack in the loop, so by
    // the time we observed the ack the audit trail is populated.
    let records = sink.records();
    assert!(
        records
            .iter()
            .any(|r| r.webhook_id == "w-e2e-2" && r.plugin_id == "echo" && r.success),
        "echo handler outcome should be audited, got: {records:?}"
    );

    // Clean shutdown on signal.
    tx.send(true).unwrap();
    let reason = tokio::time::timeout(Duration::from_secs(1), jh)
        .await
        .expect("service loop should stop")
        .expect("join ok");
    assert_eq!(reason, StopReason::Shutdown);
}
