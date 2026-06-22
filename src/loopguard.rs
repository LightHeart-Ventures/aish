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
        _ => format!("loop-guard: blocked repeated call ×{count} — asking the model to re-plan: {desc}"),
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
}

impl ExitReason {
    /// The short, stable tag used in logs, banners, and disposition routing.
    pub fn tag(&self) -> &'static str {
        match self {
            ExitReason::Completed => "completed",
            ExitReason::ForcedSummarize { .. } => "forced-summarize",
            ExitReason::LoopDetected { .. } => "loop-detected",
            ExitReason::BudgetExhausted { .. } => "budget-exhausted",
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
                format!("forced a summarize-exit after {iterations} tool-call round(s) to avoid losing the work")
            }
            ExitReason::LoopDetected { call, count } => {
                format!("detected a loop — the call `{call}` was repeated {count} times without progress")
            }
            ExitReason::BudgetExhausted { iterations } => {
                format!("exhausted the {iterations}-round tool-call budget without a final answer")
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
            ExitReason::LoopDetected { count, .. } => (0, *count),
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
            "loop-detected" => Some(ExitReason::LoopDetected { call: String::new(), count }),
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

    /// Whether this disposition keeps the run going (vs. terminating it).
    pub fn is_recovery(self) -> bool {
        matches!(self, Disposition::Resume | Disposition::Nudge)
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
pub fn classify_disposition(reason: &ExitReason, auto_recoveries: usize, max_auto: usize) -> Disposition {
    match reason {
        ExitReason::Completed => Disposition::None,
        _ if auto_recoveries >= max_auto => Disposition::FlagOperator,
        ExitReason::LoopDetected { .. } => Disposition::Nudge,
        ExitReason::ForcedSummarize { .. } | ExitReason::BudgetExhausted { .. } => Disposition::Resume,
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
            assert_eq!(budget_phase(max, max), BudgetPhase::ForceSummarize, "max={max}");
        }
        // A degenerate zero budget forces immediately.
        assert_eq!(budget_phase(1, 0), BudgetPhase::ForceSummarize);
    }

    #[test]
    fn budget_suffix_is_empty_only_when_normal() {
        assert_eq!(budget_suffix(BudgetPhase::Normal, 10), "");
        assert!(budget_suffix(BudgetPhase::SoftWarn, 7).contains("about 7"));
        assert!(budget_suffix(BudgetPhase::SoftWarn, 7).to_lowercase().contains("converge"));
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
        assert!(ExitReason::LoopDetected { call: "x".into(), count: 4 }.is_abnormal());
        assert!(ExitReason::BudgetExhausted { iterations: 50 }.is_abnormal());
    }

    #[test]
    fn banner_round_trips_through_parse() {
        let cases = [
            ExitReason::ForcedSummarize { iterations: 45 },
            ExitReason::BudgetExhausted { iterations: 50 },
            ExitReason::LoopDetected { call: "🔧 read a".into(), count: 4 },
        ];
        for r in cases {
            let parsed = ExitReason::parse_banner(&r.banner()).expect("must parse its own banner");
            // The tag and the carried scalar survive; LoopDetected's `call` text
            // is not embedded in the banner, so compare on tag + count/iterations.
            assert_eq!(parsed.tag(), r.tag());
            match (&parsed, &r) {
                (ExitReason::LoopDetected { count: a, .. }, ExitReason::LoopDetected { count: b, .. }) => {
                    assert_eq!(a, b)
                }
                (ExitReason::ForcedSummarize { iterations: a }, ExitReason::ForcedSummarize { iterations: b }) => {
                    assert_eq!(a, b)
                }
                (ExitReason::BudgetExhausted { iterations: a }, ExitReason::BudgetExhausted { iterations: b }) => {
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
        assert_eq!(ExitReason::parse_banner("[aish-stop tag=mystery iterations=1 count=0] x"), None);
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
        assert_eq!(ExitReason::parse_banner(&empty).unwrap().tag(), "forced-summarize");
    }

    // ── disposition routing ───────────────────────────────────────────────
    #[test]
    fn disposition_routes_by_reason_and_budget() {
        let max = MAX_AUTO_RECOVERIES;
        // Normal completion → nothing to do, regardless of recoveries spent.
        assert_eq!(classify_disposition(&ExitReason::Completed, 0, max), Disposition::None);
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
            classify_disposition(&ExitReason::LoopDetected { call: "x".into(), count: 4 }, 0, max),
            Disposition::Nudge
        );
        // Once auto-recoveries are spent, ANY abnormal stop flags the operator.
        assert_eq!(
            classify_disposition(&ExitReason::ForcedSummarize { iterations: 45 }, max, max),
            Disposition::FlagOperator
        );
        assert_eq!(
            classify_disposition(&ExitReason::LoopDetected { call: "x".into(), count: 4 }, max, max),
            Disposition::FlagOperator
        );
    }

    #[test]
    fn recovery_directive_matches_disposition() {
        let loop_reason = ExitReason::LoopDetected { call: "🔧 read a".into(), count: 4 };
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
    fn disposition_verbs_and_recovery_flag() {
        assert!(Disposition::Resume.is_recovery());
        assert!(Disposition::Nudge.is_recovery());
        assert!(!Disposition::FlagOperator.is_recovery());
        assert!(!Disposition::None.is_recovery());
        assert_eq!(Disposition::Resume.verb(), "auto-resuming");
        assert_eq!(Disposition::FlagOperator.verb(), "flagging for operator");
    }
}
