//! `:alert <condition>` — the operator arms a lightweight monitor and aish
//! surfaces a message in the interactive console the moment the condition is
//! met: a **detailed** line in the OutputField (printed above the prompt by the
//! background presenter) and a **shortened** banner on the SecondStatusLine. An
//! audible cue — deliberately DISTINCT from the single ASCII-BEL a finished
//! worker plays (see [`crate::tools::play_finish_bell`]) — accompanies it.
//!
//! Two evaluation strategies keep it cheap yet general:
//!   * **Native probes** run token-free on the presenter's poll tick — a
//!     file-change watch (mtime / existence flips) or a command probe (exit 0
//!     with an optional stdout match), e.g. a `gh pr view <n>` merge check.
//!   * **Semantic** conditions phrased freely ("let me know when PR 333 is
//!     merged") that no local probe can decide are delegated to a background
//!     coordinator, which resolves them with its full toolset and fires the
//!     alert through the same surfacing path.
//!
//! This module holds the pure, side-effect-light core: the domain types,
//! free-text parsing, native polling, the distinct bell, and the two rendered
//! strings. Persistence lives in `db.rs`; the arm/monitor/surface wiring lives
//! in `repl.rs`.

use regex::Regex;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::{SystemTime, UNIX_EPOCH};

/// How long, by default, between native poll evaluations of a single alert.
/// The presenter ticks far more often than this; each alert throttles itself to
/// this cadence so a `gh` probe doesn't run every frame.
pub const DEFAULT_POLL_SECS: i64 = 15;

/// What an alert watches and how aish decides it fired.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum AlertKind {
    /// Fire when a filesystem path's mtime moves, or it is created/removed.
    /// Baseline is captured at arm time; `baseline_mtime`/`baseline_exists`
    /// describe the world when the alert was set.
    FileChange {
        path: PathBuf,
        baseline_mtime: Option<i64>,
        baseline_exists: bool,
    },
    /// Fire when a shell-free command probe succeeds: exit status 0 AND, when
    /// `expect` is set, its stdout contains that (case-insensitive) substring.
    Command {
        program: String,
        args: Vec<String>,
        /// Optional stdout substring that must be present for a fire.
        expect: Option<String>,
    },
    /// A free-text condition no local probe can decide. Delegated to a
    /// background coordinator that resolves it and fires via `set_alert`.
    Semantic { description: String },
}

impl AlertKind {
    /// True when the presenter can evaluate this kind itself (no agent needed).
    pub fn is_native(&self) -> bool {
        !matches!(self, AlertKind::Semantic { .. })
    }

    /// Serialize the kind to a JSON blob for persistence.
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_else(|_| "null".into())
    }

    /// Reconstruct a kind from its persisted JSON blob.
    pub fn from_json(s: &str) -> Option<Self> {
        serde_json::from_str(s).ok()
    }

    /// A stable tag for persistence / display.
    pub fn tag(&self) -> &'static str {
        match self {
            AlertKind::FileChange { .. } => "file",
            AlertKind::Command { .. } => "command",
            AlertKind::Semantic { .. } => "semantic",
        }
    }
}

/// An armed alert. `condition` is the operator's raw request, preserved verbatim
/// for display and for a delegated coordinator's task.
#[derive(Clone, Debug)]
pub struct Alert {
    pub id: i64,
    pub condition: String,
    pub kind: AlertKind,
    /// Audible cue on fire (default true; `:alert ... --silent` clears it).
    pub audible: bool,
    /// Epoch-seconds of the last native evaluation (0 = never).
    pub last_checked: i64,
}

impl Alert {
    /// True once enough time has elapsed since the last native check.
    pub fn due(&self, now: i64) -> bool {
        self.kind.is_native() && now.saturating_sub(self.last_checked) >= DEFAULT_POLL_SECS
    }
}

/// The rendered surfacing produced when an alert fires.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Fired {
    /// Full line for the OutputField / operator console.
    pub detail: String,
    /// Compact banner for the SecondStatusLine.
    pub short: String,
}

fn now_epoch() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn mtime_of(path: &std::path::Path) -> Option<i64> {
    std::fs::metadata(path)
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
}

/// Extract the first filesystem-looking token from free text.
fn first_path(text: &str) -> Option<PathBuf> {
    // Absolute, home-relative, or dot-relative paths. Trailing punctuation is
    // trimmed so "on /tm/f;" yields "/tmp/f".
    let re = Regex::new(r"(~?/[^\s]+|\.{1,2}/[^\s]+)").ok()?;
    let m = re.find(text)?;
    let raw = m.as_str().trim_end_matches(|c: char| ",.;:)!?".contains(c));
    if raw.is_empty() {
        return None;
    }
    let expanded = if let Some(rest) = raw.strip_prefix("~/") {
        dirs_home().map(|h| h.join(rest)).unwrap_or_else(|| PathBuf::from(raw))
    } else {
        PathBuf::from(raw)
    };
    Some(expanded)
}

