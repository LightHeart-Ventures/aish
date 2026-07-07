//! Declarative plugin background timers (TASK-317, SPR-073).
//!
//! A plugin declares `provides.timers[]` in its `plugin.json`; each entry names
//! a program, an interval, and an optional cache file (see
//! [`crate::plugins::PluginTimer`]). At startup [`arm`] discovers every enabled
//! plugin and, for each timer, spawns ONE cheap detached `tokio` loop that
//! fork/execs the program every `every` and — when `cache` is set — writes its
//! stdout to a file. This is the always-on, turn-independent alternative to
//! throttling refresh work inside a `TurnEnd` hook: the statusline reads the
//! cache file, the timer keeps it fresh.
//!
//! Everything here is error-isolated. A timer that fails to parse, spawn, times
//! out, or can't write its cache is logged best-effort and never disturbs the
//! shell or the other timers ("a broken plugin never blocks startup").

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::plugins;

/// Default per-run wall-clock timeout when a timer omits `timeout_ms`.
const DEFAULT_TIMEOUT_MS: u64 = 60_000;

/// Small settle delay before a timer's FIRST run so arming doesn't stampede
/// fork/execs at shell startup. After this, runs cadence at `every`.
const STARTUP_SETTLE: Duration = Duration::from_secs(2);

/// Parse a compact interval string into a [`Duration`].
///
/// Accepts a bare integer (seconds) or an integer with a single unit suffix:
/// `s` (seconds), `m` (minutes), `h` (hours), `d` (days) — case-insensitive,
/// surrounding whitespace tolerated. Returns `None` for an empty, zero, or
/// unparseable value so the caller disarms the timer.
///
/// ```ignore
/// assert_eq!(parse_every("30s"), Some(Duration::from_secs(30)));
/// assert_eq!(parse_every("10m"), Some(Duration::from_secs(600)));
/// assert_eq!(parse_every("90"),  Some(Duration::from_secs(90)));
/// assert_eq!(parse_every("0"),   None);
/// assert_eq!(parse_every("soon"),None);
/// ```
pub fn parse_every(s: &str) -> Option<Duration> {
    let s = s.trim();
    let last = s.chars().last()?;
    let (num, mult) = match last {
        's' | 'S' => (&s[..s.len() - 1], 1u64),
        'm' | 'M' => (&s[..s.len() - 1], 60),
        'h' | 'H' => (&s[..s.len() - 1], 3_600),
        'd' | 'D' => (&s[..s.len() - 1], 86_400),
        c if c.is_ascii_digit() => (s, 1),
        _ => return None,
    };
    let n: u64 = num.trim().parse().ok()?;
    let secs = n.checked_mul(mult)?;
    (secs > 0).then(|| Duration::from_secs(secs))
}

/// Resolve a timer `cache` path: an absolute path is honored as-is; a relative
/// path resolves under `~/.aish/` (so `"state/statusline/ccquota.txt"` lands at
/// `~/.aish/state/statusline/ccquota.txt`).
fn resolve_cache(cache: &str) -> PathBuf {
    let p = Path::new(cache);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        PathBuf::from(std::env::var("HOME").unwrap_or_default())
            .join(".aish")
            .join(p)
    }
}

/// Resolve a timer `command` to an executable path. If it names a file that
/// exists inside the plugin directory, that file runs; otherwise the value is
/// used verbatim (PATH lookup by `tokio::process::Command`).
fn resolve_command(dir: &Path, command: &str) -> PathBuf {
    let candidate = dir.join(command);
    if candidate.exists() {
        candidate
    } else {
        PathBuf::from(command)
    }
}

/// Discover every enabled plugin under `plugins_dir` and arm its declared
/// timers. Each timer becomes one detached `tokio` loop. Must be called from
/// within a `tokio` runtime (aish's `#[tokio::main]` satisfies this). Returns
/// the number of timers armed (0 when none declared / all disarmed).
pub fn arm(plugins_dir: &Path) -> usize {
    let plugins = plugins::discover(plugins_dir);
    let mut armed = 0usize;
    for p in plugins {
        for (idx, timer) in p.manifest.timers().iter().enumerate() {
            let Some(interval) = parse_every(&timer.every) else {
                log(&format!(
                    "{}: timer #{idx} disarmed — unparseable interval {:?}",
                    p.manifest.id, timer.every
                ));
                continue;
            };
            let plugin_id = p.manifest.id.clone();
            let dir = p.dir.clone();
            let program = resolve_command(&dir, &timer.command);
            let args = timer.args.clone();
            let cache = timer.cache.as_deref().map(resolve_cache);
            let timeout = Duration::from_millis(timer.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS));
            armed += 1;
            tokio::spawn(async move {
                tokio::time::sleep(STARTUP_SETTLE).await;
                loop {
                    run_once(&plugin_id, &dir, &program, &args, cache.as_deref(), timeout).await;
                    tokio::time::sleep(interval).await;
                }
            });
        }
    }
    if armed > 0 {
        log(&format!("armed {armed} plugin timer(s)"));
    }
    armed
}

