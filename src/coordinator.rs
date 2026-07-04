//! Durable, resumable background coordinator — the DEFAULT background path.
//!
//! Ported from atum_cli's batch-coordinator (`batch-coordinator.ts` /
//! `batch-controller-{plan,round}.ts` / `batch-controller-store.ts`). Where
//! `batch.rs` offloads ONE tool-less request to the Anthropic Batches API, the
//! coordinator is a multi-round agentic loop that:
//!   * runs full-tool turns locally (filesystem, run_program, MCP), AND
//!   * fans heavy, latency-insensitive sub-work out to the Batches API,
//! persisting its phase to SQLite so a crash/exit resumes instead of re-running.
//!
//! ## Phase state machine (borrowed from atum's `runCoordinator`)
//! ```text
//!                ┌──────────────┐  spawn batch   ┌────────────────┐
//!   start ─────► │ coordinating │ ─────────────► │ awaiting_batch │
//!                └──────────────┘ ◄───────────── └────────────────┘
//!                   │       ▲       fold results
//!          done     │       │ (loop another round)
//!                   ▼       │
//!                ┌──────┐   │  cap / error
//!                │ done │   └──────────► failed
//!                └──────┘
//! ```
//! `coordinating` — running agentic turns; the default resting phase.
//! `awaiting_batch` — blocked on a spawned Batches job; heartbeats while it polls.
//! `checkpoint` — a deliberate, resumable PAUSE (TASK-294): a parent asked the
//!   run to halt at the next round boundary WITHOUT finishing. Unlike stand-down
//!   (which ends the run) `drive` persists this phase and returns, leaving the
//!   transcript/worktree intact for a later manual resume. Non-terminal, and
//!   intentionally exempt from orphan reaping.
//! `done` / `failed` — terminal. A `done` row is returned idempotently on resume.
//!
//! ## Operator mid-flight messaging (the `:tell` / SendMessage channel)
//! The interactive session can steer a running coordinator without killing and
//! re-launching it: `:tell <run-id> <message>` enqueues a row in the durable
//! `coordinator_messages` mailbox (see `db::CoordinatorStore`). At each round
//! boundary `drive` drains the mailbox for its `run_id` and folds the messages
//! into the next turn as an operator interjection, so updated instructions or
//! clarifications reach the model on its very next round. Delivery is
//! round-boundary (a message sent mid-turn lands at the next round), and because
//! the mailbox is durable it survives a restart and works across sessions.
//!
//! ## Worker-exit evaluation (auto-resume / nudge / flag-for-operator)
//! `engine::run_turn` no longer spins the whole iteration budget away and throws
//! the work out: it tags an abnormal stop with a greppable
//! [`crate::loopguard::ExitReason`] banner on the first line of its answer
//! (`loop-detected` / `forced-summarize` / `budget-exhausted`). After each round
//! `drive` reads that banner and picks a [`crate::loopguard::Disposition`]:
//!   * **resume** an out-of-budget stop — drive another round with a "continue,
//!     don't redo completed work" directive (the work so far is preserved in
//!     history / the turn-audit replay);
//!   * **nudge** a confirmed loop — feed a change-approach directive instead of
//!     blindly resuming the same path;
//!   * **flag for the operator** once auto-recovery is spent — stop and record a
//!     clear failure so a human can take over.
//! Auto-recoveries are capped ([`crate::loopguard::MAX_AUTO_RECOVERIES`]) so the
//! recovery itself can't become an infinite loop.
//!
//! ## What aish adapts vs. atum
//! atum injects the agent step + a real Batches client as seams and runs in a
//! container with an external orchestrator lease. aish has neither: a single
//! agentic turn IS `engine::run_turn` (which itself executes tools locally and,
//! when batch-mode tools fire, spawns Batches jobs into `session.batch_jobs`).
//! So aish's "round" = one `run_turn` followed by awaiting any batches that turn
//! spawned. We don't reproduce atum's separate `round`/`plan` tables — the model
//! drives plan→map→reduce inside its own transcript; we persist the coarse phase
//! (the irreducible resume signal) and a heartbeat, which is the reviewable core.
//!
//! TODO(coordinator): atum reattaches to a specific in-flight Anthropic batch id
//! across a process restart (per-round `batchId` persisted before polling). aish
//! already does that reattach for top-level batches via `batch::rehydrate`, but a
//! coordinator that *crashes mid-round* loses its in-memory transcript, so on
//! restart we surface/reap the run rather than resuming its conversation. Full
//! transcript persistence (atum's `saveStep`) is the next increment.

use crate::backend::Backend;
use crate::db::CoordinatorStore;
use crate::session::Session;
use crate::tools;
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// Cross-turn interrupt latch for a running coordinator. Set from an async
/// SIGINT handler installed in [`drive`] (Ctrl-C at the interactive prompt is
/// forwarded to an `:attach`ed worker's process group as SIGINT — see
/// `repl.rs`), read+cleared at the top of every engine turn iteration
/// (`engine::run_turn`). It is process-global because the coordinator runs one
/// `drive` loop per process; interactive sessions never install the handler, so
/// the latch is never set there and the engine seam is a no-op.
static INTERRUPT: AtomicBool = AtomicBool::new(false);

/// Signal that the operator interrupted the current turn. Called from the
/// SIGINT handler task; idempotent.
pub fn request_interrupt() {
    INTERRUPT.store(true, Ordering::SeqCst);
}

/// Read AND clear the interrupt latch. Returns `true` exactly once per
/// interrupt so a single Ctrl-C stops a single turn, not every turn after it.
pub fn take_interrupt() -> bool {
    INTERRUPT.swap(false, Ordering::SeqCst)
}

/// Default upper bound on agentic rounds (a misbehaving model can't spin
/// forever). Mirrors atum's `DEFAULT_MAX_STEPS` backstop, scaled for a shell
/// session. Bounded but generous: real multi-file work (rewrite a crate,
/// iterate to a green build) needs many rounds, and the parent-side stdout
/// capture is already capped at 1MB (`worker::read_capped`), so rounds aren't
/// the OOM lever — a runaway still terminates here. Most work completes well
/// under this.
///
/// Overridable at runtime via `AISH_COORDINATOR_MAX_ROUNDS` — a deliberately
/// *non-durable* bandaid (per the loop-exhaustion review): when a legitimate
/// task is genuinely starved by the cap you can lift it without a rebuild, but
/// the real fix is fewer wasted rounds (the circuit breaker + decision-point
/// prompt below, and richer context upstream), not a bigger number.
const DEFAULT_MAX_ROUNDS: usize = 48;

/// Pre-dispatch circuit breaker (loop guard): refuse to start a *new* run when
/// this many prior runs of the SAME task have already terminated in `failed`.
/// A task that has failed this often is unlikely to succeed on yet another
/// identical attempt — failing fast saves a whole multi-round burn. Overridable
/// via `AISH_COORDINATOR_MAX_FAILED_ATTEMPTS`; `0` disables the gate.
const DEFAULT_MAX_FAILED_ATTEMPTS: usize = 3;

/// The effective round cap for this run (env override, clamped, else default).
fn max_rounds() -> usize {
    env_usize("AISH_COORDINATOR_MAX_ROUNDS", DEFAULT_MAX_ROUNDS, 1, 1000)
}

/// The effective failed-attempt circuit-breaker threshold (env override,
/// clamped, else default). `0` means the gate is disabled.
fn max_failed_attempts() -> usize {
    env_usize(
        "AISH_COORDINATOR_MAX_FAILED_ATTEMPTS",
        DEFAULT_MAX_FAILED_ATTEMPTS,
        0,
        1000,
    )
}

/// Bounded retention for terminal `failed` runs (coordinator-lifecycle bug #129
/// item 5). `clear_finished` now KEEPS `failed` rows so a reaped/errored run
/// stays inspectable instead of vanishing; this caps how many survive so the
/// table can't grow without bound. Keep at most the `KEEP` most-recent failed
/// rows, and drop any failed row older than `MAX_AGE_DAYS`. Both are overridable
/// at runtime (`AISH_COORDINATOR_FAILED_KEEP`,
/// `AISH_COORDINATOR_FAILED_MAX_AGE_DAYS`).
const DEFAULT_FAILED_RETENTION_KEEP: usize = 50;
const DEFAULT_FAILED_RETENTION_MAX_AGE_DAYS: usize = 14;

/// Effective keep-recent bound for `failed` rows (env override, clamped). `0`
/// keeps none (every failed row is eligible to be reaped by count).
fn failed_retention_keep() -> usize {
    env_usize(
        "AISH_COORDINATOR_FAILED_KEEP",
        DEFAULT_FAILED_RETENTION_KEEP,
        0,
        100_000,
    )
}

/// Effective max age (in seconds) for a retained `failed` row (env override is
/// in days, clamped). A failed row older than this is reaped regardless of the
/// keep-recent count.
fn failed_retention_max_age_secs() -> i64 {
    let days = env_usize(
        "AISH_COORDINATOR_FAILED_MAX_AGE_DAYS",
        DEFAULT_FAILED_RETENTION_MAX_AGE_DAYS,
        0,
        3650,
    );
    days as i64 * 86_400
}

/// Pure bounded-retention decision for terminal `failed` runs. Given each failed
/// run's `(run_id, created_at_secs)` (a `None` timestamp is treated as the
/// oldest — and so always eligible to reap), return the run ids to DELETE so
/// that at most `keep_recent` of the MOST-RECENT rows survive AND no surviving
/// row is older than `max_age_secs`. Rows are ordered newest-first by
/// `created_at` (ties broken by run_id desc for determinism); a row is reaped
/// when it falls beyond the keep window OR exceeds the age bound. The result
/// order is newest→oldest among victims. Order-independent and idempotent
/// (re-running on the survivors returns an empty plan). (coordinator-lifecycle
/// bug #129 item 5.)
fn failed_retention_plan(
    rows: &[(String, Option<i64>)],
    now_secs: i64,
    keep_recent: usize,
    max_age_secs: i64,
) -> Vec<String> {
    let mut ordered: Vec<&(String, Option<i64>)> = rows.iter().collect();
    ordered.sort_by(|a, b| {
        b.1.unwrap_or(i64::MIN)
            .cmp(&a.1.unwrap_or(i64::MIN))
            .then_with(|| b.0.cmp(&a.0))
    });
    let mut victims = Vec::new();
    for (idx, (run_id, created)) in ordered.iter().enumerate() {
        let beyond_keep = idx >= keep_recent;
        let too_old = match created {
            Some(secs) => now_secs.saturating_sub(*secs) > max_age_secs,
            None => true, // no timestamp → can't prove it's fresh → reap-eligible
        };
        if beyond_keep || too_old {
            victims.push((*run_id).clone());
        }
    }
    victims
}

