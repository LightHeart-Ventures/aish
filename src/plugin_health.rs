//! Plugin error audit trail + runtime health (TASK-274, SPR-104).
//!
//! Two small pieces of plugin robustness state live here:
//!
//! 1. **Per-plugin errors log** — `<plugins_dir>/<id>/errors.jsonl`
//!    (`~/.aish/plugins/<id>/errors.jsonl`): one JSON object per line
//!    (`{ts, kind, source, message, recovery}`), kept as a ring of the
//!    [`MAX_ENTRIES`] most recent entries. Every write is best-effort — a full
//!    disk or read-only plugin dir must never affect the shell. Surfaced by
//!    `:plugin errors <id> [N]` and the health marker in `:plugin list`.
//!
//! 2. **Config-invalid registry** — the DERIVED, runtime-only set of plugins
//!    whose `config.json` failed validation (PO decision 2026-10-07: warn once,
//!    keep skills, skip hooks + webhook handlers; never auto-disable, never
//!    persisted). Refreshed by `crate::plugins::refresh_config_health` at
//!    startup and on every `:plugin reload|enable|disable|remove`; consulted by
//!    the Phase 1.6 webhook dispatcher, which deliberately does not depend on
//!    `crate::plugins`.
//!
//! Self-contained (std + serde only) so `tests/plugin_dispatcher_tests.rs` can
//! `#[path]`-include it alongside `plugin_dispatcher.rs`.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{OnceLock, RwLock};

/// File name of the per-plugin errors log, inside the plugin's directory.
pub const ERRORS_FILE: &str = "errors.jsonl";
/// Ring size: the log keeps at most this many (most recent) entries.
pub const MAX_ENTRIES: usize = 200;
/// Window for the "N recent errors" health marker in `:plugin list`.
pub const RECENT_WINDOW_SECS: u64 = 24 * 3600;

/// Error kinds recorded aish-side.
pub const KIND_CONFIG_INVALID: &str = "config_invalid";
pub const KIND_HOOK_TIMEOUT: &str = "hook_timeout";
pub const KIND_HOOK_FAILED: &str = "hook_failed";
pub const KIND_WEBHOOK_FAILED: &str = "webhook_failed";
pub const KIND_HANDLER_FAILED: &str = "handler_failed";
pub const KIND_HANDLER_TIMEOUT: &str = "handler_timeout";

/// One audit-trail entry.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ErrorEntry {
    /// Unix epoch seconds.
    pub ts: u64,
    /// One of the `KIND_*` constants.
    pub kind: String,
    /// Where it happened: `config.json`, `on_init`, `webhook pull_request`, …
    pub source: String,
    /// The error itself.
    pub message: String,
    /// What aish did about it / what the operator should do.
    pub recovery: String,
}

impl ErrorEntry {
    /// A new entry stamped with the current time.
    pub fn new(
        kind: impl Into<String>,
        source: impl Into<String>,
        message: impl Into<String>,
        recovery: impl Into<String>,
    ) -> Self {
        Self {
            ts: now_secs(),
            kind: kind.into(),
            source: source.into(),
            message: message.into(),
            recovery: recovery.into(),
        }
    }
}

/// Current unix time in seconds (0 if the clock is before the epoch).
pub fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Keep a plugin id safe as a path component: anything outside
/// `[A-Za-z0-9._-]` becomes `_`; an empty / all-dots id becomes `unknown`.
fn sanitize_id(id: &str) -> String {
    let s: String = id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect();
    if s.is_empty() || s.chars().all(|c| c == '.') {
        "unknown".to_string()
    } else {
        s
    }
}

/// The on-disk directory of plugin `id`: `<plugins_dir>/<id>` when it holds a
/// `plugin.json`; otherwise the directory whose manifest declares that `id`
/// (directory name ≠ id); otherwise `<plugins_dir>/<sanitized id>`.
pub fn plugin_dir_for(plugins_dir: &Path, id: &str) -> PathBuf {
    let safe = sanitize_id(id);
    let direct = plugins_dir.join(&safe);
    if safe == id && direct.join("plugin.json").is_file() {
        return direct;
    }
    if let Ok(entries) = std::fs::read_dir(plugins_dir) {
        for e in entries.flatten() {
            let p = e.path();
            let Ok(text) = std::fs::read_to_string(p.join("plugin.json")) else {
                continue;
            };
            let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else {
                continue;
            };
            if v.get("id").and_then(|x| x.as_str()) == Some(id) {
                return p;
            }
        }
    }
    direct
}

/// `<plugin dir>/errors.jsonl` for plugin `id`.
pub fn errors_path(plugins_dir: &Path, id: &str) -> PathBuf {
    plugin_dir_for(plugins_dir, id).join(ERRORS_FILE)
}

