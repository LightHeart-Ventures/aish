//! Host-side subprocess background workers — full-tool deferrable jobs.
//!
//! Where `batch.rs` offloads a tool-LESS task to the Anthropic Batches API, this
//! re-execs aish itself as a child process in `--coordinator` mode. The child
//! runs the full agentic tool loop (filesystem, run_program, MCP) in the SAME
//! cwd and inherits the parent's environment, so it has exactly the tools and
//! MCP servers the interactive session has. It prints its final answer to
//! stdout; we capture that as the result and surface it the same way batch
//! results land (`on_complete` → `flush_results`).
//!
//! No Docker: the child is a plain host subprocess. Isolation is the same trust
//! model as interactive aish (which already runs arbitrary commands on the
//! host). What the subprocess buys over an in-process task is an INDEPENDENT
//! `Session` — its own history, cwd, and MCP connections — so a background job
//! can't corrupt the live session's state.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::process::Command;
use uuid::Uuid;

/// How long a worker may run before it's killed. Generous — these are deferred,
/// possibly-long jobs — but bounded so a wedged child can't live forever.
const WORKER_TIMEOUT: Duration = Duration::from_secs(60 * 60); // 1h

/// Max bytes of a child's stdout/stderr we keep. A runaway coordinator that
/// dumps gigabytes must never OOM the PARENT (the interactive aish / goal loop)
/// via an unbounded read — so we cap the capture and drain the rest.
const CAPTURE_CAP: usize = 1024 * 1024; // 1 MB

/// Read up to `cap` bytes of `r` into a String, then keep draining (so the
/// child never blocks on a full pipe) but discard the overflow. A truncation
/// marker is appended if it overflowed.
async fn read_capped<R: tokio::io::AsyncRead + Unpin>(mut r: R, cap: usize) -> String {
    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 16 * 1024];
    let mut overflowed = false;
    loop {
        match r.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if buf.len() < cap {
                    let take = n.min(cap - buf.len());
                    buf.extend_from_slice(&chunk[..take]);
                    overflowed |= take < n;
                } else {
                    overflowed = true; // past the cap — keep draining, drop the bytes
                }
            }
        }
    }
    let mut s = String::from_utf8_lossy(&buf).into_owned();
    if overflowed {
        s.push_str("\n…[output truncated — exceeded the capture cap]");
    }
    s
}

// ---------------------------------------------------------------------------
// Prompt-badge pulse — colour the ⟳N indicator by recent background activity
// ---------------------------------------------------------------------------

/// How long a background-worker event keeps the prompt's `⟳N` badge pulsing
/// (coloured glyph) before it fades back to the idle dim `⟳N`. Short enough to
/// read as a transient "pulse", long enough to be seen at the next prompt draw.
pub const PULSE_FADE: Duration = Duration::from_millis(900);

/// A single prompt-badge pulse event, derived from a coordinator's stderr.
/// Most-recent-wins across all live workers (see [`fresh_pulse`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Pulse {
    /// A tool call finished successfully — pulse green ✓.
    ToolOk,
    /// A tool call failed — pulse red ✗.
    ToolErr,
    /// The model emitted a turn/narration line — pulse magenta ⟳.
    Turn,
}

/// Classify ONE raw coordinator-stderr line into a prompt-badge pulse event, or
/// `None` when it carries no event. Pure, so it's unit-testable without a pipe.
///
/// The coordinator runs non-TTY, so its post-execution tool line is the static
/// `✓/✗ 🔧 <desc>` shape (see `engine::tool_result_line`): a `✓` glyph alongside
/// the wrench is a success, a `✗` a failure. A bare `🔧 <desc>` START line (no
/// status glyph) is the tool *beginning*, not an outcome → `None`. A `🗨` line is
/// turn narration (`engine::emit_narration`) → a turn-completion pulse.
fn classify_event(line: &str) -> Option<Pulse> {
    if line.contains('🔧') || line.contains('⚙') {
        if line.contains('✓') {
            return Some(Pulse::ToolOk);
        }
        if line.contains('✗') {
            return Some(Pulse::ToolErr);
        }
        return None; // a bare start line — no outcome yet
    }
    if line.trim_start().starts_with('🗨') {
        return Some(Pulse::Turn);
    }
    None
}

/// How many of the child's most-recent stderr lines we retain for the failure
/// message. We stream stderr live rather than accumulating it (an unbounded
/// accumulation was the OOM risk), so the failure path can only quote a bounded
/// tail rather than the whole thing.
const STDERR_TAIL_LINES: usize = 20;

/// Decide whether a single raw coordinator-stderr line is worth forwarding to
/// the user's terminal, and if so return the cleaned text to forward.
///
/// The coordinator runs non-TTY, so a tool emits TWO static lines per call (see
/// `engine::ToolSpinner`): a bare `🔧 <desc>` START line at `start`, then a
/// `✓/✗ 🔧 <desc>` RESULT line at `finish` (the latter is what `classify_event`
/// reads to drive the prompt-badge pulse). Forwarding BOTH made every tool call
/// appear TWICE in the `:worker-output` stream — the duplicate tool-call logging
/// bug. We now forward ONLY the RESULT line (the one carrying the `✓/✗` outcome)
/// so each tool call is logged exactly once; the bare START line is dropped here
/// (the badge pulse still fires from the RESULT line in `stream_stderr`,
/// independent of this gate). The "coordinator run … starting" banner and blank
/// lines carry no wrench and are dropped too. Cleaning strips leading whitespace
/// and the outer dim `\x1b[2m…\x1b[0m` wrapper, so `announce` (which re-wraps in
/// dim) doesn't double-wrap.
fn clean_activity_line(raw: &str) -> Option<String> {
    if !raw.contains('🔧') && !raw.contains('⚙') {
        return None;
    }
    // Forward only the RESULT line (it carries the ✓/✗ outcome). The bare START
    // line — a wrench with no status glyph — is the tool *beginning*; forwarding
    // it as well is exactly the duplicate. Drop it.
    if !raw.contains('✓') && !raw.contains('✗') {
        return None;
    }
    let mut s = raw.trim();
    // Strip the dim wrapper the coordinator emits around static tool lines.
    s = s.strip_prefix("\x1b[2m").unwrap_or(s);
    s = s.strip_suffix("\x1b[0m").unwrap_or(s);
    let cleaned = s.trim();
    if cleaned.is_empty() {
        return None;
    }
    Some(cleaned.to_string())
}

/// A coordinator stderr line carrying turn text (the `🗨` sentinel emitted by
/// `engine::emit_narration`) or a batch-phase notice (`📦` from the coordinator
/// loop). Returns the cleaned text after the sentinel, or `None`.
fn strip_sentinel(raw: &str, mark: &str) -> Option<String> {
    let rest = raw.trim_start().strip_prefix(mark)?.trim_start();
    (!rest.is_empty()).then(|| rest.to_string())
}

/// Decide what (if anything) to forward to the user's terminal for ONE raw
/// coordinator-stderr line, given whether `:worker-output` is on. Returns
/// `(label_suffix, text)` to announce as `[label<suffix>] text`, or `None` to
/// drop the line. Pure — the single source of truth for the suppression gate,
/// so it's unit-testable without a live pipe.
///
/// Suppression policy (the default): a background coordinator is QUIET. With
/// `show_output` off NOTHING from its stderr is forwarded — not its `🔧`
/// tool-activity, not its turn narration. The user still sees the job is alive
/// via the prompt's `⟳N` pulse and its completion notice (both independent of
/// this stream); they just don't get the firehose of every tool call. Flipping
/// `:worker-output on` opens the full live stream:
/// - `🔧` tool activity (the `✓/✗ 🔧` RESULT line, once per call) → `[label] …`
/// - `🗨` turn text (a standard model call) → `[label·standard] …`
/// - `📦` batch fan-out notice → `[label·batch] …`
fn forward_decision(line: &str, show_output: bool) -> Option<(&'static str, String)> {
    if !show_output {
        // Default: keep background coordinators quiet. The job's liveness is
        // shown by the ⟳N prompt pulse + completion notice, not this stream.
        return None;
    }
    if let Some(activity) = clean_activity_line(line) {
        return Some(("", activity));
    }
    if let Some(text) = strip_sentinel(line, "🗨") {
        return Some(("·standard", text));
    }
    if let Some(text) = strip_sentinel(line, "📦") {
        return Some(("·batch", text));
    }
    None
}

/// Stream a child's stderr line by line, forwarding the interesting lines to the
/// user's terminal live via `announce`, and retaining only the last
/// `STDERR_TAIL_LINES` raw lines as a bounded ring for the failure message.
/// Returns the retained tail joined with newlines (oldest-first).
///
/// Forwarding is decided per line by [`forward_decision`], which gates ALL
/// coordinator output (tool `🔧` lines included) behind the `:worker-output`
/// toggle (`show_output`). Default (off) → a quiet background job; on → the full
/// live `🔧`/`🗨`/`📦` stream. The toggle is read PER LINE, so flipping it
/// mid-run takes effect on the next line.
///
/// The bounded tail is retained for EVERY line regardless of forwarding, so a
/// failure message can quote recent stderr even when output is suppressed. This
/// keeps the child's stderr pipe drained (so it never blocks) without
/// accumulating all of stderr in memory.
async fn stream_stderr<R: tokio::io::AsyncRead + Unpin>(
    r: R,
    label: &str,
    show_output: Arc<AtomicBool>,
    pulse: Option<Arc<WorkerJob>>,
) -> String {
    let mut lines = BufReader::new(r).lines();
    let mut tail: VecDeque<String> = VecDeque::with_capacity(STDERR_TAIL_LINES);
    while let Ok(Some(line)) = lines.next_line().await {
        // Drive the prompt-badge pulse from EVERY line (independent of the
        // `:worker-output` forwarding gate) so the badge colour-pulses even when
        // the verbose stream is suppressed — the badge is the quiet liveness cue.
        if let Some(job) = &pulse {
            match classify_event(&line) {
                Some(Pulse::ToolOk) => job.record_tool_outcome(true),
                Some(Pulse::ToolErr) => job.record_tool_outcome(false),
                Some(Pulse::Turn) => job.record_turn_completion(),
                None => {}
            }
        }
        if let Some((suffix, text)) = forward_decision(&line, show_output.load(Ordering::Relaxed)) {
            crate::tools::announce(&format!("[{label}{suffix}]"), &text);
        }
        if tail.len() == STDERR_TAIL_LINES {
            tail.pop_front();
        }
        tail.push_back(line);
    }
    tail.into_iter().collect::<Vec<_>>().join("\n")
}

