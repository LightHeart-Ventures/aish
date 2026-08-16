use crate::backend::{Backend, Msg, OutputSchemaRef, Role, ToolResult};
use crate::session::Session;
use crate::tools::{self, Confirm};
use anyhow::Result;
use std::sync::{Arc, Mutex};

/// Animation state for a running tool, shared between the spinner task and the
/// confirm wrapper. The task reads it (under the lock) right before each frame;
/// `pause_spinner`/`resume_spinner`/`stop_spinner` write it (under the SAME
/// lock), so the lock serializes draw-vs-control and no frame can land after the
/// prompt (the race that made permission prompts look "off-screen"). Pausing for
/// the prompt also means the spinner only animates while the tool is *executing*,
/// never while waiting for the permission answer.
#[derive(PartialEq, Clone, Copy)]
enum Spin {
    Running,
    Paused,
    Stopped,
}
type SpinState = Arc<Mutex<Spin>>;

// Per-turn tool-call iteration backstop. Generous enough that a legitimate
// multi-file task (read several files, edit them, build, test, fix) completes
// within one turn, but still a hard stop so a runaway tool loop terminates.
// The loop no longer SLAMS into this cap empty-handed: the budget phases in
// `crate::loopguard` fire a soft warning at ~75% and a forced summarize-exit at
// ~90% (see the loop below), so the model converges and hands back a partial
// answer before the hard limit is ever reached.
const MAX_ITERATIONS: usize = 50;

/// Operator override for the serial-chain yield depth, read from
/// `AISH_SERIAL_CHAIN_YIELD_DEPTH`. Defaults to
/// [`crate::loopguard::SERIAL_CHAIN_YIELD_DEPTH`]; a parsed value is honoured
/// only inside `[1, 1000]`, otherwise the default stands (a typo can never
/// uncap or zero-cap the guard). Lets a genuinely-serial workload — a long
/// chain of dependent calls that cannot be batched — raise the ceiling instead
/// of false-tripping the yield and burning the coordinator's auto-recovery
/// budget. Same forgiving parse/clamp discipline as `AISH_COORDINATOR_MAX_ROUNDS`.
fn serial_chain_yield_depth() -> usize {
    std::env::var("AISH_SERIAL_CHAIN_YIELD_DEPTH")
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .filter(|&n| (1..=1000).contains(&n))
        .unwrap_or(crate::loopguard::SERIAL_CHAIN_YIELD_DEPTH)
}

/// Operator overrides for the per-turn cumulative tool-call budget, read from
/// `AISH_CALL_BUDGET_SOFT` / `AISH_CALL_BUDGET_HARD`. Each defaults to its
/// compile-time counterpart ([`crate::loopguard::CALL_BUDGET_SOFT`] /
/// [`CALL_BUDGET_HARD`](crate::loopguard::CALL_BUDGET_HARD)); a parsed value is
/// honoured only inside `[1, 100000]`, otherwise the default stands (a typo can
/// never uncap or zero-cap the guard). Lets a genuinely-wide but legitimate turn
/// — a large multi-file edit+build+test batch that can't be split — run past the
/// default hard ceiling instead of false-tripping the yield and draining the
/// coordinator's auto-recovery budget. A bad pairing where soft ≥ hard is benign:
/// `record` checks hard first, so the turn just yields hard-first and the soft
/// advisory never fires. Same forgiving parse/clamp discipline as
/// `AISH_SERIAL_CHAIN_YIELD_DEPTH`.
fn call_budget() -> (usize, usize) {
    fn resolve(var: &str, default: usize) -> usize {
        std::env::var(var)
            .ok()
            .and_then(|v| v.trim().parse::<usize>().ok())
            .filter(|&n| (1..=100_000).contains(&n))
            .unwrap_or(default)
    }
    (
        resolve("AISH_CALL_BUDGET_SOFT", crate::loopguard::CALL_BUDGET_SOFT),
        resolve("AISH_CALL_BUDGET_HARD", crate::loopguard::CALL_BUDGET_HARD),
    )
}

/// One full agentic turn: user input → (model ⇄ tools)* → final text.
/// Frontend-agnostic: confirmation is a callback, output goes through eprintln
/// only for transient activity lines.
///
/// ## Loop-guard / graceful degradation (see `crate::loopguard`)
/// The `model ⇄ tools` loop is bounded by `MAX_ITERATIONS`, but three guards
/// keep it from burning that whole budget spinning and then throwing the work
/// away:
///   1. **Same-call repeat guard** — an identical `(tool, args)` call repeated
///      past a threshold is NOT re-executed (no duplicate side effect); the
///      model is fed a corrective result, and a confirmed loop breaks the turn
///      with a tagged `loop-detected` stop.
///   2. **Soft warning** — past ~75% of the budget, a "converge now" notice is
///      folded into the system prompt.
///   3. **Forced summarize** — past ~90%, the model is handed NO tools and must
///      produce a best-effort final answer, returned with a `forced-summarize`
///      banner instead of an empty hard-cap stop.
/// An abnormal stop prepends a greppable [`crate::loopguard::ExitReason::banner`]
/// line to the answer so the coordinator can pick a recovery disposition
/// (resume / nudge / flag-for-operator) — even across the worker subprocess
/// stdout boundary.
pub async fn run_turn(
    backend: &Backend,
    session: &mut Session,
    input: String,
    confirm: &mut Confirm<'_>,
) -> Result<String> {
    let result = run_turn_inner(backend, session, input, confirm).await;
    // Observe hooks at the turn boundary: TurnEnd on a final answer (carrying its
    // length), TurnEndFailure on a backend/loop error. Fires once per LOGICAL
    // turn (this is the single return point), so a consumer never double-counts a
    // prefill-continuation that spanned several rounds. Zero-cost when no hook is
    // registered (`has` short-circuits before any payload is built).
    match &result {
        Ok(text) if session.hooks.has(crate::hooks::HookEvent::TurnEnd) => {
            let p = session
                .hook_payload(crate::hooks::HookEvent::TurnEnd)
                .with("answer_len", text.len() as u64);
            session
                .hooks
                .fire_observe(crate::hooks::HookEvent::TurnEnd, p);
        }
        Err(e) if session.hooks.has(crate::hooks::HookEvent::TurnEndFailure) => {
            let p = session
                .hook_payload(crate::hooks::HookEvent::TurnEndFailure)
                .with("error", e.to_string());
            session
                .hooks
                .fire_observe(crate::hooks::HookEvent::TurnEndFailure, p);
        }
        _ => {}
    }
    result
}

/// TASK-407 (SPR-071): repo-open auto-index handoff. When aish enters any repo
/// where the `codebase-memory` server is enrolled AND connected, warm its
/// structural index ONCE per repo-open so the graph is ready before the first
/// coordinator query. Bounded handoff: the index tool kicks a background build
/// and returns fast, and the call is wrapped in a short timeout so a large repo
/// can never hang the prompt. Cheap to call every turn — the dedup set
/// short-circuits after the first fire and the only work on a miss is one
/// `.mcp.json` read. The `auto_index` config gate (env override ->
/// `.mcp.json` -> default-on) honours opt-out.
async fn maybe_auto_index_repo(session: &mut Session) {
    use crate::codebase_memory as cbm;
    // Repo-open marker: check canonical cwd as the stable dedup key. No need
    // for external markers — if the server is connected, we warm the index.
    // Stable dedup key: the canonical repo root (falls back to cwd as-is).
    let repo_root = std::fs::canonicalize(&session.cwd).unwrap_or_else(|_| session.cwd.clone());
    let already = session.codebase_indexed.contains(&repo_root);

    // Enrolled? (registered in `~/.aish/.mcp.json`). On absence/parse-failure,
    // treat the config as an empty object -> `is_enrolled` returns false.
    let root = std::fs::read_to_string(cbm::aish_home().join(".mcp.json"))
        .ok()
        .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
        .unwrap_or_else(|| serde_json::json!({}));
    let enrolled = cbm::is_enrolled(&root, cbm::SERVER_NAME);
    // Connected? The server actually handshook this session (binary present +
    // tools live) -- not merely declared on disk.
    let connected = session
        .mcp
        .server_names()
        .iter()
        .any(|n| n == cbm::SERVER_NAME);
    // Config gate: env override (`AISH_CODEBASE_AUTO_INDEX`) -> `.mcp.json` -> on.
    let env = session
        .env
        .iter()
        .find(|(k, _)| k == cbm::AUTO_INDEX_ENV)
        .map(|(_, v)| v.clone());
    let gate_on = cbm::auto_index_enabled(&root, env.as_deref());
    if !cbm::should_auto_index(enrolled, connected, gate_on, already) {
        return;
    }
    // Dedup: mark this repo root warmed BEFORE the (bounded) handoff so a slow or
    // timing-out index can't double-fire on the next turn.
    session.codebase_indexed.insert(repo_root.clone());

    let qualified = cbm::index_tool_qualified();
    let args = cbm::index_args(&repo_root);
    // Bounded handoff: the index tool kicks a background build and returns fast;
    // the timeout guarantees we never block the prompt on a large repo.
    const AUTO_INDEX_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
    // Best-effort diagnostics: handoff warnings are appended to
    // `~/.aish/codebase-memory.log` and only echoed to stderr when
    // `AISH_CODEBASE_DEBUG` is truthy, so the normal TUI/coordinator stream stays
    // clean (this is a non-fatal background warm, not an actionable error).
    let debug_echo = cbm::debug_echo_enabled(
        session
            .env
            .iter()
            .find(|(k, _)| k == cbm::DEBUG_ENV)
            .map(|(_, v)| v.as_str()),
    );
    match tokio::time::timeout(AUTO_INDEX_TIMEOUT, session.mcp.call(&qualified, &args)).await {
        Ok(Ok(_)) => {}
        Ok(Err(e)) => {
            let msg = format!("repo-open auto-index handoff failed: {e}");
            cbm::log_handoff_event(&msg);
            if debug_echo {
                eprintln!("[codebase-memory] {msg}");
            }
        }
        Err(_) => {
            let msg = format!(
                "repo-open auto-index handoff timed out after {}s (index continues server-side)",
                AUTO_INDEX_TIMEOUT.as_secs()
            );
            cbm::log_handoff_event(&msg);
            if debug_echo {
                eprintln!("[codebase-memory] {msg}");
            }
        }
    }
}

