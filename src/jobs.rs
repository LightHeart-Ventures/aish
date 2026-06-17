//! Unified job model — the single source of truth for foreground and
//! background jobs and their lifecycle state (TASK-118 / S3.1).
//!
//! Before this module, background jobs lived in `tools.rs` as an ad-hoc
//! `Vec<Arc<Job>>` whose state was an implicit `Option<String>` (`None` =
//! running, `Some(summary)` = finished), and foreground commands were not
//! tracked at all. Both kinds of job now share one representation, one
//! registry ([`Jobs`]), and one explicit `running` / `stopped` / `done`
//! state machine ([`JobState`]).

use std::sync::{Arc, Mutex};
use tokio::sync::oneshot;

/// Bytes of streamed output retained per job (oldest output is dropped).
const JOB_BUFFER_CAP: usize = 64_000;

/// Whether the shell waits on the job (foreground) or it runs detached with
/// its output streaming to the user (background).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JobKind {
    Foreground,
    Background,
}

/// Lifecycle state of a job. `Done` carries the human-readable exit summary
/// (e.g. `"exited 0"`, `"killed"`) surfaced to the model and `:jobs`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum JobState {
    Running,
    Stopped,
    Done(String),
}

impl JobState {
    /// Label shown by `:jobs` and `job_output`. Preserves the pre-TASK-118
    /// strings exactly: `"running"` while live and the exit summary once done.
    pub fn label(&self) -> String {
        match self {
            JobState::Running => "running".into(),
            JobState::Stopped => "stopped".into(),
            JobState::Done(summary) => summary.clone(),
        }
    }

    /// True once the job has reached `Done`.
    pub fn is_done(&self) -> bool {
        matches!(self, JobState::Done(_))
    }
}

/// One tracked job — foreground or background — with its captured output and
/// current lifecycle state.
pub struct Job {
    pub id: usize,
    pub desc: String,
    pub kind: JobKind,
    /// Process-group leader pid (`pgid == pid`) of a foreground job, used by
    /// `fg`/`bg` to deliver SIGCONT when resuming a Ctrl-Z-stopped job
    /// (TASK-121). `None` for background jobs — they are reaped by their own
    /// waiter task and are never suspended to the prompt.
    pid: Option<i32>,
    state: Mutex<JobState>,
    buffer: Mutex<String>,
    /// Channel that asks the background waiter task to kill the child. `None`
    /// for foreground jobs (the shell waits on them directly).
    kill: Mutex<Option<oneshot::Sender<()>>>,
}

/// The unified job table: the single source of truth for every foreground and
/// background job in a session.
pub type Jobs = Arc<Mutex<Vec<Arc<Job>>>>;

impl Job {
    /// A background job plus the receiver its waiter task uses to learn it
    /// should kill the child.
    pub fn background(id: usize, desc: String) -> (Arc<Self>, oneshot::Receiver<()>) {
        let (kill_tx, kill_rx) = oneshot::channel();
        let job = Arc::new(Job {
            id,
            desc,
            kind: JobKind::Background,
            pid: None,
            state: Mutex::new(JobState::Running),
            buffer: Mutex::new(String::new()),
            kill: Mutex::new(Some(kill_tx)),
        });
        (job, kill_rx)
    }

    /// A foreground job (the shell waits on it; no kill channel). `pid` is the
    /// child's process-group leader so `fg`/`bg` can SIGCONT it on resume.
    pub fn foreground(id: usize, desc: String, pid: i32) -> Arc<Self> {
        Arc::new(Job {
            id,
            desc,
            kind: JobKind::Foreground,
            pid: Some(pid),
            state: Mutex::new(JobState::Running),
            buffer: Mutex::new(String::new()),
            kill: Mutex::new(None),
        })
    }

    /// Current state label: `"running"`, `"stopped"`, or the exit summary.
    pub fn status(&self) -> String {
        self.state.lock().unwrap().label()
    }

    /// The process-group leader pid for a foreground job, used to deliver
    /// SIGCONT on `fg`/`bg` resume (TASK-121). `None` for background jobs.
    pub fn pid(&self) -> Option<i32> {
        self.pid
    }

    /// Mark the job finished with the given exit summary.
    pub fn finish(&self, summary: impl Into<String>) {
        *self.state.lock().unwrap() = JobState::Done(summary.into());
    }

    /// Suspend a running job (SIGTSTP / Ctrl-Z). No-op once finished.
    pub fn stop(&self) {
        let mut state = self.state.lock().unwrap();
        if !state.is_done() {
            *state = JobState::Stopped;
        }
    }

