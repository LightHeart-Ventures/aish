//! SPR-059 — aish webhook broker client integration.
//!
//! Wires the [`aish_webhook_client`] crate (broker connection manager +
//! background message loop + plugin handler dispatch, TASK-264/265/268) into the
//! aish engine. When the `WEBHOOK_BROKER_URL` environment variable is set,
//! [`WebhookHandle::spawn_from_env`] builds a [`BrokerConfig`], loads plugin
//! webhook handlers from `~/.aish/plugins`, and `tokio::spawn`s a background
//! task that:
//!
//!   1. connects to the broker (auth handshake),
//!   2. runs the read → dispatch → ack loop,
//!   3. auto-reconnects with exponential backoff on disconnect,
//!   4. shuts down gracefully on `:quit`.
//!
//! The REPL surfaces the service via `:webhook status|reload|logs`. A shared
//! [`MemoryAuditSink`] captures every handler outcome so `:webhook logs` can
//! show recent activity without a subscriber.
//!
//! Configuration (env vars):
//!   * `WEBHOOK_BROKER_URL`    — broker WebSocket URL (`wss://…/ws`). REQUIRED to
//!                               enable the service; unset ⇒ soft no-op.
//!   * `WEBHOOK_TENANT_ID`     — tenant to authenticate as (default `"default"`).
//!   * `WEBHOOK_BROKER_SECRET` — optional shared secret echoed in the auth frame.
//!   * `WEBHOOK_CLIENT_ID`     — optional stable client id (generated if absent).
//!   * `AISH_PLUGINS_DIR`      — override the plugin directory scanned for handlers.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use aish_webhook_client::{
    AuditRecord, BrokerClient, BrokerConfig, ConnState, FlashSink, MemoryAuditSink, PluginRegistry,
    StopReason, WebhookDispatcher, WebhookService, transport::TungsteniteTransport,
};
use tokio::sync::watch;

/// Runtime status shared between the background service task and the REPL.
#[derive(Debug, Clone)]
pub struct WebhookStatus {
    /// Current connection state.
    pub state: ConnState,
    /// When the current connection was established (`None` while down).
    pub connected_since: Option<Instant>,
    /// Number of reconnect cycles since the service started.
    pub reconnects: u64,
    /// Most recent disconnect/error reason, if any.
    pub last_error: Option<String>,
    /// True once the service loop has exited (shutdown).
    pub stopped: bool,
}

impl Default for WebhookStatus {
    fn default() -> Self {
        Self {
            state: ConnState::Disconnected,
            connected_since: None,
            reconnects: 0,
            last_error: None,
            stopped: false,
        }
    }
}

/// Adapt the SecondStatusLine flash slot (`session.flash`) into a webhook-client
/// [`FlashSink`]. Each broker-delivered handler's stdout is distilled to a
/// one-line, ≤60-char message (via [`crate::plugin_dispatcher::nline_message`])
/// and written into the single most-recent-wins slot the footer renders — the
/// last hop of "hello-world plugin received a broker webhook → SecondStatusLine".
pub fn flash_sink_from_slot(slot: Arc<Mutex<Option<String>>>) -> FlashSink {
    Arc::new(move |stdout: String| {
        if let Some(msg) = crate::plugin_dispatcher::nline_message(&stdout) {
            if let Ok(mut s) = slot.lock() {
                *s = Some(msg);
            }
        }
    })
}

/// Handle to a running webhook background service. Dropping it detaches the task
/// (it also dies with the process); call [`WebhookHandle::shutdown`] to stop it
/// gracefully first.
pub struct WebhookHandle {
    pub broker_url: String,
    pub tenant_id: String,
    pub plugins_dir: PathBuf,
    /// Number of webhook handlers loaded from the plugin directory.
    pub handler_count: usize,
    shutdown_tx: watch::Sender<bool>,
    status: Arc<Mutex<WebhookStatus>>,
    audit: Arc<MemoryAuditSink>,
    _join: tokio::task::JoinHandle<()>,
}

impl WebhookHandle {
    /// Read `WEBHOOK_BROKER_URL` (+ optional companions) and, when a non-empty
    /// broker URL is present, spawn the background service. Returns `None` when
    /// unconfigured — the common case — so callers treat "no broker" as a
    /// soft no-op.
    pub fn spawn_from_env(flash: Option<FlashSink>) -> Option<Self> {
        let broker_url = std::env::var("WEBHOOK_BROKER_URL").ok()?;
        if broker_url.trim().is_empty() {
            return None;
        }
        let config = config_from_env(broker_url);
        let dir = plugins_dir();
        Some(Self::spawn(config, dir, flash))
    }

