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
    /// Portion of `input_tokens` that was served from the model's prompt cache
    /// (Anthropic `cache_read_input_tokens`). A high fraction of the input means
    /// the stable system+tools prefix is being reused instead of re-billed at
    /// full rate. Zero when the backend reports no cache buckets. (TASK-320)
    pub cache_read_tokens: usize,
    /// Portion of `input_tokens` spent WRITING new entries into the prompt cache
    /// (Anthropic `cache_creation_input_tokens`) — billed at a premium, paid once
    /// when a fresh prefix is cached. Zero when unreported. (TASK-320)
    pub cache_creation_tokens: usize,
}

impl Usage {
    pub fn total(self) -> usize {
        self.input_tokens + self.output_tokens
    }

    /// Cache-read hit rate for THIS usage: cached-read tokens as a fraction of
    /// the full input prompt, 0.0–100.0. Returns `None` when there was no input
    /// to measure against. AC #3's measurement primitive (TASK-320).
    #[allow(dead_code)]
    pub fn cache_hit_pct(self) -> Option<f64> {
        if self.input_tokens == 0 {
            None
        } else {
            Some((self.cache_read_tokens as f64 / self.input_tokens as f64) * 100.0)
        }
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
    } else if m.contains("gpt-5") || m.contains("gpt-4.1") || m.contains("o3") || m.contains("o4") {
        // OpenAI GPT-5 / GPT-4.1 / o-series expose very large windows.
        1_000_000
    } else if m.contains("gpt-4o") || m.contains("gpt-4") {
        128_000
    } else if m.contains("/") {
        // OpenRouter model slugs are "vendor/model"; assume a modern large window.
        128_000
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

/// Default ceiling on the number of tool calls retained *in context* before a
/// compaction is forced — independent of the % window. This is TASK-321's core
/// lever: on a large-window model (Claude, 200k) the %-window trigger alone
/// doesn't fire until ~150k tokens are live, by which point a tool-heavy
/// coordinator run has already re-sent a growing transcript across ~100+ turns
/// and billed millions of cumulative input tokens. Capping the live tool-call
/// count compacts *far* earlier, bounding per-turn transcript size (and thus the
/// quadratic cumulative cost of a run). Overridable via `AISH_COMPACT_TOOL_CALLS`
/// (`off`/`none`/`0` disables). (TASK-321)
pub const COMPACT_TOOL_CALL_CEILING: usize = 50;

/// Default absolute cumulative-token ceiling before a compaction is forced,
/// independent of the % window. `0` (the default) leaves the token lever to the
/// existing percentage-of-window trigger (itself a token budget, expressed
/// relative to the model). Set an absolute figure via `AISH_COMPACT_TOKEN_BUDGET`
/// to compact at a fixed token count regardless of window size. (TASK-321)
pub const COMPACT_TOKEN_CEILING: usize = 0;

/// Default ceiling on the number of *messages* retained in context before a
/// compaction is forced — a structural backstop that is independent of every
/// token estimate. (ISS-409752)
///
/// The other three levers all measure CONTENT: percentage of window, absolute
/// tokens, and in-context tool calls. None of them bounds `session.history.len()`
/// itself, so a long agentic turn made of many *small* rounds can grow the
/// transcript without limit:
///   * the %-window lever needs ~150k live tokens on a 200k model (and far more
///     on a 1M-token window) before it fires;
///   * the absolute token lever is OFF by default (`COMPACT_TOKEN_CEILING == 0`);
///   * the tool-call lever is explicitly disablable via
///     `AISH_COMPACT_TOOL_CALLS=off` — a documented, supported operator setting.
///
/// With a large window and the tool-call lever off, in-turn history growth had
/// no ceiling at all: every round appended an assistant message plus its
/// tool-result message and nothing ever reclaimed them, which is the unbounded
/// `run_turn_inner` growth reported in ISS-409752. A message COUNT is O(1) to
/// read, cannot be fooled by estimator error, and degrades gracefully — it only
/// ever makes compaction fire *earlier*.
///
/// The default is deliberately well above [`KEEP_RECENT_MSGS`] so a normal
/// interactive turn never trips it; it exists to catch runaway loops.
/// Overridable via `AISH_COMPACT_MAX_MSGS` (`off`/`none`/`0` disables).
pub const COMPACT_MSG_CEILING: usize = 120;

/// Parse a compaction-ceiling override from an env value: `None`/empty → the
/// compiled-in `default`; `off`/`none` (case-insensitive) → `0` (lever disabled);
/// a valid non-negative integer → that value (`0` also disables); anything else
/// → `default` (a typo must not silently drop the guard). (TASK-321)
pub fn parse_ceiling(raw: Option<&str>, default: usize) -> usize {
    match raw.map(str::trim) {
        None | Some("") => default,
        Some(s) if s.eq_ignore_ascii_case("off") || s.eq_ignore_ascii_case("none") => 0,
        Some(s) => s.parse::<usize>().unwrap_or(default),
    }
}

/// Which lever tripped a compaction — surfaced in the log line so an operator
/// can see whether the context window, the tool-call cap, or the token cap fired.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompactTrigger {
    /// Live token usage reached `threshold_pct`% of the model window.
    WindowPct,
    /// Live in-context tool-call count reached the tool-call ceiling.
    ToolCalls,
    /// Live token usage reached the absolute token ceiling.
    TokenBudget,
    /// In-context message count reached the message ceiling. The structural
    /// backstop: fires regardless of how small each message is. (ISS-409752)
    MsgCount,
}

impl CompactTrigger {
    /// Short human label for the compaction log line.
    pub fn label(self) -> &'static str {
        match self {
            Self::WindowPct => "context window",
            Self::ToolCalls => "tool-call cap",
            Self::TokenBudget => "token budget",
            Self::MsgCount => "message cap",
        }
    }
}