/// The agentic turn body. Wrapped by [`run_turn`], which fires the
/// `TurnEnd`/`TurnEndFailure` observe hooks around it.
async fn run_turn_inner(
    backend: &Backend,
    session: &mut Session,
    input: String,
    confirm: &mut Confirm<'_>,
) -> Result<String> {
    // Decide, for this turn, whether a stronger model is worth escalating to
    // (weak frontend → Some) and stash it so the `escalate` tool can rebuild that
    // backend at call time. Drives both the tool's availability and the nudge.
    session.escalation = backend
        .escalation_target(&session.batch_model, &session.env)
        .map(|(provider, model)| (provider.to_string(), model));
    let escalate_available = session.escalation.is_some();
    let system = session.system_prompt(escalate_available);
    let mut tool_defs = tools::tool_defs(session.batch_mode, escalate_available, session.nested);
    if backend.include_mcp_tools() {
        // TASK-323: apply the per-run/per-mode tool allowlist BEFORE serializing
        // the MCP tool-schema block, and measure the payload reduction. The
        // filter is order-preserving, so for a fixed allowlist the emitted tools
        // block is byte-identical turn to turn — no mid-session cache invalidation.
        let (scoped, measure) = session
            .mcp
            .scope_tool_defs(session.tool_allowlist.as_deref());
        if measure.trimmed() {
            static SCOPE_LOG: std::sync::Once = std::sync::Once::new();
            SCOPE_LOG.call_once(|| {
                eprintln!(
                    "[tool-scope] MCP tools {}->{} (~{} tokens saved: {}->{})",
                    measure.tools_before,
                    measure.tools_after,
                    measure.tokens_saved(),
                    measure.tokens_before,
                    measure.tokens_after,
                );
            });
        }
        tool_defs.extend(scoped);
    }
    // TASK-13: on a fresh conversation, seed the turn with the previous recorded
    // output so a prompt like "summarize that" can reference it without
    // re-running. Mid-conversation the output is already in `history`, so we
    // don't duplicate it.
    // Context-awareness: before adding this turn, compact the conversation if it
    // has grown past the window threshold — offloading the oldest slice to the
    // SQLite memories table and replacing it with a short in-context summary.
    maybe_compact(backend, session);
    // TASK-407: repo-open auto-index handoff (dedup'd, bounded, opt-out-aware).
    maybe_auto_index_repo(session).await;
    // Skill-awareness (crate::skill_match): score THIS turn's request against the
    // installed local skill catalog and, when one clearly fits, fold a short note
    // pointing at its SKILL.md into the turn input. Matched on the raw request
    // (before context-seeding) so a prepended preamble can't skew the keyword
    // match; the note goes into the turn input, never the cached system prompt,
    // so the prompt-cache prefix stays byte-stable.
    let task = input.clone();
    // `:new` sets suppress_context_seed so the freshly-cleared (empty) history
    // doesn't re-trigger the last-output seed below. Consume it one-shot: a later
    // command's output can still seed a genuinely fresh prompt.
    let seed_prev = if std::mem::take(&mut session.suppress_context_seed) {
        None
    } else {
        session.last_output()
    };
    let input = seed_context(session.history.is_empty(), seed_prev, input);
    // Prefer an INSTALLED skill: when one clearly fits, fold in the note pointing
    // at its SKILL.md. When NONE fits a substantial task, fall back to an OFFLINE
    // recommendation of an installable registry skill (read from the
    // binary-shipped index — no network on the hot path), so the model surfaces a
    // `:skill add <ref>` suggestion instead of faking or hand-rolling the work.
    let input = match crate::skill_match::hint(&task, &session.skills) {
        Some(note) => {
            // Observe hook: SkillMatched — an INSTALLED skill cleared the
            // relevance bar for this turn's task and its SKILL.md note was folded
            // into the input. Carry the top-ranked skill's name + path (the path
            // is matchable via the matcher's `path_glob`), its score, and the
            // total number of matching skills, so a consumer can log which
            // playbook was surfaced. Observe-only — it can't change the hint. The
            // registry-recommendation path (no installed match) is a different
            // event and deliberately does NOT fire this. Zero-cost when no hook is
            // registered (`has` short-circuits before any rank/payload work).
            if session.hooks.has(crate::hooks::HookEvent::SkillMatched) {
                let matches = crate::skill_match::rank(&task, &session.skills);
                if let Some(top) = matches.first() {
                    let p = session
                        .hook_payload(crate::hooks::HookEvent::SkillMatched)
                        .with("skill", top.skill.name.clone())
                        .with("path", top.skill.path.to_string_lossy().into_owned())
                        .with("score", top.score as u64)
                        .with("match_count", matches.len() as u64);
                    session
                        .hooks
                        .fire_observe(crate::hooks::HookEvent::SkillMatched, p);
                }
            }
            // Repo-awareness: when a skill fits AND the working directory has a
            // `.repospec.json`, remind the model to read that spec first and keep
            // its conventions in mind while applying the skill. The skill's steps
            // are generic; the repo spec is project-specific and should win. The
            // fs check lives here (not in the pure, unit-tested `hint`) since it
            // depends on `session.cwd`.
            let note = if session
                .cwd
                .join(crate::skill_match::REPOSPEC_FILE)
                .exists()
            {
                format!("{note}\n{}", crate::skill_match::repospec_reminder())
            } else {
                note
            };
            format!("{note}\n\n{input}")
        }
        None => maybe_recommend_skill(&task, input, session),
    };
    // S9.3: persist the turn input to the per-worker transcript (coordinator
    // run only — None/no-op interactively), so `:attach`/resume can replay the
    // user/system messages, not just the tool turns the audit journal records.
    if let Some(w) = session.worker_transcript.as_mut() {
        w.record_message("user", "text", &input);
    }
    session.history.push(Msg::user(input));
    session.last_turn_tools.clear();
    // One logical turn begins here — tally it for the interactive activity-stream
    // status line (`turns: …`). Counts the LOGICAL turn once (this body runs per
    // `run_turn`), so a prefill-continuation spanning several rounds isn't
    // double-counted.
    session.turns_total = session.turns_total.saturating_add(1);

    // Observe hook: UserPromptSubmit — the turn has begun (after compaction +
    // skill-hint folding, the prompt is now the trailing history message). Phase
    // 1 is observe-only: the prompt-prepend/append mutation is Phase 3. The raw
    // request (`task`, pre-seeding) is sent so a consumer logs what the user
    // actually asked. Zero-cost when no hook is registered.
    if session.hooks.has(crate::hooks::HookEvent::UserPromptSubmit) {
        let p = session
            .hook_payload(crate::hooks::HookEvent::UserPromptSubmit)
            .with("prompt", task.clone());
        session
            .hooks
            .fire_observe(crate::hooks::HookEvent::UserPromptSubmit, p);
    }

    // First local use lazy-loads (and maybe downloads) weights — do it before
    // any spinner exists so the download progress line owns stderr.
    backend.prepare().await?;

    // While set, the model is mid-PREFILL-CONTINUATION: a prior round's plain-text
    // answer hit the output limit, so its partial text is the trailing assistant
    // message in `history` and each subsequent round RESUMES it. We MERGE the new
    // chunk into that same message (a second consecutive assistant message would
    // break role alternation) and keep going until the answer completes — instead
    // of handing the user a reply cut off mid-sentence. Only backends that resume
    // a trailing assistant message verbatim opt in (see
    // `Backend::supports_prefill_continuation`).
    let mut continuing = false;

    // Per-turn loop guards: a same-call repeat tally + the budget thresholds that
    // drive the soft-warning → forced-summarize graceful degradation. Both live
    // only for this turn (dropped with the function) so nothing bleeds across.
    let mut repeat_guard = crate::loopguard::RepeatGuard::default();

    // TASK-324 AC2: detect drip-fed single read turns (grep→read→grep→read…) and
    // fold a one-shot "batch your independent reads" nudge into the NEXT round's
    // prompt. `batch_guard` tallies consecutive lone-read turns; `pending_batch_nudge`
    // carries the nudge forward exactly one round, then is dropped so the base
    // prompt stays byte-stable for cache reuse.
    let mut batch_guard = crate::loopguard::BatchGuard::default();
    let mut pending_batch_nudge: Option<String> = None;

    // TASK-358: serial-chain depth yield. Tracks the current run of consecutive
    // single-tool-call rounds ACROSS iterations of this turn (unlike the
    // round-local guards above). A deep serial chain (grep→read→edit→run→…)
    // drains the rate-limit window — every round re-sends the whole context —
    // even when the calls differ (not a loop) and the budget is not yet spent.
    // Past `SERIAL_CHAIN_YIELD_DEPTH` the turn yields with a resumable banner so
    // the durable coordinator loop checkpoints and re-plans toward batching.
    let mut serial_chain_guard =
        crate::loopguard::SerialChainGuard::with_threshold(serial_chain_yield_depth());
    // TASK-357: cumulative per-turn tool-call budget. Counts EVERY tool call
    // executed across the whole turn (a batched round of N advances it by N),
    // orthogonal to the round/iteration budget and the serial-chain SHAPE guard.
    // A soft advisory logs at CALL_BUDGET_SOFT; the turn yields (resumably) once
    // the tally crosses CALL_BUDGET_HARD.
    let (call_budget_soft, call_budget_hard) = call_budget();
    let mut call_budget_guard =
        crate::loopguard::CallBudgetGuard::with_budget(call_budget_soft, call_budget_hard);

    for iteration in 1..=MAX_ITERATIONS {
        // ── Operator interrupt seam (Ctrl-C on an `:attach`ed worker). A
        // coordinator installs a SIGINT handler that latches an interrupt flag
        // (see `coordinator::drive`); check+clear it here, at the clean top of an
        // iteration where history is consistent (the previous iteration fully
        // appended its tool_use + tool_results, so there is no dangling
        // tool_use). End the turn with an `interrupted` banner — the drive loop
        // turns that into a "reassess" round while keeping the coordinator alive.
        // Gated on `session.nested`: interactive sessions never install the
        // handler, so this is a no-op for them (and the flag is never set).
        if session.nested && crate::coordinator::take_interrupt() {
            let reason = crate::loopguard::ExitReason::Interrupted;
            eprintln!("\x1b[2maish: {}\x1b[0m", reason.log_line());
            return Ok(crate::loopguard::with_banner(
                &reason,
                "[turn interrupted by the operator before it finished]",
            ));
        }
        // Context-awareness INSIDE the agentic loop. The pre-loop `maybe_compact`
        // above only fires between TURNS, but a single long turn — the
        // coordinator's bread-and-butter, and any tool-heavy interactive turn —
        // appends many (often large) tool-result messages and can blow past the
        // model's window WITHIN this one `run_turn`, long before the next turn's
        // pre-loop check would ever run. So re-check here, right before the next
        // model call, using the running usage figure from the previous round
        // (`session.context_used`, updated each iteration below). This is the fix
        // for "ran out of context mid-conversation": without it, compaction was
        // structurally unreachable during the very loop that grows history.
        //
        // Skipped while a prefill continuation is in flight: that path resumes a
        // trailing assistant message verbatim, and compaction must not perturb
        // the message the backend is mid-resume on (it only ever drops a PREFIX
        // and keeps the recent tail, so this is belt-and-suspenders).
        if !continuing {
            maybe_compact(backend, session);
            maybe_auto_index_repo(session).await;
        }
        // Budget phase for THIS round: Normal → run freely; SoftWarn → fold a
        // "converge now" notice into the prompt; ForceSummarize → hand the model
        // NO tools so it must produce a best-effort final answer rather than being
        // killed empty-handed at the hard cap.
        let phase = crate::loopguard::budget_phase(iteration, MAX_ITERATIONS);
        let force = matches!(phase, crate::loopguard::BudgetPhase::ForceSummarize);
        // The base prompt stays byte-stable (prompt-cache friendly); the budget
        // suffix is appended only while converging/forcing.
        let effective_system = {
            let budget = match phase {
                crate::loopguard::BudgetPhase::Normal => String::new(),
                other => crate::loopguard::budget_suffix(
                    other,
                    MAX_ITERATIONS.saturating_sub(iteration),
                ),
            };
            // One-shot batch nudge from a prior round's drip-feed streak (AC2).
            let nudge = pending_batch_nudge.take().unwrap_or_default();
            if budget.is_empty() && nudge.is_empty() {
                system.clone()
            } else {
                format!("{system}{nudge}{budget}")
            }
        };
        // No tools on the forced-summarize step — the model literally cannot keep
        // looping, so a final (possibly partial) answer is guaranteed.
        let active_tools: &[crate::backend::ToolDef] = if force { &[] } else { &tool_defs };

        // Model-reasoning phase: the "thinking" spinner owns stderr while the
        // backend produces the next message (which may consume prior tool
        // results). It is stopped before any tool-execution animation begins,
        // so the two never run at once.
        emit_thinking(session);
        let spinner = Spinner::start();
        let turn = backend
            .complete(&effective_system, &session.history, active_tools)
            .await;
        drop(spinner);
        let turn = turn?;
        let usage = turn.usage;

        // Update the running context figure from the backend's reported usage
        // (the prompt the model just saw), or a char-based estimate as a fallback.
        session.context_used = match usage {
            Some(u) => u.total(),
            None => crate::context::estimate_history_tokens(&session.history),
        };
        // Accumulate this round's token usage into the session totals that feed
        // the interactive activity-stream status line. Only real backend-reported
        // usage is summed (the estimate fallback above is a window gauge, not a
        // billed figure), so `tokens in/out` stays an honest running tally.
        if let Some(u) = usage {
            session.tokens_in = session.tokens_in.saturating_add(u.input_tokens);
            session.tokens_out = session.tokens_out.saturating_add(u.output_tokens);
            // TASK-320: track cache read/write volume so the status line and
            // `:context` can report a session-level cache hit rate. Purely
            // observational — these never affect context-window math.
            session.cache_read_total =
                session.cache_read_total.saturating_add(u.cache_read_tokens);
            session.cache_creation_total = session
                .cache_creation_total
                .saturating_add(u.cache_creation_tokens);
        }

        // ── Forced summarize: graceful degradation before the hard limit. The
        // model had no tools this step, so `turn.text` is its best final answer.
        // Record it in history (so the transcript stays well-formed) and return
        // it tagged `forced-summarize`, so the coordinator can decide whether to
        // auto-resume the remaining work.
        if force {
            let text = if continuing {
                match session.history.last_mut() {
                    Some(last) => {
                        last.text.push_str(&turn.text);
                        last.raw = None;
                        last.tool_calls.clear();
                        last.text.clone()
                    }
                    None => turn.text.clone(),
                }
            } else {
                session.history.push(Msg {
                    role: Role::Assistant,
                    text: turn.text.clone(),
                    tool_calls: vec![],
                    tool_results: vec![],
                    raw: turn.raw,
                });
                turn.text.clone()
            };
            let reason = crate::loopguard::ExitReason::ForcedSummarize {
                iterations: iteration,
            };
            eprintln!("\x1b[2maish: {}\x1b[0m", reason.log_line());
            return Ok(crate::loopguard::with_banner(&reason, &text));
        }

        if continuing {
            // Prefill-continuation round: fold this chunk into the partial answer
            // already sitting as the last assistant message (don't append a new
            // one — consecutive assistant messages break role alternation).
            let merged = match session.history.last_mut() {
                Some(last) => {
                    last.text.push_str(&turn.text);
                    last.raw = None; // stay clean text(+tool) — no stale thinking block
                    last.tool_calls = turn.tool_calls.clone();
                    last.text.clone()
                }
                None => turn.text.clone(),
            };
            // The continuation itself was cut off again → keep resuming.
            if turn.truncated_text && turn.tool_calls.is_empty() {
                continue;
            }
            continuing = false;
            // A clean continuation with no tool call IS the finished answer.
            if turn.tool_calls.is_empty() {
                return Ok(merged);
            }
            // Rare: the model resumed its prose and then requested a tool. The
            // merged message already carries those tool calls; fall through to
            // execute them like any other tool round.
        } else {
            session.history.push(Msg {
                role: Role::Assistant,
                text: turn.text.clone(),
                tool_calls: turn.tool_calls.clone(),
                tool_results: vec![],
                raw: turn.raw,
            });

            // A response cut off mid-tool-call had its partial tool call dropped
            // (see claude.rs) — `tool_calls` is empty but this is NOT a final
            // answer. The corrective note is already in `turn.text` (now in history
            // as the assistant message); loop so the model retries with a smaller
            // edit.
            if turn.truncated_tool_call {
                if !turn.text.trim().is_empty() {
                    emit_narration(session, &turn.text);
                }
                continue;
            }

            if turn.tool_calls.is_empty() {
                // A final answer that hit the output limit is RESUMED via a
                // prefill-continuation round (on backends that support it) rather
                // than returned half-finished. The partial text is already the
                // trailing assistant message, so the next `complete` continues it;
                // chunks accumulate into that message and we return the whole thing.
                if turn.truncated_text && backend.supports_prefill_continuation() {
                    continuing = true;
                    continue;
                }
                return Ok(turn.text);
            }
        }

        // Interim narration from the model (its reasoning between tool calls) —
        // rendered at normal brightness like the final answer, because it's
        // substantive content the user reads. Only the transient 🔧 tool-activity
        // lines stay dim, so the rounds still read as structured.
        if !turn.text.trim().is_empty() {
            emit_narration(session, &turn.text);
        }

        // TASK-324 AC2: record this executing turn's tool-call shape. When the
        // model has drip-fed a lone batchable read for `BATCH_NUDGE_STREAK` turns
        // running, arm a one-shot nudge for the next round's prompt.
        {
            let names: Vec<&str> = turn.tool_calls.iter().map(|c| c.name.as_str()).collect();
            if let Some(nudge) = batch_guard.record(&names) {
                eprintln!(
                    "\x1b[2m  ⚠ batch-guard: {} single-read turns in a row — nudging to batch independent reads\x1b[0m",
                    crate::loopguard::BATCH_NUDGE_STREAK
                );
                pending_batch_nudge = Some(nudge);
            }
        }

        // TASK-358: record this round's tool-call shape for serial-chain depth.
        // A round with exactly one call EXTENDS the chain; a batched (≥2) or
        // no-call round RESETS it. When the chain first exceeds
        // `SERIAL_CHAIN_YIELD_DEPTH` we still let THIS round's call execute below
        // (so the tool_use is paired with its tool_result and history stays
        // well-formed), then yield AFTER the round — see the `serial_yield`
        // check past the tool loop.
        let serial_yield = serial_chain_guard.record(turn.tool_calls.len());
        if let Some(depth) = serial_yield {
            eprintln!(
                "\x1b[2m  ⚠ serial-chain-guard: {depth} consecutive single-call rounds — yielding to re-plan toward batching\x1b[0m"
            );
        }

        // TASK-357: advance the cumulative per-turn call budget by THIS round's
        // call count. A hard crossing is deferred (like `serial_yield`) until
        // after the round's tool_results are appended, so history stays
        // well-formed; a soft crossing just logs an advisory once.
        let call_budget = call_budget_guard.record(turn.tool_calls.len());
        match call_budget {
            crate::loopguard::CallBudgetAction::SoftWarn { count } => {
                eprintln!(
                    "\x1b[2m  ⚠ call-budget-guard: {count} tool calls this turn (soft {}) — consider converging\x1b[0m",
                    call_budget_soft
                );
            }
            crate::loopguard::CallBudgetAction::HardYield { count } => {
                eprintln!(
                    "\x1b[2m  ⚠ call-budget-guard: {count} tool calls this turn (hard {}) — yielding to resume with fresh context\x1b[0m",
                    call_budget_hard
                );
            }
            crate::loopguard::CallBudgetAction::Continue => {}
        }

        let mut results: Vec<ToolResult> = Vec::with_capacity(turn.tool_calls.len());
        // Set when the same-call guard CONFIRMS a loop this round: we still finish
        // the round's bookkeeping (push the synthetic results so history stays
        // well-formed), then break the turn with a tagged `loop-detected` stop.
        let mut loop_break: Option<(String, usize)> = None;
        for call in &turn.tool_calls {
            // Prefix the per-tool glyph (🛠️ local · 🔧 MCP · 🤝 escalate) so it
            // travels with the desc through the running spinner,
            // the finished ✓/✗ line, and the retroactive reveal — every place
            // that renders the activity line. Flatten ONLY the description
            // (collapses embedded newlines from e.g. a `gh pr create --body`
            // payload); the glyph is joined afterward with a single space. The
            // double-width wrench/handshake need no more than that; the narrow
            // 🛠️ hammer (U+1F6E0+VS16, width-1 in most terminals) carries its own
            // trailing spacer in `tool_glyph` so it still reads `🛠️ <desc>`.
            let desc = format!(
                "{} {}",
                tool_glyph(&call.name),
                flatten_ws(&describe_call(call))
            );

            // ── Same-call repeat guard (loop detection). A tool call whose exact
            // (name, args) signature has recurred past the soft threshold is NOT
            // re-executed — that avoids a duplicate side effect (a second
            // `git push`, a re-sent notification) — and the model is fed a
            // corrective result so it can re-plan. Once it crosses the HARD limit
            // (it kept repeating even after being told to stop) the turn is broken
            // with a `loop-detected` stop. Logged so loops are greppable.
            let repeat_count = repeat_guard.record(&call.name, &call.args);
            let repeat = crate::loopguard::repeat_action(repeat_count);
            if !matches!(repeat, crate::loopguard::RepeatAction::Allow) {
                eprintln!(
                    "\x1b[2m  ⚠ {}\x1b[0m",
                    crate::loopguard::repeat_log_line(&desc, repeat_count, repeat)
                );
                let result = ToolResult::text(
                    call.id.clone(),
                    crate::loopguard::blocked_result_text(&desc, repeat_count),
                    true,
                );
                if session.raw_tool_output {
                    print_raw_result(&result);
                }
                if let Some(w) = session.worker_transcript.as_mut() {
                    w.record_tool_call(&call.id, &call.name, &call.args);
                    w.record_tool_result(&call.id, &call.name, &result.content, result.is_error);
                }
                session.last_turn_tools.push((desc.clone(), result.clone()));
                results.push(result);
                if matches!(repeat, crate::loopguard::RepeatAction::Break) {
                    loop_break = Some((desc.clone(), repeat_count));
                }
                continue;
            }

            // Phase 2 blocking gate: PreToolUse. Every matching hook runs
            // SEQUENTIALLY (most-restrictive-wins); the first `Deny` VETOES the
            // call BEFORE it executes. The veto is threaded through the SAME
            // synthetic "declined" ToolResult path a human decline uses, so the
            // model handles a hook veto identically — it sees an error result and
            // re-plans. An observe-style PreToolUse hook (a logger that exits 0)
            // returns Allow and falls through to normal execution, subsuming the
            // old fire-and-forget observe behavior (now synchronous, bounded by
            // the hook's timeout). Zero-cost when no PreToolUse hook is registered
            // (`has` short-circuits before any payload is built). See crate::hooks.
            if session.hooks.has(crate::hooks::HookEvent::PreToolUse) {
                let p = tool_hook_payload(session, crate::hooks::HookEvent::PreToolUse, call);
                if let crate::hooks::Decision::Deny(reason) = session
                    .hooks
                    .evaluate(crate::hooks::HookEvent::PreToolUse, p)
                    .await
                {
                    eprintln!("\x1b[2m  ⛔ {} — hook denied: {reason}\x1b[0m", desc);
                    let result = ToolResult::text(
                        call.id.clone(),
                        format!("Blocked by a PreToolUse hook: {reason}"),
                        true,
                    );
                    if session.raw_tool_output {
                        print_raw_result(&result);
                    }
                    if let Some(w) = session.worker_transcript.as_mut() {
                        w.record_tool_call(&call.id, &call.name, &call.args);
                        w.record_tool_result(
                            &call.id,
                            &call.name,
                            &result.content,
                            result.is_error,
                        );
                    }
                    // Observe hook: PermissionDenied — a hook vetoed the call.
                    // Fire-and-forget so an audit sink records the veto (carrying
                    // the reason). Zero-cost when unconfigured.
                    if session.hooks.has(crate::hooks::HookEvent::PermissionDenied) {
                        let dp = tool_hook_payload(
                            session,
                            crate::hooks::HookEvent::PermissionDenied,
                            call,
                        )
                        .with("reason", reason.clone());
                        session
                            .hooks
                            .fire_observe(crate::hooks::HookEvent::PermissionDenied, dp);
                    }
                    session.last_turn_tools.push((desc.clone(), result.clone()));
                    results.push(result);
                    continue;
                }
            }

            // Tier-1 turn audit (background coordinator only — None otherwise).
            // Ask the journal whether this exact tool call was already completed
            // in a prior, crashed run: a Replay short-circuits execution and
            // feeds the model the RECORDED result (no duplicate side effect); an
            // Execute journals a `pending` record and runs the tool live. The
            // borrow ends with this expression, so `execute` can re-borrow below.
            let audit_step = session
                .turn_audit
                .as_mut()
                .map(|a| a.begin(&call.name, &call.args));

            let mut result =
                if let Some(crate::turn_audit::Step::Replay { output, is_error }) = &audit_step {
                    // Resumed turn: surface a dim replay marker (no spinner, no
                    // re-execution) and reuse the journaled result.
                    let (output, is_error) = (output.clone(), *is_error);
                    eprintln!("\x1b[2m  \u{21ba} replayed {desc}\x1b[0m");
                    ToolResult::text(call.id.clone(), output, is_error)
                } else {
                    // Live turn. Tool-execution phase: this call gets its own animated
                    // line while it runs — a braille spinner turning to the LEFT of a
                    // steady tool glyph. Calls execute sequentially, so only the
                    // current one animates; the spinner is replaced by a final static
                    // line (✓/✗ + desc) when the tool returns. `run_interactive` hands
                    // the terminal to a child, so it opts out of the animation (which
                    // would fight the child for stderr) and shows a plain static line.
                    let tool_spin = ToolSpinner::start(&desc, animates(&call.name));
                    // Pause the animation around any permission prompt: the spinner
                    // must not animate (or overwrite the prompt) while we wait for the
                    // answer, then it resumes for the tool's actual execution.
                    let stopper = tool_spin.stopper();
                    let mut gated = |p: &str| {
                        pause_spinner(&stopper);
                        let decision = confirm(p);
                        resume_spinner(&stopper);
                        decision
                    };
                    let result = tools::execute(call, session, &mut gated).await;
                    tool_spin.finish(&desc, result.is_error);
                    // Journal the terminal (complete/failed) record for this live turn.
                    if let Some(crate::turn_audit::Step::Execute { turn }) = audit_step {
                        if let Some(a) = session.turn_audit.as_mut() {
                            a.complete(turn, &call.name, &result);
                        }
                    }
                    result
                };
            // Phase 3.4 runtime enforcement: if this result declared an
            // `output_schema`, validate its structured payload against the
            // plugin's named schema. Fail-open — a violation is logged and
            // annotated for the model, never blocking the payload. Zero-cost
            // (no plugin discovery) when no schema was declared: the common case.
            validate_output_schema(&mut result);
            // Tally this completed tool call and paint the interactive activity
            // stream: up to the last 5 lines of the call's output, then a running
            // status line (tokens in/out, tool calls, turns). Mirrors how an
            // escalation is surfaced — a ✓/✗ header (already drawn by the spinner
            // finish above) with a short streamed tail beneath it.
            session.tool_calls_total = session.tool_calls_total.saturating_add(1);
            // Tool-call failure & fallback telemetry: classify a failure, detect
            // a retry of a previously-failed tool, and record whether the retry
            // recovered. Best-effort — never sinks the turn. See tool_telemetry.
            crate::tool_telemetry::record(session, &call.name, &result);
            emit_activity_stream(session, &result);
            // Observe hooks: PostToolUse (always) + PostToolUseFailure (on error).
            // Carry the tool name, program/path, and the error flag so an audit
            // sink sees the outcome of every call. Zero-cost when unconfigured.
            if session.hooks.has(crate::hooks::HookEvent::PostToolUse) {
                let p = tool_hook_payload(session, crate::hooks::HookEvent::PostToolUse, call)
                    .with("is_error", result.is_error);
                session
                    .hooks
                    .fire_observe(crate::hooks::HookEvent::PostToolUse, p);
            }
            if result.is_error
                && session
                    .hooks
                    .has(crate::hooks::HookEvent::PostToolUseFailure)
            {
                let p =
                    tool_hook_payload(session, crate::hooks::HookEvent::PostToolUseFailure, call)
                        .with("is_error", true);
                session
                    .hooks
                    .fire_observe(crate::hooks::HookEvent::PostToolUseFailure, p);
            }
            if session.raw_tool_output {
                print_raw_result(&result);
            }
            // S9.3: persist this tool turn (call + result) to the per-worker
            // transcript so :attach/resume can replay the full turn-by-turn
            // history (coordinator run only — None/no-op interactively). The
            // input is redacted inside record_tool_call (AC8).
            if let Some(w) = session.worker_transcript.as_mut() {
                w.record_tool_call(&call.id, &call.name, &call.args);
                w.record_tool_result(&call.id, &call.name, &result.content, result.is_error);
            }
            session.last_turn_tools.push((desc, result.clone()));
            results.push(result);
        }
        eprintln!(); // breathing room between tool activity and what follows
        session.history.push(Msg::tool_results(results));

        // Confirmed loop this round → stop the turn with a tagged partial answer,
        // so the work isn't silently spun away and the coordinator can decide a
        // recovery disposition.
        if let Some((call_desc, count)) = loop_break {
            let reason = crate::loopguard::ExitReason::LoopDetected {
                call: call_desc,
                count,
            };
            eprintln!("\x1b[2maish: {}\x1b[0m", reason.log_line());
            let partial = if turn.text.trim().is_empty() {
                "Stopped by the loop guard before a final answer was produced.".to_string()
            } else {
                turn.text.clone()
            };
            return Ok(crate::loopguard::with_banner(&reason, &partial));
        }

        // TASK-358: deep serial chain this turn → graceful yield with a resumable
        // banner. Fires AFTER the round's tool_results are appended (history is
        // well-formed) and after the loop_break check (a confirmed loop is the
        // more specific stop and takes precedence). The coordinator classifies
        // `serial-chain-yield` as a Resume disposition, so the next round picks up
        // from the checkpoint and re-plans toward batching independent calls.
        if let Some(depth) = serial_yield {
            let reason = crate::loopguard::ExitReason::SerialChainYield { depth };
            eprintln!("\x1b[2maish: {}\x1b[0m", reason.log_line());
            
            // Invoke the advisor to evaluate the serial-chain pattern and optionally
            // inject a resume directive based on whether this is a batching opportunity
            // or a stuck pattern. The advisor reads the turn-audit (if available).
            let turns_audit = build_turns_audit_from_history(&session.history);
            let advice = crate::advisor::SerialYieldAdvisor::evaluate(&turns_audit);
            
            eprintln!(
                "\x1b[2m  → advisor: {} — {}\x1b[0m",
                match advice.classification {
                    crate::advisor::YieldClassification::BatchingOpportunity => "batching-opportunity",
                    crate::advisor::YieldClassification::StuckPattern => "stuck-pattern",
                    crate::advisor::YieldClassification::Unknown => "unknown",
                },
                advice.summary
            );
            
            let mut partial = if turn.text.trim().is_empty() {
                format!(
                    "Yielded after a deep serial call chain ({depth} consecutive single-call \
rounds) to re-plan toward batching independent calls."
                )
            } else {
                turn.text.clone()
            };
            
            // If the advisor has a resume directive (for batching opportunities),
            // append it to guide the next round's planning AND route it into the
            // binding nudge channel (TASK-324) so the next round's system prompt
            // injects it, making it binding (not advisory-only). This ensures a
            // smaller/faster model cannot ignore the directive.
            if let Some(directive) = &advice.resume_directive {
                partial.push('\n');
                partial.push('\n');
                partial.push_str(directive);
                
                // TASK-358 AC1: reuse the TASK-324 batching nudge carrier to make
                // this directive binding. The next iteration will consume
                // pending_batch_nudge.take() at line 453 and inject it into
                // effective_system, forcing the model to see it in the system prompt.
                if matches!(
                    advice.classification,
                    crate::advisor::YieldClassification::BatchingOpportunity
                ) {
                    #[allow(unused_assignments)]
                    {
                        pending_batch_nudge = Some(directive.clone());
                    }
                    eprintln!(
                        "\x1b[2m  → routing resume_directive to batching-nudge channel for binding injection\x1b[0m"
                    );
                }
            }
            
            // TASK-358 AC2: when a stuck pattern is detected, escalate to operator.
            // The advisor has classified this as StuckPattern (not a batching
            // opportunity), meaning the model is stuck in a non-productive loop.
            // Emit an alert for the operator with the turn audit so they can
            // investigate the root cause and potentially adjust the prompt or
            // add guardrails.
            if matches!(
                advice.classification,
                crate::advisor::YieldClassification::StuckPattern
            ) {
                eprintln!(
                    "\x1b[91m  → ESCALATING stuck pattern to operator\x1b[0m"
                );
                // Log the turn count and pattern summary for operator review.
                // Future: emit via atum notification API for real-time alerts.
                eprintln!(
                    "\x1b[91m  → Turn audit: {} turns, pattern: {}\x1b[0m",
                    turns_audit.len(),
                    advice.summary
                );
            }
            
            return Ok(crate::loopguard::with_banner(&reason, &partial));
        }

        // TASK-357: cumulative per-turn call budget hit its HARD cap → graceful
        // yield. Deferred to here (same rationale as `serial_yield`): the round's
        // tool_results are already appended so history is well-formed, and the two
        // more-specific stops above (loop, serial-chain) take precedence. The
        // coordinator classifies `call-budget-exceeded` as Resume, so the next
        // round continues from this checkpoint with fresh context.
        if let crate::loopguard::CallBudgetAction::HardYield { count } = call_budget {
            let reason = crate::loopguard::ExitReason::CallBudgetExceeded { count };
            eprintln!("\x1b[2maish: {}\x1b[0m", reason.log_line());
            let partial = if turn.text.trim().is_empty() {
                format!(
                    "Yielded after {count} tool calls this turn (per-turn hard budget) to spread \
load across the rate-limit window and resume with fresh context."
                )
            } else {
                turn.text.clone()
            };
            return Ok(crate::loopguard::with_banner(&reason, &partial));
        }
    }

    // Hard backstop. With the forced-summarize step firing at FORCE_SUMMARIZE_PCT
    // this is effectively unreachable, but keep a tagged exit so a future budget
    // change can't silently resurrect the old "throw the work away" stop.
    let reason = crate::loopguard::ExitReason::BudgetExhausted {
        iterations: MAX_ITERATIONS,
    };
    eprintln!("\x1b[2maish: {}\x1b[0m", reason.log_line());
    Ok(crate::loopguard::with_banner(
        &reason,
        "Stopped after exhausting the tool-call iteration budget without a final answer.",
    ))
}

