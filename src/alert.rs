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

/// Which filesystem transition a [`AlertKind::FileChange`] watch fires on.
///
/// Rows persisted before this field existed deserialize as
/// [`FileWatchMode::Any`] thanks to `#[serde(default)]`, so alerts armed by an
/// older build keep working across an upgrade.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum FileWatchMode {
    /// Any observable transition: creation, removal, or an mtime bump.
    #[default]
    Any,
    /// Only a false → true existence flip ("when build.done appears").
    Appear,
    /// Only a true → false existence flip ("when the lockfile is deleted").
    Vanish,
}

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
        /// Which transition fires. Absent in rows armed by older builds, hence
        /// `#[serde(default)]` → [`FileWatchMode::Any`].
        #[serde(default)]
        mode: FileWatchMode,
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
    /// Persisted per-row; the presenter reads it back from the fired-row tuple
    /// (`take_fired`) rather than this in-memory copy, so it's `#[allow]`ed here.
    #[allow(dead_code)]
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

#[allow(dead_code)] // test helper + documents the epoch-seconds contract for `poll`'s `now`
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
///
/// Three passes, highest-confidence first:
///   1. An explicitly rooted path — absolute, `~/`-relative, or `./`-relative.
///   2. A quoted token that looks like a path (so names with spaces survive).
///   3. A bare relative token: one containing `/` (`target/release/aish`) or a
///      plain filename with a letter-led extension (`build.done`).
///
/// Version strings (`1.2.3`), URLs, emails, and flags are rejected so ordinary
/// prose never becomes a path. An extension-less bare name (`Makefile`) is NOT
/// recognized — too close to prose; write `./Makefile` to watch one.
fn first_path(text: &str) -> Option<PathBuf> {
    // Quoted first: a quoted token is the most explicit signal, and checking it
    // before the rooted pass keeps "/tmp/my notes.txt" whole instead of
    // truncating it to "/tmp/my" at the first space.
    quoted_path(text)
        .or_else(|| rooted_path(text))
        .or_else(|| bare_relative_path(text))
}

/// Pass 1: an explicitly rooted path. Trailing punctuation is trimmed so
/// "on /tmp/f;" yields "/tmp/f".
fn rooted_path(text: &str) -> Option<PathBuf> {
    let re = Regex::new(r"(~?/[^\s]+|\.{1,2}/[^\s]+)").ok()?;
    for m in re.find_iter(text) {
        // A ':' immediately before the slashes means we're inside a URL
        // ("https://host/path"), not looking at a filesystem path.
        if text[..m.start()].ends_with(':') {
            continue;
        }
        let raw = trim_path_punct(m.as_str());
        if path_like(raw) {
            return Some(expand_home(raw));
        }
    }
    None
}