/// A resolved compaction budget: the three OR'd ceilings the engine evaluates
/// before every model call. A ceiling of `0` disables that individual lever
/// (`window == 0` disables the percentage lever, matching [`should_compact`]).
#[derive(Debug, Clone, Copy)]
pub struct CompactBudget {
    /// Model context window (tokens); `0` = unknown/disabled.
    pub window: usize,
    /// Percentage of `window` at which to compact.
    pub threshold_pct: usize,
    /// Max tool calls retained in-context before forcing a compaction; `0` = off.
    pub tool_call_ceiling: usize,
    /// Absolute live-token ceiling before forcing a compaction; `0` = off.
    pub token_ceiling: usize,
    /// Max messages retained in-context before forcing a compaction; `0` = off.
    /// Structural backstop, independent of any token estimate. (ISS-409752)
    pub msg_ceiling: usize,
}

impl CompactBudget {
    /// Decide whether — and why — to compact now. `used` is the running live
    /// token figure; `tool_calls_in_context` is how many tool calls the current
    /// (uncompacted) transcript still carries; `msgs_in_context` is
    /// `history.len()`. The tool-call cap is checked first (the cheap, early
    /// lever that bounds cost before the window fills), then the structural
    /// message cap, then the absolute token cap, then the percentage window.
    /// `None` ⇒ leave history intact.
    pub fn trigger(
        &self,
        used: usize,
        tool_calls_in_context: usize,
        msgs_in_context: usize,
    ) -> Option<CompactTrigger> {
        if self.tool_call_ceiling > 0 && tool_calls_in_context >= self.tool_call_ceiling {
            return Some(CompactTrigger::ToolCalls);
        }
        // Structural backstop BEFORE the token levers: it is the one lever that
        // still fires when every content-based estimate says "plenty of room"
        // (huge window, token lever off, tool-call lever disabled). (ISS-409752)
        if self.msg_ceiling > 0 && msgs_in_context >= self.msg_ceiling {
            return Some(CompactTrigger::MsgCount);
        }
        if self.token_ceiling > 0 && used >= self.token_ceiling {
            return Some(CompactTrigger::TokenBudget);
        }
        if should_compact(used, self.window, self.threshold_pct) {
            return Some(CompactTrigger::WindowPct);
        }
        None
    }
}

