//! Plugin webhook dispatcher — event → plugin routing (Phase 1.6).
//!
//! A minimal, non-blocking event router. Lifecycle events (a workspace opening,
//! a skill loading, a background job starting/finishing, a tool being invoked)
//! are turned into a [`PluginEvent`] and delivered to every plugin that opted in
//! via its `plugin.json` manifest:
//!
//!   * `"webhook_url"`     — an HTTP endpoint the event is POSTed to (JSON body).
//!   * `"webhook_command"` — a shell command run with the event JSON on stdin;
//!                           its captured output lands in the plugin state store
//!                           under `<plugin_id>:last_webhook_output`.
//!
//! Delivery is **fire-and-forget**: [`PluginDispatcher::route`] reads the
//! manifests (a cheap directory scan), spawns one detached `tokio` task per
//! subscriber, and returns immediately — a slow endpoint or a hung command can
//! never stall the shell. Timeout + retry policy beyond a single bounded HTTP
//! timeout is deferred to Phase 2 (see `docs/plugin-webhook-events.md`).
//!
//! Scope for this phase is the read-only MVP: events flow OUT to plugins; no
//! plugin can yet mutate shell state through a hook (Phase 2+).
//!
//! Testability: this module is self-contained apart from
//! [`crate::plugin_state::PluginStateStore`], so `tests/plugin_dispatcher_tests.rs`
//! compiles it (and `plugin_state.rs`) directly via `#[path]` — `crate::` here
//! resolves to the test crate root, which declares the same sibling modules.

// The public surface (event variants, the awaiting route, the global accessor)
// exists for hook sites landing across Phase 1.6+; only some is wired today.
#![allow(dead_code)]

use crate::plugin_state::PluginStateStore;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// A handle to the SecondStatusLine's single most-recent-wins "flash" slot
/// (`session.flash`). When a subscribed plugin's `webhook_command` prints a
/// line, the dispatcher drops it here (capped at 60 chars) so the plugin's
/// reaction to a received webhook surfaces live on the footer's 2nd statusline.
/// `Option`al so the dispatcher stays usable (and unit-testable) without a live
/// session — tests construct it with no sink.
pub type FlashSink = Arc<Mutex<Option<String>>>;

/// Lifecycle events a plugin can subscribe to. The wire name (`as_str`) is the
/// stable identifier sent to webhooks and logged on the `plugin-events` channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    WorkspaceOpen,
    SkillLoaded,
    BackgroundJobStart,
    BackgroundJobComplete,
    ToolInvoked,
}

impl Event {
    /// Stable snake_case wire name for this event.
    pub fn as_str(self) -> &'static str {
        match self {
            Event::WorkspaceOpen => "workspace_open",
            Event::SkillLoaded => "skill_loaded",
            Event::BackgroundJobStart => "background_job_start",
            Event::BackgroundJobComplete => "background_job_complete",
            Event::ToolInvoked => "tool_invoked",
        }
    }
}

/// The event payload delivered to a plugin. Serialized as the HTTP POST body and
/// piped to a `webhook_command` on stdin.
#[derive(Debug, Clone, Serialize)]
pub struct PluginEvent {
    pub plugin_id: String,
    pub event_type: String,
    /// Unix epoch seconds when the event was routed.
    pub timestamp: u64,
    /// Event-specific payload (may be an empty object).
    pub payload_json: Value,
}

/// The two webhook fields the dispatcher cares about, parsed straight from
/// `plugin.json`. A deliberately narrow view of the manifest so this module
/// stays decoupled from `crate::plugins::PluginManifest` (keeps the `#[path]`
/// test include free of the `crate::plugins`/`crate::skills` cascade).
#[derive(Debug, Clone, Deserialize)]
struct WebhookManifest {
    id: String,
    #[serde(default)]
    enabled: Option<bool>,
    #[serde(default)]
    webhook_url: Option<String>,
    #[serde(default)]
    webhook_command: Option<String>,
}

/// A resolved subscriber: which plugin, and how to reach it.
#[derive(Debug, Clone)]
struct Subscriber {
    plugin_id: String,
    url: Option<String>,
    command: Option<String>,
}

/// Routes [`Event`]s to plugin webhooks. Cheap to `clone` (the state store is an
/// `Arc` handle and `reqwest::Client` clones share a connection pool), so it is
/// trivially handed to spawned tasks and stored in a global.
#[derive(Clone)]
pub struct PluginDispatcher {
    plugins_dir: PathBuf,
    state: PluginStateStore,
    http: reqwest::Client,
    http_timeout: Duration,
    /// Optional SecondStatusLine sink (see [`FlashSink`]). `None` in tests.
    flash: Option<FlashSink>,
}

