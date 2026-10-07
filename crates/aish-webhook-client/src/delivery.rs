//! TASK-273 — durable webhook delivery log + local test/replay helpers.
//!
//! Where [`crate::audit`] records one row per *handler outcome*, this module
//! records one [`DeliveryRecord`] per *(delivery, plugin)*: the webhook's
//! identity, the (secret-redacted) payload, and a summary of every handler of
//! that plugin that the delivery reached. Records are appended as JSON lines to
//! `<dir>/<plugin_id>.jsonl` by [`DeliveryLog`], capped per plugin so the log
//! can't grow without bound.
//!
//! The payload is kept so `:webhook replay` can re-dispatch a past delivery;
//! [`redact_secrets`] blanks secret-looking keys before anything hits disk.
//!
//! ## On-disk format (one JSON object per line)
//!
//! ```json
//! {"id":"wh_1","webhook_id":"wh_1","tenant_id":"acme","plugin_id":"github",
//!  "event_type":"pull_request","received_at_ms":1782000000000,
//!  "received_at":"2026-06-21T00:00:00Z","payload":{"action":"opened"},
//!  "handlers":[{"name":"on_pr.sh","status":"ok","exit_code":0,"duration_ms":12,"error":null}]}
//! ```
//!
//! Readers must tolerate (skip) malformed lines and unknown fields.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::audit::sanitize_plugin_id;
use crate::dispatcher::{passes_filters, HandlerOutcome, PluginRegistry};
use crate::envelope::Webhook;
use crate::error::Result;

/// Default per-plugin retention (records) for [`DeliveryLog`].
pub const DEFAULT_DELIVERY_MAX: usize = 1000;
/// Env var overriding the per-plugin retention. `0` disables persistence.
pub const AUDIT_MAX_ENV: &str = "AISH_WEBHOOK_AUDIT_MAX";

/// Replacement value for redacted secrets.
pub const REDACTED: &str = "***";

/// Key fragments (lowercase) that mark an object key as secret.
const SECRET_KEY_FRAGMENTS: &[&str] = &[
    "secret",
    "token",
    "password",
    "passwd",
    "authorization",
    "api_key",
    "apikey",
    "api-key",
    "signature",
    "cookie",
    "credential",
    "private_key",
    "private-key",
];

/// Summary of one handler's run for a delivery.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HandlerSummary {
    /// Handler name: file name of the handler program (`command[0]`), or the
    /// subscribed event type when unknown.
    pub name: String,
    /// `ok` | `error` | `skipped` (filtered out).
    pub status: String,
    #[serde(default)]
    pub exit_code: Option<i32>,
    #[serde(default)]
    pub duration_ms: u128,
    #[serde(default)]
    pub error: Option<String>,
}

impl HandlerSummary {
    /// Summarise an outcome. `name` is supplied by the caller (outcomes don't
    /// carry the handler command).
    pub fn from_outcome(name: impl Into<String>, o: &HandlerOutcome) -> Self {
        let status = if o.executed && o.success {
            "ok"
        } else if !o.executed && o.error.is_none() {
            "skipped"
        } else {
            "error"
        };
        Self {
            name: name.into(),
            status: status.to_string(),
            exit_code: o.exit_code,
            duration_ms: o.duration_ms,
            error: o.error.clone(),
        }
    }
}

/// One persisted delivery of a webhook to one plugin.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DeliveryRecord {
    /// Record id used by `:webhook replay` — the delivery (webhook) id.
    pub id: String,
    pub webhook_id: String,
    #[serde(default)]
    pub tenant_id: String,
    pub plugin_id: String,
    pub event_type: String,
    pub received_at_ms: u128,
    /// `received_at_ms` as RFC 3339 UTC (`2026-07-03T12:00:00Z`).
    pub received_at: String,
    /// Secret-redacted provider payload.
    #[serde(default)]
    pub payload: Value,
    #[serde(default)]
    pub handlers: Vec<HandlerSummary>,
}

