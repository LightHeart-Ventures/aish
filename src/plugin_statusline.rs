//! First-class declarative plugin statusline segments (TASK-318, SPR-073).
//!
//! Phase 1 (TASK-316) gave plugins a *file convention*: arm a timer, pick the
//! magic `~/.aish/state/statusline/<id>.txt` cache path yourself, and write a
//! one-line badge there; core's footer reader folds any fresh `*.txt` onto the
//! SecondStatusLine. That works but is loose — the plugin owns the cache path,
//! the write, and the staleness dance, and any process can drop a file in.
//!
//! Phase 2b makes it *first-class*. A plugin declares only:
//!
//! ```json
//! { "provides": { "statusline": { "command": "statusline.sh", "every": "10m" } } }
//! ```
//!
//! and **core** owns the rest: at startup [`arm`] spawns ONE cheap detached
//! `tokio` loop per declared statusline that fork/execs `command` every `every`,
//! captures its first non-empty stdout line, and stores it in a core-owned
//! **in-memory** registry keyed by plugin id. The footer renderer reads that
//! registry ([`segments`]) directly — no file, no `*.txt` convention, no path
//! for the plugin to know. A segment ages out once its last refresh is older
//! than the render-time `stale_after`, so a wedged capture self-heals.
//!
//! Everything is error-isolated exactly like [`crate::plugin_timers`]: a
//! statusline that fails to parse, spawn, time out, or produce output is logged
//! best-effort and never disturbs the shell or the other plugins.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::plugin_timers::{parse_every, resolve_command};
use crate::plugins;

/// Default refresh cadence when a statusline omits (or malforms) `every`.
/// Generous by design — a statusline command may poll a slow external tool.
const DEFAULT_EVERY: Duration = Duration::from_secs(600);

/// Default per-run wall-clock timeout when a statusline omits `timeout_ms`.
const DEFAULT_TIMEOUT_MS: u64 = 60_000;

/// Small settle delay before the FIRST run so arming doesn't stampede
/// fork/execs at shell startup. After this, runs cadence at `every`.
const STARTUP_SETTLE: Duration = Duration::from_secs(2);

/// One cached statusline segment: the ready-to-render line (plugin owns its
/// ANSI color) and the monotonic instant it was last refreshed (for staleness).
#[derive(Debug, Clone)]
struct Segment {
    line: String,
    updated: Instant,
}

/// The process-wide, core-owned statusline cache: plugin id → latest segment.
/// This is the "core owns the cache" contract — no filesystem, no plugin-chosen
/// path. Written by the [`arm`] loops, read by [`segments`] on the footer tick.
fn registry() -> &'static Mutex<BTreeMap<String, Segment>> {
    static REGISTRY: OnceLock<Mutex<BTreeMap<String, Segment>>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(BTreeMap::new()))
}

/// Store (or refresh) a plugin's latest statusline line in the core registry.
fn record(plugin_id: &str, line: String) {
    if let Ok(mut map) = registry().lock() {
        map.insert(
            plugin_id.to_string(),
            Segment {
                line,
                updated: Instant::now(),
            },
        );
    }
}

/// Pick the segment line from a command's stdout: the first non-empty line,
/// trailing whitespace trimmed. `None` when the output is empty/whitespace-only
/// (the caller leaves the prior segment in place to age out).
fn pick_line(stdout: &str) -> Option<String> {
    stdout
        .lines()
        .map(str::trim_end)
        .find(|l| !l.trim().is_empty())
        .map(str::to_string)
}

/// Snapshot the fresh statusline segments from `map` as of `now`, sorted by
/// plugin id (the `BTreeMap` iteration order gives us that for free). A segment
/// whose last refresh is older than `stale_after` is skipped. Pure + injectable
/// (`now`) so staleness unit-tests without sleeping.
fn fresh_segments(
    map: &BTreeMap<String, Segment>,
    now: Instant,
    stale_after: Duration,
) -> Vec<String> {
    map.values()
        .filter(|seg| now.saturating_duration_since(seg.updated) <= stale_after)
        .map(|seg| seg.line.clone())
        .collect()
}