/// Headless background coordinator: the durable, resumable multi-round loop
/// (see `crate::coordinator`). Runs full-tool agentic rounds and, when a round
/// fans heavy sub-work out to the Anthropic Batches API, awaits it (phase
/// `awaiting_batch`, heartbeating) and folds the results into the next round —
/// persisting each phase transition to the `coordinator_runs` store so a crash
/// resumes. Confirmation is auto-allowed: the caller runs us unattended (yolo,
/// no TTY). `run_id` keys the durable row.
///
/// This is the body of `aish --coordinator`, which is now the DEFAULT background
/// path (`run_in_background` spawns one of these); the tool-less Batches offload
/// is an internal optimization a round can reach for, not a user-facing mode.
pub async fn run_coordinator(
    backend: &Backend,
    session: &mut Session,
    input: String,
    run_id: &str,
) -> Result<()> {
    eprintln!("\x1b[2maish: coordinator run {run_id} starting\x1b[0m");
    let store = session.coordinator_store.clone();
    let outcome = crate::coordinator::drive(backend, session, input, run_id, store.as_ref()).await;
    match outcome.phase {
        crate::coordinator::Phase::Done => {
            if let Some(result) = &outcome.result {
                if session.output_json {
                    println!("{}", crate::json_ok(result));
                } else {
                    println!("{}", crate::md::render_stdout(result));
                }
            }
            Ok(())
        }
        // A checkpointed run is a deliberate, resumable PAUSE (TASK-294), NOT a
        // failure: emit any partial result and exit 0 so the parent doesn't mark
        // it failed. The durable row stays at `checkpoint` for a later resume.
        crate::coordinator::Phase::Checkpoint => {
            if let Some(result) = &outcome.result {
                if session.output_json {
                    println!("{}", crate::json_ok(result));
                } else {
                    println!("{}", crate::md::render_stdout(result));
                }
            }
            Ok(())
        }
        // A failed run prints its error to stdout (the worker captures stdout as
        // the result) and propagates a non-zero exit so the parent marks it
        // failed. The durable row already records the failure for rehydrate. In
        // `--output json` mode the error is emitted as a structured object first,
        // so a driving agent parses `{"ok":false,"error":"…"}` instead of a bare
        // anyhow chain on stderr.
        _ => {
            let err = outcome.error.unwrap_or_else(|| "coordinator failed".into());
            if session.output_json {
                println!("{}", crate::json_ok_err(&err));
            }
            anyhow::bail!("{err}")
        }
    }
}