    /// Spawn the background service for an explicit config + plugin directory.
    pub fn spawn(config: BrokerConfig, plugins_dir: PathBuf, flash: Option<FlashSink>) -> Self {
        // Load plugin webhook handlers; soft-fail to an empty registry so a
        // missing/!readable plugin dir never blocks broker connectivity.
        let registry = match PluginRegistry::load_dir(&plugins_dir) {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    dir = %plugins_dir.display(),
                    "webhook: plugin registry load failed; starting with no handlers"
                );
                PluginRegistry::from_plugins(Vec::new())
            }
        };
        let handler_count = registry.len();
        let registry = Arc::new(registry);
        let audit = Arc::new(MemoryAuditSink::new());
        let status = Arc::new(Mutex::new(WebhookStatus::default()));
        let (shutdown_tx, shutdown_rx) = watch::channel(false);

        let task_config = config.clone();
        let task_registry = registry.clone();
        let task_audit = audit.clone();
        let task_status = status.clone();
        let task_flash = flash;
        let join = tokio::spawn(async move {
            service_loop(
                task_config,
                task_registry,
                task_audit,
                task_status,
                shutdown_rx,
                task_flash,
            )
            .await;
        });

        Self {
            broker_url: config.broker_url,
            tenant_id: config.tenant_id,
            plugins_dir,
            handler_count,
            shutdown_tx,
            status,
            audit,
            _join: join,
        }
    }

    /// Snapshot of the current runtime status.
    pub fn status(&self) -> WebhookStatus {
        self.status.lock().unwrap().clone()
    }

    /// Total number of webhook events dispatched (audited) so far.
    pub fn events(&self) -> usize {
        self.audit.len()
    }

    /// The most recent `n` audit records (handler outcomes), oldest-first.
    pub fn recent_logs(&self, n: usize) -> Vec<AuditRecord> {
        tail(self.audit.records(), n)
    }

    /// Signal the background loop to disconnect and stop.
    pub fn shutdown(&self) {
        let _ = self.shutdown_tx.send(true);
    }

    /// Human-readable multi-line status block for `:webhook status`.
    pub fn status_lines(&self) -> String {
        let st = self.status();
        let state = match st.state {
            ConnState::Connected => "connected",
            ConnState::Connecting => "connecting",
            ConnState::Disconnected => {
                if st.stopped {
                    "stopped"
                } else {
                    "disconnected"
                }
            }
        };
        let up = st
            .connected_since
            .map(|t| fmt_dur(t.elapsed()))
            .unwrap_or_else(|| "—".to_string());
        let err = st.last_error.as_deref().unwrap_or("none");
        format!(
            "🪝 webhook: {state} — {url} (tenant {tenant})\n   \
             handlers: {h} from {dir}\n   \
             events: {ev}  reconnects: {rc}  up: {up}\n   \
             last event: {err}",
            url = self.broker_url,
            tenant = self.tenant_id,
            h = self.handler_count,
            dir = self.plugins_dir.display(),
            ev = self.events(),
            rc = st.reconnects,
        )
    }
}

/// Rebuild the webhook service from the environment, reloading plugin handlers
/// from disk. Used by `:webhook reload`. Returns the reloaded handler count on
/// success. Errors (returned as a message) when no broker URL is configured.
pub fn reload(
    slot: &mut Option<WebhookHandle>,
    flash: Option<FlashSink>,
) -> Result<usize, String> {
    let configured = std::env::var("WEBHOOK_BROKER_URL")
        .ok()
        .is_some_and(|u| !u.trim().is_empty());
    if !configured {
        return Err("WEBHOOK_BROKER_URL is not set — nothing to reload".to_string());
    }
    // Tear down the old task first so the broker connection count stays sane.
    if let Some(h) = slot.take() {
        h.shutdown();
    }
    match WebhookHandle::spawn_from_env(flash) {
        Some(h) => {
            let n = h.handler_count;
            *slot = Some(h);
            Ok(n)
        }
        None => Err("failed to re-initialize webhook service".to_string()),
    }
}

/// Format one audit record for `:webhook logs`.
pub fn fmt_record(r: &AuditRecord) -> String {
    let outcome = if !r.matched {
        "skip"
    } else if r.success {
        "ok"
    } else {
        "fail"
    };
    let exit = r
        .exit_code
        .map(|c| c.to_string())
        .unwrap_or_else(|| "-".to_string());
    let err = r.error.as_deref().unwrap_or("");
    format!(
        "{et} [{outcome}] plugin={pid} exit={exit} {ms}ms {err}",
        et = r.event_type,
        pid = r.plugin_id,
        ms = r.duration_ms,
    )
    .trim_end()
    .to_string()
}

