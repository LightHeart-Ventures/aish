//! Durable, resumable background coordinator — the DEFAULT background path.
//!
//! Ported from atum_cli's batch-coordinator (`batch-coordinator.ts` /
//! `batch-controller-{plan,round}.ts` / `batch-controller-store.ts`). Where
//! `batch.rs` offloads ONE tool-less request to the Anthropic Batches API, the
//! coordinator is a multi-round agentic loop that:
//!   * runs full-tool turns locally (filesystem, run_program, MCP), AND
//!   * fans heavy, latency-insensitive sub-work out to the Batches API,
//!     persisting its phase to SQLite so a crash/exit resumes instead of re-running.
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
//!
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
use crate::control::{ControlChannel, ControlSignal};
use crate::db::CoordinatorStore;
use crate::session::Session;
use crate::tools;

/// Reassess directive folded into the next round when an operator interrupt
/// (Ctrl-C) is observed. Shared by the two interrupt seams so they stay
/// identical: the engine mid-turn abort path (which returns an `Interrupted`
/// banner handled after the turn) and the unified control channel's boundary
/// drain (a between-turns interrupt latched before the turn ran — TASK-297).
const OPERATOR_REASSESS: &str = "[operator interrupt] The operator pressed Ctrl-C to interrupt your \
previous turn mid-flight. Stop what you were doing — do NOT blindly resume it. Re-read the task \
and any newer operator messages, reassess your approach, and either continue with the most \
sensible next step or, if you should wait for direction, give a brief status plus your best \
partial result.";

/// PHASE-0 existence guard folded into the coordinator's first-turn prompt
/// (TASK-355). A headless coordinator handed a "build feature X" task sometimes
/// re-implements something already shipped — a silent, expensive failure mode
/// (the motivating w_nMYxaem3 run issued 87 tool calls before rate-limiting
/// because it never checked whether the activity-tray feature already existed).
/// This directive makes the model run a cheap existence check FIRST: read the
/// repo's `.repospec.json` `features` array, grep the feature's key symbol(s)
/// across `src/`+`tests/`, and scan `git`/`gh` + `background_status` for an
/// existing branch, PR, or peer coordinator on the same task — and STOP with
/// evidence instead of rebuilding when the work is already done. It turns an
/// 87-call runaway into 2–3 calls. Held as a `const` so it is unit-testable
/// without driving a whole run.
const PHASE0_GUARD: &str = "PHASE-0 GUARD — verify the work isn't already done BEFORE you build \
it: for any feature/fix/refactor task, run a cheap existence check before writing a single line of \
code — re-implementing something already shipped is a silent, expensive failure mode. (1) If the \
repo has a `.repospec.json`, read its `features`/`modules` arrays to learn the intended symbol and \
file names; (2) grep the feature's key symbol(s) across `src/` and `tests/`, and check `git log` / \
`git branch` / `gh pr list` for a matching branch or merged PR; (3) also call `background_status` \
and `git worktree list` to see whether a PEER coordinator or existing branch/worktree is ALREADY \
doing THIS task. IF the feature already exists — STOP and report \"Feature already shipped via \
<PR/commit/branch>\" with the evidence, rather than rebuilding it. IF a peer is already on it — \
defer or `tell`-coordinate instead of duplicating. ONLY when the existence check comes back empty \
do you proceed to build. (4) Before your FIRST edit, if the codebase-memory MCP server is \
connected, call `detect_changes` to map your working-tree changes → affected symbols + a risk \
classification, and fold that affected-symbol/risk summary into this guard's reasoning AND the \
draft-PR body; when the server is absent, skip this step silently and continue as today. This is \
cheap insurance on every build task — a 2–3 call guard that \
prevents an 87-call runaway.";

/// The 5-PHASE PIPELINE directive (TASK-356). Complements `PHASE0_GUARD` by
/// restructuring the *rest* of the run into discrete, parallel phases so a
/// coordinator stops thrashing one tool-call per turn (the w_nMYxaem3 run
/// emitted 87 serial calls). The rule is: batch ALL context reads in Phase 1
/// BEFORE any write, do Phase 2 planning with ZERO tool calls (pure reasoning),
/// then fire ALL Phase 3 writes/commits in one parallel batch and TRUST them
/// (no read-back verification), collapsing an 87-call serial chain to ~15–20
/// calls. Phase transition markers ("--- PHASE n ---") let turn logging count
/// calls per phase. See `docs/reference/coordinator/patterns.md` (TASK-359) for the full
/// pipeline table and batching rules. Held as a `const` so it is unit-testable
/// without driving a whole run.
const PHASE_PIPELINE: &str = "5-PHASE PIPELINE — after the Phase-0 guard clears, run the work in \
these five ordered phases and emit a one-line phase marker (e.g. \"--- PHASE 1: DISCOVERY ---\") at \
the start of each so turn logging can attribute calls. The goal is to collapse a serial \
one-call-per-turn chain (the failure mode that produced an 87-call runaway) down to ~15–20 total \
calls.\n\
--- PHASE 1: DISCOVERY --- (3–5 calls, ALL PARALLEL) Front-load EVERY context read you know you'll \
need in ONE batch — glob_expand, list_dir, git status, grep_files, and ranged read_file of every \
already-known path fired together in a single turn. Do not read one file, think, then read the \
next; independent reads have no dependency and MUST batch. The only serial exception is \
grep-then-read of the SAME file (you need the line number first). Close Phase 1 by stating the \
DELTA (`ask - state`): the specific things the ask REQUIRES that the current state does not yet \
provide, plus what is explicitly OUT OF SCOPE. This is the artifact Phase 2 plans against — the \
Phase-0 guard only answered \"already shipped?\", this answers \"what exactly is left?\".\n\
--- PHASE 2: PLANNING --- (0 calls, REASONING ONLY) From the Phase 1 delta, emit a PLAN GRAPH — not a \
bare file list: for each unit of work a node `{ id (lowercase-kebab slug), intent, files[], \
depends_on[], acceptance[] }`. `depends_on` names the node ids that must land first; two nodes \
touching the same file are NOT independent and must be merged into one node or chained. You are \
FORBIDDEN from making ANY tool call in this phase — no reads, no greps, no status checks. If you \
need another read, it belonged in Phase 1 — fold it into the Phase 3 batch, do not spend a planning \
turn on it.\n\
--- PHASE 3: ACTIONS --- (5–10 calls, ALL PARALLEL) Fire every independent write in ONE batch — \
write_file / edit_file to different files, plus independent commands — together. TRUST your writes: \
do NOT read a file back to confirm a write landed; the tool result already tells you it succeeded. \
Serialize ONLY genuine state transitions (git checkout -b → commit → push → gh pr create) and any \
write whose content depends on a value you have not yet read.\n\
--- PHASE 4: VALIDATION --- (1–2 calls, SERIAL) Run the canonical gate once (for aish: \
`cargo test --no-default-features --locked`) and confirm green, then open/finish the PR. One check, \
not a re-inspection of every file you just wrote.";

/// TASK-807: fan-out is DERIVED from the Phase-2 plan graph's ready-set, not a
/// discretionary judgement call. This replaces the old
/// "RE-EVALUATE THE PLAN AFTER TRIAGE — don't over-decompose" directive, which
/// suppressed the SYMPTOM (an 87-call runaway from unstructured fan-out) rather
/// than the CAUSE (Phase 2 emitted a bare file list, so fan-out had nothing to
/// key off and had to guess). The dependency graph replaces that suppression
/// with a structural guard: dispatch iff >=2 nodes are ready AND their `files`
/// sets are pairwise disjoint. This is the prompt twin of
/// `PlanGraph::fan_out_candidates` (TASK-802) — both state the SAME condition.
/// Held as a `const` so the rule is unit-testable without driving a whole run.
const FAN_OUT_DERIVED: &str = "FAN-OUT IS DERIVED, NOT DISCRETIONARY: Before you fan work out with \
`run_in_background`, compute `ready` = the Phase-2 plan-graph nodes whose `depends_on` are ALL \
done. If `ready.len() >= 2` AND those nodes' `files` sets are pairwise disjoint, dispatch one \
worker per ready node. Otherwise execute solo in this turn. NEVER dispatch a node with an unmet \
dependency, and never two nodes touching the same file. (The old \"don't over-decompose\" directive \
existed because unstructured fan-out once produced an 87-call runaway; the dependency graph \
replaces that discretionary suppression with a structural guard.) If triage collapses the \
remaining work to a single root cause the ready-set collapses to one node and you run solo by \
construction — and if a now-redundant fan-out is ALREADY in flight, use `tell` to narrow or cancel \
the pointless peers rather than letting redundant work run.";

/// TASK-406 + TASK-410: advertise the codebase-memory MCP code-intelligence
/// tools to the coordinator as FIRST-CLASS discovery, so a single structural
/// graph query is preferred over a grep→read loop. The token-efficiency
/// headline (~3.4k tokens for five structural queries vs ~412k for the
/// equivalent grep/read scan) is stated inline so the model has a concrete
/// reason to reach for the graph first. This is purely a prompt-surface
/// advertisement — the tools arrive free via MCP once the `:codebase` server is
/// enrolled; when the server is ABSENT the model simply won't have them and the
/// guidance is a no-op (graceful degradation, TASK-411). It also delineates
/// `manage_adr` (code-architecture decisions, graph-tied) from aish's durable
/// `remember()`/memory.rs facts so the two stores aren't conflated (TASK-410).
/// Held as a `const` so the advertisement is unit-testable without a live MCP
/// connection.
const CODE_INTEL_DISCOVERY: &str = "CODE-INTELLIGENCE DISCOVERY — prefer a structural graph query \
over a grep→read loop. When the codebase-memory MCP server is connected, these tools answer \
architecture questions in ONE call instead of dozens of file reads (~3.4k tokens for five \
structural queries vs ~412k for the equivalent grep/read scan) — reach for them FIRST during \
Phase-1 discovery, and fall back to grep/read only when they are unavailable:\n\
- `get_architecture` — high-level module/layer map of the repo; start here instead of listing and \
reading directories one by one.\n\
- `search_graph` — locate a symbol with its definition and usages structurally (replaces a grep \
sweep plus several read_files).\n\
- `trace_path` — trace the call/dependency path between two symbols (who calls what) without \
manually walking files.\n\
- `semantic_query` — ask a natural-language question over the indexed graph when you don't yet \
know the symbol name.\n\
- dead-code detection — find unreferenced/unreachable symbols before you add, move, or delete \
code.\n\
- `detect_changes` — map uncommitted changes → affected symbols + risk (used by the Phase-0 guard \
and folded into the draft-PR body).\n\
- `manage_adr` — record and recall Architectural Decision Records so design decisions persist \
structurally across sessions. This is DISTINCT from aish's durable-facts memory (memory.rs / the \
`remember()` tool): memory.rs holds free-form project facts and preferences, while `manage_adr` \
holds code-architecture decisions tied to the graph. Use ADRs for \"why this design\", `remember()` \
for \"this project fact\" — do not duplicate one store in the other.\n\
GRACEFUL ABSENCE: if the codebase-memory server is not connected these tools are simply \
unavailable — fall back to grep/read as today and NEVER fail a run over their absence.";
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

/// Is this `failed` row safe to prune? A SALVAGE row is stored in the `failed`
/// phase but is NOT a failure: it is the only surviving pointer to real work
/// preserved on a branch in an orphaned worktree. Age-based retention would
/// silently delete that pointer (ISS-409757 impact #1), so salvage rows are
/// excluded from the sweep and are retired only when their worktree is actually
/// cleaned up. Typed discriminator — no `LIKE '%salvaged%'` on free text.
fn is_prunable_failed(phase: &str, kind: Option<&str>) -> bool {
    Phase::parse(phase) == Phase::Failed && kind != Some(crate::coordinator_store::SALVAGE_KIND)
}

