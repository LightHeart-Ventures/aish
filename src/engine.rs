use crate::backend::{Backend, Msg, Role, ToolResult};
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
    // Decide, for this turn, whether a stronger model is worth escalating to
    // (weak frontend → Some) and stash it so the `escalate` tool can rebuild that
    // backend at call time. Drives both the tool's availability and the nudge.
    session.escalation = backend
        .escalation_target(&session.batch_model, &session.env)
        .map(|(provider, model)| (provider.to_string(), model));
    let escalate_available = session.escalation.is_some();
    let system = session.system_prompt(escalate_available);
    let mut tool_defs = tools::tool_defs(session.batch_mode, escalate_available);
    if backend.include_mcp_tools() {
        tool_defs.extend(session.mcp.tool_defs());
    }
    // TASK-13: on a fresh conversation, seed the turn with the previous recorded
    // output so a prompt like "summarize that" can reference it without
    // re-running. Mid-conversation the output is already in `history`, so we
    // don't duplicate it.
    // Context-awareness: before adding this turn, compact the conversation if it
    // has grown past the window threshold — offloading the oldest slice to the
    // SQLite memories table and replacing it with a short in-context summary.
    maybe_compact(backend, session);
    // Skill-awareness (crate::skill_match): score THIS turn's request against the
    // installed local skill catalog and, when one clearly fits, fold a short note
    // pointing at its SKILL.md into the turn input. Matched on the raw request
    // (before context-seeding) so a prepended preamble can't skew the keyword
    // match; the note goes into the turn input, never the cached system prompt,
    // so the prompt-cache prefix stays byte-stable.
    let task = input.clone();
    let input = seed_context(session.history.is_empty(), session.last_output(), input);
    // Prefer an INSTALLED skill: when one clearly fits, fold in the note pointing
    // at its SKILL.md. When NONE fits a substantial task, fall back to an OFFLINE
    // recommendation of an installable registry skill (read from the
    // binary-shipped index — no network on the hot path), so the model surfaces a
    // `:skill add <ref>` suggestion instead of faking or hand-rolling the work.
    let input = match crate::skill_match::hint(&task, &session.skills) {
        Some(note) => format!("{note}\n\n{input}"),
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

    for iteration in 1..=MAX_ITERATIONS {
        // Keep the input prompt VISIBLE while the turn runs (option-1 of the
        // type-while-busy ask): aish's editor only reads BETWEEN turns, so during
        // the model⇄tools loop the prompt would otherwise vanish. Print a dim,
        // prompt-shaped reminder at each round boundary so the user can SEE the
        // prompt is still live — text or a `:command` typed now is line-buffered
        // by the TTY and runs the moment this turn returns; Ctrl-C aborts. Gated
        // to an interactive TTY (a background coordinator / piped run prints none).
        emit_prompt_footer(session);
        // Budget phase for THIS round: Normal → run freely; SoftWarn → fold a
        // "converge now" notice into the prompt; ForceSummarize → hand the model
        // NO tools so it must produce a best-effort final answer rather than being
        // killed empty-handed at the hard cap.
        let phase = crate::loopguard::budget_phase(iteration, MAX_ITERATIONS);
        let force = matches!(phase, crate::loopguard::BudgetPhase::ForceSummarize);
        // The base prompt stays byte-stable (prompt-cache friendly); the budget
        // suffix is appended only while converging/forcing.
        let effective_system = match phase {
            crate::loopguard::BudgetPhase::Normal => system.clone(),
            other => format!(
                "{system}{}",
                crate::loopguard::budget_suffix(other, MAX_ITERATIONS.saturating_sub(iteration))
            ),
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
        let turn = backend.complete(&effective_system, &session.history, active_tools).await;
        drop(spinner);
        let turn = turn?;
        let usage = turn.usage;

        // Update the running context figure from the backend's reported usage
        // (the prompt the model just saw), or a char-based estimate as a fallback.
        session.context_used = match usage {
            Some(u) => u.total(),
            None => crate::context::estimate_history_tokens(&session.history),
        };

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
            let reason = crate::loopguard::ExitReason::ForcedSummarize { iterations: iteration };
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
            let desc = format!("{} {}", tool_glyph(&call.name), flatten_ws(&describe_call(call)));

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

            let result = if let Some(crate::turn_audit::Step::Replay { output, is_error }) =
                &audit_step
            {
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
            let reason = crate::loopguard::ExitReason::LoopDetected { call: call_desc, count };
            eprintln!("\x1b[2maish: {}\x1b[0m", reason.log_line());
            let partial = if turn.text.trim().is_empty() {
                "Stopped by the loop guard before a final answer was produced.".to_string()
            } else {
                turn.text.clone()
            };
            return Ok(crate::loopguard::with_banner(&reason, &partial));
        }
    }

    // Hard backstop. With the forced-summarize step firing at FORCE_SUMMARIZE_PCT
    // this is effectively unreachable, but keep a tagged exit so a future budget
    // change can't silently resurrect the old "throw the work away" stop.
    let reason = crate::loopguard::ExitReason::BudgetExhausted { iterations: MAX_ITERATIONS };
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
                println!("{}", crate::md::render_stdout(result));
            }
            Ok(())
        }
        // A failed run prints its error to stdout (the worker captures stdout as
        // the result) and propagates a non-zero exit so the parent marks it
        // failed. The durable row already records the failure for rehydrate.
        _ => {
            let err = outcome.error.unwrap_or_else(|| "coordinator failed".into());
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
    let window = backend.context_window();
    if !crate::context::should_compact(
        session.context_used,
        window,
        crate::context::COMPACT_THRESHOLD_PCT,
    ) {
        return;
    }
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
    eprintln!(
        "\x1b[2maish: context at/over {}% — compacted {dropped} earlier message(s) to memory\x1b[0m",
        crate::context::COMPACT_THRESHOLD_PCT
    );
}

