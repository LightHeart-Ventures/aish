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
    state: Mutex<JobState>,
    buffer: Mutex<String>,
    /// Channel that asks the background waiter task to kill the child. `None`
    /// for foreground jobs (the shell waits on them directly).
    kill: Mutex<Option<oneshot::Sender<()>>>,
    /// Process-group id the job leads (`pgid == leader pid`), recorded once the
    /// child is spawned. `None` until set. Used to signal the whole job on shell
    /// exit (SIGHUP/SIGCONT — TASK-123); the libc signalling lives in `tools`.
    pgid: Mutex<Option<libc::pid_t>>,
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
            state: Mutex::new(JobState::Running),
            buffer: Mutex::new(String::new()),
            kill: Mutex::new(Some(kill_tx)),
            pgid: Mutex::new(None),
        });
        (job, kill_rx)
    }

    /// A foreground job (the shell waits on it; no kill channel).
    pub fn foreground(id: usize, desc: String) -> Arc<Self> {
        Arc::new(Job {
            id,
            desc,
            kind: JobKind::Foreground,
            state: Mutex::new(JobState::Running),
            buffer: Mutex::new(String::new()),
            kill: Mutex::new(None),
            pgid: Mutex::new(None),
        })
    }

    /// Current state label: `"running"`, `"stopped"`, or the exit summary.
    pub fn status(&self) -> String {
        self.state.lock().unwrap().label()
    }

    /// Mark the job finished with the given exit summary.
    pub fn finish(&self, summary: impl Into<String>) {
        *self.state.lock().unwrap() = JobState::Done(summary.into());
    }

    /// True once the job has reached its terminal `Done` state.
    pub fn is_done(&self) -> bool {
        self.state.lock().unwrap().is_done()
    }

    /// True while the job is suspended (SIGTSTP / Ctrl-Z).
    pub fn is_stopped(&self) -> bool {
        matches!(*self.state.lock().unwrap(), JobState::Stopped)
    }

    /// Record the process group this job leads (`pgid == leader pid`). Set once,
    /// right after the child is spawned.
    pub fn set_pgid(&self, pgid: libc::pid_t) {
        *self.pgid.lock().unwrap() = Some(pgid);
    }

    /// The process group this job leads, if recorded.
    pub fn pgid(&self) -> Option<libc::pid_t> {
        *self.pgid.lock().unwrap()
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
        self.kill
            .lock()
            .unwrap()
            .take()
            .is_some_and(|tx| tx.send(()).is_ok())
    }
}

/// A POSIX job specifier as accepted by `jobs` / `fg` / `bg` / `wait`: either a
/// concrete job id (`%n` or a bare `n`) or the current job (`%%` / `%+`, also
/// the default when the operand is omitted).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JobSpec {
    /// The current job — `%%`, `%+`, or no operand at all.
    Current,
    /// A specific job id — `%n` or a bare `n`.
    Id(usize),
}

impl JobSpec {
    /// Parse one job-control operand. `%%`/`%+` select the current job; `%n` and
    /// a bare `n` select job `n`. Returns `None` for anything that isn't a valid
    /// specifier (so callers can report "no such job").
    pub fn parse(token: &str) -> Option<JobSpec> {
        match token {
            "%%" | "%+" => Some(JobSpec::Current),
            _ => token
                .strip_prefix('%')
                .unwrap_or(token)
                .parse::<usize>()
                .ok()
                .map(JobSpec::Id),
        }
    }
}

/// The "current job" (`%%`): the most recently added job that is still active.
/// A stopped job is preferred over a running one (matching the POSIX shells'
/// rule that the most recently stopped job is current); terminated jobs are
/// skipped. `None` when there is no active job.
pub fn current_job(table: &[Arc<Job>]) -> Option<Arc<Job>> {
    table
        .iter()
        .rev()
        .find(|j| j.is_stopped())
        .or_else(|| table.iter().rev().find(|j| !j.is_done()))
        .cloned()
}

