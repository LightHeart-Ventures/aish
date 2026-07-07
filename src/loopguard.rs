//! Loop-guard, budget, and graceful-degradation primitives for the agentic
//! tool loop — the "don't burn the whole iteration budget spinning" layer.
//!
//! `engine::run_turn` runs a bounded `model ⇄ tools` loop (`MAX_ITERATIONS`).
//! Left unguarded, two failure modes waste that whole budget and then throw the
//! work away with a flat `[stopped: …]` line:
//!   1. **Looping** — the model re-issues the SAME tool call with the SAME args
//!      over and over (a clarify→refine→clarify cycle, or an error-retry streak)
//!      making no progress.
//!   2. **A genuine, undersized budget** — real multi-step work that simply
//!      needs more room, cut off mid-task with nothing to show.
//!
//! This module is the pure, unit-testable core of three mitigations wired into
//! the engine (and reused by the coordinator's worker-exit evaluation):
//!
//! * **Same-call repeat guard** ([`RepeatGuard`] / [`repeat_action`]) — counts
//!   identical `(tool, args)` signatures and, past a threshold, BLOCKS the
//!   re-execution (no duplicate side effect) and finally BREAKS the turn with a
//!   logged [`ExitReason::LoopDetected`]. "Call the same tool with the same args
//!   twice → stop and re-plan", enforced rather than merely suggested.
//!
//! * **Soft-warning → forced-summarize before the hard limit** ([`budget_phase`]
//!   / [`budget_suffix`]) — at [`SOFT_WARN_PCT`] of the budget the model is told
//!   to converge; at [`FORCE_SUMMARIZE_PCT`] it is handed NO tools and a
//!   summarize directive, so it MUST emit a best-effort partial answer instead
//!   of being killed at the hard cap with empty hands.
//!
//! * **Structured exit reasons** ([`ExitReason`]) — every abnormal stop is
//!   tagged (`loop-detected` / `forced-summarize` / `budget-exhausted`) and
//!   carried back on the FIRST line of the answer as a greppable
//!   [`ExitReason::banner`], so the reason survives even the worker subprocess
//!   stdout boundary. The coordinator parses that banner to decide a recovery
//!   [`Disposition`]: auto-RESUME from where it left off, NUDGE the model to
//!   change approach, or FLAG it for the operator.

use std::collections::HashMap;

/// Fraction of the iteration budget after which the model is nudged to converge
/// (a soft warning folded into the system prompt — see [`budget_suffix`]).
pub const SOFT_WARN_PCT: usize = 75;

/// Fraction of the iteration budget after which the loop FORCES a summarize-exit:
/// the model is handed no tools and must produce its best partial answer. Set
/// below 100 so degradation is graceful — a partial result, not an empty stop.
pub const FORCE_SUMMARIZE_PCT: usize = 90;

/// Nth identical `(tool, args)` call at which the guard stops RE-EXECUTING it
/// (the call is blocked and a corrective result is fed back so the model gets
/// one chance to re-plan). Two identical calls are tolerated; the third is
/// blocked.
pub const REPEAT_SOFT_LIMIT: usize = 3;

/// Nth identical call at which the turn is BROKEN with `LoopDetected` — the
/// model kept repeating even after being blocked, so it is confirmed looping.
pub const REPEAT_HARD_LIMIT: usize = 4;

/// How many automatic recoveries (resume/nudge) the coordinator will attempt for
/// a worker that keeps ending abnormally before it gives up and flags the
/// operator. Keeps an auto-recovery from becoming its own infinite loop.
pub const MAX_AUTO_RECOVERIES: usize = 2;

// ---------------------------------------------------------------------------
// Iteration budget — soft-warning → forced-summarize before the hard limit
// ---------------------------------------------------------------------------

/// Where the current iteration sits relative to the budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BudgetPhase {
    /// Plenty of budget left — run normally.
    Normal,
    /// Past [`SOFT_WARN_PCT`] — fold a "start wrapping up" notice into the prompt.
    SoftWarn,
    /// Past [`FORCE_SUMMARIZE_PCT`] — hand the model no tools and force a
    /// best-effort final answer.
    ForceSummarize,
}

/// Classify a 1-based iteration index against a budget. The last iteration
/// (`iteration == max`) always lands in `ForceSummarize`, so the loop never
/// reaches the hard cap with no answer produced. A zero budget is degenerate and
/// forces immediately.
pub fn budget_phase(iteration: usize, max: usize) -> BudgetPhase {
    if max == 0 {
        return BudgetPhase::ForceSummarize;
    }
    // Percent-of-budget consumed by (and including) this iteration.
    let pct = iteration.saturating_mul(100) / max;
    if pct >= FORCE_SUMMARIZE_PCT {
        BudgetPhase::ForceSummarize
    } else if pct >= SOFT_WARN_PCT {
        BudgetPhase::SoftWarn
    } else {
        BudgetPhase::Normal
    }
}

/// The system-prompt suffix for a non-`Normal` budget phase. Appended to the
/// (otherwise byte-stable, prompt-cache-friendly) base system prompt only while
/// converging or forcing, so the model SEES the budget pressure. `remaining` is
/// the number of tool-call rounds left; it's surfaced in the soft warning.
pub fn budget_suffix(phase: BudgetPhase, remaining: usize) -> String {
    match phase {
        BudgetPhase::Normal => String::new(),
        BudgetPhase::SoftWarn => format!(
            "\n\n[BUDGET — converge now: you have about {remaining} tool-call round(s) left this \
turn. Finish the highest-value step and prepare to summarize. Do NOT start new exploration or \
repeat calls you've already made; if you're blocked, say so plainly and give your best partial \
result.]"
        ),
        BudgetPhase::ForceSummarize => String::from(
            "\n\n[BUDGET EXHAUSTED — this is your FINAL step and you have NO tools available now. \
Do not attempt any tool call. Write your best final answer from what you already have: what you \
accomplished, what still remains, and any concrete blocker. If the work is incomplete, say so \
clearly and hand back the partial result — a clearly-labelled partial answer is far better than \
nothing.]",
        ),
    }
}

// ---------------------------------------------------------------------------
// Same-call repeat guard
// ---------------------------------------------------------------------------

/// What the engine should do with a tool call given how many times its exact
/// `(tool, args)` signature has now been seen this turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepeatAction {
    /// Run the call normally.
    Allow,
    /// Do NOT re-execute (avoid the duplicate side effect); feed a corrective
    /// result back so the model can re-plan.
    Block,
    /// Confirmed loop — block AND break the turn with `LoopDetected`.
    Break,
}