/// Print the model's interim narration. In an interactive session it goes out
/// plainly. In a background coordinator (`session.nested`) each line is tagged
/// with a `🗨` sentinel so the parent's worker stream can recognize it as *turn*
/// output (vs `🔧` tool lines) and forward it only when `:worker-output` is on.
/// A coordinator turn is always a standard (Messages API) model call, hence the
/// `[standard]` label the parent attaches; batch fan-out is announced separately.
fn emit_narration(session: &mut Session, text: &str) {
    let rendered = crate::md::render(text.trim(), "");
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
    std::env::var("COLUMNS").ok().and_then(|c| c.parse().ok()).unwrap_or(80)
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

/// `~`-abbreviated working directory for the in-turn prompt footer — mirrors the
/// REPL prompt's home-collapsing (`repl::short_cwd`) so the footer reads like the
/// real prompt. Kept local to avoid a back-dependency on the repl module.
fn short_cwd(cwd: &std::path::Path) -> String {
    let cwd = cwd.display().to_string();
    match std::env::var("HOME") {
        Ok(home) if !home.is_empty() && cwd.starts_with(&home) => cwd.replacen(&home, "~", 1),
        _ => cwd,
    }
}

/// The dim, prompt-shaped reminder shown at each round boundary while a turn is
/// in flight (the option-1 "visible prompt during a turn" slice). Pure so it's
/// unit-testable; `emit_prompt_footer` does the TTY/nested gating and the write.
/// Reads like the real prompt (`<cwd> ❯`) plus a parenthetical that states the
/// honest affordance: type-ahead is line-buffered and runs after this turn, and
/// Ctrl-C aborts.
fn prompt_footer_line(cwd: &std::path::Path) -> String {
    format!(
        "\x1b[2m{} ❯ (type ahead — text or a :command runs after this turn · Ctrl-C aborts)\x1b[0m",
        short_cwd(cwd)
    )
}

/// Emit the in-turn prompt footer (see `prompt_footer_line`). Interactive TTY
/// only: a background coordinator (`session.nested`) and any piped/non-TTY run
/// print nothing — they have no live prompt to keep visible.
fn emit_prompt_footer(session: &Session) {
    if session.nested || !stderr_is_tty() {
        return;
    }
    eprintln!("{}", prompt_footer_line(&session.cwd));
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
                eprint!("\r\x1b[36m{}\x1b[0m \x1b[2;36mthinking…\x1b[0m", FRAMES[i % FRAMES.len()]);
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
        Self { state, task: Some(task), animated: true }
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
            eprintln!("\r\x1b[2K\x1b[2m  {}\x1b[0m", tool_result_line(desc, is_error));
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
        format!("\x1b[31m✗\x1b[0m {desc}")  // red for error
    } else {
        format!("\x1b[32m✓\x1b[0m {desc}")  // green for success
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
        "list_dir" => format!("list {}", a["path"].as_str().unwrap_or(".")),
        "change_dir" => format!("cd {}", a["path"].as_str().unwrap_or("?")),
        "remember" => format!("remember: {}", a["content"].as_str().unwrap_or("?")),
        "recall" => format!("recall: {}", a["query"].as_str().unwrap_or("(recent)")),
        "run_in_background" => format!(
            "background: {}",
            crate::batch::one_line(a["task"].as_str().unwrap_or("?"))
        ),
        "background_status" => "background status".to_string(),
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

/// Re-print the most recent turn's tool calls and their raw results. Drives the
/// retroactive reveal when raw output is toggled on after an answer.
pub fn reveal_last_turn(session: &Session) {
    for (desc, result) in &session.last_turn_tools {
        eprintln!("\x1b[2m  {}\x1b[0m", tool_result_line(desc, result.is_error));
        print_raw_result(result);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_cwd_collapses_home_prefix() {
        // A path under $HOME is abbreviated to `~`; anything else is verbatim.
        let home = std::env::var("HOME").unwrap_or_default();
        if !home.is_empty() {
            let under = std::path::PathBuf::from(&home).join("projects/aish");
            let out = short_cwd(&under);
            assert!(out.starts_with('~'), "home should collapse to ~: {out}");
            assert!(!out.contains(&home), "literal home must be gone: {out}");
        }
        // A path outside HOME is returned unchanged.
        assert_eq!(short_cwd(std::path::Path::new("/var/log")), "/var/log");
    }

    #[test]
    fn prompt_footer_line_reads_like_a_prompt_and_states_affordances() {
        // Dim-wrapped, carries the ❯ prompt glyph, and names BOTH affordances the
        // visible-prompt slice promises: type-ahead (text or a :command) and the
        // Ctrl-C abort. The cwd is rendered through short_cwd.
        let line = prompt_footer_line(std::path::Path::new("/tmp/x"));
        assert!(line.starts_with("\x1b[2m"), "footer must be dim: {line}");
        assert!(line.ends_with("\x1b[0m"), "footer must reset SGR: {line}");
        assert!(line.contains("/tmp/x ❯"), "footer must show the cwd + prompt glyph: {line}");
        assert!(line.contains(":command"), "footer must mention :commands: {line}");
        assert!(line.contains("Ctrl-C"), "footer must mention the abort key: {line}");
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
    fn tool_result_line_marks_status() {
        let ok = tool_result_line("read /etc/hosts", false);
        assert!(ok.contains("\x1b[32m✓\x1b[0m"), "success checkmark should be green: {ok}");
        let err = tool_result_line("write x", true);
        assert!(err.contains("\x1b[31m✗\x1b[0m"), "error X should be red: {err}");
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
        let desc = format!("{} {}", tool_glyph(&call.name), flatten_ws(&describe_call(&call)));
        assert!(
            desc.starts_with("🤝 escalate:"),
            "escalate line must lead with the handshake: {desc}"
        );
        assert!(!desc.contains('🔧'), "escalate must not show the wrench: {desc}");
        let done = tool_result_line(&desc, false);
        assert!(done.contains("\x1b[32m✓\x1b[0m"), "done line should be green-checked: {done}");
        assert!(done.contains("🤝"), "done line keeps the handshake: {done}");
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
        assert_eq!(TOOL_FRAMES[0 % TOOL_FRAMES.len()], TOOL_FRAMES[TOOL_FRAMES.len() % TOOL_FRAMES.len()]);
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
            assert!(out.width() <= max, "width {} exceeds max {max}: {out:?}", out.width());
            assert!(out.ends_with('…'), "truncated line should end with an ellipsis: {out:?}");
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
        assert!(out.width() <= 10, "wide-glyph desc overflowed: width {}", out.width());
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
        assert!(!out.contains('\r'), "carriage returns must be gone: {out:?}");
        assert!(!out.contains('\t'), "tabs must be gone: {out:?}");
        assert_eq!(out, "🔧 gh pr create --title fix --body ## Issue body - a - b");
    }

    #[test]
    fn flatten_ws_squeezes_runs_and_trims() {
        assert_eq!(flatten_ws("  a   b\t c \n"), "a b c");
        assert_eq!(flatten_ws("single"), "single");
        assert_eq!(flatten_ws("\n\n"), "");
    }

    #[test]
    fn seed_context_injects_only_on_empty_history() {
        // Fresh conversation with a prior output → input is seeded.
        let seeded = seed_context(true, Some("df output".into()), "summarize that".into());
        assert!(seeded.contains("df output"));
        assert!(seeded.ends_with("summarize that"));
        // Mid-conversation → untouched (the model already has prior output).
        assert_eq!(seed_context(false, Some("df output".into()), "next".into()), "next");
        // No prior output, or an empty one → untouched.
        assert_eq!(seed_context(true, None, "hi".into()), "hi");
        assert_eq!(seed_context(true, Some("   \n".into()), "hi".into()), "hi");
    }
}
