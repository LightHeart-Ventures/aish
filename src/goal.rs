//! Goals — two complementary concepts live here:
//!
//! 1. **`Goal`** (domain model, further down) — a durable, structured record of
//!    something the user wants to achieve: a title/description, a lifecycle
//!    `status` (active|paused|completed|abandoned), `milestones`, `blockers`,
//!    `linked_tasks`, and an optional `parent_id` for subgoal nesting. Persisted
//!    in `aish.db` (the `goals` table) via `crate::db` helpers — loaded on
//!    session start, saved on mutation. Independent of any execution engine.
//!
//! 2. **`GoalLoop`** (below) — the background *pursuit* engine: a
//!    stopping-oracle modeled on Claude Code's `/goal`. It is ONE way a goal can
//!    be executed (the generator/verifier batch loop). A domain `Goal` can carry
//!    such a pursuit, but its structure (milestones, hierarchy, links) outlives
//!    any single loop.
//!
//! Background goal loop — a stopping-oracle modeled on Claude Code's `/goal`,
//! adapted to run as background batch work (non-blocking) and gated on `:batch`.
//!
//! Generator/verifier split: each turn a full-tool **worker** (the generator)
//! pursues the condition, then a separate **judge** call on the batch model (the
//! verifier) reads the worker's output and decides — yes/no + a one-line reason —
//! whether the goal is demonstrably met. A "no" feeds the reason forward as
//! guidance for the next turn; a "yes" delivers the result and stops.
//!
//! Stop/safety: like `/goal`, the real bound is a turn/time clause the user puts
//! in the condition (the judge reads it from the transcript). Because this runs
//! UNATTENDED in the background, we add a hard `MAX_TURNS` backstop so a
//! misjudged loop can't spend forever.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Unattended runaway backstop. `/goal` itself has none (the user can Ctrl-C);
/// a background loop can't be watched, so we cap it.
const MAX_TURNS: usize = 25;

/// How many times the goal loop will REVIEW-ANALYZE-REPLAN after a worker turn
/// ends abnormally (most often the coordinator flagging for operator once its
/// own serial-chain / loop-guard auto-recovery budget is spent) before treating
/// the goal as failed. A worker failure is NOT proof the goal is impossible, so
/// we fold the error into the next turn's guidance and drive another attempt —
/// bounded here so a persistently-failing worker still terminates the goal. The
/// streak resets after any productive (non-erroring) turn, so this caps
/// CONSECUTIVE failures, not lifetime ones.
const MAX_GOAL_RECOVERIES: usize = 3;

/// Bounded transcript ring the goal keeps for `:attach goal` / Shift-Tab replay
/// — the goal analogue of a worker's captured activity. A goal is essentially a
/// specialized worker, so its attach/cycle UI mirrors a worker's: header + input
/// row + activity tail. We cap the ring so a long-running (up to `MAX_TURNS`)
/// pursuit can't grow it unbounded; the replay tails the last ~screen anyway.
const TRANSCRIPT_CAP: usize = 200;

const MESSAGES_API: &str = "https://api.anthropic.com/v1/messages";
/// Cap the work output handed to the judge so a chatty turn can't blow the
/// verifier's context.
const JUDGE_INPUT_CAP: usize = 16_000;

#[derive(Clone, Copy, PartialEq)]
enum Status {
    Active,
    Achieved,
    Failed,
    Cleared,
}

/// What the loop is doing right now within a turn (work → check), or between turns.
#[derive(Clone, Copy, PartialEq)]
enum Step {
    Idle,
    Working,
    Checking,
}

pub struct GoalLoop {
    pub condition: String,
    /// When the goal was first spawned — basis for total elapsed time.
    started: Instant,
    /// Lifecycle-hook registry inherited from the launching session, plus the
    /// envelope fields (`session_id`/`cwd`/`mode`) every hook payload carries.
    /// The goal loop is decoupled from `Session`, so it snapshots what it needs
    /// to build + fire `GoalStart`/`GoalTurnEnd`/`GoalEnd` payloads on its own.
    hooks: crate::hooks::HookSet,
    session_id: String,
    cwd: std::path::PathBuf,
    mode: String,
    inner: Mutex<Inner>,
}

struct Inner {
    status: Status,
    turns: usize,
    last_reason: Option<String>,
    cancel: bool,
    /// When the current turn began — basis for per-turn elapsed time.
    turn_started: Option<Instant>,
    /// Current phase within the loop.
    phase: Step,
    /// Bounded activity transcript (newest last), replayed on `:attach goal` /
    /// Shift-Tab so the goal's history renders like a worker's. Capped at
    /// [`TRANSCRIPT_CAP`] lines.
    transcript: Vec<String>,
    /// Operator steer mailbox — instructions queued via `:tell goal <msg>` or by
    /// typing while `:attach`ed to the goal. Drained at the START of each turn
    /// (see [`GoalLoop::take_steers`]) and folded into that turn's generator
    /// directive by [`compose_guidance`], so the human can course-correct a
    /// running goal mid-flight the way `:tell` steers a coordinator. FIFO.
    steers: Vec<String>,
}

pub type Handle = Arc<GoalLoop>;

impl GoalLoop {
    /// True while the loop is still pursuing the goal.
    pub fn is_active(&self) -> bool {
        let i = self.inner.lock().unwrap();
        i.status == Status::Active && !i.cancel
    }

    /// Request the loop stop (it checks between turns; a worker turn already in
    /// flight finishes first).
    pub fn clear(&self) {
        let mut i = self.inner.lock().unwrap();
        i.cancel = true;
        if i.status == Status::Active {
            i.status = Status::Cleared;
        }
    }

    /// Queue an operator steer instruction (`:tell goal <msg>` or a line typed
    /// while `:attach`ed to the goal). Folded into the NEXT turn's generator
    /// directive so the human can course-correct a running goal without killing
    /// it — the goal analogue of `:tell`-ing a coordinator. Returns `false`
    /// (nothing queued) when the message is blank or the loop is no longer
    /// active, so the caller can tell the operator there was nothing to steer.
    pub fn steer(&self, msg: &str) -> bool {
        let msg = msg.trim();
        if msg.is_empty() {
            return false;
        }
        {
            let mut i = self.inner.lock().unwrap();
            if i.status != Status::Active || i.cancel {
                return false;
            }
            i.steers.push(msg.to_string());
        }
        self.note(&format!("operator steer queued: {msg}"));
        true
    }

    /// Drain the pending operator steer messages (FIFO). Called once at the top
    /// of each turn so a steer applies to the turn it precedes and is not
    /// replayed on later turns (one-shot, like a coordinator `:tell`).
    fn take_steers(&self) -> Vec<String> {
        let mut i = self.inner.lock().unwrap();
        std::mem::take(&mut i.steers)
    }

    /// Fire a goal-lifecycle hook (observe-only, best-effort, off the hot path).
    /// No-op with zero allocation when no hook listens for `event`. The payload
    /// is stamped with [`crate::hooks::Agent::Goal`] so a hook can scope itself
    /// to the autonomous goal loop; `build` attaches the event-specific fields
    /// (condition, turn, met, status, reason). Requires a tokio runtime in scope
    /// — every call site is inside `run_goal`, which is spawned onto one.
    fn fire_hook(
        &self,
        event: crate::hooks::HookEvent,
        build: impl FnOnce(crate::hooks::HookPayload) -> crate::hooks::HookPayload,
    ) {
        if !self.hooks.has(event) {
            return;
        }
        let p = crate::hooks::HookPayload::new(
            event,
            &self.session_id,
            crate::hooks::Agent::Goal,
            &self.cwd,
            &self.mode,
        );
        self.hooks.fire_observe(event, build(p));
    }

    /// One-line `:goal` status report — includes overall + current-turn elapsed
    /// and the current phase while active; a finished goal shows just the final
    /// state and total elapsed.
    pub fn status_line(&self) -> String {
        let i = self.inner.lock().unwrap();
        let total = fmt_duration(self.started.elapsed());
        let reason = i
            .last_reason
            .as_deref()
            .map(|r| format!(" · last check: {r}"))
            .unwrap_or_default();
        let condition = truncate_condition(&self.condition);

        if i.status == Status::Active {
            let phase = match i.phase {
                Step::Working => "working",
                Step::Checking => "checking",
                Step::Idle => "starting",
            };
            // Per-turn elapsed only makes sense once a turn is under way.
            let this_turn = match i.turn_started {
                Some(t) => format!("{} this turn / {} total", fmt_duration(t.elapsed()), total),
                None => format!("{total} total"),
            };
            format!(
                "goal [active · {phase}] · turn {} · {this_turn} · {condition}{reason}",
                i.turns
            )
        } else {
            let state = match i.status {
                Status::Achieved => "achieved",
                Status::Failed => "failed",
                Status::Cleared => "cleared",
                Status::Active => unreachable!(),
            };
            format!(
                "goal [{state}] · {} turn(s) · {total} total · {condition}{reason}",
                i.turns
            )
        }
    }

    /// Record an activity line into the bounded replay transcript, then surface
    /// it as a transient `[goal]` progress line over the prompt. A goal is a
    /// specialized worker, so — like a worker capturing its forwarded activity —
    /// every per-turn announcement is also retained so `:attach goal` /
    /// Shift-Tab can replay the goal's history tail (see [`attach_backfill`]).
    fn note(&self, line: &str) {
        self.record(line);
        announce(line);
    }

    /// Like [`note`], but records the line into the replay transcript WITHOUT
    /// surfacing a transient `[goal]` line over the prompt. Used for the noisy
    /// per-turn `working…`/`checking…` progress ticks: they stay available for
    /// `:attach goal` / Shift-Tab replay but no longer spam the live console.
    fn note_quiet(&self, line: &str) {
        self.record(line);
    }

    /// Append `line` to the bounded replay transcript (no console output).
    fn record(&self, line: &str) {
        let mut i = self.inner.lock().unwrap();
        i.transcript.push(line.to_string());
        let len = i.transcript.len();
        if len > TRANSCRIPT_CAP {
            i.transcript.drain(0..len - TRANSCRIPT_CAP);
        }
    }

