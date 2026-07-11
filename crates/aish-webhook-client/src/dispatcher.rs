//! TASK-268 — Handler Dispatch (Phase 5).
//!
//! Loads webhook handlers from `plugin.json` manifests, matches incoming events
//! to handlers, applies filters, and fork/exec's each matching handler with a
//! per-handler timeout and full error isolation (one handler failing never
//! blocks another). No shell is ever involved.

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt;

use crate::audit::{AuditRecord, AuditSink, NoopAuditSink};
use crate::envelope::Webhook;
use crate::error::Result;

/// Default per-handler execution timeout.
pub const DEFAULT_HANDLER_TIMEOUT: Duration = Duration::from_secs(30);

/// One webhook handler declared by a plugin.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct WebhookHandler {
    /// Event this handler subscribes to (`"pull_request"`, `"*"` = all).
    pub event_type: String,
    /// argv to fork/exec. `command[0]` is the program; the rest are args.
    pub command: Vec<String>,
    /// AND-combined equality filters over dotted payload paths.
    #[serde(default)]
    pub filters: serde_json::Map<String, serde_json::Value>,
    /// Optional per-handler timeout override (seconds).
    #[serde(default)]
    pub timeout_secs: Option<u64>,
}

/// A plugin manifest (`plugin.json`) — only the fields we need.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PluginManifest {
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub version: String,
    /// Declared webhook handlers.
    #[serde(default)]
    pub webhooks: Vec<WebhookHandler>,
}

/// In-memory registry of loaded plugin manifests.
#[derive(Debug, Clone, Default)]
pub struct PluginRegistry {
    plugins: Vec<PluginManifest>,
}

impl PluginRegistry {
    /// Build from already-parsed manifests.
    pub fn from_plugins(plugins: Vec<PluginManifest>) -> Self {
        Self { plugins }
    }

    /// Load every `<root>/*/plugin.json` into the registry. Unreadable or
    /// malformed manifests are skipped with a warning (one bad plugin must not
    /// sink the rest).
    pub fn load_dir(root: impl AsRef<Path>) -> Result<Self> {
        let mut plugins = Vec::new();
        let root = root.as_ref();
        if !root.is_dir() {
            return Ok(Self { plugins });
        }
        for entry in std::fs::read_dir(root)? {
            let entry = match entry {
                Ok(e) => e,
                Err(_) => continue,
            };
            let manifest_path = entry.path().join("plugin.json");
            if !manifest_path.is_file() {
                continue;
            }
            match std::fs::read_to_string(&manifest_path)
                .ok()
                .and_then(|raw| serde_json::from_str::<PluginManifest>(&raw).ok())
            {
                Some(m) => plugins.push(m),
                None => tracing::warn!(path = %manifest_path.display(), "skipping malformed plugin.json"),
            }
        }
        Ok(Self { plugins })
    }

    pub fn len(&self) -> usize {
        self.plugins.len()
    }
    pub fn is_empty(&self) -> bool {
        self.plugins.is_empty()
    }

    /// All (plugin_id, handler) pairs subscribed to `event_type` (or `"*"`).
    pub fn matching(&self, event_type: &str) -> Vec<(&str, &WebhookHandler)> {
        let mut out = Vec::new();
        for p in &self.plugins {
            for h in &p.webhooks {
                if h.event_type == event_type || h.event_type == "*" {
                    out.push((p.id.as_str(), h));
                }
            }
        }
        out
    }
}

/// Result of attempting one handler.
#[derive(Debug, Clone, Serialize)]
pub struct HandlerOutcome {
    pub plugin_id: String,
    pub event_type: String,
    /// Event matched this handler's subscription.
    pub matched: bool,
    /// Passed filters and was fork/exec'd.
    pub executed: bool,
    /// Process exit code (None if it never ran or was killed).
    pub exit_code: Option<i32>,
    /// exit_code == Some(0).
    pub success: bool,
    pub stdout: String,
    pub stderr: String,
    /// Populated on spawn failure or timeout.
    pub error: Option<String>,
    pub duration_ms: u128,
}