/// Map an occurrence count (number of times this signature has been seen,
/// INCLUDING the current call) to an action. Pure, so the thresholds are tested
/// in isolation.
pub fn repeat_action(count: usize) -> RepeatAction {
    if count >= REPEAT_HARD_LIMIT {
        RepeatAction::Break
    } else if count >= REPEAT_SOFT_LIMIT {
        RepeatAction::Block
    } else {
        RepeatAction::Allow
    }
}

/// Per-turn tally of identical tool-call signatures. Lives for the duration of
/// one `run_turn` and is dropped with it, so counts never bleed across turns.
#[derive(Default)]
pub struct RepeatGuard {
    seen: HashMap<u64, usize>,
}

impl RepeatGuard {
    /// Record one tool call and return its new occurrence count (1 on first
    /// sight). The signature is a stable hash of the tool name plus its
    /// canonicalised arguments, so reordered JSON keys still collide.
    pub fn record(&mut self, name: &str, args: &serde_json::Value) -> usize {
        let sig = signature(name, args);
        let c = self.seen.entry(sig).or_insert(0);
        *c += 1;
        *c
    }
}

/// Stable 64-bit signature of a `(tool, args)` pair: FNV-1a over the tool name,
/// a separator, and a key-sorted canonical JSON rendering of the args. Sorting
/// keys means two semantically-identical argument objects that differ only in
/// key order produce the same signature (the model rarely reorders, but the
/// guard must not be fooled when it does).
pub fn signature(name: &str, args: &serde_json::Value) -> u64 {
    let mut canon = String::with_capacity(64);
    canon.push_str(name);
    canon.push('\u{0}');
    canonicalize(args, &mut canon);
    fnv1a(canon.as_bytes())
}

/// Append a deterministic, key-sorted rendering of `v` to `out`. Not valid JSON
/// (we don't need to re-parse it) — just a canonical string where object key
/// order can't change the result.
fn canonicalize(v: &serde_json::Value, out: &mut String) {
    use serde_json::Value;
    match v {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            out.push('{');
            for k in keys {
                out.push_str(k);
                out.push(':');
                canonicalize(&map[k], out);
                out.push(',');
            }
            out.push('}');
        }
        Value::Array(items) => {
            out.push('[');
            for i in items {
                canonicalize(i, out);
                out.push(',');
            }
            out.push(']');
        }
        Value::String(s) => {
            out.push('"');
            out.push_str(s);
            out.push('"');
        }
        other => out.push_str(&other.to_string()),
    }
}

/// FNV-1a 64-bit — a tiny, dependency-free hash. Signatures only need to be
/// collision-resistant enough to tell "the same call" from "a different call"
/// within one turn, not be cryptographic.
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf29ce484222325;
    for b in bytes {
        hash ^= *b as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

/// The corrective result body fed back to the model when a repeated call is
/// blocked. Tells it the call was NOT executed and that it must change course.
pub fn blocked_result_text(desc: &str, count: usize) -> String {
    format!(
        "[loop-guard] This identical call has now been requested {count} times this turn with no \
new information and was NOT executed again (to avoid a duplicate side effect): {desc}\nStop \
repeating it. Either take a genuinely different action, or — if you are blocked — state the \
specific blocker plainly and give your best partial answer."
    )
}

/// A dim, one-line stderr log for a blocked/broken repeated call.
pub fn repeat_log_line(desc: &str, count: usize, action: RepeatAction) -> String {
    match action {
        RepeatAction::Break => {
            format!("loop-guard: repeated call ×{count} — breaking turn (re-plan failed): {desc}")
        }
        _ => format!(
            "loop-guard: blocked repeated call ×{count} — asking the model to re-plan: {desc}"
        ),
    }
}

// ---------------------------------------------------------------------------
// Structured exit reasons + cross-process banner
// ---------------------------------------------------------------------------

/// Why an agentic turn ended. `Completed` is the normal path; the rest are the
/// graceful-degradation / loop-guard stops, each tagged so logs are greppable
/// and the coordinator can pick a recovery [`Disposition`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExitReason {
    /// The model produced a final answer on its own. The engine returns the raw
    /// answer (no banner) in this case, so the variant is constructed only in the
    /// disposition-routing tests / as the "normal" sentinel — hence the allow.
    #[allow(dead_code)]
    Completed,
    /// The forced-summarize step fired: a partial answer was produced before the
    /// hard cap. `iterations` is how many rounds ran.
    ForcedSummarize { iterations: usize },
    /// The same `(tool, args)` call was repeated past the hard limit.
    LoopDetected { call: String, count: usize },
    /// The loop ran out of budget without an answer (a backstop — normally the
    /// forced-summarize step fires first).
    BudgetExhausted { iterations: usize },
    /// The operator interrupted the turn — Ctrl-C forwarded (as SIGINT) to a
    /// background coordinator the interactive session is `:attach`ed to. This is
    /// NOT a failure: the coordinator stays alive, folds a reassess directive,
    /// and drives the next round. Carries no counters.
    Interrupted,
    /// TASK-358: the turn ran a deep SERIAL chain — more than
    /// [`SERIAL_CHAIN_YIELD_DEPTH`] consecutive rounds that each issued exactly
    /// ONE tool call (grep→read→edit→run→…), never batching. This is not a loop
    /// (the calls differ) and not a budget stop, but a token-wasteful,
    /// rate-limit-concentrating shape: every round re-sends the whole context.
    /// The coordinator yields with a resumable banner so the durable loop
    /// checkpoints and the next round re-plans toward batching. `depth` is the
    /// streak length reached. NOT a failure — auto-resumes.
    SerialChainYield { depth: usize },
    /// TASK-357: the turn hit the per-turn tool-call HARD budget
    /// ([`CALL_BUDGET_HARD`]). Distinct from the round/iteration budget
    /// (`BudgetExhausted`, which counts model rounds) and the serial-chain shape
    /// guard: this counts the CUMULATIVE number of individual tool calls executed
    /// across the whole turn and yields once it crosses the hard cap — spreading
    /// load across the rate-limit window and creating a natural checkpoint. A
    /// soft advisory fires earlier at [`CALL_BUDGET_SOFT`] (logged, no stop).
    /// `count` is the cumulative call tally reached. NOT a failure — auto-resumes.
    CallBudgetExceeded { count: usize },
}

