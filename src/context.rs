//! Context-window awareness + history compaction (offload-to-SQLite).
//!
//! A shell session's `history` (see [`crate::session::Session::history`]) grows
//! with every turn — and an agentic turn can append many tool-call/tool-result
//! messages. Left unbounded it eventually overflows the model's context window
//! and every request fails. This module makes aish *context-aware*:
//!
//!   * [`Usage`] carries the token counts a backend reports for a completion, so
//!     the session knows how full the window is (see
//!     [`crate::session::Session::context_used`]).
//!   * [`should_compact`] decides, from that running figure and the model's
//!     [`context_window`], when the conversation is too large.
//!   * [`plan_compaction`] turns the oldest slice of history into (a) a full
//!     transcript to OFFLOAD into the SQLite `memories` table and (b) a short
//!     in-context summary message that replaces it — freeing the window while
//!     keeping the dropped content recoverable via the `recall` tool.
//!
//! The split logic is pure and unit-tested; the engine wires it to the DB.

use crate::backend::{Msg, Role};

/// Token usage reported by a backend for one completion. `input_tokens` is the
/// whole prompt the model saw this call (system + tools + history, incl. any
/// cached prefix); `output_tokens` is the reply it produced. Their sum is a good
/// proxy for "how full is the window heading into the next turn".
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Usage {
    pub input_tokens: usize,
    pub output_tokens: usize,
}

impl Usage {
    pub fn total(self) -> usize {
        self.input_tokens + self.output_tokens
    }
}

/// Approximate context window (in tokens) for a model id. Conservative when the
/// model is unknown — the point is a stable threshold to compact against, not an
/// exact accounting.
pub fn context_window(model: &str) -> usize {
    let m = model.to_ascii_lowercase();
    if m.contains("claude") || m.contains("opus") || m.contains("sonnet") || m.contains("haiku") {
        200_000
    } else if m.contains("grok") {
        131_072
    } else {
        // Local / unknown model — assume a small window so compaction kicks in
        // early rather than letting an overflow fail the request.
        8_192
    }
}

/// Percentage of the window at which the conversation is compacted. Chosen so a
/// compaction frees real room while a working tail of recent turns is retained.
pub const COMPACT_THRESHOLD_PCT: usize = 75;

/// How many of the most-recent history messages a compaction always keeps
/// in-context (the live working set the next turn most likely references).
pub const KEEP_RECENT_MSGS: usize = 12;

/// True when `used` tokens have reached `threshold_pct`% of `window`.
pub fn should_compact(used: usize, window: usize, threshold_pct: usize) -> bool {
    window > 0 && used.saturating_mul(100) >= window.saturating_mul(threshold_pct)
}

/// Rough token estimate for a string (~4 chars/token). The fallback when a
/// backend doesn't report [`Usage`].
pub fn estimate_text_tokens(s: &str) -> usize {
    s.chars().count().div_ceil(4)
}

/// Estimate of the tokens a whole history occupies — text plus tool-call and
/// tool-result payloads. Used to re-seat the running figure right after a
/// compaction (when the exact next-turn usage isn't known yet).
pub fn estimate_history_tokens(history: &[Msg]) -> usize {
    let mut n = 0;
    for m in history {
        n += estimate_text_tokens(&m.text);
        for c in &m.tool_calls {
            n += estimate_text_tokens(&c.name) + estimate_text_tokens(&c.args.to_string());
        }
        for r in &m.tool_results {
            n += estimate_text_tokens(&r.content);
        }
    }
    n
}

/// The largest prefix length `split` such that compacting `history[..split]`:
///   * keeps at least `keep_recent` messages in `history[split..]`, and
///   * lands on an Assistant message at `history[split]`.
///
/// Landing on an Assistant boundary is what keeps the remaining conversation
/// valid for the API: a `tool_result` (user) message is only ever valid when the
/// immediately-preceding assistant message carried the matching `tool_use`, so a
/// cut must never separate that pair. An Assistant message has no such backward
/// dependency, and replacing the dropped prefix with a single synthetic *user*
/// summary then yields a clean `user → assistant → …` alternation.
///
/// Returns `None` when the conversation is too short to compact safely.
pub fn compaction_split(history: &[Msg], keep_recent: usize) -> Option<usize> {
    let len = history.len();
    if len <= keep_recent + 1 {
        return None;
    }
    // Keep [split..] with len-split >= keep_recent  ⇒  split <= len - keep_recent.
    let mut s = (len - keep_recent).min(len - 1);
    while s >= 1 {
        if history[s].role == Role::Assistant {
            return Some(s);
        }
        s -= 1;
    }
    None
}