    /// Backfill rows for `:attach goal` / Shift-Tab (TASK-301) — the goal
    /// analogue of a worker's replayed activity tail. A goal is essentially a
    /// specialized worker, so its attach UI is rendered identically (header +
    /// input row + activity rows) by [`crate::repl::backfill_goal_attached`].
    /// This returns the ACTIVITY rows: the captured per-turn transcript, with
    /// the current one-line status appended last so the tail always shows live
    /// phase/turn/elapsed/last-verifier-check context. Newest last.
    pub fn attach_backfill(&self) -> Vec<String> {
        let i = self.inner.lock().unwrap();
        let mut rows = i.transcript.clone();
        drop(i);
        rows.push(self.status_line());
        rows
    }


    fn set(&self, status: Status, reason: Option<String>) {
        let mut i = self.inner.lock().unwrap();
        i.status = status;
        if reason.is_some() {
            i.last_reason = reason;
        }
    }
}

/// Human-readable elapsed time: `45s`, `4m12s`, `1h03m`. Coarse on purpose —
/// seconds drop off once we're past an hour.
fn fmt_duration(d: Duration) -> String {
    let secs = d.as_secs();
    let (h, m, s) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    if h > 0 {
        format!("{h}h{m:02}m")
    } else if m > 0 {
        format!("{m}m{s:02}s")
    } else {
        format!("{s}s")
    }
}

/// Keep the condition snippet on one line — long goals get an ellipsis.
pub(crate) fn truncate_condition(condition: &str) -> String {
    truncate_ellipsis(condition, 60)
}

/// Ellipsize `s` to at most `max` chars (a trailing `…` is added when it's
/// clipped) so a value stays on one compact line. Shared by `truncate_condition`
/// (the `:goal` status line) and [`Goal::prompt_summary`] (the TASK-279 per-turn
/// system-prompt block), so both truncate identically.
pub fn truncate_ellipsis(s: &str, max: usize) -> String {
    if s.chars().count() > max {
        let head: String = s.chars().take(max).collect();
        format!("{}…", head.trim_end())
    } else {
        s.to_string()
    }
}

/// Start pursuing `condition` in the background. The work runs as full-tool
/// coordinator subprocesses (`worker::run_once`); the verifier judges on
/// `model` (the batch model). Returns a handle the REPL reads for `:goal` status.
pub fn spawn(
    condition: String,
    spec: crate::worker::WorkerSpec,
    model: String,
    cred: crate::backend::claude::Credential,
    hooks: crate::hooks::HookSet,
    mode: String,
) -> Handle {
    let goal = Arc::new(GoalLoop {
        condition,
        started: Instant::now(),
        hooks,
        // The launching session's id + cwd, snapshotted for the hook envelope.
        session_id: spec.launch_session_id.clone(),
        cwd: spec.cwd.clone(),
        mode,
        inner: Mutex::new(Inner {
            status: Status::Active,
            turns: 0,
            last_reason: None,
            cancel: false,
            turn_started: None,
            phase: Step::Idle,
            transcript: Vec::new(),
            steers: Vec::new(),
        }),
    });
    tokio::spawn(run_goal(goal.clone(), spec, model, cred));
    goal
}

/// Shared opening block of every goal-turn generator directive. Kept as a const
/// so the builder ([`goal_directive`]) and its inverse
/// ([`goal_condition_from_directive`]) can never drift apart — the inverse backs
/// the `:workers` goal-turn coalescing (TASK-302).
///
/// The prefix also encodes the operator-requested plan-first + task-lifecycle
/// discipline: the first turn must deconstruct/restate the goal, state its
/// constraints and a measurable definition of success, break the goal into
/// bite-size, right-sized sub-tasks (a review→design→build pipeline for a
/// mega-task; file-locality grouping so several TODOs touching one file go to a
/// single worker instead of colliding parallel ones), map dependencies and
/// parallelism, and persist that plan durably BEFORE building. Execution then
/// fans independent sub-work out via `run_in_background`, tracks the work as a
/// board task (spec/comments/branch/PR kept current, moved to completed only once
/// verified), and has an independent verifier fact-check key results so a wrong
/// intermediate answer can't cascade.
///
/// Because each goal turn runs as a full-tool coordinator subprocess
/// ([`crate::worker::run_once`]), it has the always-surfaced `message_console`
/// channel. We instruct it to post a per-turn note so the operator sees live
/// progress even while the goal loop runs unattended: (1) a turn summary, and
/// (2) any pull request it opened, with a one-line summary. `message_console`
/// emits a `📣` sentinel line that is surfaced to the operator regardless of the
/// `:worker-output` gate, without polluting the stdout result the verifier judges.
///
/// The reverse-parser strips this ENTIRE prefix, so keeping the reporting
/// instructions here (before the condition) leaves the recovered condition clean
/// and the `:workers` grouping key stable.
const GOAL_DIRECTIVE_PREFIX: &str =
    "Work toward this goal, then report what you did and the evidence.\n\n\
Plan before you build. On your FIRST turn (and whenever no plan exists yet), do this BEFORE writing any code or opening any change:\n\
1. Deconstruct the goal and restate it in your own words in 60 characters or less.\n\
2. Identify the constraints and limits — time, budget, access, and the tools/permissions the work needs.\n\
3. State concretely and measurably what success looks like — the evidence that will prove the goal is met.\n\
4. Break the goal into bite-size sub-tasks and right-size the work so no single worker bites off more than it can finish in one focused pass:\n\
   a. Decompose a large or multi-phase goal into a logical pipeline of stages with real dependencies — e.g. review → design → build → test — and run the stages in order rather than handing one worker the whole mega-task at once.\n\
   b. Size each sub-task so ONE worker can complete it in a single focused pass; if a unit spans many files or several unrelated concerns, split it further until each piece is self-contained.\n\
   c. Optimize by file locality: bundle sub-tasks or TODOs that all touch the SAME file(s) into ONE worker so it does them together — do NOT split edits to a shared file across parallel workers that would collide, reload the same context, or conflict on the same lines.\n\
   Then map the dependencies between the bite-size sub-tasks and note which are independent and file-disjoint (safe to run in parallel) versus which must wait on an upstream stage.\n\
5. Persist that plan durably so it survives a restart: record the goal, its sub-tasks/milestones, dependencies, and success criteria in a durable store (the goal store or an aish.db table). Mark each sub-task complete in that store as you finish it.\n\n\
Then build toward the goal:\n\
6. Dispatch the independent, file-disjoint sub-tasks in parallel with the run_in_background tool — one worker per bite-size unit, and one worker for all the TODOs that share a file — keeping serial only the work with real dependencies or that touches shared files.\n\
7. Track the work as a board task: open a task (or reuse the linked one), keep its spec, comments, branch, and pull-request fields up to date as you progress, and move it to completed only once the goal is verified done.\n\
8. Guard against cascading errors: have an independent agent or verifier fact-check each key result before you build further on it.\n\n\
Before you finish this turn, call the `message_console` tool once to surface your progress to the operator:\n\
1. A one- to two-line summary of what you did this turn and the evidence for it (on the first turn, include your 60-character restatement and your success definition).\n\
2. If you opened a pull request this turn, include its number/URL and a one-line summary of what it changes.\n\n\
Goal:\n";
/// Marker separating the goal condition from the verifier's last-check guidance
/// in a re-tried turn's directive.
const GOAL_DIRECTIVE_GUIDANCE_MARKER: &str = "\n\nThe goal is NOT yet met — last check said: ";

/// Build the per-turn generator directive. `guidance` is the verifier's feedback
/// from the previous turn (`None` on the first turn). Single source of truth for
/// the directive shape so [`goal_condition_from_directive`] can reverse it.
pub(crate) fn goal_directive(condition: &str, guidance: Option<&str>) -> String {
    match guidance {
        Some(g) => {
            format!("{GOAL_DIRECTIVE_PREFIX}{condition}{GOAL_DIRECTIVE_GUIDANCE_MARKER}{g}")
        }
        None => format!("{GOAL_DIRECTIVE_PREFIX}{condition}"),
    }
}

/// Build the guidance woven into the NEXT goal-turn directive after a worker turn
/// ended abnormally (`run_once` returned `Err`). Framed to read naturally after
/// the [`GOAL_DIRECTIVE_GUIDANCE_MARKER`] ("last check said: …") so the generator
/// treats it as an interruption to recover from, not a fresh goal. It carries the
/// raw error (so the model can REVIEW + ANALYZE the actual cause) and, when the
/// error is the tell-tale serial-chain / single-call / batching flag, an explicit
/// instruction to batch independent tool calls — the exact re-plan the loop guard
/// was asking for. Kept as a named helper so it is unit-testable in isolation.
pub(crate) fn recovery_guidance(err: &str) -> String {
    let low = err.to_lowercase();
    let batching_flag = low.contains("single-call")
        || low.contains("serial chain")
        || low.contains("serial-chain")
        || low.contains("batch");
    let mut g = format!(
        "your previous work turn was cut short by an internal execution guard, \
         NOT because the goal is impossible: {err}. Do not restart from scratch and \
         do not abandon the goal — REVIEW that error, ANALYZE the root cause, then \
         resume from where you left off and adjust your approach so the same guard \
         does not trip again."
    );
    if batching_flag {
        g.push_str(
            " In particular: fire every INDEPENDENT tool call together in ONE turn \
             (one batch of reads/greps/list_dirs/status queries up front) instead of \
             one call per round — only keep a call serial when its input genuinely \
             depends on a previous call's output. Front-load your context-gathering, \
             then act.",
        );
    }
    g
}

/// Inverse of [`goal_directive`]: recover the goal condition from a persisted
/// goal-turn `task` string. Returns `None` when `task` isn't a goal directive,
/// so non-goal coordinator rows pass through un-grouped. Used by `:workers` to
/// collapse a goal loop's per-turn `goal-<uuid>` coordinator rows under ONE goal
/// — a multi-turn goal then renders a single row instead of one-per-turn
/// (TASK-302).
pub(crate) fn goal_condition_from_directive(task: &str) -> Option<String> {
    let rest = task.strip_prefix(GOAL_DIRECTIVE_PREFIX)?;
    let condition = match rest.split_once(GOAL_DIRECTIVE_GUIDANCE_MARKER) {
        Some((cond, _guidance)) => cond,
        None => rest,
    };
    Some(condition.to_string())
}