impl DeliveryRecord {
    /// Split one dispatch into per-plugin records (plugin order = first
    /// appearance in `outcomes`). Handler names are resolved from `registry`
    /// when given (n-th outcome of a plugin ↔ n-th matching handler).
    pub fn group_by_plugin(
        webhook: &Webhook,
        outcomes: &[HandlerOutcome],
        registry: Option<&PluginRegistry>,
    ) -> Vec<DeliveryRecord> {
        let now = now_ms();
        let matching = registry
            .map(|r| r.matching(&webhook.event_type))
            .unwrap_or_default();
        let payload = redact_secrets(&webhook.payload);
        let mut out: Vec<DeliveryRecord> = Vec::new();
        for o in outcomes {
            let idx = match out.iter().position(|r| r.plugin_id == o.plugin_id) {
                Some(i) => i,
                None => {
                    out.push(DeliveryRecord {
                        id: webhook.id.clone(),
                        webhook_id: webhook.id.clone(),
                        tenant_id: webhook.tenant_id.clone(),
                        plugin_id: o.plugin_id.clone(),
                        event_type: webhook.event_type.clone(),
                        received_at_ms: now,
                        received_at: rfc3339_utc(now),
                        payload: payload.clone(),
                        handlers: Vec::new(),
                    });
                    out.len() - 1
                }
            };
            let nth = out[idx].handlers.len();
            let name = matching
                .iter()
                .filter(|(pid, _)| *pid == o.plugin_id)
                .nth(nth)
                .and_then(|(_, h)| h.command.first())
                .map(|c| handler_name(c))
                .unwrap_or_else(|| o.event_type.clone());
            out[idx]
                .handlers
                .push(HandlerSummary::from_outcome(name, o));
        }
        out
    }

    /// Delivery metadata for the plugin-memory `webhooks` namespace (no
    /// payload): `{webhook_id, event_type, received_at, handlers:[…]}`.
    pub fn metadata(&self) -> Value {
        json!({
            "webhook_id": self.webhook_id,
            "event_type": self.event_type,
            "received_at": self.received_at,
            "handlers": self.handlers,
        })
    }

    /// Rebuild a webhook envelope from this record (for replay). The payload
    /// is the redacted copy.
    pub fn to_webhook(&self) -> Webhook {
        Webhook {
            id: self.webhook_id.clone(),
            tenant_id: self.tenant_id.clone(),
            plugin_id: self.plugin_id.clone(),
            event_type: self.event_type.clone(),
            payload: self.payload.clone(),
        }
    }

    /// `ok`, `error`, or `skipped` rolled up over the handlers (error wins).
    pub fn overall_status(&self) -> &'static str {
        if self.handlers.iter().any(|h| h.status == "error") {
            "error"
        } else if self.handlers.iter().any(|h| h.status == "ok") {
            "ok"
        } else {
            "skipped"
        }
    }
}

/// File name of a handler program path (`/p/handlers/on_pr.sh` → `on_pr.sh`).
fn handler_name(cmd: &str) -> String {
    Path::new(cmd)
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| cmd.to_string())
}

fn is_secret_key(k: &str) -> bool {
    let k = k.to_ascii_lowercase();
    SECRET_KEY_FRAGMENTS.iter().any(|f| k.contains(f))
}

/// Deep-copy `v`, replacing the value of every object key that looks secret
/// (case-insensitive substring match on [`SECRET_KEY_FRAGMENTS`]) with
/// [`REDACTED`]. Arrays and nested objects are walked.
pub fn redact_secrets(v: &Value) -> Value {
    match v {
        Value::Object(m) => Value::Object(
            m.iter()
                .map(|(k, val)| {
                    let nv = if is_secret_key(k) {
                        Value::String(REDACTED.to_string())
                    } else {
                        redact_secrets(val)
                    };
                    (k.clone(), nv)
                })
                .collect(),
        ),
        Value::Array(a) => Value::Array(a.iter().map(redact_secrets).collect()),
        other => other.clone(),
    }
}

/// Current wall-clock time, unix epoch millis.
pub fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

/// Format unix-epoch millis as RFC 3339 UTC with second precision.
pub fn rfc3339_utc(ms: u128) -> String {
    let secs = (ms / 1000) as i64;
    let days = secs.div_euclid(86_400);
    let sod = secs.rem_euclid(86_400);
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let mut y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    if m <= 2 {
        y += 1;
    }
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        sod / 3600,
        (sod % 3600) / 60,
        sod % 60
    )
}

