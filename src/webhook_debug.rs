//! TASK-273 — webhook testing & debugging tools.
//!
//! Backs the `:webhook logs|test|replay` subcommands and the persistent
//! delivery log wired into the broker dispatcher by [`crate::webhook`]:
//!
//! * every broker delivery is persisted per plugin to
//!   `~/.aish/state/webhooks/<plugin>.jsonl` (format: see
//!   [`aish_webhook_client::delivery`]), capped by `AISH_WEBHOOK_AUDIT_MAX`
//!   (default 1000 per plugin, `0` disables), payload secret-redacted;
//! * the plugin's `webhooks` memory namespace gets `last_delivery` metadata;
//! * `:webhook test <plugin> <event>` synthesises an envelope and dry-runs (or,
//!   with `--run`, executes) it against the local plugin registry;
//! * `:webhook replay [<plugin>] <id|last>` re-dispatches a stored record;
//! * `:webhook status` asks the broker's `/health` for its pending queue size.
//!
//! Test and replay runs are dispatched with no audit/delivery sink, so they
//! never land in the delivery log.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use aish_webhook_client::delivery::{
    DeliveryLog, DeliveryRecord, audit_max_from_env, plan_dispatch, sample_payload, synth_webhook,
};
use aish_webhook_client::{
    DeliverySink, HandlerOutcome, PluginRegistry, Webhook, WebhookDispatcher,
};

/// Default number of records shown by `:webhook logs`.
pub const DEFAULT_LOGS_LIMIT: usize = 20;
/// Max stdout/stderr lines echoed per handler by `--run`.
const OUTPUT_LINES: usize = 20;

/// Subcommands offered by TAB after `:webhook `.
pub const SUBCOMMANDS: &[&str] = &["status", "reload", "logs", "test", "replay"];

/// One-line usage for `:webhook`.
pub const USAGE: &str = "usage: :webhook [status | reload | logs [N] [--plugin id] [--event type] \
     | test <plugin> <event> [--payload file.json] [--run] | replay [plugin] <id|last> [--run]]";

/// `~/.aish/state/webhooks` — where per-plugin delivery logs live.
pub fn state_dir() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_default())
        .join(".aish")
        .join("state")
        .join("webhooks")
}

/// The delivery log at [`state_dir`] with env-configured retention.
pub fn delivery_log() -> DeliveryLog {
    DeliveryLog::new(state_dir(), audit_max_from_env())
}

/// Build the dispatcher [`DeliverySink`]: persist each per-plugin record to
/// `log` and write `last_delivery` metadata into the plugin's `webhooks`
/// memory namespace (only when `<plugins_dir>/<plugin-id>/` exists, so a
/// manifest id that differs from its directory never spawns a stray dir).
/// Failures are logged, never propagated — a broken log must not break
/// dispatch.
pub fn delivery_sink(
    log: DeliveryLog,
    plugins_dir: PathBuf,
    registry: Arc<PluginRegistry>,
) -> DeliverySink {
    Arc::new(move |webhook: &Webhook, outcomes: &[HandlerOutcome]| {
        let memory = crate::plugin_memory::PluginMemory::new(plugins_dir.clone());
        for rec in DeliveryRecord::group_by_plugin(webhook, outcomes, Some(&registry)) {
            if let Err(e) = log.append(&rec) {
                tracing::warn!(error = %e, plugin_id = %rec.plugin_id, "webhook delivery log write failed");
            }
            if plugins_dir.join(&rec.plugin_id).is_dir()
                && let Err(e) =
                    memory.set(&rec.plugin_id, "webhooks", "last_delivery", rec.metadata())
            {
                tracing::warn!(error = %e, plugin_id = %rec.plugin_id, "webhook last_delivery memory write failed");
            }
        }
    })
}

// ───────────────────────────── argument parsing ─────────────────────────────

/// Parsed `:webhook logs` arguments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogsArgs {
    pub limit: usize,
    pub plugin: Option<String>,
    pub event: Option<String>,
}