fn dirs_home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

/// Parse a free-text `:alert` condition into a concrete [`AlertKind`].
///
/// Heuristics, in order:
///   1. A PR/pull-request number plus a "merge" cue → a `gh pr view` probe.
///   2. A filesystem path plus a change/watch cue → a file-change watch.
///   3. Anything else → a semantic condition for a coordinator to resolve.
pub fn parse_condition(condition: &str) -> AlertKind {
    let lower = condition.to_lowercase();

    // 1) PR merged — the canonical example. "PR 333 is merged".
    if lower.contains("merg") {
        if let Ok(re) = Regex::new(r"(?:pr|pull\s*request)\D{0,6}(\d{1,7})") {
            if let Some(c) = re.captures(&lower) {
                if let Some(num) = c.get(1) {
                    let n = num.as_str().to_string();
                    return AlertKind::Command {
                        program: "gh".into(),
                        args: vec![
                            "pr".into(),
                            "view".into(),
                            n,
                            "--json".into(),
                            "state".into(),
                            "-q".into(),
                            ".state".into(),
                        ],
                        expect: Some("MERGED".into()),
                    };
                }
            }
        }
    }

    // 2) File change / existence watch.
    let file_cue = ["file", "change", "chang", "modif", "watch", "exist", "creat", "delet", "touch", "appear"]
        .iter()
        .any(|k| lower.contains(k));
    if file_cue {
        if let Some(path) = first_path(condition) {
            let baseline_exists = path.exists();
            let baseline_mtime = mtime_of(&path);
            return AlertKind::FileChange {
                path,
                baseline_mtime,
                baseline_exists,
            };
        }
    }

    // 3) Delegate to a coordinator.
    AlertKind::Semantic {
        description: condition.trim().to_string(),
    }
}

/// Evaluate a native alert once. Returns `Some(Fired)` when the condition is met
/// now, else `None`. Non-native (semantic) alerts always return `None` here —
/// they fire through the coordinator path via `set_alert`.
///
/// `now` is epoch-seconds (pass [`now_epoch`] result); the caller updates
/// `last_checked` afterwards.
pub fn poll(alert: &Alert) -> Option<Fired> {
    match &alert.kind {
        AlertKind::FileChange {
            path,
            baseline_mtime,
            baseline_exists,
        } => {
            let exists = path.exists();
            let changed_existence = exists != *baseline_exists;
            let changed_mtime = exists && mtime_of(path) != *baseline_mtime;
            if changed_existence || changed_mtime {
                let what = if changed_existence {
                    if exists { "created" } else { "removed" }
                } else {
                    "changed"
                };
                Some(Fired {
                    detail: format!(
                        "\x1b[1;33m⚠ ALERT\x1b[0m  {} — file {} {}",
                        alert.condition,
                        path.display(),
                        what
                    ),
                    short: format!("⚠ {} {}", short_path(path), what),
                })
            } else {
                None
            }
        }
        AlertKind::Command {
            program,
            args,
            expect,
        } => {
            let out = std::process::Command::new(program)
                .args(args)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .output()
                .ok()?;
            if !out.status.success() {
                return None;
            }
            let stdout = String::from_utf8_lossy(&out.stdout);
            let met = match expect {
                Some(want) => stdout.to_lowercase().contains(&want.to_lowercase()),
                None => true,
            };
            if met {
                Some(Fired {
                    detail: format!("\x1b[1;33m⚠ ALERT\x1b[0m  {}", alert.condition),
                    short: format!("⚠ {}", truncate(&alert.condition, 40)),
                })
            } else {
                None
            }
        }
        AlertKind::Semantic { .. } => None,
    }
}

/// A short, tail-biased path label for the compact status line.
fn short_path(path: &std::path::Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| truncate(&path.to_string_lossy(), 24))
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() > max {
        let head: String = s.chars().take(max.saturating_sub(1)).collect();
        format!("{}…", head.trim_end())
    } else {
        s.to_string()
    }
}

/// Render a fired alert's compact banner without a probe (used when a delegated
/// coordinator reports a semantic condition met via `set_alert`).
pub fn render_semantic_fired(condition: &str, message: &str) -> Fired {
    let msg = if message.trim().is_empty() {
        condition.trim()
    } else {
        message.trim()
    };
    Fired {
        detail: format!("\x1b[1;33m⚠ ALERT\x1b[0m  {msg}"),
        short: format!("⚠ {}", truncate(msg, 40)),
    }
}