/// Parse a retention value as [`AUDIT_MAX_ENV`] would hold it. `None`/invalid
/// → [`DEFAULT_DELIVERY_MAX`].
pub fn parse_audit_max(raw: Option<&str>) -> usize {
    raw.and_then(|s| s.trim().parse::<usize>().ok())
        .unwrap_or(DEFAULT_DELIVERY_MAX)
}

/// Per-plugin retention from [`AUDIT_MAX_ENV`] (default 1000, `0` disables).
pub fn audit_max_from_env() -> usize {
    parse_audit_max(std::env::var(AUDIT_MAX_ENV).ok().as_deref())
}

/// Append-only, per-plugin, capped JSONL delivery log.
///
/// Each plugin's file keeps at most `max` readable records. To avoid rewriting
/// the file on every append, compaction (rewrite keeping the newest `max`
/// lines, via temp file + rename) runs once the file exceeds `max + max/10`
/// lines; readers always return only the newest `max`.
#[derive(Debug, Clone)]
pub struct DeliveryLog {
    dir: PathBuf,
    max: usize,
}

impl DeliveryLog {
    pub fn new(dir: impl Into<PathBuf>, max: usize) -> Self {
        Self {
            dir: dir.into(),
            max,
        }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn max(&self) -> usize {
        self.max
    }

    /// False when retention is `0` (persistence disabled).
    pub fn is_enabled(&self) -> bool {
        self.max > 0
    }

    /// `<dir>/<sanitized plugin id>.jsonl`.
    pub fn path_for(&self, plugin_id: &str) -> PathBuf {
        self.dir
            .join(format!("{}.jsonl", sanitize_plugin_id(plugin_id)))
    }

    /// Append one record (no-op when disabled), compacting when over the slack.
    pub fn append(&self, rec: &DeliveryRecord) -> Result<()> {
        if !self.is_enabled() {
            return Ok(());
        }
        std::fs::create_dir_all(&self.dir)?;
        let path = self.path_for(&rec.plugin_id);
        let mut line = serde_json::to_string(rec)?;
        line.push('\n');
        {
            let mut f = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)?;
            f.write_all(line.as_bytes())?;
            f.flush()?;
        }
        let slack = (self.max / 10).max(1);
        let body = std::fs::read_to_string(&path)?;
        let lines: Vec<&str> = body.lines().filter(|l| !l.trim().is_empty()).collect();
        if lines.len() > self.max + slack {
            let keep = &lines[lines.len() - self.max..];
            let tmp = path.with_extension("jsonl.tmp");
            let mut out = keep.join("\n");
            out.push('\n');
            std::fs::write(&tmp, out)?;
            std::fs::rename(&tmp, &path)?;
        }
        Ok(())
    }

    /// The newest `max` records for `plugin_id` (oldest → newest). Malformed
    /// lines are skipped; a missing file is empty.
    pub fn read(&self, plugin_id: &str) -> Vec<DeliveryRecord> {
        self.read_path(&self.path_for(plugin_id))
    }

    fn read_path(&self, path: &Path) -> Vec<DeliveryRecord> {
        let Ok(body) = std::fs::read_to_string(path) else {
            return Vec::new();
        };
        let mut recs: Vec<DeliveryRecord> = body
            .lines()
            .filter_map(|l| serde_json::from_str::<DeliveryRecord>(l).ok())
            .collect();
        let cap = self.max.max(1);
        if recs.len() > cap {
            recs.drain(..recs.len() - cap);
        }
        recs
    }

    /// Every plugin's records merged, sorted by `received_at_ms` (stable).
    pub fn read_all(&self) -> Vec<DeliveryRecord> {
        let mut all = Vec::new();
        let Ok(rd) = std::fs::read_dir(&self.dir) else {
            return all;
        };
        let mut paths: Vec<PathBuf> = rd
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().is_some_and(|x| x == "jsonl"))
            .collect();
        paths.sort();
        for p in paths {
            all.extend(self.read_path(&p));
        }
        all.sort_by_key(|r| r.received_at_ms);
        all
    }

    /// Find a record by id (or `last` = newest), optionally within one plugin.
    /// With several matches the newest wins.
    pub fn find(&self, plugin_id: Option<&str>, id: &str) -> Option<DeliveryRecord> {
        let recs = match plugin_id {
            Some(p) => self.read(p),
            None => self.read_all(),
        };
        if id == "last" {
            return recs.into_iter().last();
        }
        recs.into_iter().rev().find(|r| r.id == id)
    }
}