/// Rough token estimate for a string (~4 bytes/token). The fallback when a
/// backend doesn't report [`Usage`].
///
/// Deliberately measures BYTES (`str::len`, O(1)) rather than chars
/// (`chars().count()`, O(n)). [`enforce_prompt_ceiling`] re-estimates the WHOLE
/// history before every model call, so a char-based scan walked every byte of
/// every retained tool result on every round — O(total transcript bytes) per
/// iteration, tens of megabytes of UTF-8 decoding across a tool-heavy turn.
/// Byte length is a field read, collapsing that to O(#messages). (ISS-409753)
///
/// Byte count is also the SAFER proxy: it is never lower than the char count,
/// so the overflow guard can only become more conservative, and for multibyte
/// text (CJK, emoji) real BPE tokenizers emit far more tokens per *char* than
/// 1/4 — `chars()/4` badly under-counts there, `len()/4` does not.
pub fn estimate_text_tokens(s: &str) -> usize {
    s.len().div_ceil(4)
}

/// Estimate of the tokens a whole history occupies — text plus tool-call and
/// tool-result payloads. Used to re-seat the running figure right after a
/// compaction (when the exact next-turn usage isn't known yet).
pub fn estimate_history_tokens(history: &[Msg]) -> usize {
    let mut n = 0;
    for m in history {
        n += estimate_text_tokens(&m.text);
        for c in &m.tool_calls {
            n += estimate_text_tokens(&c.name) + json_bytes(&c.args).div_ceil(4);
        }
        for r in &m.tool_results {
            n += estimate_text_tokens(&r.content);
        }
    }
    n
}

/// Serialized-JSON byte length of `v`, computed WITHOUT serializing it.
///
/// `Value::to_string()` allocates a fresh `String` for EVERY tool call in the
/// retained transcript on EVERY estimate — and tool args routinely carry whole
/// file bodies (`write_file`), so one estimate could copy megabytes, dozens of
/// times per turn. Walking the tree and summing lengths is allocation-free and
/// O(#nodes) instead of O(#bytes-copied). (ISS-409753)
///
/// Deliberately approximate, and biased to OVER-count (escape sequences are not
/// expanded, numbers are charged a flat width) so the overflow guard can only
/// become more conservative — never less.
fn json_bytes(v: &serde_json::Value) -> usize {
    match v {
        serde_json::Value::Null => 4,
        serde_json::Value::Bool(b) => {
            if *b {
                4
            } else {
                5
            }
        }
        // Flat width rather than formatting the number: cheap and over-counts.
        serde_json::Value::Number(_) => 8,
        // +2 for the surrounding quotes.
        serde_json::Value::String(s) => s.len() + 2,
        // Brackets + the `,` separators between elements.
        serde_json::Value::Array(a) => {
            2 + a.len().saturating_sub(1) + a.iter().map(json_bytes).sum::<usize>()
        }
        // Braces + separators; each key costs its bytes plus `"":`.
        serde_json::Value::Object(o) => {
            2 + o.len().saturating_sub(1)
                + o.iter()
                    .map(|(k, val)| k.len() + 3 + json_bytes(val))
                    .sum::<usize>()
        }
    }
}

/// Count the tool calls carried by a history slice — the assistant-side tool
/// invocations across every message. Used to re-seat the tool-call watermark
/// after a compaction so the *in-context* tool-call count (`tool_calls_total -
/// tool_calls_at_last_compact`) reflects only the calls the retained transcript
/// still carries. (TASK-321)
pub fn count_tool_calls(history: &[Msg]) -> usize {
    history.iter().map(|m| m.tool_calls.len()).sum()
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
    // Reclaim the Vec spine. `splice` shifts the retained tail down but leaves
    // capacity at the run's high-water mark, so a turn that peaked at thousands
    // of messages kept holding that allocation for the rest of the session even
    // though compaction had dropped the contents. Shrinking here is what makes
    // the compaction actually return memory to the allocator. (ISS-409752)
    history.shrink_to_fit();
}

