//! TASK-264 — Broker Connection Manager.
//!
//! [`BrokerClient`] owns a [`Transport`], performs the auth handshake, exposes a
//! reconnect loop with exponential backoff, answers heartbeats, and reads the
//! next webhook off the wire (the read half of the TASK-265 message loop).

use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use crate::backoff::ExponentialBackoff;
use crate::envelope::{BrokerConfig, ClientFrame, ServerFrame, Webhook};
use crate::error::{Result, WebhookClientError};
use crate::transport::{Transport, WsMessage};

/// Connection lifecycle state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnState {
    Disconnected,
    Connecting,
    Connected,
}

static CLIENT_SEQ: AtomicU64 = AtomicU64::new(1);

fn generate_client_id() -> String {
    let n = CLIENT_SEQ.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("aish-{nanos:x}-{n}")
}

/// Broker connection manager (generic over the transport for testability).
pub struct BrokerClient<T: Transport> {
    config: BrokerConfig,
    transport: Option<T>,
    session_token: Option<String>,
    client_id: String,
    state: ConnState,
    last_connected_at: Option<Instant>,
    backoff: ExponentialBackoff,
    auth_timeout: Duration,
}

impl<T: Transport> BrokerClient<T> {
    /// Build a client from config (does not connect).
    pub fn new(config: BrokerConfig) -> Self {
        let client_id = config.client_id.clone().unwrap_or_else(generate_client_id);
        Self {
            config,
            transport: None,
            session_token: None,
            client_id,
            state: ConnState::Disconnected,
            last_connected_at: None,
            backoff: ExponentialBackoff::default(),
            auth_timeout: Duration::from_secs(10),
        }
    }

    /// Override the backoff schedule (e.g. for tests / tuning).
    pub fn with_backoff(mut self, backoff: ExponentialBackoff) -> Self {
        self.backoff = backoff;
        self
    }

    /// Override the auth-handshake timeout.
    pub fn with_auth_timeout(mut self, d: Duration) -> Self {
        self.auth_timeout = d;
        self
    }

    pub fn state(&self) -> ConnState {
        self.state
    }
    pub fn is_connected(&self) -> bool {
        self.state == ConnState::Connected
    }
    pub fn client_id(&self) -> &str {
        &self.client_id
    }
    pub fn session_token(&self) -> Option<&str> {
        self.session_token.as_deref()
    }
    pub fn last_connected_at(&self) -> Option<Instant> {
        self.last_connected_at
    }
    pub fn config(&self) -> &BrokerConfig {
        &self.config
    }

    /// Take ownership of an established transport and complete the auth
    /// handshake. On success the client is `Connected` and the backoff resets.
    pub async fn connect(&mut self, transport: T) -> Result<()> {
        self.state = ConnState::Connecting;
        self.transport = Some(transport);
        match self.authenticate().await {
            Ok(()) => {
                self.state = ConnState::Connected;
                self.last_connected_at = Some(Instant::now());
                self.backoff.reset();
                Ok(())
            }
            Err(e) => {
                self.state = ConnState::Disconnected;
                self.transport = None;
                Err(e)
            }
        }
    }

    /// Send the auth frame and await the broker's acknowledgement.
    async fn authenticate(&mut self) -> Result<()> {
        // Serialize the auth frame in its own scope so the immutable borrow of
        // `self.config` ends before we take the `&mut self.transport` below.
        let txt = {
            let frame = ClientFrame::Auth {
                tenant_id: &self.config.tenant_id,
                client_id: &self.client_id,
                plugin: self.config.plugin.as_deref(),
                secret: self.config.secret.as_deref(),
            };
            serde_json::to_string(&frame)?
        };
        {
            let transport = self
                .transport
                .as_mut()
                .ok_or(WebhookClientError::TransportClosed)?;
            transport.send(WsMessage::Text(txt)).await?;
        }

        let transport = self
            .transport
            .as_mut()
            .ok_or(WebhookClientError::TransportClosed)?;

        // Await the first inbound frame within the handshake window, tolerating
        // transport-level pings before the auth_ok arrives.
        let deadline = tokio::time::sleep(self.auth_timeout);
        tokio::pin!(deadline);
        loop {
            tokio::select! {
                _ = &mut deadline => return Err(WebhookClientError::Timeout(self.auth_timeout)),
                res = transport.recv() => {
                    match res? {
                        None => return Err(WebhookClientError::TransportClosed),
                        Some(WsMessage::Ping(p)) => { transport.send(WsMessage::Pong(p)).await?; }
                        Some(WsMessage::Pong(_)) => {}
                        Some(WsMessage::Close) => return Err(WebhookClientError::TransportClosed),
                        Some(WsMessage::Text(txt)) => match ServerFrame::parse(&txt)? {
                            ServerFrame::AuthOk { session_token, client_id } => {
                                if let Some(st) = session_token { self.session_token = Some(st); }
                                if let Some(cid) = client_id { self.client_id = cid; }
                                return Ok(());
                            }
                            // Some brokers start streaming immediately; treat the
                            // first webhook as implicit auth success is unsafe, so
                            // require an explicit ack: ignore other frames here.
                            _ => {}
                        },
                    }
                }
            }
        }
    }