/// Apply bounded retention to the store's terminal `failed` rows using the
/// runtime knobs + wall clock. Best-effort; returns the count reaped. Thin
/// wrapper over the deterministic [`reap_failed_runs_with`].
fn reap_failed_runs(store: &CoordinatorStore) -> usize {
    reap_failed_runs_with(
        store,
        failed_retention_keep(),
        failed_retention_max_age_secs(),
        now_unix_secs(),
    )
}

/// Testable core of [`reap_failed_runs`]: load the store's `failed` rows, decide
/// the bounded-retention victims via [`failed_retention_plan`], and delete them.
/// Parameterized on the knobs + `now_secs` so it's deterministic under test (no
/// env / wall-clock dependence). Best-effort; a store read/write error yields 0.
fn reap_failed_runs_with(
    store: &CoordinatorStore,
    keep_recent: usize,
    max_age_secs: i64,
    now_secs: i64,
) -> usize {
    let rows = match store.load_all() {
        Ok(r) => r,
        Err(_) => return 0,
    };
    let failed: Vec<(String, Option<i64>)> = rows
        .into_iter()
        .filter(|r| Phase::parse(&r.phase) == Phase::Failed)
        .map(|r| {
            (
                r.run_id,
                r.created_at.as_deref().and_then(parse_sqlite_timestamp),
            )
        })
        .collect();
    let plan = failed_retention_plan(&failed, now_secs, keep_recent, max_age_secs);
    store.delete_runs(&plan).unwrap_or(0)
}

/// Bounded retention for terminal `done` rows — the mirror of
/// [`reap_failed_runs`] for the completed side. Only exercised when the startup
/// digest is SUPPRESSED (the default): `rehydrate` then KEEPS `done` rows
/// (instead of clearing them the moment they're loaded) so a completed
/// background result stays retrievable via `:workers all` / `background_status`
/// / `:result <id>` rather than being surfaced-and-dropped over the prompt.
/// Without this the kept rows would accumulate on every restart, so we apply the
/// same keep-recent + max-age window used for `failed` rows. Best-effort; a
/// store read/write error yields 0.
fn reap_done_runs(store: &CoordinatorStore) -> usize {
    let keep = failed_retention_keep();
    let max_age = failed_retention_max_age_secs();
    let now = now_unix_secs();
    let rows = match store.load_all() {
        Ok(r) => r,
        Err(_) => return 0,
    };
    let done: Vec<(String, Option<i64>)> = rows
        .into_iter()
        .filter(|r| Phase::parse(&r.phase) == Phase::Done)
        .map(|r| {
            (
                r.run_id,
                r.created_at.as_deref().and_then(parse_sqlite_timestamp),
            )
        })
        .collect();
    let plan = failed_retention_plan(&done, now, keep, max_age);
    store.delete_runs(&plan).unwrap_or(0)
}

/// Parse a truthy/falsey flag string (`1/true/on/yes` → true, `0/false/off/no`
/// → false); anything else is `None`. Shared by the `AISH_STARTUP_DIGEST` env
/// override and the `:startup-digest` REPL toggle.
pub fn parse_flag(s: &str) -> Option<bool> {
    match s.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "on" | "yes" | "y" => Some(true),
        "0" | "false" | "off" | "no" | "n" => Some(false),
        _ => None,
    }
}

/// Whether the verbose startup coordinator digest is shown at boot — the
/// completed-result walls, the per-salvage lines, and the `reattached
/// coordinator runs (…)` summary. Suppressed by DEFAULT so a fresh terminal
/// doesn't open onto a wall of prior workers' output. Re-enable per-invocation
/// with the `AISH_STARTUP_DIGEST` env var (truthy), or durably with
/// `:startup-digest on` (persisted `startup_digest` setting). The env override
/// wins over the persisted setting; absent both, the default is `false`.
fn startup_digest_enabled(session: &Session) -> bool {
    if let Ok(v) = std::env::var("AISH_STARTUP_DIGEST") {
        if let Some(b) = parse_flag(&v) {
            return b;
        }
    }
    if let Some(db) = session.db.as_ref() {
        if let Ok(Some(v)) = db.get_setting("startup_digest") {
            return parse_flag(&v).unwrap_or(false);
        }
    }
    false
}

/// Read a `usize` from environment variable `var`, accept it only when it parses
/// and falls within `[min, max]`, otherwise fall back to `default`. Keeps the
/// runtime knobs above forgiving: a typo'd or wild value silently reverts to the
/// safe default rather than uncapping (or zero-capping) the coordinator. The
/// parse/clamp decision is the pure [`clamp_usize`], so it's unit-testable
/// without mutating process env (unsafe under edition 2024).
fn env_usize(var: &str, default: usize, min: usize, max: usize) -> usize {
    clamp_usize(std::env::var(var).ok(), default, min, max)
}

/// Pure parse-and-clamp: `raw` (a possibly-absent env value) is accepted only
/// when it parses to a `usize` inside `[min, max]`; anything else yields
/// `default`. Leading/trailing whitespace is tolerated.
fn clamp_usize(raw: Option<String>, default: usize, min: usize, max: usize) -> usize {
    raw.and_then(|v| v.trim().parse::<usize>().ok())
        .filter(|&n| n >= min && n <= max)
        .unwrap_or(default)
}

/// How often to beat the run's durable heartbeat while awaiting batches — proof
/// of liveness so a restart can tell a live run from an orphaned one. Matches
/// atum's `DEFAULT_HEARTBEAT_INTERVAL_MS`.
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(30);

/// A run is considered orphaned at startup when its owner is gone and its last
/// heartbeat is older than this. Generous so a momentarily-paused awaiting run
/// (a long batch poll) is never falsely reaped.
const ORPHAN_STALE_AFTER: Duration = Duration::from_secs(15 * 60);

/// The coordinator's phase — the durable resume signal. String-backed in SQLite
/// (the `coordinator_runs.phase` CHECK constraint), mirrored here as a type so
/// the transition logic is total and testable (atum keeps it an open string;
/// aish closes the set since the phases are fixed).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// Running agentic turns — the default resting phase.
    Coordinating,
    /// Blocked on a spawned Batches job; heartbeating while it polls.
    AwaitingBatch,
    /// A deliberate, resumable PAUSE (TASK-294): a parent requested a halt at the
    /// round boundary. Non-terminal — the run stops without finishing and can be
    /// resumed manually later. Never orphan-reaped.
    Checkpoint,
    /// Terminal: the task finished, `result` holds the assembled output.
    Done,
    /// Terminal: the run hit the round cap, errored, or was orphaned.
    Failed,
}

impl Phase {
    /// The SQLite string form (the `coordinator_runs.phase` column values).
    pub fn as_str(self) -> &'static str {
        match self {
            Phase::Coordinating => "coordinating",
            Phase::AwaitingBatch => "awaiting_batch",
            Phase::Checkpoint => "checkpoint",
            Phase::Done => "done",
            Phase::Failed => "failed",
        }
    }

    /// Parse a stored phase string. Unknown/legacy values map to `Failed` so a
    /// row we can't interpret is treated as a dead run, never resumed blindly.
    pub fn parse(s: &str) -> Phase {
        match s {
            "coordinating" => Phase::Coordinating,
            "awaiting_batch" => Phase::AwaitingBatch,
            "checkpoint" => Phase::Checkpoint,
            "done" => Phase::Done,
            _ => Phase::Failed,
        }
    }

    /// Terminal phases never transition again.
    // Part of the phase-machine's documented surface and exercised by the resume
    // contract test; the in-process resume increment (see the module TODO) is the
    // first non-test caller. `allow(dead_code)` keeps it without churn until then.
    #[allow(dead_code)]
    pub fn is_terminal(self) -> bool {
        matches!(self, Phase::Done | Phase::Failed)
    }

    /// Whether a *resumed* run in this phase can keep running, or is finished.
    /// Borrowed from atum's resume contract: `done` → return stored result;
    /// `coordinating`/`awaiting_batch` → continue; `failed` → terminal.
    #[allow(dead_code)]
    pub fn is_resumable(self) -> bool {
        matches!(
            self,
            Phase::Coordinating | Phase::AwaitingBatch | Phase::Checkpoint
        )
    }
}

/// Outcome of driving a coordinator run to a terminal state. Mirrors atum's
/// `CoordinatorResult`.
pub struct Outcome {
    pub phase: Phase,
    pub result: Option<String>,
    pub error: Option<String>,
    /// Rounds executed before reaching a terminal phase — informative for logs
    /// and the resume increment; not yet surfaced by the headless caller.
    #[allow(dead_code)]
    pub rounds: usize,
}

/// Render queued operator messages as an interjection block prepended to the
/// next turn's input. The framing tells the model these are updated, supervisory
/// instructions sent mid-run — they take precedence over earlier assumptions
/// where they conflict — so a clarification actually redirects the work rather
/// than being read as stale context. Pure, so it's unit-testable.
fn format_interjection(messages: &[String]) -> String {
    let body = messages
        .iter()
        .map(|m| format!("- {}", m.trim()))
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "[Operator interjection — the human supervising this run sent you the message(s) below \
mid-flight. Treat them as updated instructions/clarifications and fold them into your remaining \
work; where they conflict with an earlier assumption, the interjection wins:]\n{body}"
    )
}

/// Drain any operator messages queued for `run_id` and, when present, fold them
/// into `next_input` as an interjection (see `format_interjection`). Returns the
/// count folded so callers can emit a notice. A no-op (returns 0) when there's no
/// store or nothing queued. Best-effort: a store error is swallowed (the run must
/// not die because the mailbox read hiccuped).
fn fold_operator_messages(
    store: Option<&CoordinatorStore>,
    run_id: &str,
    next_input: &mut String,
) -> usize {
    let Some(s) = store else {
        return 0;
    };
    let msgs = s.drain_messages(run_id).unwrap_or_default();
    if msgs.is_empty() {
        return 0;
    }
    let interjection = format_interjection(&msgs);
    // Prepend so the operator's steer is the first thing the model reads this
    // round, ahead of the fold-results / task continuation text.
    *next_input = format!("{interjection}\n\n{next_input}");
    msgs.len()
}