impl PluginDispatcher {
    /// Build a dispatcher rooted at `plugins_dir` (each `<plugins_dir>/<id>/`
    /// holds a `plugin.json`) writing command output to `state`.
    pub fn new(plugins_dir: impl Into<PathBuf>, state: PluginStateStore) -> Self {
        Self {
            plugins_dir: plugins_dir.into(),
            state,
            http: reqwest::Client::new(),
            http_timeout: Duration::from_secs(10),
            flash: None,
        }
    }

    /// Attach a SecondStatusLine [`FlashSink`] — the slot a subscribed plugin's
    /// command output is surfaced to. Builder-style; consumes and returns self.
    pub fn with_flash(mut self, flash: FlashSink) -> Self {
        self.flash = Some(flash);
        self
    }

    /// Scan every enabled plugin manifest and collect the ones subscribing to a
    /// webhook (either `webhook_url` or `webhook_command`). A missing dir or an
    /// unparseable manifest is skipped silently — a broken plugin never blocks
    /// dispatch (mirrors `crate::plugins::discover`).
    fn subscribers(&self) -> Vec<Subscriber> {
        let mut out = Vec::new();
        let Ok(entries) = std::fs::read_dir(&self.plugins_dir) else {
            return out;
        };
        for entry in entries.flatten() {
            let manifest_path = entry.path().join("plugin.json");
            let Ok(text) = std::fs::read_to_string(&manifest_path) else {
                continue;
            };
            let Ok(m) = serde_json::from_str::<WebhookManifest>(&text) else {
                continue;
            };
            if !m.enabled.unwrap_or(true) {
                continue;
            }
            if m.webhook_url.is_none() && m.webhook_command.is_none() {
                continue;
            }
            out.push(Subscriber {
                plugin_id: m.id,
                url: m.webhook_url,
                command: m.webhook_command,
            });
        }
        out.sort_by(|a, b| a.plugin_id.cmp(&b.plugin_id));
        out
    }

    /// Build the [`PluginEvent`] a given subscriber receives for `event`.
    fn make_event(&self, plugin_id: &str, event: Event) -> PluginEvent {
        let payload = match event {
            Event::WorkspaceOpen => json!({
                "cwd": std::env::current_dir()
                    .map(|p| p.to_string_lossy().into_owned())
                    .unwrap_or_default(),
            }),
            _ => json!({}),
        };
        PluginEvent {
            plugin_id: plugin_id.to_string(),
            event_type: event.as_str().to_string(),
            timestamp: now_secs(),
            payload_json: payload,
        }
    }

    /// Route `event` to all subscribers — **non-blocking**. Reads manifests, then
    /// spawns one detached `tokio` task per delivery and returns immediately;
    /// nothing awaits the HTTP/command completion. Must be called from within a
    /// `tokio` runtime (aish's `#[tokio::main]` satisfies this).
    pub fn route(&self, event: Event) -> Result<(), String> {
        let subs = self.subscribers();
        if !subs.is_empty() {
            log_channel(&format!(
                "dispatch {} -> {} plugin(s)",
                event.as_str(),
                subs.len()
            ));
        }
        for sub in subs {
            let ev = self.make_event(&sub.plugin_id, event);
            let this = self.clone();
            tokio::spawn(async move {
                this.deliver(sub, ev).await;
            });
        }
        Ok(())
    }

    /// Like [`route`], but AWAITS every delivery and returns how many ran.
    /// Deterministic — used by tests; production wiring uses the fire-and-forget
    /// [`route`].
    pub async fn route_awaiting(&self, event: Event) -> Result<usize, String> {
        let subs = self.subscribers();
        let mut handles = Vec::new();
        for sub in subs {
            let ev = self.make_event(&sub.plugin_id, event);
            let this = self.clone();
            handles.push(tokio::spawn(async move { this.deliver(sub, ev).await }));
        }
        let n = handles.len();
        for h in handles {
            let _ = h.await;
        }
        Ok(n)
    }

    /// Deliver one event to one subscriber: HTTP POST if a URL is set, and/or run
    /// the command if one is set. Errors are logged, never propagated — a failed
    /// delivery must not crash the spawning task.
    async fn deliver(&self, sub: Subscriber, ev: PluginEvent) {
        if let Some(url) = &sub.url {
            match self
                .http
                .post(url)
                .timeout(self.http_timeout)
                .json(&ev)
                .send()
                .await
            {
                Ok(resp) => log_channel(&format!(
                    "{} http {} -> {} ({})",
                    ev.plugin_id,
                    ev.event_type,
                    url,
                    resp.status().as_u16()
                )),
                Err(e) => log_channel(&format!(
                    "{} http {} -> {} FAILED: {e}",
                    ev.plugin_id, ev.event_type, url
                )),
            }
        }
        if let Some(cmd) = &sub.command {
            self.run_command(&sub.plugin_id, cmd, &ev).await;
        }
    }

