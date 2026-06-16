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
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::process::Command;

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

/// How many of the child's most-recent stderr lines we retain for the failure
/// message. We stream stderr live rather than accumulating it (an unbounded
/// accumulation was the OOM risk), so the failure path can only quote a bounded
/// tail rather than the whole thing.
const STDERR_TAIL_LINES: usize = 20;

/// Decide whether a single raw coordinator-stderr line is worth forwarding to
/// the user's terminal, and if so return the cleaned text to forward.
///
/// The coordinator runs non-TTY, so its tool-activity lines are static and
/// escape-light: `"\x1b[2m  🔧 git status\x1b[0m"`. We forward only the lines
/// that show tool activity (those containing `🔧`) so the goal stream isn't
/// noisy — the "coordinator run … starting" banner and blank lines are dropped.
/// Cleaning strips leading whitespace and the dim `\x1b[2m…\x1b[0m` wrapper, so
/// `announce` (which re-wraps in dim) doesn't double-wrap.
fn clean_activity_line(raw: &str) -> Option<String> {
    if !raw.contains('🔧') {
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

/// Stream a child's stderr line by line, forwarding the interesting lines to the
/// user's terminal live via `announce`, and retaining only the last
/// `STDERR_TAIL_LINES` raw lines as a bounded ring for the failure message.
/// Returns the retained tail joined with newlines (oldest-first).
///
/// Three line kinds are recognized by sentinel:
/// - `🔧` tool activity → always forwarded as `[label] …`.
/// - `🗨` turn text (a standard model call) → forwarded as `[label·standard] …`
///   only while `:worker-output` is on (`show_output`).
/// - `📦` batch fan-out notice → forwarded as `[label·batch] …`, also gated on
///   `show_output` (so "off" stays exactly today's tool-only stream).
///
/// This keeps the child's stderr pipe drained (so it never blocks) and gives the
/// user live activity without accumulating all of stderr in memory.
async fn stream_stderr<R: tokio::io::AsyncRead + Unpin>(
    r: R,
    label: &str,
    show_output: Arc<AtomicBool>,
) -> String {
    let mut lines = BufReader::new(r).lines();
    let mut tail: VecDeque<String> = VecDeque::with_capacity(STDERR_TAIL_LINES);
    let base = format!("[{label}]");
    while let Ok(Some(line)) = lines.next_line().await {
        if let Some(activity) = clean_activity_line(&line) {
            crate::tools::announce(&base, &activity);
        } else if show_output.load(Ordering::Relaxed) {
            if let Some(text) = strip_sentinel(&line, "🗨") {
                crate::tools::announce(&format!("[{label}·standard]"), &text);
            } else if let Some(text) = strip_sentinel(&line, "📦") {
                crate::tools::announce(&format!("[{label}·batch]"), &text);
            }
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

/// Build the branch name + worktree path for a worker. Pure (no IO) so it's
/// unit-testable. The worktree lives OUTSIDE the repo — under the system temp
/// dir, in a subdir keyed by a hash of the source path so two repos with the
/// same basename don't collide — so it never pollutes the source repo's
/// `git status` with an untracked dir. The branch is `aish/<id>`.
fn worktree_layout(src: &std::path::Path, id: &str) -> (String, PathBuf) {
    let branch = format!("aish/{id}");
    // Stable per-repo key (FNV-1a of the absolute source path) so worktrees for
    // the same repo cluster together and distinct repos never share a dir.
    let mut hash: u64 = 0xcbf29ce484222325;
    for b in src.to_string_lossy().as_bytes() {
        hash ^= *b as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    let path = std::env::temp_dir()
        .join("aish-worktrees")
        .join(format!("{hash:016x}"))
        .join(id);
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
    let (branch, path) = worktree_layout(src, id);
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
    /// Session-local handle, e.g. "worker_1".
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
    /// stream reads it per line, so flipping it mid-run starts/stops forwarding
    /// this worker's *turn* output (the always-on `🔧` tool lines are unaffected).
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
}

/// Register a new background worker and start its run task. Returns the
/// session-local job id. The spec is captured up front so the spawned task is
/// self-contained.
pub fn spawn(jobs: &WorkerJobs, task: String, spec: WorkerSpec) -> String {
    let mut guard = jobs.lock().unwrap();
    let n = guard
        .iter()
        .filter_map(|j| j.id.strip_prefix("worker_").and_then(|s| s.parse::<usize>().ok()))
        .max()
        .unwrap_or(0)
        + 1;
    let id = format!("worker_{n}");
    let job = Arc::new(WorkerJob {
        id: id.clone(),
        task: task.clone(),
        inner: Mutex::new(JobInner {
            status: "running".into(),
            result: None,
            error: None,
            displayed: false,
            branch: None,
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
    let worktree = if spec.isolate { create_worktree(&spec.cwd, &job.id, &spec.base) } else { None };
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
    // stderr is STREAMED live — its `🔧` tool-activity lines forward to the
    // user's terminal as the coordinator works (prefixed with the worker id), so
    // a background job isn't a silent black box. A bounded stderr tail is kept
    // for the failure message.
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");
    let label = job.id.clone();
    let show_output = spec.show_output.clone();
    let collect = tokio::spawn(async move {
        tokio::join!(read_capped(stdout, CAPTURE_CAP), stream_stderr(stderr, &label, show_output))
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
    // to the user's terminal as `[goal] 🔧 …` tool-activity, retaining only a
    // bounded tail for the failure message — no unbounded accumulation.
    let show_output = spec.show_output.clone();
    let collect = tokio::spawn(async move {
        tokio::join!(read_capped(stdout, CAPTURE_CAP), stream_stderr(stderr, "goal", show_output))
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
    fn spawn_assigns_incrementing_ids() {
        let jobs: WorkerJobs = Default::default();
        let mk = |jobs: &WorkerJobs, id: usize| {
            jobs.lock().unwrap().push(Arc::new(WorkerJob {
                id: format!("worker_{id}"),
                task: "t".into(),
                inner: Mutex::new(JobInner {
                    status: "running".into(),
                    result: None,
                    error: None,
                    displayed: false,
                    branch: None,
                }),
            }));
        };
        mk(&jobs, 1);
        mk(&jobs, 2);
        let next = jobs
            .lock()
            .unwrap()
            .iter()
            .filter_map(|j| j.id.strip_prefix("worker_").and_then(|s| s.parse::<usize>().ok()))
            .max()
            .unwrap_or(0)
            + 1;
        assert_eq!(next, 3);
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
    fn clean_activity_line_forwards_only_tool_lines() {
        // The coordinator's non-TTY static tool line: two leading spaces, dim-wrapped.
        assert_eq!(
            clean_activity_line("\x1b[2m  🔧 git status\x1b[0m"),
            Some("🔧 git status".to_string())
        );
        // An MCP tool name, same shape.
        assert_eq!(
            clean_activity_line("\x1b[2m  🔧 mcp__atum__list_tools\x1b[0m"),
            Some("🔧 mcp__atum__list_tools".to_string())
        );
        // A ✓/✗-prefixed post-execution line still carries 🔧 and is forwarded.
        assert_eq!(
            clean_activity_line("\x1b[2m  ✓ 🔧 read /etc/hosts\x1b[0m"),
            Some("✓ 🔧 read /etc/hosts".to_string())
        );
        // No wrapper at all — still cleaned/forwarded.
        assert_eq!(clean_activity_line("🔧 ls"), Some("🔧 ls".to_string()));
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
    fn worktree_layout_builds_branch_and_path() {
        let src = std::path::Path::new("/repo");
        let (branch, path) = worktree_layout(src, "worker_3");
        assert_eq!(branch, "aish/worker_3");
        // Lives OUTSIDE the repo (in temp), keyed by repo, ending in the id — so
        // it never pollutes the source `git status`.
        assert!(path.starts_with(std::env::temp_dir()), "got: {}", path.display());
        assert!(path.ends_with("worker_3"), "got: {}", path.display());
        assert!(!path.starts_with(src));
        // Distinct ids never collide on path or branch.
        let (b2, p2) = worktree_layout(src, "worker_4");
        assert_ne!(branch, b2);
        assert_ne!(path, p2);
        // Distinct repos get distinct keyed dirs even with the same id.
        let (_, other_repo) = worktree_layout(std::path::Path::new("/other"), "worker_3");
        assert_ne!(path, other_repo);
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