// ---------------------------------------------------------------------------
// Pre-flight overflow guard
//
// The three levers above are REACTIVE: they judge from the usage the backend
// reported for the *previous* round. That is fine for gradual growth, but a
// single round can append several large tool results and leap from "under
// threshold" straight past the window — the next request then dies with
//     claude api invalid_request_error (400): prompt is too long:
//     225423 tokens > 200000 maximum
// before any compaction has run. Worse, `plan_compaction` returns `None` for a
// short-but-huge history (a handful of oversized messages), so the reactive
// path can *silently no-op forever* while every request 400s.
//
// The guard below sizes the prompt that is ABOUT to be sent and compacts —
// repeatedly, and with a shrinking retained tail — until it fits, clamping
// oversized tool-result bodies as a last resort.
// ---------------------------------------------------------------------------

/// Hard pre-flight ceiling on the prompt, as a percentage of the model window.
/// Below 100% to leave headroom for estimator error (~4 chars/token is an
/// approximation) and for the reply, which shares the window with the prompt.
pub const PROMPT_CEILING_PCT: usize = 85;

/// Floor on how many recent messages an EMERGENCY compaction retains, used when
/// keeping [`KEEP_RECENT_MSGS`] still leaves the prompt over the ceiling. Two is
/// the smallest tail that can still carry a valid assistant → tool_result pair.
pub const MIN_KEEP_RECENT_MSGS: usize = 2;

/// Last-resort per-tool-result token cap. Applied in place to the retained tail
/// when there is nothing left to drop but the prompt is still over the ceiling
/// (e.g. one `read_file` of a giant file inside the working set).
pub const CLAMP_RESULT_TOKENS: usize = 4_000;

/// Marker appended to a tool result truncated by [`clamp_oversized_results`].
/// Also the idempotency guard: an already-clamped body is left alone.
pub const CLAMP_MARKER: &str =
    "\n[…truncated by aish to fit the context window — re-run the tool with a narrower range]";

/// Absolute token ceiling for a prompt against `window`. `0` when the window is
/// unknown (lever disabled).
pub fn prompt_ceiling(window: usize) -> usize {
    window.saturating_mul(PROMPT_CEILING_PCT) / 100
}

/// Estimate of the full prompt a round will send: system + tool schemas +
/// history. `tool_tokens` comes from [`crate::mcp::tool_defs_token_estimate`].
pub fn estimate_prompt_tokens(system: &str, tool_tokens: usize, history: &[Msg]) -> usize {
    estimate_text_tokens(system) + tool_tokens + estimate_history_tokens(history)
}

/// Same estimate as [`estimate_prompt_tokens`], but ANCHORED on the token count
/// the backend last reported so only the newly-appended tail has to be sized.
///
/// [`estimate_prompt_tokens`] re-scans the entire transcript, and the pre-flight
/// guard calls it before every model call — so a tool-heavy turn re-scanned a
/// growing history on each of its ~50 iterations (O(n) work on O(n) growth).
/// `base` is the exact figure the backend reported for the prompt it last saw
/// (`session.context_used`) and `mark` is `history.len()` at that instant, so
/// everything below `mark` is already accounted for and only `history[mark..]`
/// — normally one or two messages — needs estimating. (ISS-409753)
///
/// Anchoring is also MORE accurate than the pure estimate: the base is real
/// tokenizer output rather than a ~4-bytes/token approximation.
///
/// Falls back to the full scan whenever the anchor is unusable: `base == 0` (no
/// usage reported yet this session) or `mark > history.len()` (history was
/// compacted or cleared without re-seating), so a stale mark can never silently
/// under-count.
pub fn estimate_prompt_tokens_anchored(
    base: usize,
    mark: usize,
    system: &str,
    tool_tokens: usize,
    history: &[Msg],
) -> usize {
    if base == 0 || mark > history.len() {
        return estimate_prompt_tokens(system, tool_tokens, history);
    }
    // `base` already covers the system prompt and tool schemas as the model saw
    // them, so only the appended messages are added here.
    base + estimate_history_tokens(&history[mark..])
}