/// Parsed `:webhook test` arguments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TestArgs {
    pub plugin: String,
    pub event: String,
    pub payload: Option<PathBuf>,
    pub run: bool,
}

/// Parsed `:webhook replay` arguments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplayArgs {
    pub plugin: Option<String>,
    pub id: String,
    pub run: bool,
}

fn flag_value<'a>(args: &[&'a str], i: &mut usize, flag: &str) -> Result<&'a str, String> {
    *i += 1;
    args.get(*i)
        .copied()
        .ok_or_else(|| format!("{flag} needs a value"))
}

/// `[N] [<plugin>] [--plugin id] [--event type] [--limit N]`.
pub fn parse_logs_args(args: &[&str]) -> Result<LogsArgs, String> {
    let mut out = LogsArgs {
        limit: DEFAULT_LOGS_LIMIT,
        plugin: None,
        event: None,
    };
    let mut i = 0;
    while i < args.len() {
        match args[i] {
            "--plugin" | "-p" => out.plugin = Some(flag_value(args, &mut i, "--plugin")?.into()),
            "--event" | "-e" => out.event = Some(flag_value(args, &mut i, "--event")?.into()),
            "--limit" | "-n" => {
                let v = flag_value(args, &mut i, "--limit")?;
                out.limit = v
                    .parse()
                    .map_err(|_| format!("--limit expects a number, got `{v}`"))?;
            }
            a if a.starts_with('-') => return Err(format!("unknown flag `{a}`")),
            a => match a.parse::<usize>() {
                Ok(n) => out.limit = n,
                Err(_) if out.plugin.is_none() => out.plugin = Some(a.into()),
                Err(_) => return Err(format!("unexpected argument `{a}`")),
            },
        }
        i += 1;
    }
    Ok(out)
}

/// `<plugin> <event> [--payload file.json] [--run]`.
pub fn parse_test_args(args: &[&str]) -> Result<TestArgs, String> {
    let mut pos = Vec::new();
    let mut payload = None;
    let mut run = false;
    let mut i = 0;
    while i < args.len() {
        match args[i] {
            "--run" => run = true,
            "--dry-run" => run = false,
            "--payload" => payload = Some(PathBuf::from(flag_value(args, &mut i, "--payload")?)),
            a if a.starts_with('-') => return Err(format!("unknown flag `{a}`")),
            a => pos.push(a),
        }
        i += 1;
    }
    match pos.as_slice() {
        [plugin, event] => Ok(TestArgs {
            plugin: (*plugin).into(),
            event: (*event).into(),
            payload,
            run,
        }),
        _ => Err("usage: :webhook test <plugin> <event> [--payload file.json] [--run]".into()),
    }
}

/// `[<plugin>] <record-id|last> [--run]`.
pub fn parse_replay_args(args: &[&str]) -> Result<ReplayArgs, String> {
    let mut pos = Vec::new();
    let mut run = false;
    for a in args {
        match *a {
            "--run" => run = true,
            "--dry-run" => run = false,
            f if f.starts_with('-') => return Err(format!("unknown flag `{f}`")),
            p => pos.push(p),
        }
    }
    match pos.as_slice() {
        [id] => Ok(ReplayArgs {
            plugin: None,
            id: (*id).into(),
            run,
        }),
        [plugin, id] => Ok(ReplayArgs {
            plugin: Some((*plugin).into()),
            id: (*id).into(),
            run,
        }),
        _ => Err("usage: :webhook replay [plugin] <record-id|last> [--run]".into()),
    }
}

// ───────────────────────────── logs ─────────────────────────────

/// Apply plugin/event filters, then keep the newest `limit` (oldest → newest).
pub fn filter_records(recs: Vec<DeliveryRecord>, args: &LogsArgs) -> Vec<DeliveryRecord> {
    let mut v: Vec<DeliveryRecord> = recs
        .into_iter()
        .filter(|r| args.plugin.as_deref().is_none_or(|p| r.plugin_id == p))
        .filter(|r| args.event.as_deref().is_none_or(|e| r.event_type == e))
        .collect();
    if v.len() > args.limit {
        v.drain(..v.len() - args.limit);
    }
    v
}