/// Build a [`BrokerConfig`] from the environment given a broker URL.
fn config_from_env(broker_url: String) -> BrokerConfig {
    BrokerConfig {
        broker_url,
        tenant_id: std::env::var("WEBHOOK_TENANT_ID").unwrap_or_else(|_| "default".to_string()),
        plugin: None,
        transport: "websocket".to_string(),
        enabled: true,
        secret: std::env::var("WEBHOOK_BROKER_SECRET").ok(),
        client_id: std::env::var("WEBHOOK_CLIENT_ID").ok(),
    }
}

/// Resolve the plugin directory scanned for webhook handlers.
fn plugins_dir() -> PathBuf {
    if let Ok(d) = std::env::var("AISH_PLUGINS_DIR") {
        return PathBuf::from(d);
    }
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .unwrap_or_default();
    PathBuf::from(home).join(".aish").join("plugins")
}

/// Return the last `n` elements of `v` (oldest-first), or all of them when
/// `v.len() <= n`.
fn tail<T>(mut v: Vec<T>, n: usize) -> Vec<T> {
    let len = v.len();
    if len > n {
        v.split_off(len - n)
    } else {
        v
    }
}

fn fmt_dur(d: Duration) -> String {
    let secs = d.as_secs();
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m{}s", secs / 60, secs % 60)
    } else {
        format!("{}h{}m", secs / 3600, (secs % 3600) / 60)
    }
}

fn set_state(status: &Arc<Mutex<WebhookStatus>>, st: ConnState) {
    status.lock().unwrap().state = st;
}

/// Resolve when the shutdown flag flips to `true` (or the sender is dropped).
async fn wait_for_shutdown(mut rx: watch::Receiver<bool>) {
    if *rx.borrow() {
        return;
    }
    while rx.changed().await.is_ok() {
        if *rx.borrow() {
            return;
        }
    }
}

/// The background connect → run → reconnect loop. Runs until shutdown.
async fn service_loop(
    config: BrokerConfig,
    registry: Arc<PluginRegistry>,
    audit: Arc<MemoryAuditSink>,
    status: Arc<Mutex<WebhookStatus>>,
    shutdown_rx: watch::Receiver<bool>,
    flash: Option<FlashSink>,
) {
    let mut dispatcher = WebhookDispatcher::new(registry).with_audit_sink(audit);
    if let Some(f) = flash {
        // Wire the broker dispatcher to the SecondStatusLine: a handler's stdout
        // now surfaces on the footer. This is the seam that completes the goal.
        dispatcher = dispatcher.with_flash_sink(f);
    }
    let dispatcher = Arc::new(dispatcher);

    loop {
        if *shutdown_rx.borrow() {
            break;
        }

        set_state(&status, ConnState::Connecting);
        let mut client = BrokerClient::new(config.clone());
        let url = config.broker_url.clone();

        // reconnect_with_backoff loops forever (max_attempts = None) until a
        // connect succeeds; race it against the shutdown signal so `:quit`
        // interrupts an in-progress reconnect.
        let connected = tokio::select! {
            r = client.reconnect_with_backoff(|| TungsteniteTransport::connect(&url), None) => r.is_ok(),
            _ = wait_for_shutdown(shutdown_rx.clone()) => false,
        };
        if !connected {
            break;
        }

        {
            let mut s = status.lock().unwrap();
            s.state = ConnState::Connected;
            s.connected_since = Some(Instant::now());
            s.last_error = None;
        }
        tracing::info!(url = %config.broker_url, "webhook: connected to broker");

        let mut service = WebhookService::new(client, dispatcher.clone());
        let reason = service.run(shutdown_rx.clone()).await;
        match reason {
            StopReason::Shutdown => break,
            StopReason::BrokerClosed | StopReason::Disconnected => {
                let mut s = status.lock().unwrap();
                s.state = ConnState::Disconnected;
                s.connected_since = None;
                s.reconnects += 1;
                s.last_error = Some(format!("{reason:?}"));
                drop(s);
                tracing::warn!(?reason, "webhook: disconnected — reconnecting with backoff");
                // Fall through to the next loop iteration → reconnect.
            }
        }
    }

    let mut s = status.lock().unwrap();
    s.state = ConnState::Disconnected;
    s.connected_since = None;
    s.stopped = true;
    tracing::info!("webhook: service stopped");
}

#[cfg(test)]
mod tests {
    use super::*;