/// The fresh first-class statusline segments, ready to fold onto the
/// SecondStatusLine. Segments older than `stale_after` are hidden (a plugin
/// that stopped refreshing self-heals). Ordered by plugin id for stable output.
/// Cheap — a single mutex lock over an in-memory map; safe to call every footer
/// repaint tick.
pub fn segments(stale_after: Duration) -> Vec<String> {
    match registry().lock() {
        Ok(map) => fresh_segments(&map, Instant::now(), stale_after),
        Err(_) => Vec::new(),
    }
}

/// Discover every enabled plugin under `plugins_dir` and arm its declared
/// `provides.statusline`. Each becomes one detached `tokio` loop that refreshes
/// the core-owned in-memory cache. Must be called from within a `tokio` runtime
/// (aish's `#[tokio::main]` satisfies this). Returns the number armed.
pub fn arm(plugins_dir: &Path) -> usize {
    let plugins = plugins::discover(plugins_dir);
    let mut armed = 0usize;
    for p in plugins {
        let Some(sl) = p.manifest.statusline() else {
            continue;
        };
        if sl.command.trim().is_empty() {
            log(&format!(
                "{}: statusline disarmed — empty command",
                p.manifest.id
            ));
            continue;
        }
        let interval = sl
            .every
            .as_deref()
            .and_then(parse_every)
            .unwrap_or(DEFAULT_EVERY);
        let plugin_id = p.manifest.id.clone();
        let dir = p.dir.clone();
        let program = resolve_command(&dir, &sl.command);
        let args = sl.args.clone();
        let timeout = Duration::from_millis(sl.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS));
        armed += 1;
        tokio::spawn(async move {
            tokio::time::sleep(STARTUP_SETTLE).await;
            loop {
                run_once(&plugin_id, &dir, &program, &args, timeout).await;
                tokio::time::sleep(interval).await;
            }
        });
    }
    if armed > 0 {
        log(&format!("armed {armed} plugin statusline(s)"));
    }
    armed
}

/// Run one refresh: fork/exec `program` (with `args`) in `dir`, bounded by
/// `timeout`, and — on a non-empty first stdout line — store it in the core
/// registry. All failures are logged best-effort and swallowed; the prior
/// segment (if any) stays until it ages out.
async fn run_once(
    plugin_id: &str,
    dir: &Path,
    program: &Path,
    args: &[String],
    timeout: Duration,
) {
    let mut cmd = tokio::process::Command::new(program);
    cmd.args(args)
        .current_dir(dir)
        .env("AISH_PLUGIN_ID", plugin_id)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true); // reap the child if the timeout future drops us

    let child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            log(&format!(
                "{plugin_id}: statusline spawn FAILED ({}): {e}",
                program.display()
            ));
            return;
        }
    };

    let out = match tokio::time::timeout(timeout, child.wait_with_output()).await {
        Ok(Ok(o)) => o,
        Ok(Err(e)) => {
            log(&format!("{plugin_id}: statusline wait FAILED: {e}"));
            return;
        }
        Err(_) => {
            log(&format!("{plugin_id}: statusline timed out after {timeout:?}"));
            return;
        }
    };

    if !out.status.success() {
        log(&format!(
            "{plugin_id}: statusline exit {:?}",
            out.status.code()
        ));
        // Fall through: some tools print a usable line yet exit non-zero.
    }

    let stdout = String::from_utf8_lossy(&out.stdout);
    if let Some(line) = pick_line(&stdout) {
        record(plugin_id, line);
    }
}

