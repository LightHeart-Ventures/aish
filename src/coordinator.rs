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
use std::time::Duration;

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
    env_usize("AISH_COORDINATOR_MAX_FAILED_ATTEMPTS", DEFAULT_MAX_FAILED_ATTEMPTS, 0, 1000)
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
        matches!(self, Phase::Coordinating | Phase::AwaitingBatch)
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
fn fold_operator_messages(store: Option<&CoordinatorStore>, run_id: &str, next_input: &mut String) -> usize {
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
    if let Some(s) = store {
        // session.session_id/name were adopted from the LAUNCHING session at
        // startup (see main.rs), so the row attributes to who asked for the work.
        let _ = s.insert(run_id, &input, &session.session_id, session.name.as_deref());
    }

    // ── Pre-dispatch circuit breaker (loop guard, per the loop-exhaustion
    // review). If this exact task has already failed `max_failed_attempts()`
    // times, a fresh identical run is very unlikely to fare differently —
    // refuse fast instead of burning another full multi-round attempt on a
    // known-bad request. The current run's own row (just inserted as
    // `coordinating`) is never counted; only prior `failed` rows are. The
    // counter is per-store-lifetime (terminal rows are purged on a clean
    // restart via `clear_finished`), so this stops in-session re-dispatch
    // storms — e.g. a goal loop relaunching the same task — rather than acting
    // as cross-restart history. Disabled when the threshold is 0.
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
codes, diffs).\n\nDECISION POINTS — avoid loops: If you notice you are repeating the same action or re-deriving a \
fact you already have, STOP and change approach. After about 3 failed attempts at the SAME \
sub-problem, do NOT keep retrying the same way — either try a materially different approach or \
stop and report explicitly: say \"I'm blocked because <specific reason>\", list what you tried \
and what you observed, and give your best partial result. A clearly-stated blocker is a \
successful outcome; an endless retry loop is a failure.\n\nWRAPPING UP — open a draft PR for \
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
            let error = format!("coordinator exceeded the {round_cap}-round cap");
            if let Some(s) = store {
                let _ = s.set_failed(run_id, &error);
            }
            return Outcome { phase: Phase::Failed, result: None, error: Some(error), rounds };
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
                if let Some(s) = store {
                    let _ = s.set_failed(run_id, &error);
                }
                return Outcome { phase: Phase::Failed, result: None, error: Some(error), rounds };
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

        // ── Worker-exit evaluation: did this round's turn end abnormally? The
        // engine tags a loop-detected / forced-summarize / budget-exhausted stop
        // with a parseable banner on the first line of the answer. Decide a
        // recovery disposition rather than treating the (possibly partial) answer
        // as a finished result.
        if let Some(reason) = crate::loopguard::ExitReason::parse_banner(&answer) {
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
            if let Some(s) = store {
                let _ = s.set_failed(run_id, &error);
            }
            return Outcome { phase: Phase::Failed, result: Some(answer), error: Some(error), rounds };
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
            next_input = "The background sub-tasks you offloaded have completed (their results were \
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
        if let Some(s) = store {
            let _ = s.set_done(run_id, &answer);
        }
        return Outcome { phase: Phase::Done, result: Some(answer), error: None, rounds };
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
    let own = session.session_id.clone();
    let mut surfaced = 0usize;
    let mut reaped = 0usize;
    for row in rows {
        let phase = Phase::parse(&row.phase);
        match phase {
            Phase::Done => {
                // Surface a completed background coordinator result once, then
                // clear it so we don't re-announce on the next start.
                if let Some(result) = &row.result {
                    if !result.trim().is_empty() {
                        print_completed(&row.run_id, result);
                        surfaced += 1;
                    }
                }
            }
            Phase::Failed => {} // terminal, nothing to do (cleared below)
            Phase::Coordinating | Phase::AwaitingBatch => {
                // Non-terminal. If it's ours (same session id — only possible
                // after an in-process resume, since ids are per-run) or its
                // heartbeat is fresh, leave it; otherwise it's orphaned.
                let mine = row.session_id.as_deref() == Some(own.as_str());
                if !mine && heartbeat_is_stale(row.heartbeat_at.as_deref()) {
                    let _ = store.set_failed(
                        &row.run_id,
                        "orphaned: owner gone and heartbeat stale (reaped on startup)",
                    );
                    reaped += 1;
                }
            }
        }
    }
    // Drop terminal rows (the just-surfaced done runs and any failed/reaped ones)
    // so the store doesn't grow without bound and a restart stays a no-op. This
    // also purges any orphaned mailbox messages (see CoordinatorStore::clear_finished).
    let _ = store.clear_finished();
    if surfaced > 0 || reaped > 0 {
        eprintln!(
            "\x1b[2maish: reattached coordinator runs ({surfaced} delivered, {reaped} reaped)\x1b[0m"
        );
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn phase_string_roundtrip_is_total() {
        for p in [Phase::Coordinating, Phase::AwaitingBatch, Phase::Done, Phase::Failed] {
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
        store.enqueue_message("run_x", "use the staging DB", None).unwrap();
        let mut input = "continue the task".to_string();
        let n = fold_operator_messages(Some(&store), "run_x", &mut input);
        assert_eq!(n, 1);
        assert!(input.contains("use the staging DB"));
        assert!(input.trim_end().ends_with("continue the task"), "original input kept after the interjection");
        let interj = input.find("Operator interjection").unwrap();
        let cont = input.find("continue the task").unwrap();
        assert!(interj < cont, "interjection is prepended");
        // Delete-on-read: a second fold sees nothing.
        let mut input2 = "next".to_string();
        assert_eq!(fold_operator_messages(Some(&store), "run_x", &mut input2), 0);
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
        assert_eq!(parse_sqlite_timestamp("2021-01-01 00:00:00"), Some(1_609_459_200));
        // Leap-year day handled by the civil algorithm.
        assert_eq!(parse_sqlite_timestamp("2020-02-29 12:00:00"), Some(1_582_977_600));
        // Malformed → None.
        assert_eq!(parse_sqlite_timestamp("not a timestamp"), None);
        assert_eq!(parse_sqlite_timestamp("2021-13"), None);
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
        store.insert("goal-abc", "pursue goal", "sess-a", None).unwrap();
        // A run awaiting a batch — also active.
        store.insert("run_await", "fan out", "sess-b", None).unwrap();
        store.set_phase("run_await", "awaiting_batch").unwrap();
        // A finished run — terminal, must NOT count.
        store.insert("run_done", "done work", "sess-c", None).unwrap();
        store.set_done("run_done", "result").unwrap();
        // A failed run — terminal, must NOT count.
        store.insert("run_failed", "broke", "sess-d", None).unwrap();
        store.set_failed("run_failed", "boom").unwrap();
        // This session's own worker, ALSO tracked in-memory (deduped out so it
        // isn't double-counted against worker::running_count).
        store.insert("worker_7", "my worker", "sess-me", None).unwrap();

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