/// A planned compaction: what to persist and what to leave in-context.
pub struct Compaction {
    /// Full, role-tagged transcript of the dropped messages — written verbatim
    /// to the SQLite `memories` table so nothing is actually lost.
    pub offload: String,
    /// The single synthetic user message that replaces the dropped prefix.
    pub summary_msg: Msg,
    /// How many leading messages are dropped (the splice length).
    pub dropped: usize,
}

/// Plan a compaction of the oldest part of `history`, or `None` when it's too
/// short. Pure: the caller persists `offload` and applies `summary_msg`.
pub fn plan_compaction(history: &[Msg], keep_recent: usize) -> Option<Compaction> {
    let split = compaction_split(history, keep_recent)?;
    let dropped = &history[..split];
    Some(Compaction {
        offload: flatten_for_offload(dropped),
        summary_msg: Msg::user(inline_summary(dropped)),
        dropped: split,
    })
}

/// Apply a planned compaction in place: replace `history[..c.dropped]` with the
/// single summary message.
pub fn apply_compaction(history: &mut Vec<Msg>, c: &Compaction) {
    history.splice(0..c.dropped, std::iter::once(c.summary_msg.clone()));
}

/// Role-tagged, flattened transcript of `msgs` for durable offload. Includes
/// assistant text, the names of any tools it called, and tool-result bodies, so
/// a later `recall` surfaces the substance of the dropped conversation.
fn flatten_for_offload(msgs: &[Msg]) -> String {
    let mut out = String::from("[context-offload] compacted conversation transcript:\n");
    for m in msgs {
        match m.role {
            Role::User => {
                if m.tool_results.is_empty() {
                    if !m.text.trim().is_empty() {
                        out.push_str("USER: ");
                        out.push_str(m.text.trim());
                        out.push('\n');
                    }
                } else {
                    for r in &m.tool_results {
                        out.push_str("TOOL_RESULT: ");
                        out.push_str(r.content.trim());
                        out.push('\n');
                    }
                }
            }
            Role::Assistant => {
                if !m.text.trim().is_empty() {
                    out.push_str("ASSISTANT: ");
                    out.push_str(m.text.trim());
                    out.push('\n');
                }
                for c in &m.tool_calls {
                    out.push_str("ASSISTANT_TOOL: ");
                    out.push_str(&c.name);
                    out.push('\n');
                }
            }
        }
    }
    out
}

