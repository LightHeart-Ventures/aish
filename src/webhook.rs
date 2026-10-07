//! SPR-059 — aish webhook broker client integration.
//!
//! Wires the [`aish_webhook_client`] crate (broker connection manager +
//! background message loop + plugin handler dispatch, TASK-264/265/268) into the
//! aish engine. When the `WEBHOOK_BROKER_URL` environment variable is set,
//! [`WebhookHandle::spawn_from_env`] builds a [`BrokerConfig`], loads plugin
//! webhook handlers from `~/.aish/plugins`, and `tokio::spawn`s a background
//! task that:
//!
//!   1. registers its `(tenant, plugin)` route over HTTP(S)
//!      (`POST /clients/register` → `session_token`, TASK-449),
//!   2. connects to the broker WebSocket (`{"type":"auth","session_token"}`),
//!   3. runs the read → dispatch → ack (`{"type":"ack","webhook_id"}`) loop,
//!   4. auto-reconnects with exponential backoff on disconnect, re-registering
//!      when the broker rejects the token (e.g. its DB was reset),
//!   5. shuts down gracefully on `:quit`.
//!
//! The REPL surfaces the service via `:webhook status|reload|logs|test|replay`.
//! A shared [`MemoryAuditSink`] captures every handler outcome for the status
//! counters; TASK-273 additionally persists every delivery to
//! `~/.aish/state/webhooks/<plugin>.jsonl` (see [`crate::webhook_debug`]).
//!
//! Configuration (env vars):
//!   * `WEBHOOK_BROKER_URL`    — broker WebSocket URL (`wss://…/ws`). REQUIRED to
//!                               enable the service; unset ⇒ soft no-op. The
//!                               register URL is derived from it
//!                               (`wss://h/ws` → `https://h/clients/register`).
//!   * `WEBHOOK_PLUGIN_ID`     — broker plugin id to register for (e.g.
//!                               `hello-world`). REQUIRED alongside the URL;
//!                               unset ⇒ warning + no-op.
//!   * `WEBHOOK_TENANT_ID`     — tenant to register as (default `"default"`).
//!   * `WEBHOOK_BROKER_SECRET` — optional shared secret sent as the register
//!                               `secret`; the broker then requires an HMAC
//!                               `X-Signature` on inbound webhooks.
//!   * `WEBHOOK_CLIENT_ID`     — optional stable client id (generated if absent);
//!                               sent as the register `session_id`.
//!   * `AISH_PLUGINS_DIR`      — override the plugin directory scanned for handlers.
//!   * `AISH_WEBHOOK_AUDIT_MAX` — per-plugin delivery-log retention (default
//!                               1000; `0` disables persistence).

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use aish_webhook_client::{
    AuditRecord, AuditSink, BrokerClient, BrokerConfig, ConnState, DeliverySink,
    ExponentialBackoff, FlashSink, HandlerCounters, MemoryAuditSink, ObserverAuditSink,
    PluginRegistry, StopReason, WebhookClientError, WebhookDispatcher, WebhookService,
    transport::TungsteniteTransport,
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
    /// TASK-375 — per-plugin handler counters (dispatched/ok/failed/timeout).
    pub counters: Arc<HandlerCounters>,
}

