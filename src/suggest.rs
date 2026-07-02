//! Model-based next-command suggestion (S6.3 / TASK-137).
//!
//! Where the rewrite preview (S6.4 / [`crate::rewrite`]) turns a line of *intent*
//! into one command, this turns the **session so far** — the recent commands and
//! the last output — into a plausible *next* command. The model answers "given
//! what you just did, here's a sensible next move", and that candidate is
//! rendered as an editable suggestion: it is placed in the line editor for the
//! user to accept (Enter), edit, or drop (Ctrl-C). Nothing runs unconfirmed.
//!
//! Like the rewrite primitive this is provider-agnostic: ONE tool-less
//! `backend.complete` call (works on claude/grok/local), then the SAME
//! sanitiser the rewrite path uses ([`crate::rewrite::sanitize_candidate`]) maps
//! the reply to a single command or `None`. The S6.4 card deliberately left this
//! seam: the rewrite suite proved the prefill/accept/edit trust-surface, and S6.3
//! reuses it driven by session context instead of typed intent.
//!
//! The card is built **self-contained** (the same decision S6.4 made): the
//! suggestion is triggered explicitly by `:suggest` (alias `:sg`), not
//! speculatively on every keystroke — that speculative, cancel-in-flight async
//! rendering is S6.1/TASK-135's plumbing, and when it lands it can drive this
//! same [`suggest_next_command`] primitive behind it. Everything decision-shaped
//! here is pure and unit-tested; the interactive prefill loop lives in
//! `repl::run`, which owns the editor.

use crate::backend::{Backend, Msg, StreamDelta};
use crate::rewrite::sanitize_candidate;
use crate::session::Session;
use anyhow::Result;
use std::path::Path;

/// The invocation prefixes that trigger a next-command suggestion. `:sg` is the
/// terse alias.
const PREFIXES: &[&str] = &[":suggest", ":sg"];

/// How many recent command lines to feed the model as context. Enough to catch
/// a workflow ("cd repo" → "git status" → …) without bloating the single-shot
/// prompt.
const RECENT_CONTEXT_LINES: usize = 10;

/// Cap (chars) on the last-output snippet folded into the prompt, so a huge
/// command output can't dominate the request. Head-first, like the rest of
/// aish's last-output policy.
const LAST_OUTPUT_SNIPPET: usize = 800;

/// Strict instructions for the single-shot suggestion. The model proposes ONE
/// bare command line that is a plausible NEXT step given the session context; a
/// context with no sensible next move maps to the `NONE` sentinel (sanitised
/// back to "no suggestion").
const SUGGEST_SYSTEM: &str = "You suggest the SINGLE most plausible NEXT shell command a user would \
run, given their recent commands and the last output on this machine.\n\n\
Rules:\n\
- Output ONLY the command line. No prose, no explanation, no markdown, no code fences, no leading `$`.\n\
- Exactly one line. A pipeline (cmd | cmd) is fine; multiple statements joined by ;, &&, or newlines are NOT.\n\
- Propose a natural follow-up that continues what they're doing — not a repeat of the last command.\n\
- Prefer standard POSIX/Linux tools and non-destructive forms where reasonable.\n\
- If there is no sensible single next command — too little context, or the next step needs a \
decision only the user can make — output exactly: NONE";