/// Before a new turn, compact the conversation when it has grown past the
/// context-window threshold: offload the oldest slice to the SQLite `memories`
/// table (recoverable via the `recall` tool, tagged `context-offload`) and
/// replace it in-context with a short summary message. Keeps long, agentic
/// sessions — interactive and headless coordinator alike — from overflowing the
/// model's window. A no-op until usage is known (`context_used` > 0) and the
/// conversation is long enough to split on a safe assistant boundary.
fn maybe_compact(backend: &Backend, session: &mut Session) {
    let budget = crate::context::CompactBudget {
        window: backend.context_window(),
        threshold_pct: crate::context::COMPACT_THRESHOLD_PCT,
        tool_call_ceiling: session.compact_tool_call_ceiling,
        token_ceiling: session.compact_token_ceiling,
    };
    // In-context tool calls = session total minus the watermark set at the last
    // compaction (the calls the retained transcript still carries). (TASK-321)
    let tool_calls_in_context = session
        .tool_calls_total
        .saturating_sub(session.tool_calls_at_last_compact);
    let Some(trigger) = budget.trigger(session.context_used, tool_calls_in_context) else {
        return;
    };
    let Some(plan) =
        crate::context::plan_compaction(&session.history, crate::context::KEEP_RECENT_MSGS)
    else {
        return;
    };
    // Offload the dropped transcript to the dedicated offloads table BEFORE
    // mutating history, so nothing is lost even if the process dies right after.
    // Stored OUT of `memories` so a routine recall of curated facts never drags
    // the (potentially MB-scale) transcript along; recoverable via `recall` with
    // the `context-offload` tag.
    if let Some(db) = &session.db {
        let _ = db.remember_offload(&plan.offload);
    }
    let dropped = plan.dropped;
    crate::context::apply_compaction(&mut session.history, &plan);
    // Exact next-turn usage isn't known yet; re-seat the figure from an estimate.
    session.context_used = crate::context::estimate_history_tokens(&session.history);
    // Re-seat the tool-call watermark so the in-context count reflects only the
    // calls the retained transcript still carries. (TASK-321)
    session.tool_calls_at_last_compact = session
        .tool_calls_total
        .saturating_sub(crate::context::count_tool_calls(&session.history));
    eprintln!(
        "\x1b[2maish: {} tripped — compacted {dropped} earlier message(s) to memory\x1b[0m",
        trigger.label()
    );
}

