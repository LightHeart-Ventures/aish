use crate::backend::{Backend, Msg, Role, ToolResult};
use crate::session::Session;
use crate::tools::{self, Confirm};
use anyhow::Result;

const MAX_ITERATIONS: usize = 40; // runaway-loop backstop

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

        if turn.tool_calls.is_empty() {
            return Ok(turn.text);
        }

        // Interim narration from the model, shown dim.
        if !turn.text.trim().is_empty() {
            eprintln!("\x1b[2m{}\x1b[0m", crate::md::render(turn.text.trim(), "\x1b[2m"));
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
            let result = tools::execute(call, session, confirm).await;
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

/// Headless background coordinator: run one full agentic turn with the full
/// toolset (filesystem, run_program, MCP — the same tools an interactive turn
/// has), then — unlike `aish -c` — AWAIT every background batch this turn
/// offloaded before returning, so deferred sub-work isn't dropped when the
/// process exits. Confirmation is auto-allowed: the caller runs us unattended
/// (yolo, no TTY), so there is no one to answer a prompt. `run_id` identifies
/// this run in logs (and, later, the durable coordinator store).
pub async fn run_coordinator(
    backend: &Backend,
    session: &mut Session,
    input: String,
    run_id: &str,
) -> Result<()> {
    eprintln!("\x1b[2maish: coordinator run {run_id} starting\x1b[0m");
    let mut allow = |_: &str| tools::Decision::AllowOnce;
    let out = run_turn(backend, session, input, &mut allow).await?;
    println!("{}", crate::md::render_stdout(&out));
    // Await deferred sub-work: each batch's result auto-prints as it completes
    // (batch::on_complete), so we just block until none remain running. This is
    // the behavioral difference from `-c`, which would exit here and orphan them.
    crate::batch::await_all(&session.batch_jobs).await;
    Ok(())
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
struct ToolSpinner(Option<tokio::task::JoinHandle<()>>);

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
            return Self(None);
        }
        eprint!("\x1b[?25l"); // hide the cursor so it doesn't blink at the spinner's tail
        let desc = desc.to_string();
        Self(Some(tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_millis(120));
            for i in 0.. {
                tick.tick().await;
                eprint!("\r\x1b[2K\x1b[2m  {} {desc}\x1b[0m", TOOL_FRAMES[i % TOOL_FRAMES.len()]);
            }
        })))
    }

    /// Stop the animation and leave a static result line behind. On a TTY the
    /// spinning line is erased and replaced in place; piped, the static line
    /// was already printed at `start`, so we do nothing.
    fn finish(mut self, desc: &str, is_error: bool) {
        if let Some(h) = self.0.take() {
            h.abort();
            // Erase the spinner, print the result line, then restore the cursor.
            eprintln!("\r\x1b[2K\x1b[2m  {}\x1b[0m\x1b[?25h", tool_result_line(desc, is_error));
        }
    }
}

impl Drop for ToolSpinner {
    fn drop(&mut self) {
        // Only fires when `finish` wasn't called (e.g. the turn was aborted
        // mid-tool) — stop the animation and restore the cursor so it's never
        // left hidden.
        if let Some(h) = self.0.take() {
            h.abort();
            eprint!("\r\x1b[2K\x1b[?25h");
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