    /// Keep retrying `factory()` → [`connect`] until one succeeds, sleeping
    /// with exponential backoff between attempts. Bounded by `max_attempts`
    /// (None = retry forever). Heartbeat/keep-alive is handled by [`poll_next`].
    pub async fn reconnect_with_backoff<F, Fut>(
        &mut self,
        mut factory: F,
        max_attempts: Option<u32>,
    ) -> Result<()>
    where
        F: FnMut() -> Fut,
        Fut: Future<Output = Result<T>>,
    {
        let mut attempts = 0u32;
        let mut last_err: WebhookClientError;
        loop {
            attempts += 1;
            match factory().await {
                Ok(t) => match self.connect(t).await {
                    Ok(()) => return Ok(()),
                    Err(e) => last_err = e,
                },
                Err(e) => last_err = e,
            }
            if let Some(max) = max_attempts {
                if attempts >= max {
                    return Err(last_err);
                }
            }
            let wait = self.backoff.next_backoff();
            tracing::warn!(attempt = attempts, ?wait, error = %last_err, "broker reconnect failed, retrying");
            tokio::time::sleep(wait).await;
        }
    }

    /// Read the next webhook, transparently answering heartbeats.
    ///
    /// * transport `Ping` → replies `Pong` and keeps reading
    /// * application `{"type":"ping"}` → replies application `Pong`
    /// * `Close` / stream end → `Ok(None)` (graceful) or `Err(TransportClosed)`
    ///
    /// Returns `Ok(Some(webhook))` for the first webhook frame.
    pub async fn poll_next(&mut self) -> Result<Option<Webhook>> {
        loop {
            let transport = self
                .transport
                .as_mut()
                .ok_or(WebhookClientError::TransportClosed)?;
            match transport.recv().await? {
                None => {
                    self.state = ConnState::Disconnected;
                    return Err(WebhookClientError::TransportClosed);
                }
                Some(WsMessage::Ping(p)) => {
                    transport.send(WsMessage::Pong(p)).await?;
                }
                Some(WsMessage::Pong(_)) => {}
                Some(WsMessage::Close) => {
                    self.state = ConnState::Disconnected;
                    return Ok(None);
                }
                Some(WsMessage::Text(txt)) => match ServerFrame::parse(&txt)? {
                    ServerFrame::Webhook(w) => return Ok(Some(w)),
                    ServerFrame::Ping => {
                        self.send_frame(&ClientFrame::Pong).await?;
                    }
                    ServerFrame::AuthOk { .. } | ServerFrame::Other => {}
                },
            }
        }
    }

    /// Acknowledge a delivered webhook so the broker drops it from its queue.
    pub async fn ack(&mut self, id: &str) -> Result<()> {
        self.send_frame(&ClientFrame::Ack { id }).await
    }

    /// Graceful shutdown: best-effort close + state reset.
    pub async fn close(&mut self) {
        if let Some(mut t) = self.transport.take() {
            let _ = t.send(WsMessage::Close).await;
            let _ = t.close().await;
        }
        self.state = ConnState::Disconnected;
    }