impl ExitReason {
    /// The short, stable tag used in logs, banners, and disposition routing.
    pub fn tag(&self) -> &'static str {
        match self {
            ExitReason::Completed => "completed",
            ExitReason::ForcedSummarize { .. } => "forced-summarize",
            ExitReason::LoopDetected { .. } => "loop-detected",
            ExitReason::BudgetExhausted { .. } => "budget-exhausted",
            ExitReason::Interrupted => "interrupted",
            ExitReason::SerialChainYield { .. } => "serial-chain-yield",
            ExitReason::CallBudgetExceeded { .. } => "call-budget-exceeded",
        }
    }

    /// True for every non-`Completed` reason (i.e. an abnormal stop worth acting
    /// on).
    pub fn is_abnormal(&self) -> bool {
        !matches!(self, ExitReason::Completed)
    }

    /// A human sentence describing the stop, for operator-facing messages.
    pub fn detail(&self) -> String {
        match self {
            ExitReason::Completed => "completed normally".to_string(),
            ExitReason::ForcedSummarize { iterations } => {
                format!(
                    "forced a summarize-exit after {iterations} tool-call round(s) to avoid losing the work"
                )
            }
            ExitReason::LoopDetected { call, count } => {
                format!(
                    "detected a loop — the call `{call}` was repeated {count} times without progress"
                )
            }
            ExitReason::BudgetExhausted { iterations } => {
                format!("exhausted the {iterations}-round tool-call budget without a final answer")
            }
            ExitReason::Interrupted => "was interrupted by the operator (Ctrl-C)".to_string(),
            ExitReason::SerialChainYield { depth } => {
                format!(
                    "yielded after {depth} consecutive single-call rounds (a deep serial chain) to \
re-plan toward batching independent calls"
                )
            }
            ExitReason::CallBudgetExceeded { count } => {
                format!(
                    "yielded after {count} tool calls this turn (per-turn hard budget) to spread \
load across the rate-limit window and resume with fresh context"
                )
            }
        }
    }

    /// A single dim-able stderr log line for this stop.
    pub fn log_line(&self) -> String {
        format!("turn stopped [{}]: {}", self.tag(), self.detail())
    }

    /// A greppable, machine-parseable banner line prepended to an abnormal
    /// answer so the stop reason survives the worker→parent stdout boundary (the
    /// coordinator persists the whole answer; the parent captures it). `count`
    /// is `0` when not meaningful. Round-trips through [`ExitReason::parse_banner`].
    pub fn banner(&self) -> String {
        let (iters, count) = match self {
            ExitReason::Completed => (0, 0),
            ExitReason::ForcedSummarize { iterations } => (*iterations, 0),
            ExitReason::BudgetExhausted { iterations } => (*iterations, 0),
            ExitReason::Interrupted => (0, 0),
            ExitReason::LoopDetected { count, .. } => (0, *count),
            // Carry `depth` in the `count` field of the banner (round-tripped
            // back into `depth` by parse_banner).
            ExitReason::SerialChainYield { depth } => (0, *depth),
            // Carry the cumulative call tally in the banner `count` field.
            ExitReason::CallBudgetExceeded { count } => (0, *count),
        };
        format!(
            "[aish-stop tag={} iterations={iters} count={count}] {}",
            self.tag(),
            self.detail()
        )
    }

    /// Parse the leading [`ExitReason::banner`] line out of a worker's answer
    /// text, when present. Returns `None` for a normal (un-bannered) answer.
    /// Forgiving: an unrecognised tag or a missing field yields `None` rather
    /// than an error, so a stray bracketed line can never be misread as a stop.
    pub fn parse_banner(text: &str) -> Option<ExitReason> {
        let line = text.lines().next()?.trim();
        let inner = line.strip_prefix("[aish-stop ")?.split(']').next()?;
        let mut tag = "";
        let mut iters = 0usize;
        let mut count = 0usize;
        for field in inner.split_whitespace() {
            if let Some(v) = field.strip_prefix("tag=") {
                tag = v;
            } else if let Some(v) = field.strip_prefix("iterations=") {
                iters = v.parse().unwrap_or(0);
            } else if let Some(v) = field.strip_prefix("count=") {
                count = v.parse().unwrap_or(0);
            }
        }
        match tag {
            "forced-summarize" => Some(ExitReason::ForcedSummarize { iterations: iters }),
            "budget-exhausted" => Some(ExitReason::BudgetExhausted { iterations: iters }),
            "interrupted" => Some(ExitReason::Interrupted),
            "serial-chain-yield" => Some(ExitReason::SerialChainYield { depth: count }),
            "call-budget-exceeded" => Some(ExitReason::CallBudgetExceeded { count }),
            "loop-detected" => Some(ExitReason::LoopDetected {
                call: String::new(),
                count,
            }),
            _ => None,
        }
    }
}

/// Prepend the reason's banner to a partial answer (one banner line, a blank
/// line, then the body), so an abnormal stop is both human-visible and
/// machine-parseable. A normal `Completed` answer is returned unchanged.
pub fn with_banner(reason: &ExitReason, body: &str) -> String {
    if !reason.is_abnormal() {
        return body.to_string();
    }
    let body = body.trim();
    if body.is_empty() {
        reason.banner()
    } else {
        format!("{}\n\n{body}", reason.banner())
    }
}

// ---------------------------------------------------------------------------
// Worker-exit evaluation — auto-resume / nudge / flag-for-operator
// ---------------------------------------------------------------------------

/// What the coordinator should do after a worker (one agentic round) ends with a
/// given [`ExitReason`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Disposition {
    /// Nothing to do — the turn completed normally.
    None,
    /// Auto-restart from where the previous worker ended: the work so far is
    /// preserved (history / turn-audit replay), so drive another round with a
    /// "continue, don't redo" directive. The fit for an out-of-budget stop.
    Resume,
    /// Keep the same worker going but steer it: it was looping, so feed a
    /// change-approach nudge rather than blindly resuming the same path.
    Nudge,
    /// Stop and surface the failure for a human — auto-recovery is exhausted or
    /// the failure isn't one the coordinator should silently paper over.
    FlagOperator,
}

impl Disposition {
    /// A present-tense verb for status lines (`"resuming"`, `"nudging"`, …).
    pub fn verb(self) -> &'static str {
        match self {
            Disposition::None => "continuing",
            Disposition::Resume => "auto-resuming",
            Disposition::Nudge => "nudging",
            Disposition::FlagOperator => "flagging for operator",
        }
    }
}

/// Decide how to handle a worker that ended with `reason`, given how many
/// auto-recoveries have already been spent. Policy:
///   * a normal completion → `None`;
///   * once `auto_recoveries >= max_auto`, ANY abnormal stop → `FlagOperator`
///     (don't auto-recover forever);
///   * a confirmed loop → `Nudge` (change approach, don't just resume the loop);
///   * an out-of-budget / forced-summarize stop → `Resume` (continue the work).
/// Pure — the whole routing table is unit-tested.
pub fn classify_disposition(
    reason: &ExitReason,
    auto_recoveries: usize,
    max_auto: usize,
) -> Disposition {
    match reason {
        ExitReason::Completed | ExitReason::Interrupted => Disposition::None,
        _ if auto_recoveries >= max_auto => Disposition::FlagOperator,
        ExitReason::LoopDetected { .. } => Disposition::Nudge,
        ExitReason::ForcedSummarize { .. }
        | ExitReason::BudgetExhausted { .. }
        | ExitReason::SerialChainYield { .. }
        | ExitReason::CallBudgetExceeded { .. } => Disposition::Resume,
    }
}

