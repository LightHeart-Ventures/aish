//! Inline AI command-rewrite preview (S6.4 / TASK-138).
//!
//! fish expands an abbreviation when you press space; aish expands *intent* into
//! a concrete command — but as a **trust surface**: the model's candidate
//! command is shown in the editor and must be accepted or edited BEFORE it runs.
//! Nothing executes unconfirmed (see `docs/S6.4-rewrite-preview.md`).
//!
//! This module is the provider-agnostic primitive: given a line of intent it
//! issues ONE `backend.complete` call (no tools) and returns a single, sanitised
//! command line — or `None` when the intent can't be expressed as one command.
//! The interactive prefill/accept/edit loop lives in `repl::run`, which owns the
//! line editor; everything decision-shaped here is pure and unit-tested.

use crate::backend::{Backend, Msg};
use crate::session::Session;
use anyhow::Result;
use std::path::Path;

/// The invocation prefixes that trigger a rewrite. `:rw` is the terse alias.
const PREFIXES: &[&str] = &[":rewrite", ":rw"];

/// Strict instructions for the single-shot rewrite. The model must emit ONE bare
/// command line and nothing else; an intent that can't be a single command maps
/// to the `NONE` sentinel (sanitised back to "no candidate").
const REWRITE_SYSTEM: &str = "You translate a user's natural-language shell intent into a SINGLE \
concrete shell command line that accomplishes it on this machine.\n\n\
Rules:\n\
- Output ONLY the command line. No prose, no explanation, no markdown, no code fences, no leading `$`.\n\
- Exactly one line. A pipeline (cmd | cmd) is fine; multiple statements joined by ;, &&, or newlines are NOT.\n\
- Prefer standard POSIX/Linux tools and non-destructive forms where reasonable.\n\
- If the intent cannot be accomplished by a single command line — it needs investigation, several \
steps, or is a question — output exactly: NONE";

/// Recognise a `:rewrite`/`:rw` invocation and return the intent text after it
/// (trimmed of the leading space). A bare `:rewrite` / `:rw` with no argument
/// returns `Some("")` so the caller can show usage; a non-invocation returns
/// `None`. Pure.
pub fn parse_invocation(line: &str) -> Option<&str> {
    let trimmed = line.trim();
    for p in PREFIXES {
        if let Some(rest) = trimmed.strip_prefix(p) {
            // Must be a whole token: `:rw` exactly, or `:rw ` + intent — never a
            // longer word like `:rwxyz` (which isn't this command).
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

/// Assemble the user-message text the rewrite request sends: the working
/// directory and OS for grounding, then the intent. Pure + deterministic so the
/// prompt shape is unit-testable.
pub fn build_user_prompt(cwd: &Path, intent: &str) -> String {
    format!(
        "Current directory: {}\nHost OS: {}\n\nIntent: {}",
        cwd.display(),
        std::env::consts::OS,
        intent.trim()
    )
}

/// Turn the model's raw reply into a single runnable command, or `None` when it
/// declined (`NONE`) or returned nothing usable. Strips markdown code fences, a
/// leading shell prompt sigil (`$ ` / `% `), and surrounding whitespace, then
/// takes the first meaningful line. Pure.
pub fn sanitize_candidate(raw: &str) -> Option<String> {
    let defenced = strip_code_fences(raw);
    let line = defenced.lines().map(str::trim).find(|l| !l.is_empty())?;
    // Drop a copy-pasted prompt sigil if the model led with one.
    let line = line
        .strip_prefix("$ ")
        .or_else(|| line.strip_prefix("% "))
        .unwrap_or(line)
        .trim();
    if line.is_empty() || line.eq_ignore_ascii_case("none") {
        return None;
    }
    Some(line.to_string())
}

/// Remove whole-line markdown code fences (```` ``` ````-prefixed lines), so a
/// fenced reply collapses to just its body. Pure.
fn strip_code_fences(raw: &str) -> String {
    raw.lines()
        .filter(|l| !l.trim_start().starts_with("```"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Ask the active backend to rewrite `intent` into ONE concrete command. Issues
/// a single tool-less `complete` call (works on every backend) and sanitises the
/// reply. Returns `Ok(None)` when the model declines or returns nothing usable.
pub async fn rewrite_to_command(
    backend: &Backend,
    session: &Session,
    intent: &str,
) -> Result<Option<String>> {
    let prompt = build_user_prompt(&session.cwd, intent);
    let turn = backend
        .complete(REWRITE_SYSTEM, &[Msg::user(prompt)], &[])
        .await?;
    Ok(sanitize_candidate(&turn.text))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn parse_invocation_recognises_the_family() {
        assert_eq!(parse_invocation(":rewrite delete tmp files"), Some("delete tmp files"));
        assert_eq!(parse_invocation(":rw delete tmp files"), Some("delete tmp files"));
        // leading/trailing whitespace around the whole line and the intent is trimmed
        assert_eq!(parse_invocation("  :rw   find big files  "), Some("find big files"));
        // bare invocation → Some("") so the caller can print usage
        assert_eq!(parse_invocation(":rewrite"), Some(""));
        assert_eq!(parse_invocation(":rw"), Some(""));
    }

    #[test]
    fn parse_invocation_rejects_non_invocations() {
        assert_eq!(parse_invocation("rewrite the file"), None); // no leading colon
        assert_eq!(parse_invocation(":rwxyz now"), None); // not a whole token
        assert_eq!(parse_invocation(":rewritten history"), None); // longer word
        assert_eq!(parse_invocation(":mode dev"), None);
        assert_eq!(parse_invocation("ls -la"), None);
        assert_eq!(parse_invocation(""), None);
    }

    #[test]
    fn sanitize_takes_first_meaningful_line() {
        assert_eq!(sanitize_candidate("ls -la"), Some("ls -la".to_string()));
        assert_eq!(sanitize_candidate("  \n\n  ls -la  \n"), Some("ls -la".to_string()));
        // first non-empty line wins (defends against a stray trailing note)
        assert_eq!(
            sanitize_candidate("find . -name '*.tmp' -delete\n# done"),
            Some("find . -name '*.tmp' -delete".to_string())
        );
    }

    #[test]
    fn sanitize_strips_code_fences_and_prompt_sigils() {
        assert_eq!(
            sanitize_candidate("```sh\nrm -rf build\n```"),
            Some("rm -rf build".to_string())
        );
        assert_eq!(
            sanitize_candidate("```\ndu -sh *\n```"),
            Some("du -sh *".to_string())
        );
        assert_eq!(sanitize_candidate("$ git status"), Some("git status".to_string()));
        assert_eq!(sanitize_candidate("% pwd"), Some("pwd".to_string()));
    }

    #[test]
    fn sanitize_maps_none_sentinel_and_empty_to_no_candidate() {
        assert_eq!(sanitize_candidate("NONE"), None);
        assert_eq!(sanitize_candidate("none"), None);
        assert_eq!(sanitize_candidate("  None  "), None);
        assert_eq!(sanitize_candidate(""), None);
        assert_eq!(sanitize_candidate("   \n\t  "), None);
        // a fenced-only reply with nothing inside is also no candidate
        assert_eq!(sanitize_candidate("```\n```"), None);
    }

    #[test]
    fn build_user_prompt_grounds_with_cwd_and_intent() {
        let p = build_user_prompt(&PathBuf::from("/tmp/work"), "  list files  ");
        assert!(p.contains("Current directory: /tmp/work"));
        assert!(p.contains("Host OS:"));
        assert!(p.contains("Intent: list files"), "intent trimmed + present: {p}");
    }
}