    /// Resume a stopped job (SIGCONT). No-op unless currently stopped.
    pub fn resume(&self) {
        let mut state = self.state.lock().unwrap();
        if matches!(*state, JobState::Stopped) {
            *state = JobState::Running;
        }
    }

    /// Append a line of streamed output, dropping the oldest bytes once the
    /// retained buffer exceeds [`JOB_BUFFER_CAP`].
    pub fn push_line(&self, line: &str) {
        let mut buf = self.buffer.lock().unwrap();
        buf.push_str(line);
        buf.push('\n');
        if buf.len() > JOB_BUFFER_CAP {
            let mut cut = buf.len() - JOB_BUFFER_CAP;
            while !buf.is_char_boundary(cut) {
                cut += 1;
            }
            buf.drain(..cut);
        }
    }

    /// A snapshot of the retained output.
    pub fn output(&self) -> String {
        self.buffer.lock().unwrap().clone()
    }

    /// Ask the background waiter to kill the child. Returns false when there is
    /// no live kill channel (already signalled, or a foreground job).
    pub fn kill(&self) -> bool {
        self.kill.lock().unwrap().take().is_some_and(|tx| tx.send(()).is_ok())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_labels_match_legacy_strings() {
        assert_eq!(JobState::Running.label(), "running");
        assert_eq!(JobState::Stopped.label(), "stopped");
        assert_eq!(JobState::Done("exited 0".into()).label(), "exited 0");
    }

    // AC ac_d124555bbca4 — one table is the single source of truth for both
    // foreground and background jobs, each reporting state via the same model.
    #[test]
    fn one_table_is_source_of_truth_for_both_kinds() {
        let jobs: Jobs = Default::default();
        {
            let mut table = jobs.lock().unwrap();
            let (bg, _kill_rx) = Job::background(1, "tail -f log".into());
            table.push(bg);
            table.push(Job::foreground(2, "cargo build".into(), 1234));
        }
        let table = jobs.lock().unwrap();
        assert_eq!(table.len(), 2);
        let kinds: Vec<JobKind> = table.iter().map(|j| j.kind).collect();
        assert!(kinds.contains(&JobKind::Background));
        assert!(kinds.contains(&JobKind::Foreground));
        // Every job — regardless of kind — reports its state the same way.
        assert!(table.iter().all(|j| j.status() == "running"));
    }

    // AC ac_d124555bbca4 — the running/stopped/done machine is shared by both
    // kinds and rejects illegal transitions out of the terminal state.
    #[test]
    fn running_stopped_done_transitions() {
        let fg = Job::foreground(1, "sleep 100".into(), 1234);
        assert_eq!(fg.status(), "running");
        fg.stop();
        assert_eq!(fg.status(), "stopped");
        fg.resume();
        assert_eq!(fg.status(), "running");
        fg.finish("exited 0");
        assert_eq!(fg.status(), "exited 0");
        // Terminal: stop/resume are no-ops once done.
        fg.stop();
        assert_eq!(fg.status(), "exited 0");
        fg.resume();
        assert_eq!(fg.status(), "exited 0");
    }

    // AC ac_3a59c5860222 — background status + output capture behave exactly as
    // before the unification.
    #[test]
    fn background_status_and_output_preserved() {
        let (job, _kill_rx) = Job::background(7, "server".into());
        assert_eq!(job.kind, JobKind::Background);
        assert_eq!(job.status(), "running");
        assert_eq!(job.output(), "");

        job.push_line("listening on :8080");
        job.push_line("request 1");
        assert_eq!(job.output(), "listening on :8080\nrequest 1\n");

        job.finish("exited 0");
        assert_eq!(job.status(), "exited 0");
    }

    // AC ac_3a59c5860222 — the retained buffer is still capped, dropping the
    // oldest output while keeping the most recent.
    #[test]
    fn background_output_buffer_is_capped() {
        let (job, _kill_rx) = Job::background(1, "noisy".into());
        for i in 0..5000 {
            job.push_line(&format!("line {i} ........................................"));
        }
        assert!(job.output().len() <= JOB_BUFFER_CAP);
        assert!(job.output().contains("line 4999"));
    }

    // AC ac_3a59c5860222 — kill() signals the waiter once, then reports spent
    // (matching the pre-TASK-118 "False when already finished" semantics).
    #[test]
    fn background_kill_signals_once() {
        let (job, _kill_rx) = Job::background(1, "daemon".into());
        assert!(job.kill());
        assert!(!job.kill());
    }

    // Foreground jobs have no kill channel — the shell waits on them directly.
    #[test]
    fn foreground_has_no_kill_channel() {
        let fg = Job::foreground(1, "ls".into(), 1234);
        assert!(!fg.kill());
    }
}