/// Build the directive fed into the next round for a recovery disposition.
/// `Resume` → "continue from where you left off, don't redo completed work".
/// `Nudge` → "you were looping on X; change approach or declare a blocker".
/// Non-recovery dispositions have no directive (`None`).
pub fn recovery_directive(disp: Disposition, reason: &ExitReason) -> Option<String> {
    match disp {
        Disposition::Resume => Some(format!(
            "[auto-resume] Your previous turn stopped before finishing ({}). The work you already \
completed is preserved — do NOT redo it. Pick up exactly where you left off, do only the \
remaining steps, and drive the task to completion. If you are genuinely blocked, say so plainly \
and give your best partial result.",
            reason.detail()
        )),
        Disposition::Nudge => {
            let what = match reason {
                ExitReason::LoopDetected { call, .. } if !call.is_empty() => {
                    format!("you kept repeating the same action ({call})")
                }
                _ => "you were repeating the same action without making progress".to_string(),
            };
            Some(format!(
                "[nudge] Your previous turn was stopped by the loop guard: {what}. Do NOT repeat \
that call. Re-read the task, then take a materially different approach — a different command, a \
different file, or a different strategy. If no approach can work, state the specific blocker and \
hand back your best partial result instead of retrying.",
            ))
        }
        Disposition::None | Disposition::FlagOperator => None,
    }
}

/// The evaluated outcome of a single coordinator round: WHY the worker's turn
/// ended ([`ExitReason`]), WHAT the coordinator should do about it
/// ([`Disposition`]), and HOW MANY auto-recoveries had already been spent this
/// run when the disposition was chosen. Merging the two tightly-coupled reads —
/// the reason and the action — into one value lets the drive loop decide both in
/// a single `match`, so they can no longer drift apart across separate call
/// sites. Only produced for an ABNORMAL stop: a normal completion carries no
/// banner and [`RoundExit::evaluate`] yields `None`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoundExit {
    /// Why the round's agentic turn stopped.
    pub reason: ExitReason,
    /// What the coordinator should do about it, per [`classify_disposition`].
    pub disposition: Disposition,
    /// How many auto-recoveries had been spent BEFORE this round — the value the
    /// disposition was classified against.
    pub recovery_count: usize,
}

impl RoundExit {
    /// Evaluate a finished round's `answer`: parse the exit banner and, when the
    /// turn ended abnormally, classify the recovery [`Disposition`] against how
    /// many auto-recoveries have already been spent. Returns `None` for a normal
    /// (un-bannered) answer — the round produced a real result, nothing to
    /// recover. Composes [`ExitReason::parse_banner`] + [`classify_disposition`]
    /// so a caller reads the reason and its disposition as one unit.
    pub fn evaluate(answer: &str, auto_recoveries: usize, max_auto: usize) -> Option<RoundExit> {
        let reason = ExitReason::parse_banner(answer)?;
        let disposition = classify_disposition(&reason, auto_recoveries, max_auto);
        Some(RoundExit {
            reason,
            disposition,
            recovery_count: auto_recoveries,
        })
    }

    /// The directive to fold into the NEXT round for a recovery disposition
    /// (`Resume`/`Nudge`), or `None` for a terminal one. Thin pass-through to
    /// [`recovery_directive`] over the bundled reason + disposition.
    pub fn directive(&self) -> Option<String> {
        recovery_directive(self.disposition, &self.reason)
    }
}

// ---------------------------------------------------------------------------
// Batch-nudge guard — encourage batching independent read-only calls
// ---------------------------------------------------------------------------

/// Consecutive turns that EACH issue exactly one batchable read-only tool call
/// before the model is nudged to batch. Three lone-read turns in a row is the
/// "drip-feeding reads" pattern TASK-324 targets (grep→read→grep→read…), where
/// each extra round needlessly re-sends the whole context.
pub const BATCH_NUDGE_STREAK: usize = 3;

/// Whether a tool is a side-effect-free read whose INDEPENDENT calls should be
/// batched into a single turn (front-loaded context-gathering). Only the pure
/// inspection tools qualify; anything that mutates or runs a program
/// (`run_program`, `edit_file`, `write_file`, …) is excluded because ordering
/// and side effects can matter, so serial calls there aren't a batching failure.
pub fn is_batchable_read(name: &str) -> bool {
    matches!(
        name,
        "read_file" | "grep_files" | "list_dir" | "glob_expand" | "stat_file" | "diff_files"
    )
}

/// Tracks consecutive single-read turns across one `run_turn` and decides when
/// to fold a one-shot "batch your independent reads" nudge into the next round's
/// prompt. Lives for the duration of one turn and is dropped with it, so the
/// streak never bleeds across turns.
#[derive(Default)]
pub struct BatchGuard {
    /// Consecutive turns that each issued exactly one batchable read.
    streak: usize,
    /// True once the current streak has already been nudged, so the model is
    /// nudged AT MOST once per streak (no per-round nagging).
    nudged: bool,
}

impl BatchGuard {
    /// Record one executed turn's tool-call names and return the nudge string
    /// the first time the drip-feed threshold is crossed this streak. A turn
    /// that batches (≥2 calls) or does anything other than a lone batchable read
    /// RESETS the streak and re-arms the nudge for a future streak.
    pub fn record(&mut self, tool_names: &[&str]) -> Option<String> {
        let lone_read = tool_names.len() == 1 && is_batchable_read(tool_names[0]);
        if !lone_read {
            self.streak = 0;
            self.nudged = false;
            return None;
        }
        self.streak += 1;
        if self.streak >= BATCH_NUDGE_STREAK && !self.nudged {
            self.nudged = true;
            return Some(batch_nudge_suffix());
        }
        None
    }
}

/// The one-shot system-prompt suffix that nudges the model to batch independent
/// reads. Appended to the NEXT round's prompt only when [`BatchGuard::record`]
/// trips, then dropped — so the base prompt stays byte-stable for cache reuse
/// (same discipline as [`budget_suffix`]).
pub fn batch_nudge_suffix() -> String {
    String::from(
        "\n\n[BATCH — you've issued several single-tool read turns in a row \
(read_file/grep_files/list_dir/glob_expand/stat_file/diff_files). These independent reads should \
go OUT TOGETHER: fire ALL the reads/greps/lists you already know you need in ONE turn, not one per \
round. Every extra round re-sends the whole context, so front-load your inspection calls now. Only \
serialize a call that genuinely DEPENDS on a previous call's output (e.g. grep to find a line, THEN \
read that exact line).]",
    )
}