    /// Fork/exec a `webhook_command` as **argv (no shell)** — piping the event
    /// JSON on stdin and exposing `AISH_EVENT_TYPE` / `AISH_PLUGIN_ID` in the
    /// environment. The command string is tokenized by [`split_argv`] and run
    /// via `tokio::process::Command` directly; there is deliberately NO `sh -c`
    /// (SPR-069 TASK-379) so shell metacharacters in a manifest or payload are
    /// inert. The captured `{exit_code, stdout, stderr, event}` is persisted to
    /// plugin state under `<plugin_id>:last_webhook_output`.
    async fn run_command(&self, plugin_id: &str, cmd: &str, ev: &PluginEvent) {
        use tokio::io::AsyncWriteExt;
        let payload = serde_json::to_string(ev).unwrap_or_else(|_| "{}".into());
        // NO SHELL (SPR-069 TASK-379/446): the command is fork/exec'd as argv,
        // never `sh -c`. Tokens are passed verbatim, so `$(...)`, `;`, `|`,
        // backticks in a manifest or payload are literal argv bytes — never
        // interpreted. Payload data reaches the handler on stdin only.
        let argv = split_argv(cmd);
        let Some((program, args)) = argv.split_first() else {
            log_channel(&format!("{plugin_id} empty webhook_command; skipped"));
            return;
        };
        let mut child = match tokio::process::Command::new(program)
            .args(args)
            .env("AISH_EVENT_TYPE", &ev.event_type)
            .env("AISH_PLUGIN_ID", plugin_id)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
        {
            Ok(c) => c,
            Err(e) => {
                log_channel(&format!("{plugin_id} command spawn FAILED: {e}"));
                return;
            }
        };
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(payload.as_bytes()).await;
            // Drop closes stdin so the child sees EOF.
        }
        let out = match child.wait_with_output().await {
            Ok(o) => o,
            Err(e) => {
                log_channel(&format!("{plugin_id} command wait FAILED: {e}"));
                return;
            }
        };
        let record = json!({
            "exit_code": out.status.code(),
            "stdout": String::from_utf8_lossy(&out.stdout),
            "stderr": String::from_utf8_lossy(&out.stderr),
            "event": ev.event_type,
            "at": ev.timestamp,
        });
        if let Err(e) = self.state.set(plugin_id, "last_webhook_output", &record) {
            log_channel(&format!("{plugin_id} state write FAILED: {e}"));
        } else {
            log_channel(&format!(
                "{plugin_id} command {} -> exit {:?}",
                ev.event_type,
                out.status.code()
            ));
        }
        // Surface the plugin's reaction on the SecondStatusLine (≤60 chars,
        // most-recent-wins). This is the "plugin received a webhook → show it on
        // the 2nd statusline" path: the plugin decides the text, the engine caps
        // and routes it.
        if let Some(flash) = &self.flash {
            if let Some(msg) = nline_message(&String::from_utf8_lossy(&out.stdout)) {
                if let Ok(mut slot) = flash.lock() {
                    *slot = Some(msg);
                }
            }
        }
    }
}

/// Split a legacy `webhook_command` string into an argv vector for **no-shell**
/// fork/exec (SPR-069 TASK-379). Whitespace separates tokens; single and double
/// quotes group a token that contains spaces. Crucially, NO shell evaluation
/// happens: `$VAR`, `$(...)`, backticks, `;`, `|`, `&&`, and redirections are
/// copied verbatim into the token they appear in — inert argv bytes, never
/// interpreted. A trailing unbalanced quote simply closes at end-of-string.
pub fn split_argv(cmd: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_token = false;
    let mut quote: Option<char> = None;
    for c in cmd.chars() {
        match quote {
            Some(q) => {
                if c == q {
                    quote = None;
                } else {
                    cur.push(c);
                }
            }
            None => {
                if c == '\'' || c == '"' {
                    quote = Some(c);
                    in_token = true;
                } else if c.is_whitespace() {
                    if in_token {
                        out.push(std::mem::take(&mut cur));
                        in_token = false;
                    }
                } else {
                    cur.push(c);
                    in_token = true;
                }
            }
        }
    }
    if in_token {
        out.push(cur);
    }
    out
}

#[cfg(test)]
mod argv_tests {
    use super::split_argv;

    #[test]
    fn splits_on_whitespace() {
        assert_eq!(split_argv("sleep 0.3"), vec!["sleep", "0.3"]);
        assert_eq!(split_argv("  echo   hi  "), vec!["echo", "hi"]);
        assert!(split_argv("   ").is_empty());
    }

