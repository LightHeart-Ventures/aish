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

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tokio::process::Command;

/// How long a worker may run before it's killed. Generous — these are deferred,
/// possibly-long jobs — but bounded so a wedged child can't live forever.
const WORKER_TIMEOUT: Duration = Duration::from_secs(60 * 60); // 1h

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
    let mut cmd = Command::new(&spec.exe);
    cmd.arg("-c")
        .arg(&task)
        .arg("--coordinator")
        .arg("--run-id")
        .arg(&job.id)
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

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            job.set_failed(format!("couldn't launch worker subprocess: {e}"));
            on_complete(&jobs, &job);
            return;
        }
    };

    // Drain stdout and stderr concurrently (sequential reads can deadlock if the
    // child fills the other pipe's buffer). The child's stderr — spinners, tool
    // activity — is captured, not shown; only its stdout (the final answer) and,
    // on failure, its stderr matter to us.
    let mut stdout = child.stdout.take().expect("piped stdout");
    let mut stderr = child.stderr.take().expect("piped stderr");
    let collect = tokio::spawn(async move {
        let mut out = String::new();
        let mut err = String::new();
        let _ = tokio::join!(
            stdout.read_to_string(&mut out),
            stderr.read_to_string(&mut err),
        );
        (out, err)
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
        Some(s) => job.set_failed(format!(
            "worker exited unsuccessfully ({s}): {}",
            err.trim()
        )),
        None => job.set_failed(format!(
            "worker timed out after {}s",
            WORKER_TIMEOUT.as_secs()
        )),
    }
    on_complete(&jobs, &job);
}

/// Called when one worker finishes. While others run, print a brief progress
/// line; once all have finished, flush every not-yet-shown result at once.
/// Mirrors `batch::on_complete`.
fn on_complete(jobs: &WorkerJobs, finished: &Arc<WorkerJob>) {
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

/// Print every finished-but-not-yet-shown worker result over the prompt, then
/// mark them shown. Mirrors `batch::flush_results`.
fn flush_results(jobs: &WorkerJobs) {
    let pending: Vec<Arc<WorkerJob>> = {
        let g = jobs.lock().unwrap();
        g.iter().filter(|j| j.is_terminal() && !j.is_displayed()).cloned().collect()
    };
    if pending.is_empty() {
        return;
    }
    print!("\r\x1b[2K"); // wipe the prompt line the result is landing over
    for job in &pending {
        let label = if job.status() == "failed" { "failed" } else { "complete" };
        println!("\x1b[2m── worker {} {label} ──\x1b[0m", job.id);
        println!("{}", crate::md::render_stdout(job.fetch().trim()));
        job.mark_displayed();
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
}