/// Merge the carried-forward verifier guidance with any fresh operator steer
/// messages (`:tell goal <msg>` or input typed while `:attach`ed) into the
/// single `guidance` string folded into this turn's generator directive.
///
/// Steer text is framed as a priority operator instruction. When a verifier
/// note is also pending, both are carried (steer first, since an explicit human
/// course-correction outranks the machine judge's last critique). Returns `None`
/// when there is neither, so the first unsteered turn's directive stays clean
/// and [`goal_condition_from_directive`] recovers a bare condition. Routed
/// through the SAME [`GOAL_DIRECTIVE_GUIDANCE_MARKER`] channel as verifier
/// feedback, so the `:workers` reverse-parser strips it identically and the
/// goal-coalescing key stays stable regardless of steer content.
pub(crate) fn compose_guidance(verifier: Option<&str>, steers: &[String]) -> Option<String> {
    if steers.is_empty() {
        return verifier.map(str::to_string);
    }
    let joined = steers.join(" | ");
    let steer_block = format!(
        "the operator steered this goal mid-flight — treat this as a priority \
         instruction and fold it into your approach: {joined}"
    );
    match verifier {
        Some(v) if !v.is_empty() => Some(format!("{steer_block}\n\n(prior verifier note: {v})")),
        _ => Some(steer_block),
    }
}

/// Terminal outcome of a goal pursuit, surfaced on the `GoalEnd` hook payload.
struct GoalOutcome {
    /// Wire status: `"achieved"` | `"failed"` | `"cleared"`.
    status: &'static str,
    /// Turns executed when the loop ended.
    turns: usize,
    /// Final one-line reason/verdict, when there is one.
    reason: Option<String>,
}

async fn run_goal(
    goal: Handle,
    spec: crate::worker::WorkerSpec,
    model: String,
    cred: crate::backend::claude::Credential,
) {
    goal.note(&format!("started — {}", goal.condition));
    goal.fire_hook(crate::hooks::HookEvent::GoalStart, |p| {
        p.with("condition", goal.condition.clone())
    });

    let outcome = run_goal_loop(&goal, spec, model, cred).await;

    goal.fire_hook(crate::hooks::HookEvent::GoalEnd, |p| {
        let p = p
            .with("status", outcome.status)
            .with("turns", outcome.turns as u64);
        match &outcome.reason {
            Some(r) => p.with("reason", r.clone()),
            None => p,
        }
    });
}

/// The generator/verifier loop. Returns the terminal [`GoalOutcome`] so the
/// caller ([`run_goal`]) can fire `GoalEnd` from exactly one place regardless of
/// which stopping condition ended the pursuit.
async fn run_goal_loop(
    goal: &Handle,
    spec: crate::worker::WorkerSpec,
    model: String,
    cred: crate::backend::claude::Credential,
) -> GoalOutcome {
    let mut guidance: Option<String> = None;
    // Consecutive abnormal-worker-turn recoveries spent so far (reset by any
    // productive turn). Bounds the REVIEW-ANALYZE-REPLAN loop below.
    let mut recoveries: usize = 0;

    loop {
        // Stop checks between turns.
        let turn = {
            let mut i = goal.inner.lock().unwrap();
            if i.cancel {
                return GoalOutcome {
                    status: "cleared",
                    turns: i.turns,
                    reason: None,
                };
            }
            if i.turns >= MAX_TURNS {
                i.status = Status::Failed;
                let turns = i.turns;
                drop(i);
                let reason =
                    format!("hit the {MAX_TURNS}-turn backstop without meeting the goal");
                goal.note(&format!("stopped — {reason}"));
                return GoalOutcome {
                    status: "failed",
                    turns,
                    reason: Some(reason),
                };
            }
            i.turns += 1;
            i.turns
        };

        // Fold any operator steer messages (`:tell goal` / typed input while
        // `:attach`ed) into this turn's guidance so the human can course-correct
        // a running goal mid-flight. Drained one-shot; `guidance` (verifier
        // feedback) still carries across turns independently.
        let steers = goal.take_steers();
        if !steers.is_empty() {
            goal.note(&format!(
                "turn {turn}: folding {} operator steer(s) into this turn",
                steers.len()
            ));
        }
        let effective = compose_guidance(guidance.as_deref(), &steers);
        // Generator: a full-tool worker pursues the goal with the latest guidance.
        let directive = goal_directive(&goal.condition, effective.as_deref());
        goal.note_quiet(&format!("turn {turn}: working…"));
        {
            let mut i = goal.inner.lock().unwrap();
            i.turn_started = Some(Instant::now());
            i.phase = Step::Working;
        }
        let run_id = format!("goal-{}", uuid::Uuid::new_v4());
        let output = match crate::worker::run_once(&spec, &directive, &run_id).await {
            Ok(o) => {
                // Productive turn — clear the failure streak so intermittent,
                // spread-out worker hiccups don't accumulate toward the cap.
                recoveries = 0;
                o
            }
            Err(e) => {
                // A worker turn can end abnormally — most commonly the nested
                // coordinator FLAGGING FOR OPERATOR once its own loop / serial-
                // chain guards exhaust their auto-recovery budget (e.g. "yielded
                // after N consecutive single-call rounds … re-plan toward
                // batching"). That is an execution-shape complaint, NOT proof the
                // goal is impossible. So instead of dying, the goal agent REVIEWS
                // the error, ANALYZES it, folds it into the next turn's guidance,
                // and drives another attempt that re-plans (e.g. batches the
                // independent calls). Bounded by MAX_GOAL_RECOVERIES so a
                // persistently-failing worker still terminates the goal.
                if recoveries < MAX_GOAL_RECOVERIES {
                    recoveries += 1;
                    goal.note(&format!(
                        "turn {turn} interrupted — reviewing, re-planning, retrying \
                         (recovery {recoveries}/{MAX_GOAL_RECOVERIES}): {e}"
                    ));
                    goal.set(Status::Active, Some(format!("recovering: {e}")));
                    guidance = Some(recovery_guidance(&e));
                    continue;
                }
                goal.set(Status::Failed, Some(e.clone()));
                goal.note(&format!(
                    "failed after {recoveries} recovery attempt(s) — {e}"
                ));
                return GoalOutcome {
                    status: "failed",
                    turns: turn,
                    reason: Some(e),
                };
            }
        };
        if !goal.is_active() {
            return GoalOutcome {
                status: "cleared",
                turns: turn,
                reason: None,
            };
        }

        // Verifier: the batch model judges whether the output demonstrates the goal.
        goal.note_quiet(&format!("turn {turn}: checking…"));
        goal.inner.lock().unwrap().phase = Step::Checking;
        let (met, reason) = match judge(&cred, &model, &goal.condition, &output).await {
            Ok((met, reason)) => (met, reason),
            // Couldn't verify — keep going but record why; don't silently stop.
            Err(e) => (false, format!("could not verify this turn: {e}")),
        };
        goal.fire_hook(crate::hooks::HookEvent::GoalTurnEnd, |p| {
            p.with("turn", turn as u64)
                .with("met", met)
                .with("reason", reason.clone())
        });
        if met {
            goal.set(Status::Achieved, Some(reason.clone()));
            deliver(goal, turn, &reason, &output);
            return GoalOutcome {
                status: "achieved",
                turns: turn,
                reason: Some(reason),
            };
        }
        goal.set(Status::Active, Some(reason.clone()));
        guidance = Some(reason);
    }
}

/// Ask the verifier (batch model) whether the goal is demonstrably met. Returns
/// `(met, reason)`. A strict judge: evidence in the output, not mere claims.
async fn judge(
    cred: &crate::backend::claude::Credential,
    model: &str,
    condition: &str,
    work: &str,
) -> Result<(bool, String), String> {
    let work = if work.chars().count() > JUDGE_INPUT_CAP {
        let head: String = work.chars().take(JUDGE_INPUT_CAP).collect();
        format!("{head}\n…(truncated)")
    } else {
        work.to_string()
    };
    let body = json!({
        "model": model,
        "max_tokens": 512,
        // Shaped per credential (OAuth needs the Claude Code identity block).
        "system": cred.system_value(
            "You are a strict completion judge for an autonomous agent. Decide whether the \
    GOAL is DEMONSTRABLY met by the WORK OUTPUT — judge only what the output shows as evidence (command \
    results, file contents, exit codes), never what is merely asserted without proof. If the goal \
    states a turn/time bound, honor it. Reply with ONLY a JSON object, no prose: \
    {\"met\": true|false, \"reason\": \"<one sentence>\"}.",
        ),
        "messages": [{
            "role": "user",
            "content": format!("GOAL:\n{condition}\n\nWORK OUTPUT:\n{work}")
        }]
    });
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(120))
        .build()
        .map_err(|e| e.to_string())?;
    let req = client
        .post(MESSAGES_API)
        .header("anthropic-version", "2023-06-01");
    let resp = cred
        .apply(req)
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("judge request failed: {e}"))?;
    let v: Value = resp
        .json()
        .await
        .map_err(|e| format!("judge returned non-JSON: {e}"))?;
    if let Some(msg) = v["error"]["message"].as_str() {
        return Err(format!("judge api error: {msg}"));
    }
    let text = v["content"]
        .as_array()
        .and_then(|a| {
            a.iter().find_map(|b| {
                if b["type"] == "text" {
                    b["text"].as_str()
                } else {
                    None
                }
            })
        })
        .unwrap_or("")
        .trim();
    // The judge should return bare JSON, but tolerate prose around a {...}.
    let parsed: Value = serde_json::from_str(text)
        .or_else(|_| {
            match (text.find('{'), text.rfind('}')) {
                (Some(s), Some(e)) if e > s => serde_json::from_str(&text[s..=e]),
                _ => serde_json::from_str(text), // re-raise the original error
            }
        })
        .map_err(|e| format!("couldn't parse judge verdict: {e} (got: {text})"))?;
    let met = parsed["met"].as_bool().unwrap_or(false);
    let reason = parsed["reason"]
        .as_str()
        .unwrap_or("(no reason given)")
        .to_string();
    Ok((met, reason))
}