    async fn send_frame(&mut self, frame: &ClientFrame<'_>) -> Result<()> {
        let txt = serde_json::to_string(frame)?;
        let transport = self
            .transport
            .as_mut()
            .ok_or(WebhookClientError::TransportClosed)?;
        transport.send(WsMessage::Text(txt)).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::mock_transport;

    fn cfg() -> BrokerConfig {
        BrokerConfig {
            broker_url: "wss://x".into(),
            tenant_id: "t1".into(),
            plugin: Some("github".into()),
            transport: "websocket".into(),
            enabled: true,
            secret: Some("s3cr3t".into()),
            client_id: Some("c-fixed".into()),
        }
    }

    #[tokio::test]
    async fn connect_sends_auth_and_captures_session() {
        let (mock, mut handle) = mock_transport();
        let mut client = BrokerClient::new(cfg());

        // Drive: server replies auth_ok once it sees the auth frame.
        let jh = tokio::spawn(async move {
            let sent = handle.next_sent().await.unwrap();
            match sent {
                WsMessage::Text(t) => {
                    assert!(t.contains("\"type\":\"auth\""), "auth frame: {t}");
                    assert!(t.contains("t1"));
                    assert!(t.contains("github"));
                }
                other => panic!("expected auth text, got {other:?}"),
            }
            handle.push_text(r#"{"type":"auth_ok","session_token":"tok-9","client_id":"c-assigned"}"#);
            handle
        });

        client.connect(mock).await.unwrap();
        assert!(client.is_connected());
        assert_eq!(client.session_token(), Some("tok-9"));
        assert_eq!(client.client_id(), "c-assigned");
        let _ = jh.await.unwrap();
    }

    #[tokio::test]
    async fn auth_times_out_when_no_ack() {
        let (mock, _handle) = mock_transport();
        let mut client = BrokerClient::new(cfg()).with_auth_timeout(Duration::from_millis(50));
        let err = client.connect(mock).await.unwrap_err();
        matches!(err, WebhookClientError::Timeout(_));
        assert_eq!(client.state(), ConnState::Disconnected);
    }

    #[tokio::test]
    async fn poll_next_answers_transport_ping_then_returns_webhook() {
        let (mock, handle) = mock_transport();
        let mut client = BrokerClient::new(cfg());
        // pre-auth: shove it straight to Connected via connect()
        let (mock2, mut h2) = mock_transport();
        let drive = tokio::spawn(async move {
            let _ = h2.next_sent().await; // auth frame
            h2.push_text(r#"{"type":"auth_ok"}"#);
            h2
        });
        client.connect(mock2).await.unwrap();
        let _ = drive.await.unwrap();
        drop((mock, handle));

        // Now inject a ping + webhook on the *live* transport. Re-wire: use a
        // fresh connect against a controllable handle.
        let (mock3, mut h3) = mock_transport();
        let drive2 = tokio::spawn(async move {
            let _ = h3.next_sent().await;
            h3.push_text(r#"{"type":"auth_ok"}"#);
            // heartbeat then a webhook
            h3.push(WsMessage::Ping(vec![1, 2, 3]));
            h3.push_text(r#"{"id":"d1","event_type":"pull_request","payload":{"action":"opened"}}"#);
            h3
        });
        client.connect(mock3).await.unwrap();
        let mut h3 = drive2.await.unwrap();

        let w = client.poll_next().await.unwrap().unwrap();
        assert_eq!(w.id, "d1");
        assert_eq!(w.event_type, "pull_request");

        // The client must have answered the ping with a pong.
        let mut saw_pong = false;
        while let Ok(Some(msg)) =
            tokio::time::timeout(Duration::from_millis(50), h3.next_sent()).await
        {
            if let WsMessage::Pong(p) = msg {
                assert_eq!(p, vec![1, 2, 3]);
                saw_pong = true;
                break;
            }
        }
        assert!(saw_pong, "client should pong the broker heartbeat");
    }

    #[tokio::test]
    async fn ack_emits_ack_frame() {
        let (mock, mut handle) = mock_transport();
        let mut client = BrokerClient::new(cfg());
        let drive = tokio::spawn(async move {
            let _ = handle.next_sent().await;
            handle.push_text(r#"{"type":"auth_ok"}"#);
            handle
        });
        client.connect(mock).await.unwrap();
        let mut handle = drive.await.unwrap();
        client.ack("d-42").await.unwrap();
        let sent = handle.next_sent().await.unwrap();
        match sent {
            WsMessage::Text(t) => {
                assert!(t.contains("\"type\":\"ack\""));
                assert!(t.contains("d-42"));
            }
            other => panic!("expected ack, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn reconnect_retries_until_success() {
        let mut client = BrokerClient::new(cfg())
            .with_backoff(ExponentialBackoff::new(
                Duration::from_millis(1),
                Duration::from_millis(2),
                2.0,
                false,
            ));
        let attempts = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
        let a2 = attempts.clone();
        // Factory fails twice, then hands back a transport that will auth-ok.
        let factory = move || {
            let a2 = a2.clone();
            async move {
                let n = a2.fetch_add(1, Ordering::Relaxed);
                if n < 2 {
                    Err(WebhookClientError::Connection("boom".into()))
                } else {
                    let (mock, mut handle) = mock_transport();
                    tokio::spawn(async move {
                        let _ = handle.next_sent().await;
                        handle.push_text(r#"{"type":"auth_ok"}"#);
                        // keep handle alive briefly
                        tokio::time::sleep(Duration::from_millis(20)).await;
                    });
                    Ok(mock)
                }
            }
        };
        client.reconnect_with_backoff(factory, Some(5)).await.unwrap();
        assert!(client.is_connected());
        assert_eq!(attempts.load(Ordering::Relaxed), 3);
    }
}