// ---------------------------------------------------------------------------
// TASK-358: serial-chain depth yield
// ---------------------------------------------------------------------------

/// Consecutive rounds that EACH issue exactly one tool call (a deep SERIAL
/// chain: grep→read→edit→run→…) before the coordinator gracefully YIELDS to
/// re-plan toward batching. Unlike [`BATCH_NUDGE_STREAK`] this counts ANY lone
/// call (not just batchable reads) and drives a turn-yield rather than a mere
/// prompt nudge: a long serial chain drains the rate-limit window — every round
/// re-sends the whole context — even when the individual calls differ (so it is
/// not a loop) and the budget is not yet spent. Raised to 12 (from the original
/// 8) so genuinely-serial dependent chains — which cannot be batched — run
/// deeper before yielding instead of false-tripping every 8 rounds and draining
/// the coordinator's auto-recovery budget; the 13th consecutive lone call trips.
pub const SERIAL_CHAIN_YIELD_DEPTH: usize = 12;

/// Tracks the current run of consecutive single-tool-call rounds within one
/// `run_turn` and signals when the chain has grown deep enough to yield. Lives
/// for the duration of one turn and is dropped with it, so the streak never
/// bleeds across turns (same discipline as [`BatchGuard`]).
#[derive(Default)]
pub struct SerialChainGuard {
    /// Length of the current uninterrupted run of single-call rounds.
    depth: usize,
    /// Per-turn yield threshold. `None` uses the compile-time
    /// [`SERIAL_CHAIN_YIELD_DEPTH`] default; `Some(n)` is an operator override
    /// (env `AISH_SERIAL_CHAIN_YIELD_DEPTH`, resolved at construction in the
    /// engine) that lets a GENUINELY-serial workload — a long chain of
    /// dependent calls that CANNOT be batched (e.g. checkout→commit→push→PR, or
    /// a repeated grep-then-read-the-same-file) — run deeper before yielding,
    /// instead of false-tripping and eventually exhausting the coordinator's
    /// auto-recovery budget. The env read stays out of this pure core: the
    /// caller resolves the number, this field just carries it so `record`
    /// remains unit-testable without touching process env.
    threshold: Option<usize>,
}

impl SerialChainGuard {
    /// Construct a fresh guard with an explicit yield threshold — the operator
    /// override path. `depth` starts at 0; only the ceiling differs from
    /// [`SerialChainGuard::default`].
    pub fn with_threshold(threshold: usize) -> Self {
        Self {
            depth: 0,
            threshold: Some(threshold),
        }
    }

    /// The effective yield threshold: the override when set, else the
    /// compile-time [`SERIAL_CHAIN_YIELD_DEPTH`] default.
    fn threshold(&self) -> usize {
        self.threshold.unwrap_or(SERIAL_CHAIN_YIELD_DEPTH)
    }

    /// Record one executed round's tool-call count. A round with exactly one
    /// call EXTENDS the serial chain; a batched round (≥2 calls) or a no-call
    /// round RESETS it. Returns `Some(depth)` once the chain first exceeds the
    /// effective [`SerialChainGuard::threshold`] so the caller can yield. Because
    /// a yield ends the turn (dropping this guard), it fires at most once per
    /// chain.
    pub fn record(&mut self, calls_this_round: usize) -> Option<usize> {
        if calls_this_round == 1 {
            self.depth += 1;
            if self.depth > self.threshold() {
                return Some(self.depth);
            }
        } else {
            self.depth = 0;
        }
        None
    }
}

/// TASK-357: per-turn CUMULATIVE tool-call budget. Where [`SerialChainGuard`]
/// watches the *shape* of rounds (consecutive lone calls) and the round/iteration
/// budget counts model *rounds*, this counts the total number of individual tool
/// calls executed across the whole turn — a batched round of 5 advances it by 5.
/// A very wide turn (many big batches) can drain the rate-limit window and grow
/// history without ever tripping the serial-chain or round guards; this cap gives
/// that case a natural checkpoint so the durable coordinator loop can re-plan with
/// fresh context. Soft advisory at [`CALL_BUDGET_SOFT`] (logged only); hard yield
/// once the tally crosses [`CALL_BUDGET_HARD`].
pub const CALL_BUDGET_SOFT: usize = 20;
/// Hard per-turn cumulative tool-call budget — the turn yields (resumably) once
/// the cumulative count crosses this. Per the TASK-357 card: soft @ 20, hard @ 30.
pub const CALL_BUDGET_HARD: usize = 30;

/// What the [`CallBudgetGuard`] wants the caller to do after a round.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallBudgetAction {
    /// Under the soft cap — carry on.
    Continue,
    /// Crossed the soft cap this round (fires once) — log an advisory but keep
    /// going. `count` is the cumulative tally reached.
    SoftWarn { count: usize },
    /// Crossed the hard cap — the caller should finish the round's bookkeeping
    /// then yield with [`ExitReason::CallBudgetExceeded`]. `count` is the tally.
    HardYield { count: usize },
}

/// Accumulates the cumulative count of tool calls executed within one `run_turn`
/// and signals soft/hard budget crossings. Lives for one turn and is dropped with
/// it, so the tally never bleeds across turns.
#[derive(Default)]
pub struct CallBudgetGuard {
    /// Cumulative tool calls executed so far this turn.
    count: usize,
    /// Latches once the soft advisory has fired, so it warns at most once.
    soft_fired: bool,
}