/// Drive a coordinator run to a terminal state, persisting phase transitions to
/// `store` so a restart resumes. This is the headless `--coordinator` body
/// (called by `engine::run_coordinator`): it runs full-tool agentic rounds and,
/// after each round, awaits any Anthropic batches that round spawned (folding
/// them back is implicit — their results auto-print and the next round's turn
/// sees them in history). A round that spawns no batch and produces a final text
/// answer ends the run.
///
/// Adapted from atum's `runCoordinator` loop, collapsed to aish's model where a
/// single `run_turn` IS the agent step and batch fan-out happens inside it.
pub async fn drive(
    backend: &Backend,
    session: &mut Session,
    input: String,
    run_id: &str,
    store: Option<&CoordinatorStore>,
) -> Outcome {
    // Pin the verbatim task into the system prompt so it survives every history
    // compaction for the whole run (see `Session::task_anchor`). The first turn's
    // `next_input` below also carries the task, but that message is conversational
    // history — the earliest thing `crate::context` offloads when the window fills
    // — after which only a "[Context compacted: …]" banner remains. The anchored
    // copy lives in the never-compacted system prompt, so the worker keeps its
    // assignment in front of it no matter how long it runs.
    session.task_anchor = Some(input.clone());

    // ── Operator interrupt (Ctrl-C forwarding). By default SIGINT terminates
    // the process; a coordinator must instead treat it as "interrupt the
    // current turn and reassess" so a live worker survives a Ctrl-C from an
    // `:attach`ed interactive session (which forwards SIGINT to this process
    // group — see repl.rs). Install an async handler that latches the interrupt
    // flag; the engine turn loop reads+clears it at its next iteration seam and
    // ends the turn with an `interrupted` banner, which the drive loop below
    // turns into a reassess round. Best-effort: if the handler can't be
    // installed we simply keep the default SIGINT behavior.
    if let Ok(mut sigint) =
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())
    {
        tokio::spawn(async move {
            while sigint.recv().await.is_some() {
                request_interrupt();
                eprintln!("\x1b[2maish: SIGINT — interrupting current turn\x1b[0m");
            }
        });
    }

    if let Some(s) = store {
        // session.session_id/name were adopted from the LAUNCHING session at
        // startup (see main.rs), so the row attributes to who asked for the work.
        let _ = s.insert(run_id, &input, &session.session_id, session.name.as_deref());
        // TASK-289: record this live coordinator PROCESS in the durable registry
        // so a parent-death restart can reap our pid (and, once TASK-291 lands,
        // resume an in-flight Batches job). `coord_id` == `run_id`; generation 0
        // on first start; no batch job yet (the resume path stamps it later);
        // phase mirrors the freshly-inserted `coordinating` row.
        let _ = s.register_run(
            run_id,
            0,
            std::process::id() as i64,
            None,
            "coordinating",
            Some(&session.session_id),
        );
    }

    // ── Pre-dispatch circuit breaker (loop guard, per the loop-exhaustion
    // review). If this exact task has already failed `max_failed_attempts()`
    // times, a fresh identical run is very unlikely to fare differently —
    // refuse fast instead of burning another full multi-round attempt on a
    // known-bad request. The current run's own row (just inserted as
    // `coordinating`) is never counted; only prior `failed` rows are. The
    // counter now persists across restarts within the failed-row retention
    // window (`clear_finished` keeps `failed` rows; `reap_failed_runs` bounds
    // them — #129 item 5), so a task that keeps failing stays known-bad until
    // its failed rows age/count out, not just for the session. Disabled when the
    // threshold is 0.
    if let Some(s) = store {
        let cap = max_failed_attempts();
        if cap > 0 {
            let prior = s.failed_attempts(&input).unwrap_or(0) as usize;
            if prior >= cap {
                let error = format!(
                    "pre-dispatch circuit breaker: this task already failed {prior} time(s) (cap {cap}) — refusing to re-dispatch a known-bad request. Change the task, or raise/clear AISH_COORDINATOR_MAX_FAILED_ATTEMPTS to override."
                );
                eprintln!("\x1b[2maish: {error}\x1b[0m");
                let _ = s.set_failed(run_id, &error);
                return Outcome {
                    phase: Phase::Failed,
                    result: None,
                    error: Some(error),
                    rounds: 0,
                };
            }
        }
    }

    // Tier‑1 turn audit: attach (or re‑open, on a resume) the append‑only
    // tool journal at `.atum/run-${run_id}.jsonl` inside the worktree. On a
    // reconnect this recovers the completed turns so `engine::run_turn` replays
    // them instead of re‑executing side‑effecting tool calls. Best‑effort:
    // attach never fails (an unopenable journal degrades to a no‑op).
    let audit = crate::turn_audit::TurnAudit::attach(&session.cwd, run_id);
    if let Some(summary) = audit.resume_summary() {
        eprintln!("\x1b[2maish: {summary}\x1b[0m");
    }
    session.turn_audit = Some(audit);

    // S9.3: per-worker conversation store. Record a `running` meta.json and
    // attach the transcript WRITER so `engine::run_turn` persists each turn-event
    // (user message, tool call/result, narration) and this loop each round’s
    // synthesis to ~/.aish/workers/<run_id>/. The store is keyed by the run id —
    // the SAME host path worker.rs mounts at /aish/state — so the host reader and
    // an in-container writer share one dir. On a RESUME the writer continues the
    // seq past what is already on disk. Best-effort throughout: a write error is
    // swallowed so the store never sinks a live worker.
    {
        let mut meta = crate::worker_store::WorkerMeta::new(
            run_id,
            &session.session_id,
            &input,
            &worker_store_repo_key(&session.cwd),
            backend.kind(),
            &backend.model(),
            run_id,
        );
        meta.branch = current_worktree_branch(&session.cwd);
        let _ = crate::worker_store::write_meta_atomic(&meta);
    }
    session.worker_transcript = Some(crate::worker_store::TranscriptWriter::attach(run_id));

    let mut rounds = 0usize;
    // How many times we've auto-recovered (resume/nudge) a worker that ended
    // abnormally this run. Bounded by `loopguard::MAX_AUTO_RECOVERIES` so the
    // recovery can't itself spin forever — past the cap we flag the operator.
    let mut auto_recoveries = 0usize;
    // Read the round cap once per run so a single env value governs the whole
    // loop (a mid-run env change can't move the goalposts under us).
    let round_cap = max_rounds();
    // The coordinator's model HAS the full toolset (run_turn passes tool_defs),
    // but a model handed a big task headless sometimes rationalizes "I'm a
    // text-only assistant without file access" and refuses on turn 1 instead of
    // calling read_file. Lead the first turn with an explicit assertion of its
    // capabilities to head that off; later rounds use the fold-results message.
    //
    // The DECISION POINTS block is an explicit anti-loop directive (per the
    // loop-exhaustion review): it tells the model to stop re-trying the same
    // failing approach and instead declare a concrete blocker, which is a
    // *successful* terminal outcome here — spinning is not.
    //
    // The RE-EVALUATE THE PLAN AFTER TRIAGE block is a companion anti-loop
    // directive aimed at *over-decomposition* rather than repetition (per the
    // fan-out review): a coordinator that has already collapsed a problem to one
    // root cause during triage should NOT still fire the parallel fan-out it
    // pre-planned. It tells the model to re-check the plan before dispatching —
    // root cause found → stop parallelizing — and to `tell`-narrow/cancel a
    // redundant fan-out that is already in flight.
    //
    // The WRAPPING UP block nudges the agent to finish PR-worthy work the way a
    // human would: commit on its (already dedicated) branch, push, and open a
    // DRAFT pull request via `gh` — it has the same git+gh auth as the launching
    // session. Isolated workers otherwise strand their commits on `aish/<id>`
    // with no PR; this nudge closes that gap while staying opt-out for read-only
    // tasks that produced nothing committable.
    let mut next_input = format!(
        "You are running headless as an autonomous aish coordinator in {cwd}. You have your FULL \
toolset RIGHT NOW — read_file, write_file, list_dir, change_dir, run_program (build, test, git, \
gh, anything), and the connected MCP servers. You CAN read and edit files and run commands on \
this machine. Do NOT claim to be a text-only assistant or that you lack access — call the tools \
and actually do the work, then report what you did with concrete evidence (command output, exit \
codes, diffs).\n\nNEVER FABRICATE, ALWAYS VERIFY: Report ONLY what you actually did and observed \
— your final report is an evidence record, not a plausible story. Never claim to have run a command, \
watched a job or workflow run, or seen a result unless that tool call is really in your transcript. If \
you narrate an action ('watching the release…', 'streaming the logs…'), attach the actual tool call in \
the SAME turn; a turn ends when you reply, so a bare narration executes nothing. A long-running \
streamer like `gh run watch` must be launched as a background job (run_program background:true) and read \
later via job_output, or polled with a visible foreground `gh run view --json status,conclusion` — never \
describe having watched it otherwise. Confirm every outcome you report with a real read (gh run view, gh \
release view, git show, a status query) and state only what that evidence shows; if you could not verify \
something, say so explicitly rather than inventing a result.\n\nDECISION POINTS — avoid loops: If you notice you are repeating the same action or re-deriving a \
fact you already have, STOP and change approach. After about 3 failed attempts at the SAME \
sub-problem, do NOT keep retrying the same way — either try a materially different approach or \
stop and report explicitly: say \"I'm blocked because <specific reason>\", list what you tried \
and what you observed, and give your best partial result. A clearly-stated blocker is a \
successful outcome; an endless retry loop is a failure.\n\nRE-EVALUATE THE PLAN AFTER TRIAGE — don't over-decompose: \
Before you fan work out with `run_in_background`, re-check the plan against what triage just learned. If initial \
triage narrowed or COLLAPSED the suspected causes to a single root cause, do NOT dispatch the parallel fan-out you \
pre-planned — the independent-looking angles are now redundant. Root cause found → stop parallelizing: handle it \
solo, or narrow the fan-out to only the sub-problems that are still genuinely independent and where parallelism \
yields marginal new information. Only fan out when sub-problems are truly independent. If a redundant fan-out is \
ALREADY in flight when triage has since collapsed the problem, use `tell` to narrow or cancel the now-pointless \
peers rather than letting redundant work run.\n\nCOORDINATING WITH OTHER AGENTS — the `:tell` channel: an [Operator interjection] you receive mid-run arrived through this channel — the human (or another agent) steering you; treat it as updated instructions. You can steer ANOTHER in-flight coordinator the same way: call the `tell` tool with its run id (find ids with background_status) and a message, and it is folded into that coordinator's next round. Use it to hand off a finding, correct a peer's course, or narrow its scope.\n\nWRAPPING UP — open a draft PR for \
PR-worthy work: When you finish, if you created or changed files that are meant to land (a fix, \
feature, refactor, or docs) — as opposed to a read-only investigation, question, or analysis that \
produced no committable changes — do NOT leave the work uncommitted or stranded on a local branch. \
You have the SAME git + gh auth as the interactive session, so finish the job: stage and commit on a \
feature branch (you are typically already on a dedicated work branch — commit THERE; never commit to \
or push the default branch), push it, and open a DRAFT pull request with `gh pr create --draft --fill` \
(pass `--title`/`--body` when `--fill` cannot infer them). Put the PR URL in your final answer. If there \
are no committable changes, or `gh`/the remote is unavailable, skip the PR and report the branch name \
plus `git status` instead — do not fail the run over it.\n\nTASK:\n{input}",
        cwd = session.cwd.display(),
    );

    loop {
        if rounds >= round_cap {
            // TASK-291: hitting the round cap is NOT a failure — park the run in
            // the resumable `checkpoint` phase (TASK-294) with a state snapshot
            // (the last assistant synthesis) so an operator can review it and a
            // future `:resume` can continue from here. The full turn-by-turn
            // transcript is already durable in the worker store on disk.
            // Checkpoint rows are exempt from orphan reaping (see
            // `is_orphaned_row`) and are retained (neither `clear_finished` nor
            // the failed-reaper touch them), so the result stays reachable via
            // `:result` / `background_status`. `persist_terminal` writes phase +
            // result + metrics atomically (TASK-285 `finish_run`).
            let banner = format!(
                "[!] task exceeded max-rounds ({round_cap}). Transcript saved; operator review recommended."
            );
            eprintln!("{banner}");
            let last_synth = session
                .history
                .iter()
                .rev()
                .find(|m| {
                    matches!(m.role, crate::backend::Role::Assistant) && !m.text.trim().is_empty()
                })
                .map(|m| m.text.trim().to_string())
                .unwrap_or_default();
            let result = if last_synth.is_empty() {
                banner.clone()
            } else {
                format!("{banner}\n\nLast progress before checkpoint:\n{last_synth}")
            };
            persist_terminal(store, run_id, Phase::Checkpoint, Some(&result), None, session);
            finalize_worker_store(run_id, "checkpoint", Some(&result));
            return Outcome {
                phase: Phase::Checkpoint,
                result: Some(result),
                error: None,
                rounds,
            };
        }

        // Liveness: beat the run's heartbeat at EVERY round boundary, not only
        // while awaiting a batch. A long `coordinating` round (many tool calls,
        // no batch fan-out) otherwise lets the heartbeat go stale, risking a
        // false orphan-reap by a concurrent startup. (coordinator-lifecycle bug)
        if let Some(s) = store {
            let _ = s.heartbeat(run_id);
        }

        // ── operator messages: fold any `:tell`/SendMessage interjections that
        // arrived before this round into the upcoming turn. Delivery is
        // round-boundary — a message sent mid-turn lands on the next round.
        let folded = fold_operator_messages(store, run_id, &mut next_input);
        if folded > 0 {
            // Plain (no 🔧/🗨/📦 sentinel) so the parent's worker stream leaves it
            // in the failure tail without forwarding or pulsing the prompt badge.
            eprintln!("✉ folded {folded} operator message(s) into this round");
        }

        // ── stand-down: a parent raised the harsh `:stop` flag (harsher than a
        // `:tell` — it doesn't just steer the run, it ENDS it). Honor it here at
        // the round boundary: take ONE final graceful wrap-up turn so the worker
        // can preserve in-flight work (commit/push/draft-PR) and report a status,
        // then terminate as `done`. Any operator messages folded just above ride
        // along in `next_input`, so a `:tell` sent alongside the stop is still
        // seen. The immediacy over `:tell` (which also waits for the next round)
        // comes from the parent additionally SIGINT-ing the worker's process
        // group: that interrupts the in-flight turn and lands us here promptly
        // instead of after a possibly-long current round.
        let standing_down = store
            .map(|s| s.stand_down_requested(run_id).unwrap_or(false))
            .unwrap_or(false);
        if standing_down {
            eprintln!("🛑 stand-down ordered by parent — one final wrap-up turn, then exiting");
            let directive = "[STAND DOWN] Your parent has ordered you to STAND DOWN now — this \
overrides the task. Do NOT start or continue any substantive work. In this SINGLE final turn: \
preserve whatever you have in flight (if you have uncommitted changes and a remote is available, \
commit them to your branch, push, and open or refresh a DRAFT pull request), then give a brief \
final status plus your best partial result. After this turn you are terminated.";
            next_input = if next_input.trim().is_empty() {
                directive.to_string()
            } else {
                format!("{directive}\n\n{next_input}")
            };
            if let Some(s) = store {
                let _ = s.set_phase(run_id, Phase::Coordinating.as_str());
            }
            let mut allow = |_: &str| tools::Decision::AllowOnce;
            let answer =
                match crate::engine::run_turn(backend, session, next_input, &mut allow).await {
                    Ok(a) => a,
                    Err(e) => {
                        let error = format!("stand-down wrap-up turn failed: {e:#}");
                        persist_terminal(
                            store,
                            run_id,
                            Phase::Failed,
                            None,
                            Some(&error),
                            session,
                        );
                        finalize_worker_store(run_id, "failed", None);
                        return Outcome {
                            phase: Phase::Failed,
                            result: None,
                            error: Some(error),
                            rounds,
                        };
                    }
                };
            rounds += 1;
            if let Some(a) = session.turn_audit.as_mut() {
                a.synthesis(rounds as u64, &answer);
            }
            if let Some(w) = session.worker_transcript.as_mut() {
                w.record_message("assistant", "synthesis", &answer);
            }
            persist_terminal(store, run_id, Phase::Done, Some(&answer), None, session);
            finalize_worker_store(run_id, "done", Some(&answer));
            return Outcome {
                phase: Phase::Done,
                result: Some(answer),
                error: None,
                rounds,
            };
        }

        // ── checkpoint: a parent requested a deliberate PAUSE (TASK-294). Unlike
        // stand-down (which ENDS the run) a checkpoint HALTS it without finishing:
        // persist the resumable `checkpoint` phase and return at the round
        // boundary. NO wrap-up turn is taken — the transcript/worktree is left
        // intact so the run can be resumed manually later, and the row is
        // intentionally exempt from orphan reaping. Checked AFTER stand-down so a
        // terminate order wins over a pause when both race.
        let checkpointing = store
            .map(|s| s.checkpoint_requested(run_id).unwrap_or(false))
            .unwrap_or(false);
        if checkpointing {
            eprintln!(
                "⏸ checkpoint requested by parent — halting at round boundary (resumable)"
            );
            // Atomically persist the resumable `checkpoint` phase together with
            // the run's cumulative metrics in ONE store txn (TASK-285 pattern).
            // Checkpoint is non-terminal, but `persist_terminal`/`finish_run` is
            // just a phase+metrics UPDATE — passing `Phase::Checkpoint` with no
            // result/error snapshots effort at the pause without a torn write.
            persist_terminal(store, run_id, Phase::Checkpoint, None, None, session);
            finalize_worker_store(run_id, "checkpoint", None);
            return Outcome {
                phase: Phase::Checkpoint,
                result: None,
                error: None,
                rounds,
            };
        }

        // ── coordinating: one full-tool agentic turn ────────────────────────
        if let Some(s) = store {
            let _ = s.set_phase(run_id, Phase::Coordinating.as_str());
        }
        let mut allow = |_: &str| tools::Decision::AllowOnce;
        let turn = crate::engine::run_turn(backend, session, next_input, &mut allow).await;
        rounds += 1;

        let answer = match turn {
            Ok(a) => a,
            Err(e) => {
                let error = format!("{e:#}");
                persist_terminal(store, run_id, Phase::Failed, None, Some(&error), session);
                finalize_worker_store(run_id, "failed", None);
                return Outcome {
                    phase: Phase::Failed,
                    result: None,
                    error: Some(error),
                    rounds,
                };
            }
        };

        // Tier-1 audit: journal this round's end-of-turn synthesis (the model's
        // tool-less narrative answer for the round) alongside the per-turn tool
        // calls already logged by `engine::run_turn`. A run that emits the same
        // synthesis round after round is visibly looping in the `.jsonl` — the
        // bare tool log alone can hide that. Best-effort; empty text is skipped.
        if let Some(a) = session.turn_audit.as_mut() {
            a.synthesis(rounds as u64, &answer);
        }
        // S9.3: mirror the round synthesis into the per-worker transcript so a
        // replay shows each round’s final narrative answer, not just the tool turns.
        if let Some(w) = session.worker_transcript.as_mut() {
            w.record_message("assistant", "synthesis", &answer);
        }

        // ── Worker-exit evaluation: did this round's turn end abnormally? The
        // engine tags a loop-detected / forced-summarize / budget-exhausted stop
        // with a parseable banner on the first line of the answer. Decide a
        // recovery disposition rather than treating the (possibly partial) answer
        // as a finished result.
        if let Some(reason) = crate::loopguard::ExitReason::parse_banner(&answer) {
            // Operator Ctrl-C interrupt: NOT a failure and NOT an auto-recovery.
            // Keep the coordinator alive, fold a reassess directive, and drive
            // the next round — where fold_operator_messages also picks up any
            // fresh `:tell` steer the operator sent alongside the interrupt.
            if matches!(reason, crate::loopguard::ExitReason::Interrupted) {
                eprintln!(
                    "\x1b[2maish: round {rounds} interrupted by operator (Ctrl-C) — reassessing\x1b[0m"
                );
                next_input = "[operator interrupt] The operator pressed Ctrl-C to interrupt your \
previous turn mid-flight. Stop what you were doing — do NOT blindly resume it. Re-read the task \
and any newer operator messages, reassess your approach, and either continue with the most \
sensible next step or, if you should wait for direction, give a brief status plus your best \
partial result."
                    .to_string();
                continue;
            }
            let disp = crate::loopguard::classify_disposition(
                &reason,
                auto_recoveries,
                crate::loopguard::MAX_AUTO_RECOVERIES,
            );
            if disp.is_recovery() {
                // Auto-resume the work from where it left off, or nudge the model
                // off a loop — feed the matching directive into the next round and
                // keep driving (still bounded by `round_cap`). The completed work
                // is preserved in history + the turn-audit replay, so a resume
                // continues rather than restarting from scratch.
                auto_recoveries += 1;
                eprintln!(
                    "\x1b[2maish: round {rounds} ended [{}] — {} (auto-recovery {auto_recoveries}/{})\x1b[0m",
                    reason.tag(),
                    disp.verb(),
                    crate::loopguard::MAX_AUTO_RECOVERIES,
                );
                next_input = crate::loopguard::recovery_directive(disp, &reason)
                    .unwrap_or_else(|| "Continue the task from where you left off.".to_string());
                continue;
            }
            // FlagOperator: auto-recovery is exhausted (or the stop isn't one to
            // paper over). Stop the run and record a clear, human-actionable
            // failure so the operator can take over — the partial answer is
            // preserved on the worktree branch + the turn-audit journal.
            let error = format!(
                "flagged for operator after {auto_recoveries} auto-recovery attempt(s): {}",
                reason.detail()
            );
            eprintln!("\x1b[2maish: {error}\x1b[0m");
            persist_terminal(store, run_id, Phase::Failed, None, Some(&error), session);
            finalize_worker_store(run_id, "failed", Some(&answer));
            return Outcome {
                phase: Phase::Failed,
                result: Some(answer),
                error: Some(error),
                rounds,
            };
        }

        // ── awaiting_batch: did this round fan work out to the Batches API? ──
        // The model offloads heavy sub-work via the run_in_background→batch path,
        // which lands jobs in `session.batch_jobs`. If any are running, we are in
        // the awaiting_batch phase: persist it, heartbeat, and block until they
        // finish (their results auto-print; the next round's turn sees them).
        if crate::batch::running_count(&session.batch_jobs) > 0 {
            if let Some(s) = store {
                let _ = s.set_phase(run_id, Phase::AwaitingBatch.as_str());
            }
            // Forwarded to the watcher (via the `📦` sentinel) as the batch-vs-
            // standard indicator: this round fanned work out to the Batches API.
            let n = crate::batch::running_count(&session.batch_jobs);
            eprintln!("📦 fanned {n} sub-task(s) out to the Batches API; awaiting results");
            await_batches_with_heartbeat(session, run_id, store).await;

            // Fold the batch results back: feed the just-completed sub-work into
            // the next round so the model can reduce over it. The results were
            // surfaced inline; this round-trips the coordinator back to
            // `coordinating` to assemble/continue.
            next_input =
                "The background sub-tasks you offloaded have completed (their results were \
delivered above). Fold them into your work: continue the task, or give the final answer if done."
                    .to_string();
            continue;
        }

        // The turn produced a final text answer with no pending sub-work — it
        // would normally end the run. But an operator message may have landed
        // DURING this turn; pick it up before finishing so a late clarification
        // isn't dropped on the floor. When present, continue another round with
        // the interjection as the input instead of terminating.
        let mut late_input = String::new();
        let late = fold_operator_messages(store, run_id, &mut late_input);
        if late > 0 {
            eprintln!("✉ {late} operator message(s) arrived during the turn; continuing");
            next_input = late_input;
            continue;
        }

        // No pending sub-work, no pending messages → done.
        persist_terminal(store, run_id, Phase::Done, Some(&answer), None, session);
        finalize_worker_store(run_id, "done", Some(&answer));
        return Outcome {
            phase: Phase::Done,
            result: Some(answer),
            error: None,
            rounds,
        };
    }
}