/// Print a transient `[goal]` progress/announce line over the prompt.
fn announce(line: &str) {
    crate::tools::announce("[goal]", line);
}

/// Deliver the achieved outcome over the prompt (rendered markdown), like a
/// finished batch result.
fn deliver(goal: &Handle, turns: usize, reason: &str, output: &str) {
    use std::io::Write;
    print!("\r\x1b[2K");
    println!("\x1b[2m── goal achieved ({turns} turn(s)) ──\x1b[0m");
    println!("\x1b[2m{reason}\x1b[0m");
    println!("{}", crate::md::render_stdout(output.trim()));
    let _ = goal; // handle kept for symmetry / future status integration
    std::io::stdout().flush().ok();
}

// ───────────────────────── Domain model ─────────────────────────
//
// The durable, structured `Goal` record (AC1/AC2/AC4 of TASK-277). This is
// intentionally decoupled from the `GoalLoop` pursuit engine above: a goal is a
// plan (title, milestones, blockers, links, hierarchy) that persists in
// `aish.db`; the loop is one optional way to *execute* it.

/// Current unix time in whole seconds — the timestamp basis for goal records.
/// Monotonicity isn't required (these are wall-clock audit stamps), so a clock
/// skew just yields a slightly-off `created_at`/`updated_at`, never a panic.
fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Lifecycle state of a persistent [`Goal`]. Distinct from the pursuit loop's
/// internal `Status` (which tracks a single background run).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum GoalStatus {
    /// Being actively pursued / worked toward (the default for a new goal).
    #[default]
    Active,
    /// Deliberately set aside — kept, but not being worked right now.
    Paused,
    /// Achieved. Terminal.
    Completed,
    /// Dropped without completing. Terminal.
    Abandoned,
}

impl GoalStatus {
    /// Canonical lowercase token used for the DB `CHECK` constraint + JSON.
    pub fn as_str(&self) -> &'static str {
        match self {
            GoalStatus::Active => "active",
            GoalStatus::Paused => "paused",
            GoalStatus::Completed => "completed",
            GoalStatus::Abandoned => "abandoned",
        }
    }

    /// Parse a stored token back into a status. Unknown/empty falls back to
    /// `Active` so a hand-edited or future-versioned row never hard-fails a load.
    pub fn from_token(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "paused" => GoalStatus::Paused,
            "completed" => GoalStatus::Completed,
            "abandoned" => GoalStatus::Abandoned,
            _ => GoalStatus::Active,
        }
    }

    /// A terminal status can't transition further (used by callers / UI to
    /// gray-out actions).
    pub fn is_terminal(&self) -> bool {
        matches!(self, GoalStatus::Completed | GoalStatus::Abandoned)
    }
}

impl std::fmt::Display for GoalStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A concrete checkpoint on the way to a goal. `done` flips as it's achieved.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Milestone {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub done: bool,
}

impl Milestone {
    pub fn new(title: impl Into<String>) -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            title: title.into(),
            done: false,
        }
    }
}

/// Something impeding progress toward a goal. `resolved` flips when cleared.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Blocker {
    pub id: String,
    pub description: String,
    #[serde(default)]
    pub resolved: bool,
}

impl Blocker {
    pub fn new(description: impl Into<String>) -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            description: description.into(),
            resolved: false,
        }
    }
}

/// A reference to an external work item this goal is tied to — e.g. a board card
/// key like `"TASK-277"`. `title` is an optional human label cached at link time.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskRef {
    pub key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Flips true when the work item this ref points at is finished — set by a
    /// finishing linked coordinator (TASK-282). Feeds the goal progress rollup.
    #[serde(default)]
    pub done: bool,
}

impl TaskRef {
    pub fn new(key: impl Into<String>) -> Self {
        Self {
            key: key.into(),
            title: None,
            done: false,
        }
    }

    pub fn with_title(key: impl Into<String>, title: impl Into<String>) -> Self {
        Self {
            key: key.into(),
            title: Some(title.into()),
            done: false,
        }
    }
}

/// Render the `:workers` goal-hierarchy block (TASK-300): the goal and its
/// linked work items as a tree, correlating each linked task key to a live
/// coordinator whose task text references that key. The current architecture
/// does not stamp a `goal_id` on spawned coordinators, so the parent↔child link
/// is derived from the durable `Goal.linked_tasks` records (real data) matched
/// against each worker's task string. Returns `None` when the goal has no linked
/// tasks (nothing to nest). Pure/formatting-only → unit-testable without a live
/// board or spawned workers. `workers` is `(worker_id, worker_task)` pairs.
pub fn render_goal_hierarchy(goal: &Goal, workers: &[(String, String)]) -> Option<String> {
    if goal.linked_tasks.is_empty() {
        return None;
    }
    let title = truncate_ellipsis(&goal.title, 60);
    let n = goal.linked_tasks.len();
    let kids = if n == 1 { "child" } else { "children" };
    let mut out = format!("├─ goal: {title} [{n} {kids}]");
    for (i, t) in goal.linked_tasks.iter().enumerate() {
        let branch = if i + 1 == n { "└─" } else { "├─" };
        // Correlate a live worker to this linked task by task-key substring.
        let line = match workers.iter().find(|(_, task)| task.contains(&t.key)) {
            Some((id, _)) => format!("\n│  {branch} {id} ({})", t.key),
            None => format!("\n│  {branch} {} (no active worker)", t.key),
        };
        out.push_str(&line);
    }
    Some(out)
}

/// A durable, structured goal record. Persisted in `aish.db`'s `goals` table.
///
/// Hierarchy: `parent_id` is `Some` for a subgoal, `None` for a top-level goal.
/// The tree is arbitrary-depth; the store fetches children by `parent_id`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Goal {
    /// Stable unique id (uuid v4).
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub status: GoalStatus,
    #[serde(default)]
    pub milestones: Vec<Milestone>,
    #[serde(default)]
    pub blockers: Vec<Blocker>,
    #[serde(default)]
    pub linked_tasks: Vec<TaskRef>,
    /// Parent goal id when this is a subgoal; `None` at the top level.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<String>,
    /// Unix seconds when created / last mutated. Audit stamps only.
    #[serde(default)]
    pub created_at: i64,
    #[serde(default)]
    pub updated_at: i64,
}