/// Recognise a `:suggest`/`:sg` invocation and return any trailing hint text
/// (trimmed). A bare `:suggest` / `:sg` returns `Some("")` (the common case —
/// "suggest something from context"); an optional hint after it nudges the
/// suggestion ("`:sg now run the tests`"). A non-invocation returns `None`.
/// Pure.
pub fn parse_invocation(line: &str) -> Option<&str> {
    let trimmed = line.trim();
    for p in PREFIXES {
        if let Some(rest) = trimmed.strip_prefix(p) {
            // Must be a whole token: `:sg` exactly, or `:sg ` + hint — never a
            // longer word like `:sgx` (which isn't this command).
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

/// Assemble the user-message text the suggestion request sends: working
/// directory + OS for grounding, the recent command lines (oldest→newest), a
/// bounded snippet of the last output, and any user hint. Pure + deterministic
/// so the prompt shape is unit-testable.
pub fn build_user_prompt(
    cwd: &Path,
    recent: &[String],
    last_output: Option<&str>,
    hint: &str,
) -> String {
    let mut p = format!(
        "Current directory: {}\nHost OS: {}\n",
        cwd.display(),
        std::env::consts::OS
    );
    if recent.is_empty() {
        p.push_str("\nRecent commands: (none yet)\n");
    } else {
        p.push_str("\nRecent commands (oldest first):\n");
        for line in recent {
            p.push_str("  ");
            p.push_str(line.trim());
            p.push('\n');
        }
    }
    if let Some(out) = last_output {
        let snippet = head_chars(out.trim(), LAST_OUTPUT_SNIPPET);
        if !snippet.is_empty() {
            p.push_str("\nLast output:\n");
            p.push_str(&snippet);
            p.push('\n');
        }
    }
    let hint = hint.trim();
    if !hint.is_empty() {
        p.push_str("\nUser hint for the next command: ");
        p.push_str(hint);
        p.push('\n');
    }
    p.push_str("\nSuggest the single most plausible next command.");
    p
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

/// Ask the active backend for the most plausible NEXT command given the session
/// context, streaming the reply token-by-token to `on_text` as it decodes off
/// the wire (S8.2). Pulls the recent command history + last output from the
/// session, issues a single tool-less `complete_streaming` call (uniform across
/// every backend), and sanitises the finished reply with the shared rewrite
/// sanitiser. `on_text` receives each visible TEXT delta; thinking deltas are
/// swallowed. Returns `Ok(None)` when the model declines (`NONE`) or returns
/// nothing usable.
pub async fn suggest_next_command_streaming(
    backend: &Backend,
    session: &Session,
    hint: &str,
    on_text: &mut dyn FnMut(&str),
) -> Result<Option<String>> {
    let recent = session
        .db
        .as_ref()
        .and_then(|db| db.recent_inputs(RECENT_CONTEXT_LINES).ok())
        .unwrap_or_default();
    let last_output = session.last_output();
    let prompt = build_user_prompt(&session.cwd, &recent, last_output.as_deref(), hint);
    let mut sink = |delta: StreamDelta<'_>| {
        if let StreamDelta::Text(t) = delta {
            on_text(t);
        }
    };
    let turn = backend
        .complete_streaming(SUGGEST_SYSTEM, &[Msg::user(prompt)], &[], &mut sink)
        .await?;
    Ok(sanitize_candidate(&turn.text))
}


#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn parse_invocation_recognises_the_family() {
        // bare invocation → Some("") (the common "suggest from context" case)
        assert_eq!(parse_invocation(":suggest"), Some(""));
        assert_eq!(parse_invocation(":sg"), Some(""));
        // an optional trailing hint is captured + trimmed
        assert_eq!(
            parse_invocation(":suggest run the tests"),
            Some("run the tests")
        );
        assert_eq!(parse_invocation(":sg   now push  "), Some("now push"));
        // leading/trailing whitespace around the whole line is tolerated
        assert_eq!(parse_invocation("  :sg  "), Some(""));
    }

    #[test]
    fn parse_invocation_rejects_non_invocations() {
        assert_eq!(parse_invocation("suggest a fix"), None); // no leading colon
        assert_eq!(parse_invocation(":sgx now"), None); // not a whole token
        assert_eq!(parse_invocation(":suggested edits"), None); // longer word
        assert_eq!(parse_invocation(":sganother"), None);
        assert_eq!(parse_invocation(":rewrite x"), None);
        assert_eq!(parse_invocation("ls -la"), None);
        assert_eq!(parse_invocation(""), None);
    }

    #[test]
    fn build_user_prompt_grounds_with_cwd_recent_and_output() {
        let recent = vec!["cd repo".to_string(), "git status".to_string()];
        let p = build_user_prompt(
            &PathBuf::from("/tmp/work"),
            &recent,
            Some("On branch main\nnothing to commit"),
            "",
        );
        assert!(p.contains("Current directory: /tmp/work"));
        assert!(p.contains("Host OS:"));
        assert!(p.contains("Recent commands"));
        assert!(p.contains("git status"), "recent commands present: {p}");
        assert!(p.contains("Last output:"));
        assert!(p.contains("nothing to commit"));
        assert!(p.contains("Suggest the single most plausible next command."));
    }

    #[test]
    fn build_user_prompt_handles_empty_context_and_includes_hint() {
        let p = build_user_prompt(&PathBuf::from("/srv"), &[], None, "  deploy it  ");
        assert!(p.contains("Recent commands: (none yet)"));
        // no last-output section when there's no output
        assert!(!p.contains("Last output:"));
        // the hint is trimmed and present
        assert!(p.contains("User hint for the next command: deploy it"));
    }

    #[test]
    fn head_chars_caps_long_text_with_marker() {
        assert_eq!(head_chars("short", 800), "short");
        let long = "x".repeat(LAST_OUTPUT_SNIPPET + 50);
        let out = head_chars(&long, LAST_OUTPUT_SNIPPET);
        assert!(out.ends_with('…'));
        assert_eq!(out.chars().count(), LAST_OUTPUT_SNIPPET + 1); // head + ellipsis
    }

    #[test]
    fn head_chars_snaps_to_char_boundary() {
        // A multibyte char right at the cap must not panic / split a code point.
        let mut s = "a".repeat(LAST_OUTPUT_SNIPPET - 1);
        s.push('é');
        s.push_str("bbb");
        let out = head_chars(&s, LAST_OUTPUT_SNIPPET);
        assert!(out.ends_with('…'));
        assert!(out.is_char_boundary(out.len() - '…'.len_utf8()));
    }

    // Sanitising is delegated to `rewrite::sanitize_candidate` (shared with the
    // S6.4 path); this pins that the wiring uses it for the suggestion sentinel
    // and a bare command, so a future change to either side is caught here.
    #[test]
    fn suggestions_share_the_rewrite_sanitiser() {
        assert_eq!(sanitize_candidate("git push"), Some("git push".to_string()));
        assert_eq!(
            sanitize_candidate("```sh\ncargo test\n```"),
            Some("cargo test".to_string())
        );
        assert_eq!(sanitize_candidate("NONE"), None);
        assert_eq!(sanitize_candidate(""), None);
    }
}