    #[test]
    fn quotes_group_a_token_with_spaces() {
        assert_eq!(split_argv(r#"printf "a b""#), vec!["printf", "a b"]);
        assert_eq!(split_argv("printf 'a b'"), vec!["printf", "a b"]);
        assert_eq!(split_argv(r#"a"b c"d"#), vec!["ab cd"]);
    }

    #[test]
    fn shell_metacharacters_are_literal_never_evaluated() {
        // The load-bearing security property (TASK-379/446): metacharacters are
        // ordinary token bytes, so nothing is ever shell-interpreted.
        assert_eq!(split_argv("echo $(whoami)"), vec!["echo", "$(whoami)"]);
        assert_eq!(
            split_argv("rm -rf / ; echo hi"),
            vec!["rm", "-rf", "/", ";", "echo", "hi"],
        );
        assert_eq!(split_argv("echo `id`"), vec!["echo", "`id`"]);
        assert_eq!(split_argv("cat a|b"), vec!["cat", "a|b"]);
    }
}

/// Distill a webhook command's stdout into the one-line SecondStatusLine
/// message: the first non-empty (trimmed) line, hard-capped at 60 display
/// characters (`…` elision when longer). `None` when the command printed
/// nothing surfaceable. Pure + char-aware (never splits a UTF-8 codepoint).
pub fn nline_message(stdout: &str) -> Option<String> {
    let line = stdout.lines().map(str::trim).find(|l| !l.is_empty())?;
    Some(cap_chars(line, 60))
}

/// Truncate `s` to at most `max` Unicode scalar values, appending `…` (counted
/// within the budget) when elided. Operates on `char`s so a multibyte glyph is
/// never cut mid-sequence.
fn cap_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let head: String = s.chars().take(max.saturating_sub(1)).collect();
    format!("{}…", head.trim_end())
}

/// Unix epoch seconds, saturating to 0 before the epoch (never panics).
fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Emit a line on the dedicated `plugin-events` observability channel. Quiet by
/// default; set `AISH_PLUGIN_EVENTS=1` to surface dispatch/delivery lines on
/// stderr. Kept as a single sink so the channel is easy to redirect later.
fn log_channel(msg: &str) {
    if std::env::var("AISH_PLUGIN_EVENTS")
        .map(|v| v != "0" && !v.is_empty())
        .unwrap_or(false)
    {
        eprintln!("\x1b[36m[plugin-events]\x1b[0m {msg}");
    }
}

/// Process-wide dispatcher, initialized once on shell startup.
static GLOBAL: OnceLock<PluginDispatcher> = OnceLock::new();

/// Initialize (once) and return the global dispatcher. Idempotent: the first
/// successful call wins; later calls return the existing dispatcher and ignore
/// the arguments.
pub fn init_global(
    plugins_dir: &Path,
    state: PluginStateStore,
    flash: Option<FlashSink>,
) -> &'static PluginDispatcher {
    if let Some(existing) = GLOBAL.get() {
        return existing;
    }
    let mut dispatcher = PluginDispatcher::new(plugins_dir.to_path_buf(), state);
    if let Some(f) = flash {
        dispatcher = dispatcher.with_flash(f);
    }
    let _ = GLOBAL.set(dispatcher);
    GLOBAL.get().expect("global set above")
}

/// The global dispatcher if [`init_global`] has run, else `None`. Hook sites call
/// this to route events without threading the dispatcher through every call.
pub fn dispatcher() -> Option<&'static PluginDispatcher> {
    GLOBAL.get()
}

#[cfg(test)]
mod flash_tests {
    use super::nline_message;

    #[test]
    fn first_nonempty_line_is_used() {
        // Leading blank/whitespace lines are skipped; the first real line wins,
        // and trailing lines are ignored (single-slot, one-line footer).
        assert_eq!(
            nline_message("\n  \n👋 hello-world: webhook received\nsecond\n").as_deref(),
            Some("👋 hello-world: webhook received"),
        );
    }

    #[test]
    fn empty_or_blank_stdout_is_none() {
        assert_eq!(nline_message(""), None);
        assert_eq!(nline_message("   \n\t\n"), None);
    }

    #[test]
    fn caps_at_60_display_chars_with_ellipsis() {
        let out = nline_message(&"x".repeat(200)).unwrap();
        assert!(
            out.chars().count() <= 60,
            "must cap to 60 chars, got {}",
            out.chars().count(),
        );
        assert!(out.ends_with('…'), "an over-long line must be elided: {out}");
    }

    #[test]
    fn never_splits_a_multibyte_codepoint() {
        // 100 four-byte emoji: the cap must land on a char boundary (String
        // construction would panic otherwise) and stay within the 60-char budget.
        let out = nline_message(&"🚀".repeat(100)).unwrap();
        assert!(out.chars().count() <= 60);
        assert!(out.ends_with('…'));
    }

    #[test]
    fn a_line_at_the_limit_is_kept_verbatim() {
        let sixty = "y".repeat(60);
        assert_eq!(nline_message(&sixty).as_deref(), Some(sixty.as_str()));
    }
}