impl Goal {
    /// A fresh top-level goal: new id, `Active`, empty collections, stamped now.
    pub fn new(title: impl Into<String>) -> Self {
        let ts = now_secs();
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            title: title.into(),
            description: String::new(),
            status: GoalStatus::default(),
            milestones: Vec::new(),
            blockers: Vec::new(),
            linked_tasks: Vec::new(),
            parent_id: None,
            created_at: ts,
            updated_at: ts,
        }
    }

    /// A fresh subgoal parented under `parent_id`.
    pub fn subgoal(title: impl Into<String>, parent_id: impl Into<String>) -> Self {
        let mut g = Goal::new(title);
        g.parent_id = Some(parent_id.into());
        g
    }

    /// Builder-style description setter (used in construction chains).
    pub fn with_description(mut self, description: impl Into<String>) -> Self {
        self.description = description.into();
        self
    }

    /// True when this goal hangs under a parent.
    pub fn is_subgoal(&self) -> bool {
        self.parent_id.is_some()
    }

    /// Bump `updated_at`. Called by every mutator so persistence is ordered.
    pub fn touch(&mut self) {
        self.updated_at = now_secs();
    }

    pub fn add_milestone(&mut self, title: impl Into<String>) -> &Milestone {
        self.milestones.push(Milestone::new(title));
        self.touch();
        self.milestones.last().expect("just pushed")
    }

    pub fn add_blocker(&mut self, description: impl Into<String>) -> &Blocker {
        self.blockers.push(Blocker::new(description));
        self.touch();
        self.blockers.last().expect("just pushed")
    }

    pub fn link_task(&mut self, task: TaskRef) {
        // Dedup on key so re-linking the same card is a no-op.
        if !self.linked_tasks.iter().any(|t| t.key == task.key) {
            self.linked_tasks.push(task);
            self.touch();
        }
    }

    pub fn set_status(&mut self, status: GoalStatus) {
        self.status = status;
        self.touch();
    }

    /// `(done, total)` milestone counts — a cheap progress signal for the UI.
    pub fn milestone_progress(&self) -> (usize, usize) {
        let done = self.milestones.iter().filter(|m| m.done).count();
        (done, self.milestones.len())
    }

    /// Unresolved blockers — the ones actually impeding the goal right now.
    pub fn open_blockers(&self) -> usize {
        self.blockers.iter().filter(|b| !b.resolved).count()
    }

    /// `(done, total)` linked-task counts — the coordinator-driven progress
    /// signal (TASK-282). A finishing linked coordinator flips one `done`.
    pub fn linked_task_progress(&self) -> (usize, usize) {
        let done = self.linked_tasks.iter().filter(|t| t.done).count();
        (done, self.linked_tasks.len())
    }

    /// Mark the linked task with `key` finished. Returns true when this actually
    /// flipped a not-yet-done ref (so callers can skip a no-op persist). Bumps
    /// `updated_at` only on a real change.
    pub fn complete_linked_task(&mut self, key: &str) -> bool {
        if let Some(t) = self
            .linked_tasks
            .iter_mut()
            .find(|t| t.key == key && !t.done)
        {
            t.done = true;
            self.touch();
            true
        } else {
            false
        }
    }

    /// A 0..=100 rollup of goal progress across BOTH milestones and linked
    /// tasks (TASK-282). Terminal `Completed` always reads 100. With no
    /// trackable items the percentage is 0 (nothing to roll up yet). The ratio
    /// is rounded half-up without floats.
    pub fn progress_percent(&self) -> u8 {
        if self.status == GoalStatus::Completed {
            return 100;
        }
        let (m_done, m_total) = self.milestone_progress();
        let (t_done, t_total) = self.linked_task_progress();
        let total = m_total + t_total;
        if total == 0 {
            return 0;
        }
        let done = m_done + t_done;
        // Round half-up without floats: (done*100 + total/2) / total.
        (((done * 100) + total / 2) / total) as u8
    }

    /// The next checkpoint to tackle — the first not-yet-`done` milestone in
    /// order. `None` when every milestone is done (or there are none). This is
    /// the primitive goal-aware routing consumes to pick the next unit of work.
    pub fn next_open_milestone(&self) -> Option<&Milestone> {
        self.milestones.iter().find(|m| !m.done)
    }

    /// Goal-aware next-work selection (TASK-280): the next aligned unit of work
    /// to advance this goal, or `None` when the goal isn't actionable
    /// (paused / blocked / terminal / already complete) or has nothing left to
    /// pick up. This is the seam the task-less `:dispatch` path consumes to route
    /// work toward the live goal instead of printing bare usage.
    ///
    /// Precedence: the data model keeps `milestones` and `linked_tasks` as
    /// sibling lists (no explicit task→milestone tag), so a still-open linked
    /// task is the concrete unit of work — framed by the next incomplete
    /// milestone it advances toward (AC2: prefer the next incomplete milestone's
    /// tasks). When every linked task is done, the next incomplete milestone
    /// itself becomes the suggested work.
    pub fn next_aligned_work(&self) -> Option<NextWork<'_>> {
        if !self.is_actionable() {
            return None;
        }
        let milestone = self.next_open_milestone();
        if let Some(task) = self.linked_tasks.iter().find(|t| !t.done) {
            return Some(NextWork::Task { task, milestone });
        }
        milestone.map(NextWork::Milestone)
    }

    /// Whether this goal is ready to be worked *right now*: it's `Active`, has no
    /// open blockers, and isn't already fully complete. Routing skips goals that
    /// are paused, blocked, terminal, or done.
    pub fn is_actionable(&self) -> bool {
        self.status == GoalStatus::Active
            && self.open_blockers() == 0
            && self.progress_percent() < 100
    }

    /// Auto-advance the status when everything tracked is finished: a non-terminal
    /// goal with ≥1 tracked item all at 100% flips to `Completed`. Returns true
    /// when the status changed so the caller can persist. No-op when there is
    /// nothing to roll up (avoids "completing" an empty goal).
    pub fn rollup_status(&mut self) -> bool {
        if self.status.is_terminal() {
            return false;
        }
        let (_, m_total) = self.milestone_progress();
        let (_, t_total) = self.linked_task_progress();
        if m_total + t_total == 0 {
            return false;
        }
        if self.progress_percent() == 100 {
            self.set_status(GoalStatus::Completed);
            true
        } else {
            false
        }
    }

    /// TASK-279: a compact, token-cheap summary of this goal for injection into
    /// the per-turn system prompt. One or two short lines — the (truncated)
    /// title, the current milestone with a `done/total` + percent progress
    /// signal, and any open blockers (capped and truncated so a goal with many
    /// long blockers can't bloat the prompt). Callers guard on an *active* goal,
    /// so this is never rendered for paused or terminal goals.
    pub fn prompt_summary(&self) -> String {
        let mut out = format!("Goal: {}", truncate_ellipsis(&self.title, 72));
        let (done, total) = self.milestone_progress();
        if total > 0 {
            let pct = done * 100 / total;
            match self.milestones.iter().find(|m| !m.done) {
                Some(current) => out.push_str(&format!(
                    " — current milestone: {} ({done}/{total} done, {pct}%)",
                    truncate_ellipsis(&current.title, 60)
                )),
                None => out.push_str(&format!(" — milestones {done}/{total} done ({pct}%)")),
            }
        }
        let open: Vec<&Blocker> = self.blockers.iter().filter(|b| !b.resolved).collect();
        if !open.is_empty() {
            const MAX_SHOWN: usize = 3;
            let shown = open
                .iter()
                .take(MAX_SHOWN)
                .map(|b| truncate_ellipsis(&b.description, 60))
                .collect::<Vec<_>>()
                .join("; ");
            let extra = open.len().saturating_sub(MAX_SHOWN);
            let more = if extra > 0 {
                format!(" (+{extra} more)")
            } else {
                String::new()
            };
            out.push_str(&format!("\nOpen blockers: {shown}{more}"));
        }
        out
    }
}

/// Aggregate `(done, total)` milestone counts across a goal **and its entire
/// descendant subtree**, identified by `root_id` within `all`. This is the
/// cross-tree progress rollup: a parent's real progress folds in its subgoals.
/// Unknown `root_id` yields `(0, 0)`. Pure over the slice — no DB, no ordering
/// assumptions beyond parent_id links.
pub fn subtree_progress(root_id: &str, all: &[Goal]) -> (usize, usize) {
    let mut done = 0;
    let mut total = 0;
    // Iterative DFS over parent_id edges; cycle-safe via a visited set.
    let mut stack = vec![root_id.to_string()];
    let mut seen = std::collections::HashSet::new();
    while let Some(id) = stack.pop() {
        if !seen.insert(id.clone()) {
            continue;
        }
        if let Some(g) = all.iter().find(|g| g.id == id) {
            let (d, t) = g.milestone_progress();
            done += d;
            total += t;
            for child in all.iter().filter(|c| c.parent_id.as_deref() == Some(id.as_str())) {
                stack.push(child.id.clone());
            }
        }
    }
    (done, total)
}

/// Subtree progress as a rounded 0–100 percentage (see [`subtree_progress`]).
/// Empty/unknown subtrees report 0.
pub fn subtree_percent(root_id: &str, all: &[Goal]) -> u8 {
    let (done, total) = subtree_progress(root_id, all);
    if total == 0 {
        return 0;
    }
    (((done * 100) + total / 2) / total) as u8
}

/// A concrete next unit of work selected for an actionable goal (TASK-280) — what
/// goal-aware routing surfaces when `:dispatch` is invoked with no task and there
/// is a live goal to advance. Either a specific linked task to pick up (framed by
/// the milestone in flight) or, when no linked tasks remain open, the next
/// incomplete milestone itself. Borrows the goal so rendering stays allocation-light.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NextWork<'a> {
    /// The next incomplete linked task to pick up, with the current open
    /// milestone (if any) it advances toward.
    Task {
        task: &'a TaskRef,
        milestone: Option<&'a Milestone>,
    },
    /// No linked tasks remain open — tackle the next incomplete milestone.
    Milestone(&'a Milestone),
}

impl NextWork<'_> {
    /// The task text a coordinator would be assigned for this unit of work — the
    /// linked task's key (plus cached title) or the milestone title. This is the
    /// concrete string the operator confirms via `:dispatch <text>`.
    pub fn dispatch_text(&self) -> String {
        match self {
            NextWork::Task { task, .. } => match &task.title {
                Some(t) => format!("{}: {t}", task.key),
                None => task.key.clone(),
            },
            NextWork::Milestone(m) => m.title.clone(),
        }
    }

    /// A one-line human summary of the suggested work, for the operator prompt.
    pub fn summary(&self) -> String {
        match self {
            NextWork::Task { task, milestone } => {
                let label = match &task.title {
                    Some(t) => format!("{} ({t})", task.key),
                    None => task.key.clone(),
                };
                match milestone {
                    Some(m) => format!(
                        "task {label} toward milestone \u{201c}{}\u{201d}",
                        truncate_ellipsis(&m.title, 60)
                    ),
                    None => format!("task {label}"),
                }
            }
            NextWork::Milestone(m) => {
                format!("milestone \u{201c}{}\u{201d}", truncate_ellipsis(&m.title, 60))
            }
        }
    }
}