/// Print the model's interim narration. In an interactive session it goes out
/// plainly. In a background coordinator (`session.nested`) each line is tagged
/// with a `🗨` sentinel so the parent's worker stream can recognize it as *turn*
/// output (vs `🔧` tool lines) and forward it only when `:worker-output` is on.
/// A coordinator turn is always a standard (Messages API) model call, hence the
/// `[standard]` label the parent attaches; batch fan-out is announced separately.
fn emit_narration(session: &mut Session, text: &str) {
    // In a coordinator each rendered line is re-framed by the parent as a pane
    // row (`┃ [label] …`); render tables/rules narrow enough to survive that
    // gutter so the parent's terminal doesn't hard-wrap the box. Interactively
    // there's no gutter — render at full terminal width.
    let rendered = if session.nested {
        crate::md::render_pane(text.trim(), "")
    } else {
        crate::md::render(text.trim(), "")
    };
    if session.nested {
        for line in rendered.lines() {
            eprintln!("🗨 {line}");
        }
    } else {
        eprintln!("{rendered}");
    }
    // S9.3: persist the model’s interim reasoning to the per-worker
    // transcript (coordinator run only — None/no-op interactively) so a replay
    // shows what the agent SAID between tool calls, not just what it did.
    if let Some(w) = session.worker_transcript.as_mut() {
        w.record_message("assistant", "narration", text);
    }
}

/// Signal the start of the model-reasoning ("thinking") phase. An interactive
/// session shows the live `Spinner` (TTY-only) instead, so this fires only in a
/// background coordinator (`session.nested`), where it emits a `💭` sentinel the
/// parent's worker stream recognizes and surfaces as `[label] thinking…` when
/// `:worker-output` is on. It lets the user see the agent is reasoning between
/// tool calls, not just its `🔧` tool activity. One line per round; the
/// interactive path is untouched (the `Spinner` still owns stderr there).
fn emit_thinking(session: &Session) {
    if session.nested {
        eprintln!("💭 thinking…");
    }
}

/// Paint the interactive activity stream beneath a just-finished tool call:
/// a single collapsed summary line — how many lines the call produced plus a
/// `Ctrl-O to expand` hint (instead of echoing the last few output lines) —
/// followed by a running status line: `tokens in/out`, the cumulative `tool
/// calls`, and `turns`. This is the terminal-only companion to the ✓/✗ header
/// the [`ToolSpinner`] already drew on finish. The full output is one keystroke
/// away: Ctrl-O expands every tool result verbatim ([`reveal_last_turn`]) and a
/// second Ctrl-O collapses back to this summary ([`collapse_last_turn`]). Silent
/// unless stderr is a TTY (so `aish -c` piped mode and non-tty background
/// coordinators stay clean), and skipped when `raw_tool_output` is on (the full
/// result is already being printed verbatim).
fn emit_activity_stream(session: &Session, result: &ToolResult) {
    if !stderr_is_tty() || session.raw_tool_output {
        return;
    }
    let cols = stderr_cols();
    // Count against the same body Ctrl-O would reveal so the summary and the
    // expanded view agree (raw_body substitutes a placeholder / pretty JSON for
    // an empty `content`).
    let line_count = raw_body(result).lines().count();
    for line in activity_stream_lines(line_count) {
        eprintln!("\x1b[2m{}\x1b[0m", truncate_to_cols(&line, cols));
    }
}

/// Compact running session stats for the main statusline — `tokens in/out`,
/// cumulative tool calls, and turns — placed to the LEFT of the clock. Returns
/// an empty string for a fresh session that has done nothing yet, so the
/// statusline stays clean until the first turn/tool call.
pub fn statusline_stats(session: &Session) -> String {
    if session.tokens_in == 0
        && session.tokens_out == 0
        && session.tool_calls_total == 0
        && session.turns_total == 0
    {
        return String::new();
    }
    // TASK-320 AC#3: append a session-level cache-read hit rate when the backend
    // reported any cache activity — the numerator is cumulative cached-read
    // tokens, the denominator the full input tally (which already includes the
    // cached prefix). Omitted for cache-less sessions so the line stays clean.
    let cache = if session.cache_read_total > 0 && session.tokens_in > 0 {
        let pct = (session.cache_read_total as f64 / session.tokens_in as f64) * 100.0;
        format!(", cache: {pct:.0}% hit")
    } else {
        String::new()
    };
    format!(
        "tokens: {} in / {} out, tool calls: {}, turns: {}{}",
        session.tokens_in, session.tokens_out, session.tool_calls_total, session.turns_total, cache
    )
}

/// Pure builder for the interactive activity-stream line beneath a tool header:
/// a collapsed `N lines of output — Ctrl-O to expand` summary (omitted when the
/// call produced nothing). Indented to column 5 so it nests under the ✓/✗ tool
/// header and lines up with the expanded body ([`reveal_last_turn`]) — Ctrl-O
/// toggling no longer shifts it a column. The running token/tool/turn stats now
/// live on the main statusline ([`statusline_stats`]), not here. Kept free of
/// ANSI/terminal I/O so the format contract is unit-testable; the caller dims +
/// width-clamps each line.
fn activity_stream_lines(line_count: usize) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    if line_count > 0 {
        let noun = if line_count == 1 { "line" } else { "lines" };
        lines.push(format!(
            "     … {line_count} {noun} of output — Ctrl-O to expand"
        ));
    }
    lines
}

/// When no INSTALLED skill matched a substantial task, fold in an OFFLINE
/// recommendation of an installable registry skill (read from the binary-shipped
/// index — `skill_provider::local_index_catalog`, no network on the hot path).
/// Deduped per session via `session.skill_suggested`, so the same skill is only
/// suggested once. A no-op (returns `input` unchanged) when the task is trivial,
/// nothing in the catalog clears the relevance bar, or it was already suggested.
fn maybe_recommend_skill(task: &str, input: String, session: &mut Session) -> String {
    if !crate::skill_match::is_skill_worthy(task) {
        return input;
    }
    let catalog = crate::skill_provider::local_index_catalog();
    match crate::skill_match::recommend_install(task, &session.skills, &catalog) {
        Some(rec) if session.skill_suggested.insert(rec.reference.clone()) => {
            format!("{}\n\n{input}", rec.note)
        }
        _ => input,
    }
}

/// TASK-13: prepend the previous recorded output to a turn's input so the model
/// can reference it ("summarize that") without re-running the command. Applied
/// only when the conversation is empty — mid-conversation the output is already
/// in `history`, and an empty/whitespace previous output is left untouched.
fn seed_context(history_empty: bool, prev: Option<String>, input: String) -> String {
    match prev {
        Some(prev) if history_empty && !prev.trim().is_empty() => {
            format!("[Previous command output, for reference:\n{prev}\n]\n\n{input}")
        }
        _ => input,
    }
}

/// True when stderr is a terminal — the gate for every animation/ANSI line
/// here. Mirrors `md::render_stdout`'s isatty(1) check, but on fd 2 since all
/// transient activity goes to stderr. In `aish -c` piped mode this is false,
/// so no spinner/animation escape codes ever reach the output.
fn stderr_is_tty() -> bool {
    // SAFETY: plain isatty query.
    unsafe { libc::isatty(2) == 1 }
}

/// Width in columns of the stderr terminal, used to keep an animated activity
/// line on a SINGLE physical row. The transient redraw is `\r` + `\x1b[2K`
/// (carriage-return + erase-line), which only returns to and clears the row the
/// cursor is on. A line longer than the terminal wraps onto extra rows, the
/// redraw clears just the last of them, and the rest stay on screen — so each
/// frame scrolls a fresh copy instead of animating in place. Queried via
/// TIOCGWINSZ on fd 2; falls back to `$COLUMNS`, then a conservative 80.
fn stderr_cols() -> usize {
    // SAFETY: a read-only TIOCGWINSZ ioctl on fd 2.
    unsafe {
        let mut ws: libc::winsize = std::mem::zeroed();
        if libc::ioctl(2, libc::TIOCGWINSZ, &mut ws) == 0 && ws.ws_col > 0 {
            return ws.ws_col as usize;
        }
    }
    std::env::var("COLUMNS")
        .ok()
        .and_then(|c| c.parse().ok())
        .unwrap_or(80)
}

/// Truncate `s` to at most `max` terminal columns (Unicode *display* width, so a
/// wide glyph like 🔧 counts as two), appending an ellipsis when it overflows.
/// This is what keeps the animated tool line to one physical row: the desc is a
/// full command line that easily exceeds the terminal, and a wrapped line breaks
/// the `\r`+erase redraw (see `stderr_cols`). A line that already fits is
/// returned unchanged.
fn truncate_to_cols(s: &str, max: usize) -> String {
    use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};
    if s.width() <= max {
        return s.to_string();
    }
    if max == 0 {
        return String::new();
    }
    let budget = max.saturating_sub(1); // leave a column for the ellipsis
    let mut out = String::new();
    let mut w = 0usize;
    for ch in s.chars() {
        let cw = ch.width().unwrap_or(0);
        if w + cw > budget {
            break;
        }
        out.push(ch);
        w += cw;
    }
    out.push('…');
    out
}

/// Transient "⠋ thinking…" line on stderr while the model is working — the
/// model-reasoning phase indicator. TTY-gated; erased on drop (first token,
/// tool call, or turn abort).
struct Spinner(Option<tokio::task::JoinHandle<()>>);

impl Spinner {
    fn start() -> Self {
        if !stderr_is_tty() {
            return Self(None);
        }
        eprint!("\x1b[?25l"); // hide the cursor while thinking; restored on drop
        Self(Some(tokio::spawn(async {
            const FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
            let mut tick = tokio::time::interval(std::time::Duration::from_millis(80));
            for i in 0.. {
                tick.tick().await;
                eprint!(
                    "\r\x1b[36m{}\x1b[0m \x1b[2;36mthinking…\x1b[0m",
                    FRAMES[i % FRAMES.len()]
                );
            }
        })))
    }
}

impl Drop for Spinner {
    fn drop(&mut self) {
        if let Some(h) = self.0.take() {
            h.abort();
            eprint!("\r\x1b[2K\x1b[?25h"); // erase the spinner line + restore the cursor
        }
    }
}

/// Running-tool indicator: a braille spinner turning to the LEFT of a steady
/// tool glyph while the tool executes — the tool-execution phase, distinct from
/// the model's "thinking" spinner. The glyph stays put (it's *our* tool marker:
/// 🛠️ local · 🔧 MCP · 🤝 escalate) and only the spinner to its
/// left animates, mirroring the look of the thinking icon (same braille frames,
/// cyan glyph). Keeps the dim, two-space-indented style of the static line it
/// replaces. TTY-gated; on `finish` the animation is erased and a static result
/// line (✓/✗ + desc) is printed in its place.
struct ToolSpinner {
    /// Shared animation state (Running/Paused/Stopped). `stopper()` hands a clone
    /// to the confirm wrapper so it can pause/resume around the prompt.
    state: SpinState,
    /// The animation task — aborted by `finish`/`Drop` once stopped.
    task: Option<tokio::task::JoinHandle<()>>,
    /// True when we actually animated (vs the static piped/non-animating line).
    animated: bool,
}

/// Frames for the running-tool spinner — the same braille cycle as the
/// "thinking" indicator, drawn to the LEFT of a steady tool glyph so the glyph
/// stays put while the spinner turns.
const TOOL_FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// Columns consumed to the LEFT of the desc on an animated tool line: the
/// two-space indent (2) + the braille spinner glyph (1) + the space after it (1).
/// The desc is truncated to `stderr_cols() - PREFIX_COLS` so the whole line fits
/// one physical row.
const PREFIX_COLS: usize = 4;

/// The steady glyph shown to the RIGHT of the animated braille spinner on a
/// tool-activity line. Local tool/exe/script calls use the 🛠️ hammer-and-wrench;
/// MCP calls use the 🔧 wrench; `escalate` uses a 🤝
/// handshake so a consult reads as its own distinct event — a collaborative
/// hand-off with the live "thinking" spinner to its left — rather than looking
/// like just another tool call. Baked into the desc at the call site so it
/// travels through the running spinner, the ✓/✗ finish line, and the reveal.
/// The handshake is double-width like the wrench, so it needs no trailing-space
/// spacer: the call site joins it to the desc with a single space.
fn tool_glyph(tool_name: &str) -> &'static str {
    let n = tool_name.trim().to_ascii_lowercase();
    // An `escalate` hand-off keeps its 🤝 handshake so a consult reads as its
    // own collaborative event. MCP tool calls (`mcp__<server>__<tool>`, plus the
    // bare `mcp_`/`atum_` shorthands) keep the 🔧 wrench. Every other local
    // tool/exe/script call uses the 🛠️ hammer-and-wrench — exactly ONE source
    // glyph per line, so an activity line never carries two.
    if n == "escalate" {
        return "🤝"; // handshake — a consult, distinct from the wrench
    }
    if n.starts_with("mcp__") || n.starts_with("mcp_") || n.starts_with("atum_") {
        return "🔧"; // MCP tool call — the wrench
    }
    "🛠️ " // local tool/exe/script — hammer & wrench (VS16 emoji presentation)
    // + a trailing spacer: U+1F6E0 renders width-1 in most terminals, so the
    // single join-space looks swallowed; the extra space keeps a clear gap.
}