/// Append `entry` to plugin `id`'s log, keeping at most [`MAX_ENTRIES`]
/// lines (oldest dropped; the trimmed rewrite goes through `.tmp` + rename).
pub fn append(plugins_dir: &Path, id: &str, entry: &ErrorEntry) -> std::io::Result<()> {
    let path = errors_path(plugins_dir, id);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let line = serde_json::to_string(entry).map_err(std::io::Error::other)?;
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    let lines: Vec<&str> = existing.lines().filter(|l| !l.trim().is_empty()).collect();
    if lines.len() < MAX_ENTRIES {
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)?;
        f.write_all(format!("{line}\n").as_bytes())?;
        return Ok(());
    }
    let keep = &lines[lines.len() - (MAX_ENTRIES - 1)..];
    let mut body = keep.join("\n");
    body.push('\n');
    body.push_str(&line);
    body.push('\n');
    let tmp = path.with_extension("jsonl.tmp");
    std::fs::write(&tmp, body)?;
    std::fs::rename(&tmp, &path)
}

/// Best-effort [`append`]: an I/O failure is swallowed (the audit trail must
/// never take the shell down with it).
pub fn record(
    plugins_dir: &Path,
    id: &str,
    kind: &str,
    source: &str,
    message: &str,
    recovery: &str,
) {
    let _ = append(
        plugins_dir,
        id,
        &ErrorEntry::new(kind, source, message, recovery),
    );
}

/// Best-effort append that skips the write when the newest entry already has
/// the same `kind` + `message` — so a persistently-broken config is logged once,
/// not once per startup.
pub fn record_unless_repeat(plugins_dir: &Path, id: &str, entry: &ErrorEntry) {
    if let Some(last) = read_all(plugins_dir, id).last() {
        if last.kind == entry.kind && last.message == entry.message {
            return;
        }
    }
    let _ = append(plugins_dir, id, entry);
}