/// Best-effort, quiet diagnostics: append a line to
/// `~/.aish/state/plugin-statusline.log`. Never writes to stdout/stderr (that
/// would corrupt the pinned footer) and never fails loudly.
fn log(msg: &str) {
    let base = std::path::PathBuf::from(std::env::var("HOME").unwrap_or_default())
        .join(".aish")
        .join("state");
    let _ = std::fs::create_dir_all(&base);
    let line = format!("[plugin-statusline] {msg}\n");
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(base.join("plugin-statusline.log"))
    {
        let _ = f.write_all(line.as_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugins::PluginManifest;
    use std::path::PathBuf;

    #[test]
    fn manifest_parses_statusline_minimal() {
        // AC1: plugin.json accepts `provides.statusline: { command }`.
        let m: PluginManifest =
            serde_json::from_str(r#"{"id":"demo","provides":{"statusline":{"command":"badge.sh"}}}"#)
                .unwrap();
        let sl = m.statusline().expect("statusline parsed");
        assert_eq!(sl.command, "badge.sh");
        assert!(sl.args.is_empty());
        assert!(sl.every.is_none());
        assert!(sl.timeout_ms.is_none());
    }

    #[test]
    fn manifest_parses_statusline_full() {
        let json = r#"{
            "id": "demo",
            "provides": { "statusline": {
                "command": "statusline.sh",
                "args": ["--json"],
                "every": "10m",
                "timeout_ms": 120000
            } }
        }"#;
        let m: PluginManifest = serde_json::from_str(json).unwrap();
        let sl = m.statusline().unwrap();
        assert_eq!(sl.command, "statusline.sh");
        assert_eq!(sl.args, vec!["--json".to_string()]);
        assert_eq!(parse_every(sl.every.as_deref().unwrap()), Some(DEFAULT_EVERY));
        assert_eq!(sl.timeout_ms, Some(120_000));
    }

    #[test]
    fn manifest_without_statusline_is_none() {
        let m: PluginManifest = serde_json::from_str(r#"{"id":"x"}"#).unwrap();
        assert!(m.statusline().is_none());
        // A `provides` block that declares only other capabilities → still none.
        let m2: PluginManifest =
            serde_json::from_str(r#"{"id":"x","provides":{"login":"acme"}}"#).unwrap();
        assert!(m2.statusline().is_none());
    }

    #[test]
    fn pick_line_first_nonempty() {
        assert_eq!(pick_line("⚡cc 63%w"), Some("⚡cc 63%w".to_string()));
        assert_eq!(pick_line("\n\n  first \nsecond"), Some("  first".to_string()));
        assert_eq!(pick_line(""), None);
        assert_eq!(pick_line("   \n\t\n"), None);
    }

    #[test]
    fn fresh_segments_orders_by_plugin_id_and_hides_stale() {
        let mut map = BTreeMap::new();
        let now = Instant::now();
        map.insert(
            "zeta".to_string(),
            Segment { line: "Z".to_string(), updated: now },
        );
        map.insert(
            "alpha".to_string(),
            Segment { line: "A".to_string(), updated: now },
        );
        let stale_after = Duration::from_secs(3600);
        // Both fresh, sorted by id (BTreeMap order): alpha before zeta.
        assert_eq!(
            fresh_segments(&map, now, stale_after),
            vec!["A".to_string(), "Z".to_string()]
        );
        // Advance "now" well past stale_after → everything hidden.
        let later = now
            .checked_add(stale_after + Duration::from_secs(1))
            .unwrap();
        assert!(fresh_segments(&map, later, stale_after).is_empty());
    }

    #[test]
    fn record_then_segments_roundtrips() {
        // Use a unique id so the shared process-wide registry stays isolated.
        let id = format!("test-roundtrip-{}", std::process::id());
        record(&id, "\x1b[2mhi\x1b[0m".to_string());
        let all = segments(Duration::from_secs(3600));
        assert!(all.iter().any(|s| s == "\x1b[2mhi\x1b[0m"), "got {all:?}");
    }

    #[tokio::test]
    async fn run_once_records_first_line() {
        let dir = std::env::temp_dir();
        let id = format!("test-runonce-{}", std::process::id());
        run_once(
            &id,
            &dir,
            &PathBuf::from("printf"),
            &["badge-line\\nignored\\n".to_string()],
            Duration::from_secs(10),
        )
        .await;
        let all = segments(Duration::from_secs(3600));
        assert!(all.iter().any(|s| s == "badge-line"), "got {all:?}");
    }

    #[tokio::test]
    async fn run_once_bad_program_is_silent() {
        // A non-existent program must not panic and must not record anything.
        let dir = std::env::temp_dir();
        let id = format!("test-badprog-{}", std::process::id());
        run_once(
            &id,
            &dir,
            &PathBuf::from("definitely-not-a-real-program-xyz"),
            &[],
            Duration::from_secs(5),
        )
        .await;
        let all = segments(Duration::from_secs(3600));
        assert!(!all.iter().any(|s| s.contains("badprog")));
    }
}
