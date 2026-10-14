//! "What next?" — session context handed to the LLM as an *input*, not a command.
//!
//! The sibling primitive [`crate::suggest`] (`:suggest` / `:sg`) answers the
//! same question in the *command* register: it asks the model for ONE shell
//! line and prefills it in the editor so the operator can accept, edit, or drop
//! it. That is the right surface when the next move is a command you already
//! half-know.
//!
//! It is the wrong surface when the next move is a *judgement*: "what should I
//! do next here?" has answers like "the three cards in col_review are stale —
//! close them", or "nothing; CI is green, go ship". Those don't fit on a
//! command line, and forcing them to produces the worst failure mode of the
//! suggest path — a plausible-looking command the model invented because the
//! contract demanded one line of shell.
//!
//! So `:whatnext` (aliases `:next`, `:wn`) does NOT run a command and does not
//! prefill one. It builds a natural-language INPUT out of the session so far —
//! cwd, host OS, recent command lines, a bounded slice of the last output, plus
//! any focus the operator typed after the invocation — and hands that to the
//! model as a normal forced-model turn (the existing `?` escape hatch). The
//! model answers with its full toolset: it can read files, check `gh`, query
//! the board, and *then* say what's next — or just do the cheap reversible step
//! itself.
//!
//! Everything decision-shaped here is pure and unit-tested; `repl::run` owns
//! the one-line wiring (build the input, park it in `injected`, let the normal
//! dispatch path route it to the model).

use crate::session::Session;
use std::path::Path;

/// The invocation prefixes that trigger a "what next" turn. `:next` / `:wn` are
/// the terse aliases; `:what-next` is tolerated because people hyphenate it.
const PREFIXES: &[&str] = &[":whatnext", ":what-next", ":next", ":wn"];

/// How many recent command lines to fold into the input. Matches the suggest
/// path's window — enough to show a workflow without bloating the turn.
const RECENT_CONTEXT_LINES: usize = 10;

/// Cap (chars) on the last-output snippet folded into the input, head-first,
/// so one noisy command can't dominate the question.
const LAST_OUTPUT_SNIPPET: usize = 800;

/// Recognise a `:whatnext` / `:next` / `:wn` invocation and return any trailing
/// focus text (trimmed). A bare invocation returns `Some("")` — the common
/// "read the room" case; trailing text narrows it (`:wn just the PR`). A
/// non-invocation returns `None`. Pure.
pub fn parse_invocation(line: &str) -> Option<&str> {
    let trimmed = line.trim();
    for p in PREFIXES {
        if let Some(rest) = trimmed.strip_prefix(p) {
            // Must be a whole token: `:wn` exactly, or `:wn ` + focus — never a
            // longer word like `:next-thing` (which isn't this command).
            if rest.is_empty() {
                return Some("");
            }
            if let Some(after) = rest.strip_prefix(char::is_whitespace) {
                return Some(after.trim());
            }
        }
    }
    None
}

/// Assemble the natural-language input handed to the model: the question first
/// (so it survives any later truncation), then the grounding context — cwd, OS,
/// recent commands oldest→newest, a bounded last-output slice, any focus — and
/// finally the answer contract. Pure + deterministic so the shape is testable.
pub fn build_input(
    cwd: &Path,
    recent: &[String],
    last_output: Option<&str>,
    focus: &str,
) -> String {
    let focus = focus.trim();
    let mut p = String::from("What should I do next?\n");
    if !focus.is_empty() {
        p.push_str("Focus on: ");
        p.push_str(focus);
        p.push('\n');
    }
    p.push_str(&format!(
        "\nSession context:\n- Current directory: {}\n- Host OS: {}\n",
        cwd.display(),
        std::env::consts::OS
    ));
    if recent.is_empty() {
        p.push_str("- Recent commands: (none yet)\n");
    } else {
        p.push_str("- Recent commands (oldest first):\n");
        for line in recent {
            p.push_str("    ");
            p.push_str(line.trim());
            p.push('\n');
        }
    }
    if let Some(out) = last_output {
        let snippet = head_chars(out.trim(), LAST_OUTPUT_SNIPPET);
        if !snippet.is_empty() {
            p.push_str("- Last output:\n");
            p.push_str(&snippet);
            p.push('\n');
        }
    }
    p.push_str(
        "\nAnswer the question: name the most useful next move and why, briefly. \
Check anything you need with your tools first — don't guess at repo/CI/board state. \
Do the step yourself if it's cheap, safe, and reversible; otherwise say what to run. \
If the honest answer is \"nothing — you're done here\", say that.",
    );
    p
}

/// Pull the live session context and build the input. Thin glue over
/// [`build_input`] (which holds the testable shape): recent command lines from
/// the history DB, the last command output, the cwd.
pub fn build_from_session(session: &Session, focus: &str) -> String {
    let recent = session
        .db
        .as_ref()
        .and_then(|db| db.recent_inputs(RECENT_CONTEXT_LINES).ok())
        .unwrap_or_default();
    let last_output = session.last_output();
    build_input(&session.cwd, &recent, last_output.as_deref(), focus)
}