/// Goal-aware routing selection: pick the next goal to work from a set. Returns
/// the first [`Goal::is_actionable`] goal in input order (deterministic), or
/// `None` when everything is paused/blocked/terminal/done. This is the seam the
/// invoke path builds on to route work toward the user's live goals.
pub fn route_next(goals: &[Goal]) -> Option<&Goal> {
    goals.iter().find(|g| g.is_actionable())
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fmt_duration_buckets() {
        assert_eq!(fmt_duration(Duration::from_secs(0)), "0s");
        assert_eq!(fmt_duration(Duration::from_secs(45)), "45s");
        assert_eq!(fmt_duration(Duration::from_secs(59)), "59s");
        // 4m12s
        assert_eq!(fmt_duration(Duration::from_secs(4 * 60 + 12)), "4m12s");
        // seconds zero-padded within a minute
        assert_eq!(fmt_duration(Duration::from_secs(60 + 5)), "1m05s");
        // 1h03m — past an hour, seconds drop off, minutes zero-padded
        assert_eq!(
            fmt_duration(Duration::from_secs(3600 + 3 * 60 + 9)),
            "1h03m"
        );
        assert_eq!(fmt_duration(Duration::from_secs(3600)), "1h00m");
    }

    #[test]
    fn truncate_condition_ellipsizes() {
        assert_eq!(truncate_condition("short goal"), "short goal");
        let long = "a".repeat(80);
        let out = truncate_condition(&long);
        assert!(out.ends_with('…'));
        assert_eq!(out.chars().count(), 61); // 60 + ellipsis
    }

    /// Build a Goal directly (bypassing spawn's background loop) to assert wording.
    fn goal_with(status: Status, phase: Step, turn_started: bool) -> GoalLoop {
        GoalLoop {
            condition: "Complete the work".to_string(),
            started: Instant::now(),
            hooks: crate::hooks::HookSet::empty(),
            session_id: "test-session".to_string(),
            cwd: std::path::PathBuf::from("/tmp"),
            mode: "build".to_string(),
            inner: Mutex::new(Inner {
                status,
                turns: 3,
                last_reason: Some("not yet".to_string()),
                cancel: false,
                turn_started: turn_started.then(Instant::now),
                phase,
                transcript: Vec::new(),
                steers: Vec::new(),
            }),
        }
    }

    // compose_guidance: no verifier note and no steers → clean (None) so the
    // first turn's directive recovers a bare condition.
    #[test]
    fn compose_guidance_empty_is_none() {
        assert_eq!(compose_guidance(None, &[]), None);
    }

    // Verifier feedback with no steer is carried through verbatim.
    #[test]
    fn compose_guidance_verifier_only_passthrough() {
        assert_eq!(
            compose_guidance(Some("tighten the error handling"), &[]),
            Some("tighten the error handling".to_string())
        );
    }

    // A steer with no verifier note is framed as a priority operator instruction.
    #[test]
    fn compose_guidance_steer_only_is_priority_block() {
        let out = compose_guidance(None, &["prefer sqlite over postgres".to_string()]).unwrap();
        assert!(out.contains("operator steered this goal"), "framed: {out}");
        assert!(out.contains("prefer sqlite over postgres"), "carries msg: {out}");
        assert!(!out.contains("prior verifier note"), "no verifier tail: {out}");
    }

    // Steer + verifier: both carried, steer FIRST (human course-correction
    // outranks the machine judge's last critique), verifier demoted to a tail.
    #[test]
    fn compose_guidance_steer_and_verifier_orders_steer_first() {
        let out = compose_guidance(
            Some("add tests"),
            &["ship it as a draft PR".to_string()],
        )
        .unwrap();
        let steer_at = out.find("ship it as a draft PR").unwrap();
        let verifier_at = out.find("add tests").unwrap();
        assert!(steer_at < verifier_at, "steer precedes verifier: {out}");
        assert!(out.contains("prior verifier note"), "verifier demoted: {out}");
    }

    // Multiple steers are joined FIFO into one directive.
    #[test]
    fn compose_guidance_joins_multiple_steers() {
        let out = compose_guidance(
            None,
            &["first".to_string(), "second".to_string()],
        )
        .unwrap();
        assert!(out.contains("first | second"), "FIFO join: {out}");
    }

    // steer(): queues on an active goal, is drained one-shot by take_steers,
    // and a blank message is rejected.
    #[test]
    fn test_steer_queues_and_drains_once() {
        let g = goal_with(Status::Active, Step::Working, true);
        assert!(g.steer("narrow to the parser"), "active goal accepts steer");
        assert!(!g.steer("   "), "blank steer rejected");
        let drained = g.take_steers();
        assert_eq!(drained, vec!["narrow to the parser".to_string()]);
        assert!(g.take_steers().is_empty(), "one-shot: second drain empty");
    }

    // steer(): a finished (or cancelled) goal has nothing to steer.
    #[test]
    fn test_steer_rejected_when_not_active() {
        let g = goal_with(Status::Achieved, Step::Idle, false);
        assert!(!g.steer("too late"), "inactive goal rejects steer");
        assert!(g.take_steers().is_empty());
    }

    // TASK-301: `:attach goal` history backfill surfaces the goal's durable
    // state (condition + status line) since a GoalLoop keeps no transcript.
    #[test]
    fn test_attach_goal_shows_history() {
        let g = goal_with(Status::Active, Step::Checking, true);
        let lines = g.attach_backfill();
        let joined = lines.join("\n");
        assert!(
            joined.contains("Complete the work"),
            "backfill shows the goal condition: {joined}"
        );
        assert!(
            joined.contains("goal [active"),
            "backfill shows the status line: {joined}"
        );
        assert!(
            joined.contains("turn 3"),
            "backfill shows turn progress: {joined}"
        );
        assert!(
            joined.contains("last check: not yet"),
            "backfill shows the last verifier check: {joined}"
        );
    }

    // A goal is a specialized worker: its per-turn activity is captured into a
    // bounded transcript (via `note`) and replayed by `:attach goal` /
    // Shift-Tab, oldest-first, with the live status line appended last — exactly
    // how a worker's forwarded activity tail replays on worker-entry.
    #[test]
    fn test_attach_goal_replays_captured_transcript_then_status() {
        let g = goal_with(Status::Active, Step::Working, true);
        g.note("started — Complete the work");
        g.note("turn 1: working…");
        g.note("turn 1: checking…");
        let lines = g.attach_backfill();
        // Activity rows come first, in capture order…
        assert_eq!(lines[0], "started — Complete the work");
        assert_eq!(lines[1], "turn 1: working…");
        assert_eq!(lines[2], "turn 1: checking…");
        // …and the live one-line status is appended last.
        assert!(
            lines.last().unwrap().contains("goal [active"),
            "status line appended last: {lines:?}"
        );
    }

    // The transcript is bounded — old rows drop off so a long-running goal's
    // replay stays ~1 screen, mirroring the worker replay budget.
    #[test]
    fn goal_transcript_is_bounded() {
        let g = goal_with(Status::Active, Step::Working, true);
        for n in 0..(TRANSCRIPT_CAP + 25) {
            g.note(&format!("turn {n}: working…"));
        }
        let rows = g.attach_backfill();
        // transcript capped, plus the appended status line.
        assert!(
            rows.len() <= TRANSCRIPT_CAP + 1,
            "transcript stays bounded: {} rows",
            rows.len()
        );
        // Oldest rows evicted — the newest survive.
        let joined = rows.join("\n");
        assert!(
            joined.contains(&format!("turn {}: working…", TRANSCRIPT_CAP + 24)),
            "newest activity retained: {joined}"
        );
        assert!(
            !joined.contains("turn 0: working…"),
            "oldest activity evicted: {joined}"
        );
    }

    #[test]
    fn status_line_active_shows_phase_and_turn() {
        let g = goal_with(Status::Active, Step::Checking, true);
        let line = g.status_line();
        assert!(line.contains("goal [active · checking]"), "got: {line}");
        assert!(line.contains("turn 3"), "got: {line}");
        assert!(line.contains("this turn /"), "got: {line}");
        assert!(line.contains("total"), "got: {line}");
        assert!(line.contains("Complete the work"), "got: {line}");
        assert!(line.contains("last check: not yet"), "got: {line}");
        assert!(!line.contains('\n'), "must be one line: {line}");
    }

    #[test]
    fn status_line_finished_omits_phase_and_perturn() {
        let g = goal_with(Status::Achieved, Step::Idle, false);
        let line = g.status_line();
        assert!(line.contains("goal [achieved]"), "got: {line}");
        assert!(line.contains("3 turn(s)"), "got: {line}");
        assert!(line.contains("total"), "got: {line}");
        assert!(!line.contains("this turn"), "got: {line}");
        assert!(!line.contains("checking"), "got: {line}");
        assert!(!line.contains("working"), "got: {line}");
    }

    /// AC#3 regression: the batch stopping-oracle loop is unchanged. Pin the
    /// hard backstop and the generator→verifier state machine so a refactor of
    /// the new Goal domain model can never silently weaken the unattended loop.
    #[test]
    fn batch_oracle_loop_invariants_unchanged() {
        // Hard MAX_TURNS backstop still guards runaway unattended pursuit.
        assert_eq!(MAX_TURNS, 25, "MAX_TURNS backstop must not drift");

        // The loop's phase machine still has its three generator/verifier steps.
        assert!(Step::Idle == Step::Idle);
        assert!(Step::Working != Step::Checking);

        // An achieved goal is terminal for the loop: is_active() flips false and
        // the recorded reason is preserved for delivery.
        let g = goal_with(Status::Active, Step::Working, true);
        assert!(g.is_active(), "active loop reports active");
        g.set(Status::Achieved, Some("evidence met".into()));
        assert!(!g.is_active(), "achieved loop is no longer active");
        assert!(g.status_line().contains("achieved"), "{}", g.status_line());

        // A failed goal (the MAX_TURNS path) is likewise terminal.
        let f = goal_with(Status::Active, Step::Checking, true);
        f.set(Status::Failed, Some("backstop".into()));
        assert!(!f.is_active(), "failed loop is no longer active");
    }
}

#[cfg(test)]
mod domain_tests {
    use super::*;

    #[test]
    fn new_goal_defaults_are_active_and_empty() {
        let g = Goal::new("Ship TASK-277");
        assert_eq!(g.title, "Ship TASK-277");
        assert_eq!(g.status, GoalStatus::Active);
        assert!(g.milestones.is_empty());
        assert!(g.blockers.is_empty());
        assert!(g.linked_tasks.is_empty());
        assert!(g.parent_id.is_none());
        assert!(!g.is_subgoal());
        assert!(!g.id.is_empty());
        assert!(g.created_at > 0);
        assert_eq!(g.created_at, g.updated_at);
    }

    #[test]
    fn subgoal_carries_parent() {
        let parent = Goal::new("Parent");
        let child = Goal::subgoal("Child", parent.id.clone());
        assert!(child.is_subgoal());
        assert_eq!(child.parent_id.as_deref(), Some(parent.id.as_str()));
    }

    #[test]
    fn mutators_bump_updated_at_and_collections() {
        let mut g = Goal::new("G");
        let created = g.updated_at;
        g.updated_at -= 5; // simulate an older stamp so touch() is observable
        g.add_milestone("m1");
        g.add_blocker("b1");
        g.link_task(TaskRef::with_title("TASK-277", "Persistent goals"));
        assert_eq!(g.milestones.len(), 1);
        assert_eq!(g.blockers.len(), 1);
        assert_eq!(g.linked_tasks.len(), 1);
        assert!(g.updated_at >= created);
    }

    #[test]
    fn link_task_dedups_on_key() {
        let mut g = Goal::new("G");
        g.link_task(TaskRef::new("TASK-277"));
        g.link_task(TaskRef::with_title("TASK-277", "dup"));
        assert_eq!(g.linked_tasks.len(), 1);
    }

    #[test]
    fn progress_and_open_blocker_counts() {
        let mut g = Goal::new("G");
        g.add_milestone("m1");
        g.add_milestone("m2");
        g.milestones[0].done = true;
        g.add_blocker("b1");
        g.add_blocker("b2");
        g.blockers[1].resolved = true;
        assert_eq!(g.milestone_progress(), (1, 2));
        assert_eq!(g.open_blockers(), 1);
    }

    #[test]
    fn status_token_roundtrip() {
        for s in [
            GoalStatus::Active,
            GoalStatus::Paused,
            GoalStatus::Completed,
            GoalStatus::Abandoned,
        ] {
            assert_eq!(GoalStatus::from_token(s.as_str()), s);
        }
        // Unknown / hand-edited tokens degrade to Active, never panic.
        assert_eq!(GoalStatus::from_token("wat"), GoalStatus::Active);
        assert_eq!(GoalStatus::from_token(""), GoalStatus::Active);
        assert_eq!(GoalStatus::from_token(" COMPLETED "), GoalStatus::Completed);
    }