/// Short, in-context replacement for the dropped prefix. Tells the model (and
/// the reader) that history was compacted and how to get it back, and echoes the
/// first couple of user asks so the thread stays coherent.
fn inline_summary(msgs: &[Msg]) -> String {
    let asks: Vec<String> = msgs
        .iter()
        .filter(|m| m.role == Role::User && m.tool_results.is_empty())
        .filter(|m| !m.text.trim().is_empty())
        .take(3)
        .map(|m| {
            let line = m.text.trim().lines().next().unwrap_or("").trim();
            let brief: String = line.chars().take(100).collect();
            format!("  • {brief}")
        })
        .collect();
    let earlier = if asks.is_empty() {
        String::new()
    } else {
        format!("\nEarlier you asked about:\n{}", asks.join("\n"))
    };
    format!(
        "[Context compacted: {} earlier message(s) were offloaded to long-term memory to free \
context. Use the recall tool with query \"context-offload\" (or tag \"context-offload\") to \
retrieve the recent offloaded transcript(s) if you need them — each is truncated, so narrow your \
ask if you need more.{earlier}]",
        msgs.len()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::{Msg, ToolCall, ToolResult};
    use serde_json::json;

    fn assistant_call(text: &str, tool: &str) -> Msg {
        Msg {
            role: Role::Assistant,
            text: text.into(),
            tool_calls: vec![ToolCall {
                id: "t1".into(),
                name: tool.into(),
                args: json!({}),
            }],
            tool_results: vec![],
            raw: None,
        }
    }
    fn tool_results(content: &str) -> Msg {
        Msg::tool_results(vec![ToolResult::text("t1", content, false)])
    }

    #[test]
    fn window_sizes_by_model_family() {
        assert_eq!(context_window("claude-haiku-4-5"), 200_000);
        assert_eq!(context_window("claude-opus-4-8"), 200_000);
        assert_eq!(context_window("grok-4"), 131_072);
        assert_eq!(context_window("some-local-gguf"), 8_192);
    }

    #[test]
    fn should_compact_thresholds_on_percentage() {
        // 75% of 1000 = 750.
        assert!(!should_compact(749, 1000, 75));
        assert!(should_compact(750, 1000, 75));
        assert!(should_compact(900, 1000, 75));
        // A zero/unknown window never triggers (avoids div-by-zero surprises).
        assert!(!should_compact(10, 0, 75));
    }

    #[test]
    fn estimate_counts_text_and_tool_payloads() {
        let h = vec![Msg::user("aaaa"), assistant_call("bbbb", "read_file")];
        // 4 chars → 1 token for the user text; assistant text 4 chars → 1, plus
        // the tool name + args. The exact figure isn't load-bearing; it must be
        // non-zero and grow with content.
        assert!(estimate_history_tokens(&h) >= 2);
        assert!(estimate_history_tokens(&h) > estimate_history_tokens(&h[..1]));
    }

    #[test]
    fn compaction_split_returns_none_when_short() {
        let h = vec![Msg::user("hi"), assistant_call("yo", "x")];
        assert_eq!(compaction_split(&h, KEEP_RECENT_MSGS), None);
    }

    #[test]
    fn compaction_split_lands_on_assistant_and_keeps_recent() {
        // Build: user, assistant(tool), tool_results, assistant(final), then a
        // long tail of (user, assistant) pairs.
        let mut h = vec![
            Msg::user("first prompt"),
            assistant_call("calling", "read_file"),
            tool_results("file body"),
            Msg {
                role: Role::Assistant,
                text: "done".into(),
                tool_calls: vec![],
                tool_results: vec![],
                raw: None,
            },
        ];
        for i in 0..14 {
            h.push(Msg::user(format!("q{i}")));
            h.push(Msg {
                role: Role::Assistant,
                text: format!("a{i}"),
                tool_calls: vec![],
                tool_results: vec![],
                raw: None,
            });
        }
        let split = compaction_split(&h, 12).expect("should compact a long history");
        // Keeps at least keep_recent messages.
        assert!(h.len() - split >= 12);
        // Lands on an Assistant message — never mid tool_use/tool_result pair.
        assert_eq!(h[split].role, Role::Assistant);
    }

    #[test]
    fn plan_and_apply_offloads_and_replaces_prefix() {
        let mut h = vec![
            Msg::user("how do I build this"),
            assistant_call("let me look", "read_file"),
            tool_results("Cargo.toml contents"),
            Msg {
                role: Role::Assistant,
                text: "use cargo build".into(),
                tool_calls: vec![],
                tool_results: vec![],
                raw: None,
            },
        ];
        for i in 0..14 {
            h.push(Msg::user(format!("q{i}")));
            h.push(Msg {
                role: Role::Assistant,
                text: format!("a{i}"),
                tool_calls: vec![],
                tool_results: vec![],
                raw: None,
            });
        }
        let before = h.len();
        let plan = plan_compaction(&h, 12).expect("plan");
        // Offload transcript carries the substance: a user ask, the tool name,
        // a tool result, and assistant text.
        assert!(plan.offload.contains("USER: how do I build this"));
        assert!(plan.offload.contains("ASSISTANT_TOOL: read_file"));
        assert!(plan.offload.contains("TOOL_RESULT: Cargo.toml contents"));
        // The inline summary names the recall query and echoes the first ask.
        let s = &plan.summary_msg.text;
        assert!(s.contains("context-offload"));
        assert!(s.contains("how do I build this"));

        let dropped = plan.dropped;
        apply_compaction(&mut h, &plan);
        // History shrank by (dropped - 1): the prefix became one summary message.
        assert_eq!(h.len(), before - dropped + 1);
        // The retained conversation still starts cleanly: summary (user) then an
        // assistant message — valid alternation for the API.
        assert_eq!(h[0].role, Role::User);
        assert_eq!(h[1].role, Role::Assistant);
        // No orphaned tool_result leads the retained tail.
        assert!(h[1].tool_results.is_empty());
    }
}