/// Wrap the input so the REPL's normal dispatch routes it to the model instead
/// of trying to run it: the `?` escape hatch ([`crate::repl`]'s `Route::Model`).
/// Keeping this here — rather than inlining `format!("?{input}")` at the call
/// site — pins the contract in one tested place. Pure.
pub fn injected_line(input: &str) -> String {
    format!("?{}", input.trim_start())
}

/// Keep the leading `max` chars of `s` (snapped to a char boundary), appending
/// an ellipsis marker when anything was dropped. Pure.
fn head_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let head: String = s.chars().take(max).collect();
    format!("{head}…")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn parse_invocation_recognises_the_family() {
        assert_eq!(parse_invocation(":whatnext"), Some(""));
        assert_eq!(parse_invocation(":what-next"), Some(""));
        assert_eq!(parse_invocation(":next"), Some(""));
        assert_eq!(parse_invocation(":wn"), Some(""));
        // trailing focus text is captured + trimmed
        assert_eq!(parse_invocation(":wn just the PR"), Some("just the PR"));
        assert_eq!(
            parse_invocation(":whatnext   the failing test  "),
            Some("the failing test")
        );
        // whitespace around the whole line is tolerated
        assert_eq!(parse_invocation("  :next  "), Some(""));
    }

    #[test]
    fn parse_invocation_rejects_non_invocations() {
        assert_eq!(parse_invocation("what next"), None); // no leading colon
        assert_eq!(parse_invocation(":wnx"), None); // not a whole token
        assert_eq!(parse_invocation(":nextcard"), None);
        assert_eq!(parse_invocation(":whatnextplease"), None);
        assert_eq!(parse_invocation(":suggest x"), None); // sibling command
        assert_eq!(parse_invocation("ls -la"), None);
        assert_eq!(parse_invocation(""), None);
    }

    #[test]
    fn build_input_leads_with_the_question_and_grounds_it() {
        let recent = vec!["cd repo".to_string(), "cargo test".to_string()];
        let p = build_input(
            &PathBuf::from("/tmp/work"),
            &recent,
            Some("test result: FAILED. 1 failed"),
            "",
        );
        // the question comes FIRST — it's an input to the model, not a command
        assert!(p.starts_with("What should I do next?"), "got: {p}");
        assert!(p.contains("- Current directory: /tmp/work"));
        assert!(p.contains("Host OS:"));
        assert!(p.contains("cargo test"), "recent commands present: {p}");
        assert!(p.contains("- Last output:"));
        assert!(p.contains("1 failed"));
        // and the answer contract is an ANSWER, not a command line
        assert!(p.contains("name the most useful next move"));
        assert!(p.contains("cheap, safe, and reversible"));
    }

    #[test]
    fn build_input_handles_empty_context_and_includes_focus() {
        let p = build_input(&PathBuf::from("/srv"), &[], None, "  the open PR  ");
        assert!(p.contains("- Recent commands: (none yet)"));
        assert!(!p.contains("- Last output:")); // no section without output
        assert!(p.contains("Focus on: the open PR"));
    }

    #[test]
    fn injected_line_forces_the_model_route() {
        // The `?` prefix is what makes repl::split_route pick Route::Model, so
        // the input is handed to the LLM instead of dispatched as a command.
        let line = injected_line("What should I do next?\ncontext…");
        assert!(line.starts_with('?'));
        assert!(line.contains("What should I do next?"));
        // Round-trips through the same split the REPL uses: strip the sigil and
        // the question survives intact.
        assert!(line[1..].starts_with("What should I do next?"));
    }

    #[test]
    fn head_chars_caps_long_text_with_marker() {
        assert_eq!(head_chars("short", 800), "short");
        let long = "x".repeat(LAST_OUTPUT_SNIPPET + 50);
        let out = head_chars(&long, LAST_OUTPUT_SNIPPET);
        assert!(out.ends_with('…'));
        assert_eq!(out.chars().count(), LAST_OUTPUT_SNIPPET + 1);
    }

    #[test]
    fn head_chars_snaps_to_char_boundary() {
        let mut s = "a".repeat(LAST_OUTPUT_SNIPPET - 1);
        s.push('é');
        s.push_str("bbb");
        let out = head_chars(&s, LAST_OUTPUT_SNIPPET);
        assert!(out.ends_with('…'));
        assert!(out.is_char_boundary(out.len() - '…'.len_utf8()));
    }

    #[test]
    fn long_last_output_is_bounded_in_the_input() {
        let noisy = "line\n".repeat(1000);
        let p = build_input(&PathBuf::from("/x"), &[], Some(&noisy), "");
        assert!(p.contains('…'), "snippet truncated: {}", p.len());
        // question + context stays well clear of a runaway turn
        assert!(p.len() < 2_000, "input stayed bounded: {}", p.len());
    }
}