    #[test]
    fn terminal_status_flags() {
        assert!(GoalStatus::Completed.is_terminal());
        assert!(GoalStatus::Abandoned.is_terminal());
        assert!(!GoalStatus::Active.is_terminal());
        assert!(!GoalStatus::Paused.is_terminal());
    }

    #[test]
    fn prompt_summary_shows_current_milestone_progress_and_blockers() {
        // TASK-279: title + current (first not-done) milestone + done/total +
        // percent + open blockers, all in the compact block.
        let mut g = Goal::new("Ship goal-context injection");
        g.add_milestone("design");
        g.add_milestone("build");
        g.add_milestone("open PR");
        g.milestones[0].done = true; // 1 of 3 done → current = "build", 33%
        g.add_blocker("waiting on review");
        let s = g.prompt_summary();
        assert!(s.contains("Goal: Ship goal-context injection"), "{s}");
        assert!(s.contains("current milestone: build"), "{s}");
        assert!(s.contains("1/3 done"), "{s}");
        assert!(s.contains("33%"), "{s}");
        assert!(s.contains("Open blockers: waiting on review"), "{s}");
    }

    #[test]
    fn prompt_summary_omits_empty_sections_and_caps_blockers() {
        // No milestones, no blockers → just the title line, nothing dangling.
        let g = Goal::new("Bare goal");
        assert_eq!(g.prompt_summary(), "Goal: Bare goal");

        // All milestones done → summary reports full completion, no "current".
        let mut done = Goal::new("Done goal");
        done.add_milestone("a");
        done.milestones[0].done = true;
        let ds = done.prompt_summary();
        assert!(ds.contains("milestones 1/1 done (100%)"), "{ds}");
        assert!(!ds.contains("current milestone"), "{ds}");

        // >3 open blockers → first three shown, rest summarized as "(+N more)".
        let mut many = Goal::new("Blocked goal");
        for i in 0..5 {
            many.add_blocker(format!("blocker {i}"));
        }
        let ms = many.prompt_summary();
        assert!(ms.contains("(+2 more)"), "{ms}");
    }

    #[test]
    fn prompt_summary_truncates_long_title() {
        // AC3: long goal text is ellipsized so the block stays one compact line.
        let g = Goal::new("x".repeat(120));
        let s = g.prompt_summary();
        assert!(s.contains('…'), "long title must be ellipsized: {s}");
        assert!(!s.contains(&"x".repeat(120)), "full long title must not appear");
    }

    #[test]
    fn goal_json_roundtrips() {
        let mut g = Goal::new("Roundtrip").with_description("desc");
        g.add_milestone("m1");
        g.add_blocker("b1");
        g.link_task(TaskRef::new("TASK-277"));
        g.set_status(GoalStatus::Paused);
        let json = serde_json::to_string(&g).expect("serialize");
        let back: Goal = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(g, back);
    }

    // ── rollup: progress % (single goal) ────────────────────────────────────
    #[test]
    fn progress_percent_rounds_and_handles_empty() {
        let mut g = Goal::new("P");
        assert_eq!(g.progress_percent(), 0, "no milestones ⇒ 0%");
        g.add_milestone("a");
        g.add_milestone("b");
        g.add_milestone("c");
        assert_eq!(g.progress_percent(), 0);
        g.milestones[0].done = true; // 1/3 = 33.33 → 33
        assert_eq!(g.progress_percent(), 33);
        g.milestones[1].done = true; // 2/3 = 66.67 → 67 (half-up)
        assert_eq!(g.progress_percent(), 67);
        g.milestones[2].done = true; // 3/3 → 100
        assert_eq!(g.progress_percent(), 100);
    }

    #[test]
    fn next_open_milestone_picks_first_undone() {
        let mut g = Goal::new("N");
        assert!(g.next_open_milestone().is_none(), "none when empty");
        g.add_milestone("first");
        g.add_milestone("second");
        g.milestones[0].done = true;
        assert_eq!(g.next_open_milestone().unwrap().title, "second");
        g.milestones[1].done = true;
        assert!(g.next_open_milestone().is_none(), "none when all done");
    }

    // ── TASK-280: goal-aware next-work selection ────────────────────────────

    #[test]
    fn next_aligned_work_none_when_not_actionable() {
        // Paused goal → no routing (task-less dispatch falls back to usage).
        let mut g = Goal::new("G");
        g.add_milestone("m1");
        g.link_task(TaskRef::new("TASK-1"));
        g.set_status(GoalStatus::Paused);
        assert!(g.next_aligned_work().is_none(), "paused goal routes nothing");

        // Blocked goal → no routing even while Active.
        let mut g = Goal::new("G");
        g.add_milestone("m1");
        g.add_blocker("waiting on review");
        assert!(g.next_aligned_work().is_none(), "blocked goal routes nothing");

        // Fully complete goal → no routing.
        let mut g = Goal::new("G");
        g.add_milestone("m1");
        g.milestones[0].done = true;
        assert!(g.next_aligned_work().is_none(), "complete goal routes nothing");

        // Empty active goal (nothing tracked) → nothing to pick up.
        let g = Goal::new("G");
        assert!(g.next_aligned_work().is_none(), "empty goal routes nothing");
    }

    #[test]
    fn next_aligned_work_prefers_incomplete_task_framed_by_milestone() {
        let mut g = Goal::new("Ship it");
        g.add_milestone("Design");
        g.add_milestone("Build");
        g.milestones[0].done = true; // next open milestone == "Build"
        g.link_task(TaskRef::with_title("TASK-9", "wire routing"));
        g.link_task(TaskRef::new("TASK-10"));
        g.linked_tasks[0].done = true; // first OPEN task == TASK-10

        match g.next_aligned_work().expect("actionable goal routes work") {
            NextWork::Task { task, milestone } => {
                assert_eq!(task.key, "TASK-10", "picks first not-done linked task");
                assert_eq!(
                    milestone.map(|m| m.title.as_str()),
                    Some("Build"),
                    "AC2: frames by the next incomplete milestone"
                );
            }
            other => panic!("expected a task suggestion, got {other:?}"),
        }
    }

    #[test]
    fn next_aligned_work_falls_back_to_milestone_when_tasks_done() {
        let mut g = Goal::new("Ship it");
        g.add_milestone("Build");
        g.link_task(TaskRef::new("TASK-1"));
        g.linked_tasks[0].done = true; // all linked tasks done, milestone still open
        match g.next_aligned_work().expect("milestone remains") {
            NextWork::Milestone(m) => assert_eq!(m.title, "Build"),
            other => panic!("expected a milestone suggestion, got {other:?}"),
        }
    }

    #[test]
    fn next_work_dispatch_text_and_summary_render() {
        let m = Milestone::new("Build");
        let titled = TaskRef::with_title("TASK-9", "wire routing");
        let bare = TaskRef::new("TASK-10");

        let w = NextWork::Task {
            task: &titled,
            milestone: Some(&m),
        };
        assert_eq!(w.dispatch_text(), "TASK-9: wire routing");
        assert!(w.summary().contains("TASK-9 (wire routing)"));
        assert!(w.summary().contains("Build"));

        let w = NextWork::Task {
            task: &bare,
            milestone: None,
        };
        assert_eq!(w.dispatch_text(), "TASK-10");
        assert_eq!(w.summary(), "task TASK-10");

        let w = NextWork::Milestone(&m);
        assert_eq!(w.dispatch_text(), "Build");
        assert!(w.summary().contains("Build"));
    }

    // ── rollup: progress % (subtree aggregate) ──────────────────────────────
    #[test]
    fn subtree_progress_folds_in_descendants() {
        // root ─┬─ a ── grand
        //       └─ b
        let mut root = Goal::new("root");
        root.add_milestone("r1"); // 0/1
        let mut a = Goal::subgoal("a", root.id.clone());
        a.add_milestone("a1");
        a.milestones[0].done = true; // 1/1
        let mut grand = Goal::subgoal("grand", a.id.clone());
        grand.add_milestone("g1");
        grand.add_milestone("g2");
        grand.milestones[0].done = true; // 1/2
        let b = Goal::subgoal("b", root.id.clone()); // 0/0

        let all = vec![root.clone(), a, grand, b];
        // done = 0(root)+1(a)+1(grand)+0(b) = 2 ; total = 1+1+2+0 = 4 ⇒ 50%
        assert_eq!(subtree_progress(&root.id, &all), (2, 4));
        assert_eq!(subtree_percent(&root.id, &all), 50);
        // Unknown root is empty, never panics.
        assert_eq!(subtree_progress("ghost", &all), (0, 0));
        assert_eq!(subtree_percent("ghost", &all), 0);
    }

    // ── goal-aware routing selection ────────────────────────────────────────
    #[test]
    fn is_actionable_gates_on_status_blockers_and_completion() {
        let mut g = Goal::new("work");
        g.add_milestone("m");
        assert!(g.is_actionable(), "active + unblocked + incomplete ⇒ actionable");

        g.add_blocker("waiting");
        assert!(!g.is_actionable(), "open blocker ⇒ not actionable");
        g.blockers[0].resolved = true;
        assert!(g.is_actionable(), "cleared blocker ⇒ actionable again");

        g.milestones[0].done = true; // 100%
        assert!(!g.is_actionable(), "fully complete ⇒ not actionable");

        let mut paused = Goal::new("later");
        paused.add_milestone("m");
        paused.set_status(GoalStatus::Paused);
        assert!(!paused.is_actionable(), "paused ⇒ not actionable");
    }

    #[test]
    fn route_next_picks_first_actionable_in_order() {
        // done ── skipped; blocked ── skipped; ready ── chosen.
        let mut done = Goal::new("done");
        done.add_milestone("x");
        done.milestones[0].done = true;

        let mut blocked = Goal::new("blocked");
        blocked.add_milestone("y");
        blocked.add_blocker("dep");

        let mut ready = Goal::new("ready");
        ready.add_milestone("z");

        let goals = vec![done, blocked, ready];
        assert_eq!(route_next(&goals).unwrap().title, "ready");

        // Nothing actionable ⇒ None.
        let mut only_paused = Goal::new("p");
        only_paused.set_status(GoalStatus::Paused);
        assert!(route_next(&[only_paused]).is_none());
        assert!(route_next(&[]).is_none());
    }

