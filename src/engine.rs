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
/// the prompt also means the emoji only animates while the tool is *executing*,
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
const MAX_ITERATIONS: usize = 50;

/// One full agentic turn: user input → (model ⇄ tools)* → final text.
/// Frontend-agnostic: confirmation is a callback, output goes through eprintln
/// only for transient activity lines.
pub async fn run_turn(
    backend: &Backend,
    session: &mut Session,
    input: String,
    confirm: &mut Confirm<'_>,
) -> Result<String> {
    let system = session.system_prompt();
    let mut tool_defs = tools::tool_defs(session.batch_mode);
    if backend.include_mcp_tools() {
        tool_defs.extend(session.mcp.tool_defs());
    }
    // TASK-13: on a fresh conversation, seed the turn with the previous recorded
    // output so a prompt like "summarize that" can reference it without
    // re-running. Mid-conversation the output is already in `history`, so we
    // don't duplicate it.
    let input = seed_context(session.history.is_empty(), session.last_output(), input);
    session.history.push(Msg::user(input));
    session.last_turn_tools.clear();

    // First local use lazy-loads (and maybe downloads) weights — do it before
    // any spinner exists so the download progress line owns stderr.
    backend.prepare().await?;

    for _ in 0..MAX_ITERATIONS {
        // Model-reasoning phase: the "thinking" spinner owns stderr while the
        // backend produces the next message (which may consume prior tool
        // results). It is stopped before any tool-execution animation begins,
        // so the two never run at once.
        let spinner = Spinner::start();
        let turn = backend.complete(&system, &session.history, &tool_defs).await;
        drop(spinner);
        let turn = turn?;

        session.history.push(Msg {
            role: Role::Assistant,
            text: turn.text.clone(),
            tool_calls: turn.tool_calls.clone(),
            tool_results: vec![],
            raw: turn.raw,
        });

        // A response cut off mid-tool-call had its partial tool call dropped (see
        // claude.rs) — `tool_calls` is empty but this is NOT a final answer. The
        // corrective note is already in `turn.text` (now in history as the
        // assistant message); loop so the model retries with a smaller edit.
        if turn.truncated_tool_call {
            if !turn.text.trim().is_empty() {
                eprintln!("{}", crate::md::render(turn.text.trim(), ""));
            }
            continue;
        }

        if turn.tool_calls.is_empty() {
            return Ok(turn.text);
        }

        // Interim narration from the model (its reasoning between tool calls) —
        // rendered at normal brightness like the final answer, because it's
        // substantive content the user reads. Only the transient 🔧 tool-activity
        // lines stay dim, so the rounds still read as structured.
        if !turn.text.trim().is_empty() {
            eprintln!("{}", crate::md::render(turn.text.trim(), ""));
        }

        let mut results: Vec<ToolResult> = Vec::with_capacity(turn.tool_calls.len());
        for call in &turn.tool_calls {
            let desc = describe_call(call);
            // Tool-execution phase: this call gets its own animated emoji line
            // while it runs. Calls execute sequentially, so only the current
            // one animates; the spinner is replaced by a final static line
            // (✓/✗ + desc) when the tool returns. `run_interactive` hands the
            // terminal to a child, so it opts out of the animation (which would
            // fight the child for stderr) and shows a plain static line.
            let tool_spin = ToolSpinner::start(&desc, animates(&call.name));
            // Pause the animation around any permission prompt: the emoji must
            // not animate (or overwrite the prompt) while we wait for the answer,
            // then it resumes for the tool's actual execution.
            let stopper = tool_spin.stopper();
            let mut gated = |p: &str| {
                pause_spinner(&stopper);
                let decision = confirm(p);
                resume_spinner(&stopper);
                decision
            };
            let result = tools::execute(call, session, &mut gated).await;
            tool_spin.finish(&desc, result.is_error);
            if session.raw_tool_output {
                print_raw_result(&result);
            }
            session.last_turn_tools.push((desc, result.clone()));
            results.push(result);
        }
        eprintln!(); // breathing room between tool activity and what follows
        session.history.push(Msg::tool_results(results));
    }

    Ok("[stopped: turn exceeded the tool-call iteration limit]".into())
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

/// Cycling-emoji animation on the tool-call line while that tool executes —
/// the tool-execution phase indicator, distinct from the model's "thinking"
/// spinner. A gear/hourglass cycle reads as "work in progress" at a glance and
/// keeps the same dim, two-space-indented style as the static 🔧 line it
/// replaces. TTY-gated; on `finish` the animation is erased and a static
/// result line (✓/✗ + desc) is printed in its place.
struct ToolSpinner {
    /// Shared animation state (Running/Paused/Stopped). `stopper()` hands a clone
    /// to the confirm wrapper so it can pause/resume around the prompt.
    state: SpinState,
    /// The animation task — aborted by `finish`/`Drop` once stopped.
    task: Option<tokio::task::JoinHandle<()>>,
    /// True when we actually animated (vs the static piped/non-animating line).
    animated: bool,
}

/// Frames for the running-tool animation. Gear ↔ hourglass: "a tool is turning
/// / time is passing". Small and tasteful — two glyphs, no emoji soup.
const TOOL_FRAMES: [&str; 4] = ["⚙️ ", "⏳", "⚙️ ", "⌛"];

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
            // once, no animation.
            eprintln!("\x1b[2m  🔧 {desc}\x1b[0m");
            return Self {
                state: Arc::new(Mutex::new(Spin::Stopped)),
                task: None,
                animated: false,
            };
        }
        eprint!("\x1b[?25l"); // hide the cursor so it doesn't blink at the spinner's tail
        let state = Arc::new(Mutex::new(Spin::Running));
        let s = state.clone();
        let desc = desc.to_string();
        let task = tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_millis(120));
            // Consume the immediate first tick so no frame draws for the first
            // ~120ms — long enough for a permission gate to pause us before the
            // emoji ever appears, so it never animates over the prompt.
            tick.tick().await;
            for i in 0.. {
                tick.tick().await;
                let g = s.lock().unwrap();
                match *g {
                    Spin::Stopped => break,
                    Spin::Paused => {} // waiting on a confirm — draw nothing
                    Spin::Running => {
                        eprint!("\r\x1b[2K\x1b[2m  {} {desc}\x1b[0m", TOOL_FRAMES[i % TOOL_FRAMES.len()])
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
        }
    }
}