impl Default for WebhookStatus {
    fn default() -> Self {
        Self {
            state: ConnState::Disconnected,
            connected_since: None,
            reconnects: 0,
            last_error: None,
            stopped: false,
            counters: Arc::new(HandlerCounters::new()),
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
        let Some(config) = config_from_env(broker_url) else {
            tracing::warn!(
                "webhook: WEBHOOK_BROKER_URL is set but WEBHOOK_PLUGIN_ID is not — \
                 the broker routes by (tenant, plugin); not starting the broker client"
            );
            return None;
        };
        let dir = plugins_dir();
        Some(Self::spawn(config, dir, flash))
    }

    /// Spawn the background service for an explicit config + plugin directory.
    pub fn spawn(config: BrokerConfig, plugins_dir: PathBuf, flash: Option<FlashSink>) -> Self {
        // Load plugin webhook handlers; soft-fail to an empty registry so a
        // missing/!readable plugin dir never blocks broker connectivity.
        let mut registry = match PluginRegistry::load_dir(&plugins_dir) {
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
        // TASK-274: a plugin whose config fails validation keeps its skills but
        // its webhook handlers are skipped until the config is fixed (the
        // operator was already warned once at startup / on `:plugin reload`).
        let invalid: Vec<String> = crate::plugins::config_invalid_plugins(&plugins_dir)
            .into_iter()
            .map(|(id, err)| {
                tracing::warn!(plugin = %id, error = %err,
                    "webhook: config invalid — handlers skipped until fixed");
                id
            })
            .collect();
        registry.exclude(&invalid);
        let handler_count = registry.len();
        let registry = Arc::new(registry);
        let audit = Arc::new(MemoryAuditSink::new());
        // TASK-274: mirror handler failures/timeouts into the plugin's
        // `errors.jsonl` audit trail; `:webhook logs` keeps reading `audit`.
        let observer_dir = plugins_dir.clone();
        let task_sink: Arc<dyn AuditSink> = Arc::new(ObserverAuditSink::new(
            audit.clone(),
            Arc::new(move |r: &AuditRecord| {
                if let Some(e) = handler_error_entry(r) {
                    let _ = crate::plugin_health::append(&observer_dir, &r.plugin_id, &e);
                }
            }),
        ));
        let status = Arc::new(Mutex::new(WebhookStatus::default()));
        let (shutdown_tx, shutdown_rx) = watch::channel(false);

        let task_config = config.clone();
        let task_registry = registry.clone();
        let task_audit = task_sink;
        let task_status = status.clone();
        let task_flash = flash;
        let task_delivery = crate::webhook_debug::delivery_sink(
            crate::webhook_debug::delivery_log(),
            plugins_dir.clone(),
            registry.clone(),
        );
        let join = tokio::spawn(async move {
            service_loop(
                task_config,
                task_registry,
                task_audit,
                task_status,
                shutdown_rx,
                task_flash,
                task_delivery,
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
             last event: {err}\n   \
             {handlers}",
            url = self.broker_url,
            tenant = self.tenant_id,
            h = self.handler_count,
            dir = self.plugins_dir.display(),
            ev = self.events(),
            rc = st.reconnects,
            handlers = fmt_handler_counts(&st.counters),
        )
    }
}

/// Rebuild the webhook service from the environment, reloading plugin handlers
/// from disk. Used by `:webhook reload`. Returns the reloaded handler count on
/// success. Errors (returned as a message) when no broker URL is configured.
pub fn reload(slot: &mut Option<WebhookHandle>, flash: Option<FlashSink>) -> Result<usize, String> {
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
        None => {
            Err("failed to re-initialize webhook service (is WEBHOOK_PLUGIN_ID set?)".to_string())
        }
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

/// The errors-log entry for a broker handler outcome (TASK-274): `Some` only
/// for a handler that ran and failed — `handler_timeout` when it was killed
/// at its budget, `handler_failed` otherwise. Successes and filter-skips
/// produce nothing.
fn handler_error_entry(r: &AuditRecord) -> Option<crate::plugin_health::ErrorEntry> {
    if !r.executed || r.success {
        return None;
    }
    let message = match (&r.error, r.exit_code) {
        (Some(e), _) => e.clone(),
        (None, Some(c)) => format!("exit status {c}"),
        (None, None) => "failed".to_string(),
    };
    let kind = if message.starts_with("timed out") {
        crate::plugin_health::KIND_HANDLER_TIMEOUT
    } else {
        crate::plugin_health::KIND_HANDLER_FAILED
    };
    Some(crate::plugin_health::ErrorEntry::new(
        kind,
        format!("webhook {}", r.event_type),
        message,
        "event dropped; handler runs again on the next delivery",
    ))
}

/// Build a [`BrokerConfig`] from the environment given a broker URL. Returns
/// `None` when `WEBHOOK_PLUGIN_ID` is unset/blank: the broker routes webhooks
/// by `(tenant_id, plugin_id)` and `POST /clients/register` requires both.
fn config_from_env(broker_url: String) -> Option<BrokerConfig> {
    let non_empty = |k: &str| {
        std::env::var(k)
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
    };
    Some(BrokerConfig {
        broker_url,
        tenant_id: non_empty("WEBHOOK_TENANT_ID").unwrap_or_else(|| "default".to_string()),
        plugin: Some(non_empty("WEBHOOK_PLUGIN_ID")?),
        transport: "websocket".to_string(),
        enabled: true,
        secret: non_empty("WEBHOOK_BROKER_SECRET"),
        client_id: non_empty("WEBHOOK_CLIENT_ID"),
    })
}

/// Resolve the plugin directory scanned for webhook handlers.
pub(crate) fn plugins_dir() -> PathBuf {
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
/// TASK-375 — handler health for `:webhook status`: totals, plus a per-plugin
/// `id ok/run` list (failures/timeouts flagged) once anything has run.
fn fmt_handler_counts(counters: &HandlerCounters) -> String {
    let t = counters.totals();
    let mut out = format!(
        "handlers run: {}  ok: {}  failed: {}  timeout: {}  avg: {}ms",
        t.dispatched,
        t.ok,
        t.failed,
        t.timeout,
        t.avg_ms()
    );
    let per = counters.per_plugin();
    if !per.is_empty() {
        let parts: Vec<String> = per
            .iter()
            .map(|(id, c)| {
                let mut p = format!("{id} {}/{}", c.ok, c.dispatched);
                if c.failed > 0 {
                    p.push_str(&format!(" ✗{}", c.failed));
                }
                if c.timeout > 0 {
                    p.push_str(&format!(" ⏱{}", c.timeout));
                }
                p
            })
            .collect();
        out.push_str(&format!("\n   plugins: {}", parts.join(", ")));
    }
    out
}

fn tail<T>(mut v: Vec<T>, n: usize) -> Vec<T> {
    let len = v.len();
    if len > n { v.split_off(len - n) } else { v }
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
    audit: Arc<dyn AuditSink>,
    status: Arc<Mutex<WebhookStatus>>,
    shutdown_rx: watch::Receiver<bool>,
    flash: Option<FlashSink>,
    delivery: DeliverySink,
) {
    let mut dispatcher = WebhookDispatcher::new(registry)
        .with_audit_sink(audit)
        .with_delivery_sink(delivery)
        .with_counters(status.lock().unwrap().counters.clone());
    if let Some(f) = flash {
        // Wire the broker dispatcher to the SecondStatusLine: a handler's stdout
        // now surfaces on the footer. This is the seam that completes the goal.
        dispatcher = dispatcher.with_flash_sink(f);
    }
    let dispatcher = Arc::new(dispatcher);

    // Session token issued by `POST /clients/register`. Reused across
    // reconnects (the broker persists it); dropped and re-issued only when the
    // broker rejects it with `auth_error`.
    let mut session: Option<(String, String)> = None; // (client_id, session_token)
    let mut register_backoff = ExponentialBackoff::default();

    loop {
        if *shutdown_rx.borrow() {
            break;
        }

        set_state(&status, ConnState::Connecting);
        let mut client = BrokerClient::new(config.clone());

        // 1. Register (HTTP) when we hold no valid token.
        match session.clone() {
            Some((client_id, token)) => {
                let mut cfg = config.clone();
                cfg.client_id = Some(client_id);
                client = BrokerClient::new(cfg).with_session_token(token);
            }
            None => {
                let registered = tokio::select! {
                    r = client.register() => Some(r),
                    _ = wait_for_shutdown(shutdown_rx.clone()) => None,
                };
                match registered {
                    None => break,
                    Some(Ok(resp)) => {
                        register_backoff.reset();
                        tracing::info!(client_id = %resp.client_id, "webhook: registered with broker");
                        session = Some((resp.client_id, resp.session_token));
                    }
                    Some(Err(e)) => {
                        let wait = register_backoff.next_backoff();
                        tracing::warn!(error = %e, ?wait, "webhook: broker registration failed, retrying");
                        status.lock().unwrap().last_error = Some(e.to_string());
                        tokio::select! {
                            _ = tokio::time::sleep(wait) => continue,
                            _ = wait_for_shutdown(shutdown_rx.clone()) => break,
                        }
                    }
                }
            }
        }

        // 2. Dial + authenticate. reconnect_with_backoff loops forever
        // (max_attempts = None) on transport errors but returns immediately on
        // an auth rejection; race it against shutdown so `:quit` interrupts an
        // in-progress reconnect.
        let url = config.broker_url.clone();
        let connected = tokio::select! {
            r = client.reconnect_with_backoff(|| TungsteniteTransport::connect(&url), None) => Some(r),
            _ = wait_for_shutdown(shutdown_rx.clone()) => None,
        };
        match connected {
            None => break,
            Some(Ok(())) => register_backoff.reset(),
            Some(Err(e)) => {
                if matches!(e, WebhookClientError::Auth(_)) {
                    // Token unknown to the broker (e.g. its DB was reset on
                    // redeploy): forget it and re-register next iteration.
                    session = None;
                }
                tracing::warn!(error = %e, "webhook: broker auth failed — re-registering");
                status.lock().unwrap().last_error = Some(e.to_string());
                let wait = register_backoff.next_backoff();
                tokio::select! {
                    _ = tokio::time::sleep(wait) => continue,
                    _ = wait_for_shutdown(shutdown_rx.clone()) => break,
                }
            }
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
    fn handler_counts_render_totals_and_per_plugin() {
        let c = HandlerCounters::new();
        assert_eq!(
            fmt_handler_counts(&c),
            "handlers run: 0  ok: 0  failed: 0  timeout: 0  avg: 0ms"
        );
        let out = |plugin: &str, success: bool, error: Option<&str>| {
            aish_webhook_client::HandlerOutcome {
                plugin_id: plugin.into(),
                event_type: "push".into(),
                matched: true,
                executed: true,
                exit_code: None,
                success,
                stdout: String::new(),
                stderr: String::new(),
                error: error.map(str::to_string),
                duration_ms: 20,
            }
        };
        c.record(&out("gh", true, None));
        c.record(&out("gh", false, None));
        c.record(&out("slack", false, Some("timed out after 30s")));
        let s = fmt_handler_counts(&c);
        assert!(s.starts_with("handlers run: 3  ok: 1  failed: 1  timeout: 1  avg: 20ms"));
        assert!(s.contains("plugins: gh 1/2 ✗1, slack 0/1 ⏱1"), "{s}");
    }

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
            std::env::set_var("WEBHOOK_PLUGIN_ID", "hello-world");
            std::env::set_var("WEBHOOK_BROKER_SECRET", "s3cr3t");
            std::env::remove_var("WEBHOOK_CLIENT_ID");
        }
        let cfg = config_from_env("wss://broker.example/ws".to_string()).expect("plugin id set");
        assert_eq!(cfg.broker_url, "wss://broker.example/ws");
        assert_eq!(cfg.tenant_id, "acme");
        assert_eq!(cfg.plugin.as_deref(), Some("hello-world"));
        assert_eq!(cfg.secret.as_deref(), Some("s3cr3t"));
        assert_eq!(cfg.client_id, None);
        assert!(cfg.enabled);
        assert_eq!(cfg.transport, "websocket");
        unsafe {
            std::env::remove_var("WEBHOOK_TENANT_ID");
            std::env::remove_var("WEBHOOK_PLUGIN_ID");
            std::env::remove_var("WEBHOOK_BROKER_SECRET");
        }
    }

    #[test]
    fn config_from_env_requires_plugin_id() {
        let _g = ENV_LOCK.lock().unwrap();
        // SAFETY: guarded by ENV_LOCK.
        unsafe {
            std::env::remove_var("WEBHOOK_PLUGIN_ID");
        }
        assert!(config_from_env("wss://broker.example/ws".to_string()).is_none());
        unsafe {
            std::env::set_var("WEBHOOK_PLUGIN_ID", "   ");
        }
        assert!(config_from_env("wss://broker.example/ws".to_string()).is_none());
        unsafe {
            std::env::remove_var("WEBHOOK_PLUGIN_ID");
        }
    }

    #[test]
    fn spawn_from_env_none_without_plugin_id() {
        let _g = ENV_LOCK.lock().unwrap();
        // SAFETY: guarded by ENV_LOCK.
        unsafe {
            std::env::set_var("WEBHOOK_BROKER_URL", "ws://127.0.0.1:9/ws");
            std::env::remove_var("WEBHOOK_PLUGIN_ID");
        }
        // Returns before any tokio::spawn, so no runtime is needed.
        assert!(WebhookHandle::spawn_from_env(None).is_none());
        unsafe {
            std::env::remove_var("WEBHOOK_BROKER_URL");
        }
    }

    #[test]
    fn bundled_hello_world_declares_ping_webhook() {
        // TASK-449: the broker e2e relies on hello-world loading a `ping`
        // handler from `webhooks[]` (the legacy `webhook_command` loads none).
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("plugins");
        let reg = PluginRegistry::load_dir(&dir).unwrap();
        let hits = reg.matching("ping");
        let (pid, h) = hits
            .iter()
            .find(|(pid, _)| *pid == "hello-world")
            .expect("hello-world ping handler");
        assert_eq!(*pid, "hello-world");
        assert!(
            h.command[0].ends_with("plugins/hello-world/handlers/ping.sh"),
            "resolved against the plugin dir: {:?}",
            h.command
        );
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

    /// TASK-274: only executed-and-failed handlers become errors-log entries,
    /// classified timeout vs failure.
    #[test]
    fn handler_error_entry_classifies_outcomes() {
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
        assert!(handler_error_entry(&ok).is_none());
        let filtered = AuditRecord {
            executed: false,
            success: false,
            exit_code: None,
            ..ok.clone()
        };
        assert!(handler_error_entry(&filtered).is_none());

        let failed = AuditRecord {
            success: false,
            exit_code: Some(2),
            ..ok.clone()
        };
        let e = handler_error_entry(&failed).unwrap();
        assert_eq!(e.kind, "handler_failed");
        assert_eq!(e.source, "webhook pull_request");
        assert_eq!(e.message, "exit status 2");

        let timed_out = AuditRecord {
            success: false,
            exit_code: None,
            error: Some("timed out after 30s".into()),
            ..ok
        };
        assert_eq!(
            handler_error_entry(&timed_out).unwrap().kind,
            "handler_timeout"
        );
    }
}