/// One handler a webhook would reach (dry-run plan).
#[derive(Debug, Clone, PartialEq)]
pub struct PlannedHandler {
    pub plugin_id: String,
    /// The handler's subscribed event type (`*` for wildcard).
    pub event_type: String,
    pub command: Vec<String>,
    /// The handler's payload filters pass for this webhook.
    pub filters_pass: bool,
}

/// Which handlers in `registry` `webhook` would reach, without executing any.
pub fn plan_dispatch(registry: &PluginRegistry, webhook: &Webhook) -> Vec<PlannedHandler> {
    registry
        .matching(&webhook.event_type)
        .into_iter()
        .map(|(pid, h)| PlannedHandler {
            plugin_id: pid.to_string(),
            event_type: h.event_type.clone(),
            command: h.command.clone(),
            filters_pass: passes_filters(&webhook.payload, &h.filters),
        })
        .collect()
}

/// A small, realistic sample payload for `:webhook test` (GitHub-shaped for
/// the common events, `{}` otherwise). Every sample carries `"aish_test": true`.
pub fn sample_payload(event_type: &str) -> Value {
    let mut v = match event_type {
        "pull_request" => json!({
            "action": "opened",
            "number": 1,
            "pull_request": {
                "number": 1,
                "title": "aish test pull request",
                "state": "open",
                "base": {"ref": "main"},
                "head": {"ref": "feature/aish-test"},
                "user": {"login": "aish"}
            },
            "repository": {"full_name": "example/repo"},
            "sender": {"login": "aish"}
        }),
        "push" => json!({
            "ref": "refs/heads/main",
            "before": "0000000000000000000000000000000000000000",
            "after": "1111111111111111111111111111111111111111",
            "commits": [],
            "repository": {"full_name": "example/repo"},
            "sender": {"login": "aish"}
        }),
        "issues" => json!({
            "action": "opened",
            "issue": {"number": 1, "title": "aish test issue", "state": "open"},
            "repository": {"full_name": "example/repo"},
            "sender": {"login": "aish"}
        }),
        "ping" => json!({"zen": "Keep it logically awesome.", "hook_id": 1}),
        _ => json!({}),
    };
    if let Value::Object(m) = &mut v {
        m.insert("aish_test".into(), Value::Bool(true));
    }
    v
}

