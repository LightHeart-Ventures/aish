//! Transport abstraction.
//!
//! The connection manager and message loop are written against the [`Transport`]
//! trait so their logic is exercised end-to-end by [`MockTransport`] with no
//! sockets. The real WebSocket transport ([`TungsteniteTransport`]) is compiled
//! only under the `net` feature.

use crate::error::{Result, WebhookClientError};
use async_trait::async_trait;

/// The minimal WebSocket message set the client cares about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WsMessage {
    Text(String),
    Ping(Vec<u8>),
    Pong(Vec<u8>),
    Close,
}

/// A bidirectional, ordered message channel to the broker.
#[async_trait]
pub trait Transport: Send {
    /// Send one frame.
    async fn send(&mut self, msg: WsMessage) -> Result<()>;
    /// Receive the next frame; `Ok(None)` means the stream ended.
    async fn recv(&mut self) -> Result<Option<WsMessage>>;
    /// Close the transport (best-effort).
    async fn close(&mut self) -> Result<()>;
}

// ---------------------------------------------------------------------------
// Real WebSocket transport (feature = "net")
// ---------------------------------------------------------------------------
#[cfg(feature = "net")]
pub use net_impl::TungsteniteTransport;

#[cfg(feature = "net")]
mod net_impl {
    use super::*;
    use futures_util::{SinkExt, StreamExt};
    use tokio::net::TcpStream;
    use tokio_tungstenite::{
        connect_async, tungstenite::protocol::Message, MaybeTlsStream, WebSocketStream,
    };

    /// tokio-tungstenite backed transport (ws:// and wss://).
    pub struct TungsteniteTransport {
        inner: WebSocketStream<MaybeTlsStream<TcpStream>>,
    }

    impl TungsteniteTransport {
        /// Dial the broker and complete the WebSocket upgrade.
        pub async fn connect(url: &str) -> Result<Self> {
            let (inner, _resp) = connect_async(url)
                .await
                .map_err(|e| WebhookClientError::Connection(e.to_string()))?;
            Ok(Self { inner })
        }
    }

    #[async_trait]
    impl Transport for TungsteniteTransport {
        async fn send(&mut self, msg: WsMessage) -> Result<()> {
            let m = match msg {
                WsMessage::Text(t) => Message::Text(t),
                WsMessage::Ping(p) => Message::Ping(p),
                WsMessage::Pong(p) => Message::Pong(p),
                WsMessage::Close => Message::Close(None),
            };
            self.inner
                .send(m)
                .await
                .map_err(|e| WebhookClientError::Connection(e.to_string()))
        }

        async fn recv(&mut self) -> Result<Option<WsMessage>> {
            match self.inner.next().await {
                Some(Ok(Message::Text(t))) => Ok(Some(WsMessage::Text(t))),
                Some(Ok(Message::Binary(b))) => {
                    Ok(Some(WsMessage::Text(String::from_utf8_lossy(&b).into_owned())))
                }
                Some(Ok(Message::Ping(p))) => Ok(Some(WsMessage::Ping(p))),
                Some(Ok(Message::Pong(p))) => Ok(Some(WsMessage::Pong(p))),
                Some(Ok(Message::Close(_))) => Ok(Some(WsMessage::Close)),
                // Frame() and other internal variants: ignore, keep reading.
                Some(Ok(_)) => Ok(None),
                Some(Err(e)) => Err(WebhookClientError::Connection(e.to_string())),
                None => Ok(None),
            }
        }

        async fn close(&mut self) -> Result<()> {
            let _ = self.inner.close(None).await;
            Ok(())
        }
    }
}

// ---------------------------------------------------------------------------
// Mock transport for hermetic tests (always compiled)
// ---------------------------------------------------------------------------
use tokio::sync::mpsc;

/// In-memory transport backed by tokio channels. Pair it with [`MockHandle`]
/// (returned by [`mock_transport`]) to drive the client from a test: push
/// server→client frames and inspect what the client sent.
pub struct MockTransport {
    to_client: mpsc::UnboundedReceiver<WsMessage>,
    from_client: mpsc::UnboundedSender<WsMessage>,
    closed: bool,
}

/// Test-side handle to a [`MockTransport`].
pub struct MockHandle {
    to_client: mpsc::UnboundedSender<WsMessage>,
    from_client: mpsc::UnboundedReceiver<WsMessage>,
}

/// Construct a connected mock transport + its controlling handle.
pub fn mock_transport() -> (MockTransport, MockHandle) {
    let (to_c_tx, to_c_rx) = mpsc::unbounded_channel();
    let (from_c_tx, from_c_rx) = mpsc::unbounded_channel();
    (
        MockTransport {
            to_client: to_c_rx,
            from_client: from_c_tx,
            closed: false,
        },
        MockHandle {
            to_client: to_c_tx,
            from_client: from_c_rx,
        },
    )
}

impl MockHandle {
    /// Deliver a frame to the client's `recv()`.
    pub fn push(&self, msg: WsMessage) {
        let _ = self.to_client.send(msg);
    }

    /// Convenience: deliver a text frame.
    pub fn push_text(&self, s: impl Into<String>) {
        self.push(WsMessage::Text(s.into()));
    }

    /// End the client's stream (recv → Ok(None)).
    pub fn disconnect(self) {
        drop(self.to_client);
    }

    /// Await the next frame the client sent, or `None` if it dropped its end.
    pub async fn next_sent(&mut self) -> Option<WsMessage> {
        self.from_client.recv().await
    }
}

#[async_trait]
impl Transport for MockTransport {
    async fn send(&mut self, msg: WsMessage) -> Result<()> {
        self.from_client
            .send(msg)
            .map_err(|_| WebhookClientError::TransportClosed)
    }

    async fn recv(&mut self) -> Result<Option<WsMessage>> {
        if self.closed {
            return Ok(None);
        }
        Ok(self.to_client.recv().await)
    }

    async fn close(&mut self) -> Result<()> {
        self.closed = true;
        Ok(())
    }
}