impl HandlerOutcome {
    fn skipped(plugin_id: &str, event_type: &str) -> Self {
        Self {
            plugin_id: plugin_id.to_string(),
            event_type: event_type.to_string(),
            matched: true,
            executed: false,
            exit_code: None,
            success: false,
            stdout: String::new(),
            stderr: String::new(),
            error: None,
            duration_ms: 0,
        }
    }
}

/// Look up a dotted path (`"a.b.c"`) in a JSON value.
fn get_path<'a>(v: &'a serde_json::Value, path: &str) -> Option<&'a serde_json::Value> {
    let mut cur = v;
    for seg in path.split('.') {
        cur = cur.get(seg)?;
    }
    Some(cur)
}

/// AND-combined equality filter check over the webhook payload.
pub fn passes_filters(
    payload: &serde_json::Value,
    filters: &serde_json::Map<String, serde_json::Value>,
) -> bool {
    filters
        .iter()
        .all(|(k, expected)| get_path(payload, k).map(|got| got == expected).unwrap_or(false))
}

/// Dispatches webhooks to plugin handlers (Phase 5 seam realized).
pub struct WebhookDispatcher {
    registry: Arc<PluginRegistry>,
    default_timeout: Duration,
    audit: Arc<dyn AuditSink>,
}

impl WebhookDispatcher {
    pub fn new(registry: Arc<PluginRegistry>) -> Self {
        Self {
            registry,
            default_timeout: DEFAULT_HANDLER_TIMEOUT,
            audit: Arc::new(NoopAuditSink),
        }
    }

    pub fn with_default_timeout(mut self, d: Duration) -> Self {
        self.default_timeout = d;
        self
    }

    /// Attach an audit sink (TASK-268 §5.5). Every dispatched outcome —
    /// executed, filtered, or failed — is written to the sink so handler
    /// activity is durably auditable. Defaults to a no-op sink.
    pub fn with_audit_sink(mut self, sink: Arc<dyn AuditSink>) -> Self {
        self.audit = sink;
        self
    }

    pub fn registry(&self) -> &PluginRegistry {
        &self.registry
    }

    /// Dispatch a webhook to every matching handler concurrently, isolating
    /// failures. Returns one [`HandlerOutcome`] per matching handler.
    pub async fn dispatch(&self, webhook: &Webhook) -> Vec<HandlerOutcome> {
        let matches = self.registry.matching(&webhook.event_type);
        if matches.is_empty() {
            tracing::debug!(event_type = %webhook.event_type, "no handlers matched");
            return Vec::new();
        }

        let payload = Arc::new(webhook.payload.clone());
        let mut tasks = Vec::with_capacity(matches.len());

        for (plugin_id, handler) in matches {
            let plugin_id = plugin_id.to_string();
            let event_type = webhook.event_type.clone();

            if !passes_filters(&payload, &handler.filters) {
                tracing::debug!(%plugin_id, %event_type, "filtered out");
                // Represent as a synchronous, already-resolved outcome.
                let out = HandlerOutcome::skipped(&plugin_id, &event_type);
                tasks.push(tokio::spawn(async move { out }));
                continue;
            }

            let command = handler.command.clone();
            let timeout = handler
                .timeout_secs
                .map(Duration::from_secs)
                .unwrap_or(self.default_timeout);
            let env = HandlerEnv {
                id: webhook.id.clone(),
                tenant_id: webhook.tenant_id.clone(),
                plugin_id: plugin_id.clone(),
                event_type: event_type.clone(),
            };
            let payload = payload.clone();

            tasks.push(tokio::spawn(async move {
                run_handler(&command, &env, &payload, timeout).await
            }));
        }

        let mut outcomes = Vec::with_capacity(tasks.len());
        for t in tasks {
            match t.await {
                Ok(o) => outcomes.push(o),
                Err(join_err) => {
                    // A panicking handler task must not abort the batch.
                    tracing::error!(error = %join_err, "handler task panicked");
                }
            }
        }

        for o in &outcomes {
            if o.executed && !o.success {
                tracing::warn!(plugin_id = %o.plugin_id, event_type = %o.event_type,
                    exit_code = ?o.exit_code, error = ?o.error, "handler failed");
            } else if o.executed {
                tracing::info!(plugin_id = %o.plugin_id, event_type = %o.event_type,
                    duration_ms = o.duration_ms, "handler ok");
            }
        }

        // TASK-268 §5.5 — persist an auditable record of every outcome
        // (executed, filtered, or failed). A sink write must never break
        // dispatch, so failures are logged and swallowed.
        for o in &outcomes {
            let rec = AuditRecord::from_outcome(&webhook.id, &webhook.tenant_id, o);
            if let Err(e) = self.audit.record(&rec).await {
                tracing::warn!(error = %e, plugin_id = %o.plugin_id,
                    "audit sink write failed");
            }
        }

        outcomes
    }
}