/// Pass 2: a quoted token, which is how operators express names with spaces.
fn quoted_path(text: &str) -> Option<PathBuf> {
    let re = Regex::new(r#"["']([^"']{1,200})["']"#).ok()?;
    for c in re.captures_iter(text) {
        if let Some(g) = c.get(1) {
            let raw = trim_path_punct(g.as_str().trim());
            if looks_like_filename(raw) {
                return Some(expand_home(raw));
            }
        }
    }
    None
}

/// Pass 3: a bare relative path or filename sitting in the prose.
fn bare_relative_path(text: &str) -> Option<PathBuf> {
    text.split_whitespace()
        .map(trim_path_punct)
        .find(|raw| looks_like_filename(raw))
        .map(expand_home)
}

/// Strip surrounding quotes and trailing sentence punctuation from a token.
fn trim_path_punct(s: &str) -> &str {
    s.trim_matches('"')
        .trim_matches('\'')
        .trim_end_matches(|c: char| ",.;:)!?".contains(c))
}

/// Cheap sanity gate shared by every pass: not a URL, email, or flag.
fn path_like(raw: &str) -> bool {
    !raw.is_empty()
        && raw.len() <= 4096
        && !raw.contains("://")
        && !raw.contains('@')
        && !raw.starts_with('-')
}

/// Abbreviations that survive punctuation trimming and would otherwise read as
/// `stem.ext` filenames.
const PROSE_ABBREVIATIONS: [&str; 5] = ["e.g", "i.e", "a.m", "p.m", "u.s"];

/// True when a token is plausibly a relative path or filename, not prose.
fn looks_like_filename(raw: &str) -> bool {
    if !path_like(raw) {
        return false;
    }
    if raw.contains('/') {
        return true;
    }
    // Bare filename: demand a letter-led alphanumeric extension so version
    // strings ("1.2.3") and abbreviations ("e.g.") don't qualify.
    let Some((stem, ext)) = raw.rsplit_once('.') else {
        return false;
    };
    !stem.is_empty()
        && (1..=8).contains(&ext.len())
        && ext.starts_with(|c: char| c.is_ascii_alphabetic())
        && ext.chars().all(|c| c.is_ascii_alphanumeric())
        && !PROSE_ABBREVIATIONS.contains(&raw.to_lowercase().as_str())
}

/// Resolve a parsed token to an absolute path: expand a leading `~/` against
/// `$HOME`, and anchor anything still relative to the cwd at ARM time.
///
/// Anchoring matters because an alert outlives the directory it was armed in —
/// a stored `build.done` would silently start watching a different file after
/// the next `cd`.
fn expand_home(raw: &str) -> PathBuf {
    let p = if let Some(rest) = raw.strip_prefix("~/") {
        match dirs_home() {
            Some(h) => h.join(rest),
            None => PathBuf::from(raw),
        }
    } else {
        PathBuf::from(raw)
    };
    if p.is_absolute() {
        return p;
    }
    match std::env::current_dir() {
        Ok(cwd) => cwd.join(p),
        Err(_) => p,
    }
}

/// Build a `gh pr view <n> --json state` probe expecting GitHub state `want`.
fn gh_pr_state_probe(n: &str, want: &str) -> AlertKind {
    AlertKind::Command {
        program: "gh".into(),
        args: vec![
            "pr".into(),
            "view".into(),
            n.to_string(),
            "--json".into(),
            "state".into(),
            "-q".into(),
            ".state".into(),
        ],
        expect: Some(want.into()),
    }
}

/// Pull a PR number out of lowercased free text.
///
/// `allow_bare_hash` additionally accepts `#123` with no "PR"/"pull request"
/// nearby — safe only for merge cues, since merging is PR-specific (issues
/// close, they never merge). Even then a mention of "issue" vetoes the match.
fn pr_number(lower: &str, allow_bare_hash: bool) -> Option<String> {
    if let Ok(re) = Regex::new(r"\b(?:pr|pull\s*requests?)\b\D{0,6}(\d{1,7})")
        && let Some(c) = re.captures(lower)
        && let Some(m) = c.get(1)
    {
        return Some(m.as_str().to_string());
    }
    if allow_bare_hash
        && !lower.contains("issue")
        && let Ok(re) = Regex::new(r"#(\d{1,7})")
        && let Some(c) = re.captures(lower)
        && let Some(m) = c.get(1)
    {
        return Some(m.as_str().to_string());
    }
    None
}

/// Pick the transition to watch from the operator's wording.
///
/// A directional watch that could never fire is demoted to
/// [`FileWatchMode::Any`]: asking for "appears" on a file that already exists,
/// or "deleted" on one that is already absent. The fired message always names
/// what actually happened, so the demotion can't produce a misleading alert.
fn file_watch_mode(lower: &str, exists_now: bool) -> FileWatchMode {
    let vanish = [
        "delet",
        "remov",
        "disappear",
        "vanish",
        "is gone",
        "goes away",
    ]
    .iter()
    .any(|k| lower.contains(k));
    if vanish {
        return if exists_now {
            FileWatchMode::Vanish
        } else {
            FileWatchMode::Any
        };
    }
    let appear = ["appear", "creat", "exist", "show up", "shows up", "written"]
        .iter()
        .any(|k| lower.contains(k));
    if appear {
        return if exists_now {
            FileWatchMode::Any
        } else {
            FileWatchMode::Appear
        };
    }
    FileWatchMode::Any
}

fn dirs_home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

/// Parse a free-text `:alert` condition into a concrete [`AlertKind`].
///
/// Heuristics, in order:
///   1. A PR/pull-request number plus a merge or close cue → a `gh pr view`
///      state probe ("PR 333 is merged", "#412 lands", "pull request 7 closes").
///   2. A filesystem path plus a change/watch cue → a file-change watch,
///      directional when the wording says appear or delete.
///   3. Anything else → a semantic condition for a coordinator to resolve.
///
/// Every rule that matches here is one fewer coordinator spawned to babysit a
/// question `gh` or `stat` can answer, so the probes are deliberately broad on
/// phrasing and strict on what counts as a PR number or a path.
pub fn parse_condition(condition: &str) -> AlertKind {
    let lower = condition.to_lowercase();

    // 1) PR state probe. Merge cues accept a bare "#412"; close cues demand the
    //    words "PR"/"pull request", because issues close too and a `gh pr view`
    //    on an issue number would quietly watch the wrong thing.
    let merge_cue = ["merg", "land"].iter().any(|k| lower.contains(k));
    let close_cue = lower.contains("clos");
    if merge_cue || close_cue {
        let want = if merge_cue { "MERGED" } else { "CLOSED" };
        if let Some(n) = pr_number(&lower, merge_cue) {
            return gh_pr_state_probe(&n, want);
        }
    }

    // 2) File change / existence watch.
    let file_cue = [
        "file",
        "change",
        "chang",
        "modif",
        "watch",
        "exist",
        "creat",
        "delet",
        "remov",
        "touch",
        "appear",
        "show up",
        "written",
        "disappear",
        "vanish",
    ]
    .iter()
    .any(|k| lower.contains(k));
    if file_cue && let Some(path) = first_path(condition) {
        let baseline_exists = path.exists();
        let baseline_mtime = mtime_of(&path);
        return AlertKind::FileChange {
            path,
            baseline_mtime,
            baseline_exists,
            mode: file_watch_mode(&lower, baseline_exists),
        };
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
            mode,
        } => {
            let exists = path.exists();
            let created = exists && !*baseline_exists;
            let removed = !exists && *baseline_exists;
            let changed_mtime = exists && *baseline_exists && mtime_of(path) != *baseline_mtime;
            // A directional watch ignores the transitions it wasn't armed for,
            // so "when build.done appears" stays quiet on an mtime bump.
            let what = match mode {
                FileWatchMode::Appear => created.then_some("created"),
                FileWatchMode::Vanish => removed.then_some("removed"),
                FileWatchMode::Any if created => Some("created"),
                FileWatchMode::Any if removed => Some("removed"),
                FileWatchMode::Any if changed_mtime => Some("changed"),
                FileWatchMode::Any => None,
            };
            what.map(|what| Fired {
                detail: format!(
                    "\x1b[1;33m⚠ ALERT\x1b[0m  {} — file {} {}",
                    alert.condition,
                    path.display(),
                    what
                ),
                short: format!("⚠ {} {}", short_path(path), what),
            })
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
    if matches!(
        std::env::var("AISH_ALERT_BELL").ok().as_deref(),
        Some("0") | Some("false") | Some("off")
    ) {
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
    use std::path::Path;

    #[test]
    fn parses_pr_merge_condition() {
        let k = parse_condition("let me know when PR 333 is merged");
        match k {
            AlertKind::Command {
                program,
                args,
                expect,
            } => {
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
                mode: FileWatchMode::Any,
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

    // ---- PR probe coverage -------------------------------------------------

    fn pr_probe(condition: &str) -> (String, Option<String>) {
        match parse_condition(condition) {
            AlertKind::Command { args, expect, .. } => (args.join(" "), expect),
            other => panic!("expected Command for {condition:?}, got {other:?}"),
        }
    }

    #[test]
    fn parses_bare_hash_pr_merge() {
        let (args, expect) = pr_probe("ping me when #412 lands");
        assert!(args.contains("412"), "args: {args}");
        assert_eq!(expect.as_deref(), Some("MERGED"));
    }

    #[test]
    fn parses_pr_closed_as_closed_state() {
        let (args, expect) = pr_probe("let me know when PR 7 is closed");
        assert!(args.contains('7'), "args: {args}");
        assert_eq!(expect.as_deref(), Some("CLOSED"));
    }

    #[test]
    fn merge_cue_wins_over_close_cue() {
        // "closed by a merge" is a merge question — probe for MERGED.
        let (_, expect) = pr_probe("alert when pull request 90 is closed by a merge");
        assert_eq!(expect.as_deref(), Some("MERGED"));
    }

    #[test]
    fn bare_hash_close_is_not_a_pr_probe() {
        // Issues close too; a bare "#42 is closed" must not become `gh pr view 42`.
        assert!(matches!(
            parse_condition("tell me when #42 is closed"),
            AlertKind::Semantic { .. }
        ));
    }

    #[test]
    fn pr_probe_is_json_state_query() {
        let (args, _) = pr_probe("when PR 333 is merged");
        assert!(args.starts_with("pr view 333"), "args: {args}");
        assert!(args.contains("--json state"), "args: {args}");
    }

    // ---- file probe coverage ----------------------------------------------

    fn file_probe(condition: &str) -> (PathBuf, FileWatchMode) {
        match parse_condition(condition) {
            AlertKind::FileChange { path, mode, .. } => (path, mode),
            other => panic!("expected FileChange for {condition:?}, got {other:?}"),
        }
    }

    #[test]
    fn appear_wording_arms_appear_mode() {
        let (path, mode) = file_probe("let me know when /tmp/aish-does-not-exist-xyz appears");
        assert_eq!(path, PathBuf::from("/tmp/aish-does-not-exist-xyz"));
        assert_eq!(mode, FileWatchMode::Appear);
    }

    #[test]
    fn delete_wording_arms_vanish_mode() {
        let p = std::env::temp_dir().join(format!("aish-lock-{}", now_epoch()));
        std::fs::write(&p, b"held").unwrap();
        let (path, mode) = file_probe(&format!("alert me when {} is deleted", p.display()));
        assert_eq!(path, p);
        assert_eq!(mode, FileWatchMode::Vanish);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn vanish_wording_on_absent_file_demotes_to_any() {
        // "deleted" on a file that is already gone could never fire, so the
        // parser widens it rather than arming a dead probe.
        let (_, mode) = file_probe("alert me when /tmp/aish-absent-xyz-123 is deleted");
        assert_eq!(mode, FileWatchMode::Any);
    }

    #[test]
    fn relative_path_is_resolved_against_cwd() {
        let (path, mode) = file_probe("watch for build.done to appear");
        assert!(
            path.is_absolute(),
            "expected absolute, got {}",
            path.display()
        );
        assert!(path.ends_with("build.done"), "got {}", path.display());
        assert_eq!(mode, FileWatchMode::Appear);
    }

    #[test]
    fn quoted_path_with_spaces_is_recovered() {
        let (path, _) = file_probe("watch for changes on \"/tmp/my notes.txt\"");
        assert_eq!(path, PathBuf::from("/tmp/my notes.txt"));
    }

    #[test]
    fn tilde_path_is_expanded_when_home_known() {
        if dirs_home().is_none() {
            return;
        }
        let (path, _) = file_probe("watch ~/.aish/history.db for changes");
        assert!(path.is_absolute(), "got {}", path.display());
        assert!(
            !path.starts_with("~"),
            "tilde left unexpanded: {}",
            path.display()
        );
    }

    #[test]
    fn url_is_not_mistaken_for_a_path() {
        assert!(matches!(
            parse_condition("watch https://example.com/health for changes"),
            AlertKind::Semantic { .. }
        ));
    }

    #[test]
    fn version_number_is_not_mistaken_for_a_path() {
        assert!(matches!(
            parse_condition("tell me when the deploy changes to 1.2.3"),
            AlertKind::Semantic { .. }
        ));
    }

    // ---- directional poll semantics ---------------------------------------

    fn file_alert(path: &Path, mode: FileWatchMode, baseline_exists: bool) -> Alert {
        Alert {
            id: 9,
            condition: format!("watch {}", path.display()),
            kind: AlertKind::FileChange {
                path: path.to_path_buf(),
                baseline_mtime: if baseline_exists {
                    mtime_of(path)
                } else {
                    None
                },
                baseline_exists,
                mode,
            },
            audible: true,
            last_checked: 0,
        }
    }

    #[test]
    fn appear_mode_ignores_mtime_bumps() {
        let p = std::env::temp_dir().join(format!("aish-appear-{}", now_epoch()));
        std::fs::write(&p, b"v1").unwrap();
        let alert = file_alert(&p, FileWatchMode::Appear, true);
        // File already existed at arm time, so a later write is not an appearance.
        std::fs::write(&p, b"v2-much-longer").unwrap();
        assert!(poll(&alert).is_none(), "appear mode must ignore edits");
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn appear_mode_fires_on_creation() {
        let p = std::env::temp_dir().join(format!("aish-appear2-{}", now_epoch()));
        let _ = std::fs::remove_file(&p);
        let alert = file_alert(&p, FileWatchMode::Appear, false);
        assert!(poll(&alert).is_none());
        std::fs::write(&p, b"done").unwrap();
        let fired = poll(&alert).expect("creation should fire");
        assert!(fired.short.contains("created"), "short: {}", fired.short);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn vanish_mode_fires_only_on_removal() {
        let p = std::env::temp_dir().join(format!("aish-vanish-{}", now_epoch()));
        std::fs::write(&p, b"lock").unwrap();
        let alert = file_alert(&p, FileWatchMode::Vanish, true);
        assert!(poll(&alert).is_none(), "still present → no fire");
        std::fs::remove_file(&p).unwrap();
        let fired = poll(&alert).expect("removal should fire");
        assert!(fired.short.contains("removed"), "short: {}", fired.short);
    }

    #[test]
    fn legacy_file_change_row_defaults_to_any_mode() {
        // Rows persisted before `mode` existed must still deserialize.
        let json = r#"{"FileChange":{"path":"/tmp/legacy","baseline_mtime":null,"baseline_exists":false}}"#;
        let kind: AlertKind = serde_json::from_str(json).expect("legacy row should deserialize");
        match kind {
            AlertKind::FileChange { mode, path, .. } => {
                assert_eq!(mode, FileWatchMode::Any);
                assert_eq!(path, PathBuf::from("/tmp/legacy"));
            }
            other => panic!("expected FileChange, got {other:?}"),
        }
    }
}
