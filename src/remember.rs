//! `:remember` — capture a rich, self-contained memory from a short operator
//! note PLUS the live session context.
//!
//! Where the `remember` tool stores exactly the text it's handed, the
//! `:remember` command ENRICHES first: it takes the operator's terse note and
//! the recent conversation + command history and asks the model to synthesize
//! ONE durable, self-contained memory — so a later `recall` surfaces something
//! useful months on, without the surrounding session to lean on. A note like
//! `:remember this fix` becomes a memory that names the actual file, decision,
//! and rationale it referred to.
//!
//! This module is the provider-agnostic primitive: pure prompt-shaping +
//! sanitising, plus ONE `backend.complete` call (no tools). The interactive
//! colon-command wiring lives in `repl`. Everything decision-shaped here is pure
//! and unit-tested; the model call degrades gracefully to storing the raw note
//! when the backend errors or declines.

use crate::backend::{Backend, Msg, Role};
use crate::session::Session;

/// How much recent conversation to feed the synthesizer. Bounded so a long
/// session can't blow the single completion's context.
const MAX_CONTEXT_MSGS: usize = 12;
/// Per-message truncation inside the context block.
const MAX_MSG_CHARS: usize = 500;
/// How many recent command inputs to include as grounding.
const MAX_RECENT_INPUTS: usize = 8;
/// Hard cap on the stored memory so an over-eager model can't persist a wall.
const MAX_MEMORY_CHARS: usize = 1200;

/// Strict instructions for the single-shot memory synthesis. The model emits
/// ONLY the memory text; a note it genuinely can't turn into a durable memory
/// maps to the `NONE` sentinel (caller falls back to the raw note).
const REMEMBER_SYSTEM: &str = "You are the memory-writer for aish, an AI-native shell. \
The operator wants to durably remember something. Given their short note and the recent \
session context, write ONE rich, self-contained memory that will still make sense months \
later with none of this session present.\n\n\
Rules:\n\
- Output ONLY the memory text. No preamble, no `Memory:` label, no markdown, no quotes, no code fences.\n\
- Write 1-3 sentences (a short paragraph at most). Be specific and concrete.\n\
- Fold in the essential context the note refers to — project, file paths, decisions, rationale, \
versions, IDs — drawn from the session. Never invent facts the context does not support.\n\
- Prefer durable facts, preferences, decisions, and lessons over transient state.\n\
- Third person, declarative, self-contained; start with the subject, not `I` or `the user`.\n\
- If the note is already a complete durable fact, lightly polish it; never pad.\n\
- If the note cannot be turned into a durable memory at all, output exactly: NONE";

/// Render the recent session context (conversation + typed commands) into a
/// compact block the synthesizer can lean on. Newest-relevant conversation is
/// kept; tool-only messages (no prose) are skipped. Pure + deterministic.
pub fn build_context_block(history: &[Msg], recent_inputs: &[String]) -> String {
    let mut out = String::new();

    let convo: Vec<&Msg> = history
        .iter()
        .filter(|m| !m.text.trim().is_empty())
        .collect();
    let start = convo.len().saturating_sub(MAX_CONTEXT_MSGS);
    if convo.len() > start {
        out.push_str("Recent conversation:\n");
        for m in &convo[start..] {
            let who = match m.role {
                Role::User => "user",
                Role::Assistant => "assistant",
            };
            let text = truncate(m.text.trim(), MAX_MSG_CHARS);
            out.push_str(who);
            out.push_str(": ");
            out.push_str(&text);
            out.push('\n');
        }
    }

    let inputs: Vec<&String> = recent_inputs
        .iter()
        .rev()
        .take(MAX_RECENT_INPUTS)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    if !inputs.is_empty() {
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str("Recent commands:\n");
        for i in inputs {
            out.push_str("- ");
            out.push_str(&truncate(i.trim(), MAX_MSG_CHARS));
            out.push('\n');
        }
    }

    out
}

/// Assemble the user-message text: the operator's note, then the context block
/// (when any). Pure so the prompt shape is unit-testable.
pub fn build_user_prompt(note: &str, context: &str) -> String {
    let note = note.trim();
    if context.trim().is_empty() {
        format!("Operator note to remember: {note}")
    } else {
        format!("Operator note to remember: {note}\n\n---\nSession context:\n{context}")
    }
}