    #[test]
    fn complete_linked_task_flips_once() {
        let mut g = Goal::new("G");
        g.link_task(TaskRef::new("TASK-282"));
        g.updated_at -= 5;
        let before = g.updated_at;
        assert!(g.complete_linked_task("TASK-282"), "first flip changes it");
        assert_eq!(g.linked_task_progress(), (1, 1));
        assert!(g.updated_at >= before, "touch bumps updated_at");
        // Re-completing is a no-op (already done) and re-keying a missing task too.
        assert!(!g.complete_linked_task("TASK-282"));
        assert!(!g.complete_linked_task("TASK-999"));
    }

    #[test]
    fn progress_percent_blends_milestones_and_tasks() {
        let mut g = Goal::new("G");
        // No trackable items → 0%.
        assert_eq!(g.progress_percent(), 0);
        g.add_milestone("m1");
        g.add_milestone("m2");
        g.link_task(TaskRef::new("TASK-282"));
        g.link_task(TaskRef::new("TASK-283"));
        // 4 items, none done → 0%.
        assert_eq!(g.progress_percent(), 0);
        g.milestones[0].done = true;
        g.complete_linked_task("TASK-282");
        // 2 of 4 done → 50%.
        assert_eq!(g.progress_percent(), 50);
    }

    #[test]
    fn rollup_status_completes_when_all_done() {
        let mut g = Goal::new("G");
        g.link_task(TaskRef::new("TASK-282"));
        // Not all done → no rollup.
        assert!(!g.rollup_status());
        assert_eq!(g.status, GoalStatus::Active);
        g.complete_linked_task("TASK-282");
        assert!(g.rollup_status(), "all tasks done → Completed");
        assert_eq!(g.status, GoalStatus::Completed);
        assert_eq!(g.progress_percent(), 100);
        // Idempotent: a second rollup on a terminal goal does nothing.
        assert!(!g.rollup_status());
    }

    #[test]
    fn rollup_status_ignores_empty_goal() {
        // A goal with nothing tracked must never auto-complete.
        let mut g = Goal::new("G");
        assert!(!g.rollup_status());
        assert_eq!(g.status, GoalStatus::Active);
        assert_eq!(g.progress_percent(), 0);
    }

    // TASK-300: `:workers` shows the goal's linked work items as a tree,
    // correlating each linked task key to a live coordinator by task text.
    #[test]
    fn test_workers_list_shows_goal_hierarchy() {
        let mut goal = Goal::new("ship hierarchies");
        goal.link_task(TaskRef::new("TASK-280"));
        goal.link_task(TaskRef::new("TASK-281"));
        let workers = vec![
            ("w_abc123".to_string(), "implement TASK-280 board sync".to_string()),
            ("w_def456".to_string(), "TASK-281 render tree".to_string()),
        ];
        let tree = render_goal_hierarchy(&goal, &workers).expect("hierarchy for a linked goal");
        assert!(
            tree.contains("goal: ship hierarchies [2 children]"),
            "got: {tree}"
        );
        assert!(tree.contains("├─ w_abc123 (TASK-280)"), "got: {tree}");
        assert!(tree.contains("└─ w_def456 (TASK-281)"), "got: {tree}");

        // A linked task with no live worker still renders, flagged.
        let mut solo = Goal::new("solo");
        solo.link_task(TaskRef::new("TASK-999"));
        let tree2 = render_goal_hierarchy(&solo, &[]).expect("hierarchy with no workers");
        assert!(tree2.contains("[1 child]"), "got: {tree2}");
        assert!(tree2.contains("TASK-999 (no active worker)"), "got: {tree2}");

        // No linked tasks → nothing to nest.
        assert!(render_goal_hierarchy(&Goal::new("empty"), &[]).is_none());
    }

    // TASK-302: the goal loop mints a fresh `goal-<uuid>` coordinator each turn,
    // so the ONLY stable key tying a goal's turns together is the condition
    // embedded in the directive. `goal_condition_from_directive` must recover it
    // for both directive shapes (first turn + re-tried turn) so `:workers` can
    // collapse the turns into one row.
    #[test]
    fn goal_directive_round_trips_condition() {
        let cond = "make the tests green\nacross the board";
        // First turn (no guidance).
        let first = goal_directive(cond, None);
        assert_eq!(
            goal_condition_from_directive(&first).as_deref(),
            Some(cond),
            "first-turn directive should round-trip the condition"
        );
        // Re-tried turn (verifier guidance appended) recovers the SAME condition,
        // so both turns hash to one goal group regardless of changing guidance.
        let retry = goal_directive(cond, Some("still 3 tests failing"));
        assert_eq!(
            goal_condition_from_directive(&retry).as_deref(),
            Some(cond),
            "re-tried directive should recover the identical condition"
        );
        assert_eq!(
            goal_condition_from_directive(&first),
            goal_condition_from_directive(&retry),
            "every turn of one goal shares a grouping key"
        );
        // A plain (non-goal) coordinator task is not a directive → no key.
        assert!(goal_condition_from_directive("fix the flaky CI run").is_none());
    }

    // A worker turn flagged for operator by the serial-chain guard is recoverable:
    // the goal loop must be able to analyze it and re-plan toward batching. The
    // guidance it feeds the next turn carries the raw error AND the explicit
    // batch-independent-calls instruction.
    #[test]
    fn recovery_guidance_carries_error_and_batching_directive() {
        let err = "goal worker exited unsuccessfully (exit status: 1): flagged for \
                   operator after 2 auto-recovery attempt(s): yielded after 9 \
                   consecutive single-call rounds (a deep serial chain) to re-plan \
                   toward batching independent calls";
        let g = recovery_guidance(err);
        // Carries the raw error so the model can review/analyze the real cause.
        assert!(g.contains("single-call rounds"), "guidance must quote the error");
        // Recognizes the batching flag and gives the concrete re-plan.
        assert!(
            g.to_lowercase().contains("independent tool call"),
            "serial-chain flag must yield an explicit batch-independent-calls instruction"
        );
        // Framed as an interruption to recover from, not a fresh goal.
        assert!(
            g.to_lowercase().contains("not because the goal is impossible")
                || g.to_lowercase().contains("do not restart"),
            "guidance must tell the agent to resume, not restart"
        );
    }

    // A non-batching failure (e.g. an OOM kill) still yields review/analyze
    // guidance, but WITHOUT the batching-specific paragraph.
    #[test]
    fn recovery_guidance_omits_batching_para_for_unrelated_errors() {
        let g = recovery_guidance(
            "goal worker was killed by the OS (signal 9) — most likely out of memory",
        );
        assert!(g.to_lowercase().contains("review"), "still asks the agent to review");
        assert!(
            !g.to_lowercase().contains("independent tool call"),
            "unrelated failures must not get the batching instruction"
        );
    }

    // The recovery cap must stay a small, sane bound so a persistently-failing
    // worker still terminates the goal.
    #[test]
    fn goal_recovery_cap_is_bounded() {
        assert!(
            (1..=5).contains(&MAX_GOAL_RECOVERIES),
            "MAX_GOAL_RECOVERIES must be a small positive bound"
        );
    }

    // The per-turn directive must tell the (nested) worker to surface progress
    // via `message_console`: a turn summary AND any PR it opened (with summary).
    #[test]
    fn goal_directive_instructs_message_console_turn_summary_and_pr() {
        let d = goal_directive("ship the fix", None);
        assert!(
            d.contains("message_console"),
            "directive should instruct the worker to use message_console"
        );
        assert!(
            d.to_lowercase().contains("summary of what you did this turn"),
            "directive should ask for a per-turn summary"
        );
        assert!(
            d.to_lowercase().contains("pull request"),
            "directive should ask for any opened PR with a summary"
        );
        // Instructions live in the prefix, so the condition still round-trips clean.
        assert_eq!(
            goal_condition_from_directive(&d).as_deref(),
            Some("ship the fix")
        );
    }

    // The per-turn directive must tell the worker to right-size sub-tasks so it
    // does not bite off more than it can chew: decompose a mega-task into a
    // review→design→build pipeline, cap each unit at one focused worker pass, and
    // group work by file locality (TODOs touching one file → one worker). This is
    // the operator-requested anti-"mega-task" discipline (bite-size chunking +
    // file-change optimization).
    #[test]
    fn goal_directive_instructs_bite_size_and_file_locality() {
        let d = goal_directive("ship the fix", None).to_lowercase();
        for needle in [
            "bite-size",
            "right-size",
            "pipeline of stages",
            "focused pass",
            "file locality",
            "same file",
            "one worker",
        ] {
            assert!(
                d.contains(needle),
                "bite-size/file-locality directive missing instruction: {needle:?}"
            );
        }
        // Instructions live in the prefix → the condition still round-trips clean,
        // so `:workers` goal-turn coalescing is unaffected.
        assert_eq!(
            goal_condition_from_directive(&goal_directive("ship the fix", None)).as_deref(),
            Some("ship the fix")
        );
    }


    // The per-turn directive must also encode the plan-first + task-lifecycle
    // discipline: deconstruct/restate, constraints, measurable success, dependency
    // + parallel mapping, durable plan persistence, board-task lifecycle, and
    // independent verification — all inside the prefix so the condition round-trips.
    #[test]
    fn goal_directive_instructs_plan_first_and_task_lifecycle() {
        let d = goal_directive("ship the fix", None).to_lowercase();
        for needle in [
            "plan before you build",
            "restate it in your own words",
            "constraints and limits",
            "success looks like",
            "map the dependencies",
            "in parallel",
            "persist that plan",
            "run_in_background",
            "board task",
            "to completed",
            "independent",
        ] {
            assert!(
                d.contains(needle),
                "plan-first directive missing instruction: {needle:?}"
            );
        }
        // Instructions still live in the prefix → condition round-trips clean.
        assert_eq!(
            goal_condition_from_directive(&goal_directive("ship the fix", None)).as_deref(),
            Some("ship the fix")
        );
    }
}