/// Persisted records for `args` (reads one plugin's file when filtered).
pub fn query_logs(log: &DeliveryLog, args: &LogsArgs) -> Vec<DeliveryRecord> {
    let recs = match &args.plugin {
        Some(p) => log.read(p),
        None => log.read_all(),
    };
    filter_records(recs, args)
}

/// One line per record for `:webhook logs`.
pub fn fmt_delivery(r: &DeliveryRecord) -> String {
    let handlers: Vec<String> = r
        .handlers
        .iter()
        .map(|h| {
            let mut s = format!("{}:{}", h.name, h.status);
            if h.status != "skipped" {
                s.push_str(&format!(" {}ms", h.duration_ms));
            }
            if let Some(e) = &h.error {
                s.push_str(&format!(" ({e})"));
            }
            s
        })
        .collect();
    format!(
        "{at} {id} [{st}] plugin={p} {ev} — {h}",
        at = r.received_at,
        id = r.id,
        st = r.overall_status(),
        p = r.plugin_id,
        ev = r.event_type,
        h = if handlers.is_empty() {
            "no handlers".to_string()
        } else {
            handlers.join(", ")
        },
    )
}

// ───────────────────────────── test / replay ─────────────────────────────

/// Read + parse a JSON payload file for `:webhook test --payload`.
pub fn load_payload(path: &Path) -> Result<serde_json::Value, String> {
    let raw = std::fs::read_to_string(path)
        .map_err(|e| format!("cannot read payload file {}: {e}", path.display()))?;
    serde_json::from_str(&raw)
        .map_err(|e| format!("payload file {} is not valid JSON: {e}", path.display()))
}

/// Load the plugin registry and narrow it to `plugin`, erroring (with the list
/// of known plugin ids) when it isn't installed.
fn plugin_registry(plugins_dir: &Path, plugin: &str) -> Result<PluginRegistry, String> {
    let all = PluginRegistry::load_dir(plugins_dir)
        .map_err(|e| format!("cannot load plugins from {}: {e}", plugins_dir.display()))?;
    let sub = all.only(plugin);
    if sub.is_empty() {
        let known = all.plugin_ids();
        return Err(format!(
            "no webhook plugin `{plugin}` in {} (known: {})",
            plugins_dir.display(),
            if known.is_empty() {
                "none".to_string()
            } else {
                known.join(", ")
            }
        ));
    }
    // TASK-274: a config-invalid plugin's webhook handlers are skipped on the
    // live path, so `:webhook test|replay` refuse it the same way (dry-run too
    // — the plan would never run) and point at the fix.
    if let Some((_, err)) = crate::plugins::config_invalid_plugins(plugins_dir)
        .into_iter()
        .find(|(id, _)| id == plugin)
    {
        return Err(format!(
            "plugin `{plugin}`: config invalid — {err}; its webhook handlers are skipped \
             until fixed (:plugin errors {plugin}, then :plugin reload)"
        ));
    }
    Ok(sub)
}

/// Dry-run plan or executed outcomes for `webhook` against `registry`.
async fn dispatch_or_plan(registry: PluginRegistry, webhook: &Webhook, run: bool) -> String {
    let mut out = Vec::new();
    if !run {
        let plan = plan_dispatch(&registry, webhook);
        if plan.is_empty() {
            out.push(format!(
                "dry-run: no handler of `{}` subscribes to `{}`",
                webhook.plugin_id, webhook.event_type
            ));
        } else {
            out.push(format!(
                "dry-run: {} handler(s) match `{}` (use --run to execute):",
                plan.len(),
                webhook.event_type
            ));
            for p in plan {
                out.push(format!(
                    "  {} [{}] {} — filters {}",
                    p.plugin_id,
                    p.event_type,
                    p.command.join(" "),
                    if p.filters_pass {
                        "pass"
                    } else {
                        "FAIL (would skip)"
                    }
                ));
            }
        }
        return out.join("\n");
    }
    let dispatcher = WebhookDispatcher::new(Arc::new(registry));
    let outcomes = dispatcher.dispatch(webhook).await;
    if outcomes.is_empty() {
        return format!(
            "no handler of `{}` subscribes to `{}`",
            webhook.plugin_id, webhook.event_type
        );
    }
    for o in &outcomes {
        out.push(fmt_outcome(o));
    }
    out.join("\n")
}