/// Block until every spawned batch reaches a terminal state, beating the run's
/// durable heartbeat on `HEARTBEAT_INTERVAL` so a long poll doesn't look like a
/// dead run. This is aish's analogue of atum's `collectBatch` keep-alive timer:
/// a batch wait has no latency SLA, so liveness must be stamped while we wait.
/// The heartbeat is best-effort — a store error never stalls or sinks the wait.
async fn await_batches_with_heartbeat(
    session: &Session,
    run_id: &str,
    store: Option<&CoordinatorStore>,
) {
    let mut last_beat = tokio::time::Instant::now();
    loop {
        if crate::batch::running_count(&session.batch_jobs) == 0 {
            return;
        }
        if last_beat.elapsed() >= HEARTBEAT_INTERVAL {
            if let Some(s) = store {
                let _ = s.heartbeat(run_id);
            }
            last_beat = tokio::time::Instant::now();
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

/// Startup reattach for `coordinator_runs` (mirrors `batch::rehydrate`).
///
/// A coordinator runs in a child process (or this one when re-exec'd headless),
/// and its live transcript is in-memory, so unlike a platform-side batch we can't
/// reattach to a crashed run's conversation. Instead we:
///   * surface every `done` run's result to the terminal (so a completed
///     background job isn't silently lost across a restart), and
///   * reap orphaned runs — a non-terminal row whose owning session is gone and
///     whose heartbeat is stale — by stamping `failed`, so they don't linger as
///     phantom "running" entries in `background_status`.
///
/// Runs belonging to a *live* owner (another running aish, by session id) or with
/// a fresh heartbeat are left untouched. Idempotent: already-surfaced/terminal
/// rows are cleared, so a second start is a no-op.
pub fn rehydrate(session: &mut Session) {
    // Best-effort: prune git's record of worktrees whose directories are gone
    // (e.g. a crashed isolated worker left a dangling registration), then sweep
    // the managed worktree root for orphaned/old CLEAN leftovers. Moving off the
    // OS temp dir (ISS-2046) means the OS no longer GCs these, so aish must — the
    // sweeper NEVER removes a dirty or commits-ahead worktree (operator's work).
    crate::worker::prune_worktrees(&session.cwd);
    crate::worker::sweep_worktrees(&session.cwd);
    // S9.3: age out finished per-worker conversation-store dirs under the state
    // root, mirroring the worktree sweep above. Never reclaims a running or
    // work-bearing (kept-branch) worker dir (worker_store::should_sweep_worker).
    let _ = crate::worker_store::sweep_worker_dirs();

    // A coordinator CHILD (`AISH_COORDINATOR=1`) runs the full `main()` startup
    // before it reaches `run_coordinator`. Without this guard EVERY spawned
    // child would surface + reap + purge the SHARED `coordinator_runs` store at
    // boot — deleting sibling runs' rows and "surfacing" their results into the
    // child's captured (and invisible) stdout. That is the core mechanism behind
    // lost coordinator rows/results. Only the launching interactive session owns
    // the reattach + salvage sweep; a child just runs its one task.
    if std::env::var_os("AISH_COORDINATOR").is_some() {
        return;
    }

    let Some(store) = session.coordinator_store.clone() else {
        return;
    };
    let rows = match store.load_all() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("\x1b[2maish: couldn't load saved coordinator runs: {e:#}\x1b[0m");
            return;
        }
    };
    // Whether to emit the verbose startup digest (completed-result walls,
    // per-salvage lines, reattach summary). Suppressed by DEFAULT — a fresh
    // terminal shouldn't open onto a wall of prior workers' output. All the
    // state bookkeeping below still runs; only the human-facing prints are gated.
    let digest = startup_digest_enabled(session);
    let own = session.session_id.clone();
    let mut surfaced = 0usize;
    let mut reaped = 0usize;
    for row in rows {
        let phase = Phase::parse(&row.phase);
        match phase {
            Phase::Done => {
                // Surface a completed background coordinator result. When the
                // digest is shown, print the full wall and (below) clear the row.
                // When suppressed, print nothing and KEEP the row so the result
                // stays retrievable via `:workers all` / `background_status` /
                // `:result <id>` instead of being surfaced-and-dropped.
                if let Some(result) = &row.result {
                    if !result.trim().is_empty() {
                        if digest {
                            print_completed(&row.run_id, result);
                        }
                        surfaced += 1;
                    }
                }
            }
            Phase::Failed => {} // terminal, nothing to do (cleared below)
            // A checkpointed run is a DELIBERATE, resumable pause (TASK-294): it
            // is left untouched on rehydrate — never surfaced, never reaped — so
            // it stays parked at `checkpoint` for a later manual resume even when
            // its launching session is gone.
            Phase::Checkpoint => {}
            Phase::Coordinating | Phase::AwaitingBatch => {
                // Non-terminal. If it's ours (same session id — only possible
                // after an in-process resume, since ids are per-run) or its
                // heartbeat is fresh, leave it; otherwise it's orphaned. Same
                // predicate the live reaper (`reap_orphaned_runs`) uses.
                if is_orphaned_row(
                    row.session_id.as_deref(),
                    own.as_str(),
                    &phase,
                    row.heartbeat_at.as_deref(),
                ) {
                    let _ = store.set_failed(
                        &row.run_id,
                        "orphaned: owner gone and heartbeat stale (reaped on startup)",
                    );
                    reaped += 1;
                }
            }
        }
    }
    // Terminal `done` rows: when the digest was shown, the result was delivered
    // to the terminal, so drop it (historical behavior) — `clear_finished` also
    // purges orphaned mailbox messages. When suppressed (the default), KEEP the
    // `done` rows so their results stay retrievable, and instead bound them like
    // `failed` rows (keep-recent + max-age) so the table can't grow without
    // bound; purge orphaned mailbox messages directly since `clear_finished`
    // didn't run. `failed` rows are RETAINED for forensics (#129 item 5) either
    // way so a reaped/errored run stays visible in `:workers`.
    if digest {
        let _ = store.clear_finished();
    } else {
        store.purge_orphan_messages();
        let _ = reap_done_runs(&store);
    }
    // Bound the now-retained `failed` rows: keep a recent, age-limited window so
    // the forensic trail survives a restart without the table growing unbounded.
    // Runs BEFORE salvage and BEFORE the post-sweep id snapshot, so a work-bearing
    // worktree whose row we trim here is simply re-derived by salvage below (the
    // worktree is the durable source of truth; the store row is a derived view).
    let reaped_failed = reap_failed_runs(&store);
    // Salvage runs whose durable row was lost on early termination. Run AFTER the
    // terminal-row purge + failed-row reap and key off a FRESH post-sweep id set,
    // so the `failed` salvage rows we write this pass survive the boot (they're
    // re-derived from the surviving worktree — not duplicated — on the next
    // startup).
    let known_after: HashSet<String> = store
        .load_all()
        .map(|rows| rows.into_iter().map(|r| r.run_id).collect())
        .unwrap_or_default();
    let salvaged = salvage_orphaned_worktrees(&session.cwd, &store, &known_after, digest);
    // TASK-289: scan the durable coordinator registry — mark rows whose owning
    // process is dead as `orphaned` (parent-death recovery) and log any that
    // carried an in-flight batch job as resurrectable (full resume is TASK-291).
    let (regs_reaped, regs_resurrectable) = scan_coordinator_registry(&store, digest);
    if digest
        && (surfaced > 0
            || reaped > 0
            || salvaged > 0
            || reaped_failed > 0
            || regs_reaped > 0
            || regs_resurrectable > 0)
    {
        eprintln!(
            "\x1b[2maish: reattached coordinator runs ({surfaced} delivered, {reaped} reaped, {salvaged} salvaged, {reaped_failed} failed-pruned, {regs_reaped} registry-orphaned, {regs_resurrectable} resurrectable)\x1b[0m"
        );
    }
}

/// TASK-289 startup scan of the durable `coordinator_registry`: for every row
/// still considered live, check whether its OS `pid` is alive. A dead pid means
/// the owning coordinator process died without cleanly deregistering — mark the
/// row `orphaned` (parent-death recovery). When such an orphan carried an
/// in-flight `batch_job_id` it is RESURRECTABLE (its Batches job keeps running
/// platform-side), so log it for the future resume path (TASK-291/SPR-059) —
/// this scan does NOT itself resume anything. A row whose pid is still alive is
/// left untouched. Returns `(reaped, resurrectable)` counts. Best-effort: a
/// store error yields `(0, 0)` and never sinks startup.
fn scan_coordinator_registry(store: &CoordinatorStore, digest: bool) -> (usize, usize) {
    let rows = match store.get_live_runs() {
        Ok(r) => r,
        Err(e) => {
            if digest {
                eprintln!("\x1b[2maish: couldn't scan coordinator registry: {e:#}\x1b[0m");
            }
            return (0, 0);
        }
    };
    let mut reaped = 0usize;
    let mut resurrectable = 0usize;
    for row in rows {
        if pid_is_alive(row.pid) {
            continue; // owner still running — not an orphan
        }
        if store.mark_orphaned(&row.coord_id).is_ok() {
            reaped += 1;
            if row.batch_job_id.is_some() {
                resurrectable += 1;
                if digest {
                    eprintln!(
                        "\x1b[2maish: coordinator {} orphaned (pid {} gone) — resurrectable via batch {} (resume: TASK-291)\x1b[0m",
                        crate::batch::short_id(&row.coord_id),
                        row.pid,
                        row.batch_job_id.as_deref().unwrap_or("?"),
                    );
                }
            }
        }
    }
    (reaped, resurrectable)
}

/// True when process `pid` is alive (signal-0 probe). `kill(pid, 0)` sends no
/// signal but performs the existence + permission check: `Ok`/`EPERM` ⇒ the
/// process exists, `ESRCH` ⇒ it does not. A non-positive pid is never a live
/// process. Used by the TASK-289 registry scan to reap dead coordinators.
fn pid_is_alive(pid: i64) -> bool {
    if pid <= 0 {
        return false;
    }
    // SAFETY: kill with signal 0 is the documented liveness probe; it never
    // delivers a signal, only reports existence/permission via errno.
    let rc = unsafe { libc::kill(pid as libc::pid_t, 0) };
    if rc == 0 {
        return true;
    }
    // rc == -1: alive only when the failure is EPERM (exists, not permitted),
    // dead on ESRCH (no such process).
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// Pure salvage decision: a worktree that still holds work but has NO surviving
/// `coordinator_runs` row was lost to an early termination — recover it. When a
/// row already exists (terminal or not), the normal lifecycle owns it, so don't
/// double-report. Unit-tested.
fn is_salvageable(has_row: bool, has_work: bool) -> bool {
    has_work && !has_row
}

/// Recover runs whose durable row was lost on early termination: scan the managed
/// worktree root for work-bearing leaves (uncommitted changes or commits ahead),
/// and for any with no surviving store row, insert a `failed` salvage row and
/// announce the recoverable branch/path — so the otherwise-invisible work shows
/// up in `:workers` again and an operator can review/merge it. Best-effort: a
/// store write that fails is skipped, never sinking startup. Returns the count
/// salvaged. (coordinator-lifecycle bug: rows lost on early termination.)
fn salvage_orphaned_worktrees(
    cwd: &std::path::Path,
    store: &CoordinatorStore,
    known: &HashSet<String>,
    announce: bool,
) -> usize {
    let mut salvaged = 0usize;
    for w in crate::worker::work_bearing_worktrees(cwd) {
        if !is_salvageable(known.contains(&w.id), true) {
            continue;
        }
        let error = format!(
            "salvaged: coordinator_runs row lost on early termination — work preserved on branch `{}` at {} (review/merge from the parent repo; not auto-merged)",
            w.branch,
            w.path.display(),
        );
        if store
            .insert_salvaged(
                &w.id,
                &format!("(salvaged orphan worktree {})", w.id),
                &error,
            )
            .is_ok()
        {
            if announce {
                eprintln!(
                    "\x1b[2maish: salvaged orphaned worker {} — work on branch `{}` ({})\x1b[0m",
                    w.id,
                    w.branch,
                    w.path.display(),
                );
            }
            salvaged += 1;
        }
    }
    salvaged
}

/// Count active (non-terminal) coordinator runs in the durable store whose
/// `run_id` is NOT already tracked in `in_memory_ids` (this session's in-process
/// worker subprocesses, which `worker::running_count` already counts). This is
/// what makes the prompt's `⟳N` activity badge agree with `:workers`: a goal-loop
/// generator turn (`run_once`, never registered in `worker_jobs`), a run launched
/// from another session, and a run reattached after a restart all live ONLY in
/// the durable store — so without counting them the prompt shows no activity
/// indicator even while `:workers` lists them coordinating. Best-effort: a store
/// read error counts 0 rather than breaking the prompt.
pub fn active_store_count(store: &CoordinatorStore, in_memory_ids: &HashSet<String>) -> usize {
    store
        .load_all()
        .map(|rows| {
            rows.into_iter()
                .filter(|r| !in_memory_ids.contains(&r.run_id))
                .filter(|r| {
                    matches!(
                        Phase::parse(&r.phase),
                        Phase::Coordinating | Phase::AwaitingBatch
                    )
                })
                .count()
        })
        .unwrap_or(0)
}

/// Print a completed coordinator result above the prompt, matching the batch /
/// worker completion block style so it reads consistently.
fn print_completed(run_id: &str, result: &str) {
    if crate::present::deferred() {
        // Interactive REPL: let the presenter own the prompt; a dim inline note
        // is enough (the full result is recoverable via background_status/store).
        crate::tools::announce(
            &format!("[{}]", crate::batch::short_id(run_id)),
            "background coordinator finished while away",
        );
        return;
    }
    println!(
        "\x1b[2m── coordinator {} complete ──\x1b[0m\n{}",
        crate::batch::short_id(run_id),
        crate::md::render_stdout(result.trim())
    );
}

/// True when a heartbeat timestamp is older than `ORPHAN_STALE_AFTER` (or
/// missing/unparseable). Compares against SQLite `current_timestamp` strings,
/// which are UTC `"YYYY-MM-DD HH:MM:SS"`.
fn heartbeat_is_stale(heartbeat_at: Option<&str>) -> bool {
    let Some(hb) = heartbeat_at else {
        return true; // no beat recorded → treat as stale
    };
    match parse_sqlite_timestamp(hb) {
        Some(beat_secs) => {
            let now = now_unix_secs();
            now.saturating_sub(beat_secs) > ORPHAN_STALE_AFTER.as_secs() as i64
        }
        None => true, // unparseable → stale (don't keep a row we can't reason about)
    }
}

/// Decide whether a coordinator store row is an ORPHAN that should be reaped: a
/// non-terminal run (`coordinating`/`awaiting_batch`) NOT owned by the reading
/// session whose heartbeat is stale (its owner process is gone). Pure so both
/// the startup reaper and the live status-read reaper share ONE predicate —
/// keeping their liveness criteria from drifting apart. Terminal rows and this
/// session's own rows are never orphans.
fn is_orphaned_row(
    session_id: Option<&str>,
    own: &str,
    phase: &Phase,
    heartbeat_at: Option<&str>,
) -> bool {
    matches!(phase, Phase::Coordinating | Phase::AwaitingBatch)
        && session_id != Some(own)
        && heartbeat_is_stale(heartbeat_at)
}

/// Live orphan reap for the status-read paths (`:workers`, `background_status`).
/// The durable store is the source of truth, but a coordinator that fans out
/// interactive sub-coordinators runs their reconcile as detached in-process
/// tasks: if that parent process exits before they finish, the children's rows
/// are left stuck at `coordinating` forever — the operator sees zombie
/// "coordinating" workers doing no apparent work (the reported symptom). Startup
/// already reaps these (`reattach_saved_runs`), but a long-lived interactive
/// session never restarts, so nothing flips them. Calling this on every status
/// read reconciles any stale, unowned, non-terminal row to `failed` so zombies
/// self-heal LIVE instead of lingering until the next process start. Returns the
/// number reaped. Uses the SAME `is_orphaned_row` predicate as the startup path.
pub fn reap_orphaned_runs(store: &CoordinatorStore, own_session_id: &str) -> usize {
    let rows = match store.load_all() {
        Ok(r) => r,
        Err(_) => return 0,
    };
    let mut reaped = 0usize;
    for run_id in orphaned_run_ids(&rows, own_session_id) {
        if store
            .set_failed(
                &run_id,
                "orphaned: owner gone and heartbeat stale (reaped live)",
            )
            .is_ok()
        {
            reaped += 1;
        }
    }
    reaped
}

/// Pure core of the reap: the run-ids among `rows` that are orphans for the
/// reading session `own`. Split out from the store I/O so the full ownership ×
/// phase × staleness matrix is unit-testable without a live DB.
fn orphaned_run_ids(rows: &[crate::db::CoordinatorRow], own: &str) -> Vec<String> {
    rows.iter()
        .filter(|row| {
            is_orphaned_row(
                row.session_id.as_deref(),
                own,
                &Phase::parse(&row.phase),
                row.heartbeat_at.as_deref(),
            )
        })
        .map(|row| row.run_id.clone())
        .collect()
}

/// Current UTC time as unix seconds.
fn now_unix_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Parse SQLite's `current_timestamp` form — `"YYYY-MM-DD HH:MM:SS"` in UTC —
/// to unix seconds, with a plain civil-date computation (no chrono dependency).
/// Returns `None` on any malformed field.
fn parse_sqlite_timestamp(s: &str) -> Option<i64> {
    let (date, time) = s.trim().split_once(' ')?;
    let mut d = date.splitn(3, '-');
    let year: i64 = d.next()?.parse().ok()?;
    let month: i64 = d.next()?.parse().ok()?;
    let day: i64 = d.next()?.parse().ok()?;
    let mut t = time.splitn(3, ':');
    let hour: i64 = t.next()?.parse().ok()?;
    let min: i64 = t.next()?.parse().ok()?;
    let sec: i64 = t.next().unwrap_or("0").parse().ok()?;
    Some(civil_to_unix(year, month, day, hour, min, sec))
}

/// Days-from-civil → unix seconds. Howard Hinnant's `days_from_civil` algorithm
/// (public domain), so we don't pull in a date crate just for orphan detection.
fn civil_to_unix(year: i64, month: i64, day: i64, hour: i64, min: i64, sec: i64) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let doy = (153 * (if month > 2 { month - 3 } else { month + 9 }) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    days * 86_400 + hour * 3_600 + min * 60 + sec
}

/// Finalize the S9.3 per-worker conversation store at a terminal phase: write
/// the final answer to `result.txt` (the cross-container-boundary result
/// channel) when one is present, then flip `meta.json.status` to `done`/`failed`
/// via an atomic rewrite. Best-effort — a missing store or write error is
/// swallowed so finalizing the transcript never changes the run’s outcome.
fn finalize_worker_store(run_id: &str, status: &str, result: Option<&str>) {
    if let Some(r) = result {
        let _ = crate::worker_store::write_result(run_id, r);
    }
    let _ = crate::worker_store::set_status(run_id, status);
}

/// Atomically persist a run's TERMINAL outcome — the terminal `phase` plus its
/// `result`/`error` and the live session's cumulative cost/effort metrics
/// (tokens in/out, agentic turns, tool-call count) — in ONE store transaction
/// (TASK-285). Replaces the former `set_done`/`set_failed` followed by a
/// separate `record_metrics` write: a panic/crash between those two statements
/// used to leave the `coordinator_runs` row half-updated (terminal phase with
/// zero metrics, or metrics under a still-`coordinating` phase, either of which
/// muddies resume/reporting). Routed through [`CoordinatorStore::finish_run`],
/// the phase, result/error, heartbeat, and metrics commit as a unit — a re-read
/// after a rolled-back mid-write sees the prior resumable row intact.
/// Best-effort: a store error is swallowed so persistence can never sink a
/// completing run (the same contract the two calls it replaces had).
fn persist_terminal(
    store: Option<&CoordinatorStore>,
    run_id: &str,
    phase: Phase,
    result: Option<&str>,
    error: Option<&str>,
    session: &Session,
) {
    if let Some(s) = store {
        let metrics = crate::coordinator_store::RunMetrics {
            tokens_in: session.tokens_in as u64,
            tokens_out: session.tokens_out as u64,
            turns: session.turns_total as u64,
            tool_calls: session.tool_calls_total as u64,
        };
        let _ = s.finish_run(run_id, phase.as_str(), result, error, metrics);
    }
}

/// Best-effort current git branch of `dir`, recorded in the worker `meta.json`
/// so retention never reclaims a dir whose worktree still holds kept work on an
/// `aish/<id>` branch (AC7). `None` outside a repo, on a detached HEAD, or when
/// the branch isn’t an aish worktree branch (the trunk carries no kept work).
fn current_worktree_branch(dir: &std::path::Path) -> Option<String> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let branch = String::from_utf8_lossy(&out.stdout).trim().to_string();
    branch.starts_with("aish/").then_some(branch)
}

/// A filesystem-safe repo key for the worker `meta.json` (informational): the
/// run directory’s basename. Kept lightweight (no extra git probe) — the
/// authoritative cross-reference is `meta.run_id` ↔ the SQLite run row (AC3).
fn worker_store_repo_key(dir: &std::path::Path) -> String {
    dir.file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "repo".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn phase_string_roundtrip_is_total() {
        for p in [
            Phase::Coordinating,
            Phase::AwaitingBatch,
            Phase::Checkpoint,
            Phase::Done,
            Phase::Failed,
        ] {
            assert_eq!(Phase::parse(p.as_str()), p);
        }
        // Unknown/legacy phase strings are treated as a dead run.
        assert_eq!(Phase::parse("planning"), Phase::Failed);
        assert_eq!(Phase::parse(""), Phase::Failed);
    }

    #[test]
    fn terminal_and_resumable_partition_the_phases() {
        assert!(!Phase::Coordinating.is_terminal() && Phase::Coordinating.is_resumable());
        assert!(!Phase::AwaitingBatch.is_terminal() && Phase::AwaitingBatch.is_resumable());
        // Checkpoint is a resumable, non-terminal pause (TASK-294).
        assert!(!Phase::Checkpoint.is_terminal() && Phase::Checkpoint.is_resumable());
        assert!(Phase::Done.is_terminal() && !Phase::Done.is_resumable());
        assert!(Phase::Failed.is_terminal() && !Phase::Failed.is_resumable());
    }

    #[test]
    fn format_interjection_frames_messages_as_supervisory() {
        let block = format_interjection(&[
            "focus on the auth module first".to_string(),
            "  skip the e2e tests  ".to_string(),
        ]);
        // The framing names it an operator interjection and asserts precedence.
        assert!(block.contains("Operator interjection"));
        assert!(block.contains("the interjection wins"));
        // Each message is a trimmed bullet, in order.
        assert!(block.contains("- focus on the auth module first"));
        assert!(block.contains("- skip the e2e tests"));
        let auth = block.find("auth module").unwrap();
        let e2e = block.find("e2e tests").unwrap();
        assert!(auth < e2e, "messages must keep send order");
    }

    #[test]
    fn fold_operator_messages_prepends_and_drains() {
        let path = std::env::temp_dir().join(format!("aish_fold_{}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let store = CoordinatorStore::open(&path).unwrap();
        store.insert("run_x", "do the thing", "sess", None).unwrap();

        // No messages → no-op, input unchanged.
        let mut input = "continue the task".to_string();
        assert_eq!(fold_operator_messages(Some(&store), "run_x", &mut input), 0);
        assert_eq!(input, "continue the task");

        // One message → folded, prepended ahead of the existing input, drained.
        store
            .enqueue_message("run_x", "use the staging DB", None)
            .unwrap();
        let mut input = "continue the task".to_string();
        let n = fold_operator_messages(Some(&store), "run_x", &mut input);
        assert_eq!(n, 1);
        assert!(input.contains("use the staging DB"));
        assert!(
            input.trim_end().ends_with("continue the task"),
            "original input kept after the interjection"
        );
        let interj = input.find("Operator interjection").unwrap();
        let cont = input.find("continue the task").unwrap();
        assert!(interj < cont, "interjection is prepended");
        // Delete-on-read: a second fold sees nothing.
        let mut input2 = "next".to_string();
        assert_eq!(
            fold_operator_messages(Some(&store), "run_x", &mut input2),
            0
        );
        assert_eq!(input2, "next");

        // No store → no-op.
        let mut input3 = "x".to_string();
        assert_eq!(fold_operator_messages(None, "run_x", &mut input3), 0);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn sqlite_timestamp_parses_to_unix() {
        // 1970-01-01 00:00:00 is the unix epoch.
        assert_eq!(parse_sqlite_timestamp("1970-01-01 00:00:00"), Some(0));
        // A known instant: 2021-01-01 00:00:00 UTC = 1609459200.
        assert_eq!(
            parse_sqlite_timestamp("2021-01-01 00:00:00"),
            Some(1_609_459_200)
        );
        // Leap-year day handled by the civil algorithm.
        assert_eq!(
            parse_sqlite_timestamp("2020-02-29 12:00:00"),
            Some(1_582_977_600)
        );
        // Malformed → None.
        assert_eq!(parse_sqlite_timestamp("not a timestamp"), None);
        assert_eq!(parse_sqlite_timestamp("2021-13"), None);
    }

    #[test]
    fn is_salvageable_only_when_work_exists_and_no_row() {
        // Work-bearing worktree with no surviving store row → salvage it.
        assert!(is_salvageable(false, true));
        // A surviving row (terminal or live) means the lifecycle owns it — skip.
        assert!(!is_salvageable(true, true));
        // No work in the leaf → nothing to recover, regardless of row state.
        assert!(!is_salvageable(false, false));
        assert!(!is_salvageable(true, false));
    }

    #[test]
    fn parse_flag_recognizes_truthy_falsey_and_rejects_junk() {
        for s in ["1", "true", "on", "yes", "y", "TRUE", " On "] {
            assert_eq!(parse_flag(s), Some(true), "{s:?} should be truthy");
        }
        for s in ["0", "false", "off", "no", "n", "OFF", " No "] {
            assert_eq!(parse_flag(s), Some(false), "{s:?} should be falsey");
        }
        for s in ["", "maybe", "2", "onoff"] {
            assert_eq!(parse_flag(s), None, "{s:?} should be unrecognized");
        }
    }

    #[test]
    fn failed_retention_plan_keeps_recent_and_drops_old_and_excess() {
        let now = 1_000_000i64;
        let day = 86_400i64;
        let rows = vec![
            ("r_new1".to_string(), Some(now - day)),
            ("r_new2".to_string(), Some(now - 2 * day)),
            ("r_old".to_string(), Some(now - 30 * day)), // exceeds the 14d age bound
            ("r_none".to_string(), None),                // unknown ts → reap-eligible
        ];
        // Generous count bound (10), 14-day age bound: only the old + unknown go.
        let plan = failed_retention_plan(&rows, now, 10, 14 * day);
        assert!(plan.contains(&"r_old".to_string()));
        assert!(plan.contains(&"r_none".to_string()));
        assert!(!plan.contains(&"r_new1".to_string()));
        assert!(!plan.contains(&"r_new2".to_string()));

        // keep_recent = 1 keeps only the single most-recent fresh row (r_new1).
        let plan = failed_retention_plan(&rows, now, 1, 14 * day);
        assert!(
            !plan.contains(&"r_new1".to_string()),
            "newest survives the count bound"
        );
        assert!(
            plan.contains(&"r_new2".to_string()),
            "older fresh row trimmed by count"
        );
        assert!(plan.contains(&"r_old".to_string()));
        assert!(plan.contains(&"r_none".to_string()));
    }

    #[test]
    fn failed_retention_plan_is_idempotent_and_order_independent() {
        let now = 1_000_000i64;
        let day = 86_400i64;
        let rows = vec![
            ("a".to_string(), Some(now - day)),
            ("b".to_string(), Some(now - 2 * day)),
            ("c".to_string(), Some(now - 3 * day)),
        ];
        // keep 2 → drops the single oldest ("c"), regardless of input order.
        let plan = failed_retention_plan(&rows, now, 2, 100 * day);
        assert_eq!(plan, vec!["c".to_string()]);
        let mut shuffled = rows.clone();
        shuffled.reverse();
        assert_eq!(
            failed_retention_plan(&shuffled, now, 2, 100 * day),
            vec!["c".to_string()]
        );
        // Re-running on the survivors removes nothing (idempotent).
        let survivors: Vec<_> = rows.into_iter().filter(|(id, _)| id != "c").collect();
        assert!(failed_retention_plan(&survivors, now, 2, 100 * day).is_empty());
    }

    #[test]
    fn reap_failed_runs_trims_failed_only_to_bound() {
        let path = std::env::temp_dir().join(format!("aish_reapfailed_{}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let store = CoordinatorStore::open(&path).unwrap();
        store.insert("f1", "t", "s", None).unwrap();
        store.set_failed("f1", "boom").unwrap();
        store.insert("f2", "t", "s", None).unwrap();
        store.set_failed("f2", "boom").unwrap();
        store.insert("ok", "t", "s", None).unwrap();
        store.set_done("ok", "done").unwrap();
        store.insert("live", "t", "s", None).unwrap(); // coordinating

        let now = now_unix_secs();
        // Generous bounds keep all (few, fresh) failed rows.
        assert_eq!(reap_failed_runs_with(&store, 10, 100 * 86_400, now), 0);
        // keep=0 reaps every failed row, but leaves done + coordinating intact.
        assert_eq!(reap_failed_runs_with(&store, 0, 100 * 86_400, now), 2);
        let ids: Vec<String> = store
            .load_all()
            .unwrap()
            .into_iter()
            .map(|r| r.run_id)
            .collect();
        assert!(
            ids.contains(&"ok".to_string()),
            "done row is clear_finished's job, not the reaper's"
        );
        assert!(ids.contains(&"live".to_string()), "non-terminal untouched");
        assert!(!ids.contains(&"f1".to_string()));
        assert!(!ids.contains(&"f2".to_string()));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn clamp_usize_parses_clamps_and_falls_back() {
        // Unset / absent → default.
        assert_eq!(clamp_usize(None, 48, 1, 1000), 48);
        // A clean in-range value is taken (whitespace tolerated).
        assert_eq!(clamp_usize(Some("  64 ".into()), 48, 1, 1000), 64);
        // Out of range (low/high) → default, never an uncapped or zero cap.
        assert_eq!(clamp_usize(Some("0".into()), 48, 1, 1000), 48);
        assert_eq!(clamp_usize(Some("99999".into()), 48, 1, 1000), 48);
        // Unparseable → default.
        assert_eq!(clamp_usize(Some("lots".into()), 48, 1, 1000), 48);
        // The circuit-breaker case: 0 is a VALID disable value when min allows it.
        assert_eq!(clamp_usize(Some("0".into()), 3, 0, 1000), 0);
        // Boundaries are inclusive.
        assert_eq!(clamp_usize(Some("1".into()), 48, 1, 1000), 1);
        assert_eq!(clamp_usize(Some("1000".into()), 48, 1, 1000), 1000);
    }

    #[test]
    fn active_store_count_counts_nonterminal_minus_in_memory() {
        use std::collections::HashSet;
        let path = std::env::temp_dir().join(format!("aish_active_{}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let store = CoordinatorStore::open(&path).unwrap();

        // A goal-loop generator turn (other-session/durable-only) — coordinating.
        store
            .insert("goal-abc", "pursue goal", "sess-a", None)
            .unwrap();
        // A run awaiting a batch — also active.
        store
            .insert("run_await", "fan out", "sess-b", None)
            .unwrap();
        store.set_phase("run_await", "awaiting_batch").unwrap();
        // A finished run — terminal, must NOT count.
        store
            .insert("run_done", "done work", "sess-c", None)
            .unwrap();
        store.set_done("run_done", "result").unwrap();
        // A failed run — terminal, must NOT count.
        store.insert("run_failed", "broke", "sess-d", None).unwrap();
        store.set_failed("run_failed", "boom").unwrap();
        // This session's own worker, ALSO tracked in-memory (deduped out so it
        // isn't double-counted against worker::running_count).
        store
            .insert("worker_7", "my worker", "sess-me", None)
            .unwrap();

        let in_memory: HashSet<String> = ["worker_7".to_string()].into_iter().collect();
        // goal-abc + run_await = 2 active; run_done/run_failed terminal; worker_7 deduped.
        assert_eq!(active_store_count(&store, &in_memory), 2);

        // With nothing tracked in-memory, the own-worker row counts too → 3.
        assert_eq!(active_store_count(&store, &HashSet::new()), 3);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn fresh_heartbeat_is_not_stale_missing_is() {
        // A heartbeat "now" is fresh.
        let now = now_unix_secs();
        let fresh = unix_to_sqlite(now);
        assert!(!heartbeat_is_stale(Some(&fresh)));
        // An ancient heartbeat is stale.
        let old = unix_to_sqlite(now - 60 * 60); // an hour ago > 15m
        assert!(heartbeat_is_stale(Some(&old)));
        // Missing/garbage heartbeats are stale.
        assert!(heartbeat_is_stale(None));
        assert!(heartbeat_is_stale(Some("garbage")));
    }

    /// Build a minimal CoordinatorRow fixture for the orphan-reap tests.
    fn row(
        run_id: &str,
        phase: &str,
        session_id: Option<&str>,
        heartbeat_at: Option<String>,
    ) -> crate::db::CoordinatorRow {
        crate::db::CoordinatorRow {
            run_id: run_id.into(),
            task: "t".into(),
            phase: phase.into(),
            result: None,
            error: None,
            session_id: session_id.map(str::to_string),
            session_name: None,
            created_at: None,
            heartbeat_at,
            tokens_in: 0,
            tokens_out: 0,
            turns: 0,
            tool_calls: 0,
        }
    }

    #[test]
    fn is_orphaned_row_matrix() {
        let now = now_unix_secs();
        let stale = Some(unix_to_sqlite(now - 60 * 60)); // 1h ago > 15m
        let fresh = Some(unix_to_sqlite(now));

        // Orphan: coordinating, foreign owner, stale heartbeat.
        assert!(is_orphaned_row(
            Some("other"),
            "me",
            &Phase::Coordinating,
            stale.as_deref()
        ));
        // Orphan: awaiting_batch counts too.
        assert!(is_orphaned_row(
            Some("other"),
            "me",
            &Phase::AwaitingBatch,
            stale.as_deref()
        ));
        // Orphan: missing heartbeat is stale.
        assert!(is_orphaned_row(Some("other"), "me", &Phase::Coordinating, None));

        // NOT orphan: it's mine (same session), even if stale.
        assert!(!is_orphaned_row(
            Some("me"),
            "me",
            &Phase::Coordinating,
            stale.as_deref()
        ));
        // NOT orphan: foreign but heartbeat fresh (owner still alive).
        assert!(!is_orphaned_row(
            Some("other"),
            "me",
            &Phase::Coordinating,
            fresh.as_deref()
        ));
        // NOT orphan: terminal phases are never reaped.
        assert!(!is_orphaned_row(Some("other"), "me", &Phase::Done, None));
        assert!(!is_orphaned_row(Some("other"), "me", &Phase::Failed, None));
    }

    #[test]
    fn orphaned_run_ids_selects_only_stale_unowned_nonterminal() {
        let now = now_unix_secs();
        let stale = || Some(unix_to_sqlite(now - 60 * 60));
        let fresh = || Some(unix_to_sqlite(now));
        let rows = vec![
            row("zombie", "coordinating", Some("gone"), stale()), // reap
            row("zombie_batch", "awaiting_batch", Some("gone"), stale()), // reap
            row("no_beat", "coordinating", Some("gone"), None),   // reap
            row("mine", "coordinating", Some("me"), stale()),     // keep (mine)
            row("live", "coordinating", Some("other"), fresh()),  // keep (fresh)
            row("done", "done", Some("other"), stale()),          // keep (terminal)
            row("failed", "failed", Some("other"), stale()),      // keep (terminal)
        ];
        let mut ids = orphaned_run_ids(&rows, "me");
        ids.sort();
        assert_eq!(ids, vec!["no_beat", "zombie", "zombie_batch"]);
    }

    #[test]
    fn reap_orphaned_runs_flips_zombie_but_spares_fresh_and_own() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("reap_orphan_{}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let store = CoordinatorStore::open(&path).unwrap();
        // Foreign, non-terminal — but its heartbeat is FRESH (insert stamps
        // current_timestamp), so it must NOT be reaped.
        store.insert("foreign_fresh", "task", "other", None).unwrap();
        // This session's own non-terminal run — never reaped regardless.
        store.insert("mine", "task", "me", None).unwrap();

        // Nothing is stale yet → zero reaped, both rows stay coordinating.
        assert_eq!(reap_orphaned_runs(&store, "me"), 0);
        let phase_of = |id: &str| {
            store
                .load_all()
                .unwrap()
                .into_iter()
                .find(|r| r.run_id == id)
                .map(|r| r.phase)
        };
        assert_eq!(phase_of("foreign_fresh").as_deref(), Some("coordinating"));
        assert_eq!(phase_of("mine").as_deref(), Some("coordinating"));

        let _ = std::fs::remove_file(&path);
    }

    /// Test helper: unix seconds → SQLite `current_timestamp` string (UTC).
    /// Inverse of `parse_sqlite_timestamp`, used only to build test fixtures.
    fn unix_to_sqlite(secs: i64) -> String {
        let days = secs.div_euclid(86_400);
        let rem = secs.rem_euclid(86_400);
        let (h, m, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
        // Inverse civil algorithm (Hinnant's civil_from_days).
        let z = days + 719_468;
        let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
        let doe = z - era * 146_097;
        let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
        let y = yoe + era * 400;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let d = doy - (153 * mp + 2) / 5 + 1;
        let mth = if mp < 10 { mp + 3 } else { mp - 9 };
        let year = if mth <= 2 { y + 1 } else { y };
        format!("{year:04}-{mth:02}-{d:02} {h:02}:{m:02}:{s:02}")
    }
}
