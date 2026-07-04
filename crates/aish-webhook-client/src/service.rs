//! TASK-265 — Message-loop service.
//!
//! Ties a [`BrokerClient`] to a [`WebhookDispatcher`]: reads webhooks in a
//! background loop, dispatches each to plugin handlers, acks on completion, and
//! shuts down gracefully when signalled. Designed to be `tokio::spawn`ed so it
//! never blocks the aish REPL.

use std::sync::Arc;

use tokio::sync::watch;

use crate::client::BrokerClient;
use crate::dispatcher::WebhookDispatcher;
use crate::transport::Transport;

/// A running webhook service.
pub struct WebhookService<T: Transport> {
    client: BrokerClient<T>,
    dispatcher: Arc<WebhookDispatcher>,
}

/// Why the [`WebhookService::run`] loop returned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    /// A shutdown signal was received (`:quit`).
    Shutdown,
    /// The broker closed the stream cleanly.
    BrokerClosed,
    /// The transport dropped unexpectedly (caller may reconnect).
    Disconnected,
}

impl<T: Transport> WebhookService<T> {
    pub fn new(client: BrokerClient<T>, dispatcher: Arc<WebhookDispatcher>) -> Self {
        Self { client, dispatcher }
    }

    /// Borrow the underlying client (e.g. to inspect state).
    pub fn client(&self) -> &BrokerClient<T> {
        &self.client
    }

    /// Run the read → dispatch → ack loop until shutdown or disconnect.
    ///
    /// `shutdown` fires when the watched value flips to `true`. On any exit the
    /// broker connection is closed gracefully.
    pub async fn run(&mut self, mut shutdown: watch::Receiver<bool>) -> StopReason {
        // Honour an already-set shutdown flag.
        if *shutdown.borrow() {
            self.client.close().await;
            return StopReason::Shutdown;
        }

        let reason = loop {
            tokio::select! {
                changed = shutdown.changed() => {
                    // Sender dropped or value flipped → treat as shutdown.
                    if changed.is_err() || *shutdown.borrow() {
                        break StopReason::Shutdown;
                    }
                }
                res = self.client.poll_next() => {
                    match res {
                        Ok(Some(webhook)) => {
                            let outcomes = self.dispatcher.dispatch(&webhook).await;
                            tracing::debug!(
                                id = %webhook.id,
                                handlers = outcomes.len(),
                                "dispatched webhook"
                            );
                            if let Err(e) = self.client.ack(&webhook.id).await {
                                tracing::warn!(error = %e, id = %webhook.id, "ack failed");
                                break StopReason::Disconnected;
                            }
                        }
                        Ok(None) => break StopReason::BrokerClosed,
                        Err(e) => {
                            tracing::warn!(error = %e, "broker read error");
                            break StopReason::Disconnected;
                        }
                    }
                }
            }
        };

        self.client.close().await;
        reason
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dispatcher::{PluginManifest, PluginRegistry, WebhookHandler};
    use crate::envelope::BrokerConfig;
    use crate::transport::{mock_transport, WsMessage};
    use std::time::Duration;

    fn cfg() -> BrokerConfig {
        BrokerConfig {
            broker_url: "wss://x".into(),
            tenant_id: "t1".into(),
            plugin: None,
            transport: "websocket".into(),
            enabled: true,
            secret: None,
            client_id: Some("svc".into()),
        }
    }

    fn dispatcher() -> Arc<WebhookDispatcher> {
        let reg = PluginRegistry::from_plugins(vec![PluginManifest {
            id: "echo".into(),
            name: "".into(),
            version: "".into(),
            webhooks: vec![WebhookHandler {
                event_type: "pull_request".into(),
                command: vec!["true".into()],
                filters: Default::default(),
                timeout_secs: None,
            }],
        }]);
        Arc::new(WebhookDispatcher::new(Arc::new(reg)))
    }

    #[tokio::test]
    async fn run_dispatches_then_acks_then_shuts_down() {
        let (mock, mut handle) = mock_transport();
        let mut client = BrokerClient::new(cfg());

        // Complete auth, then deliver one webhook.
        let drive = tokio::spawn(async move {
            let _auth = handle.next_sent().await; // auth frame
            handle.push_text(r#"{"type":"auth_ok"}"#);
            handle.push_text(
                r#"{"id":"w1","event_type":"pull_request","payload":{"action":"opened"}}"#,
            );
            handle
        });
        client.connect(mock).await.unwrap();
        let mut handle = drive.await.unwrap();

        let (tx, rx) = watch::channel(false);
        let mut svc = WebhookService::new(client, dispatcher());
        let jh = tokio::spawn(async move { svc.run(rx).await });

        // Expect an ack for w1.
        let mut acked = false;
        while let Ok(Some(msg)) =
            tokio::time::timeout(Duration::from_millis(500), handle.next_sent()).await
        {
            if let WsMessage::Text(t) = msg {
                if t.contains("\"type\":\"ack\"") && t.contains("w1") {
                    acked = true;
                    break;
                }
            }
        }
        assert!(acked, "service should ack the dispatched webhook");

        // Signal shutdown; the loop must return Shutdown.
        tx.send(true).unwrap();
        let reason =
            tokio::time::timeout(Duration::from_secs(1), jh).await.unwrap().unwrap();
        assert_eq!(reason, StopReason::Shutdown);
    }

    #[tokio::test]
    async fn broker_close_stops_loop() {
        let (mock, mut handle) = mock_transport();
        let mut client = BrokerClient::new(cfg());
        let drive = tokio::spawn(async move {
            let _ = handle.next_sent().await;
            handle.push_text(r#"{"type":"auth_ok"}"#);
            handle.push(WsMessage::Close);
            handle
        });
        client.connect(mock).await.unwrap();
        let _ = drive.await.unwrap();

        let (_tx, rx) = watch::channel(false);
        let mut svc = WebhookService::new(client, dispatcher());
        let reason = tokio::time::timeout(Duration::from_secs(1), svc.run(rx))
            .await
            .unwrap();
        assert_eq!(reason, StopReason::BrokerClosed);
    }
}