/// Human-readable result of one executed (or filtered) handler.
pub fn fmt_outcome(o: &HandlerOutcome) -> String {
    let status = if o.executed && o.success {
        "ok"
    } else if !o.executed && o.error.is_none() {
        "skipped (filters)"
    } else {
        "error"
    };
    let exit = o
        .exit_code
        .map(|c| c.to_string())
        .unwrap_or_else(|| "-".into());
    let mut s = format!(
        "{} [{}] {status} exit={exit} {}ms",
        o.plugin_id, o.event_type, o.duration_ms
    );
    if let Some(e) = &o.error {
        s.push_str(&format!(" — {e}"));
    }
    for (label, text) in [("stdout", &o.stdout), ("stderr", &o.stderr)] {
        let t = text.trim();
        if t.is_empty() {
            continue;
        }
        s.push_str(&format!("\n  {label}:"));
        for line in t.lines().take(OUTPUT_LINES) {
            s.push_str(&format!("\n    {line}"));
        }
        if t.lines().count() > OUTPUT_LINES {
            s.push_str("\n    …");
        }
    }
    s
}

/// `:webhook test` — synthesise an envelope and dry-run/execute it locally.
pub async fn run_test(plugins_dir: &Path, args: &TestArgs) -> Result<String, String> {
    let registry = plugin_registry(plugins_dir, &args.plugin)?;
    let payload = match &args.payload {
        Some(p) => load_payload(p)?,
        None => sample_payload(&args.event),
    };
    let webhook = synth_webhook(&args.plugin, &args.event, payload);
    let head = format!("🧪 test {} `{}` → {}", webhook.id, args.event, args.plugin);
    Ok(format!(
        "{head}\n{}",
        dispatch_or_plan(registry, &webhook, args.run).await
    ))
}

/// `:webhook replay` — re-dispatch a stored record to its plugin locally.
pub async fn run_replay(
    log: &DeliveryLog,
    plugins_dir: &Path,
    args: &ReplayArgs,
) -> Result<String, String> {
    if !log.is_enabled() {
        return Err("delivery log disabled (AISH_WEBHOOK_AUDIT_MAX=0) — nothing to replay".into());
    }
    let rec = log
        .find(args.plugin.as_deref(), &args.id)
        .ok_or_else(|| match &args.plugin {
            Some(p) => format!("no delivery `{}` for plugin `{p}`", args.id),
            None => format!("no delivery `{}` in {}", args.id, log.dir().display()),
        })?;
    let registry = plugin_registry(plugins_dir, &rec.plugin_id)?;
    let webhook = rec.to_webhook();
    let head = format!(
        "↻ replay {} `{}` → {} (originally {})",
        rec.id, rec.event_type, rec.plugin_id, rec.received_at
    );
    Ok(format!(
        "{head}\n{}",
        dispatch_or_plan(registry, &webhook, args.run).await
    ))
}

// ───────────────────────────── broker queue ─────────────────────────────

/// Broker `/health` URL for a broker WebSocket URL (`wss://h/ws` →
/// `https://h/health`). `None` for non-ws(s)/http(s) schemes.
pub fn health_url(broker_url: &str) -> Option<String> {
    let (scheme, rest) = broker_url.trim().split_once("://")?;
    let scheme = match scheme.to_ascii_lowercase().as_str() {
        "wss" | "https" => "https",
        "ws" | "http" => "http",
        _ => return None,
    };
    let host = rest
        .split(['/', '?', '#'])
        .next()
        .filter(|h| !h.is_empty())?;
    Some(format!("{scheme}://{host}/health"))
}