    // Env access is process-global; serialize the env-mutating tests.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn tail_returns_all_when_shorter() {
        assert_eq!(tail(vec![1, 2, 3], 5), vec![1, 2, 3]);
    }

    #[test]
    fn tail_returns_last_n_oldest_first() {
        assert_eq!(tail(vec![1, 2, 3, 4, 5], 2), vec![4, 5]);
        assert_eq!(tail(vec![1, 2, 3, 4, 5], 0), Vec::<i32>::new());
    }

    #[test]
    fn fmt_dur_scales() {
        assert_eq!(fmt_dur(Duration::from_secs(5)), "5s");
        assert_eq!(fmt_dur(Duration::from_secs(65)), "1m5s");
        assert_eq!(fmt_dur(Duration::from_secs(3720)), "1h2m");
    }

    #[test]
    fn status_default_is_disconnected() {
        let s = WebhookStatus::default();
        assert_eq!(s.state, ConnState::Disconnected);
        assert!(!s.stopped);
        assert_eq!(s.reconnects, 0);
    }

    #[test]
    fn flash_sink_writes_capped_line_to_slot() {
        // The adapter is the new hop: broker handler stdout → SecondStatusLine slot.
        let slot = Arc::new(Mutex::new(None));
        let sink = flash_sink_from_slot(slot.clone());
        sink("\n  \n👋 hello-world: ping webhook received\nsecond line\n".to_string());
        assert_eq!(
            slot.lock().unwrap().as_deref(),
            Some("👋 hello-world: ping webhook received")
        );
    }

    #[test]
    fn flash_sink_ignores_blank_stdout() {
        // Nothing surfaceable → the prior most-recent-wins value is preserved.
        let slot = Arc::new(Mutex::new(Some("prior".to_string())));
        let sink = flash_sink_from_slot(slot.clone());
        sink("   \n\n".to_string());
        assert_eq!(slot.lock().unwrap().as_deref(), Some("prior"));
    }

    #[test]
    fn config_from_env_maps_optional_fields() {
        let _g = ENV_LOCK.lock().unwrap();
        // SAFETY: guarded by ENV_LOCK; single-threaded within this test.
        unsafe {
            std::env::set_var("WEBHOOK_TENANT_ID", "acme");
            std::env::set_var("WEBHOOK_BROKER_SECRET", "s3cr3t");
            std::env::remove_var("WEBHOOK_CLIENT_ID");
        }
        let cfg = config_from_env("wss://broker.example/ws".to_string());
        assert_eq!(cfg.broker_url, "wss://broker.example/ws");
        assert_eq!(cfg.tenant_id, "acme");
        assert_eq!(cfg.secret.as_deref(), Some("s3cr3t"));
        assert_eq!(cfg.client_id, None);
        assert!(cfg.enabled);
        assert_eq!(cfg.transport, "websocket");
        unsafe {
            std::env::remove_var("WEBHOOK_TENANT_ID");
            std::env::remove_var("WEBHOOK_BROKER_SECRET");
        }
    }

    #[test]
    fn spawn_from_env_none_when_unset() {
        let _g = ENV_LOCK.lock().unwrap();
        // SAFETY: guarded by ENV_LOCK.
        unsafe {
            std::env::remove_var("WEBHOOK_BROKER_URL");
        }
        assert!(WebhookHandle::spawn_from_env(None).is_none());
    }

    #[test]
    fn plugins_dir_honours_override() {
        let _g = ENV_LOCK.lock().unwrap();
        // SAFETY: guarded by ENV_LOCK.
        unsafe {
            std::env::set_var("AISH_PLUGINS_DIR", "/tmp/aish-plugins-test");
        }
        assert_eq!(plugins_dir(), PathBuf::from("/tmp/aish-plugins-test"));
        unsafe {
            std::env::remove_var("AISH_PLUGINS_DIR");
        }
    }

    #[test]
    fn fmt_record_renders_outcomes() {
        let ok = AuditRecord {
            webhook_id: "w1".into(),
            tenant_id: "t".into(),
            plugin_id: "gh".into(),
            event_type: "pull_request".into(),
            matched: true,
            executed: true,
            exit_code: Some(0),
            success: true,
            error: None,
            duration_ms: 12,
            recorded_at_ms: 0,
        };
        let s = fmt_record(&ok);
        assert!(s.contains("pull_request"));
        assert!(s.contains("[ok]"));
        assert!(s.contains("plugin=gh"));

        let skipped = AuditRecord {
            matched: false,
            success: false,
            ..ok.clone()
        };
        assert!(fmt_record(&skipped).contains("[skip]"));
    }
}
