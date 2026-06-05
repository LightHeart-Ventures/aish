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
    let mut tool_defs = tools::tool_defs();
    if backend.include_mcp_tools() {
        tool_defs.extend(session.mcp.tool_defs());
    }
    session.history.push(Msg::user(input));
    session.last_turn_tools.clear();

    for _ in 0..MAX_ITERATIONS {
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
            eprintln!("\x1b[2m  🔧 {desc}\x1b[0m");
            let result = tools::execute(call, session, confirm).await;
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

/// Transient "⠋ thinking…" line on stderr while the model is working.
/// TTY-gated; erased on drop (first token, tool call, or turn abort).
struct Spinner(Option<tokio::task::JoinHandle<()>>);

impl Spinner {
    fn start() -> Self {
        // SAFETY: plain isatty query.
        if unsafe { libc::isatty(2) } != 1 {
            return Self(None);
        }
        Self(Some(tokio::spawn(async {
            const FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
            let mut tick = tokio::time::interval(std::time::Duration::from_millis(80));
            for i in 0.. {
                tick.tick().await;
                eprint!("\r\x1b[2m{} thinking…\x1b[0m", FRAMES[i % FRAMES.len()]);
            }
        })))
    }
}

impl Drop for Spinner {
    fn drop(&mut self) {
        if let Some(h) = self.0.take() {
            h.abort();
            eprint!("\r\x1b[2K"); // erase the spinner line
        }
    }
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
        eprintln!("\x1b[2m  🔧 {desc}\x1b[0m");
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
}