/// Every parseable entry in plugin `id`'s log, oldest first. Missing file →
/// empty; malformed lines are skipped.
pub fn read_all(plugins_dir: &Path, id: &str) -> Vec<ErrorEntry> {
    std::fs::read_to_string(errors_path(plugins_dir, id))
        .unwrap_or_default()
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

/// The newest `n` entries, oldest first.
pub fn tail(plugins_dir: &Path, id: &str, n: usize) -> Vec<ErrorEntry> {
    let mut all = read_all(plugins_dir, id);
    let len = all.len();
    if len > n { all.split_off(len - n) } else { all }
}

/// Entries recorded within [`RECENT_WINDOW_SECS`] of `now`.
pub fn recent_count(plugins_dir: &Path, id: &str, now: u64) -> usize {
    let since = now.saturating_sub(RECENT_WINDOW_SECS);
    read_all(plugins_dir, id)
        .iter()
        .filter(|e| e.ts >= since)
        .count()
}

/// Render entries one per line: `YYYY-MM-DD HH:MM:SSZ kind source: message → recovery`.
pub fn format_entries(entries: &[ErrorEntry]) -> String {
    entries
        .iter()
        .map(|e| {
            let mut row = format!("{} {} {}: {}", fmt_utc(e.ts), e.kind, e.source, e.message);
            if !e.recovery.is_empty() {
                row.push_str(&format!(" → {}", e.recovery));
            }
            row
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Unix seconds → `YYYY-MM-DD HH:MM:SSZ` (UTC), via Howard Hinnant's
/// civil-from-days — no chrono dependency for one timestamp column.
fn fmt_utc(ts: u64) -> String {
    let days = (ts / 86_400) as i64;
    let secs = ts % 86_400;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02}Z",
        secs / 3600,
        (secs % 3600) / 60,
        secs % 60
    )
}

/// Config-invalid sets, keyed by plugins dir so independent plugin roots (and
/// parallel tests using private temp dirs) never clobber one another.
type Registry = HashMap<PathBuf, HashMap<String, String>>;

fn registry() -> &'static RwLock<Registry> {
    static REG: OnceLock<RwLock<Registry>> = OnceLock::new();
    REG.get_or_init(|| RwLock::new(HashMap::new()))
}

/// Replace the runtime config-invalid set (`id → validation error`) for the
/// plugins under `plugins_dir`.
pub fn set_config_invalid(plugins_dir: &Path, map: HashMap<String, String>) {
    if let Ok(mut g) = registry().write() {
        g.insert(plugins_dir.to_path_buf(), map);
    }
}

/// The validation error for plugin `id` under `plugins_dir` when its config is
/// currently invalid (as of the last refresh), else `None`.
pub fn config_invalid_reason(plugins_dir: &Path, id: &str) -> Option<String> {
    registry()
        .read()
        .ok()
        .and_then(|g| g.get(plugins_dir).and_then(|m| m.get(id).cloned()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tempdir() -> PathBuf {
        static SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let p = std::env::temp_dir().join(format!(
            "aish-plugin-health-{}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn plugin(root: &Path, dirname: &str, id: &str) {
        let d = root.join(dirname);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("plugin.json"), format!(r#"{{"id":"{id}"}}"#)).unwrap();
    }

    #[test]
    fn append_and_read_round_trip() {
        let root = tempdir();
        plugin(&root, "gh", "gh");
        let e = ErrorEntry::new(KIND_HOOK_FAILED, "on_init", "exit status 2", "skipped");
        append(&root, "gh", &e).unwrap();
        assert_eq!(read_all(&root, "gh"), vec![e]);
        assert_eq!(errors_path(&root, "gh"), root.join("gh").join(ERRORS_FILE));
    }

    #[test]
    fn ring_keeps_newest_200() {
        let root = tempdir();
        plugin(&root, "gh", "gh");
        for i in 0..(MAX_ENTRIES + 5) {
            append(&root, "gh", &ErrorEntry::new("k", "s", format!("m{i}"), "")).unwrap();
        }
        let all = read_all(&root, "gh");
        assert_eq!(all.len(), MAX_ENTRIES);
        assert_eq!(all[0].message, "m5");
        assert_eq!(all.last().unwrap().message, format!("m{}", MAX_ENTRIES + 4));
        assert_eq!(tail(&root, "gh", 3).len(), 3);
        assert_eq!(tail(&root, "gh", 3)[2].message, all.last().unwrap().message);
    }

    #[test]
    fn malformed_lines_are_skipped() {
        let root = tempdir();
        plugin(&root, "gh", "gh");
        let good = serde_json::to_string(&ErrorEntry::new("k", "s", "ok", "r")).unwrap();
        std::fs::write(
            errors_path(&root, "gh"),
            format!("{{ nope\n{good}\n\nnot json\n"),
        )
        .unwrap();
        let all = read_all(&root, "gh");
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].message, "ok");
    }

    #[test]
    fn record_unless_repeat_dedups_consecutive() {
        let root = tempdir();
        plugin(&root, "gh", "gh");
        let e = ErrorEntry::new(KIND_CONFIG_INVALID, "config.json", "bad", "r");
        record_unless_repeat(&root, "gh", &e);
        record_unless_repeat(&root, "gh", &e);
        assert_eq!(read_all(&root, "gh").len(), 1);
        let e2 = ErrorEntry::new(KIND_CONFIG_INVALID, "config.json", "worse", "r");
        record_unless_repeat(&root, "gh", &e2);
        assert_eq!(read_all(&root, "gh").len(), 2);
    }

    #[test]
    fn recent_count_honours_window() {
        let root = tempdir();
        plugin(&root, "gh", "gh");
        let now = 10_000_000;
        let mut old = ErrorEntry::new("k", "s", "old", "");
        old.ts = now - RECENT_WINDOW_SECS - 1;
        let mut fresh = ErrorEntry::new("k", "s", "fresh", "");
        fresh.ts = now - 10;
        append(&root, "gh", &old).unwrap();
        append(&root, "gh", &fresh).unwrap();
        assert_eq!(recent_count(&root, "gh", now), 1);
        assert_eq!(recent_count(&root, "missing", now), 0);
    }

    #[test]
    fn plugin_dir_resolves_mismatched_dirname_and_sanitizes() {
        let root = tempdir();
        plugin(&root, "github-plugin", "gh");
        assert_eq!(plugin_dir_for(&root, "gh"), root.join("github-plugin"));
        let p = plugin_dir_for(&root, "../evil");
        assert!(p.starts_with(&root), "{p:?}");
        assert_eq!(p, root.join(".._evil"));
    }

    #[test]
    fn format_entries_renders_utc_row() {
        let mut e = ErrorEntry::new(KIND_HOOK_TIMEOUT, "on_init", "timed out", "killed");
        e.ts = 86_400 * 365 + 3661; // 1971-01-01 01:01:01Z
        assert_eq!(
            format_entries(&[e]),
            "1971-01-01 01:01:01Z hook_timeout on_init: timed out → killed"
        );
        assert_eq!(fmt_utc(1_700_000_000), "2023-11-14 22:13:20Z");
    }

    #[test]
    fn config_invalid_registry_replaces() {
        let root = tempdir();
        let other = tempdir();
        let mut m = HashMap::new();
        m.insert("a".to_string(), "bad".to_string());
        set_config_invalid(&root, m);
        assert_eq!(config_invalid_reason(&root, "a").as_deref(), Some("bad"));
        assert_eq!(config_invalid_reason(&other, "a"), None);
        set_config_invalid(&root, HashMap::new());
        assert_eq!(config_invalid_reason(&root, "a"), None);
    }
}