/// Resolve a job specifier against the table. `None` (no operand) selects the
/// current job. Returns a human-readable error string on no match, which the
/// builtins surface as `<cmd>: <error>` and a non-zero `$?` (POSIX).
pub fn resolve_job(table: &[Arc<Job>], spec: Option<JobSpec>) -> Result<Arc<Job>, String> {
    match spec {
        None | Some(JobSpec::Current) => {
            current_job(table).ok_or_else(|| "no current job".to_string())
        }
        Some(JobSpec::Id(id)) => table
            .iter()
            .find(|j| j.id == id)
            .cloned()
            .ok_or_else(|| format!("no such job: {id}")),
    }
}

/// Render the job table the way `jobs` (and the `:jobs` meta-command) list it:
/// one `[id] <state> — <desc>` line per job, in table order. Empty when there
/// are no jobs. The single source of truth for both listing call sites.
pub fn format_listing(table: &[Arc<Job>]) -> Vec<String> {
    table
        .iter()
        .map(|j| format!("[{}] {} — {}", j.id, j.status(), j.desc))
        .collect()
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
            table.push(Job::foreground(2, "cargo build".into()));
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
        let fg = Job::foreground(1, "sleep 100".into());
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
            job.push_line(&format!(
                "line {i} ........................................"
            ));
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
        let fg = Job::foreground(1, "ls".into());
        assert!(!fg.kill());
    }

    // ── TASK-122: jobs / fg / bg / wait job selection + listing ──────────────

    #[test]
    fn job_spec_parses_posix_operands() {
        assert_eq!(JobSpec::parse("%%"), Some(JobSpec::Current));
        assert_eq!(JobSpec::parse("%+"), Some(JobSpec::Current));
        assert_eq!(JobSpec::parse("%1"), Some(JobSpec::Id(1)));
        assert_eq!(JobSpec::parse("3"), Some(JobSpec::Id(3)));
        // Not specifiers.
        assert_eq!(JobSpec::parse("%x"), None);
        assert_eq!(JobSpec::parse("foo"), None);
        assert_eq!(JobSpec::parse(""), None);
    }

    // AC ac_ea23cef2b000 — selecting the current job picks a stopped job over a
    // running one, and skips terminated jobs (POSIX "current job" rule).
    #[test]
    fn current_job_prefers_stopped_then_most_recent_active() {
        let table: Vec<Arc<Job>> = vec![
            Job::background(1, "running-old".into()).0,
            Job::background(2, "running-new".into()).0,
        ];
        // No stopped job → most recently added active job is current.
        assert_eq!(current_job(&table).unwrap().id, 2);

        // Stop the older job → it becomes current despite being added first.
        table[0].stop();
        assert_eq!(current_job(&table).unwrap().id, 1);

        // A finished job is never current.
        let done: Vec<Arc<Job>> = vec![Job::background(5, "done".into()).0];
        done[0].finish("exited 0");
        assert!(current_job(&done).is_none());
    }

    // AC ac_ea23cef2b000 — `%n` / bare id resolve to that job; an unknown id or
    // an empty table is a reportable error (non-zero `$?` in the builtins).
    #[test]
    fn resolve_job_by_id_default_and_errors() {
        let running = Job::background(1, "running".into()).0;
        let stopped = Job::background(2, "stopped".into()).0;
        stopped.stop();
        let table = vec![running, stopped];

        assert_eq!(resolve_job(&table, Some(JobSpec::Id(1))).unwrap().id, 1);
        // Default (no operand) is the current job — the stopped one here.
        assert_eq!(resolve_job(&table, None).unwrap().id, 2);
        assert!(resolve_job(&table, Some(JobSpec::Id(99))).is_err());
        assert!(resolve_job(&[], None).is_err());
    }

    // AC ac_ea23cef2b000 — `jobs` lists a running and a stopped job with their
    // POSIX state labels, shared verbatim with the `:jobs` meta-command.
    #[test]
    fn format_listing_shows_running_and_stopped_state() {
        let running = Job::background(1, "tail -f log".into()).0;
        let stopped = Job::background(2, "sleep 100".into()).0;
        stopped.stop();
        let lines = format_listing(&[running, stopped]);
        assert_eq!(lines[0], "[1] running — tail -f log");
        assert_eq!(lines[1], "[2] stopped — sleep 100");
        assert!(format_listing(&[]).is_empty());
    }
}