/// Turn the model's raw reply into a storable memory, or `None` when it declined
/// (`NONE`) or returned nothing usable. Strips a `Memory:` label, surrounding
/// quotes/backticks, markdown fences, then bounds the length. Pure.
pub fn sanitize_memory(raw: &str) -> Option<String> {
    let mut s = raw.trim();

    // Strip a wrapping code fence if the whole reply is fenced.
    if let Some(inner) = s.strip_prefix("```") {
        if let Some(end) = inner.rfind("```") {
            s = inner[..end]
                .split_once('\n')
                .map_or(&inner[..end], |(_, body)| body)
                .trim();
        }
    }

    // Drop a leading label the model sometimes prepends.
    for label in ["Memory:", "memory:", "MEMORY:"] {
        if let Some(rest) = s.strip_prefix(label) {
            s = rest.trim();
            break;
        }
    }

    // Peel one layer of surrounding quotes / backticks.
    let s = s
        .trim_matches(|c| c == '"' || c == '`' || c == '\'')
        .trim();

    if s.is_empty() || s.eq_ignore_ascii_case("none") {
        return None;
    }

    Some(truncate(s, MAX_MEMORY_CHARS))
}

/// Truncate on a char boundary, appending an ellipsis when cut. Pure.
fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// The synthesized memory plus whether the model actually enriched it (vs. a
/// fallback to the raw note when the backend errored or declined).
pub struct Synthesized {
    pub text: String,
    pub enriched: bool,
}

/// Gather the session context, ask the model for ONE rich memory, and return it.
/// Never errors: on a backend failure or a `NONE`/empty reply it falls back to
/// the operator's raw note so `:remember` always stores *something*.
pub async fn synthesize(backend: &Backend, session: &Session, note: &str) -> Synthesized {
    let note = note.trim();
    let recent_inputs = session
        .db
        .as_ref()
        .and_then(|db| db.recent_inputs(MAX_RECENT_INPUTS).ok())
        .unwrap_or_default();
    let context = build_context_block(&session.history, &recent_inputs);
    let prompt = build_user_prompt(note, &context);

    match backend
        .complete(REMEMBER_SYSTEM, &[Msg::user(prompt)], &[])
        .await
    {
        Ok(turn) => match sanitize_memory(&turn.text) {
            Some(text) => Synthesized {
                text,
                enriched: true,
            },
            None => Synthesized {
                text: note.to_string(),
                enriched: false,
            },
        },
        Err(_) => Synthesized {
            text: note.to_string(),
            enriched: false,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn context_block_renders_convo_and_commands() {
        let history = vec![
            Msg::user("fix the OOM in the worktree build"),
            Msg {
                role: Role::Assistant,
                text: "use --no-default-features".to_string(),
                tool_calls: vec![],
                tool_results: vec![],
                raw: None,
            },
        ];
        let inputs = vec!["cargo build".to_string(), "cargo test".to_string()];
        let block = build_context_block(&history, &inputs);
        assert!(block.contains("Recent conversation:"));
        assert!(block.contains("user: fix the OOM"));
        assert!(block.contains("assistant: use --no-default-features"));
        assert!(block.contains("Recent commands:"));
        assert!(block.contains("- cargo build"));
    }

    #[test]
    fn context_block_skips_tool_only_messages_and_is_empty_when_nothing() {
        let history = vec![Msg::tool_results(vec![])];
        assert!(build_context_block(&history, &[]).is_empty());
    }

    #[test]
    fn user_prompt_includes_note_with_and_without_context() {
        assert_eq!(
            build_user_prompt("hi", "  "),
            "Operator note to remember: hi"
        );
        let p = build_user_prompt("hi", "Recent commands:\n- ls");
        assert!(p.contains("Operator note to remember: hi"));
        assert!(p.contains("Session context:"));
        assert!(p.contains("- ls"));
    }

    #[test]
    fn sanitize_strips_labels_quotes_and_fences() {
        assert_eq!(
            sanitize_memory("Memory: prefers dark mode"),
            Some("prefers dark mode".to_string())
        );
        assert_eq!(
            sanitize_memory("\"the build uses cargo\""),
            Some("the build uses cargo".to_string())
        );
        assert_eq!(
            sanitize_memory("```\nfenced fact\n```"),
            Some("fenced fact".to_string())
        );
    }

    #[test]
    fn sanitize_rejects_none_and_empty() {
        assert_eq!(sanitize_memory("NONE"), None);
        assert_eq!(sanitize_memory("  none  "), None);
        assert_eq!(sanitize_memory("   "), None);
    }

    #[test]
    fn truncate_respects_char_boundaries() {
        let long = "a".repeat(MAX_MEMORY_CHARS + 50);
        let out = sanitize_memory(&long).unwrap();
        assert_eq!(out.chars().count(), MAX_MEMORY_CHARS);
        assert!(out.ends_with('…'));
    }
}