/// Best-effort broker pending-queue size (`queued_messages` from `/health`),
/// 2 s timeout. `None` when unreachable or the field is missing.
pub async fn broker_queue(broker_url: &str) -> Option<u64> {
    let url = health_url(broker_url)?;
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
        .ok()?;
    let v: serde_json::Value = client.get(url).send().await.ok()?.json().await.ok()?;
    v.get("queued_messages")?.as_u64()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tmpdir(tag: &str) -> PathBuf {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let d = std::env::temp_dir().join(format!("aish-whdebug-{tag}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// A plugins dir holding plugin `gh` with a `pull_request` handler (`true`)
    /// and one filtered on `action=closed`.
    fn plugins_fixture() -> PathBuf {
        let root = tmpdir("plugins");
        let dir = root.join("gh");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("plugin.json"),
            json!({
                "id": "gh",
                "webhooks": [
                    {"event_type": "pull_request", "command": ["true"]},
                    {"event_type": "pull_request", "command": ["false"], "filters": {"action": "closed"}}
                ]
            })
            .to_string(),
        )
        .unwrap();
        root
    }

    fn rec(plugin: &str, id: &str, event: &str, at: u128) -> DeliveryRecord {
        DeliveryRecord {
            id: id.into(),
            webhook_id: id.into(),
            tenant_id: "t".into(),
            plugin_id: plugin.into(),
            event_type: event.into(),
            received_at_ms: at,
            received_at: aish_webhook_client::delivery::rfc3339_utc(at),
            payload: json!({"action": "opened"}),
            handlers: vec![],
        }
    }

    #[test]
    fn logs_args_parse() {
        assert_eq!(
            parse_logs_args(&[]).unwrap(),
            LogsArgs {
                limit: 20,
                plugin: None,
                event: None
            }
        );
        let a = parse_logs_args(&["5", "--plugin", "gh", "--event", "push"]).unwrap();
        assert_eq!(a.limit, 5);
        assert_eq!(a.plugin.as_deref(), Some("gh"));
        assert_eq!(a.event.as_deref(), Some("push"));
        // Card form: `:webhook logs github --limit 20`.
        let b = parse_logs_args(&["github", "--limit", "7"]).unwrap();
        assert_eq!((b.plugin.as_deref(), b.limit), (Some("github"), 7));
        assert!(parse_logs_args(&["--limit", "x"]).is_err());
        assert!(parse_logs_args(&["--plugin"]).is_err());
        assert!(parse_logs_args(&["--bogus"]).is_err());
        assert!(parse_logs_args(&["a", "b"]).is_err());
    }

    #[test]
    fn test_args_parse() {
        let a = parse_test_args(&["gh", "pull_request"]).unwrap();
        assert_eq!(a.plugin, "gh");
        assert_eq!(a.event, "pull_request");
        assert!(!a.run);
        assert!(a.payload.is_none());
        let b = parse_test_args(&["gh", "push", "--payload", "/tmp/p.json", "--run"]).unwrap();
        assert!(b.run);
        assert_eq!(b.payload, Some(PathBuf::from("/tmp/p.json")));
        assert!(parse_test_args(&["gh"]).is_err());
        assert!(parse_test_args(&["gh", "push", "--payload"]).is_err());
        assert!(parse_test_args(&["gh", "push", "--x"]).is_err());
    }

    #[test]
    fn replay_args_parse() {
        let a = parse_replay_args(&["last"]).unwrap();
        assert_eq!((a.plugin, a.id.as_str(), a.run), (None, "last", false));
        let b = parse_replay_args(&["github", "webhook_abc123", "--run"]).unwrap();
        assert_eq!(b.plugin.as_deref(), Some("github"));
        assert_eq!(b.id, "webhook_abc123");
        assert!(b.run);
        assert!(parse_replay_args(&[]).is_err());
        assert!(parse_replay_args(&["a", "b", "c"]).is_err());
    }

    #[test]
    fn filter_records_by_plugin_event_and_limit() {
        let recs = vec![
            rec("gh", "1", "push", 1),
            rec("gh", "2", "pull_request", 2),
            rec("x", "3", "pull_request", 3),
            rec("gh", "4", "pull_request", 4),
        ];
        let by_event = filter_records(
            recs.clone(),
            &parse_logs_args(&["--event", "pull_request"]).unwrap(),
        );
        assert_eq!(
            by_event.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
            ["2", "3", "4"]
        );
        let both = filter_records(
            recs.clone(),
            &parse_logs_args(&["--plugin", "gh", "--event", "pull_request", "1"]).unwrap(),
        );
        assert_eq!(both.len(), 1);
        assert_eq!(both[0].id, "4", "limit keeps the newest");
        let line = fmt_delivery(&recs[0]);
        assert!(line.contains(" 1 ") && line.contains("plugin=gh") && line.contains("push"));
    }

    #[test]
    fn query_logs_reads_persisted_log() {
        let dir = tmpdir("q");
        let log = DeliveryLog::new(&dir, 100);
        log.append(&rec("gh", "a", "push", 1)).unwrap();
        log.append(&rec("other", "b", "push", 2)).unwrap();
        let all = query_logs(&log, &parse_logs_args(&[]).unwrap());
        assert_eq!(all.len(), 2);
        let gh = query_logs(&log, &parse_logs_args(&["gh"]).unwrap());
        assert_eq!(gh.len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_payload_validates_json() {
        let dir = tmpdir("payload");
        let good = dir.join("good.json");
        std::fs::write(&good, r#"{"action":"closed"}"#).unwrap();
        assert_eq!(load_payload(&good).unwrap()["action"], json!("closed"));
        let bad = dir.join("bad.json");
        std::fs::write(&bad, "{nope").unwrap();
        assert!(load_payload(&bad).unwrap_err().contains("not valid JSON"));
        assert!(
            load_payload(&dir.join("missing.json"))
                .unwrap_err()
                .contains("cannot read")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn health_url_mapping() {
        assert_eq!(
            health_url("wss://broker.example.com/ws").as_deref(),
            Some("https://broker.example.com/health")
        );
        assert_eq!(
            health_url("ws://127.0.0.1:8080/ws?x=1").as_deref(),
            Some("http://127.0.0.1:8080/health")
        );
        assert_eq!(health_url("ftp://x/ws"), None);
        assert_eq!(health_url("garbage"), None);
        assert_eq!(health_url("wss:///ws"), None);
    }

    #[tokio::test]
    async fn run_test_dry_run_and_run() {
        let plugins = plugins_fixture();
        let args = parse_test_args(&["gh", "pull_request"]).unwrap();
        let dry = run_test(&plugins, &args).await.unwrap();
        assert!(dry.contains("dry-run: 2 handler(s)"), "{dry}");
        assert!(dry.contains("filters pass"));
        assert!(dry.contains("FAIL (would skip)"));

        let args = parse_test_args(&["gh", "pull_request", "--run"]).unwrap();
        let ran = run_test(&plugins, &args).await.unwrap();
        assert!(ran.contains("ok exit=0"), "{ran}");
        assert!(ran.contains("skipped (filters)"), "{ran}");

        let none = run_test(&plugins, &parse_test_args(&["gh", "push"]).unwrap())
            .await
            .unwrap();
        assert!(none.contains("no handler"), "{none}");

        let err = run_test(&plugins, &parse_test_args(&["nope", "push"]).unwrap())
            .await
            .unwrap_err();
        assert!(err.contains("known: gh"), "{err}");

        // A custom payload flips the filtered handler to run (and fail).
        let p = plugins.join("closed.json");
        std::fs::write(&p, r#"{"action":"closed"}"#).unwrap();
        let args = TestArgs {
            plugin: "gh".into(),
            event: "pull_request".into(),
            payload: Some(p),
            run: true,
        };
        let ran = run_test(&plugins, &args).await.unwrap();
        assert!(ran.contains("error exit=1"), "{ran}");
        let _ = std::fs::remove_dir_all(&plugins);
    }

    /// TASK-274: `:webhook test` refuses a config-invalid plugin (its handlers
    /// are skipped on the live path) and works again once the config is fixed.
    #[tokio::test]
    async fn run_test_refuses_config_invalid_plugin() {
        let plugins = plugins_fixture();
        let manifest = plugins.join("gh").join("plugin.json");
        let mut m: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&manifest).unwrap()).unwrap();
        m["config_schema"] = json!({
            "type": "object",
            "properties": {"token": {"type": "string"}},
            "required": ["token"]
        });
        std::fs::write(&manifest, m.to_string()).unwrap();
        let args = parse_test_args(&["gh", "pull_request", "--run"]).unwrap();
        let err = run_test(&plugins, &args).await.unwrap_err();
        assert!(
            err.contains("config invalid") && err.contains("token"),
            "{err}"
        );
        std::fs::write(plugins.join("gh").join("config.json"), r#"{"token":"t"}"#).unwrap();
        let ran = run_test(&plugins, &args).await.unwrap();
        assert!(ran.contains("ok exit=0"), "{ran}");
        let _ = std::fs::remove_dir_all(&plugins);
    }

    #[tokio::test]
    async fn run_replay_finds_and_redispatches() {
        let plugins = plugins_fixture();
        let dir = tmpdir("replay");
        let log = DeliveryLog::new(&dir, 10);
        log.append(&rec("gh", "wh_1", "pull_request", 1)).unwrap();
        let dry = run_replay(&log, &plugins, &parse_replay_args(&["last"]).unwrap())
            .await
            .unwrap();
        assert!(dry.contains("replay wh_1"), "{dry}");
        assert!(dry.contains("dry-run"));
        let ran = run_replay(
            &log,
            &plugins,
            &parse_replay_args(&["gh", "wh_1", "--run"]).unwrap(),
        )
        .await
        .unwrap();
        assert!(ran.contains("ok exit=0"), "{ran}");
        assert!(
            run_replay(&log, &plugins, &parse_replay_args(&["missing"]).unwrap())
                .await
                .is_err()
        );
        // Replays are not persisted.
        assert_eq!(log.read("gh").len(), 1);
        let off = DeliveryLog::new(&dir, 0);
        assert!(
            run_replay(&off, &plugins, &parse_replay_args(&["last"]).unwrap())
                .await
                .unwrap_err()
                .contains("disabled")
        );
        let _ = std::fs::remove_dir_all(&plugins);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn delivery_sink_persists_and_writes_memory() {
        let plugins = plugins_fixture();
        let dir = tmpdir("sink");
        let log = DeliveryLog::new(&dir, 10);
        let registry = Arc::new(PluginRegistry::load_dir(&plugins).unwrap());
        let sink = delivery_sink(log.clone(), plugins.clone(), registry.clone());
        let dispatcher = WebhookDispatcher::new(registry).with_delivery_sink(sink);
        let wh = Webhook {
            id: "wh_live".into(),
            tenant_id: "acme".into(),
            plugin_id: String::new(),
            event_type: "pull_request".into(),
            payload: json!({"action": "opened", "secret": "s3"}),
        };
        dispatcher.dispatch(&wh).await;

        let recs = log.read("gh");
        assert_eq!(recs.len(), 1);
        assert_eq!(recs[0].id, "wh_live");
        assert_eq!(recs[0].payload["secret"], json!("***"));
        assert_eq!(recs[0].handlers.len(), 2);

        let mem = crate::plugin_memory::PluginMemory::new(plugins.clone());
        let meta = mem.get("gh", "webhooks", "last_delivery").unwrap();
        assert_eq!(meta["webhook_id"], json!("wh_live"));
        assert_eq!(meta["event_type"], json!("pull_request"));
        assert_eq!(meta["handlers"][0]["status"], json!("ok"));
        assert!(meta.get("payload").is_none());
        let _ = std::fs::remove_dir_all(&plugins);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