/// True when a backend error is a context-window overflow rejection — the class
/// that compaction can actually fix, so the engine retries instead of failing
/// the turn. Matches the wording used by Anthropic, OpenAI/OpenRouter and Grok.
pub fn is_context_overflow_error(err: &str) -> bool {
    let e = err.to_ascii_lowercase();
    e.contains("prompt is too long")
        || e.contains("context length exceeded")
        || e.contains("context_length_exceeded")
        || e.contains("maximum context length")
        || e.contains("exceeds the context window")
        || e.contains("too many tokens")
}

/// Truncate any tool-result body longer than `max_tokens` (~4 chars/token) in
/// place, appending [`CLAMP_MARKER`]. Idempotent: an already-clamped body is
/// skipped, so repeated calls converge instead of looping. Returns how many
/// bodies were clamped this call (`0` ⇒ no further progress is possible).
pub fn clamp_oversized_results(history: &mut [Msg], max_tokens: usize) -> usize {
    let max_chars = max_tokens.saturating_mul(4);
    let marker_len = CLAMP_MARKER.chars().count();
    if max_chars <= marker_len {
        return 0;
    }
    let head_chars = max_chars - marker_len;
    let mut clamped = 0;
    for m in history.iter_mut() {
        for r in m.tool_results.iter_mut() {
            if r.content.ends_with(CLAMP_MARKER) || r.content.chars().count() <= max_chars {
                continue;
            }
            let head: String = r.content.chars().take(head_chars).collect();
            r.content = format!("{head}{CLAMP_MARKER}");
            clamped += 1;
        }
    }
    clamped
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

    /// ISS-409753: the estimator must measure BYTES, not chars. It is re-run over
    /// the whole history before every model call, so O(1) `len()` is the point —
    /// and byte length must never UNDER-count, or the overflow guard under-fires.
    #[test]
    fn estimate_text_tokens_is_byte_based_and_never_undercounts() {
        assert_eq!(estimate_text_tokens(""), 0);
        assert_eq!(estimate_text_tokens("abcd"), 1);
        // div_ceil: a partial token still costs a token.
        assert_eq!(estimate_text_tokens("abcde"), 2);

        // Multibyte must count its full UTF-8 width. "日本語" is 3 chars / 9 bytes:
        // the old chars-based estimate said 1 token, bytes says 3. Real BPE emits
        // ~3 here, so byte length is both cheaper AND the closer proxy.
        let cjk = "日本語";
        assert_eq!(cjk.chars().count(), 3);
        assert_eq!(cjk.len(), 9);
        assert_eq!(estimate_text_tokens(cjk), 3);
        assert!(estimate_text_tokens(cjk) >= cjk.chars().count().div_ceil(4));
    }

    // ISS-409753: json_bytes must never UNDER-count a compact `to_string()`,
    // otherwise the pre-flight overflow guard would let a prompt through.
    #[test]
    fn json_bytes_never_undercounts_serialized_form() {
        let cases = [
            serde_json::json!(null),
            serde_json::json!(true),
            serde_json::json!(false),
            serde_json::json!(42),
            serde_json::json!("hello"),
            serde_json::json!({"path": "src/main.rs", "content": "fn main() {}"}),
            serde_json::json!({"a": [1, 2, 3], "b": {"c": "d"}}),
            serde_json::json!([]),
            serde_json::json!({}),
        ];
        for v in &cases {
            let actual = v.to_string().len();
            assert!(
                json_bytes(v) >= actual,
                "json_bytes under-counted {v}: {} < {actual}",
                json_bytes(v)
            );
        }
    }

    // A big string arg must cost ~its own length, not be skipped — the estimate
    // still has to see `write_file`-scale payloads. (ISS-409753)
    #[test]
    fn json_bytes_accounts_for_large_string_payloads() {
        let big = "x".repeat(100_000);
        let v = serde_json::json!({"content": big});
        assert!(json_bytes(&v) >= 100_000);
    }

    // ISS-409753: the anchored estimate must add ONLY the tail after `mark`.
    #[test]
    fn anchored_estimate_sizes_only_the_appended_tail() {
        let history = vec![
            Msg::user("a".repeat(4_000)),
            Msg::user("b".repeat(400)),
            Msg::user("c".repeat(400)),
        ];
        let tail = estimate_history_tokens(&history[1..]);
        assert_eq!(
            estimate_prompt_tokens_anchored(10_000, 1, "sys", 500, &history),
            10_000 + tail,
        );
        // Nothing appended since the anchor ⇒ the base is the whole answer.
        assert_eq!(
            estimate_prompt_tokens_anchored(10_000, history.len(), "sys", 500, &history),
            10_000,
        );
    }

    // ISS-409753: an unusable anchor must degrade to the full scan, never to a
    // silent under-count.
    #[test]
    fn anchored_estimate_falls_back_without_a_usable_anchor() {
        let history = vec![Msg::user("a".repeat(4_000)), Msg::user("b".repeat(400))];
        let full = estimate_prompt_tokens("sys", 500, &history);
        // No usage reported yet.
        assert_eq!(
            estimate_prompt_tokens_anchored(0, 0, "sys", 500, &history),
            full
        );
        // Stale mark pointing past the (since-compacted) history.
        assert_eq!(
            estimate_prompt_tokens_anchored(10_000, 99, "sys", 500, &history),
            full
        );
    }

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
    fn prompt_ceiling_sits_below_the_window() {
        // The reported failure: 225_423 tokens against a 200k window.
        let c = prompt_ceiling(200_000);
        assert!(c < 200_000, "ceiling must leave headroom: {c}");
        assert!(
            c > 150_000,
            "ceiling must not compact more than needed: {c}"
        );
        assert!(225_423 >= c, "the reported overflow must trip the guard");
        // Unknown window disables the lever rather than dividing by zero.
        assert_eq!(prompt_ceiling(0), 0);
    }

    #[test]
    fn prompt_estimate_counts_system_tools_and_history() {
        let history = vec![tool_results(&"x".repeat(400))];
        let est = estimate_prompt_tokens(&"s".repeat(40), 1_000, &history);
        // 40 chars system (~10) + 1000 tool tokens + 400 chars result (~100).
        assert_eq!(est, 10 + 1_000 + 100);
        // Every component actually moves the number.
        assert!(estimate_prompt_tokens("", 0, &history) < est);
    }

    #[test]
    fn overflow_errors_are_recognized_across_providers() {
        assert!(is_context_overflow_error(
            "claude api invalid_request_error (400): prompt is too long: \
225423 tokens > 200000 maximum"
        ));
        assert!(is_context_overflow_error(
            "This model's maximum context length is 128000 tokens"
        ));
        assert!(is_context_overflow_error("context_length_exceeded"));
        // Unrelated failures must NOT trigger a history-shedding retry.
        assert!(!is_context_overflow_error(
            "claude api authentication failed (401): invalid x-api-key"
        ));
        assert!(!is_context_overflow_error(
            "network error (connection reset)"
        ));
    }

    #[test]
    fn clamping_bounds_oversized_results_and_is_idempotent() {
        let mut history = vec![tool_results(&"y".repeat(100_000)), tool_results("small")];
        let n = clamp_oversized_results(&mut history, 1_000);
        assert_eq!(n, 1, "only the oversized body is clamped");
        let len = history[0].tool_results[0].content.chars().count();
        assert_eq!(len, 4_000, "clamped body fits the cap INCLUDING the marker");
        assert!(history[0].tool_results[0].content.ends_with(CLAMP_MARKER));
        assert_eq!(history[1].tool_results[0].content, "small");
        // Idempotent: a second pass finds nothing to do, so the engine's
        // shrink loop terminates instead of re-clamping forever.
        assert_eq!(clamp_oversized_results(&mut history, 1_000), 0);
    }

    #[test]
    fn emergency_split_rescues_a_history_too_short_for_the_normal_keep() {
        // 4 messages: the normal lever (keep 12) finds no split and no-ops —
        // which is exactly how a short-but-huge history 400s forever.
        let history = vec![
            Msg::user("go"),
            assistant_call("working", "read_file"),
            tool_results(&"z".repeat(900_000)),
            assistant_call("more", "read_file"),
        ];
        assert!(plan_compaction(&history, KEEP_RECENT_MSGS).is_none());
        // The emergency tail size finds the assistant boundary and sheds the head.
        let plan = plan_compaction(&history, MIN_KEEP_RECENT_MSGS)
            .expect("emergency split must find an assistant boundary");
        assert!(plan.dropped > 0);
        let mut h = history.clone();
        apply_compaction(&mut h, &plan);
        // The oversized body sits INSIDE the retained tail, so splitting alone
        // cannot rescue this shape — the clamp backstop is what actually frees
        // the window. Together they must get the history back under the window.
        assert!(estimate_history_tokens(&h) > prompt_ceiling(200_000));
        assert_eq!(clamp_oversized_results(&mut h, CLAMP_RESULT_TOKENS), 1);
        assert!(estimate_history_tokens(&h) < prompt_ceiling(200_000));
    }

    #[test]
    fn window_sizes_by_model_family() {
        assert_eq!(context_window("claude-haiku-4-5"), 200_000);
        assert_eq!(context_window("claude-opus-4-9"), 200_000);
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
    fn parse_ceiling_handles_overrides_and_typos() {
        // Absent/empty → default.
        assert_eq!(parse_ceiling(None, 50), 50);
        assert_eq!(parse_ceiling(Some(""), 50), 50);
        assert_eq!(parse_ceiling(Some("   "), 50), 50);
        // Explicit disables.
        assert_eq!(parse_ceiling(Some("0"), 50), 0);
        assert_eq!(parse_ceiling(Some("off"), 50), 0);
        assert_eq!(parse_ceiling(Some("None"), 50), 0);
        // Valid integer (with surrounding whitespace).
        assert_eq!(parse_ceiling(Some("30"), 50), 30);
        assert_eq!(parse_ceiling(Some(" 128 "), 50), 128);
        // Garbage must NOT silently disable the guard — fall back to default.
        assert_eq!(parse_ceiling(Some("banana"), 50), 50);
    }

    #[test]
    fn budget_trigger_prefers_tool_call_cap_then_token_then_window() {
        let b = CompactBudget {
            window: 200_000,
            threshold_pct: 75,
            tool_call_ceiling: 50,
            token_ceiling: 120_000,
            msg_ceiling: 0,
        };
        // Nothing tripped.
        assert_eq!(b.trigger(10_000, 3, 0), None);
        // Tool-call cap fires first, before either token lever.
        assert_eq!(b.trigger(10_000, 50, 0), Some(CompactTrigger::ToolCalls));
        // Below the cap but over the absolute token ceiling.
        assert_eq!(b.trigger(120_000, 10, 0), Some(CompactTrigger::TokenBudget));
        // Below both absolute caps but at the % window (150k of 200k).
        let b2 = CompactBudget {
            token_ceiling: 0,
            ..b
        };
        assert_eq!(b2.trigger(150_000, 10, 0), Some(CompactTrigger::WindowPct));
        assert_eq!(b2.trigger(149_999, 10, 0), None);
    }

    #[test]
    fn budget_ceilings_are_individually_disablable() {
        // Tool-call lever off (0) — only the token/window levers remain.
        let b = CompactBudget {
            window: 0,
            threshold_pct: 75,
            tool_call_ceiling: 0,
            token_ceiling: 0,
            msg_ceiling: 0,
        };
        // Every lever off ⇒ never compacts, even with a huge transcript.
        assert_eq!(b.trigger(10_000_000, 10_000, 0), None);
        // Only the tool-call lever on.
        let b = CompactBudget {
            tool_call_ceiling: 40,
            ..b
        };
        assert_eq!(b.trigger(10_000_000, 39, 0), None);
        assert_eq!(b.trigger(0, 40, 0), Some(CompactTrigger::ToolCalls));
    }

    // ISS-409752: the message cap is the structural backstop. It must fire in
    // exactly the configuration where every CONTENT-based lever is blind: a huge
    // window nowhere near its percentage threshold, the absolute token lever off,
    // and the tool-call lever explicitly disabled by the operator.
    #[test]
    fn msg_ceiling_fires_when_every_token_lever_is_blind() {
        let b = CompactBudget {
            window: 1_000_000,
            threshold_pct: 75,
            tool_call_ceiling: 0, // AISH_COMPACT_TOOL_CALLS=off
            token_ceiling: 0,     // absolute token lever off (the default)
            msg_ceiling: 120,
        };
        // Thousands of tiny rounds: only 10k tokens live, so the % window (750k)
        // is nowhere near tripping — yet the transcript is 400 messages long.
        // Before ISS-409752 this returned None forever and history grew without
        // bound; the message cap is what now catches it.
        assert_eq!(b.trigger(10_000, 0, 400), Some(CompactTrigger::MsgCount));
        // At the ceiling exactly (inclusive, like the other levers).
        assert_eq!(b.trigger(10_000, 0, 120), Some(CompactTrigger::MsgCount));
        // One below ⇒ still nothing to do.
        assert_eq!(b.trigger(10_000, 0, 119), None);
    }

    #[test]
    fn msg_ceiling_is_disablable_and_yields_to_tool_call_cap() {
        let b = CompactBudget {
            window: 0,
            threshold_pct: 75,
            tool_call_ceiling: 0,
            token_ceiling: 0,
            msg_ceiling: 0, // lever off
        };
        // Every lever off ⇒ never compacts, however long the transcript.
        assert_eq!(b.trigger(10_000_000, 10_000, 100_000), None);
        // The tool-call cap still takes precedence over the message cap so the
        // logged reason stays the cheapest-and-earliest lever that tripped.
        let b = CompactBudget {
            tool_call_ceiling: 50,
            msg_ceiling: 10,
            ..b
        };
        assert_eq!(b.trigger(0, 50, 999), Some(CompactTrigger::ToolCalls));
        // Below the tool-call cap, the message cap reports itself.
        assert_eq!(b.trigger(0, 49, 999), Some(CompactTrigger::MsgCount));
    }

    #[test]
    fn msg_ceiling_env_override_parses_like_the_other_ceilings() {
        assert_eq!(
            parse_ceiling(None, COMPACT_MSG_CEILING),
            COMPACT_MSG_CEILING
        );
        assert_eq!(parse_ceiling(Some("off"), COMPACT_MSG_CEILING), 0);
        assert_eq!(parse_ceiling(Some("64"), COMPACT_MSG_CEILING), 64);
        // The default must sit well above the retained working set, or a normal
        // turn would compact on every single round.
        assert!(COMPACT_MSG_CEILING > KEEP_RECENT_MSGS * 2);
    }

    // ISS-409752: compaction must actually hand memory back, not just shift the
    // retained tail down inside an over-sized allocation.
    #[test]
    fn apply_compaction_reclaims_vector_capacity() {
        let mut h: Vec<Msg> = Vec::with_capacity(4096);
        for i in 0..200 {
            h.push(Msg::user(format!("u{i}")));
            h.push(assistant_call(&format!("a{i}"), "read_file"));
        }
        let peak_cap = h.capacity();
        assert!(peak_cap >= 400);
        let c = plan_compaction(&h, KEEP_RECENT_MSGS).expect("long history compacts");
        apply_compaction(&mut h, &c);
        // Contents shrank to the summary + retained tail (the split walks down
        // to the nearest Assistant boundary, so the tail can be one longer than
        // `keep_recent`).
        assert!(h.len() <= KEEP_RECENT_MSGS + 2, "len {}", h.len());
        // … and the spine no longer holds the run's high-water allocation.
        assert!(
            h.capacity() < peak_cap,
            "capacity {} should be below peak {peak_cap}",
            h.capacity()
        );
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