/// Run one timer tick: fork/exec `program` (with `args`) in `dir`, bounded by
/// `timeout`, and — when `cache` is `Some` — atomically write the captured
/// stdout to that path. All failures are logged best-effort and swallowed.
async fn run_once(
    plugin_id: &str,
    dir: &Path,
    program: &Path,
    args: &[String],
    cache: Option<&Path>,
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
                "{plugin_id}: timer spawn FAILED ({}): {e}",
                program.display()
            ));
            return;
        }
    };

    let out = match tokio::time::timeout(timeout, child.wait_with_output()).await {
        Ok(Ok(o)) => o,
        Ok(Err(e)) => {
            log(&format!("{plugin_id}: timer wait FAILED: {e}"));
            return;
        }
        Err(_) => {
            log(&format!("{plugin_id}: timer timed out after {timeout:?}"));
            return;
        }
    };

    if let Some(path) = cache {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Err(e) = write_atomic(path, &out.stdout) {
            log(&format!(
                "{plugin_id}: timer cache write FAILED ({}): {e}",
                path.display()
            ));
        }
    }

    if !out.status.success() {
        log(&format!(
            "{plugin_id}: timer exit {:?}",
            out.status.code()
        ));
    }
}

/// Write `bytes` to `path` via a `<path>.tmp` + rename so a reader (the
/// statusline) never observes a half-written cache file.
fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)
}

/// Best-effort, quiet diagnostics: append a line to
/// `~/.aish/state/plugin-timers.log`. Never writes to stdout/stderr (that would
/// corrupt the pinned footer) and never fails loudly.
fn log(msg: &str) {
    let base = PathBuf::from(std::env::var("HOME").unwrap_or_default())
        .join(".aish")
        .join("state");
    let _ = std::fs::create_dir_all(&base);
    let line = format!("[plugin-timers] {msg}\n");
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(base.join("plugin-timers.log"))
    {
        let _ = f.write_all(line.as_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugins::PluginManifest;

    #[test]
    fn parse_every_units() {
        assert_eq!(parse_every("30s"), Some(Duration::from_secs(30)));
        assert_eq!(parse_every("10m"), Some(Duration::from_secs(600)));
        assert_eq!(parse_every("2h"), Some(Duration::from_secs(7_200)));
        assert_eq!(parse_every("1d"), Some(Duration::from_secs(86_400)));
        assert_eq!(parse_every("90"), Some(Duration::from_secs(90)));
        assert_eq!(parse_every("  15m "), Some(Duration::from_secs(900)));
        assert_eq!(parse_every("10M"), Some(Duration::from_secs(600)));
    }

    #[test]
    fn parse_every_rejects_bad() {
        assert_eq!(parse_every(""), None);
        assert_eq!(parse_every("0"), None);
        assert_eq!(parse_every("0s"), None);
        assert_eq!(parse_every("soon"), None);
        assert_eq!(parse_every("m"), None);
        assert_eq!(parse_every("-5"), None);
        assert_eq!(parse_every("1.5h"), None);
    }

    #[test]
    fn resolve_cache_absolute_vs_relative() {
        let abs = resolve_cache("/tmp/x.txt");
        assert_eq!(abs, PathBuf::from("/tmp/x.txt"));
        let rel = resolve_cache("state/statusline/ccquota.txt");
        assert!(rel.ends_with(".aish/state/statusline/ccquota.txt"));
    }

    #[test]
    fn manifest_parses_timers() {
        let json = r#"{
            "id": "demo",
            "provides": {
                "timers": [
                    { "command": "refresh.sh", "every": "10m",
                      "cache": "state/statusline/demo.txt" },
                    { "command": "date", "args": ["-u"], "every": "30s" }
                ]
            }
        }"#;
        let m: PluginManifest = serde_json::from_str(json).unwrap();
        let timers = m.timers();
        assert_eq!(timers.len(), 2);
        assert_eq!(timers[0].command, "refresh.sh");
        assert_eq!(parse_every(&timers[0].every), Some(Duration::from_secs(600)));
        assert_eq!(timers[0].cache.as_deref(), Some("state/statusline/demo.txt"));
        assert_eq!(timers[1].args, vec!["-u".to_string()]);
        assert!(timers[1].cache.is_none());
    }

    #[test]
    fn manifest_without_timers_is_empty() {
        let m: PluginManifest = serde_json::from_str(r#"{ "id": "x" }"#).unwrap();
        assert!(m.timers().is_empty());
    }

    #[tokio::test]
    async fn run_once_writes_cache() {
        let dir = std::env::temp_dir().join(format!("aish-timer-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let cache = dir.join("out.txt");
        run_once(
            "test",
            &dir,
            Path::new("printf"),
            &["hello-timer".to_string()],
            Some(&cache),
            Duration::from_secs(10),
        )
        .await;
        let body = std::fs::read_to_string(&cache).unwrap();
        assert_eq!(body, "hello-timer");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn run_once_bad_program_is_silent() {
        // A non-existent program must not panic and must not write the cache.
        let dir = std::env::temp_dir();
        let cache = dir.join(format!("aish-timer-missing-{}.txt", std::process::id()));
        let _ = std::fs::remove_file(&cache);
        run_once(
            "test",
            &dir,
            Path::new("definitely-not-a-real-program-xyz"),
            &[],
            Some(&cache),
            Duration::from_secs(5),
        )
        .await;
        assert!(!cache.exists());
    }
}