/// Pause the animation for a confirm prompt: stop drawing, clear the line, and
/// restore the cursor — so the emoji isn't animating while we wait for the
/// user's answer and the prompt is clean. Under the lock, so it can't race a
/// frame. Idempotent.
fn pause_spinner(state: &SpinState) {
    let mut g = state.lock().unwrap();
    if *g == Spin::Running {
        *g = Spin::Paused;
        eprint!("\r\x1b[2K\x1b[?25h");
    }
}

/// Resume the animation after the prompt — the emoji animates again during the
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

/// The static post-execution tool line: a ✓/✗ status glyph plus the 🔧 desc,
/// kept dim like the rest of the activity stream. Shared by `ToolSpinner::finish`
/// and the retroactive `reveal_last_turn`.
fn tool_result_line(desc: &str, is_error: bool) -> String {
    let mark = if is_error { "✗" } else { "✓" };
    format!("{mark} 🔧 {desc}")
}

fn describe_call(call: &crate::backend::ToolCall) -> String {
    let a = &call.args;
    match call.name.as_str() {
        "run_program" | "run_interactive" => {
            let args: Vec<&str> = a["args"]
                .as_array()
                .map(|v| v.iter().filter_map(|x| x.as_str()).collect())
                .unwrap_or_default();
            let argv = format!("{} {}", a["program"].as_str().unwrap_or("?"), args.join(" "));
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
        other => other.to_string(),
    }
}

/// The text echoed (dim) for a tool result's raw body. Empty results get a
/// placeholder so an error with no output still shows *something*.
fn raw_body(result: &ToolResult) -> &str {
    if result.content.trim().is_empty() {
        "(no output)"
    } else {
        result.content.as_str()
    }
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
    fn raw_body_placeholder() {
        let mk = |content: &str, is_error| ToolResult {
            id: "t".into(),
            content: content.into(),
            is_error,
        };
        assert_eq!(raw_body(&mk("hello", false)), "hello");
        assert_eq!(raw_body(&mk("", true)), "(no output)");
        assert_eq!(raw_body(&mk("   \n ", false)), "(no output)");
        // error results keep their content — they are included, not skipped
        assert_eq!(raw_body(&mk("boom", true)), "boom");
    }

    #[test]
    fn tool_result_line_marks_status() {
        assert_eq!(tool_result_line("read /etc/hosts", false), "✓ 🔧 read /etc/hosts");
        assert_eq!(tool_result_line("write x", true), "✗ 🔧 write x");
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