struct HandlerEnv {
    id: String,
    tenant_id: String,
    plugin_id: String,
    event_type: String,
}

/// Fork/exec one handler: no shell, payload on stdin, `WEBHOOK_*` in the env,
/// timeout-bounded (kill-on-drop), stdout/stderr captured.
async fn run_handler(
    command: &[String],
    env: &HandlerEnv,
    payload: &serde_json::Value,
    timeout: Duration,
) -> HandlerOutcome {
    let started = Instant::now();
    let mut outcome = HandlerOutcome {
        plugin_id: env.plugin_id.clone(),
        event_type: env.event_type.clone(),
        matched: true,
        executed: true,
        exit_code: None,
        success: false,
        stdout: String::new(),
        stderr: String::new(),
        error: None,
        duration_ms: 0,
    };

    if command.is_empty() {
        outcome.executed = false;
        outcome.error = Some("empty command".into());
        return outcome;
    }

    let payload_bytes = serde_json::to_vec(payload).unwrap_or_else(|_| b"{}".to_vec());

    let mut cmd = tokio::process::Command::new(&command[0]);
    cmd.args(&command[1..])
        .env("WEBHOOK_ID", &env.id)
        .env("WEBHOOK_TENANT_ID", &env.tenant_id)
        .env("WEBHOOK_PLUGIN_ID", &env.plugin_id)
        .env("WEBHOOK_EVENT_TYPE", &env.event_type)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            outcome.error = Some(format!("spawn failed: {e}"));
            outcome.duration_ms = started.elapsed().as_millis();
            return outcome;
        }
    };

    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(&payload_bytes).await;
        let _ = stdin.shutdown().await;
        drop(stdin);
    }

    match tokio::time::timeout(timeout, child.wait_with_output()).await {
        Ok(Ok(output)) => {
            outcome.exit_code = output.status.code();
            outcome.success = output.status.success();
            outcome.stdout = String::from_utf8_lossy(&output.stdout).into_owned();
            outcome.stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        }
        Ok(Err(e)) => {
            outcome.error = Some(format!("wait failed: {e}"));
        }
        Err(_) => {
            // Timed out: the future (and thus child) drops here → kill_on_drop.
            outcome.error = Some(format!("timed out after {timeout:?}"));
        }
    }
    outcome.duration_ms = started.elapsed().as_millis();
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn wh(event_type: &str, payload: serde_json::Value) -> Webhook {
        Webhook {
            id: "d1".into(),
            tenant_id: "t1".into(),
            plugin_id: "".into(),
            event_type: event_type.into(),
            payload,
        }
    }

    #[test]
    fn dotted_path_and_filters() {
        let p = json!({"action":"opened","pr":{"base":{"ref":"main"}}});
        assert_eq!(get_path(&p, "action").unwrap(), &json!("opened"));
        assert_eq!(get_path(&p, "pr.base.ref").unwrap(), &json!("main"));
        assert!(get_path(&p, "pr.missing").is_none());

        let mut f = serde_json::Map::new();
        f.insert("action".into(), json!("opened"));
        f.insert("pr.base.ref".into(), json!("main"));
        assert!(passes_filters(&p, &f));

        f.insert("action".into(), json!("closed"));
        assert!(!passes_filters(&p, &f));
    }

    #[test]
    fn registry_matching_and_wildcard() {
        let reg = PluginRegistry::from_plugins(vec![
            PluginManifest {
                id: "a".into(),
                name: "A".into(),
                version: "1".into(),
                webhooks: vec![WebhookHandler {
                    event_type: "pull_request".into(),
                    command: vec!["true".into()],
                    filters: Default::default(),
                    timeout_secs: None,
                }],
            },
            PluginManifest {
                id: "b".into(),
                name: "B".into(),
                version: "1".into(),
                webhooks: vec![WebhookHandler {
                    event_type: "*".into(),
                    command: vec!["true".into()],
                    filters: Default::default(),
                    timeout_secs: None,
                }],
            },
        ]);
        let m = reg.matching("pull_request");
        assert_eq!(m.len(), 2);
        let m2 = reg.matching("issues");
        assert_eq!(m2.len(), 1); // only wildcard
        assert_eq!(m2[0].0, "b");
    }

    #[tokio::test]
    async fn two_plugins_same_event_both_run_and_errors_isolated() {
        // Plugin A: `true` (exit 0). Plugin B: `false` (exit 1).
        let reg = PluginRegistry::from_plugins(vec![
            PluginManifest {
                id: "ok".into(),
                name: "".into(),
                version: "".into(),
                webhooks: vec![WebhookHandler {
                    event_type: "pull_request".into(),
                    command: vec!["true".into()],
                    filters: Default::default(),
                    timeout_secs: None,
                }],
            },
            PluginManifest {
                id: "fail".into(),
                name: "".into(),
                version: "".into(),
                webhooks: vec![WebhookHandler {
                    event_type: "pull_request".into(),
                    command: vec!["false".into()],
                    filters: Default::default(),
                    timeout_secs: None,
                }],
            },
        ]);
        let d = WebhookDispatcher::new(Arc::new(reg));
        let outs = d.dispatch(&wh("pull_request", json!({}))).await;
        assert_eq!(outs.len(), 2);
        let ok = outs.iter().find(|o| o.plugin_id == "ok").unwrap();
        let fail = outs.iter().find(|o| o.plugin_id == "fail").unwrap();
        assert!(ok.executed && ok.success);
        assert!(fail.executed && !fail.success);
        assert_eq!(fail.exit_code, Some(1));
    }

    #[tokio::test]
    async fn handler_receives_payload_on_stdin() {
        // `cat` echoes stdin → stdout so we can assert the payload was piped in.
        let reg = PluginRegistry::from_plugins(vec![PluginManifest {
            id: "echo".into(),
            name: "".into(),
            version: "".into(),
            webhooks: vec![WebhookHandler {
                event_type: "issues".into(),
                command: vec!["cat".into()],
                filters: Default::default(),
                timeout_secs: None,
            }],
        }]);
        let d = WebhookDispatcher::new(Arc::new(reg));
        let outs = d.dispatch(&wh("issues", json!({"hello":"world"}))).await;
        assert_eq!(outs.len(), 1);
        assert!(outs[0].success);
        assert!(outs[0].stdout.contains("hello"));
        assert!(outs[0].stdout.contains("world"));
    }

    #[tokio::test]
    async fn handler_timeout_is_bounded() {
        let reg = PluginRegistry::from_plugins(vec![PluginManifest {
            id: "slow".into(),
            name: "".into(),
            version: "".into(),
            webhooks: vec![WebhookHandler {
                event_type: "push".into(),
                command: vec!["sleep".into(), "5".into()],
                filters: Default::default(),
                timeout_secs: None,
            }],
        }]);
        let d = WebhookDispatcher::new(Arc::new(reg))
            .with_default_timeout(Duration::from_millis(150));
        let start = Instant::now();
        let outs = d.dispatch(&wh("push", json!({}))).await;
        assert!(start.elapsed() < Duration::from_secs(2), "must not wait full 5s");
        assert_eq!(outs.len(), 1);
        assert!(outs[0].error.as_deref().unwrap_or("").contains("timed out"));
        assert!(!outs[0].success);
    }

    #[tokio::test]
    async fn filtered_handler_is_not_executed() {
        let mut filters = serde_json::Map::new();
        filters.insert("action".into(), json!("opened"));
        let reg = PluginRegistry::from_plugins(vec![PluginManifest {
            id: "guarded".into(),
            name: "".into(),
            version: "".into(),
            webhooks: vec![WebhookHandler {
                event_type: "pull_request".into(),
                command: vec!["true".into()],
                filters,
                timeout_secs: None,
            }],
        }]);
        let d = WebhookDispatcher::new(Arc::new(reg));
        let outs = d.dispatch(&wh("pull_request", json!({"action":"closed"}))).await;
        assert_eq!(outs.len(), 1);
        assert!(!outs[0].executed);
        assert!(outs[0].matched);
    }

    #[test]
    fn load_dir_reads_manifests() {
        let base = std::env::temp_dir().join(format!("aish-wh-{}", std::process::id()));
        let pdir = base.join("plug-a");
        std::fs::create_dir_all(&pdir).unwrap();
        std::fs::write(
            pdir.join("plugin.json"),
            r#"{"id":"plug-a","webhooks":[{"event_type":"pull_request","command":["true"]}]}"#,
        )
        .unwrap();
        let reg = PluginRegistry::load_dir(&base).unwrap();
        assert_eq!(reg.len(), 1);
        assert_eq!(reg.matching("pull_request").len(), 1);
        let _ = std::fs::remove_dir_all(&base);
    }

    /// TASK-446 — shell-injection regression guard.
    ///
    /// Handlers are fork/exec'd (`Command::new(&command[0]).args(..)`) with NO
    /// shell, so metacharacters in a command argument or in the JSON payload
    /// MUST be passed through literally and never interpreted. If a shell were
    /// ever (re)introduced into the dispatch path, the `;`/`&&`/`$()` payloads
    /// below would create sentinel files and this test would fail — locking in
    /// the no-shell guarantee.
    #[tokio::test]
    async fn command_args_and_payload_are_not_shell_interpreted() {
        let uniq = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("aish-wh-inject-{}-{uniq}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        // 1) Metacharacters in a COMMAND ARGUMENT must stay literal.
        let sentinel = dir.join("PWNED_ARG");
        assert!(!sentinel.exists());
        let injection = format!(
            "hi; touch {s} && echo x $(touch {s})",
            s = sentinel.display()
        );
        let reg = PluginRegistry::from_plugins(vec![PluginManifest {
            id: "inject".into(),
            name: "".into(),
            version: "".into(),
            webhooks: vec![WebhookHandler {
                event_type: "push".into(),
                command: vec!["echo".into(), injection.clone()],
                filters: Default::default(),
                timeout_secs: None,
            }],
        }]);
        let d = WebhookDispatcher::new(Arc::new(reg));
        let outs = d.dispatch(&wh("push", json!({}))).await;
        assert_eq!(outs.len(), 1);
        assert!(outs[0].executed && outs[0].success);
        // The metacharacters were echoed back verbatim (proving `echo` saw them
        // as a single literal arg)...
        assert!(outs[0].stdout.contains("; touch"));
        assert!(outs[0].stdout.contains("$(touch"));
        // ...and NO shell side effect fired.
        assert!(
            !sentinel.exists(),
            "shell injection executed via command arg — sentinel was created"
        );

        // 2) Metacharacters in the JSON PAYLOAD (delivered on stdin, never to a
        // shell) must also be inert.
        let sentinel2 = dir.join("PWNED_STDIN");
        let reg2 = PluginRegistry::from_plugins(vec![PluginManifest {
            id: "inject2".into(),
            name: "".into(),
            version: "".into(),
            webhooks: vec![WebhookHandler {
                event_type: "push".into(),
                command: vec!["cat".into()],
                filters: Default::default(),
                timeout_secs: None,
            }],
        }]);
        let d2 = WebhookDispatcher::new(Arc::new(reg2));
        let evil = json!({"x": format!("$(touch {s}); touch {s}", s = sentinel2.display())});
        let outs2 = d2.dispatch(&wh("push", evil)).await;
        assert_eq!(outs2.len(), 1);
        assert!(outs2[0].success);
        assert!(
            !sentinel2.exists(),
            "shell injection executed via stdin payload — sentinel was created"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