impl CallBudgetGuard {
    /// Record a round that executed `calls_this_round` tool calls, advancing the
    /// cumulative tally. Returns the action the caller should take. Hard takes
    /// precedence over soft; the soft advisory fires at most once per turn.
    pub fn record(&mut self, calls_this_round: usize) -> CallBudgetAction {
        self.count = self.count.saturating_add(calls_this_round);
        if self.count > CALL_BUDGET_HARD {
            return CallBudgetAction::HardYield { count: self.count };
        }
        if self.count > CALL_BUDGET_SOFT && !self.soft_fired {
            self.soft_fired = true;
            return CallBudgetAction::SoftWarn { count: self.count };
        }
        CallBudgetAction::Continue
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ── budget phase ──────────────────────────────────────────────────────
    #[test]
    fn budget_phase_thresholds_with_default_budget() {
        // Mirrors engine::MAX_ITERATIONS = 50.
        assert_eq!(budget_phase(1, 50), BudgetPhase::Normal);
        assert_eq!(budget_phase(37, 50), BudgetPhase::Normal); // 74%
        assert_eq!(budget_phase(38, 50), BudgetPhase::SoftWarn); // 76%
        assert_eq!(budget_phase(44, 50), BudgetPhase::SoftWarn); // 88%
        assert_eq!(budget_phase(45, 50), BudgetPhase::ForceSummarize); // 90%
        assert_eq!(budget_phase(50, 50), BudgetPhase::ForceSummarize); // 100%
    }

    #[test]
    fn budget_phase_last_iteration_always_forces() {
        // Whatever the budget, the final iteration must force a summarize-exit so
        // the loop never reaches the hard cap empty-handed.
        for max in [1usize, 2, 7, 10, 50, 99] {
            assert_eq!(
                budget_phase(max, max),
                BudgetPhase::ForceSummarize,
                "max={max}"
            );
        }
        // A degenerate zero budget forces immediately.
        assert_eq!(budget_phase(1, 0), BudgetPhase::ForceSummarize);
    }

    #[test]
    fn budget_suffix_is_empty_only_when_normal() {
        assert_eq!(budget_suffix(BudgetPhase::Normal, 10), "");
        assert!(budget_suffix(BudgetPhase::SoftWarn, 7).contains("about 7"));
        assert!(
            budget_suffix(BudgetPhase::SoftWarn, 7)
                .to_lowercase()
                .contains("converge")
        );
        let force = budget_suffix(BudgetPhase::ForceSummarize, 0);
        assert!(force.contains("NO tools"));
        assert!(force.to_lowercase().contains("final"));
    }

    // ── repeat guard ──────────────────────────────────────────────────────
    #[test]
    fn repeat_action_maps_counts_to_thresholds() {
        assert_eq!(repeat_action(1), RepeatAction::Allow);
        assert_eq!(repeat_action(2), RepeatAction::Allow);
        assert_eq!(repeat_action(3), RepeatAction::Block); // soft
        assert_eq!(repeat_action(4), RepeatAction::Break); // hard
        assert_eq!(repeat_action(9), RepeatAction::Break);
    }

    #[test]
    fn repeat_guard_counts_identical_calls() {
        let mut g = RepeatGuard::default();
        let args = json!({"path": "a.txt"});
        assert_eq!(g.record("read_file", &args), 1);
        assert_eq!(g.record("read_file", &args), 2);
        assert_eq!(g.record("read_file", &args), 3);
        // A different arg is a different signature → its own count.
        assert_eq!(g.record("read_file", &json!({"path": "b.txt"})), 1);
        // A different tool, same-ish args → distinct too.
        assert_eq!(g.record("list_dir", &args), 1);
    }

    #[test]
    fn signature_is_stable_across_key_order() {
        // Two argument objects differing ONLY in key order must collide, so a
        // model that reorders keys can't slip past the guard.
        let a = json!({"program": "git", "args": ["status"], "env": {"X": "1", "Y": "2"}});
        let b = json!({"args": ["status"], "env": {"Y": "2", "X": "1"}, "program": "git"});
        assert_eq!(signature("run_program", &a), signature("run_program", &b));
        // A real difference (different arg) must NOT collide.
        let c = json!({"program": "git", "args": ["push"]});
        assert_ne!(signature("run_program", &a), signature("run_program", &c));
    }

    // ── exit reason + banner round-trip ───────────────────────────────────
    #[test]
    fn exit_reason_tags_and_abnormality() {
        assert_eq!(ExitReason::Completed.tag(), "completed");
        assert!(!ExitReason::Completed.is_abnormal());
        assert!(ExitReason::ForcedSummarize { iterations: 9 }.is_abnormal());
        assert!(
            ExitReason::LoopDetected {
                call: "x".into(),
                count: 4
            }
            .is_abnormal()
        );
        assert!(ExitReason::BudgetExhausted { iterations: 50 }.is_abnormal());
    }

    #[test]
    fn banner_round_trips_through_parse() {
        let cases = [
            ExitReason::ForcedSummarize { iterations: 45 },
            ExitReason::BudgetExhausted { iterations: 50 },
            ExitReason::LoopDetected {
                call: "🔧 read a".into(),
                count: 4,
            },
        ];
        for r in cases {
            let parsed = ExitReason::parse_banner(&r.banner()).expect("must parse its own banner");
            // The tag and the carried scalar survive; LoopDetected's `call` text
            // is not embedded in the banner, so compare on tag + count/iterations.
            assert_eq!(parsed.tag(), r.tag());
            match (&parsed, &r) {
                (
                    ExitReason::LoopDetected { count: a, .. },
                    ExitReason::LoopDetected { count: b, .. },
                ) => {
                    assert_eq!(a, b)
                }
                (
                    ExitReason::ForcedSummarize { iterations: a },
                    ExitReason::ForcedSummarize { iterations: b },
                ) => {
                    assert_eq!(a, b)
                }
                (
                    ExitReason::BudgetExhausted { iterations: a },
                    ExitReason::BudgetExhausted { iterations: b },
                ) => {
                    assert_eq!(a, b)
                }
                _ => panic!("variant mismatch: {parsed:?} vs {r:?}"),
            }
        }
    }

    #[test]
    fn parse_banner_ignores_normal_and_garbage_first_lines() {
        assert_eq!(ExitReason::parse_banner("here is your answer\nmore"), None);
        assert_eq!(ExitReason::parse_banner(""), None);
        // A stop-shaped line with an unknown tag is NOT misread as a stop.
        assert_eq!(
            ExitReason::parse_banner("[aish-stop tag=mystery iterations=1 count=0] x"),
            None
        );
    }

    #[test]
    fn with_banner_prepends_only_for_abnormal() {
        let normal = with_banner(&ExitReason::Completed, "the answer");
        assert_eq!(normal, "the answer");
        let reason = ExitReason::ForcedSummarize { iterations: 45 };
        let out = with_banner(&reason, "  partial work  ");
        assert!(out.starts_with("[aish-stop tag=forced-summarize"));
        assert!(out.trim_end().ends_with("partial work"));
        // An empty body degrades to just the banner (still parseable).
        let empty = with_banner(&reason, "   ");
        assert_eq!(
            ExitReason::parse_banner(&empty).unwrap().tag(),
            "forced-summarize"
        );
    }

    // ── disposition routing ───────────────────────────────────────────────
    #[test]
    fn disposition_routes_by_reason_and_budget() {
        let max = MAX_AUTO_RECOVERIES;
        // Normal completion → nothing to do, regardless of recoveries spent.
        assert_eq!(
            classify_disposition(&ExitReason::Completed, 0, max),
            Disposition::None
        );
        // Out-of-budget / forced-summarize → resume (continue the work).
        assert_eq!(
            classify_disposition(&ExitReason::ForcedSummarize { iterations: 45 }, 0, max),
            Disposition::Resume
        );
        assert_eq!(
            classify_disposition(&ExitReason::BudgetExhausted { iterations: 50 }, 1, max),
            Disposition::Resume
        );
        // A loop → nudge (change approach), not a blind resume.
        assert_eq!(
            classify_disposition(
                &ExitReason::LoopDetected {
                    call: "x".into(),
                    count: 4
                },
                0,
                max
            ),
            Disposition::Nudge
        );
        // Once auto-recoveries are spent, ANY abnormal stop flags the operator.
        assert_eq!(
            classify_disposition(&ExitReason::ForcedSummarize { iterations: 45 }, max, max),
            Disposition::FlagOperator
        );
        assert_eq!(
            classify_disposition(
                &ExitReason::LoopDetected {
                    call: "x".into(),
                    count: 4
                },
                max,
                max
            ),
            Disposition::FlagOperator
        );
    }

    #[test]
    fn recovery_directive_matches_disposition() {
        let loop_reason = ExitReason::LoopDetected {
            call: "🔧 read a".into(),
            count: 4,
        };
        let nudge = recovery_directive(Disposition::Nudge, &loop_reason).unwrap();
        assert!(nudge.contains("[nudge]"));
        assert!(nudge.contains("read a"), "names the looping call: {nudge}");

        let resume = recovery_directive(
            Disposition::Resume,
            &ExitReason::BudgetExhausted { iterations: 50 },
        )
        .unwrap();
        assert!(resume.contains("[auto-resume]"));
        assert!(resume.to_lowercase().contains("where you left off"));

        // Terminal dispositions carry no directive.
        assert!(recovery_directive(Disposition::FlagOperator, &loop_reason).is_none());
        assert!(recovery_directive(Disposition::None, &ExitReason::Completed).is_none());
    }

    #[test]
    fn disposition_verbs() {
        assert_eq!(Disposition::None.verb(), "continuing");
        assert_eq!(Disposition::Resume.verb(), "auto-resuming");
        assert_eq!(Disposition::Nudge.verb(), "nudging");
        assert_eq!(Disposition::FlagOperator.verb(), "flagging for operator");
    }

    // ── merged round-exit evaluation ──────────────────────────────────────
    #[test]
    fn round_exit_evaluate_bundles_reason_and_disposition() {
        let max = MAX_AUTO_RECOVERIES;

        // A normal (un-bannered) answer is not an exit — nothing to recover.
        assert_eq!(RoundExit::evaluate("here is your answer", 0, max), None);

        // A forced-summarize banner → resume, with the reason + count bundled.
        let banner = ExitReason::ForcedSummarize { iterations: 45 }.banner();
        let exit = RoundExit::evaluate(&banner, 0, max).expect("abnormal → Some");
        assert_eq!(exit.reason.tag(), "forced-summarize");
        assert_eq!(exit.disposition, Disposition::Resume);
        assert_eq!(exit.recovery_count, 0);
        assert!(
            exit.directive()
                .expect("resume has a directive")
                .contains("[auto-resume]")
        );

        // A loop banner → nudge.
        let loop_banner = ExitReason::LoopDetected {
            call: "read a".into(),
            count: 4,
        }
        .banner();
        let looped = RoundExit::evaluate(&loop_banner, 0, max).expect("abnormal → Some");
        assert_eq!(looped.disposition, Disposition::Nudge);
        assert!(looped.directive().unwrap().contains("[nudge]"));

        // Interrupt → the sole abnormal reason that classifies to `None`; it is
        // NOT a recovery and carries no directive.
        let intr = RoundExit::evaluate(&ExitReason::Interrupted.banner(), 0, max)
            .expect("abnormal → Some");
        assert_eq!(intr.disposition, Disposition::None);
        assert!(intr.directive().is_none());

        // Once auto-recoveries are spent, an abnormal stop flags the operator —
        // terminal, no directive.
        let flagged = RoundExit::evaluate(&banner, max, max).expect("abnormal → Some");
        assert_eq!(flagged.disposition, Disposition::FlagOperator);
        assert_eq!(flagged.recovery_count, max);
        assert!(flagged.directive().is_none());
    }

    // ── batch-nudge guard ─────────────────────────────────────────────────
    #[test]
    fn is_batchable_read_only_pure_inspection_tools() {
        for name in [
            "read_file",
            "grep_files",
            "list_dir",
            "glob_expand",
            "stat_file",
            "diff_files",
        ] {
            assert!(is_batchable_read(name), "{name} should be batchable");
        }
        // Mutating / program-running tools are NOT batchable reads.
        for name in [
            "run_program",
            "edit_file",
            "write_file",
            "rename_file",
            "append_file",
            "run_in_background",
        ] {
            assert!(!is_batchable_read(name), "{name} must not be batchable");
        }
    }

    #[test]
    fn batch_guard_nudges_after_three_lone_reads() {
        let mut g = BatchGuard::default();
        assert!(g.record(&["read_file"]).is_none(), "1st lone read: no nudge");
        assert!(g.record(&["grep_files"]).is_none(), "2nd lone read: no nudge");
        let nudge = g.record(&["list_dir"]).expect("3rd lone read trips nudge");
        assert!(nudge.contains("[BATCH"));
        // At most once per streak — the 4th lone read stays silent.
        assert!(g.record(&["stat_file"]).is_none(), "no per-round nagging");
    }

    #[test]
    fn batch_guard_batched_turn_resets_streak() {
        let mut g = BatchGuard::default();
        g.record(&["read_file"]);
        g.record(&["read_file"]);
        // A batched turn (≥2 calls) resets the streak and re-arms the nudge.
        assert!(g.record(&["read_file", "grep_files"]).is_none());
        assert!(g.record(&["read_file"]).is_none(), "streak restarted at 1");
        assert!(g.record(&["read_file"]).is_none(), "streak at 2");
        assert!(
            g.record(&["read_file"]).is_some(),
            "streak at 3 → nudge again after reset"
        );
    }

    #[test]
    fn batch_guard_non_read_turn_resets_streak() {
        let mut g = BatchGuard::default();
        g.record(&["read_file"]);
        g.record(&["grep_files"]);
        // A turn that runs a program is not a lone batchable read → reset.
        assert!(g.record(&["run_program"]).is_none());
        assert!(g.record(&["read_file"]).is_none(), "streak restarted");
        assert!(g.record(&["read_file"]).is_none());
        assert!(g.record(&["read_file"]).is_some(), "nudge after fresh streak");
    }

    #[test]
    fn batch_guard_empty_turn_resets_streak() {
        let mut g = BatchGuard::default();
        g.record(&["read_file"]);
        g.record(&["read_file"]);
        // A no-tool turn breaks the streak.
        assert!(g.record(&[]).is_none());
        assert!(g.record(&["read_file"]).is_none(), "streak restarted at 1");
    }

    // ── serial-chain depth yield (TASK-358) ───────────────────────────────
    #[test]
    fn serial_chain_guard_yields_after_depth_exceeds_threshold() {
        let mut g = SerialChainGuard::default();
        // The first SERIAL_CHAIN_YIELD_DEPTH single-call rounds do NOT yield…
        for i in 1..=SERIAL_CHAIN_YIELD_DEPTH {
            assert!(
                g.record(1).is_none(),
                "round {i} (≤ threshold) must not yield"
            );
        }
        // …the very next one (the 13th with the default threshold of 12) trips
        // it, reporting the streak length reached.
        assert_eq!(
            g.record(1),
            Some(SERIAL_CHAIN_YIELD_DEPTH + 1),
            "the round past the threshold yields with the depth"
        );
    }

    #[test]
    fn serial_chain_guard_honors_threshold_override() {
        // An operator override (env AISH_SERIAL_CHAIN_YIELD_DEPTH, resolved by
        // the engine) lets a genuinely-serial workload run deeper before the
        // turn yields — the default of 12 is not a hard ceiling.
        let raised = SERIAL_CHAIN_YIELD_DEPTH + 4;
        let mut g = SerialChainGuard::with_threshold(raised);
        for i in 1..=raised {
            assert!(
                g.record(1).is_none(),
                "round {i} within the raised threshold must not yield"
            );
        }
        assert_eq!(
            g.record(1),
            Some(raised + 1),
            "yields only once the raised threshold is exceeded"
        );
    }

    #[test]
    fn serial_chain_guard_batched_or_empty_round_resets() {
        let mut g = SerialChainGuard::default();
        for _ in 0..SERIAL_CHAIN_YIELD_DEPTH {
            assert!(g.record(1).is_none());
        }
        // A batched round (≥2 calls) resets the chain — no yield, and the streak
        // must climb from scratch again.
        assert!(g.record(3).is_none(), "batched round resets the chain");
        for i in 1..=SERIAL_CHAIN_YIELD_DEPTH {
            assert!(g.record(1).is_none(), "post-reset round {i} must not yield");
        }
        assert_eq!(g.record(1), Some(SERIAL_CHAIN_YIELD_DEPTH + 1));

        // A no-call round also resets.
        let mut g2 = SerialChainGuard::default();
        for _ in 0..SERIAL_CHAIN_YIELD_DEPTH {
            assert!(g2.record(1).is_none());
        }
        assert!(g2.record(0).is_none(), "no-call round resets the chain");
        assert!(g2.record(1).is_none(), "streak restarted at 1");
    }

    #[test]
    fn serial_chain_yield_banner_round_trips_and_resumes() {
        let reason = ExitReason::SerialChainYield { depth: 9 };
        assert_eq!(reason.tag(), "serial-chain-yield");
        assert!(reason.is_abnormal());
        // depth survives the banner round-trip (carried in the `count` field).
        let parsed =
            ExitReason::parse_banner(&reason.banner()).expect("serial-chain banner must parse");
        assert_eq!(parsed, ExitReason::SerialChainYield { depth: 9 });
        // A deep serial chain is a recoverable stop → Resume (auto-continue),
        // and once auto-recoveries are spent it flags the operator.
        let max = MAX_AUTO_RECOVERIES;
        assert_eq!(
            classify_disposition(&reason, 0, max),
            Disposition::Resume
        );
        assert_eq!(
            classify_disposition(&reason, max, max),
            Disposition::FlagOperator
        );
        // End-to-end through RoundExit: resume with the auto-resume directive.
        let exit = RoundExit::evaluate(&with_banner(&reason, "partial"), 0, max)
            .expect("abnormal → Some");
        assert_eq!(exit.disposition, Disposition::Resume);
        assert!(exit.directive().unwrap().contains("[auto-resume]"));
    }

    // ── cumulative per-turn call budget (TASK-357) ────────────────────────
    #[test]
    fn call_budget_guard_soft_then_hard_crossings() {
        let mut g = CallBudgetGuard::default();
        // Stay under the soft cap → Continue, no advisory.
        assert_eq!(g.record(CALL_BUDGET_SOFT), CallBudgetAction::Continue);
        // Cross the soft cap → SoftWarn once, carrying the tally.
        assert_eq!(
            g.record(1),
            CallBudgetAction::SoftWarn {
                count: CALL_BUDGET_SOFT + 1
            }
        );
        // Soft fires at most once — subsequent under-hard rounds are Continue.
        assert_eq!(g.record(1), CallBudgetAction::Continue);
        // Advance to just past the hard cap → HardYield with the tally.
        let need = CALL_BUDGET_HARD.saturating_sub(CALL_BUDGET_SOFT + 2) + 1;
        assert_eq!(
            g.record(need),
            CallBudgetAction::HardYield {
                count: CALL_BUDGET_HARD + 1
            }
        );
    }

    #[test]
    fn call_budget_guard_hard_takes_precedence_over_soft() {
        // A single very-wide round that leaps past the hard cap yields HardYield,
        // never a SoftWarn — hard precedence.
        let mut g = CallBudgetGuard::default();
        assert_eq!(
            g.record(CALL_BUDGET_HARD + 5),
            CallBudgetAction::HardYield {
                count: CALL_BUDGET_HARD + 5
            }
        );
    }

    #[test]
    fn call_budget_exceeded_banner_round_trips_and_resumes() {
        let reason = ExitReason::CallBudgetExceeded { count: 61 };
        assert_eq!(reason.tag(), "call-budget-exceeded");
        assert!(reason.is_abnormal());
        // count survives the banner round-trip (carried in the `count` field).
        let parsed =
            ExitReason::parse_banner(&reason.banner()).expect("call-budget banner must parse");
        assert_eq!(parsed, ExitReason::CallBudgetExceeded { count: 61 });
        // Recoverable stop → Resume (auto-continue); flags the operator once the
        // auto-recovery budget is spent.
        let max = MAX_AUTO_RECOVERIES;
        assert_eq!(classify_disposition(&reason, 0, max), Disposition::Resume);
        assert_eq!(
            classify_disposition(&reason, max, max),
            Disposition::FlagOperator
        );
        // End-to-end through RoundExit: resume with the auto-resume directive.
        let exit = RoundExit::evaluate(&with_banner(&reason, "partial"), 0, max)
            .expect("abnormal → Some");
        assert_eq!(exit.disposition, Disposition::Resume);
        assert!(exit.directive().unwrap().contains("[auto-resume]"));
    }
}