/// Whether a tool's execution should be animated. Tools that hand the terminal
/// to a child (interactive sessions) must not animate — the spinner's cursor
/// rewrites would fight the child for the screen — so they show a static line.
fn animates(tool_name: &str) -> bool {
    tool_name != "run_interactive"
}

impl ToolSpinner {
    fn start(desc: &str, animate: bool) -> Self {
        if !animate || !stderr_is_tty() {
            // Piped/headless or non-animating tool: emit the plain static line
            // once, no animation. The glyph is already part of `desc`.
            eprintln!("\x1b[2m  {desc}\x1b[0m");
            return Self {
                state: Arc::new(Mutex::new(Spin::Stopped)),
                task: None,
                animated: false,
            };
        }
        eprint!("\x1b[?25l"); // hide the cursor so it doesn't blink at the spinner's tail
        let state = Arc::new(Mutex::new(Spin::Running));
        let s = state.clone();
        // Clamp the desc to a single physical row. A wrapped line would break the
        // per-frame `\r`+erase redraw (it only clears the cursor's row), making
        // the spinner scroll a fresh copy every frame instead of animating in
        // place. `-1` leaves the last column empty so a desc that exactly fills
        // the width doesn't trip the terminal's deferred-wrap at the margin.
        let budget = stderr_cols().saturating_sub(PREFIX_COLS + 1);
        let desc = truncate_to_cols(desc, budget);
        let task = tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_millis(80));
            // Consume the immediate first tick so no frame draws for the first
            // ~80ms — long enough for a permission gate to pause us before the
            // spinner ever appears, so it never animates over the prompt.
            tick.tick().await;
            for i in 0.. {
                tick.tick().await;
                let g = s.lock().unwrap();
                match *g {
                    Spin::Stopped => break,
                    Spin::Paused => {} // waiting on a confirm — draw nothing
                    Spin::Running => {
                        // Animated braille spinner (cyan, like "thinking…") to the
                        // left of the steady, dim tool glyph + desc (the glyph is
                        // already baked into `desc`).
                        eprint!(
                            "\r\x1b[2K  \x1b[36m{}\x1b[0m \x1b[2m{desc}\x1b[0m",
                            TOOL_FRAMES[i % TOOL_FRAMES.len()]
                        )
                    }
                }
            }
        });
        Self {
            state,
            task: Some(task),
            animated: true,
        }
    }

    /// A clone of the animation state — the confirm wrapper pauses/resumes it
    /// around a permission prompt.
    fn stopper(&self) -> SpinState {
        self.state.clone()
    }

    /// Stop the animation and leave a static result line behind. On a TTY the
    /// spinning line is erased and replaced in place; piped, the static line was
    /// already printed at `start`, so we only print the result for animated runs.
    fn finish(mut self, desc: &str, is_error: bool) {
        stop_spinner(&self.state);
        if let Some(t) = self.task.take() {
            t.abort();
        }
        if self.animated {
            eprintln!(
                "\r\x1b[2K\x1b[2m  {}\x1b[0m",
                tool_result_line(desc, is_error)
            );
        } else {
            // Non-animated (piped / background coordinator): the dim start line
            // was already printed at `start`; emit the static ✓/✗ result line too
            // so the parent worker stream can pulse the prompt badge on the
            // tool outcome (green success / red failure). On a TTY the animated
            // branch already does the in-place replace.
            eprintln!("\x1b[2m  {}\x1b[0m", tool_result_line(desc, is_error));
        }
    }
}

/// Pause the animation for a confirm prompt: stop drawing, clear the line, and
/// restore the cursor — so the spinner isn't animating while we wait for the
/// user's answer and the prompt is clean. Under the lock, so it can't race a
/// frame. Idempotent.
fn pause_spinner(state: &SpinState) {
    let mut g = state.lock().unwrap();
    if *g == Spin::Running {
        *g = Spin::Paused;
        eprint!("\r\x1b[2K\x1b[?25h");
    }
}

/// Resume the animation after the prompt — the spinner animates again during the
/// tool's actual execution. Idempotent.
fn resume_spinner(state: &SpinState) {
    let mut g = state.lock().unwrap();
    if *g == Spin::Paused {
        *g = Spin::Running;
        eprint!("\x1b[?25l"); // re-hide the cursor; the next tick draws a frame
    }
}

/// Stop the animation for good, clearing the line and restoring the cursor.
/// Idempotent.
fn stop_spinner(state: &SpinState) {
    let mut g = state.lock().unwrap();
    if *g != Spin::Stopped {
        *g = Spin::Stopped;
        eprint!("\r\x1b[2K\x1b[?25h");
    }
}

impl Drop for ToolSpinner {
    fn drop(&mut self) {
        // Only does work when nothing else stopped it (e.g. the turn was aborted
        // mid-tool) — stop the animation and restore the cursor so it's never
        // left hidden.
        stop_spinner(&self.state);
        if let Some(t) = self.task.take() {
            t.abort();
        }
    }
}

/// The static post-execution tool line: a ✓/✗ status glyph plus the (already
/// glyph-prefixed) desc, kept dim like the rest of the activity stream. Shared
/// by `ToolSpinner::finish` and the retroactive `reveal_last_turn`. The tool
/// glyph (🛠️/🔧/🤝) is part of `desc`, so only the colorized status mark is added.
fn tool_result_line(desc: &str, is_error: bool) -> String {
    if is_error {
        format!("\x1b[31m✗\x1b[0m {desc}") // red for error
    } else {
        format!("\x1b[32m✓\x1b[0m {desc}") // green for success
    }
}

/// Collapse a desc to a SINGLE display line: every run of whitespace — including
/// embedded newlines, carriage returns and tabs (a `gh pr create --body`
/// markdown payload, a multi-line `remember` note, an argv with a heredoc) —
/// becomes one space, and the ends are trimmed. Companion to `truncate_to_cols`:
/// a newline has display width 0, so width-based truncation can NOT catch it —
/// an embedded newline survives, and the spinner per-frame CR+erase redraw only
/// clears the cursor row, so every frame scrolls a fresh copy of the desc tail
/// instead of animating in place (the multi-line tool-call bug). Flattening at
/// the source keeps the whole activity line — running spinner, ✓/✗ finish
/// line, and retroactive reveal — on one physical row before `truncate_to_cols`
/// clamps its width.
fn flatten_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Build a `PreToolUse`/`PostToolUse` payload for `call`: the common session
/// envelope plus the tool name and, when present, the spawned program
/// (`run_program`/`run_interactive`) and a path argument (file tools). These are
/// exactly the fields the hook matcher filters on (`tool`/`program`/`path`), so
/// a hook like `{ "tool": "run_program", "program": "git" }` resolves correctly.
/// Only called inside a `hooks.has(...)` guard.
fn tool_hook_payload(
    session: &Session,
    event: crate::hooks::HookEvent,
    call: &crate::backend::ToolCall,
) -> crate::hooks::HookPayload {
    let mut p = session.hook_payload(event).with("tool", call.name.clone());
    if let Some(program) = call.args.get("program").and_then(|v| v.as_str()) {
        p = p.with("program", program.to_string());
    }
    if let Some(path) = call.args.get("path").and_then(|v| v.as_str()) {
        p = p.with("path", path.to_string());
    }
    p
}

fn describe_call(call: &crate::backend::ToolCall) -> String {
    let a = &call.args;
    match call.name.as_str() {
        "run_program" | "run_interactive" => {
            let program = a["program"].as_str().unwrap_or("?");
            let mut args: Vec<&str> = a["args"]
                .as_array()
                .map(|v| v.iter().filter_map(|x| x.as_str()).collect())
                .unwrap_or_default();
            // Mirror tools::dedup_program_argv so the activity line shows the
            // command aish will actually run, not the model's doubled argv.
            if args.first() == Some(&program) {
                args.remove(0);
            }
            let argv = format!("{} {}", program, args.join(" "));
            if call.name == "run_interactive" {
                format!("{} (interactive — your terminal)", argv.trim())
            } else {
                argv.trim().to_string()
            }
        }
        "read_file" => format!("read {}", a["path"].as_str().unwrap_or("?")),
        "write_file" => format!(
            "write {} ({} bytes)",
            a["path"].as_str().unwrap_or("?"),
            a["content"].as_str().map(str::len).unwrap_or(0)
        ),
        "edit_file" => format!(
            "edit {} ({:?})",
            a["path"].as_str().unwrap_or("?"),
            a["pattern"].as_str().unwrap_or("?")
        ),
        "append_file" => format!(
            "append {} ({} bytes)",
            a["path"].as_str().unwrap_or("?"),
            a["content"].as_str().map(str::len).unwrap_or(0)
        ),
        "copy_file" => format!(
            "copy {} → {}",
            a["src"].as_str().unwrap_or("?"),
            a["dst"].as_str().unwrap_or("?")
        ),
        "rename_file" => format!(
            "rename {} → {}",
            a["src"].as_str().unwrap_or("?"),
            a["dst"].as_str().unwrap_or("?")
        ),
        "list_dir" => format!("list {}", a["path"].as_str().unwrap_or(".")),
        "glob_expand" => match a["path"].as_str().filter(|p| !p.is_empty()) {
            Some(p) => format!("glob {} in {p}", a["pattern"].as_str().unwrap_or("?")),
            None => format!("glob {}", a["pattern"].as_str().unwrap_or("?")),
        },
        "grep_files" => format!(
            "grep {:?} in {}",
            a["pattern"].as_str().unwrap_or("?"),
            a["path"].as_str().unwrap_or(".")
        ),
        "stat_file" => format!("stat {}", a["path"].as_str().unwrap_or("?")),
        "diff_files" => match a["b"].as_str().filter(|b| !b.is_empty()) {
            Some(b) => format!("diff {} {b}", a["a"].as_str().unwrap_or("?")),
            None => format!("diff {} (inline)", a["a"].as_str().unwrap_or("?")),
        },
        "change_dir" => format!("cd {}", a["path"].as_str().unwrap_or("?")),
        "remember" => format!("remember: {}", a["content"].as_str().unwrap_or("?")),
        "recall" => format!("recall: {}", a["query"].as_str().unwrap_or("(recent)")),
        "run_in_background" => format!(
            "background: {}",
            crate::batch::one_line(a["task"].as_str().unwrap_or("?"))
        ),
        "background_status" => "background status".to_string(),
        "job_output" => format!(
            "job output: {}",
            a["job"]
                .as_u64()
                .map(|n| n.to_string())
                .unwrap_or_else(|| "?".into())
        ),
        "get_skill" => format!(
            "skill {}/{}",
            a["server"].as_str().unwrap_or("?"),
            a["name"].as_str().unwrap_or("?")
        ),
        "tell" => format!(
            "tell {}: {}",
            a["id"].as_str().unwrap_or("?"),
            crate::batch::one_line(a["message"].as_str().unwrap_or("?"))
        ),
        "escalate" => format!(
            "escalate: {}",
            crate::batch::one_line(a["task"].as_str().unwrap_or("?"))
        ),
        other => other.to_string(),
    }
}

/// The text echoed (dim) for a tool result's raw body under Ctrl-O — the **raw**
/// half of the S7.3 split. Deliberately INDEPENDENT of what the model receives
/// (`ToolResult::model_content`): the model gets the structured JSON payload,
/// while the human always sees the verbatim, human-readable tool output here.
/// `content` is that verbatim rendering for every tool (S7.2 kept it
/// byte-for-byte), so a structured tool's Ctrl-O view stays its aligned text —
/// never a JSON dump (OQ2). Empty results get a placeholder so an error with no
/// output still shows *something*; a (rare) structured-only result — empty
/// `content` but a payload present — falls back to its pretty-printed JSON
/// rather than the bare "(no output)" (FR4).
fn raw_body(result: &ToolResult) -> String {
    if !result.content.trim().is_empty() {
        return result.content.clone();
    }
    if let Some(v) = &result.structured {
        if let Ok(pretty) = serde_json::to_string_pretty(v) {
            return pretty;
        }
    }
    "(no output)".to_string()
}