/// Synthesise a local test envelope (`id = test_<millis>`, tenant `local`).
pub fn synth_webhook(plugin_id: &str, event_type: &str, payload: Value) -> Webhook {
    Webhook {
        id: format!("test_{}", now_ms()),
        tenant_id: "local".into(),
        plugin_id: plugin_id.into(),
        event_type: event_type.into(),
        payload,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dispatcher::{PluginManifest, WebhookDispatcher, WebhookHandler};
    use std::sync::{Arc, Mutex};

    fn tmpdir(tag: &str) -> PathBuf {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let d =
            std::env::temp_dir().join(format!("aish-delivery-{tag}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    fn outcome(plugin: &str, executed: bool, success: bool) -> HandlerOutcome {
        HandlerOutcome {
            plugin_id: plugin.into(),
            event_type: "pull_request".into(),
            matched: true,
            executed,
            exit_code: executed.then_some(if success { 0 } else { 1 }),
            success,
            stdout: String::new(),
            stderr: String::new(),
            error: None,
            duration_ms: 5,
        }
    }

    fn wh(id: &str) -> Webhook {
        Webhook {
            id: id.into(),
            tenant_id: "acme".into(),
            plugin_id: String::new(),
            event_type: "pull_request".into(),
            payload: json!({"action":"opened","token":"abc"}),
        }
    }

    fn rec(plugin: &str, id: &str, at: u128) -> DeliveryRecord {
        DeliveryRecord {
            id: id.into(),
            webhook_id: id.into(),
            tenant_id: "t".into(),
            plugin_id: plugin.into(),
            event_type: "pull_request".into(),
            received_at_ms: at,
            received_at: rfc3339_utc(at),
            payload: json!({"n": id}),
            handlers: vec![],
        }
    }

    fn handler(event: &str, cmd: &str, filters: Value) -> WebhookHandler {
        WebhookHandler {
            event_type: event.into(),
            command: vec![cmd.into()],
            filters: filters.as_object().cloned().unwrap_or_default(),
            timeout_secs: None,
        }
    }

    fn registry() -> PluginRegistry {
        PluginRegistry::from_plugins(vec![
            PluginManifest {
                id: "gh".into(),
                name: String::new(),
                version: String::new(),
                enabled: None,
                webhooks: vec![
                    handler("pull_request", "/p/gh/on_pr.sh", json!({})),
                    handler("pull_request", "true", json!({"action": "closed"})),
                ],
            },
            PluginManifest {
                id: "other".into(),
                name: String::new(),
                version: String::new(),
                enabled: None,
                webhooks: vec![handler("*", "true", json!({}))],
            },
        ])
    }

    #[test]
    fn redact_is_deep_and_case_insensitive() {
        let v = json!({
            "Authorization": "Bearer x",
            "nested": {"api_key": "k", "keep": 1, "list": [{"X-Hub-Signature-256": "s"}]},
            "client_secret": "c",
            "plain": "visible"
        });
        let r = redact_secrets(&v);
        assert_eq!(r["Authorization"], json!(REDACTED));
        assert_eq!(r["nested"]["api_key"], json!(REDACTED));
        assert_eq!(r["nested"]["keep"], json!(1));
        assert_eq!(
            r["nested"]["list"][0]["X-Hub-Signature-256"],
            json!(REDACTED)
        );
        assert_eq!(r["client_secret"], json!(REDACTED));
        assert_eq!(r["plain"], json!("visible"));
    }

    #[test]
    fn rfc3339_known_epochs() {
        assert_eq!(rfc3339_utc(0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339_utc(951_782_400_000), "2000-02-29T00:00:00Z");
        assert_eq!(rfc3339_utc(1_783_080_000_123), "2026-07-03T12:00:00Z");
    }

    #[test]
    fn parse_audit_max_defaults_and_zero() {
        assert_eq!(parse_audit_max(None), DEFAULT_DELIVERY_MAX);
        assert_eq!(parse_audit_max(Some("junk")), DEFAULT_DELIVERY_MAX);
        assert_eq!(parse_audit_max(Some(" 25 ")), 25);
        assert_eq!(parse_audit_max(Some("0")), 0);
    }

    #[test]
    fn group_by_plugin_splits_and_names_handlers() {
        let reg = registry();
        let outs = vec![
            outcome("gh", true, true),
            outcome("gh", false, false),
            outcome("other", true, false),
        ];
        let recs = DeliveryRecord::group_by_plugin(&wh("w1"), &outs, Some(&reg));
        assert_eq!(recs.len(), 2);
        assert_eq!(recs[0].plugin_id, "gh");
        assert_eq!(recs[0].handlers.len(), 2);
        assert_eq!(recs[0].handlers[0].name, "on_pr.sh");
        assert_eq!(recs[0].handlers[0].status, "ok");
        assert_eq!(recs[0].handlers[1].name, "true");
        assert_eq!(recs[0].handlers[1].status, "skipped");
        assert_eq!(recs[1].handlers[0].status, "error");
        assert_eq!(recs[1].overall_status(), "error");
        // Payload persisted redacted.
        assert_eq!(recs[0].payload["token"], json!(REDACTED));
        assert_eq!(recs[0].payload["action"], json!("opened"));
        let meta = recs[0].metadata();
        assert_eq!(meta["webhook_id"], json!("w1"));
        assert!(meta.get("payload").is_none());
        assert_eq!(meta["handlers"][0]["duration_ms"], json!(5));
        // Round-trips back to an envelope for replay.
        let back = recs[0].to_webhook();
        assert_eq!(back.id, "w1");
        assert_eq!(back.event_type, "pull_request");
    }

    #[test]
    fn log_roundtrip_find_and_last() {
        let dir = tmpdir("rt");
        let log = DeliveryLog::new(&dir, 10);
        log.append(&rec("gh", "a", 1)).unwrap();
        log.append(&rec("gh", "b", 3)).unwrap();
        log.append(&rec("other", "c", 2)).unwrap();
        assert_eq!(log.read("gh").len(), 2);
        let all = log.read_all();
        let ids: Vec<&str> = all.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(ids, vec!["a", "c", "b"]);
        assert_eq!(log.find(None, "last").unwrap().id, "b");
        assert_eq!(log.find(Some("other"), "last").unwrap().id, "c");
        assert_eq!(log.find(None, "c").unwrap().plugin_id, "other");
        assert!(log.find(Some("gh"), "c").is_none());
        assert!(log.find(None, "zzz").is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn log_caps_and_compacts_keeping_newest() {
        let dir = tmpdir("cap");
        let log = DeliveryLog::new(&dir, 5);
        for i in 0..20u128 {
            log.append(&rec("gh", &format!("r{i}"), i)).unwrap();
        }
        let recs = log.read("gh");
        assert_eq!(recs.len(), 5);
        assert_eq!(recs[0].id, "r15");
        assert_eq!(recs[4].id, "r19");
        // Disk never exceeds max + slack lines.
        let lines = std::fs::read_to_string(log.path_for("gh"))
            .unwrap()
            .lines()
            .count();
        assert!(lines <= 6, "{lines} lines on disk");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn log_disabled_writes_nothing() {
        let dir = tmpdir("off");
        let log = DeliveryLog::new(&dir, 0);
        assert!(!log.is_enabled());
        log.append(&rec("gh", "a", 1)).unwrap();
        assert!(!dir.exists());
        assert!(log.read_all().is_empty());
    }

    #[test]
    fn log_skips_malformed_lines_and_sanitizes_path() {
        let dir = tmpdir("bad");
        let log = DeliveryLog::new(&dir, 10);
        log.append(&rec("gh", "a", 1)).unwrap();
        let p = log.path_for("gh");
        let mut f = std::fs::OpenOptions::new().append(true).open(&p).unwrap();
        writeln!(f, "not json").unwrap();
        drop(f);
        log.append(&rec("gh", "b", 2)).unwrap();
        assert_eq!(log.read("gh").len(), 2);
        assert!(log.path_for("../evil").starts_with(&dir));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn plan_dispatch_reports_filters() {
        let reg = registry();
        let plan = plan_dispatch(&reg.only("gh"), &wh("w"));
        assert_eq!(plan.len(), 2);
        assert!(plan[0].filters_pass);
        assert!(
            !plan[1].filters_pass,
            "action=closed filter fails on opened"
        );
        assert!(plan_dispatch(&reg.only("nope"), &wh("w")).is_empty());
        assert_eq!(reg.plugin_ids(), vec!["gh", "other"]);
    }

    #[test]
    fn sample_payloads_are_tagged_objects() {
        for ev in ["pull_request", "push", "issues", "ping", "custom.event"] {
            let p = sample_payload(ev);
            assert!(p.is_object(), "{ev}");
            assert_eq!(p["aish_test"], json!(true));
        }
        assert_eq!(sample_payload("pull_request")["action"], json!("opened"));
        let w = synth_webhook("gh", "push", json!({}));
        assert!(w.id.starts_with("test_"));
        assert_eq!(w.tenant_id, "local");
    }

    #[tokio::test]
    async fn dispatcher_calls_delivery_sink_once() {
        let reg = Arc::new(registry().only("other"));
        let seen: Arc<Mutex<Vec<(String, usize)>>> = Arc::new(Mutex::new(Vec::new()));
        let s2 = seen.clone();
        let d = WebhookDispatcher::new(reg).with_delivery_sink(Arc::new(move |w, outs| {
            s2.lock().unwrap().push((w.id.clone(), outs.len()));
        }));
        d.dispatch(&wh("w9")).await;
        assert_eq!(*seen.lock().unwrap(), vec![("w9".to_string(), 1)]);
    }
}