/// Default address-space / data cap for a worker child, in MB. Generous enough
/// for a real agentic task but bounded so a runaway can't exhaust host memory.
/// Override with `AISH_WORKER_MEM_MB`.
const DEFAULT_WORKER_MEM_MB: u64 = 4096;

/// Default CPU-time cap for a worker child, in seconds. A backstop against a
/// runaway busy-loop that the wall-clock timeout might not catch promptly.
/// Override with `AISH_WORKER_CPU_SECS`.
const DEFAULT_WORKER_CPU_SECS: u64 = 3600;

/// Parse a `u64` from `var`, falling back to `default` if unset, empty, or
/// unparseable. A value of `0` is treated as "unset / no limit" by the caller.
fn env_u64(var: &str, default: u64) -> u64 {
    parse_u64_or(std::env::var(var).ok().as_deref(), default)
}

/// Pure parsing core of `env_u64`, split out so it's testable without mutating
/// process-wide env (which is `unsafe` and racy under the test harness's
/// threads). `None`/empty/unparseable → `default`; otherwise the parsed value
/// (including a legitimate `0`, which callers read as "no limit").
fn parse_u64_or(raw: Option<&str>, default: u64) -> u64 {
    raw.and_then(|v| v.trim().parse::<u64>().ok()).unwrap_or(default)
}

/// Build the `tokio::process::Command` that re-execs aish in `--coordinator`
/// mode for a single task. Centralises the args, env, pipes, AND the resource
/// limits applied to the child, so `run_worker` and `run_once` stay in sync.
///
/// Resource limits are applied via a `pre_exec` hook (see `apply_rlimits`):
/// they run in the forked child between fork and exec.
fn worker_command(spec: &WorkerSpec, task: &str, run_id: &str, cwd: &std::path::Path) -> Command {
    let mut cmd = Command::new(&spec.exe);
    cmd.arg("-c")
        .arg(task)
        .arg("--coordinator")
        .arg("--run-id")
        .arg(run_id)
        // Full parity: the coordinator runs on the SAME backend the interactive
        // session uses (claude/grok), not a hardcoded one. The child inherits the
        // relevant credential (XAI_API_KEY / ANTHROPIC_API_KEY) via the env it's
        // spawned with — see the note on WorkerSpec.env.
        .arg("--backend")
        .arg(&spec.backend)
        .arg("--model")
        .arg(&spec.model)
        // The effective run directory: `spec.cwd` normally, or the isolated
        // worktree path when isolation is on.
        .current_dir(cwd)
        // Nested-coordinator guard: an in-container/in-worker aish must never
        // spawn its own workers (no infinite recursion). The child reads this.
        .env("AISH_COORDINATOR", "1")
        // Tie the work to the LAUNCHING session: the child adopts this id so its
        // durable records attribute to the session that asked for the work.
        .env("AISH_LAUNCH_SESSION_ID", &spec.launch_session_id)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(name) = &spec.launch_session_name {
        cmd.env("AISH_LAUNCH_SESSION_NAME", name);
    }
    for (k, v) in &spec.env {
        cmd.env(k, v);
    }

    // Read the knobs in the PARENT (env::var allocates — not allowed in the
    // post-fork child), then move plain integers into the pre_exec closure.
    let mem_mb = env_u64("AISH_WORKER_MEM_MB", DEFAULT_WORKER_MEM_MB);
    let cpu_secs = env_u64("AISH_WORKER_CPU_SECS", DEFAULT_WORKER_CPU_SECS);

    // SAFETY: `pre_exec` runs in the forked child before `exec`, where only
    // async-signal-safe calls are permitted. `apply_rlimits` performs only
    // `setrlimit` syscalls and integer arithmetic — no allocation, no locks,
    // no panics — so it is safe here. Failures are swallowed (best-effort): a
    // worker that couldn't be capped still runs rather than failing to spawn.
    // `tokio::process::Command::pre_exec` is an inherent method (it mirrors
    // `std::os::unix::process::CommandExt::pre_exec`); no extra trait import.
    unsafe {
        cmd.pre_exec(move || {
            apply_rlimits(mem_mb, cpu_secs);
            Ok(())
        });
    }
    cmd
}

/// Apply memory and CPU resource limits to the CURRENT process via
/// `setrlimit`. Intended to run inside a `pre_exec` hook (post-fork, pre-exec),
/// so it must stay async-signal-safe: only `setrlimit` syscalls and integer
/// math, no allocation, no logging, no panics. Every call is best-effort —
/// a failed `setrlimit` is silently ignored so the child still execs.
///
/// macOS caveat: on Linux `RLIMIT_AS` is a hard ceiling on the process's
/// virtual address space, so a memory runaway hits `ENOMEM`/abort well before
/// the kernel OOM-killer or macOS Jetsam steps in. On macOS the relationship
/// between `RLIMIT_AS`/`RLIMIT_DATA` and Jetsam's memory-pressure killer is
/// looser — a process can still be SIGKILLed by Jetsam under system pressure
/// regardless of these limits, and these caps don't perfectly track physical
/// footprint. This is harm-reduction (it bounds the worst single-process
/// runaways and is a real cap), NOT a guarantee against signal-9 on macOS.
fn apply_rlimits(mem_mb: u64, cpu_secs: u64) {
    // 0 == "no limit" for either knob.
    if mem_mb > 0 {
        // Saturate the byte count so a huge MB value can't wrap around.
        let bytes = mem_mb.saturating_mul(1024 * 1024);
        let lim = libc::rlimit {
            rlim_cur: bytes as libc::rlim_t,
            rlim_max: bytes as libc::rlim_t,
        };
        // Cap address space (virtual memory). Best-effort.
        unsafe {
            libc::setrlimit(libc::RLIMIT_AS, &lim);
        }
        // Also cap the data segment as a second line of defence; on some
        // platforms RLIMIT_DATA bites where RLIMIT_AS doesn't.
        unsafe {
            libc::setrlimit(libc::RLIMIT_DATA, &lim);
        }
    }
    if cpu_secs > 0 {
        let lim = libc::rlimit {
            rlim_cur: cpu_secs as libc::rlim_t,
            rlim_max: cpu_secs as libc::rlim_t,
        };
        // Cap CPU seconds — a runaway loop gets SIGXCPU then SIGKILL.
        unsafe {
            libc::setrlimit(libc::RLIMIT_CPU, &lim);
        }
    }
}

// ---------------------------------------------------------------------------
// Worktree isolation — give a writing/building coordinator its own git worktree
// so parallel coordinators can't clobber each other's tree (the headline bug).
// ---------------------------------------------------------------------------

/// A dedicated git worktree carved off `src` for one worker, on a fresh branch.
/// `path` is where the coordinator runs; `branch` is reported on completion so
/// the parent can review/merge changes (we never auto-merge).
struct Worktree {
    path: PathBuf,
    branch: String,
    /// The source repo the worktree was carved from — used to remove it cleanly.
    src: PathBuf,
    /// Commit sha the worktree branched from. Cleanup compares the worktree's tip
    /// to THIS (not the source's live HEAD): branching off `origin/main` means the
    /// base may differ from the source checkout, so an unchanged worktree's tip
    /// equals `base_sha`, not `git_head(src)`.
    base_sha: String,
}