/// Echo one tool result's raw content dim, nested under its 🔧 line. Printed
/// verbatim and never truncated — squelching (Ctrl-O) is the size control.
fn print_raw_result(result: &ToolResult) {
    for line in raw_body(result).lines() {
        eprintln!("\x1b[2m     {line}\x1b[0m");
    }
}

/// Build the most recent turn's tool calls and their raw results as EXPANDED,
/// fully-formatted (dim, indented) lines — each tool's ✓/✗ header followed by
/// its verbatim output body. The reveal half of the Ctrl-O toggle. Returns the
/// lines rather than printing them so [`render_raw_toggle`] can measure them for
/// in-place erasure.
pub fn reveal_last_turn(session: &Session) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for (desc, result) in &session.last_turn_tools {
        out.push(format!("\x1b[2m  {}\x1b[0m", tool_result_line(desc, result.is_error)));
        for line in raw_body(result).lines() {
            out.push(format!("\x1b[2m     {line}\x1b[0m"));
        }
    }
    out
}

/// Build the most recent turn's tool calls in COLLAPSED form — each tool's ✓/✗
/// header followed by a one-line `N lines of output — Ctrl-O to expand` summary
/// rather than the verbatim body. The symmetric counterpart to
/// [`reveal_last_turn`]: toggling raw output back OFF re-collapses the reveal so
/// Ctrl-O flips between the expanded and collapsed views of the same turn.
pub fn collapse_last_turn(session: &Session) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for (desc, result) in &session.last_turn_tools {
        out.push(format!("\x1b[2m  {}\x1b[0m", tool_result_line(desc, result.is_error)));
        let count = raw_body(result).lines().count();
        if count > 0 {
            let noun = if count == 1 { "line" } else { "lines" };
            out.push(format!(
                "\x1b[2m     … {count} {noun} of output — Ctrl-O to expand\x1b[0m"
            ));
        }
    }
    out
}

/// Display width of `s` with any CSI (`ESC [ … <letter>`) escape sequences
/// stripped, so a dim/coloured line measures by its VISIBLE glyphs only. Wide
/// glyphs count as two columns (via `unicode-width`).
fn ansi_stripped_width(s: &str) -> usize {
    use unicode_width::UnicodeWidthStr;
    let mut clean = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            if chars.next() == Some('[') {
                for c2 in chars.by_ref() {
                    if c2.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
            continue;
        }
        clean.push(c);
    }
    clean.width()
}

/// How many physical terminal rows a single logical (newline-free) line
/// occupies once the terminal soft-wraps it at `cols`. A blank/zero-width line
/// still occupies one row. `cols == 0` (unknown width) degrades to 1 row.
fn physical_rows(formatted_line: &str, cols: usize) -> usize {
    if cols == 0 {
        return 1;
    }
    let w = ansi_stripped_width(formatted_line);
    w.div_ceil(cols).max(1)
}

/// Build a turns audit from the conversation history for the advisor.
/// Extracts (round_number, tool_names, file_paths) tuples from the last ~10
/// assistant messages, which the advisor uses to detect batching opportunities
/// or stuck patterns.
fn build_turns_audit_from_history(history: &[Msg]) -> Vec<(usize, Vec<String>, Vec<String>)> {
    let mut turns = Vec::new();
    let mut round = 1;
    
    // Walk backwards through history, collecting assistant messages with tool calls.
    for msg in history.iter().rev().take(40) {
        if msg.role == Role::Assistant && !msg.tool_calls.is_empty() {
            let tool_names: Vec<String> = 
                msg.tool_calls.iter().map(|c| c.name.clone()).collect();
            
            // Extract file paths from tool arguments (heuristic: look for
            // "path", "file_path", "pattern" fields that look like file paths).
            let mut file_paths = Vec::new();
            for call in &msg.tool_calls {
                if let Some(args) = call.args.as_object() {
                    for (key, val) in args {
                        if (key.contains("path") || key.contains("file") || key == "pattern")
                            && val.is_string() 
                        {
                            if let Some(s) = val.as_str() {
                                if !s.is_empty() && s.len() < 200 {
                                    file_paths.push(s.to_string());
                                }
                            }
                        }
                    }
                }
            }
            
            turns.push((round, tool_names, file_paths));
            round += 1;
        }
    }
    
    // Reverse to get chronological order.
    turns.reverse();
    turns
}

/// Perform the Ctrl-O raw-output toggle, rendering the last turn's tool output
/// as an EXPANDED (`now_on`) or COLLAPSED view directly beneath the prompt — and
/// crucially, ERASING the previously-rendered toggle block first so the view
/// flips **in place** instead of stacking a new block on every keypress (the
/// bug: collapse used to append below the still-visible expanded text, so it
/// read as "never collapses").
///
/// The erase math: after rustyline handles Ctrl-O it abandons the prompt line
/// and leaves the cursor on the next row, so between the cursor and the top of
/// the prior block sit exactly `raw_view_rows` block rows + 1 prompt row. We
/// move the cursor up that many rows to column 1 (`CSI n F`) and clear to end of
/// screen (`CSI 0 J`), then paint the new view and record its physical height in
/// `session.raw_view_rows` for the next toggle. The anchor is only valid while
/// the block is the last thing printed; the REPL zeroes `raw_view_rows` on any
/// non-Ctrl-O outcome. Non-TTY (piped / background coordinator) just prints the
/// header + body with no cursor games.
pub fn render_raw_toggle(session: &mut Session, now_on: bool) {
    let header = if now_on {
        "\x1b[2mraw tool output on\x1b[0m".to_string()
    } else {
        "\x1b[2mraw tool output off\x1b[0m".to_string()
    };
    let mut lines = vec![header];
    lines.extend(if now_on {
        reveal_last_turn(session)
    } else {
        collapse_last_turn(session)
    });

    if !stderr_is_tty() {
        for line in &lines {
            eprintln!("{line}");
        }
        session.raw_view_rows = 0;
        return;
    }

    let cols = stderr_cols();
    // Erase the prior in-place block (block rows + the intervening prompt row)
    // before repainting, so the toggle flips the same region rather than
    // appending. Skipped on the first toggle of a turn (raw_view_rows == 0).
    if session.raw_view_rows > 0 {
        eprint!("\x1b[{}F\x1b[0J", session.raw_view_rows + 1);
    }
    let mut rows = 0usize;
    for line in &lines {
        eprintln!("{line}");
        rows += physical_rows(line, cols);
    }
    session.raw_view_rows = rows;
}

/// Phase 3.4 runtime enforcement hook. When `result` declares an
/// `output_schema` AND carries a structured payload, validate that payload
/// against the plugin's named schema (discovered under the default plugins
/// dir). Fail-open: a violation — or an unknown plugin/schema — is logged to
/// stderr and recorded on the result as a model-facing note, but the payload is
/// never altered or dropped. Returns immediately (no plugin discovery, no
/// allocation) when no schema is declared, which is the overwhelming common
/// case, so the hook adds zero overhead to ordinary tool calls.
fn validate_output_schema(result: &mut ToolResult) {
    // Common case: nothing declared → do not even touch the plugin loader.
    if result.output_schema.is_none() {
        return;
    }
    validate_output_schema_in(&crate::plugins::default_plugins_dir(), result);
}