/// Testable core of [`reap_failed_runs`]: load the store's `failed` rows, decide
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
        .filter(|r| is_prunable_failed(&r.phase, r.kind.as_deref()))
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
    if let Ok(v) = std::env::var("AISH_STARTUP_DIGEST")
        && let Some(b) = parse_flag(&v)
    {
        return b;
    }
    if let Some(db) = session.db.as_ref()
        && let Ok(Some(v)) = db.get_setting("startup_digest")
    {
        return parse_flag(&v).unwrap_or(false);
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

/// How many CONSECUTIVE failed heartbeat writes before we say something. One
/// lost beat is harmless — the next is 30s away and `STALL_AFTER` is ~10 beats
/// wide — but a RUN of them means the store is genuinely unwritable and this run
/// is on a path to being reaped as stalled while perfectly healthy.
const HEARTBEAT_FAILURE_LOG_AFTER: u32 = 2;

/// Write one durable beat, tracking CONSECUTIVE failures so store contention is
/// observable instead of silently swallowed (ISS-407772).
///
/// The old keeper wrote `let _ = store_hb.heartbeat(&run_id)`. With N workers
/// fanned out onto one SQLite file, a beat that loses the write lock past
/// `busy_timeout` was dropped with ZERO signal — making writer contention
/// indistinguishable from a wedged coordinator, and leaving the operator with a
/// run stamped "stalled: no heartbeat activity" and no way to tell which it was.
/// Failures stay non-fatal (a beat is best-effort; never stall the keeper), but
/// they are no longer invisible.
fn beat_once(store: &CoordinatorStore, run_id: &str, consecutive_failures: &mut u32) {
    match store.heartbeat(run_id) {
        Ok(()) => {
            if *consecutive_failures >= HEARTBEAT_FAILURE_LOG_AFTER {
                eprintln!(
                    "\x1b[2maish: heartbeat for {} recovered after {} consecutive failed writes\x1b[0m",
                    crate::batch::short_id(run_id),
                    *consecutive_failures
                );
            }
            *consecutive_failures = 0;
        }
        Err(e) => {
            *consecutive_failures = consecutive_failures.saturating_add(1);
            if *consecutive_failures >= HEARTBEAT_FAILURE_LOG_AFTER {
                eprintln!(
                    "\x1b[33maish: heartbeat write for {} failed {}x in a row ({e}) — coordinator store contention; \
                     this run may be reaped as stalled even though it is alive\x1b[0m",
                    crate::batch::short_id(run_id),
                    *consecutive_failures
                );
            }
        }
    }
}

/// Cancellation token for the heartbeat keeper thread spawned by [`drive`].
/// Held on `drive`'s stack frame, so EVERY return path — normal completion,
/// failure, checkpoint, circuit-break, or an early `?` — stops the beat without
/// a bespoke teardown call at each exit.
struct HeartbeatGuard(std::sync::Arc<std::sync::atomic::AtomicBool>);

impl Drop for HeartbeatGuard {
    fn drop(&mut self) {
        self.0.store(true, std::sync::atomic::Ordering::Relaxed);
    }
}

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

/// Drain any operator messages queued for EVERY id in `ids` and, when present,
/// fold them into `next_input` as an interjection (see `format_interjection`).
/// Returns the count folded so callers can emit a notice. A no-op (returns 0)
/// when there's no store or nothing queued. Best-effort: a store error is
/// swallowed (the run must not die because the mailbox read hiccuped).
///
/// `ids` is a LIST because a resumed run has two addresses: its fresh `run_id`
/// and the stable `w_…` job id the operator actually sees and `:tell`s. Draining
/// both is what keeps a resume-time interjection from being stranded.
fn fold_operator_messages(
    store: Option<&CoordinatorStore>,
    ids: &[&str],
    next_input: &mut String,
) -> usize {
    let Some(s) = store else {
        return 0;
    };
    let mut msgs = Vec::new();
    for id in ids {
        msgs.extend(s.drain_messages(id).unwrap_or_default());
    }
    if msgs.is_empty() {
        return 0;
    }
    let interjection = format_interjection(&msgs);
    // Prepend so the operator's steer is the first thing the model reads this
    // round, ahead of the fold-results / task continuation text.
    *next_input = format!("{interjection}\n\n{next_input}");
    msgs.len()
}

/// Parse an operator `:tell` message as a live-stream (`:output`) directive.
///
/// Defect 1, mid-flight half: a headless coordinator has no REPL, so `:output on`
/// can't be typed into it — but `tell` already reaches it durably at every round
/// boundary. When a steer's ENTIRE body is an output directive we route it to the
/// [`ControlSignal::OutputMode`] signal instead of the model's context, so an
/// operator can open (or close) a stacked coordinator's stream WITHOUT restarting
/// the run. Accepts `:output on`, `output off`, `worker-output on`, `:worker
/// output 1`, … (leading `:` optional, case- and space-insensitive).
///
/// A bare `:output` with no argument is deliberately NOT a directive — for a
/// coordinator "toggle" is ambiguous, and returning `None` just means the message
/// is delivered to the model as an ordinary steer. Pure → unit-testable.
pub(crate) fn parse_output_directive(msg: &str) -> Option<crate::worker::WorkerOutputMode> {
    let lowered = msg
        .trim()
        .trim_start_matches(':')
        .trim()
        .to_ascii_lowercase();
    let rest = lowered
        .strip_prefix("worker-output")
        .or_else(|| lowered.strip_prefix("worker output"))
        .or_else(|| lowered.strip_prefix("worker_output"))
        .or_else(|| lowered.strip_prefix("output"))?
        .trim();
    match rest {
        "on" | "1" | "true" | "yes" => Some(crate::worker::WorkerOutputMode::On),
        "off" | "0" | "false" | "no" => Some(crate::worker::WorkerOutputMode::Off),
        _ => None,
    }
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
    steer_id: Option<&str>,
    store: Option<&CoordinatorStore>,
) -> Outcome {
    // Every mailbox this run answers to. A FIRST launch has exactly one address
    // (`run_id`). A RESUMED launch runs under a fresh `run_id` while the
    // operator keeps addressing it by the stable `w_…` job id passed as
    // `steer_id` (`--steer-id`) — the only id `:tell`, `send_to_attached`, and
    // `background_status` ever surface. Draining BOTH at the round boundary is
    // what fixes interjections that used to queue against the visible id and
    // never reach the model.
    let mailbox_ids: Vec<&str> = match steer_id {
        Some(s) if !s.is_empty() && s != run_id => vec![run_id, s],
        _ => vec![run_id],
    };
    // Pin the verbatim task into the system prompt so it survives every history
    // compaction for the whole run (see `Session::task_anchor`). The first turn's
    // `next_input` below also carries the task, but that message is conversational
    // history — the earliest thing `crate::context` offloads when the window fills
    // — after which only a "[Context compacted: …]" banner remains. The anchored
    // copy lives in the never-compacted system prompt, so the worker keeps its
    // assignment in front of it no matter how long it runs.
    session.task_anchor = Some(input.clone());

    // ── Defect 1: seed the `:output` live-stream gate from the INHERITED env.
    //
    // A headless coordinator has no REPL, so `:output on` can never be typed
    // into it — its `show_worker_output` Arc started hard-`false` and stayed
    // there for the whole run. It still faithfully READ its sub-coordinator's
    // stderr in `stream_stderr` and then dropped every line at the gate, so an
    // operator with `:output on` at the TOP of the chain saw the child's rows but
    // never the grandchild's. The parent now stamps `AISH_WORKER_OUTPUT` on spawn
    // (`worker::worker_command` / the container `env_inline`); `Session::new`
    // seeds from it, and we re-assert it HERE — where the run actually begins —
    // so a Session built on another path (tests, `--resume`, an embedder) also
    // honours the inherited choice. Only an explicit ON is applied: absent/`0`
    // leaves the historical quiet default alone.
    let inherited_output = crate::worker::WorkerOutputMode::from_env();
    if inherited_output.is_on() && !session.worker_output_mode().is_on() {
        session.set_worker_output_mode(inherited_output);
        eprintln!(
            "\x1b[2maish: worker-output inherited ON from parent ({}=1)\x1b[0m",
            crate::worker::WORKER_OUTPUT_ENV
        );
    }

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

    // Stamp our own run id into the process env so any sub-coordinator we spawn
    // (via `run_in_background` → `worker_command` / container `env_inline`) can
    // read it as `AISH_PARENT_RUN_ID` and record us as its parent — this is what
    // lets `:workers` render the run tree. Root (REPL-launched) runs leave the
    // var set for their own children; they themselves have no parent.
    // SAFETY: single-threaded startup, before any worker is spawned.
    unsafe {
        std::env::set_var("AISH_RUN_ID", run_id);
    }
    // Our parent, if we were spawned by another coordinator (unset ⇒ root run).
    let parent_run_id = std::env::var("AISH_PARENT_RUN_ID")
        .ok()
        .filter(|s| !s.is_empty());

    if let Some(s) = store {
        // session.session_id/name were adopted from the LAUNCHING session at
        // startup (see main.rs), so the row attributes to who asked for the work.
        let _ = s.insert_with_parent(
            run_id,
            &input,
            &session.session_id,
            session.name.as_deref(),
            parent_run_id.as_deref(),
        );
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

    // ── Background heartbeat keeper: beat the durable heartbeat every
    // HEARTBEAT_INTERVAL throughout the FULL lifetime of drive(), not only
    // during batch waits.  A round's tool-call phase can block for tens of
    // minutes (cargo build, long network fetches, etc.) without any round
    // boundary firing — leaving the heartbeat stale and the row visually
    // flagged ⚠ in `:workers` / `background_status`, or even reaped as
    // orphaned by a concurrent startup.
    //
    // Design: a DEDICATED OS THREAD, not a `tokio::spawn`ed task.  This is the
    // load-bearing detail.  A tokio task only beats when the runtime gets to
    // poll it, so ANY blocking work on the runtime — a synchronous tool call, a
    // long `run_program`, a CPU-bound stretch — starves the keeper and the
    // heartbeat goes stale while the coordinator is perfectly healthy.  That is
    // exactly how live runs got stamped `failed` with
    // "stalled: no heartbeat activity for 5+ minutes".  An OS thread is
    // scheduled by the kernel and beats regardless of what the runtime is doing.
    //
    // `_heartbeat_cancel` is a drop guard held on THIS stack frame: dropping it
    // on any return path (completion, failure, checkpoint, circuit-break) flips
    // the flag and the thread exits at its next 250ms tick.  Best-effort — a
    // store write error never stalls the thread.
    let _heartbeat_cancel = store.map(|s| {
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stop_thread = std::sync::Arc::clone(&stop);
        let store_hb = s.clone();
        let run_id_hb = run_id.to_string();
        std::thread::spawn(move || {
            // Short tick so cancellation is prompt; the durable WRITE still only
            // happens once per HEARTBEAT_INTERVAL.
            const TICK: Duration = Duration::from_millis(250);
            let mut failures = 0u32;
            // Beat IMMEDIATELY, before the first round runs. Until the keeper's
            // first write lands, the row still carries its INSERT-time stamp — so
            // a slow first round looked exactly like a worker that never started
            // (ISS-407772), and `stall_kind` needs this first beat to tell the
            // two apart.
            beat_once(&store_hb, &run_id_hb, &mut failures);
            let mut last_beat = std::time::Instant::now();
            while !stop_thread.load(std::sync::atomic::Ordering::Relaxed) {
                std::thread::sleep(TICK);
                // CLOCK-BASED, never a running SUM of nominal sleeps — this was
                // the ISS-407772 root cause. `thread::sleep` guarantees only a
                // LOWER bound: on a box saturated by a fan-out wave (N headless
                // aish children plus their tool subprocesses) every 250ms tick
                // returns late, so `waited += TICK` counted INTENDED time and the
                // effective beat interval drifted without bound — 120 nominal
                // ticks could span many minutes of wall clock. Worse, every
                // co-spawned worker shares the same tick phase and the same load
                // curve, so they drifted in LOCKSTEP and crossed STALL_AFTER
                // together: that is how a whole wave got reaped inside the same
                // second. Reading the monotonic clock makes the interval real for
                // every beat and self-correcting after any stall.
                if last_beat.elapsed() >= HEARTBEAT_INTERVAL {
                    last_beat = std::time::Instant::now();
                    beat_once(&store_hb, &run_id_hb, &mut failures);
                }
            }
        });
        HeartbeatGuard(stop)
    });

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
    // SerialChainYield recoveries are tracked SEPARATELY from the general
    // auto_recoveries counter so that coordinators doing inherently sequential
    // work (git workflows, step-by-step code investigation) are not killed by
    // the same cap that catches true runaway loops. Each serial-chain yield
    // increments THIS counter instead of auto_recoveries; it has its own
    // higher cap (MAX_SERIAL_CHAIN_RECOVERIES). See loopguard.rs for rationale.
    let mut serial_chain_recoveries = 0usize;
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
    //
    // The PHASE-0 GUARD block (see `PHASE0_GUARD`) is prepended just before the
    // TASK so it is the last directive the model reads before starting: run a
    // cheap existence check (.repospec.json → grep → git/gh/background_status)
    // and STOP if the feature already shipped or a peer is already building it,
    // rather than re-implementing shipped work (TASK-355).
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
release view, git show, a status query) and state only what that evidence shows — a fresh read is cheaper than a wrong \
assertion, so verify unprompted; if you could not verify \
something, say so explicitly rather than inventing a result.\n\nDECISION POINTS — avoid loops: If you notice you are repeating the same action or re-deriving a \
fact you already have, STOP and change approach. After about 3 failed attempts at the SAME \
sub-problem, do NOT keep retrying the same way — either try a materially different approach or \
stop and report explicitly: say \"I'm blocked because <specific reason>\", list what you tried \
and what you observed, and give your best partial result. A clearly-stated blocker is a \
successful outcome; an endless retry loop is a failure.\n\n{FAN_OUT_DERIVED}\n\nCOORDINATING WITH OTHER AGENTS — the `:tell` channel: an [Operator interjection] you receive mid-run arrived through this channel — the human (or another agent) steering you; treat it as updated instructions. You can steer ANOTHER in-flight coordinator the same way: call the `tell` tool with its run id (find ids with background_status) and a message, and it is folded into that coordinator's next round. Use it to hand off a finding, correct a peer's course, or narrow its scope.\n\nWRAPPING UP — open a draft PR for \
PR-worthy work: When you finish, if you created or changed files that are meant to land (a fix, \
feature, refactor, or docs) — as opposed to a read-only investigation, question, or analysis that \
produced no committable changes — do NOT leave the work uncommitted or stranded on a local branch. \
You have the SAME git + gh auth as the interactive session, so finish the job: stage and commit on a \
feature branch (you are typically already on a dedicated work branch — commit THERE; never commit to \
or push the default branch), push it, and open a DRAFT pull request with `gh pr create --draft --fill` \
(pass `--title`/`--body` when `--fill` cannot infer them). Put the PR URL in your final answer. If there \
are no committable changes, or `gh`/the remote is unavailable, skip the PR and report the branch name \
plus `git status` instead — do not fail the run over it.\n\n{PHASE0_GUARD}\n\n{PHASE_PIPELINE}\n\n{CODE_INTEL_DISCOVERY}\n\nTASK:\n{input}",
        cwd = session.cwd.display(),
    );

    // ── Unified operator-control channel (TASK-297). The three operator-control
    // paths — the `:tell` steer mailbox, the Ctrl-C interrupt latch, and the
    // live-stream (`:worker-output`) toggle — are normalized into ONE prioritized
    // queue and drained once per round boundary (see `crate::control`). `control`
    // is owned here (the sole consumer); `last_output_mode` tracks the toggle so
    // only a CHANGE is enqueued.
    let mut control = ControlChannel::new();
    let mut last_output_mode = session.worker_output_mode();

    // ── Status-line activity summary (see `crate::activity_summary`). `:workers`
    // and the escalation banner previously had nothing to show but a hard-clipped
    // prefix of the raw task brief, which for a long brief degenerates into "for
    // aish: on worker start, and at the end of…" — syntax, not status. Instead a
    // LIGHTWEIGHT model (haiku) writes one status-line-sized sentence saying what
    // this worker is doing, budgeted to the live terminal width.
    //
    // Generated ONCE here from the task alone so the row is meaningful from the
    // moment the worker appears — before its first round lands — then refreshed
    // from each round's synthesis at the round boundary below.
    //
    // The backend is built once and reused: credential resolution + client setup
    // per round is pure overhead. `None` (no Claude credential, e.g. an offline or
    // non-Claude run) disables summaries entirely and every surface falls back to
    // the task brief exactly as before — this is cosmetic, never load-bearing.
    // The tracker owns the lightweight backend AND the store handle, so it is
    // the single writer for all three refresh points (startup here, mid-round
    // from `engine::run_turn`, round boundary below) — one client, one code
    // path, one place the durable row is stamped. Attached onto the session so
    // the engine can reach it from inside the tool loop.
    let tracker = store
        .and_then(|s| crate::activity_summary::ActivityTracker::attach(session, s, run_id, &input));
    session.activity = tracker;
    if let Some(t) = session.activity.as_mut() {
        t.refresh_startup().await;
    }

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
            persist_terminal(
                store,
                run_id,
                Phase::Checkpoint,
                Some(&result),
                None,
                session,
            );
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

        // ── Unified operator-control drain (TASK-297). Normalize the three
        // control sources into the prioritized channel, then drain ONCE at this
        // round boundary and dispatch highest-priority first (interrupt > steer
        // > output-mode). Delivery is round-boundary — a signal that arrives
        // mid-turn lands on the next round.
        //
        //  * interrupt — a Ctrl-C latched BETWEEN turns (an in-turn Ctrl-C is
        //    consumed by the engine seam, which ends the turn with an
        //    `Interrupted` banner handled after the turn; the atomic swap means
        //    only one of the two seams ever sees a given interrupt).
        //  * steer     — durable `:tell`/SendMessage interjections from the mailbox.
        //  * output-mode — a change to the live-stream (`:worker-output`) toggle.
        //    Producer today is the self-poll below; the channel is the seam a
        //    future inbound parent→coordinator transport plugs into.
        if take_interrupt() {
            control.sender().interrupt();
        }
        if let Some(s) = store {
            for m in mailbox_ids
                .iter()
                .flat_map(|id| s.drain_messages(id).unwrap_or_default())
            {
                // Defect 1 (mid-flight flip): a message whose WHOLE body is an
                // output directive is a CONTROL signal, not context for the
                // model. Route it to `OutputMode` so `tell <run> ":output on"`
                // opens a stacked coordinator's stream without a restart —
                // previously the only producer was the self-poll below, which in
                // a headless run can never change.
                let _delivered = match parse_output_directive(&m) {
                    Some(mode) => control.sender().output_mode(mode),
                    None => control.sender().steer(m),
                };
            }
        }
        {
            let mode = session.worker_output_mode();
            if mode != last_output_mode {
                control.sender().output_mode(mode);
                last_output_mode = mode;
            }
        }

        let mut interrupted = false;
        let mut steers: Vec<String> = Vec::new();
        for signal in control.drain_prioritized() {
            match signal {
                ControlSignal::Interrupt => interrupted = true,
                ControlSignal::Steer(m) => steers.push(m),
                ControlSignal::OutputMode(mode) => {
                    session.set_worker_output_mode(mode);
                    // Keep the self-poll baseline in step, or it would re-enqueue
                    // this same change next round and log it twice.
                    last_output_mode = mode;
                    eprintln!("\x1b[2maish: operator set worker-output {mode:?}\x1b[0m");
                }
            }
        }
        if interrupted {
            // A between-turns interrupt: reassess before doing more work. Same
            // directive the engine-banner path folds, so the two seams match.
            next_input = if next_input.trim().is_empty() {
                OPERATOR_REASSESS.to_string()
            } else {
                format!("{OPERATOR_REASSESS}\n\n{next_input}")
            };
            eprintln!(
                "\x1b[2maish: operator interrupt (between turns) — reassessing this round\x1b[0m"
            );
        }
        if !steers.is_empty() {
            // Prepend so the operator's steer is the first thing the model reads
            // this round, ahead of the task continuation text (matches the prior
            // `fold_operator_messages` framing/order).
            let interjection = format_interjection(&steers);
            next_input = format!("{interjection}\n\n{next_input}");
            // Plain (no 🔧/🗨/📦 sentinel) so the parent's worker stream leaves it
            // in the failure tail without forwarding or pulsing the prompt badge.
            eprintln!(
                "✉ folded {} operator message(s) into this round",
                steers.len()
            );
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
            // Nobody is at the keyboard: this hook says yes to everything, so the
            // sensitive-path gate must REFUSE rather than "confirm" (SEC-2.3).
            crate::sensitive::set_unattended(true);
            let mut allow = |_: &str| tools::Decision::AllowOnce;
            let answer =
                match crate::engine::run_turn(backend, session, next_input, &mut allow).await {
                    Ok(a) => a,
                    Err(e) => {
                        let error = format!("stand-down wrap-up turn failed: {e:#}");
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
            eprintln!("⏸ checkpoint requested by parent — halting at round boundary (resumable)");
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
        // Nobody is at the keyboard: this hook says yes to everything, so the
        // sensitive-path gate must REFUSE rather than "confirm" (SEC-2.3).
        crate::sensitive::set_unattended(true);
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

        // ── Refresh the status-line activity summary (see the module-start hook
        // above). Placed at the END of every round, after the synthesis is in
        // hand, so `:workers` tracks what the worker is doing NOW rather than what
        // it was launched to do. The round's synthesis is the single best signal
        // available: it is the model's own narrative of what it just finished.
        //
        // Deliberately BEFORE the abnormal-exit evaluation below: a loop-detected
        // or budget-exhausted round is exactly when an operator reads `:workers`,
        // so that round's summary must land even though the round ended badly.
        // Failure is swallowed — a summarizer hiccup leaves the prior summary in
        // place and the run proceeds untouched.
        if let Some(t) = session.activity.as_mut() {
            t.refresh_round(rounds as u64, &answer).await;
        }

        // ── Worker-exit evaluation: did this round's turn end abnormally? The
        // engine tags a loop-detected / forced-summarize / budget-exhausted stop
        // with a parseable banner on the first line of the answer. Decide a
        // recovery disposition rather than treating the (possibly partial) answer
        // as a finished result.
        if let Some(exit) = crate::loopguard::RoundExit::evaluate(
            &answer,
            auto_recoveries,
            crate::loopguard::MAX_AUTO_RECOVERIES,
        ) {
            // One evaluated `RoundExit` bundles the stop's reason AND its
            // disposition, so this single `match` decides the recovery action in
            // one place — the reason and the action can't drift across separate
            // reads. Only an ABNORMAL stop reaches here (a normal answer has no
            // banner and `evaluate` returns `None`).
            match exit.disposition {
                // Operator Ctrl-C interrupt — the only abnormal reason that
                // classifies to `None`: NOT a failure and NOT an auto-recovery.
                // Keep the coordinator alive, fold a reassess directive, and
                // drive the next round — where fold_operator_messages also picks
                // up any fresh `:tell` steer sent alongside the interrupt.
                crate::loopguard::Disposition::None => {
                    eprintln!(
                        "\x1b[2maish: round {rounds} interrupted by operator (Ctrl-C) — reassessing\x1b[0m"
                    );
                    next_input = OPERATOR_REASSESS.to_string();
                    continue;
                }
                // Auto-resume the work from where it left off, or nudge the model
                // off a loop — feed the matching directive into the next round and
                // keep driving (still bounded by `round_cap`). The completed work
                // is preserved in history + the turn-audit replay, so a resume
                // continues rather than restarting from scratch.
                crate::loopguard::Disposition::Resume | crate::loopguard::Disposition::Nudge => {
                    // SerialChainYield is tracked with its own counter so that
                    // coordinators doing genuinely sequential work (git workflows,
                    // step-by-step investigation) are not killed by the same cap
                    // that governs true runaway loops and budget exhaustion.
                    let is_serial_chain = matches!(
                        exit.reason,
                        crate::loopguard::ExitReason::SerialChainYield { .. }
                    );
                    let (recovery_count, recovery_cap) = if is_serial_chain {
                        serial_chain_recoveries += 1;
                        (
                            serial_chain_recoveries,
                            crate::loopguard::MAX_SERIAL_CHAIN_RECOVERIES,
                        )
                    } else {
                        auto_recoveries += 1;
                        (auto_recoveries, crate::loopguard::MAX_AUTO_RECOVERIES)
                    };
                    eprintln!(
                        "\x1b[2maish: round {rounds} ended [{}] — {} (auto-recovery {recovery_count}/{recovery_cap})\x1b[0m",
                        exit.reason.tag(),
                        exit.disposition.verb(),
                    );
                    // If the serial-chain counter is itself exhausted, flag the
                    // operator — the coordinator is stuck in serial-only mode
                    // despite repeated nudges to batch.
                    if is_serial_chain
                        && serial_chain_recoveries >= crate::loopguard::MAX_SERIAL_CHAIN_RECOVERIES
                    {
                        let error = format!(
                            "flagged for operator after {serial_chain_recoveries} serial-chain-yield attempt(s): {}",
                            exit.reason.detail()
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
                    next_input = exit.directive().unwrap_or_else(|| {
                        "Continue the task from where you left off.".to_string()
                    });
                    continue;
                }
                // FlagOperator: auto-recovery is exhausted (or the stop isn't one
                // to paper over). Stop the run and record a clear, human-actionable
                // failure so the operator can take over — the partial answer is
                // preserved on the worktree branch + the turn-audit journal.
                crate::loopguard::Disposition::FlagOperator => {
                    let error = format!(
                        "flagged for operator after {auto_recoveries} auto-recovery attempt(s): {}",
                        exit.reason.detail()
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
            }
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
        let late = fold_operator_messages(store, &mailbox_ids, &mut late_input);
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

    // Second half of the durable-activity bound (the first is the per-run byte
    // cap enforced by `ActivityLogWriter`): age out whole logs for runs that
    // finished long ago. Only the launching interactive session does this —
    // children returned above — so 100+ concurrent coordinators never stampede
    // the directory.
    let _ = crate::activity_log::sweep_older_than(crate::activity_log::RETAIN_DAYS);

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
                if let Some(result) = &row.result
                    && !result.trim().is_empty()
                {
                    if digest {
                        print_completed(&row.run_id, result);
                    }
                    surfaced += 1;
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
    // Runs BEFORE salvage and BEFORE the post-sweep id snapshot, so the id set
    // below is deliberately POST-reap and cannot distinguish a row lost to early
    // termination from one that merely aged out. Salvage therefore also consults
    // the never-trimmed `worktree_lifecycle` ledger — without that second gate a
    // finished run's leftover dirty tree is re-minted as a phantom `failed` row
    // on every startup (coordinator-lifecycle bug: salvage false positives).
    let reaped_failed = reap_failed_runs(&store);
    // Salvage runs whose durable row was lost on early termination. Run AFTER the
    // terminal-row purge + failed-row reap and key off a FRESH post-sweep id set,
    // so the `failed` salvage rows we write this pass survive the boot. Gated by
    // the lifecycle ledger inside `salvage_orphaned_worktrees` so only genuinely
    // unowned trees are salvaged.
    let known_after: HashSet<String> = store
        .load_all()
        .map(|rows| rows.into_iter().map(|r| r.run_id).collect())
        .unwrap_or_default();
    let salvaged = salvage_orphaned_worktrees(&session.cwd, &store, &known_after, digest);
    // ISS-409757: the filesystem scan above can only see leaves under THIS repo's
    // worktree root that still hold work. Cross-check the lifecycle LEDGER, which
    // catches exactly the leaks that scan structurally cannot: trees created by a
    // run in ANOTHER repo/checkout, and empty trees a failed teardown left behind.
    let leaked = report_leaked_worktrees(&store, digest);
    // TASK-289: scan the durable coordinator registry — mark rows whose owning
    // process is dead as `orphaned` (parent-death recovery) and log any that
    // carried an in-flight batch job as resurrectable (full resume is TASK-291).
    let (regs_reaped, regs_resurrectable) = scan_coordinator_registry(&store, digest);

    // #129: detect and reap stalled coordinators (coordinating/awaiting_batch but
    // no recent heartbeat activity). This catches the case where a coordinator
    // process hangs/deadlocks without crashing.
    let stalled_reaped = detect_and_reap_stalled_runs(&store, digest);

    if digest
        && (surfaced > 0
            || reaped > 0
            || salvaged > 0
            || reaped_failed > 0
            || leaked > 0
            || regs_reaped > 0
            || regs_resurrectable > 0
            || stalled_reaped > 0)
    {
        eprintln!(
            "\x1b[2maish: reattached coordinator runs ({surfaced} delivered, {reaped} reaped, {salvaged} salvaged, {leaked} leaked-worktrees, {reaped_failed} failed-pruned, {regs_reaped} registry-orphaned, {regs_resurrectable} resurrectable, {stalled_reaped} stalled-reaped)\x1b[0m"
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
///
/// ZOMBIES COUNT AS DEAD. A background coordinator is spawned as a CHILD of the
/// interactive aish process; when it exits, the kernel keeps its pid slot as a
/// `Z (defunct)` entry until the parent `wait()`s. aish never waits on detached
/// coordinator children, so `kill(pid, 0)` on an exited coordinator returns
/// `Ok` — "alive" — for the entire remaining life of the session. Every reaper
/// here is gated on this predicate (a live pid is the HARD evidence that vetoes
/// a stale heartbeat), so a zombie child pinned its `coordinating` row forever:
/// the worker showed as live-and-coordinating in `:workers` /
/// `background_status` long after it had finished, and only a restart (which
/// re-parents the zombies to init, which reaps them) cleared it. Checking the
/// `/proc` state field demotes a defunct child to dead and lets the stall
/// reaper do its job in a long-lived session.
fn pid_is_alive(pid: i64) -> bool {
    if pid <= 0 {
        return false;
    }
    // SAFETY: kill with signal 0 is the documented liveness probe; it never
    // delivers a signal, only reports existence/permission via errno.
    let rc = unsafe { libc::kill(pid as libc::pid_t, 0) };
    let exists = if rc == 0 {
        true
    } else {
        // rc == -1: exists only when the failure is EPERM (exists, not
        // permitted), gone on ESRCH (no such process).
        std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    };
    exists && !pid_is_zombie(pid)
}

/// True when `pid` names a process that has already exited but has not been
/// reaped by its parent (state `Z`, "defunct"). Reads `/proc/<pid>/stat`; a
/// missing/unreadable `/proc` (non-Linux, hidepid, racing exit) answers `false`
/// so behaviour degrades to the plain signal-0 probe rather than declaring live
/// processes dead.
fn pid_is_zombie(pid: i64) -> bool {
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(stat) => proc_stat_is_zombie(&stat),
        Err(_) => false,
    }
}

/// Pure parser for a `/proc/<pid>/stat` line: is the state field `Z`?
///
/// Field 2 is the executable name in parentheses and MAY CONTAIN SPACES AND
/// PARENS (`(my prog (x))`), so the state char cannot be found by splitting on
/// whitespace — it is the first non-space character after the LAST `)`.
fn proc_stat_is_zombie(stat: &str) -> bool {
    match stat.rfind(')') {
        Some(close) => stat[close + 1..].trim_start().starts_with('Z'),
        None => false,
    }
}

/// A non-terminal run whose heartbeat is older than this is considered STALLED.
/// Distinct from `ORPHAN_STALE_AFTER`: an orphan's owner PROCESS is gone, while
/// a stalled run may still be alive (a deadlock or a wedged tool call), so the
/// pid-liveness scan never catches it. A healthy run beats every
/// `HEARTBEAT_INTERVAL` (30s), so this is ~10 consecutive missed beats.
const STALL_AFTER: Duration = Duration::from_secs(5 * 60);

/// Pure predicate: should this coordinator row be reaped as STALLED? (#129)
///
/// Stalled = non-terminal phase (`Coordinating`/`AwaitingBatch`) whose last
/// heartbeat is older than `STALL_AFTER`. Heartbeats are SQLite
/// `current_timestamp` strings — UTC `"YYYY-MM-DD HH:MM:SS"`, NOT unix-epoch
/// integers — so they MUST go through `parse_sqlite_timestamp`. Parsing one as
/// an integer yields `Err` for every row, which previously collapsed the
/// heartbeat to `0` and reaped every live coordinator on the first read.
///
/// Fail-OPEN on a missing/unparseable heartbeat: reaping marks a run failed and
/// abandons its in-flight work, so a timestamp we cannot read is never grounds
/// to kill. A genuinely dead run is still reaped by the pid-liveness orphan
/// scan (`is_orphaned_row`) — the irreversible action stays with the check that
/// has hard evidence.
fn is_stalled_row(phase: &str, heartbeat_at: Option<&str>, now: i64) -> bool {
    if !matches!(
        Phase::parse(phase),
        Phase::Coordinating | Phase::AwaitingBatch
    ) {
        return false; // Done/Failed/Checkpoint are not stall candidates
    }
    let Some(hb) = heartbeat_at else {
        return false; // no beat recorded → fail open, let the orphan scan judge
    };
    match parse_sqlite_timestamp(hb) {
        Some(beat) => now.saturating_sub(beat) > STALL_AFTER.as_secs() as i64,
        None => false, // unparseable → fail open (never reap on a bad read)
    }
}

/// WHICH KIND of silence a stalled row represents (ISS-407772). The reaper used
/// to collapse both into one message — "no heartbeat activity for 5+ minutes" —
/// which is the single least actionable thing it could say, because the two cases
/// have different causes and different recoveries:
///   * `NeverStarted` — the run never emitted ONE durable beat, so it died during
///     LAUNCH: worktree/index-lock contention, a rate-limited first upstream call,
///     or a child that never reached `drive()`. Re-dispatching (ideally staggered)
///     is the fix.
///   * `WentSilent`  — it beat, then stopped: a genuine mid-flight hang or a wedged
///     tool call. The transcript up to the hang is worth reading before retrying.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StallKind {
    NeverStarted,
    WentSilent,
}

impl StallKind {
    fn label(self) -> &'static str {
        match self {
            StallKind::NeverStarted => {
                "never emitted a heartbeat — it failed during launch (worktree contention, \
                 a rate-limited first call, or a child that never started); re-dispatch it"
            }
            StallKind::WentSilent => {
                "heartbeated, then went silent — a mid-flight hang; check its transcript \
                 before re-dispatching"
            }
        }
    }
}

/// Classify a stalled row. `heartbeat_at` is seeded by the INSERT's
/// `DEFAULT current_timestamp` — evaluated to the SAME value as `created_at`
/// within one statement — so a row whose beat still equals its creation stamp was
/// never touched by the keeper thread and therefore never started. The keeper
/// beats immediately on entering `drive()`, so this stays a tight signal. Pure →
/// unit-tested.
fn stall_kind(created_at: Option<&str>, heartbeat_at: Option<&str>) -> StallKind {
    match (created_at, heartbeat_at) {
        (_, None) => StallKind::NeverStarted,
        (Some(created), Some(beat)) if created == beat => StallKind::NeverStarted,
        _ => StallKind::WentSilent,
    }
}

/// #129: detect and reap stalled coordinators — non-terminal runs that stopped
/// beating (deadlocks / hangs that do NOT kill the OS process, which the
/// pid-liveness orphan scan therefore misses). Returns the count reaped.
fn detect_and_reap_stalled_runs(store: &CoordinatorStore, digest: bool) -> usize {
    let rows = match store.load_all() {
        Ok(r) => r,
        Err(_) => return 0, // non-fatal
    };
    let now = now_unix_secs();
    let mins = STALL_AFTER.as_secs() / 60;

    // Hard evidence beats circumstantial evidence: a stale heartbeat only says
    // "nobody stamped the row lately", which a starved keeper thread, a paused
    // machine, or a wedged SQLite write can all produce on a perfectly healthy
    // run. A LIVE pid says "this coordinator still exists" — so a run whose
    // registered process is alive is never reaped, however old its beat. Runs
    // with no registry row (pre-TASK-289 rows) keep the old heartbeat-only
    // behaviour, since there is no better signal available for them.
    let live_pids: std::collections::HashMap<String, i64> = store
        .get_live_runs()
        .unwrap_or_default()
        .into_iter()
        .map(|r| (r.coord_id, r.pid))
        .collect();

    let mut stalled_reaped = 0usize;
    for row in rows {
        if !is_stalled_row(&row.phase, row.heartbeat_at.as_deref(), now) {
            continue;
        }
        if live_pids
            .get(&row.run_id)
            .is_some_and(|&pid| pid_is_alive(pid))
        {
            continue; // process still alive — stale beat, not a dead run
        }
        let short_id = crate::batch::short_id(&row.run_id);
        // Say WHICH failure this was. A launch-time death and a mid-flight hang
        // read identically in the row (both are just an old beat) but need
        // different operator action — see `StallKind` (ISS-407772).
        let kind = stall_kind(row.created_at.as_deref(), row.heartbeat_at.as_deref());
        let reason = format!(
            "stalled: no heartbeat activity for {mins}+ minutes — {}",
            kind.label()
        );
        if store.set_failed(&row.run_id, &reason).is_ok() {
            stalled_reaped += 1;
            if digest {
                eprintln!(
                    "\x1b[2maish: coordinator {short_id} stalled after {mins}+ minutes — {} — marked failed\x1b[0m",
                    kind.label()
                );
            }
        }
    }

    stalled_reaped
}

/// Pure salvage decision — a TRUE BACKSTOP, not a second opinion on completed
/// runs. A work-bearing worktree is salvaged only when NOTHING durable says a
/// real run ever owned it:
///
/// * `has_row` — a surviving `coordinator_runs` row in ANY phase (`done`,
///   `failed`, `coordinating`, `checkpoint`, …). The normal lifecycle owns it,
///   so don't double-report.
/// * `ledger_known` — a `worktree_lifecycle` row for this tree. The run row is
///   written at SPAWN (see `insert_with_parent` in `run_coordinator`) but is
///   later TRIMMED by bounded retention, so `!has_row` alone cannot distinguish
///   "row lost to early termination" from "row aged out after the run finished".
///   The ledger is written once at `git worktree add` and never deleted, so its
///   presence is durable proof the tree belonged to a real run. A finished run's
///   leftover tree is a CLEANUP problem (see `report_leaked_worktrees`), not a
///   failure, and minting a `failed` row for it is a phantom.
/// * `has_work` — the leaf is dirty or ahead; nothing to recover otherwise.
///
/// Unit-tested. (coordinator-lifecycle bug: salvage false positives.)
fn is_salvageable(has_row: bool, ledger_known: bool, has_work: bool) -> bool {
    has_work && !has_row && !ledger_known
}

/// Recover runs whose durable row was lost on early termination: scan the managed
/// worktree root for work-bearing leaves (uncommitted changes or commits ahead),
/// and for any with no surviving store row AND no lifecycle-ledger row, insert a
/// `failed` salvage row and announce the recoverable branch/path — so the
/// otherwise-invisible work shows up in `:workers` again and an operator can
/// review/merge it. Best-effort: a store write that fails is skipped, never
/// sinking startup. Returns the count salvaged.
///
/// The ledger set is read here (not passed in) because the caller's `known`
/// snapshot is deliberately taken AFTER the retention reapers, so it cannot tell
/// a lost row from a trimmed one — see [`is_salvageable`]. A ledger read error
/// yields an EMPTY set, which falls back to the old (row-only) behaviour rather
/// than silently disabling the backstop.
fn salvage_orphaned_worktrees(
    cwd: &std::path::Path,
    store: &CoordinatorStore,
    known: &HashSet<String>,
    announce: bool,
) -> usize {
    let ledger = store.ledger_worktree_ids().unwrap_or_default();
    salvage_work_bearing(
        crate::worker::work_bearing_worktrees(cwd),
        store,
        known,
        &ledger,
        announce,
    )
}

/// The gating + insert half of [`salvage_orphaned_worktrees`], split from the
/// filesystem scan so the contract can be unit-tested against a real store
/// without materialising git worktrees.
fn salvage_work_bearing(
    candidates: Vec<crate::worker::OrphanWork>,
    store: &CoordinatorStore,
    known: &HashSet<String>,
    ledger: &HashSet<String>,
    announce: bool,
) -> usize {
    let mut salvaged = 0usize;
    for w in candidates {
        if !is_salvageable(known.contains(&w.id), ledger.contains(&w.id), true) {
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

/// Age (in hours) after which an OPEN `worktree_lifecycle` row is treated as a
/// leak candidate. Set comfortably past `WORKER_TIMEOUT` so a long-but-healthy
/// run is never reported as leaked — the ledger's value is that it stays quiet
/// until a tree is genuinely abandoned.
const WORKTREE_LEAK_AFTER_HOURS: i64 = 24;

/// Startup cross-check of the worktree lifecycle ledger (ISS-409757).
///
/// `salvage_orphaned_worktrees` scans the FILESYSTEM under the current repo's
/// worktree root and only notices leaves that still hold work. Two leak classes
/// are invisible to it by construction: a tree created while the operator was in
/// a DIFFERENT repo (different root, never scanned), and a tree whose teardown
/// failed but which carries no changes (scanned, ignored as empty). Both stay on
/// disk forever. The ledger records every tree at CREATE time, so any row still
/// open long after its run is a leak candidate regardless of where it lives.
///
/// Rows whose path is gone are CLOSED (removed by hand, or by a sweep that
/// couldn't reach the ledger) rather than reported forever. Rows whose path
/// survives are REPORTED, labelled by whether the tree holds work — this
/// function deliberately never deletes anything: removal is irreversible and an
/// operator's unmerged branch is exactly what's at stake. Best-effort: a store
/// read error reports 0 and never sinks startup. Returns the count reported.
fn report_leaked_worktrees(store: &CoordinatorStore, announce: bool) -> usize {
    let Ok(rows) = store.list_orphaned_worktrees(WORKTREE_LEAK_AFTER_HOURS) else {
        return 0;
    };
    let mut reported = 0usize;
    for (id, path, run_id) in rows {
        let leaf = std::path::PathBuf::from(&path);
        if !leaf.exists() {
            let _ = store.record_worktree_cleaned_up(&id);
            continue;
        }
        reported += 1;
        if announce {
            let verdict = if crate::worker::worktree_holds_work(&leaf) {
                "HOLDS WORK — review/merge its branch before removing"
            } else {
                "no changes — safe to `git worktree remove`"
            };
            eprintln!(
                "\x1b[2maish: leaked worktree {id} (run {run_id}) still on disk at {path} — {verdict}\x1b[0m"
            );
        }
    }
    reported
}

/// Count active (non-terminal) coordinator runs in the durable store whose
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

/// Live STALL reap for the status-read paths (`:workers`, `background_status`),
/// the counterpart to [`reap_orphaned_runs`].
///
/// The orphan reaper alone leaves a real zombie class visible: it requires the
/// row to be owned by a DIFFERENT session (`session_id != own`) and its
/// heartbeat to exceed `ORPHAN_STALE_AFTER` (15 min). A coordinator that fans
/// out sub-coordinators and then exits takes its children down with it — the
/// children's rows are left `coordinating`, and because they were stamped with
/// the SAME `session_id` as the reader, `is_orphaned_row` skips them FOREVER, no
/// matter how stale the heartbeat gets. Observed in the wild: four sub-workers
/// killed ~10s after spawn when their parent exited, all four still reported
/// `coordinating` long afterwards.
///
/// `detect_and_reap_stalled_runs` has no session filter and a 5-minute
/// threshold, so it catches exactly that case — but it was only ever wired into
/// the STARTUP path (`reattach_saved_runs`), and a long-lived interactive
/// session never restarts. Running it on every status read closes the gap:
/// same-session zombies self-heal in <=5 min instead of never.
///
/// `digest: false` — an interactive status read prints its own table, so the
/// startup digest line would be noise here. Returns the number reaped.
pub fn reap_stalled_runs_live(store: &CoordinatorStore) -> usize {
    detect_and_reap_stalled_runs(store, false)
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
/// Never sinks a completing run: a store error can't propagate out. But it is
/// no longer SILENT. The terminal write is the single point where a child hands
/// its outcome to the parent, and dropping it on the floor is exactly what
/// produces the parent-side
/// `reconciled orphaned coordinator row <id> (child exited without finalizing
/// its status)` notice — the child DID finish, its one durable write just lost a
/// race (typically `SQLITE_BUSY` from a sibling coordinator holding the write
/// lock) and nobody noticed. So we retry with exponential backoff
/// ([`TERMINAL_PERSIST_ATTEMPTS`] attempts starting at
/// [`TERMINAL_PERSIST_BACKOFF_MS`] ms), and if every attempt fails we say so on
/// stderr instead of exiting quietly and letting the parent guess.
fn persist_terminal(
    store: Option<&CoordinatorStore>,
    run_id: &str,
    phase: Phase,
    result: Option<&str>,
    error: Option<&str>,
    session: &Session,
) {
    let Some(s) = store else { return };
    let metrics = crate::coordinator_store::RunMetrics {
        tokens_in: session.tokens_in as u64,
        tokens_out: session.tokens_out as u64,
        turns: session.turns_total as u64,
        tool_calls: session.tool_calls_total as u64,
    };
    let mut last_err: Option<String> = None;
    for attempt in 1..=TERMINAL_PERSIST_ATTEMPTS {
        match s.finish_run(run_id, phase.as_str(), result, error, metrics) {
            Ok(()) => return,
            Err(e) => {
                last_err = Some(e.to_string());
                if attempt < TERMINAL_PERSIST_ATTEMPTS {
                    std::thread::sleep(terminal_persist_backoff(attempt));
                }
            }
        }
    }
    if let Some(err) = last_err {
        eprintln!(
            "\x1b[33maish: could not persist terminal phase '{}' for {} after {} attempts: {} \
             — the parent will reconcile this row as orphaned\x1b[0m",
            phase.as_str(),
            run_id,
            TERMINAL_PERSIST_ATTEMPTS,
            err
        );
    }
}

/// Attempts for the terminal-phase write before giving up (see
/// [`persist_terminal`]). Five attempts with doubling backoff spans ~1.5s on top
/// of SQLite's own `busy_timeout`, which comfortably outlasts the write-lock
/// window of a sibling coordinator committing its own turn.
const TERMINAL_PERSIST_ATTEMPTS: u32 = 5;

/// First backoff step (ms) between terminal-write retries; doubles each attempt.
const TERMINAL_PERSIST_BACKOFF_MS: u64 = 100;

/// Exponential backoff for retry `attempt` (1-based) of the terminal write:
/// 100ms, 200ms, 400ms, 800ms… Pulled out of [`persist_terminal`] so the
/// schedule is unit-testable without a contended database.
fn terminal_persist_backoff(attempt: u32) -> std::time::Duration {
    let shift = attempt.saturating_sub(1).min(16);
    std::time::Duration::from_millis(TERMINAL_PERSIST_BACKOFF_MS << shift)
}

/// Best-effort current git branch of `dir`, recorded in the worker `meta.json`
/// so retention never reclaims a dir whose worktree still holds kept work on an
/// `aish/…` branch (AC7) — read from git rather than rebuilt from the id, since
/// worker branches are now brief-derived (`aish/{slug}-{id}`, see
/// [`crate::worker`]). `None` outside a repo, on a detached HEAD, or when
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

/// Does the operator-supplied `query` address the run `run_id`? Exact match, or
/// `query` is a PREFIX of the id so an operator can type `g_aB3` instead of the
/// whole thing.
///
/// Single source of truth for `stop` / `tell` / `:stop` candidate resolution
/// (they each used to inline this same closure). Callers are responsible for
/// rejecting an ambiguous query — one that matches more than one run — which is
/// exactly what a NON-UNIQUE run id makes unavoidable: when every goal-loop turn
/// recorded the literal id `goal`, `stop goal` matched 47 rows and the operator
/// had no way to name just one.
pub fn id_matches(run_id: &str, query: &str) -> bool {
    run_id == query || run_id.starts_with(query)
}

/// Resolve `query` against a set of run ids, returning every match. Exposed so
/// the uniqueness invariant `stop` depends on is directly testable: a displayed
/// goal run id must resolve to exactly ONE run. Test-only: the production
/// `stop` / `tell` paths filter their own live candidate lists with
/// [`id_matches`] (they carry a row payload, not a bare id).
#[cfg(test)]
pub fn resolve_run_ids<'a, I>(ids: I, query: &str) -> Vec<&'a str>
where
    I: IntoIterator<Item = &'a str>,
{
    ids.into_iter()
        .filter(|rid| id_matches(rid, query))
        .collect()
}

#[cfg(test)]
mod tests {
    // ── ISS-407772: a stalled row must say WHICH failure it was ──────────────
    //
    // Launch-time death and mid-flight hang are indistinguishable from the beat
    // timestamp alone, but `heartbeat_at` is seeded to `created_at` by the INSERT
    // and the keeper beats immediately on entering `drive()` — so equality means
    // "the keeper never ran", i.e. it never started.
    #[test]
    fn stall_kind_separates_never_started_from_went_silent() {
        use super::{StallKind, stall_kind};
        let created = "2026-03-01 22:00:00";

        // Beat still equals the insert stamp ⇒ the keeper never wrote ⇒ launch death.
        assert_eq!(
            stall_kind(Some(created), Some(created)),
            StallKind::NeverStarted
        );
        // No beat at all ⇒ same conclusion, never reached the keeper.
        assert_eq!(stall_kind(Some(created), None), StallKind::NeverStarted);
        assert_eq!(stall_kind(None, None), StallKind::NeverStarted);

        // Any beat LATER than creation ⇒ it ran, then went quiet: a real hang.
        assert_eq!(
            stall_kind(Some(created), Some("2026-03-01 22:00:01")),
            StallKind::WentSilent
        );
        assert_eq!(
            stall_kind(Some(created), Some("2026-03-01 22:47:13")),
            StallKind::WentSilent
        );
        // Unknown creation stamp but a beat on record ⇒ it beat; don't claim it
        // never started.
        assert_eq!(
            stall_kind(None, Some("2026-03-01 22:00:01")),
            StallKind::WentSilent
        );

        // And the two must never render the same message — the whole point.
        assert_ne!(
            StallKind::NeverStarted.label(),
            StallKind::WentSilent.label()
        );
        assert!(StallKind::NeverStarted.label().contains("launch"));
    }

    // ── ISS-407772 root cause: the keeper must measure ELAPSED time, not sum
    // nominal sleeps ────────────────────────────────────────────────────────
    //
    // The distinction that matters is BOUNDEDNESS. Summing nominal ticks makes
    // the real beat gap scale with however late the OS returns from each sleep —
    // unbounded, and every co-spawned worker shares the same load curve, so they
    // drift in LOCKSTEP and cross the reap threshold together. Reading the
    // monotonic clock caps the gap at one interval plus one overrunning tick, no
    // matter how badly the box is oversubscribed.
    #[test]
    fn heartbeat_interval_must_be_clock_based_not_a_sum_of_nominal_sleeps() {
        use super::{HEARTBEAT_INTERVAL, STALL_AFTER};
        const TICK_NOMINAL_MS: u64 = 250;

        // OLD: `waited += TICK` counts INTENDED time, so a beat fires only after
        // HEARTBEAT_INTERVAL worth of NOMINAL ticks — i.e. `overrun`x that in
        // real time. NEW: the clock is read every tick, so the gap is
        // HEARTBEAT_INTERVAL plus at most one late tick — independent of overrun.
        let ticks_to_beat = HEARTBEAT_INTERVAL.as_millis() as u64 / TICK_NOMINAL_MS;
        let old_gap = |overrun: u64| ticks_to_beat * TICK_NOMINAL_MS * overrun / 1_000;
        let new_gap = |overrun: u64| {
            HEARTBEAT_INTERVAL.as_secs() + (TICK_NOMINAL_MS * overrun).div_ceil(1_000)
        };

        // Under load, sleeps return late by a factor that grows with contention.
        for overrun in [1, 2, 4, 8, 16, 64] {
            // The accumulator's real gap is LINEAR in the overrun factor: nothing
            // bounds it, which is the defect.
            assert_eq!(old_gap(overrun), HEARTBEAT_INTERVAL.as_secs() * overrun);
            // The clock-based gap stays inside the reap threshold at every factor.
            assert!(
                new_gap(overrun) < STALL_AFTER.as_secs(),
                "clock-based beats must stay inside the stall threshold even at {overrun}x \
                 sleep overrun ({}s vs {}s)",
                new_gap(overrun),
                STALL_AFTER.as_secs()
            );
        }

        // At a heavy-but-ordinary fan-out overrun the OLD scheme blows past the
        // reap threshold outright while the new one is still nowhere near it —
        // that gap between the two is what reaped whole waves.
        let heavy = STALL_AFTER.as_secs().div_ceil(HEARTBEAT_INTERVAL.as_secs()) + 1;
        assert!(
            old_gap(heavy) > STALL_AFTER.as_secs(),
            "the nominal-sum accumulator must drift past STALL_AFTER at {heavy}x overrun \
             ({}s real gap vs {}s threshold)",
            old_gap(heavy),
            STALL_AFTER.as_secs()
        );
        assert!(new_gap(heavy) < STALL_AFTER.as_secs());
    }

    // The display layer needs the beat cadence to classify liveness (how many
    // beats a worker has MISSED drives the escalation banner's green/yellow/red
    // heart), but `style` is a pure-formatter module and must not depend on the
    // coordinator. So the cadence is mirrored as `style::HEARTBEAT_INTERVAL_SECS`
    // and pinned here: if someone retunes the writer's interval without the
    // mirror, every missed-beat count silently mis-scales — a liveness lamp that
    // lies is worse than no lamp. This test is the tripwire.
    #[test]
    fn heartbeat_interval_matches_display_const() {
        assert_eq!(
            super::HEARTBEAT_INTERVAL.as_secs() as i64,
            crate::style::HEARTBEAT_INTERVAL_SECS,
            "style::HEARTBEAT_INTERVAL_SECS mirrors the coordinator's beat cadence — \
             update both or the missed-beat tiers mis-scale"
        );
    }

    // A beat is best-effort, but a RUN of failed writes is the contention signal
    // A beat is best-effort, but a RUN of failed writes is the contention signal
    // the old `let _ = …` threw away. Success must clear the counter so a
    // recovered store stops warning.
    #[test]
    fn heartbeat_write_success_clears_the_failure_counter() {
        use super::beat_once;
        let path = std::env::temp_dir().join(format!("aish_beat_once_{}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let store = crate::db::CoordinatorStore::open(&path).unwrap();
        store.insert("run_beat", "task", "s", None).unwrap();

        let mut failures = 7; // pretend the store was unwritable for a while
        beat_once(&store, "run_beat", &mut failures);
        assert_eq!(failures, 0, "a successful beat must reset the failure run");

        // Repeated successes keep it at zero (no spurious warnings).
        beat_once(&store, "run_beat", &mut failures);
        assert_eq!(failures, 0);
        const {
            assert!(
                super::HEARTBEAT_FAILURE_LOG_AFTER >= 2,
                "a single lost beat must stay quiet"
            )
        };
        let _ = std::fs::remove_file(&path);
    }

    /// Defect 1 (mid-flight half): a headless coordinator has no REPL, so the
    /// ONLY way to open its `:output` gate after launch is a `tell`. A steer whose
    /// whole body is an output directive becomes a control signal; anything else
    /// stays an ordinary steer that reaches the model.
    #[test]
    fn tell_output_directive_becomes_a_control_signal() {
        use crate::worker::WorkerOutputMode::{Off, On};
        for on in [
            ":output on",
            "output on",
            "OUTPUT ON",
            ":worker-output on",
            "worker output 1",
            "  :output  true  ",
            ":output yes",
        ] {
            assert_eq!(
                super::parse_output_directive(on),
                Some(On),
                "{on:?} should open the stream"
            );
        }
        for off in [
            ":output off",
            "worker-output 0",
            ":output false",
            "output no",
        ] {
            assert_eq!(super::parse_output_directive(off), Some(Off), "{off:?}");
        }
        // NOT directives — these must reach the model as normal steers. A bare
        // `:output` is ambiguous for a coordinator (nothing to toggle against).
        for steer in [
            ":output",
            "output the report to a file",
            "please turn on the output when you get a chance",
            "",
            ":stop",
        ] {
            assert_eq!(
                super::parse_output_directive(steer),
                None,
                "{steer:?} must stay a steer"
            );
        }
    }

    /// The terminal-write retry schedule doubles and stays bounded, so a child
    /// that loses the write race to a sibling gets ~1.5s of retries before the
    /// parent is left to reconcile its row as orphaned.
    #[test]
    fn terminal_persist_backoff_doubles_and_totals_over_a_second() {
        let steps: Vec<u64> = (1..super::TERMINAL_PERSIST_ATTEMPTS)
            .map(|a| super::terminal_persist_backoff(a).as_millis() as u64)
            .collect();
        assert_eq!(steps, vec![100, 200, 400, 800]);
        assert!(steps.iter().sum::<u64>() >= 1_000);
    }

    /// Regression: the zombie class the live stall reap exists for.
    ///
    /// A sub-coordinator torn down when its PARENT process exited keeps the
    /// parent's `session_id`. `is_orphaned_row` skips same-session rows by
    /// design, so no amount of staleness ever makes it an orphan — the row
    /// would sit at `coordinating` forever on the status-read path. The stall
    /// predicate has no session filter, so it is the one that must catch it.
    #[test]
    fn same_session_dead_child_is_never_orphan_but_is_stalled() {
        let hb = "2024-01-01 00:00:00";
        let beat = super::parse_sqlite_timestamp(hb).expect("parseable heartbeat");
        // Ancient heartbeat, but the row belongs to the READING session.
        assert!(
            !super::is_orphaned_row(
                Some("sess-1"),
                "sess-1",
                &super::Phase::Coordinating,
                Some(hb)
            ),
            "same-session rows are excluded from the orphan reap by design"
        );
        // 10 minutes of silence — past STALL_AFTER (5 min).
        assert!(
            super::is_stalled_row("coordinating", Some(hb), beat + 10 * 60),
            "stall reap must catch it; nothing else will"
        );
    }

    /// The two reapers must not disagree about terminal rows: a `done`/`failed`
    /// row is never a stall candidate, so wiring the stall reap into the live
    /// status read cannot re-fail an already-finished run.
    #[test]
    fn terminal_rows_are_not_stall_candidates() {
        let hb = "2024-01-01 00:00:00";
        let beat = super::parse_sqlite_timestamp(hb).unwrap();
        let ancient = beat + 10 * 60;
        for phase in ["done", "failed"] {
            assert!(
                !super::is_stalled_row(phase, Some(hb), ancient),
                "{phase} must not be reaped as stalled"
            );
        }
    }

    #[test]
    fn heartbeat_stale_threshold_matches_display_const() {
        // Display threshold (style::fmt_heartbeat_age) mirrors the reaper's
        // ORPHAN_STALE_AFTER so "⚠ stale" in :workers / background_status lines
        // up with when a coordinator row actually gets reaped.
        assert_eq!(
            super::ORPHAN_STALE_AFTER.as_secs() as i64,
            crate::style::HEARTBEAT_STALE_AFTER_SECS
        );
    }

    use super::*;

    /// A stale heartbeat is CIRCUMSTANTIAL; a live pid is HARD evidence.
    /// A run whose registered process is still alive must never be reaped —
    /// that is the class of false positive that stamped healthy live runs
    /// `failed` with "stalled: no heartbeat activity for 5+ minutes".
    #[test]
    fn stall_reap_spares_runs_whose_process_is_still_alive() {
        let path = std::env::temp_dir().join(format!("aish_stallpid_{}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let store = CoordinatorStore::open(&path).unwrap();

        // Two identically-stale rows. Only one has a live process behind it.
        store.insert("run_alive", "long build", "s", None).unwrap();
        store.insert("run_dead", "abandoned", "s", None).unwrap();
        store
            .register_run(
                "run_alive",
                1,
                i64::from(std::process::id()),
                None,
                "coordinating",
                None,
            )
            .unwrap();

        store.backdate_heartbeat_for_test("run_alive", 30);
        store.backdate_heartbeat_for_test("run_dead", 30);

        assert_eq!(detect_and_reap_stalled_runs(&store, false), 1);

        let rows = store.load_all().unwrap();
        let phase = |id: &str| rows.iter().find(|r| r.run_id == id).unwrap().phase.clone();
        assert_eq!(
            phase("run_alive"),
            "coordinating",
            "live pid vetoes the reap"
        );
        assert_eq!(phase("run_dead"), "failed", "no live process → reaped");

        let _ = std::fs::remove_file(&path);
    }

    /// `/proc/<pid>/stat` field 2 is `(comm)` and may contain spaces AND
    /// parens, so the state char is the first non-space byte after the LAST
    /// `)` — never `split_whitespace().nth(2)`.
    #[test]
    fn proc_stat_zombie_state_parses_past_a_nasty_comm() {
        assert!(super::proc_stat_is_zombie("6747 (python3) Z 6635 6635 0"));
        assert!(!super::proc_stat_is_zombie("6635 (aish) S 1896 6635 0"));
        assert!(!super::proc_stat_is_zombie("1 (systemd) R 0 1 1"));
        // comm with embedded spaces + parens must not shift the state field.
        assert!(super::proc_stat_is_zombie("42 (next-server (v1)) Z 1 42 0"));
        assert!(!super::proc_stat_is_zombie(
            "42 (next-server (v1)) S 1 42 0"
        ));
        // Garbage degrades to "not a zombie" rather than panicking.
        assert!(!super::proc_stat_is_zombie(""));
        assert!(!super::proc_stat_is_zombie("no parens here"));
    }

    /// REGRESSION (stale `coordinating` workers): a coordinator child that has
    /// EXITED but not been `wait()`ed stays in the process table as
    /// `Z (defunct)`, and `kill(pid, 0)` happily reports it alive. Because a
    /// live pid is the hard evidence that vetoes the stall reap, such a zombie
    /// used to pin its row in `coordinating` for the rest of the session — the
    /// "workers go stale" symptom. A defunct pid must read as DEAD so the row
    /// gets reaped.
    #[cfg(target_os = "linux")]
    #[test]
    fn stall_reap_treats_a_defunct_child_as_dead() {
        // Spawn + let it exit, but deliberately do NOT wait() — that is exactly
        // what aish does with detached coordinator children.
        let mut child = std::process::Command::new("/bin/true")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn /bin/true");
        let zombie_pid = i64::from(child.id());
        // Wait for the exit to land without reaping it.
        for _ in 0..100 {
            if super::pid_is_zombie(zombie_pid) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(
            super::pid_is_zombie(zombie_pid),
            "child should be defunct (unreaped) by now"
        );
        assert!(
            !super::pid_is_alive(zombie_pid),
            "a defunct child must NOT count as a live process"
        );

        let path = std::env::temp_dir().join(format!("aish_zombiepid_{}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let store = CoordinatorStore::open(&path).unwrap();
        store
            .insert("run_zombie", "finished long ago", "s", None)
            .unwrap();
        store
            .register_run("run_zombie", 1, zombie_pid, None, "coordinating", None)
            .unwrap();
        store.backdate_heartbeat_for_test("run_zombie", 30);

        assert_eq!(
            detect_and_reap_stalled_runs(&store, false),
            1,
            "zombie-owned stale row must be reaped"
        );
        let rows = store.load_all().unwrap();
        assert_eq!(
            rows.iter()
                .find(|r| r.run_id == "run_zombie")
                .unwrap()
                .phase,
            "failed"
        );

        let _ = child.wait(); // clean up the zombie
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn phase0_guard_directs_existence_check_before_build() {
        // TASK-355: the guard must tell a coordinator to verify the work isn't
        // already done before building — repospec + grep + git/gh/peer scan,
        // then STOP with evidence instead of re-implementing shipped work.
        let g = PHASE0_GUARD;
        assert!(g.contains("PHASE-0 GUARD"), "labelled block header present");
        assert!(
            g.contains(".repospec.json"),
            "reads the repospec features map"
        );
        assert!(g.contains("grep"), "greps for the feature symbol");
        assert!(
            g.contains("gh pr list") || g.contains("git branch"),
            "checks git/gh for an existing branch or PR"
        );
        assert!(
            g.contains("background_status"),
            "checks for a peer coordinator already on the task"
        );
        assert!(
            g.contains("STOP"),
            "instructs the model to STOP when work exists"
        );
        assert!(
            g.contains("already shipped"),
            "reports the already-shipped conclusion with evidence"
        );
        // TASK-408: the guard advertises the codebase-memory `detect_changes`
        // pre-edit call, and is explicit that its absence is a silent no-op.
        assert!(
            g.contains("detect_changes"),
            "guard calls detect_changes before the first edit when available"
        );
        assert!(
            g.contains("skip this step silently"),
            "guard degrades gracefully when codebase-memory is absent"
        );
    }

    #[test]
    fn phase_pipeline_enforces_five_phase_batching_discipline() {
        // TASK-356: the pipeline directive must (a) name all five phases with
        // transition markers, (b) forbid tool calls in the planning phase,
        // (c) mandate batching all reads in Phase 1 before any Phase 3 write,
        // (d) tell the model to TRUST writes (no read-back), and (e) frame the
        // 87→~15 call reduction so the intent is unambiguous.
        let p = PHASE_PIPELINE;
        // Phase transition markers present for every phase (turn logging keys on these).
        for marker in [
            "--- PHASE 1: DISCOVERY ---",
            "--- PHASE 2: PLANNING ---",
            "--- PHASE 3: ACTIONS ---",
            "--- PHASE 4: VALIDATION ---",
        ] {
            assert!(p.contains(marker), "missing phase marker: {marker}");
        }
        // Planning phase forbids ANY tool call (pure reasoning).
        assert!(
            p.contains("REASONING ONLY") && p.contains("FORBIDDEN"),
            "Phase 2 must forbid tool calls"
        );
        // Reads batched in Phase 1 before writes; writes batched in Phase 3.
        assert!(
            p.contains("ALL PARALLEL"),
            "phases mandate parallel batching"
        );
        assert!(
            p.contains("Front-load"),
            "Phase 1 front-loads every context read in one batch"
        );
        // Trust writes — no read-back verification in the action phase.
        assert!(
            p.contains("TRUST your writes") && p.contains("do NOT read a file back"),
            "Phase 3 must trust writes without read-back verification"
        );
        // The call-reduction target is stated (87 → ~15–20).
        assert!(
            p.contains("87") && p.contains("15"),
            "states the 87→~15 reduction target"
        );
    }

    #[test]
    fn phase1_closes_on_a_delta_and_phase2_emits_a_plan_graph() {
        // TASK-806: Phase 1 must EXIT by stating the delta (`ask - state`),
        // including what is explicitly out of scope — the artifact Phase 2
        // plans against. PHASE0_GUARD only answers "already shipped?".
        // TASK-805: Phase 2 must emit a PLAN GRAPH (nodes with depends_on), not
        // the bare file list it used to stop at — a file list is exactly where
        // dependency information was being discarded.
        let p = PHASE_PIPELINE;
        assert!(
            p.contains("DELTA") && p.contains("ask - state"),
            "Phase 1 must close by stating the delta"
        );
        assert!(
            p.contains("OUT OF SCOPE"),
            "the delta must name what is out of scope"
        );
        assert!(
            p.contains("what exactly is left?"),
            "delta step must distinguish itself from the Phase-0 'already shipped?' question"
        );
        // The delta wording maps onto DeltaArtifact { ask_summary,
        // state_summary, delta_items, out_of_scope } (TASK-801) so TASK-803 can
        // persist exactly what the prompt asks for.
        assert!(p.contains("PLAN GRAPH"), "Phase 2 must emit a plan graph");
        for field in [
            "id (lowercase-kebab slug)",
            "intent",
            "files[]",
            "depends_on[]",
            "acceptance[]",
        ] {
            assert!(p.contains(field), "plan-graph node missing field: {field}");
        }
        assert!(
            p.contains("NOT independent"),
            "same-file nodes must be merged or chained, never parallel"
        );
        // The old file-list-only wording is GONE — that was the exact point
        // where dependency information got discarded.
        assert!(
            !p.contains("exact list of files to touch"),
            "Phase 2 must no longer stop at a bare file list"
        );
        // ...but the zero-tool-call hard rule survives the rewrite.
        assert!(
            p.contains("(0 calls, REASONING ONLY)") && p.contains("FORBIDDEN from making ANY"),
            "Phase 2 must still make ZERO tool calls"
        );
    }

    #[test]
    fn fan_out_is_derived_from_the_ready_set_not_discretionary() {
        // TASK-807: the old "RE-EVALUATE THE PLAN AFTER TRIAGE — don't
        // over-decompose" directive was a DISCRETIONARY suppression of the
        // symptom (an 87-call runaway from unstructured fan-out). It is
        // replaced by a STRUCTURAL guard derived from the Phase-2 plan graph,
        // stating the SAME condition as PlanGraph::fan_out_candidates
        // (TASK-802): >=2 ready nodes AND pairwise-disjoint file sets.
        let f = FAN_OUT_DERIVED;
        assert!(
            f.contains("FAN-OUT IS DERIVED, NOT DISCRETIONARY"),
            "fan-out rule must be stated as derived, not a judgement call"
        );
        assert!(
            f.contains("`depends_on` are ALL"),
            "ready-set is defined by satisfied dependencies"
        );
        assert!(
            f.contains("`ready.len() >= 2`") && f.contains("pairwise disjoint"),
            "must state the >=2-ready AND pairwise-disjoint-files condition"
        );
        assert!(
            f.contains("Otherwise execute solo"),
            "the else branch is solo execution, not a guess"
        );
        assert!(
            f.contains("NEVER dispatch a node with an unmet"),
            "unmet dependencies must never be dispatched"
        );
        // The motivating incident stays on the record.
        assert!(
            f.contains("87-call runaway"),
            "the 87-call runaway must remain documented as the motivating incident"
        );
        // The discretionary directive itself is retired (the phrase survives
        // ONLY as the quoted historical reference in the parenthetical).
        assert!(
            !f.contains("RE-EVALUATE THE PLAN AFTER TRIAGE"),
            "the discretionary anti-decompose directive must be retired"
        );
        // Peer coordination survives the rewrite.
        assert!(
            f.contains("`tell`"),
            "collapsed fan-out must still narrow or cancel peers in flight"
        );
    }

    #[test]
    fn fan_out_prompt_rule_and_plan_graph_predicate_cannot_drift() {
        // TASK-809, the assertion no other card makes: FAN_OUT_DERIVED is the
        // PROSE of a rule whose EXECUTABLE form is PlanGraph::fan_out_candidates
        // (TASK-802). A prompt and a predicate that disagree is the worst
        // failure mode available here — the model would be instructed to fan out
        // where the code refuses to (or vice versa), silently, with no test red.
        // So pin the decision table from the engineering spec against BOTH
        // halves: the prompt text AND the real code path, same fixtures.
        use crate::plan::{PlanGraph, PlanNode};

        fn node(id: &str, deps: &[&str], files: &[&str]) -> PlanNode {
            PlanNode {
                id: id.into(),
                intent: format!("do {id}"),
                files: files.iter().map(|s| (*s).to_string()).collect(),
                depends_on: deps.iter().map(|s| (*s).to_string()).collect(),
                acceptance: vec!["lands".into()],
                est_calls: 2,
            }
        }
        fn graph(nodes: Vec<PlanNode>) -> PlanGraph {
            PlanGraph {
                scope_key: "goal:prompt-parity".into(),
                nodes,
                created_at: 1_762_000_000,
            }
        }
        let empty = std::collections::HashSet::new();

        // Row 1 — ONE ready node => solo. The prompt says "Otherwise execute
        // solo"; the predicate returns None.
        let one_ready = graph(vec![
            node("a", &[], &["src/a.rs"]),
            node("b", &["a"], &["src/b.rs"]),
        ]);
        assert!(
            one_ready.fan_out_candidates(&empty).is_none(),
            "ready.len() == 1 must be solo in CODE, as the prompt states"
        );

        // Row 2 — >=2 ready AND pairwise-disjoint files => fan out, one worker
        // per ready node.
        let disjoint = graph(vec![
            node("a", &[], &["src/a.rs"]),
            node("b", &[], &["src/b.rs"]),
        ]);
        let fan = disjoint
            .fan_out_candidates(&empty)
            .expect(">=2 ready + disjoint files must fan out in CODE, as the prompt states");
        assert_eq!(fan.len(), 2, "one worker per ready node");

        // Row 3 — >=2 ready but OVERLAPPING files => solo. This is the 87-call
        // runaway's actual shape: parallel-looking work that contends.
        let overlapping = graph(vec![
            node("a", &[], &["src/shared.rs", "src/a.rs"]),
            node("b", &[], &["src/shared.rs"]),
        ]);
        assert_eq!(
            overlapping.ready_set(&empty).len(),
            2,
            "both nodes ARE ready — the collapse is about files, not arity"
        );
        assert!(
            overlapping.fan_out_candidates(&empty).is_none(),
            "overlapping file sets must collapse to SOLO in CODE, as the prompt states"
        );

        // Row 4 — a node with an unmet dependency is never a candidate, which is
        // the prompt's "NEVER dispatch a node with an unmet dependency".
        let chained = graph(vec![
            node("a", &[], &["src/a.rs"]),
            node("b", &["a"], &["src/b.rs"]),
            node("c", &["a"], &["src/c.rs"]),
        ]);
        let ready: Vec<_> = chained
            .ready_set(&empty)
            .iter()
            .map(|n| n.id.clone())
            .collect();
        assert_eq!(
            ready,
            vec!["a"],
            "unmet deps keep b and c out of the ready set"
        );

        // And the prompt states each row of that table in words, so neither half
        // can be edited into disagreement without a test going red.
        let f = FAN_OUT_DERIVED;
        for clause in [
            "`ready.len() >= 2`",
            "pairwise disjoint",
            "Otherwise execute solo",
            "NEVER dispatch a node with an unmet",
        ] {
            assert!(
                f.contains(clause),
                "FAN_OUT_DERIVED lost the clause `{clause}` that the code enforces"
            );
        }
        // The pipeline that PRODUCES the graph must still demand the fields the
        // predicate reads — a plan without `depends_on`/`files` makes the rule
        // undecidable at runtime.
        for field in ["depends_on", "files"] {
            assert!(
                PHASE_PIPELINE.contains(field),
                "Phase 2 must emit `{field}` — fan_out_candidates reads it"
            );
        }
    }

    #[test]
    fn assembled_coordinator_prompt_carries_both_guard_and_pipeline() {
        // The runtime prompt template (see `drive`) prepends BOTH the Phase-0
        // guard and the 5-phase pipeline just before the TASK. Guard against a
        // future edit dropping either directive from the wire.
        let template = format!("{PHASE0_GUARD}\n\n{PHASE_PIPELINE}");
        assert!(
            template.contains("PHASE-0 GUARD"),
            "guard survives in the template"
        );
        assert!(
            template.contains("--- PHASE 2: PLANNING ---"),
            "pipeline survives in the template"
        );
        // TASK-807: FAN_OUT_DERIVED rides the per-turn directive block, NOT
        // this preamble — it is covered by
        // `fan_out_is_derived_from_the_ready_set_not_discretionary` instead of
        // being asserted here, so this test keeps asserting only what the real
        // assembly actually prepends.
    }

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
        assert_eq!(
            fold_operator_messages(Some(&store), &["run_x"], &mut input),
            0
        );
        assert_eq!(input, "continue the task");

        // One message → folded, prepended ahead of the existing input, drained.
        store
            .enqueue_message("run_x", "use the staging DB", None)
            .unwrap();
        let mut input = "continue the task".to_string();
        let n = fold_operator_messages(Some(&store), &["run_x"], &mut input);
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
            fold_operator_messages(Some(&store), &["run_x"], &mut input2),
            0
        );
        assert_eq!(input2, "next");

        // No store → no-op.
        let mut input3 = "x".to_string();
        assert_eq!(fold_operator_messages(None, &["run_x"], &mut input3), 0);

        let _ = std::fs::remove_file(&path);
    }

    /// Resume-steer defect: a resumed run answers to TWO ids — its fresh
    /// `run_id` and the stable `w_…` job id the operator actually `:tell`s.
    /// Folding must drain BOTH mailboxes, or interjections queued against the
    /// visible id pile up unread.
    #[test]
    fn fold_operator_messages_drains_every_mailbox_id() {
        let path = std::env::temp_dir().join(format!("aish_fold2_{}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let store = CoordinatorStore::open(&path).unwrap();
        store
            .insert("run_thread2", "do the thing", "sess", None)
            .unwrap();

        // Operator queued against the STABLE visible id (what `:tell` does);
        // the fresh thread id has its own note too.
        store
            .enqueue_message("w_stable", "ah nm, secrets.env", None)
            .unwrap();
        store
            .enqueue_message("run_thread2", "and rerun the tests", None)
            .unwrap();

        let mut input = "continue".to_string();
        let n = fold_operator_messages(Some(&store), &["run_thread2", "w_stable"], &mut input);
        assert_eq!(n, 2, "both mailboxes drained");
        assert!(input.contains("ah nm, secrets.env"));
        assert!(input.contains("and rerun the tests"));
        // Drained: a second fold sees nothing in EITHER mailbox.
        let mut again = "next".to_string();
        assert_eq!(
            fold_operator_messages(Some(&store), &["run_thread2", "w_stable"], &mut again),
            0
        );

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
    fn is_salvageable_only_when_work_exists_and_nothing_owns_the_tree() {
        // Work-bearing worktree, no store row AND no ledger row → genuinely
        // orphaned, salvage it. This is the case the backstop exists for.
        assert!(is_salvageable(false, false, true));
        // A surviving row (terminal or live) means the lifecycle owns it — skip.
        assert!(!is_salvageable(true, false, true));
        // No row but a LEDGER row: a real run created this tree and its row was
        // simply trimmed by retention. A cleanup concern, never a failure.
        assert!(!is_salvageable(false, true, true));
        assert!(!is_salvageable(true, true, true));
        // No work in the leaf → nothing to recover, regardless of row state.
        assert!(!is_salvageable(false, false, false));
        assert!(!is_salvageable(true, false, false));
        assert!(!is_salvageable(false, true, false));
    }

    /// The salvage backstop must fire for GENUINELY lost work and for nothing
    /// else. Before this gate, `coordinator_runs` was dominated by phantom
    /// `failed` rows: the run row IS written at spawn, but bounded retention
    /// trims terminal rows, so a finished run's leftover dirty tree looked
    /// row-less on the next startup and was re-minted as a failure (with
    /// `turns=0`, poisoning aggregate stats) on every boot thereafter.
    #[test]
    fn salvage_recovers_only_trees_no_run_ever_owned() {
        let path =
            std::env::temp_dir().join(format!("aish_salvage_gate_{}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let store = CoordinatorStore::open(&path).unwrap();
        let ow = |id: &str| crate::worker::OrphanWork {
            id: id.to_string(),
            branch: format!("aish/{id}"),
            path: std::path::PathBuf::from(format!("/tmp/{id}")),
        };
        let ledger_row = |id: &str| {
            store
                .record_worktree_created(id, std::path::Path::new("/tmp/x"), id)
                .unwrap();
        };

        // (a) Completed run, row still present.
        store.insert("w_done", "shipped it", "sess", None).unwrap();
        store.set_done("w_done", "ok").unwrap();
        ledger_row("w_done");
        // (b) Completed run whose row was TRIMMED by retention — only the
        // never-reaped ledger row survives. The phantom-failure case.
        ledger_row("w_reaped");
        // (c) In-flight run (`coordinating`) and (d) a genuine failure: both have
        // live rows, so the normal lifecycle owns them.
        store.insert("w_live", "in flight", "sess", None).unwrap();
        ledger_row("w_live");
        store.insert("w_failed", "it broke", "sess", None).unwrap();
        store.set_failed("w_failed", "boom").unwrap();
        ledger_row("w_failed");
        // (e) Genuinely orphaned: no row, no ledger entry, work on disk.

        let snapshot = |s: &CoordinatorStore| -> (HashSet<String>, HashSet<String>) {
            let known = s
                .load_all()
                .map(|rows| rows.into_iter().map(|r| r.run_id).collect())
                .unwrap_or_default();
            (known, s.ledger_worktree_ids().unwrap())
        };
        let candidates = || {
            vec![
                ow("w_done"),
                ow("w_reaped"),
                ow("w_live"),
                ow("w_failed"),
                ow("w_orphan"),
            ]
        };

        let (known, ledger) = snapshot(&store);
        let n = salvage_work_bearing(candidates(), &store, &known, &ledger, false);
        assert_eq!(n, 1, "only the truly unowned tree is salvaged");

        let rows = store.load_all().unwrap();
        let salvaged: Vec<_> = rows
            .iter()
            .filter(|r| r.kind.as_deref() == Some(crate::coordinator_store::SALVAGE_KIND))
            .map(|r| (r.run_id.as_str(), r.phase.as_str()))
            .collect();
        assert_eq!(
            salvaged,
            vec![("w_orphan", "failed")],
            "a done/reaped/live/failed run's leftover tree is a cleanup concern, not a failure"
        );
        assert_eq!(
            Phase::parse(&rows.iter().find(|r| r.run_id == "w_done").unwrap().phase),
            Phase::Done,
            "the completed run's own row is untouched"
        );

        // Repeat sweeps are idempotent: the salvage row written above now counts
        // as a surviving row, so a second boot neither duplicates nor re-mints.
        let (known, ledger) = snapshot(&store);
        assert_eq!(
            salvage_work_bearing(candidates(), &store, &known, &ledger, false),
            0,
            "a second sweep over the same trees salvages nothing"
        );
        assert_eq!(
            store.load_all().unwrap().len(),
            rows.len(),
            "no duplicate rows across sweeps"
        );
        let _ = std::fs::remove_file(&path);
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

    /// ISS-409757: retention may reap genuine failures but must never reap a
    /// salvage marker — that row is the last pointer to recoverable work.
    #[test]
    fn salvage_rows_are_never_prunable_failures() {
        let salvage = Some(crate::coordinator_store::SALVAGE_KIND);
        assert!(is_prunable_failed("failed", None));
        assert!(!is_prunable_failed("failed", salvage));
        // Non-failed phases are out of scope either way.
        assert!(!is_prunable_failed("done", None));
        assert!(!is_prunable_failed("coordinating", salvage));
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
            parent_run_id: None,
            created_at: None,
            heartbeat_at,
            tokens_in: 0,
            tokens_out: 0,
            turns: 0,
            tool_calls: 0,
            kind: None,
            activity_summary: None,
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
        assert!(is_orphaned_row(
            Some("other"),
            "me",
            &Phase::Coordinating,
            None
        ));

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
        store
            .insert("foreign_fresh", "task", "other", None)
            .unwrap();
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

    // ---- #129: stall reaper (`is_stalled_row`) ----

    const NOW: i64 = 1_700_000_000;

    /// THE REGRESSION. Heartbeats are SQLite `current_timestamp` strings, not
    /// epoch integers: the old reaper did `ts.parse::<u64>().unwrap_or(0)`,
    /// which failed on EVERY row, collapsed the beat to 0, and made every live
    /// coordinator look 50+ years stale — so the first sweep marked healthy
    /// runs `failed` and their workers stalled with no work landed.
    #[test]
    fn stalled_row_does_not_reap_live_run_with_sqlite_timestamp() {
        let hb = unix_to_sqlite(NOW - 60);
        assert!(
            hb.parse::<u64>().is_err(),
            "fixture must be a non-numeric SQLite timestamp, got {hb}"
        );
        assert!(!is_stalled_row("coordinating", Some(&hb), NOW));
        assert!(!is_stalled_row("awaiting_batch", Some(&hb), NOW));
    }

    #[test]
    fn stalled_row_ignores_terminal_and_checkpoint_phases() {
        let ancient = unix_to_sqlite(NOW - 30 * 3_600);
        for phase in ["done", "failed", "checkpoint", "bogus_legacy_value"] {
            assert!(
                !is_stalled_row(phase, Some(&ancient), NOW),
                "phase `{phase}` must never be reaped as stalled"
            );
        }
    }

    #[test]
    fn stalled_row_reaps_beyond_threshold() {
        let stale = unix_to_sqlite(NOW - (STALL_AFTER.as_secs() as i64 + 60));
        assert!(is_stalled_row("coordinating", Some(&stale), NOW));
        assert!(is_stalled_row("awaiting_batch", Some(&stale), NOW));
    }

    #[test]
    fn stalled_row_boundary_is_exclusive() {
        let exact = unix_to_sqlite(NOW - STALL_AFTER.as_secs() as i64);
        assert!(!is_stalled_row("coordinating", Some(&exact), NOW));
        let past = unix_to_sqlite(NOW - STALL_AFTER.as_secs() as i64 - 1);
        assert!(is_stalled_row("coordinating", Some(&past), NOW));
    }

    /// Fail-OPEN: reaping is irreversible (marks the run failed, abandons its
    /// work), so an absent/unreadable beat is never grounds to kill. Dead runs
    /// are still caught by the pid-liveness orphan scan, which has evidence.
    #[test]
    fn stalled_row_fails_open_on_unreadable_heartbeat() {
        for hb in [None, Some(""), Some("not-a-timestamp"), Some("0")] {
            assert!(
                !is_stalled_row("coordinating", hb, NOW),
                "unreadable heartbeat {hb:?} must not trigger a reap"
            );
        }
    }

    /// A clock skew that puts the beat in the FUTURE must not underflow into a
    /// huge age and reap the run.
    #[test]
    fn stalled_row_tolerates_future_heartbeat() {
        let future = unix_to_sqlite(NOW + 3_600);
        assert!(!is_stalled_row("coordinating", Some(&future), NOW));
    }

    /// REGRESSION (production): `background_status` listed 47+ goal-loop turns
    /// that ALL reported the literal run id `goal`, so `stop goal` matched every
    /// row and could not name one — a runaway goal loop had no kill switch.
    /// Unique per-turn ids restore it: each id, and any prefix long enough to be
    /// unambiguous, must resolve to exactly ONE run.
    #[test]
    fn goal_run_ids_resolve_uniquely_for_stop() {
        let ids: Vec<String> = (0..50).map(|_| crate::worker::new_goal_id()).collect();
        let refs: Vec<&str> = ids.iter().map(String::as_str).collect();

        // Full id → exactly one run, every time.
        for id in &refs {
            let hits = super::resolve_run_ids(refs.iter().copied(), id);
            assert_eq!(hits, vec![*id], "id {id} must resolve to exactly one run");
        }

        // A prefix is still addressable (operator ergonomics) and, at 8 random
        // base62 chars, unique across a 50-turn loop.
        let probe = refs[7];
        let hits = super::resolve_run_ids(refs.iter().copied(), &probe[..6]);
        assert_eq!(hits, vec![probe], "a 6-char prefix must select one run");

        // And the shape of the OLD bug: a shared, non-unique id fans out.
        let broken = vec!["goal", "goal", "goal"];
        assert_eq!(
            super::resolve_run_ids(broken, "goal").len(),
            3,
            "the pre-fix ids matched every row — that was the defect"
        );
        // The stable stream label must NOT collide with the new run ids, or
        // `stop goal` would again sweep the whole loop.
        assert!(
            super::resolve_run_ids(refs.iter().copied(), crate::worker::GOAL_STREAM_LABEL)
                .is_empty(),
            "the attach label must address no durable run"
        );
    }

    #[test]
    fn id_matches_is_exact_or_prefix() {
        assert!(super::id_matches("g_aB3xK9pQ", "g_aB3xK9pQ"));
        assert!(super::id_matches("g_aB3xK9pQ", "g_aB3"));
        assert!(!super::id_matches("g_aB3xK9pQ", "w_aB3"));
        assert!(!super::id_matches("g_aB3", "g_aB3xK9pQ")); // query longer than id
    }
}