/// Emit the audible alert cue — deliberately DISTINCT from the single BEL a
/// finished worker plays. Default is a spaced triple-BEL pattern so the operator
/// can tell an *alert* apart from a *worker done* by ear. Opt out per-alert via
/// the `audible` flag or globally with `AISH_ALERT_BELL=0`; override the sound
/// with `AISH_ALERT_BELL_CMD` (whitespace-split, shell-free, fire-and-forget).
pub fn play_alert_bell() {
    if matches!(std::env::var("AISH_ALERT_BELL").ok().as_deref(), Some("0") | Some("false") | Some("off")) {
        return;
    }
    if let Ok(cmd) = std::env::var("AISH_ALERT_BELL_CMD") {
        let cmd = cmd.trim();
        if !cmd.is_empty() {
            let mut parts = cmd.split_whitespace();
            if let Some(prog) = parts.next() {
                let args: Vec<&str> = parts.collect();
                let _ = std::process::Command::new(prog)
                    .args(&args)
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn();
                return;
            }
        }
    }
    // Default: a spaced triple-BEL — three beeps with a short gap, unlike the
    // worker's single BEL. Written to /dev/tty so it survives stderr redirection.
    use std::io::Write;
    let pattern: &[u8] = b"\x07 \x07 \x07";
    if let Ok(mut tty) = std::fs::OpenOptions::new().write(true).open("/dev/tty") {
        let _ = tty.write_all(pattern);
        let _ = tty.flush();
    } else {
        let mut err = std::io::stderr();
        let _ = err.write_all(pattern);
        let _ = err.flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_pr_merge_condition() {
        let k = parse_condition("let me know when PR 333 is merged");
        match k {
            AlertKind::Command { program, args, expect } => {
                assert_eq!(program, "gh");
                assert!(args.contains(&"333".to_string()));
                assert_eq!(expect.as_deref(), Some("MERGED"));
            }
            other => panic!("expected Command, got {other:?}"),
        }
    }

    #[test]
    fn parses_pull_request_wording() {
        assert!(matches!(
            parse_condition("notify me once pull request #42 gets merged"),
            AlertKind::Command { .. }
        ));
    }

    #[test]
    fn parses_file_change_condition() {
        let k = parse_condition("watch for a file change on /tmp/thisfile");
        match k {
            AlertKind::FileChange { path, .. } => {
                assert_eq!(path, PathBuf::from("/tmp/thisfile"));
            }
            other => panic!("expected FileChange, got {other:?}"),
        }
    }

    #[test]
    fn trailing_punctuation_trimmed_from_path() {
        let k = parse_condition("watch for changes on /tmp/f;");
        match k {
            AlertKind::FileChange { path, .. } => assert_eq!(path, PathBuf::from("/tmp/f")),
            other => panic!("expected FileChange, got {other:?}"),
        }
    }

    #[test]
    fn falls_back_to_semantic() {
        assert!(matches!(
            parse_condition("tell me when the deploy looks healthy"),
            AlertKind::Semantic { .. }
        ));
    }

    #[test]
    fn file_change_fires_on_existence_flip() {
        let dir = std::env::temp_dir();
        let p = dir.join(format!("aish-alert-test-{}", now_epoch()));
        let _ = std::fs::remove_file(&p);
        let alert = Alert {
            id: 1,
            condition: "watch /tmp/x".into(),
            kind: AlertKind::FileChange {
                path: p.clone(),
                baseline_mtime: None,
                baseline_exists: false,
            },
            audible: true,
            last_checked: 0,
        };
        assert!(poll(&alert).is_none(), "no file yet → no fire");
        std::fs::write(&p, b"hi").unwrap();
        let fired = poll(&alert).expect("creation should fire");
        assert!(fired.short.contains("created"));
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn command_probe_expect_mismatch_does_not_fire() {
        // `true` exits 0 with empty stdout; expecting MERGED must not fire.
        let alert = Alert {
            id: 2,
            condition: "pr 1 merged".into(),
            kind: AlertKind::Command {
                program: "true".into(),
                args: vec![],
                expect: Some("MERGED".into()),
            },
            audible: true,
            last_checked: 0,
        };
        assert!(poll(&alert).is_none());
    }

    #[test]
    fn command_probe_no_expect_fires_on_exit_zero() {
        let alert = Alert {
            id: 3,
            condition: "run true".into(),
            kind: AlertKind::Command {
                program: "true".into(),
                args: vec![],
                expect: None,
            },
            audible: true,
            last_checked: 0,
        };
        assert!(poll(&alert).is_some());
    }
}
