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

/// Stream a child's stderr line by line: forward tool-activity lines to the
/// user's terminal live via `announce(prefix, …)`, and retain only the last
/// `STDERR_TAIL_LINES` raw lines as a bounded ring for the failure message.
/// Returns the retained tail joined with newlines (oldest-first).
///
/// This both keeps the child's stderr pipe drained (so it never blocks) and
/// gives the user live `[goal] 🔧 …` activity, without accumulating all of
/// stderr in memory.
async fn stream_stderr<R: tokio::io::AsyncRead + Unpin>(r: R, prefix: &str) -> String {
    let mut lines = BufReader::new(r).lines();
    let mut tail: VecDeque<String> = VecDeque::with_capacity(STDERR_TAIL_LINES);
    while let Ok(Some(line)) = lines.next_line().await {
        if let Some(activity) = clean_activity_line(&line) {
            crate::tools::announce(prefix, &activity);
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
fn worker_command(spec: &WorkerSpec, task: &str, run_id: &str) -> Command {
    let mut cmd = Command::new(&spec.exe);
    cmd.arg("-c")
        .arg(task)
        .arg("--coordinator")
        .arg("--run-id")
        .arg(run_id)
        .arg("--backend")
        .arg("claude")
        .arg("--model")
        .arg(&spec.model)
        .current_dir(&spec.cwd)
        // Nested-coordinator guard: an in-container/in-worker aish must never
        // spawn its own workers (no infinite recursion). The child reads this.
        .env("AISH_COORDINATOR", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
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
}

pub type WorkerJobs = Arc<Mutex<Vec<Arc<WorkerJob>>>>;

/// Everything the spawned run task needs, captured up front so it's
/// self-contained (mirrors how `batch::spawn` captures api_key/model).
pub struct WorkerSpec {
    /// The aish binary to re-exec (this process's own executable).
    pub exe: PathBuf,
    /// Working directory for the child — the session's cwd, so it sees the same
    /// project files and the same project `.mcp.json`.
    pub cwd: PathBuf,
    /// Model the child's coordinator turn runs on (Opus by default, like batches).
    pub model: String,
    /// Extra env for the child (the session's `~/.aishrc` exports), so MCP
    /// `${VAR}` interpolation resolves the same as it does here. The child also
    /// inherits the parent's process env (ANTHROPIC_API_KEY, ATUM_*, …).
    pub env: Vec<(String, String)>,
}

impl WorkerJob {
    fn set_done(&self, result: String) {
        let mut i = self.inner.lock().unwrap();
        i.status = "done".into();
        i.result = Some(result);
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
    let mut cmd = worker_command(&spec, &task, &job.id);

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
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
    let prefix = format!("[{}]", job.id);
    let collect = tokio::spawn(async move {
        tokio::join!(read_capped(stdout, CAPTURE_CAP), stream_stderr(stderr, &prefix))
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
    match status {
        Some(s) if s.success() => {
            let t = out.trim();
            job.set_done(if t.is_empty() { "(no output)".into() } else { t.to_string() });
        }
        Some(s) => job.set_failed(describe_failure(s, "worker", &err)),
        None => job.set_failed(format!(
            "worker timed out after {}s",
            WORKER_TIMEOUT.as_secs()
        )),
    }
    on_complete(&jobs, &job);
}

/// Run a single coordinator subprocess to completion and return its stdout (the
/// final answer). Unlike `spawn`, it doesn't register a tracked job or
/// auto-deliver — the caller consumes the output. Used by the goal loop for each
/// work step.
pub async fn run_once(spec: &WorkerSpec, task: &str, run_id: &str) -> Result<String, String> {
    let mut cmd = worker_command(spec, task, run_id);
    let mut child = cmd.spawn().map_err(|e| format!("couldn't launch goal worker: {e}"))?;
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");
    // stdout is the result (capped capture, unchanged). stderr is streamed live
    // to the user's terminal as `[goal] 🔧 …` tool-activity, retaining only a
    // bounded tail for the failure message — no unbounded accumulation.
    let collect = tokio::spawn(async move {
        tokio::join!(read_capped(stdout, CAPTURE_CAP), stream_stderr(stderr, "[goal]"))
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
            format!(
                "\x1b[2m{icon} {} {what} — `:result {}` to view · {}\x1b[0m",
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