/// True when `dir` is inside a git working tree. Cheap `git rev-parse` probe;
/// false on any error (not a repo, git missing, …).
pub fn is_git_repo(dir: &std::path::Path) -> bool {
    std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["rev-parse", "--is-inside-work-tree"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Parse a GitHub remote URL into a stable, filesystem-safe `owner--repo` key,
/// or `None` when the URL isn't a parseable GitHub remote. Pure — unit-tested.
/// Handles `https://github.com/owner/repo(.git)`, `git@github.com:owner/repo.git`,
/// and `ssh://git@github.com/owner/repo.git`; strips a trailing `.git`/slash and
/// sanitises the `/` separator to `--` (see `sanitize_repo_key`).
fn repo_key_from_remote(url: &str) -> Option<String> {
    let url = url.trim();
    let rest = url
        .strip_prefix("git@github.com:")
        .or_else(|| url.strip_prefix("ssh://git@github.com/"))
        .or_else(|| url.strip_prefix("https://github.com/"))
        .or_else(|| url.strip_prefix("http://github.com/"))?;
    let rest = rest.strip_suffix('/').unwrap_or(rest);
    let rest = rest.strip_suffix(".git").unwrap_or(rest);
    let mut parts = rest.splitn(2, '/');
    let owner = parts.next().filter(|s| !s.is_empty())?;
    let repo = parts.next().filter(|s| !s.is_empty())?;
    Some(sanitize_repo_key(&format!("{owner}/{repo}")))
}

/// Map an `owner/repo` string to a filesystem- and branch-safe key: `/` → `--`,
/// and any other char outside `[A-Za-z0-9._-]` → `-`. Pure.
fn sanitize_repo_key(s: &str) -> String {
    s.replace('/', "--")
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') { c } else { '-' })
        .collect()
}

/// Fallback repo-key for a local-only / non-GitHub repo: the source dir's
/// basename plus a short FNV-1a hash of its absolute path, preserving
/// collision-safety across two checkouts of the same-named repo. Pure given `src`.
fn fallback_repo_key(src: &std::path::Path) -> String {
    let base = src
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "repo".to_string());
    let mut hash: u64 = 0xcbf29ce484222325;
    for b in src.to_string_lossy().as_bytes() {
        hash ^= *b as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{}-{:08x}", sanitize_repo_key(&base), (hash & 0xffff_ffff) as u32)
}

/// Resolve the `{repo-key}` for `src` (IO — reads the git `origin` remote):
/// `owner--repo` from the GitHub remote when parseable, else the
/// basename+shorthash fallback. See `repo_key_from_remote` / `fallback_repo_key`.
fn repo_key(src: &std::path::Path) -> String {
    git_out(src, &["remote", "get-url", "origin"])
        .and_then(|url| repo_key_from_remote(&url))
        .unwrap_or_else(|| fallback_repo_key(src))
}

/// The root under which all worker worktrees live: `$AISH_WORKTREE_DIR` when set
/// and non-empty, else `~/.atum/worktrees`, else (no `$HOME`) a temp fallback.
/// Moving off the OS temp dir is the whole point of ISS-2046 — the OS reaps temp
/// dirs and could silently delete a worker's deliberately-kept (dirty) worktree.
fn worktree_root() -> PathBuf {
    if let Some(dir) = std::env::var_os("AISH_WORKTREE_DIR") {
        if !dir.is_empty() {
            return PathBuf::from(dir);
        }
    }
    let home = std::env::var("HOME").unwrap_or_default();
    if !home.is_empty() {
        return PathBuf::from(home).join(".atum").join("worktrees");
    }
    std::env::temp_dir().join("aish-worktrees")
}

/// Create `dir` (and parents) and tighten it to `0700` — these worktrees can
/// hold un-pushed work, so they're owner-only. Best-effort.
fn ensure_dir_0700(dir: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    if std::fs::create_dir_all(dir).is_ok() {
        let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
    }
}

/// Build the branch name + worktree path for a worker. Pure (the caller supplies
/// the already-resolved `root` and `repo_key`), so it's unit-testable. The
/// worktree lives at `{root}/{repo_key}/{id}` — OUTSIDE the source repo, so it
/// never pollutes the source `git status` — and the branch is `aish/{id}`. The
/// worker id is globally unique (`w_########`, #86), so no session prefix is
/// needed to disambiguate two sessions / two checkouts: they harmlessly share the
/// `{repo_key}` parent dir with distinct leaves.
fn worktree_layout(root: &std::path::Path, repo_key: &str, id: &str) -> (String, PathBuf) {
    let branch = format!("aish/{id}");
    let path = root.join(repo_key).join(id);
    (branch, path)
}

/// Run a git command in `src`, returning trimmed stdout on success.
fn git_out(src: &std::path::Path, args: &[&str]) -> Option<String> {
    let o = std::process::Command::new("git").arg("-C").arg(src).args(args).output().ok()?;
    o.status
        .success()
        .then(|| String::from_utf8_lossy(&o.stdout).trim().to_string())
}

/// True when a git command in `src` exits 0 (output discarded).
fn git_ok(src: &std::path::Path, args: &[&str]) -> bool {
    std::process::Command::new("git")
        .arg("-C")
        .arg(src)
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// The repo's trunk branch name — `origin/HEAD` when set (e.g. `main`/`master`),
/// else whichever of `main`/`master` exists locally or on the remote, else `main`.
fn trunk_branch(src: &std::path::Path) -> String {
    if let Some(s) = git_out(src, &["symbolic-ref", "--short", "refs/remotes/origin/HEAD"]) {
        if let Some(name) = s.strip_prefix("origin/") {
            if !name.is_empty() {
                return name.to_string();
            }
        }
    }
    for cand in ["main", "master"] {
        if git_ok(src, &["rev-parse", "--verify", "--quiet", &format!("refs/heads/{cand}")])
            || git_ok(src, &["rev-parse", "--verify", "--quiet", &format!("refs/remotes/origin/{cand}")])
        {
            return cand.to_string();
        }
    }
    "main".to_string()
}

/// Resolve the start-point ref an isolated worker should branch from.
/// `"head"` → the session's current checkout (continue-my-work). Anything else →
/// a clean trunk baseline: `origin/<trunk>` after a best-effort fetch when a
/// remote exists (so workers never inherit a stale local trunk — the exact
/// footgun behind branch sprawl), else the local trunk, else `HEAD`.
fn resolve_base_ref(src: &std::path::Path, base: &str) -> String {
    if base.eq_ignore_ascii_case("head") {
        return "HEAD".to_string();
    }
    let trunk = trunk_branch(src);
    if git_ok(src, &["remote", "get-url", "origin"]) {
        // Refresh so the baseline is genuinely current; ignore failure (offline).
        let _ = git_ok(src, &["fetch", "origin", &trunk]);
        let remote_ref = format!("origin/{trunk}");
        if git_ok(src, &["rev-parse", "--verify", "--quiet", &remote_ref]) {
            return remote_ref;
        }
    }
    if git_ok(src, &["rev-parse", "--verify", "--quiet", &format!("refs/heads/{trunk}")]) {
        return trunk;
    }
    "HEAD".to_string()
}

/// Create a fresh worktree for worker `id`, branched from `base` (`"main"` for a
/// clean trunk baseline, `"head"` to continue the current checkout — see
/// `resolve_base_ref`). Best-effort: returns `None` (caller falls back to the
/// shared `src` cwd) if `src` isn't a repo or `git worktree add` fails, so
/// isolation never blocks a job.
fn create_worktree(src: &std::path::Path, id: &str, base: &str) -> Option<Worktree> {
    if !is_git_repo(src) {
        return None;
    }
    let root = worktree_root();
    let key = repo_key(src);
    let (branch, path) = worktree_layout(&root, &key, id);
    // Off the OS temp dir now (ISS-2046), so aish owns cleanup — create the root
    // and the per-repo parent `0700` before `git worktree add` materialises the leaf.
    ensure_dir_0700(&root);
    if let Some(parent) = path.parent() {
        ensure_dir_0700(parent);
    }
    let start_point = resolve_base_ref(src, base);
    // A stale dir from a crashed prior run would make `git worktree add` fail;
    // best-effort clear it first (only an empty/leftover one is expected here).
    let _ = std::process::Command::new("git")
        .arg("-C")
        .arg(src)
        .args(["worktree", "remove", "--force"])
        .arg(&path)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    let ok = std::process::Command::new("git")
        .arg("-C")
        .arg(src)
        .args(["worktree", "add", "-b", &branch])
        .arg(&path)
        .arg(&start_point)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !ok {
        return None;
    }
    // Pin the base commit for clean-up accounting (tip == base_sha ⇒ no commits).
    let base_sha = git_head(&path).unwrap_or_default();
    Some(Worktree { path, branch, src: src.to_path_buf(), base_sha })
}

/// True when the worktree has neither uncommitted changes nor commits ahead of
/// where it branched (HEAD of `src` at create time). Such a worktree is "no
/// work was done" and can be torn down. Any git error is treated as "has
/// changes" (conservative — never delete work we can't account for).
fn worktree_is_clean(wt: &Worktree) -> bool {
    // Uncommitted/untracked changes?
    let porcelain = std::process::Command::new("git")
        .arg("-C")
        .arg(&wt.path)
        .args(["status", "--porcelain"])
        .output();
    let dirty = match porcelain {
        Ok(o) if o.status.success() => !o.stdout.is_empty(),
        _ => return false, // can't tell → assume dirty, keep it
    };
    if dirty {
        return false;
    }
    // Any commits added? The worktree branched from `base_sha`; if its tip still
    // equals that, no commits were made → clean. (Compared against the recorded
    // base, not the source's live HEAD, since the base may be origin/<trunk>.)
    match git_head(&wt.path) {
        Some(tip) => !wt.base_sha.is_empty() && tip == wt.base_sha,
        None => false, // can't compare → assume work was done, keep it
    }
}

/// The current HEAD commit sha of a repo/worktree, or `None` on error.
fn git_head(dir: &std::path::Path) -> Option<String> {
    let o = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()?;
    if o.status.success() {
        Some(String::from_utf8_lossy(&o.stdout).trim().to_string())
    } else {
        None
    }
}

/// Best-effort startup cleanup: drop git's record of worktrees whose dirs are
/// already gone (a crashed isolated worker can leave a dangling registration).
/// A no-op outside a repo. `git worktree prune` only removes missing entries —
/// it never touches a live worktree, so this is always safe to call.
pub fn prune_worktrees(dir: &std::path::Path) {
    if !is_git_repo(dir) {
        return;
    }
    let _ = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["worktree", "prune"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

/// How long a CLEAN, orphaned-or-stale worktree may sit under the managed root
/// before the startup sweeper reclaims it. A dirty / commits-ahead worktree is
/// NEVER swept regardless of age. Override with `AISH_WORKTREE_MAX_AGE_DAYS`.
const DEFAULT_WORKTREE_MAX_AGE_DAYS: u64 = 7;

/// Pure sweep policy: should a discovered worktree leaf be reclaimed? NEVER when
/// it has uncommitted changes (`dirty`) or commits ahead of its base (`ahead`) —
/// the same conservative rule as `worktree_is_clean`. A clean, not-ahead leaf is
/// swept when its git registration is gone (`orphaned`) OR it is at least
/// `max_age_days` old. Unit-tested in isolation from any IO.
fn should_sweep(dirty: bool, ahead: bool, orphaned: bool, age_days: u64, max_age_days: u64) -> bool {
    if dirty || ahead {
        return false; // never delete work left for the operator
    }
    orphaned || age_days >= max_age_days
}

/// Whether the worktree at `leaf` has commits on its branch ahead of the repo
/// trunk it can see. Conservative: any git error → `true` (treat as ahead → keep).
fn worktree_commits_ahead(leaf: &std::path::Path) -> bool {
    let trunk = trunk_branch(leaf);
    // Prefer the remote trunk ref (what isolated workers branch from), falling
    // back to the local trunk; if neither resolves we can't compare → keep.
    let base = if git_ok(leaf, &["rev-parse", "--verify", "--quiet", &format!("refs/remotes/origin/{trunk}")]) {
        format!("origin/{trunk}")
    } else if git_ok(leaf, &["rev-parse", "--verify", "--quiet", &format!("refs/heads/{trunk}")]) {
        trunk
    } else {
        return true;
    };
    match git_out(leaf, &["rev-list", "--count", &format!("{base}..HEAD")]) {
        Some(n) => n.trim() != "0",
        None => true,
    }
}

/// Age of `path` in whole days from its mtime, or 0 when unknown (a metadata
/// error makes the leaf look "fresh", so the age rule alone won't reclaim it).
fn dir_age_days(path: &std::path::Path) -> u64 {
    let modified = match std::fs::metadata(path).and_then(|m| m.modified()) {
        Ok(t) => t,
        Err(_) => return 0,
    };
    match std::time::SystemTime::now().duration_since(modified) {
        Ok(d) => d.as_secs() / 86_400,
        Err(_) => 0,
    }
}

/// The set of absolute worktree paths git currently tracks for `src` (parsed from
/// `git worktree list --porcelain`). A leaf NOT in this set is orphaned
/// (registration gone). Empty on any error.
fn registered_worktrees(src: &std::path::Path) -> std::collections::HashSet<PathBuf> {
    let mut set = std::collections::HashSet::new();
    if let Some(out) = git_out(src, &["worktree", "list", "--porcelain"]) {
        for line in out.lines() {
            if let Some(p) = line.strip_prefix("worktree ") {
                let path = PathBuf::from(p.trim());
                set.insert(path.canonicalize().unwrap_or(path));
            }
        }
    }
    set
}

/// Best-effort startup sweeper: reclaim orphaned/old CLEAN worktrees under the
/// managed root so they don't accumulate now that the OS no longer GCs them
/// (ISS-2046). Scoped to the current repo's `{repo-key}` subtree — each repo's
/// startup cleans its own. The conservative `should_sweep` rule guarantees a
/// dirty or commits-ahead worktree (work left for the operator) is NEVER removed.
pub fn sweep_worktrees(src: &std::path::Path) {
    if !is_git_repo(src) {
        return;
    }
    let max_age = env_u64("AISH_WORKTREE_MAX_AGE_DAYS", DEFAULT_WORKTREE_MAX_AGE_DAYS);
    let base_dir = worktree_root().join(repo_key(src));
    let entries = match std::fs::read_dir(&base_dir) {
        Ok(e) => e,
        Err(_) => return, // nothing created under this repo-key yet
    };
    let registered = registered_worktrees(src);
    for entry in entries.flatten() {
        let leaf = entry.path();
        if !leaf.is_dir() {
            continue;
        }
        let canon = leaf.canonicalize().unwrap_or_else(|_| leaf.clone());
        let orphaned = !registered.contains(&canon);
        // Probe cleanliness IN the leaf. A git error → can't confirm clean → treat
        // as dirty (keep): never delete work we can't account for.
        let dirty = match git_out(&leaf, &["status", "--porcelain"]) {
            Some(s) => !s.trim().is_empty(),
            None => true,
        };
        let ahead = dirty || worktree_commits_ahead(&leaf);
        let age = dir_age_days(&leaf);
        if !should_sweep(dirty, ahead, orphaned, age, max_age) {
            continue;
        }
        // Reclaim: drop git's registration (if any) + the `aish/<id>` branch, then
        // the dir. `git worktree remove` deletes a REGISTERED leaf's dir; an orphan
        // it won't touch, so remove that ourselves.
        let _ = std::process::Command::new("git")
            .arg("-C")
            .arg(src)
            .args(["worktree", "remove", "--force"])
            .arg(&leaf)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        if let Some(name) = leaf.file_name().and_then(|s| s.to_str()) {
            let _ = std::process::Command::new("git")
                .arg("-C")
                .arg(src)
                .args(["branch", "-D", &format!("aish/{name}")])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
        let _ = std::fs::remove_dir_all(&leaf);
    }
}

/// Remove a worktree and delete its branch — used when the worker made no
/// changes, so nothing is left behind. Best-effort.
fn remove_worktree(wt: &Worktree) {
    let _ = std::process::Command::new("git")
        .arg("-C")
        .arg(&wt.src)
        .args(["worktree", "remove", "--force"])
        .arg(&wt.path)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    let _ = std::process::Command::new("git")
        .arg("-C")
        .arg(&wt.src)
        .args(["branch", "-D", &wt.branch])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

/// Turn a finished child's `ExitStatus` into a human failure note. SIGKILL
/// (signal 9) on these workers is overwhelmingly the OS memory-pressure killer
/// (macOS Jetsam / Linux OOM), so we name that explicitly rather than emit the
/// useless "signal: 9 (SIGKILL)".
fn describe_failure(status: std::process::ExitStatus, role: &str, stderr: &str) -> String {
    use std::os::unix::process::ExitStatusExt;
    if status.signal() == Some(libc::SIGKILL) {
        return format!(
            "{role} was killed by the OS (signal 9) — most likely out of memory; \
             the task may have been too large, or raise AISH_WORKER_MEM_MB. {}",
            stderr.trim()
        )
        .trim_end()
        .to_string();
    }
    format!("{role} exited unsuccessfully ({status}): {}", stderr.trim())
}

/// Pick the coordinator model for a given backend kind. Claude coordinators keep
/// the session's batch model (`batch_model`, Opus by default — deferred work gets
/// the strongest model). Grok has no Batches API and no model tiers worth
/// distinguishing here, so its coordinators run on the Grok default. Anything
/// else falls back to `batch_model`.
pub fn coordinator_model(backend_kind: &str, batch_model: &str) -> String {
    match backend_kind {
        "grok" => crate::backend::grok::DEFAULT_MODEL.to_string(),
        _ => batch_model.to_string(),
    }
}

/// A background worker subprocess, tracked for the life of the session. Shared
/// between the REPL (which lists/surfaces it) and the run task (which mutates it).
pub struct WorkerJob {
    /// Session-local handle, e.g. "w_a7k3m2pQ" (short `w_########` form; legacy
    /// `worker_<uuid>` ids still display and match fine — it's an opaque string).
    pub id: String,
    pub task: String,
    inner: Mutex<JobInner>,
}

struct JobInner {
    /// "running" | "done" | "failed".
    status: String,
    result: Option<String>,
    error: Option<String>,
    /// Whether this job's result was already surfaced, so the flush doesn't
    /// print it twice.
    displayed: bool,
    /// The git branch this isolated worker left its changes on, if it kept a
    /// worktree (it made changes). Surfaced in the completion notice so the
    /// parent knows where to review/merge. `None` for shared-cwd or no-change runs.
    branch: Option<String>,
    /// Most recent tool-call outcome parsed from this worker's stderr, for the
    /// prompt-badge pulse: `(is_success, when)`. `None` until the first tool
    /// finishes. Read by [`WorkerJob::latest_pulse`] and faded after [`PULSE_FADE`].
    last_tool_outcome: Option<(bool, Instant)>,
    /// When the worker most recently emitted turn/narration text, for the
    /// magenta turn pulse. `None` until the first narration line.
    last_turn_completion: Option<Instant>,
}

pub type WorkerJobs = Arc<Mutex<Vec<Arc<WorkerJob>>>>;

/// Everything the spawned run task needs, captured up front so it's
/// self-contained (mirrors how `batch::spawn` captures api_key/model).
pub struct WorkerSpec {
    /// The aish binary to re-exec (this process's own executable).
    pub exe: PathBuf,
    /// Which backend the coordinator child runs on (`"claude"`/`"grok"`). Set
    /// from the active session's `backend_kind` so background work runs on the
    /// same provider as the interactive session (full parity). Threaded into the
    /// child's `--backend` arg by `worker_command`.
    pub backend: String,
    /// Working directory for the child — the session's cwd, so it sees the same
    /// project files and the same project `.mcp.json`. When `isolate` is set this
    /// is the SOURCE repo; the child actually runs in a dedicated worktree carved
    /// off it (see `run_worker`), not in `cwd` itself.
    pub cwd: PathBuf,
    /// Model the child's coordinator turn runs on (Opus by default, like batches).
    pub model: String,
    /// Extra env for the child (the session's `~/.aishrc` exports), so MCP
    /// `${VAR}` interpolation resolves the same as it does here. The child also
    /// inherits the parent's process env (ANTHROPIC_API_KEY, ATUM_*, …).
    pub env: Vec<(String, String)>,
    /// When true and `cwd` is a git repo, run the coordinator in a dedicated
    /// `git worktree` (fresh branch off HEAD) instead of sharing `cwd`, so
    /// parallel coordinators that write/build can't clobber each other's tree.
    /// A no-change worktree is removed on completion; one with changes is left
    /// intact and its branch is surfaced in the result. Set by the model via the
    /// `run_in_background` tool's `isolate` flag (smart-defaulted to true inside a
    /// repo). The goal loop and `:dispatch` leave this false (shared cwd).
    pub isolate: bool,
    /// The git ref an isolated worker branches its worktree from. `"main"`
    /// (the default) means a CLEAN trunk baseline — `origin/<trunk>` after a
    /// best-effort fetch when a remote exists, else the local trunk — so a job
    /// never inherits a stale or unrelated local checkout. `"head"` pins to the
    /// session's current `HEAD` for "continue what I'm working on" tasks. Only
    /// consulted when `isolate` is true.
    pub base: String,
    /// The LAUNCHING session's id — the interactive session that spawned this
    /// coordinator. The child adopts it as its own `session.session_id` so every
    /// durable record it writes (its `coordinator_runs` row, any batches it fans
    /// out) is attributed to the session that asked for the work, not to the
    /// child's throwaway uuid. This is what makes `:workers`/`background_status`
    /// recognize a background job as belonging to "you".
    pub launch_session_id: String,
    /// The launching session's friendly name (`:name`), if it has one — carried
    /// alongside the id purely for display.
    pub launch_session_name: Option<String>,
    /// Shared `:worker-output` toggle from the launching session. The live stderr
    /// stream reads it PER LINE (see `forward_decision`), so flipping it mid-run
    /// starts/stops forwarding this worker's output. It gates ALL forwarded
    /// coordinator output — the `🔧` tool-activity lines AND the turn/batch
    /// narration — so a background job is QUIET by default (only its `⟳N` prompt
    /// pulse and completion notice show) and streams its full activity only when
    /// `:worker-output` is on.
    pub show_output: Arc<AtomicBool>,
}

impl WorkerJob {
    fn set_done(&self, result: String) {
        let mut i = self.inner.lock().unwrap();
        i.status = "done".into();
        i.result = Some(result);
    }
    /// Record the branch an isolated worker left its changes on (kept worktree).
    fn set_branch(&self, branch: String) {
        self.inner.lock().unwrap().branch = Some(branch);
    }
    fn branch(&self) -> Option<String> {
        self.inner.lock().unwrap().branch.clone()
    }
    /// Record a tool-call outcome for the prompt-badge pulse (green on success,
    /// red on failure). Called from the stderr stream as the coordinator reports
    /// each tool finishing.
    fn record_tool_outcome(&self, success: bool) {
        self.inner.lock().unwrap().last_tool_outcome = Some((success, Instant::now()));
    }
    /// Record a turn/narration completion for the magenta turn pulse.
    fn record_turn_completion(&self) {
        self.inner.lock().unwrap().last_turn_completion = Some(Instant::now());
    }
    /// The most recent badge-pulse event on this worker (tool outcome vs turn
    /// completion — whichever happened later), paired with when it happened.
    /// `None` when neither has occurred. Recency is judged by the caller against
    /// [`PULSE_FADE`].
    fn latest_pulse(&self) -> Option<(Pulse, Instant)> {
        let i = self.inner.lock().unwrap();
        let tool = i
            .last_tool_outcome
            .map(|(ok, t)| (if ok { Pulse::ToolOk } else { Pulse::ToolErr }, t));
        let turn = i.last_turn_completion.map(|t| (Pulse::Turn, t));
        match (tool, turn) {
            (Some(a), Some(b)) => Some(if a.1 >= b.1 { a } else { b }),
            (Some(a), None) => Some(a),
            (None, Some(b)) => Some(b),
            (None, None) => None,
        }
    }
    fn set_failed(&self, err: String) {
        let mut i = self.inner.lock().unwrap();
        i.status = "failed".into();
        i.error = Some(err);
    }
    pub fn status(&self) -> String {
        self.inner.lock().unwrap().status.clone()
    }
    fn is_terminal(&self) -> bool {
        matches!(self.inner.lock().unwrap().status.as_str(), "done" | "failed")
    }
    fn is_displayed(&self) -> bool {
        self.inner.lock().unwrap().displayed
    }
    fn mark_displayed(&self) {
        self.inner.lock().unwrap().displayed = true;
    }
    /// One line for a `:workers`-style listing.
    pub fn summary_line(&self) -> String {
        format!("{} [{}] {}", self.id, self.status(), self.task)
    }
    /// The rendered result, a failure note, or a still-running status.
    pub fn fetch(&self) -> String {
        let i = self.inner.lock().unwrap();
        match i.status.as_str() {
            "done" => i.result.clone().unwrap_or_else(|| "(empty result)".into()),
            "failed" => format!(
                "worker {} failed: {}",
                self.id,
                i.error.clone().unwrap_or_else(|| "unknown error".into())
            ),
            other => format!("worker {} is still running (status: {other}).", self.id),
        }
    }
    /// One-line result summary for table cells — mirrors `format_result` in tools.rs.
    /// Running jobs return `"—"`; done jobs show `"✓ success"` (or `"✓ #NN"` when
    /// the result text contains a PR reference); failed jobs show `"✗ <reason>"`
    /// truncated to ~40 chars so the table stays readable.
    pub fn result_cell(&self) -> String {
        let i = self.inner.lock().unwrap();
        match i.status.as_str() {
            "done" => {
                let r = i.result.as_deref().unwrap_or("");
                if let Some(pr) = r.split_whitespace().find(|s| s.starts_with('#')) {
                    format!("✓ {pr}")
                } else {
                    "✓ success".to_string()
                }
            }
            "failed" => {
                let e = i.error.as_deref().unwrap_or("unknown error");
                let truncated = if e.len() > 40 { format!("{}…", &e[..40]) } else { e.to_string() };
                format!("✗ {truncated}")
            }
            _ => "—".to_string(),
        }
    }
}

/// Mint a fresh worker id in the short, readable `w_########` form: a `w_`
/// prefix plus 8 random base62 characters (a-z, A-Z, 0-9), e.g. `w_a7k3m2pQ`.
///
/// Why base62-8 instead of the old 32-hex-char UUID: these ids exist only to
/// disambiguate the handful of concurrent background workers a single host
/// spawns in a session, and they appear constantly in terminal output, logs,
/// and the `background_status` table — so readability wins. 62^8 ≈ 2.18×10^14
/// (~218 trillion) distinct values give ample collision resistance at those
/// counts (even a few thousand live ids keep collision odds negligible) while
/// being ~4× shorter on screen. The randomness is drawn from a UUIDv4's 122
/// bits, so no extra RNG dependency is pulled in — this is dedup-grade
/// uniqueness, not a security token. The id is an opaque string everywhere it's
/// used (only ever compared / `starts_with`-matched, never parsed), so older
/// `worker_<uuid>`-format ids keep displaying and matching correctly.
fn new_worker_id() -> String {
    const ALPHABET: &[u8; 62] =
        b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
    // Consume a UUIDv4's 122 random bits, peeling off one base62 digit at a
    // time. The modulo bias against 2^128 is astronomically small at 8 digits.
    let mut n = Uuid::new_v4().as_u128();
    let mut suffix = String::with_capacity(8);
    for _ in 0..8 {
        suffix.push(ALPHABET[(n % 62) as usize] as char);
        n /= 62;
    }
    format!("w_{suffix}")
}

/// Register a new background worker and start its run task. Returns the
/// session-local job id. The spec is captured up front so the spawned task is
/// self-contained.
pub fn spawn(jobs: &WorkerJobs, task: String, spec: WorkerSpec) -> String {
    let mut guard = jobs.lock().unwrap();
    // Short, readable `w_########` id (see `new_worker_id`) — still unique enough
    // that workers from different sessions/repos don't collide or mix in
    // `:workers` listings or the coordinator store, but far easier to read in
    // logs and the status table than the old 32-hex-char UUID.
    let id = new_worker_id();
    let job = Arc::new(WorkerJob {
        id: id.clone(),
        task: task.clone(),
        inner: Mutex::new(JobInner {
            status: "running".into(),
            result: None,
            error: None,
            displayed: false,
            branch: None,
            last_tool_outcome: None,
            last_turn_completion: None,
        }),
    });
    guard.push(job.clone());
    drop(guard);

    tokio::spawn(run_worker(jobs.clone(), job, task, spec));
    id
}

/// The run task: re-exec aish in `--coordinator` mode, capture stdout as the
/// result, enforce a timeout, then surface it.
async fn run_worker(jobs: WorkerJobs, job: Arc<WorkerJob>, task: String, spec: WorkerSpec) {
    // Isolation: a writing/building coordinator gets its own git worktree
    // (branched from `spec.base` — a clean trunk baseline by default, or the
    // current HEAD on request) so parallel coordinators can't clobber the shared
    // tree. Best-effort —
    // if `cwd` isn't a repo or `git worktree add` fails, we fall back to the
    // shared cwd (today's behavior). The worktree is torn down on completion if
    // the job made no changes; otherwise it's left intact and its branch reported.
    let worktree = if spec.isolate {
        // Worker ids are globally unique (`w_########`, #86), so the worktree
        // leaf is just the id: two sessions / two checkouts share the `{repo-key}`
        // parent dir but never collide on the leaf. Lives under `~/.atum/worktrees`
        // (off the OS-reaped temp dir — ISS-2046), swept on startup if abandoned.
        create_worktree(&spec.cwd, &job.id, &spec.base)
    } else {
        None
    };
    let run_cwd = worktree.as_ref().map(|w| w.path.clone()).unwrap_or_else(|| spec.cwd.clone());
    let mut cmd = worker_command(&spec, &task, &job.id, &run_cwd);

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            if let Some(wt) = &worktree {
                remove_worktree(wt);
            }
            job.set_failed(format!("couldn't launch worker subprocess: {e}"));
            on_complete(&jobs, &job);
            return;
        }
    };

    // Drain stdout and stderr concurrently (sequential reads can deadlock if the
    // child fills the other pipe's buffer). stdout is the final answer (capped);
    // stderr is STREAMED live — but forwarding is gated behind `:worker-output`
    // (see `stream_stderr`/`forward_decision`), so by default a background job is
    // quiet: its `🔧` tool-activity isn't echoed. A bounded stderr tail is always
    // retained for the failure message regardless of forwarding.
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");
    let label = job.id.clone();
    let show_output = spec.show_output.clone();
    let pulse_job = job.clone();
    let collect = tokio::spawn(async move {
        tokio::join!(
            read_capped(stdout, CAPTURE_CAP),
            stream_stderr(stderr, &label, show_output, Some(pulse_job))
        )
    });

    let status = match tokio::time::timeout(WORKER_TIMEOUT, child.wait()).await {
        Ok(Ok(s)) => Some(s),
        Ok(Err(e)) => {
            collect.abort();
            job.set_failed(format!("worker process error: {e}"));
            on_complete(&jobs, &job);
            return;
        }
        Err(_) => {
            // Timed out — kill the child, then fall through to report it.
            let _ = child.start_kill();
            let _ = child.wait().await;
            None
        }
    };

    let (out, err) = collect.await.unwrap_or_default();
    // Finalize the worktree (if any): a clean one (no changes, no new commits) is
    // removed so nothing is left behind; one with work is kept and its branch
    // surfaced so the parent can review/merge it. Returns the kept branch, if any.
    let kept_branch = finalize_worktree(worktree.as_ref());
    if let Some(branch) = &kept_branch {
        job.set_branch(branch.clone());
    }
    match status {
        Some(s) if s.success() => {
            let t = out.trim();
            let mut result = if t.is_empty() { "(no output)".to_string() } else { t.to_string() };
            if let Some(wt) = worktree.as_ref() {
                if let Some(branch) = &kept_branch {
                    result.push_str(&format!(
                        "\n\n(changes left on branch `{branch}` in worktree `{}` — review/merge \
from the parent repo; not auto-merged.)",
                        wt.path.display(),
                    ));
                }
            }
            job.set_done(result);
        }
        Some(s) => job.set_failed(describe_failure(s, "worker", &err)),
        None => job.set_failed(format!(
            "worker timed out after {}s",
            WORKER_TIMEOUT.as_secs()
        )),
    }
    on_complete(&jobs, &job);
}

/// Tear down or keep a finished worker's worktree. If it has no changes and no
/// commits ahead, remove it + its branch (nothing left behind) and return
/// `None`. If it has work, leave it intact and return the branch name so the
/// parent can review/merge it (never auto-merged).
fn finalize_worktree(worktree: Option<&Worktree>) -> Option<String> {
    let wt = worktree?;
    if worktree_is_clean(wt) {
        remove_worktree(wt);
        None
    } else {
        Some(wt.branch.clone())
    }
}

/// Run a single coordinator subprocess to completion and return its stdout (the
/// final answer). Unlike `spawn`, it doesn't register a tracked job or
/// auto-deliver — the caller consumes the output. Used by the goal loop for each
/// work step.
pub async fn run_once(spec: &WorkerSpec, task: &str, run_id: &str) -> Result<String, String> {
    // The goal loop never isolates (it iterates in the user's live cwd), so we
    // run in `spec.cwd` directly.
    let mut cmd = worker_command(spec, task, run_id, &spec.cwd);
    let mut child = cmd.spawn().map_err(|e| format!("couldn't launch goal worker: {e}"))?;
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");
    // stdout is the result (capped capture, unchanged). stderr is streamed live
    // to the user's terminal — gated behind `:worker-output` like the background
    // worker path — retaining only a bounded tail for the failure message.
    let show_output = spec.show_output.clone();
    let collect = tokio::spawn(async move {
        tokio::join!(
            read_capped(stdout, CAPTURE_CAP),
            stream_stderr(stderr, "goal", show_output, None)
        )
    });
    let status = match tokio::time::timeout(WORKER_TIMEOUT, child.wait()).await {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => {
            collect.abort();
            return Err(format!("goal worker process error: {e}"));
        }
        Err(_) => {
            let _ = child.start_kill();
            let _ = child.wait().await;
            return Err(format!("goal worker timed out after {}s", WORKER_TIMEOUT.as_secs()));
        }
    };
    let (out, err) = collect.await.unwrap_or_default();
    if status.success() {
        Ok(out.trim().to_string())
    } else {
        Err(describe_failure(status, "goal worker", &err))
    }
}

/// Called when one worker finishes. While others run, print a brief progress
/// line; once all have finished, flush every not-yet-shown result at once.
/// Mirrors `batch::on_complete`.
fn on_complete(jobs: &WorkerJobs, finished: &Arc<WorkerJob>) {
    // Interactive REPL: the presenter drains at a pause (see batch::on_complete).
    if crate::present::deferred() {
        return;
    }
    let (all_terminal, remaining) = {
        let g = jobs.lock().unwrap();
        let remaining = g.iter().filter(|j| !j.is_terminal()).count();
        (remaining == 0, remaining)
    };
    if !all_terminal {
        crate::tools::announce(
            &format!("[{}]", finished.id),
            &format!("{} — {remaining} worker(s) still running", finished.status()),
        );
        return;
    }
    flush_results(jobs);
}

/// Format every finished-but-not-yet-shown worker result into a display block,
/// marking each shown. Shared by the headless flush and the REPL presenter.
pub fn drain_pending(jobs: &WorkerJobs) -> Vec<String> {
    let pending: Vec<Arc<WorkerJob>> = {
        let g = jobs.lock().unwrap();
        g.iter().filter(|j| j.is_terminal() && !j.is_displayed()).cloned().collect()
    };
    pending
        .iter()
        .map(|job| {
            let label = if job.status() == "failed" { "failed" } else { "complete" };
            job.mark_displayed();
            format!(
                "\x1b[2m── worker {} {label} ──\x1b[0m\n{}",
                job.id,
                crate::md::render_stdout(job.fetch().trim())
            )
        })
        .collect()
}

/// One-line completion NOTICES for finished-but-unshown workers, marking them
/// shown. The presenter notifies (rather than dumping the full result over the
/// prompt); the user views it with `:result <id>`. Result stays in `fetch`.
pub fn notify_pending(jobs: &WorkerJobs) -> Vec<String> {
    let pending: Vec<Arc<WorkerJob>> = {
        let g = jobs.lock().unwrap();
        g.iter().filter(|j| j.is_terminal() && !j.is_displayed()).cloned().collect()
    };
    pending
        .iter()
        .map(|job| {
            let (icon, what) = if job.status() == "failed" { ("✗", "failed") } else { ("✓", "done") };
            job.mark_displayed();
            // Surface the branch an isolated worker left changes on, so the parent
            // knows where to review/merge without opening the full result.
            let branch = job.branch().map(|b| format!(" · branch `{b}`")).unwrap_or_default();
            format!(
                "\x1b[2m{icon} {} {what} — `:result {}` to view · {}{branch}\x1b[0m",
                job.id,
                job.id,
                crate::batch::one_line(&job.task)
            )
        })
        .collect()
}

/// Count of workers still running — for the prompt's `⟳N` indicator.
pub fn running_count(jobs: &WorkerJobs) -> usize {
    jobs.lock().unwrap().iter().filter(|j| !j.is_terminal()).count()
}

/// The most recent still-fresh badge pulse across ALL workers (most-recent
/// wins), or `None` when no worker has had an event within [`PULSE_FADE`]. Drives
/// the colour of the prompt's `⟳N` badge.
pub fn fresh_pulse(jobs: &WorkerJobs) -> Option<Pulse> {
    let now = Instant::now();
    jobs.lock()
        .unwrap()
        .iter()
        .filter_map(|j| j.latest_pulse())
        .filter(|(_, when)| now.saturating_duration_since(*when) < PULSE_FADE)
        .max_by_key(|&(_, when)| when)
        .map(|(p, _)| p)
}

/// Build the prompt's `⟳N` background-jobs badge, coloured by the most recent
/// background-worker event:
///   * green `✓N`   — a tool call just succeeded,
///   * red `✗N`     — a tool call just failed,
///   * magenta `⟳N` — the model just emitted a turn/narration line,
///   * dim `⟳N`     — idle (no recent event, or the pulse has faded).
/// `running` is the TOTAL live background-job count (workers + batches); the
/// badge is empty when nothing is running. `pulse` is [`fresh_pulse`]'s verdict.
/// Pure, so the colour/glyph mapping is unit-testable.
pub fn pulse_badge(running: usize, pulse: Option<Pulse>) -> String {
    if running == 0 {
        return String::new();
    }
    match pulse {
        Some(Pulse::ToolOk) => format!("\x1b[32m✓{running}\x1b[0m "), // green tick
        Some(Pulse::ToolErr) => format!("\x1b[31m✗{running}\x1b[0m "), // red cross
        Some(Pulse::Turn) => format!("\x1b[1;35m⟳{running}\x1b[0m "), // bright magenta
        None => format!("\x1b[2m⟳{running}\x1b[0m "),                 // idle dim
    }
}

/// Headless inline flush (no presenter): print every drained block to stdout.
fn flush_results(jobs: &WorkerJobs) {
    let blocks = drain_pending(jobs);
    if blocks.is_empty() {
        return;
    }
    print!("\r\x1b[2K");
    for b in &blocks {
        println!("{b}");
    }
    use std::io::Write;
    std::io::stdout().flush().ok();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spawn_assigns_unique_ids() {
        // IDs are now the short `w_########` form — readable, and unique enough
        // that two freshly-minted ids don't collide.
        let id1 = new_worker_id();
        let id2 = new_worker_id();
        assert!(id1.starts_with("w_"));
        assert!(id2.starts_with("w_"));
        assert_eq!(id1.len(), 10); // "w_" + 8 chars
        assert_ne!(id1, id2);
    }

    #[test]
    fn worker_id_format_and_collision_freeness() {
        // AC: every id matches `w_` + exactly 8 [a-zA-Z0-9] chars, and a large
        // batch is collision-free in practice (the readability/entropy tradeoff
        // documented on `new_worker_id`).
        use std::collections::HashSet;
        let mut seen = HashSet::new();
        for _ in 0..10_000 {
            let id = new_worker_id();
            let suffix = id.strip_prefix("w_").expect("id must carry the w_ prefix");
            assert_eq!(suffix.len(), 8, "suffix must be 8 chars: {id}");
            assert!(
                suffix.chars().all(|c| c.is_ascii_alphanumeric()),
                "suffix must be alphanumeric (a-z, A-Z, 0-9): {id}"
            );
            assert!(seen.insert(id.clone()), "collision generating 10k ids: {id}");
        }
        assert_eq!(seen.len(), 10_000, "all 10k generated ids must be distinct");
    }

    #[test]
    fn fetch_reports_running_then_done() {
        let job = Arc::new(WorkerJob {
            id: "worker_1".into(),
            task: "scan repo".into(),
            inner: Mutex::new(JobInner {
                status: "running".into(),
                result: None,
                error: None,
                displayed: false,
                branch: None,
                last_tool_outcome: None,
                last_turn_completion: None,
            }),
        });
        assert!(job.fetch().contains("still running"));
        job.set_done("the answer".into());
        assert_eq!(job.fetch(), "the answer");
        assert!(job.summary_line().contains("worker_1"));
        assert!(job.summary_line().contains("done"));
    }

    #[test]
    fn failed_worker_reports_error() {
        let job = Arc::new(WorkerJob {
            id: "worker_2".into(),
            task: "x".into(),
            inner: Mutex::new(JobInner {
                status: "running".into(),
                result: None,
                error: None,
                displayed: false,
                branch: None,
                last_tool_outcome: None,
                last_turn_completion: None,
            }),
        });
        job.set_failed("boom".into());
        assert!(job.fetch().contains("worker_2 failed: boom"));
    }

    #[test]
    fn mem_limit_env_parsing() {
        // Unset → default.
        assert_eq!(parse_u64_or(None, DEFAULT_WORKER_MEM_MB), DEFAULT_WORKER_MEM_MB);
        // Valid override (with surrounding whitespace) parses.
        assert_eq!(parse_u64_or(Some(" 1024 "), DEFAULT_WORKER_MEM_MB), 1024);
        // Garbage → default.
        assert_eq!(parse_u64_or(Some("not-a-number"), DEFAULT_WORKER_MEM_MB), DEFAULT_WORKER_MEM_MB);
        // Empty → default.
        assert_eq!(parse_u64_or(Some(""), DEFAULT_WORKER_MEM_MB), DEFAULT_WORKER_MEM_MB);
        // 0 is a legal "no limit" value and must round-trip, not fall back.
        assert_eq!(parse_u64_or(Some("0"), DEFAULT_WORKER_CPU_SECS), 0);
    }

    #[test]
    fn clean_activity_line_forwards_only_result_lines() {
        // Per tool the coordinator emits a bare START line then a ✓/✗ RESULT
        // line. To avoid logging each tool call twice, only the RESULT line is
        // forwarded; the bare START line is dropped.
        //
        // RESULT line (✓ success) — forwarded.
        assert_eq!(
            clean_activity_line("\x1b[2m  ✓ 🔧 read /etc/hosts\x1b[0m"),
            Some("✓ 🔧 read /etc/hosts".to_string())
        );
        // RESULT line (✗ failure) — forwarded.
        assert_eq!(
            clean_activity_line("\x1b[2m  ✗ 🔧 write x\x1b[0m"),
            Some("✗ 🔧 write x".to_string())
        );
        // RESULT line with the real inner colour codes engine.rs emits — still
        // forwarded; the outer dim wrapper is stripped, inner colour preserved.
        assert_eq!(
            clean_activity_line("\x1b[2m  \x1b[32m✓\x1b[0m 🔧 read /etc/hosts\x1b[0m"),
            Some("\x1b[32m✓\x1b[0m 🔧 read /etc/hosts".to_string())
        );
        // Bare START lines (wrench, no ✓/✗) are the duplicate — DROPPED now.
        assert_eq!(clean_activity_line("\x1b[2m  🔧 git status\x1b[0m"), None);
        assert_eq!(clean_activity_line("\x1b[2m  🔧 mcp__atum__list_tools\x1b[0m"), None);
        assert_eq!(clean_activity_line("🔧 ls"), None);
        // Lines without the wrench are dropped (banner, blanks, prose).
        assert_eq!(clean_activity_line("coordinator run abc starting"), None);
        assert_eq!(clean_activity_line(""), None);
        assert_eq!(clean_activity_line("   \x1b[2m\x1b[0m  "), None);
    }

    #[test]
    fn strip_sentinel_extracts_turn_and_batch_lines() {
        // 🗨 turn text (emitted by the coordinator's engine narration).
        assert_eq!(
            strip_sentinel("🗨 planning the migration", "🗨"),
            Some("planning the migration".to_string())
        );
        // 📦 batch fan-out notice.
        assert_eq!(
            strip_sentinel("📦 fanned 3 sub-task(s) out", "📦"),
            Some("fanned 3 sub-task(s) out".to_string())
        );
        // Wrong sentinel / plain lines / empty payload → None.
        assert_eq!(strip_sentinel("🗨 hi", "📦"), None);
        assert_eq!(strip_sentinel("just prose", "🗨"), None);
        assert_eq!(strip_sentinel("🗨   ", "🗨"), None);
        // A 🔧 tool line is NOT a turn line (it routes through clean_activity_line).
        assert_eq!(strip_sentinel("🔧 git status", "🗨"), None);
    }

    #[test]
    fn forward_decision_gates_all_output_on_worker_output_toggle() {
        // A tool emits a bare START line then a ✓/✗ RESULT line. Only the RESULT
        // line is forwarded (so each tool call logs exactly once).
        let tool_start = "\x1b[2m  🔧 git status\x1b[0m";
        let tool_result = "\x1b[2m  ✓ 🔧 git status\x1b[0m";
        let turn = "🗨 planning the migration";
        let batch = "📦 fanned 3 sub-task(s) out";
        let banner = "coordinator run abc starting";

        // OFF (default): EVERYTHING is suppressed. This is the headline behavior:
        // a background coordinator is quiet by default.
        assert_eq!(forward_decision(tool_start, false), None);
        assert_eq!(forward_decision(tool_result, false), None);
        assert_eq!(forward_decision(turn, false), None);
        assert_eq!(forward_decision(batch, false), None);
        assert_eq!(forward_decision(banner, false), None);

        // ON: the full live stream returns, each tagged with its label suffix —
        // but the tool call is forwarded ONCE, via its RESULT line only.
        assert_eq!(forward_decision(tool_start, true), None); // duplicate START — dropped
        assert_eq!(
            forward_decision(tool_result, true),
            Some(("", "✓ 🔧 git status".to_string()))
        );
        assert_eq!(
            forward_decision(turn, true),
            Some(("·standard", "planning the migration".to_string()))
        );
        assert_eq!(
            forward_decision(batch, true),
            Some(("·batch", "fanned 3 sub-task(s) out".to_string()))
        );
        // Noise (banner/blank) is dropped even when output is ON.
        assert_eq!(forward_decision(banner, true), None);
        assert_eq!(forward_decision("", true), None);
    }

    #[test]
    fn worker_output_logs_each_tool_call_exactly_once() {
        // Regression: the duplicate tool-call logging bug. A single tool call
        // produces a START line then a RESULT line on the coordinator's stderr;
        // with :worker-output ON, the forwarder must emit exactly ONE line for
        // that call (the RESULT line), not two.
        let per_call = [
            "\x1b[2m  🔧 mcp__atum__atum_get_project_task\x1b[0m", // START
            "\x1b[2m  ✓ 🔧 mcp__atum__atum_get_project_task\x1b[0m", // RESULT
        ];
        let forwarded: Vec<String> = per_call
            .iter()
            .filter_map(|l| forward_decision(l, true).map(|(_, t)| t))
            .collect();
        assert_eq!(
            forwarded,
            vec!["✓ 🔧 mcp__atum__atum_get_project_task".to_string()],
            "each tool call must forward exactly once (the RESULT line)"
        );
    }

    #[test]
    fn worktree_layout_builds_branch_and_path() {
        let root = std::path::Path::new("/wt-root");
        let (branch, path) = worktree_layout(root, "LightHeart-Ventures--aish", "w_a7k3m2pQ");
        // Branch is just `aish/{id}` — the id is globally unique, no session prefix.
        assert_eq!(branch, "aish/w_a7k3m2pQ");
        // Path = {root}/{repo-key}/{id}; the leaf is exactly the worker id.
        assert_eq!(path, root.join("LightHeart-Ventures--aish").join("w_a7k3m2pQ"));
        assert!(path.ends_with("w_a7k3m2pQ"), "got: {}", path.display());
        // Distinct ids never collide on path or branch.
        let (b2, p2) = worktree_layout(root, "LightHeart-Ventures--aish", "w_ZZ00ay12");
        assert_ne!(branch, b2);
        assert_ne!(path, p2);
        // Distinct repo-keys get distinct parent dirs even with the same id.
        let (_, other) = worktree_layout(root, "other--repo", "w_a7k3m2pQ");
        assert_ne!(path, other);
    }

    #[test]
    fn repo_key_from_remote_parses_https_and_ssh() {
        // https, with and without the `.git` suffix.
        assert_eq!(
            repo_key_from_remote("https://github.com/LightHeart-Ventures/aish.git").as_deref(),
            Some("LightHeart-Ventures--aish")
        );
        assert_eq!(
            repo_key_from_remote("https://github.com/LightHeart-Ventures/aish").as_deref(),
            Some("LightHeart-Ventures--aish")
        );
        // Trailing slash is stripped.
        assert_eq!(
            repo_key_from_remote("https://github.com/octo/Hello-World/").as_deref(),
            Some("octo--Hello-World")
        );
        // `git@` ssh form.
        assert_eq!(
            repo_key_from_remote("git@github.com:LightHeart-Ventures/aish.git").as_deref(),
            Some("LightHeart-Ventures--aish")
        );
        // `ssh://` form.
        assert_eq!(
            repo_key_from_remote("ssh://git@github.com/owner/repo.git").as_deref(),
            Some("owner--repo")
        );
        // Non-GitHub / unparseable → None (caller falls back to basename+hash).
        assert_eq!(repo_key_from_remote("https://gitlab.com/owner/repo.git"), None);
        assert_eq!(repo_key_from_remote("git@bitbucket.org:owner/repo.git"), None);
        assert_eq!(repo_key_from_remote("not a url"), None);
        assert_eq!(repo_key_from_remote("https://github.com/owner"), None); // no repo segment
    }

    #[test]
    fn sanitize_repo_key_maps_slash_and_unsafe_chars() {
        assert_eq!(sanitize_repo_key("owner/repo"), "owner--repo");
        // Allowed chars (alnum, dot, underscore, hyphen) survive.
        assert_eq!(sanitize_repo_key("Acme.Co/My_Repo-1"), "Acme.Co--My_Repo-1");
        // Anything else collapses to '-'.
        assert_eq!(sanitize_repo_key("a b/c:d"), "a-b--c-d");
    }

    #[test]
    fn fallback_repo_key_is_basename_plus_stable_shorthash() {
        let a = fallback_repo_key(std::path::Path::new("/home/me/projects/aish"));
        let b = fallback_repo_key(std::path::Path::new("/home/me/projects/aish"));
        // Stable for the same absolute path.
        assert_eq!(a, b);
        assert!(a.starts_with("aish-"), "got: {a}");
        // A different checkout of the same-named repo gets a distinct key
        // (collision-safety preserved from the old FNV-of-abspath scheme).
        let c = fallback_repo_key(std::path::Path::new("/tmp/aish"));
        assert_ne!(a, c);
        assert!(c.starts_with("aish-"), "got: {c}");
    }

    #[test]
    fn should_sweep_keeps_dirty_or_ahead_reclaims_clean() {
        // Never reclaim a dirty or commits-ahead worktree — regardless of
        // orphan/age. This is the data-safety invariant.
        assert!(!should_sweep(true, false, true, 999, 7));
        assert!(!should_sweep(false, true, true, 999, 7));
        assert!(!should_sweep(true, true, false, 0, 7));
        // Clean + orphaned (registration gone) → reclaim immediately.
        assert!(should_sweep(false, false, true, 0, 7));
        // Clean + old enough → reclaim.
        assert!(should_sweep(false, false, false, 7, 7));
        assert!(should_sweep(false, false, false, 30, 7));
        // Clean but young and still registered → keep.
        assert!(!should_sweep(false, false, false, 6, 7));
        assert!(!should_sweep(false, false, false, 0, 7));
    }

    #[tokio::test]
    async fn stream_stderr_records_pulse_from_coordinator_lines() {
        // A realistic slice of a coordinator's piped stderr: a tool start line,
        // its success result line, a narration line, then a tool failure. The
        // pulse must end on the most recent event (the failure).
        let job = Arc::new(WorkerJob {
            id: "worker_1".into(),
            task: "t".into(),
            inner: Mutex::new(JobInner {
                status: "running".into(),
                result: None,
                error: None,
                displayed: false,
                branch: None,
                last_tool_outcome: None,
                last_turn_completion: None,
            }),
        });
        let lines = concat!(
            "\x1b[2m  \u{1f527} read /etc/hosts\x1b[0m\n",
            "\x1b[2m  \x1b[32m\u{2713}\x1b[0m \u{1f527} read /etc/hosts\x1b[0m\n",
            "\u{1f5e8} planning the next step\n",
            "\x1b[2m  \u{1f527} write x\x1b[0m\n",
            "\x1b[2m  \x1b[31m\u{2717}\x1b[0m \u{1f527} write x\x1b[0m\n",
        );
        let show = Arc::new(AtomicBool::new(false));
        let reader = lines.as_bytes();
        let _tail = stream_stderr(reader, "worker_1", show, Some(job.clone())).await;
        // Most recent event was the tool FAILURE -> red cross pulse.
        assert_eq!(job.latest_pulse().map(|(p, _)| p), Some(Pulse::ToolErr));
        // And it is fresh, so the aggregate badge is the red-cross variant.
        let jobs: WorkerJobs = Arc::new(Mutex::new(vec![job]));
        assert_eq!(pulse_badge(1, fresh_pulse(&jobs)), "\x1b[31m\u{2717}1\x1b[0m ");
    }

    #[test]
    fn classify_event_maps_tool_and_turn_lines() {
        // Coordinator non-TTY result lines carry a status glyph beside the wrench.
        assert_eq!(classify_event("\x1b[2m  ✓ 🔧 read /etc/hosts\x1b[0m"), Some(Pulse::ToolOk));
        assert_eq!(classify_event("\x1b[2m  ✗ 🔧 write x\x1b[0m"), Some(Pulse::ToolErr));
        // A bare start line (no ✓/✗) is the tool beginning, not an outcome.
        assert_eq!(classify_event("\x1b[2m  🔧 git status\x1b[0m"), None);
        // Turn narration carries the speech sentinel.
        assert_eq!(classify_event("🗨 planning the migration"), Some(Pulse::Turn));
        // Noise lines carry nothing.
        assert_eq!(classify_event("coordinator run abc starting"), None);
        assert_eq!(classify_event(""), None);
        // A batch sentinel is not a pulse event.
        assert_eq!(classify_event("📦 fanned 3 sub-task(s) out"), None);
    }

    #[test]
    fn latest_pulse_picks_the_most_recent_event() {
        let job = Arc::new(WorkerJob {
            id: "worker_1".into(),
            task: "t".into(),
            inner: Mutex::new(JobInner {
                status: "running".into(),
                result: None,
                error: None,
                displayed: false,
                branch: None,
                last_tool_outcome: None,
                last_turn_completion: None,
            }),
        });
        // No events yet.
        assert!(job.latest_pulse().is_none());
        // A tool success, then (later) a turn completion: turn wins (most recent).
        job.record_tool_outcome(true);
        std::thread::sleep(std::time::Duration::from_millis(2));
        job.record_turn_completion();
        assert_eq!(job.latest_pulse().map(|(p, _)| p), Some(Pulse::Turn));
        // A still-later tool failure overtakes the turn.
        std::thread::sleep(std::time::Duration::from_millis(2));
        job.record_tool_outcome(false);
        assert_eq!(job.latest_pulse().map(|(p, _)| p), Some(Pulse::ToolErr));
    }

    #[test]
    fn fresh_pulse_aggregates_and_fades() {
        let jobs: WorkerJobs = Default::default();
        // Empty → nothing to pulse.
        assert_eq!(fresh_pulse(&jobs), None);
        let mk = |id: &str| {
            let j = Arc::new(WorkerJob {
                id: id.into(),
                task: "t".into(),
                inner: Mutex::new(JobInner {
                    status: "running".into(),
                    result: None,
                    error: None,
                    displayed: false,
                    branch: None,
                    last_tool_outcome: None,
                    last_turn_completion: None,
                }),
            });
            jobs.lock().unwrap().push(j.clone());
            j
        };
        let a = mk("worker_1");
        let b = mk("worker_2");
        a.record_tool_outcome(true);
        std::thread::sleep(std::time::Duration::from_millis(2));
        b.record_tool_outcome(false);
        // Most-recent across workers wins: b's failure.
        assert_eq!(fresh_pulse(&jobs), Some(Pulse::ToolErr));
        // A stale event (older than PULSE_FADE) fades out of the aggregate.
        {
            let mut i = b.inner.lock().unwrap();
            i.last_tool_outcome = Some((false, Instant::now() - PULSE_FADE - Duration::from_millis(50)));
        }
        {
            let mut i = a.inner.lock().unwrap();
            i.last_tool_outcome = Some((true, Instant::now() - PULSE_FADE - Duration::from_millis(50)));
        }
        assert_eq!(fresh_pulse(&jobs), None);
    }

    #[test]
    fn pulse_badge_colours_by_event_and_count() {
        // Nothing running → empty badge regardless of pulse.
        assert_eq!(pulse_badge(0, Some(Pulse::ToolOk)), "");
        // Idle (no recent event) → dim ⟳N.
        assert_eq!(pulse_badge(2, None), "\x1b[2m⟳2\x1b[0m ");
        // Tool success → green ✓N.
        assert_eq!(pulse_badge(1, Some(Pulse::ToolOk)), "\x1b[32m✓1\x1b[0m ");
        // Tool failure → red ✗N.
        assert_eq!(pulse_badge(1, Some(Pulse::ToolErr)), "\x1b[31m✗1\x1b[0m ");
        // Turn completion → bright magenta ⟳N.
        assert_eq!(pulse_badge(3, Some(Pulse::Turn)), "\x1b[1;35m⟳3\x1b[0m ");
    }

    #[test]
    fn describe_failure_names_sigkill_as_oom() {
        use std::os::unix::process::ExitStatusExt;
        // A status synthesised from signal 9 (SIGKILL).
        let killed = std::process::ExitStatus::from_raw(libc::SIGKILL);
        let msg = describe_failure(killed, "worker", "some stderr noise");
        assert!(msg.contains("killed by the OS"), "got: {msg}");
        assert!(msg.contains("AISH_WORKER_MEM_MB"), "got: {msg}");

        // A non-signal failure keeps the plain message and doesn't mention OOM.
        let exited = std::process::ExitStatus::from_raw(1 << 8); // exit code 1
        let msg = describe_failure(exited, "goal worker", "boom");
        assert!(msg.contains("exited unsuccessfully"), "got: {msg}");
        assert!(!msg.contains("killed by the OS"), "got: {msg}");
    }
}