/// Core of [`validate_output_schema`], parameterised on the plugins dir so it is
/// unit-testable against a temp plugin fixture without a live `Session`.
fn validate_output_schema_in(plugins_dir: &std::path::Path, result: &mut ToolResult) {
    let Some(OutputSchemaRef {
        plugin_id,
        schema_name,
    }) = result.output_schema.clone()
    else {
        return;
    };
    // A schema was declared but the tool emitted no structured payload — nothing
    // to validate. (Text-only results skip validation entirely.)
    let Some(value) = result.structured.clone() else {
        return;
    };
    if let Err(e) = crate::plugins::validate_against_plugin_schema(
        plugins_dir,
        &plugin_id,
        &schema_name,
        &value,
    ) {
        // Fail-open: log the violation and annotate for the model, but let the
        // payload flow through unchanged.
        let note = format!("payload violates schema `{plugin_id}/{schema_name}`: {e}");
        eprintln!("\x1b[2m  \u{26a0} schema-validation: {note}\x1b[0m");
        result.note_schema_violation(note);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- Phase 3.4: output-schema runtime-enforcement hook ----------------

    /// Materialize a throwaway plugin dir shipping a single `schemas/<name>.json`
    /// so `validate_output_schema_in` can discover it. Returns the plugins root.
    fn schema_fixture(plugin_id: &str, schema_name: &str, schema: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static N: AtomicU32 = AtomicU32::new(0);
        let root = std::env::temp_dir().join(format!(
            "aish_schema_enf_{}_{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let pdir = root.join(plugin_id);
        std::fs::create_dir_all(pdir.join("schemas")).unwrap();
        std::fs::write(pdir.join("plugin.json"), format!("{{\"id\":\"{plugin_id}\"}}")).unwrap();
        std::fs::write(pdir.join("schemas").join(format!("{schema_name}.json")), schema).unwrap();
        root
    }

    const OBJ_SCHEMA: &str =
        r#"{"type":"object","required":["name"],"properties":{"name":{"type":"string"}}}"#;

    #[test]
    fn schema_validation_passes_transparently() {
        let root = schema_fixture("plug", "item", OBJ_SCHEMA);
        let mut r = ToolResult::structured("t", "name=ok", serde_json::json!({"name": "ok"}), false)
            .with_output_schema("plug", "item");
        validate_output_schema_in(&root, &mut r);
        // Conforms → no violation recorded, model sees exactly the payload JSON.
        assert!(r.schema_violation.is_none());
        assert_eq!(r.model_content().as_ref(), r#"{"name":"ok"}"#);
    }

    #[test]
    fn schema_validation_failure_is_logged_noted_and_not_blocked() {
        let root = schema_fixture("plug", "item", OBJ_SCHEMA);
        // `name` is a number → type violation.
        let mut r = ToolResult::structured("t", "name=1", serde_json::json!({"name": 1}), false)
            .with_output_schema("plug", "item");
        validate_output_schema_in(&root, &mut r);
        // Failure is recorded (for the model) but the payload still flows.
        let note = r.schema_violation.clone().expect("violation recorded");
        assert!(note.contains("plug/item"), "note names the schema: {note}");
        let model = r.model_content();
        assert!(
            model.starts_with("[schema-validation warning]"),
            "model told of the violation: {model}"
        );
        // Fail-open: the original payload is still present after the banner.
        assert!(model.contains(r#"{"name":1}"#), "payload not blocked: {model}");
        assert!(r.structured.is_some());
    }

    #[test]
    fn no_output_schema_declaration_is_zero_cost_noop() {
        // The common case: a structured result with NO declared schema. The
        // top-level hook must return before touching the plugin loader.
        let mut r =
            ToolResult::structured("t", "x", serde_json::json!({"anything": true}), false);
        validate_output_schema(&mut r);
        assert!(r.schema_violation.is_none());
        assert!(r.output_schema.is_none());
    }

    #[test]
    fn unknown_plugin_fails_open_with_note() {
        // Schema declared against a plugin that isn't present → UnknownSchema,
        // logged fail-open, never blocking.
        let empty = std::env::temp_dir().join(format!("aish_schema_enf_empty_{}", std::process::id()));
        std::fs::create_dir_all(&empty).unwrap();
        let mut r = ToolResult::structured("t", "x", serde_json::json!({"name": "ok"}), false)
            .with_output_schema("ghost", "item");
        validate_output_schema_in(&empty, &mut r);
        assert!(r.schema_violation.is_some(), "unknown schema is noted, not silent");
        assert!(r.structured.is_some(), "payload still flows");
    }

    #[test]
    fn schema_declared_but_text_only_result_is_skipped() {
        let root = schema_fixture("plug", "item", OBJ_SCHEMA);
        // A schema ref with no structured payload → nothing to validate.
        let mut r = ToolResult::text("t", "just text", false).with_output_schema("plug", "item");
        validate_output_schema_in(&root, &mut r);
        assert!(r.schema_violation.is_none());
        assert_eq!(r.model_content().as_ref(), "just text");
    }

    #[test]
    fn raw_body_placeholder() {
        let mk = |content: &str, is_error| ToolResult::text("t", content, is_error);
        assert_eq!(raw_body(&mk("hello", false)), "hello");
        assert_eq!(raw_body(&mk("", true)), "(no output)");
        assert_eq!(raw_body(&mk("   \n ", false)), "(no output)");
        // error results keep their content — they are included, not skipped
        assert_eq!(raw_body(&mk("boom", true)), "boom");
    }

    #[test]
    fn raw_body_shows_human_text_not_model_json_for_structured_results() {
        // S7.3 / AC2 + OQ2: Ctrl-O's raw view is INDEPENDENT of what the model
        // gets. A structured result threads compact JSON to the model
        // (model_content), but raw_body keeps showing the verbatim, aligned
        // human-readable `content` — never a JSON dump.
        let r = ToolResult::structured(
            "t",
            "file f.txt 3\ndir  sub",
            serde_json::json!([{"name": "f.txt", "type": "file", "size": 3}]),
            false,
        );
        assert_eq!(raw_body(&r), "file f.txt 3\ndir  sub");
        // The model path differs — proving the split is real.
        assert_ne!(raw_body(&r), r.model_content());

        // FR4 fallback: a (rare) structured-only result with empty content
        // pretty-prints its payload rather than showing "(no output)".
        let so = ToolResult::structured("t", "", serde_json::json!({"k": 1}), false);
        assert_eq!(raw_body(&so), "{\n  \"k\": 1\n}");
    }

    #[test]
    fn raw_body_payload_is_additive_matches_text_only_view() {
        // S7.4 / AC2: the Ctrl-O raw view is ADDITIVE — attaching a payload must
        // not change what the human sees. A structured result and a text-only
        // result built from the SAME content render an identical raw body; the
        // payload is the MODEL's view (model_content), never substituted into the
        // human raw view. See docs/S7.4-tests-docs-scope.md §3.
        let content = "name  type  size\nf.txt file  3";
        let text = ToolResult::text("t", content, false);
        let structured = ToolResult::structured(
            "t",
            content,
            serde_json::json!([{"name": "f.txt", "type": "file", "size": 3}]),
            false,
        );
        assert_eq!(
            raw_body(&structured),
            raw_body(&text),
            "payload must not alter the raw view"
        );
        assert_eq!(raw_body(&structured), content);
        // And the model sees something different (the compact JSON) — proving the
        // payload is additive, not a no-op and not a substitute for the raw view.
        assert_ne!(raw_body(&structured), structured.model_content());
    }

    #[test]
    fn tool_result_line_marks_status() {
        let ok = tool_result_line("read /etc/hosts", false);
        assert!(
            ok.contains("\x1b[32m✓\x1b[0m"),
            "success checkmark should be green: {ok}"
        );
        let err = tool_result_line("write x", true);
        assert!(
            err.contains("\x1b[31m✗\x1b[0m"),
            "error X should be red: {err}"
        );
    }

    #[test]
    fn tool_glyph_escalate_handshake_mcp_wrench_local_hammer() {
        // The escalate consult keeps the 🤝 handshake; MCP tools keep the 🔧
        // wrench; every other local tool/exe/script call gets the 🛠️
        // hammer-and-wrench — exactly one source glyph, never two.
        assert_eq!(tool_glyph("escalate"), "🤝");
        assert_eq!(tool_glyph("run_program"), "🛠️ ");
        assert_eq!(tool_glyph("read_file"), "🛠️ ");
        assert_eq!(tool_glyph("mcp__atum__list_tools"), "🔧");
    }

    #[test]
    fn escalate_activity_line_uses_handshake_glyph() {
        // The desc the spinner/finish/reveal all render carries the handshake, and
        // the finished line keeps the colorized status mark in front of it. The
        // handshake is double-width like the wrench, joined to the desc with a
        // single space.
        let call = crate::backend::ToolCall {
            id: "t".into(),
            name: "escalate".into(),
            args: serde_json::json!({ "task": "estimate the LOE" }),
        };
        let desc = format!(
            "{} {}",
            tool_glyph(&call.name),
            flatten_ws(&describe_call(&call))
        );
        assert!(
            desc.starts_with("🤝 escalate:"),
            "escalate line must lead with the handshake: {desc}"
        );
        assert!(
            !desc.contains('🔧'),
            "escalate must not show the wrench: {desc}"
        );
        let done = tool_result_line(&desc, false);
        assert!(
            done.contains("\x1b[32m✓\x1b[0m"),
            "done line should be green-checked: {done}"
        );
        assert!(done.contains("🤝"), "done line keeps the handshake: {done}");
    }

    #[test]
    fn describe_call_names_target_for_internal_tools() {
        let d = |name: &str, args: serde_json::Value| {
            describe_call(&crate::backend::ToolCall {
                id: "t".into(),
                name: name.into(),
                args,
            })
        };
        use serde_json::json;
        assert_eq!(d("read_file", json!({"path": "a.rs"})), "read a.rs");
        assert_eq!(
            d("edit_file", json!({"path": "a.rs", "pattern": "foo"})),
            "edit a.rs (\"foo\")"
        );
        assert_eq!(
            d("append_file", json!({"path": "log.txt", "content": "hi"})),
            "append log.txt (2 bytes)"
        );
        assert_eq!(d("copy_file", json!({"src": "a", "dst": "b"})), "copy a → b");
        assert_eq!(
            d("rename_file", json!({"src": "a", "dst": "b"})),
            "rename a → b"
        );
        assert_eq!(d("glob_expand", json!({"pattern": "*.rs"})), "glob *.rs");
        assert_eq!(
            d("glob_expand", json!({"pattern": "*.rs", "path": "src"})),
            "glob *.rs in src"
        );
        assert_eq!(
            d("grep_files", json!({"pattern": "TODO", "path": "src"})),
            "grep \"TODO\" in src"
        );
        assert_eq!(
            d("grep_files", json!({"pattern": "TODO"})),
            "grep \"TODO\" in ."
        );
        assert_eq!(d("stat_file", json!({"path": "a.rs"})), "stat a.rs");
        assert_eq!(d("diff_files", json!({"a": "x", "b": "y"})), "diff x y");
        assert_eq!(d("diff_files", json!({"a": "x"})), "diff x (inline)");
        assert_eq!(d("job_output", json!({"job": 3})), "job output: 3");
        assert_eq!(
            d("get_skill", json!({"server": "atum", "name": "review-pr"})),
            "skill atum/review-pr"
        );
        assert_eq!(
            d("tell", json!({"id": "w_abc", "message": "narrow scope"})),
            "tell w_abc: narrow scope"
        );
        // An unmapped tool still falls back to its bare name.
        assert_eq!(d("mystery_tool", json!({})), "mystery_tool");
    }

    #[test]
    fn interactive_tools_do_not_animate() {
        assert!(!animates("run_interactive")); // hands off the terminal
        assert!(animates("run_program"));
        assert!(animates("read_file"));
    }

    #[test]
    fn tool_frames_cycle_and_are_nonempty() {
        // Indexing wraps with modulo, so the cycle must be non-empty and stable.
        assert!(!TOOL_FRAMES.is_empty());
        assert!(TOOL_FRAMES.iter().all(|f| !f.is_empty()));
        // i % len wraps back to frame 0 after a full cycle.
        assert_eq!(
            TOOL_FRAMES[0 % TOOL_FRAMES.len()],
            TOOL_FRAMES[TOOL_FRAMES.len() % TOOL_FRAMES.len()]
        );
    }

    #[test]
    fn truncate_to_cols_keeps_short_lines_intact() {
        use unicode_width::UnicodeWidthStr;
        // A line at or under the budget is returned byte-for-byte unchanged — no
        // ellipsis, no allocation surprises.
        assert_eq!(truncate_to_cols("read foo.rs", 40), "read foo.rs");
        assert_eq!(truncate_to_cols("", 10), "");
        // Exactly at the limit is still left alone.
        let s = "abcdef";
        assert_eq!(truncate_to_cols(s, s.width()), s);
    }

    #[test]
    fn truncate_to_cols_clamps_long_lines_to_width() {
        use unicode_width::UnicodeWidthStr;
        // The whole point of the fix: a desc longer than the terminal is clamped
        // so the animated `\r`+erase line stays on ONE physical row. The result
        // never exceeds the budget and ends in an ellipsis to mark the cut.
        let long = "run_program ./scripts/init-rpc-node.sh ".repeat(20);
        for max in [10usize, 20, 40, 80] {
            let out = truncate_to_cols(&long, max);
            assert!(
                out.width() <= max,
                "width {} exceeds max {max}: {out:?}",
                out.width()
            );
            assert!(
                out.ends_with('…'),
                "truncated line should end with an ellipsis: {out:?}"
            );
        }
        // A zero budget can't even hold the ellipsis — produce nothing rather
        // than overflow the row.
        assert_eq!(truncate_to_cols("anything", 0), "");
    }

    #[test]
    fn truncate_to_cols_counts_wide_glyphs_as_two() {
        use unicode_width::UnicodeWidthStr;
        // 🔧 is two display columns. Measuring by chars would let a wrench-led
        // desc overflow the row by one column and reintroduce the wrap bug.
        let desc = "🔧 ".to_string() + &"x".repeat(100);
        let out = truncate_to_cols(&desc, 10);
        assert!(
            out.width() <= 10,
            "wide-glyph desc overflowed: width {}",
            out.width()
        );
        assert!(out.starts_with('🔧'), "leading glyph preserved: {out:?}");
    }

    #[test]
    fn flatten_ws_collapses_embedded_newlines_to_one_line() {
        // The multi-line tool-call bug: a `gh pr create --body` argv carries an
        // embedded markdown body with newlines. Width truncation cannot catch a
        // width-0 newline, so it must be flattened at the source or the spinner
        // scrolls a fresh copy every frame.
        let desc = "🔧 gh pr create --title fix --body ## Issue\n\nbody\n- a\n- b";
        let out = flatten_ws(desc);
        assert!(!out.contains('\n'), "newlines must be gone: {out:?}");
        assert!(
            !out.contains('\r'),
            "carriage returns must be gone: {out:?}"
        );
        assert!(!out.contains('\t'), "tabs must be gone: {out:?}");
        assert_eq!(
            out,
            "🔧 gh pr create --title fix --body ## Issue body - a - b"
        );
    }

    #[test]
    fn flatten_ws_squeezes_runs_and_trims() {
        assert_eq!(flatten_ws("  a   b\t c \n"), "a b c");
        assert_eq!(flatten_ws("single"), "single");
        assert_eq!(flatten_ws("\n\n"), "");
    }

    #[test]
    fn activity_stream_summarizes_output() {
        // The stream shows a single collapsed summary line (line count + Ctrl-O
        // hint), indented to column 5 to nest under the tool header. The running
        // token/tool/turn stats moved to the main statusline.
        let lines = activity_stream_lines(8);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0], "     … 8 lines of output — Ctrl-O to expand");
    }

    #[test]
    fn activity_stream_handles_single_and_empty_output() {
        // One line → singular noun.
        let lines = activity_stream_lines(1);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0], "     … 1 line of output — Ctrl-O to expand");
        // No output → nothing at all (no summary row, stats live elsewhere).
        let lines = activity_stream_lines(0);
        assert!(lines.is_empty());
    }

    #[test]
    fn seed_context_injects_only_on_empty_history() {
        // Fresh conversation with a prior output → input is seeded.
        let seeded = seed_context(true, Some("df output".into()), "summarize that".into());
        assert!(seeded.contains("df output"));
        assert!(seeded.ends_with("summarize that"));
        // Mid-conversation → untouched (the model already has prior output).
        assert_eq!(
            seed_context(false, Some("df output".into()), "next".into()),
            "next"
        );
        // No prior output, or an empty one → untouched.
        assert_eq!(seed_context(true, None, "hi".into()), "hi");
        assert_eq!(seed_context(true, Some("   \n".into()), "hi".into()), "hi");
    }

    #[test]
    fn ansi_stripped_width_ignores_csi_escapes() {
        // Dim wrapper contributes zero visible width.
        assert_eq!(ansi_stripped_width("\x1b[2mhello\x1b[0m"), 5);
        // Bare text unchanged.
        assert_eq!(ansi_stripped_width("abcd"), 4);
        // Wide glyphs count as two columns.
        assert_eq!(ansi_stripped_width("\x1b[2m你好\x1b[0m"), 4);
        // Pure escape sequence → zero visible width.
        assert_eq!(ansi_stripped_width("\x1b[0m"), 0);
    }

    #[test]
    fn physical_rows_accounts_for_softwrap_and_escapes() {
        // Fits on one row.
        assert_eq!(physical_rows("hello", 80), 1);
        // Exactly fills → one row; one over → two rows.
        assert_eq!(physical_rows("abcd", 4), 1);
        assert_eq!(physical_rows("abcde", 4), 2);
        // Escape codes don't inflate the wrap math (5 visible cols / 4 = 2 rows).
        assert_eq!(physical_rows("\x1b[2mabcde\x1b[0m", 4), 2);
        // Empty line still occupies a row.
        assert_eq!(physical_rows("", 80), 1);
        // Unknown width degrades to a single row.
        assert_eq!(physical_rows("anything at all", 0), 1);
    }
}
