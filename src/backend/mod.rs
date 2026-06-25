pub mod claude;
pub mod grok;
#[cfg(feature = "local")]
pub mod local;

use anyhow::Result;
use serde_json::Value;

/// Read a finished HTTP response into `(status, parsed-or-snippet)`.
///
/// The body is decoded as TEXT first and only THEN parsed as JSON. This is the
/// difference between a transient hiccup and a dead worker: the public API sits
/// behind proxies / load balancers / edge caches that answer an overloaded or
/// failing upstream with a NON-JSON body — a 502/503/504 HTML error page, an
/// empty body, a Cloudflare challenge, a plain-text "Internal Server Error".
/// Calling `.json()` straight off the response turns any of those into a fatal
/// `error decoding response body … expected value at line 1 column 1`, which
/// (propagated through `?`) stops the worker mid-run. By decoding to text first
/// we hand the caller a recoverable signal (`Err(snippet)`) it can fold into its
/// normal retry-the-transient-5xx path instead of aborting.
///
/// Returns `Err` only when the body bytes themselves can't be read (a genuine
/// transport error); a body that simply isn't JSON comes back as
/// `Ok((status, Err(snippet)))`.
pub async fn read_status_and_json(
    resp: reqwest::Response,
) -> reqwest::Result<(u16, std::result::Result<Value, String>)> {
    let status = resp.status().as_u16();
    let body = resp.text().await?;
    let parsed = serde_json::from_str::<Value>(&body).map_err(|_| body_snippet(&body));
    Ok((status, parsed))
}

/// First ~200 whitespace-collapsed chars of a response body, for embedding a
/// non-JSON body into an error/log line without dumping a whole HTML page.
pub fn body_snippet(body: &str) -> String {
    let collapsed = body.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.is_empty() {
        return "<empty body>".to_string();
    }
    if collapsed.chars().count() > 200 {
        format!("{}…", collapsed.chars().take(200).collect::<String>())
    } else {
        collapsed
    }
}

/// One tool invocation requested by the model.
#[derive(Debug, Clone)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub args: Value,
}

/// The result we feed back for one tool call.
#[derive(Debug, Clone)]
pub struct ToolResult {
    pub id: String,
    pub content: String,
    pub is_error: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Role {
    User,
    Assistant,
}

/// Backend-agnostic conversation entry. Each backend renders this to its wire
/// format. `raw` carries provider-specific assistant content (Claude needs its
/// thinking blocks echoed verbatim); rendering falls back to the normalized
/// fields when absent or when the backend changes mid-session.
#[derive(Debug, Clone)]
pub struct Msg {
    pub role: Role,
    pub text: String,
    pub tool_calls: Vec<ToolCall>,
    pub tool_results: Vec<ToolResult>,
    pub raw: Option<Value>,
}

impl Msg {
    pub fn user(text: impl Into<String>) -> Self {
        Self { role: Role::User, text: text.into(), tool_calls: vec![], tool_results: vec![], raw: None }
    }
    pub fn tool_results(results: Vec<ToolResult>) -> Self {
        Self { role: Role::User, text: String::new(), tool_calls: vec![], tool_results: results, raw: None }
    }
}

/// One assistant turn, normalized.
#[derive(Debug, Default)]
pub struct Turn {
    pub text: String,
    pub tool_calls: Vec<ToolCall>,
    pub raw: Option<Value>,
    /// Set when the response hit the output limit (`stop_reason:"max_tokens"`)
    /// WHILE emitting a tool call, so the partial tool call was dropped rather
    /// than executed (see `claude.rs`). The agentic loop must keep going — feed
    /// the model the corrective note in `text` instead of treating the empty
    /// `tool_calls` as a final answer — so it can retry with a smaller edit.
    pub truncated_tool_call: bool,
    /// Token usage the backend reported for this completion, when available.
    /// Drives context-window awareness + compaction (see `crate::context`).
    /// `None` when the backend doesn't report usage; the engine then falls
    /// back to a char-based estimate.
    pub usage: Option<crate::context::Usage>,
    /// Set when the response hit the output limit on PLAIN TEXT (no tool call) —
    /// the visible answer was cut off mid-stream. The agentic loop continues the
    /// answer via an assistant-PREFILL round (the partial text is left as the
    /// trailing assistant message and the model resumes it; chunks are merged)
    /// rather than returning a half-finished reply. Mutually exclusive with
    /// `truncated_tool_call` (a turn either dropped a partial tool call or had
    /// its prose cut — not both). See `engine::run_turn`.
    pub truncated_text: bool,
}

/// A tool definition in neutral form (JSON Schema input).
pub struct ToolDef {
    pub name: String,
    pub description: String,
    pub schema: Value,
}

/// Enum dispatch — two backends don't justify a trait object.
pub enum Backend {
    Claude(claude::ClaudeBackend),
    Grok(grok::GrokBackend),
    #[cfg(feature = "local")]
    Local(local::LocalBackend),
}

impl Backend {
    pub fn new_claude(model: String, cred: claude::Credential) -> Result<Self> {
        Ok(Backend::Claude(claude::ClaudeBackend::new(model, cred)?))
    }

    pub fn new_grok(model: String, extra_env: &[(String, String)]) -> Result<Self> {
        Ok(Backend::Grok(grok::GrokBackend::new(model, extra_env)?))
    }

    /// A stable short name for the active backend (`"claude"`/`"grok"`/`"local"`).
    /// Used to thread the active backend through to background coordinators so
    /// they run on the same provider the interactive session does.
    pub fn kind(&self) -> &'static str {
        match self {
            Backend::Claude(_) => "claude",
            Backend::Grok(_) => "grok",
            #[cfg(feature = "local")]
            Backend::Local(_) => "local",
        }
    }

    #[cfg(feature = "local")]
    pub fn new_local() -> Self {
        Backend::Local(local::LocalBackend::new())
    }

    /// The active model id (e.g. `claude-opus-4-8`, `grok-…`). Used to stamp
    /// the per-worker `meta.json` (S9.3) so a persisted worker session records
    /// which model produced it. `"local"` for the in-process backend.
    pub fn model(&self) -> String {
        match self {
            Backend::Claude(b) => b.model.clone(),
            Backend::Grok(b) => b.model.clone(),
            #[cfg(feature = "local")]
            Backend::Local(_) => "local".to_string(),
        }
    }


    /// Pre-turn hook: the local backend lazy-loads weights here — drawing a
    /// download progress line if needed — before the engine's "thinking"
    /// spinner starts overwriting stderr. No-op for Claude.
    pub async fn prepare(&self) -> Result<()> {
        match self {
            Backend::Claude(_) => Ok(()),
            Backend::Grok(_) => Ok(()),
            #[cfg(feature = "local")]
            Backend::Local(b) => b.prepare().await,
        }
    }

    pub async fn complete(&self, system: &str, history: &[Msg], tools: &[ToolDef]) -> Result<Turn> {
        match self {
            Backend::Claude(b) => b.complete(system, history, tools).await,
            Backend::Grok(b) => b.complete(system, history, tools).await,
            #[cfg(feature = "local")]
            Backend::Local(b) => b.complete(system, history, tools).await,
        }
    }

    /// Whether this backend can resume a truncated PLAIN-TEXT answer via an
    /// assistant-prefill continuation round. Claude's Messages API resumes a
    /// trailing assistant message verbatim, so the engine continues cut-off
    /// answers there. The OpenAI-shaped chat-completions backends (Grok, local)
    /// don't have well-defined assistant-prefill continuation, so they keep the
    /// in-band "[response truncated]" note instead of risking a re-answer loop.
    pub fn supports_prefill_continuation(&self) -> bool {
        matches!(self, Backend::Claude(_))
    }

    pub fn describe(&self) -> String {
        match self {
            Backend::Claude(b) => format!("claude ({})", b.model),
            Backend::Grok(b) => format!("grok ({} · {})", b.model, b.auth_label()),
            #[cfg(feature = "local")]
            Backend::Local(b) => format!("local ({} · in-process)", b.file),
        }
    }

    /// The model's approximate context window (tokens) — what the engine and
    /// `:context` measure usage against to decide when to compact history.
    pub fn context_window(&self) -> usize {
        match self {
            Backend::Claude(b) => crate::context::context_window(&b.model),
            Backend::Grok(b) => crate::context::context_window(&b.model),
            #[cfg(feature = "local")]
            Backend::Local(_) => crate::context::context_window("local"),
        }
    }

    /// MCP tool schemas can dwarf a small local context window (one server
    /// with 144 tools is ~50k tokens of JSON Schema — bigger than the local
    /// model's entire context before the conversation even starts), so only
    /// the Claude backend gets them. Local runs with the built-in shell tools.
    pub fn include_mcp_tools(&self) -> bool {
        match self {
            Backend::Claude(_) => true,
            // Grok is a capable, large-context model — give it the full MCP tool set.
            Backend::Grok(_) => true,
            #[cfg(feature = "local")]
            Backend::Local(_) => false,
        }
    }

    pub fn set_model(&mut self, model: String) {
        match self {
            Backend::Claude(b) => b.model = model,
            Backend::Grok(b) => b.model = model,
            #[cfg(feature = "local")]
            Backend::Local(_) => {
                eprintln!("(:model on the local backend isn't wired — set AISH_LOCAL_MODEL_ID and restart, or use :backend claude)")
            }
        }
    }

    /// The stronger model this frontend should escalate hard, in-turn reasoning
    /// to — or `None` when the frontend is already at/above it (escalation would
    /// just be the model consulting itself). Returns `(provider, model)`:
    /// - a small Claude model (haiku/sonnet) → the batch model (Opus by default);
    /// - a non-default Grok model → Grok's strongest;
    /// - the local model → Claude's strong model, but only when a Claude
    ///   credential is available (an offline local run has nothing to escalate to);
    /// - an Opus / default-Grok frontend → `None`.
    ///
    /// The engine recomputes this each turn and stashes it on the session so the
    /// `escalate` tool can reconstruct the strong-model backend at call time.
    pub fn escalation_target(
        &self,
        batch_model: &str,
        env: &[(String, String)],
    ) -> Option<(&'static str, String)> {
        match self {
            Backend::Claude(b) => resolve_escalation("claude", &b.model, batch_model, false),
            Backend::Grok(b) => resolve_escalation("grok", &b.model, batch_model, false),
            #[cfg(feature = "local")]
            Backend::Local(_) => resolve_escalation(
                "local",
                "",
                batch_model,
                claude::Credential::resolve(env).is_ok(),
            ),
        }
    }
}

/// Pure escalation policy, split out so it's unit-testable without constructing
/// a live backend (which needs credentials). `claude_cred_ok` only matters for
/// the local frontend, which must reach a cloud model to escalate at all.
fn resolve_escalation(
    kind: &str,
    model: &str,
    batch_model: &str,
    claude_cred_ok: bool,
) -> Option<(&'static str, String)> {
    match kind {
        // Opus is already the strongest Claude model — nothing to escalate to.
        "claude" if model.contains("opus") => None,
        "claude" => Some(("claude", batch_model.to_string())),
        // Grok ships a single model here; if we're already on it, no escalation.
        "grok" if model == grok::DEFAULT_MODEL => None,
        "grok" => Some(("grok", grok::DEFAULT_MODEL.to_string())),
        // The local model is the weakest frontend; escalate to Claude's strong
        // model when (and only when) a Claude credential is reachable.
        "local" if claude_cred_ok => Some(("claude", batch_model.to_string())),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn body_snippet_handles_empty_and_long_bodies() {
        assert_eq!(body_snippet(""), "<empty body>");
        assert_eq!(body_snippet("   \n\t "), "<empty body>");
        // whitespace is collapsed
        assert_eq!(body_snippet("502  Bad\n\nGateway"), "502 Bad Gateway");
        // long bodies are clipped with an ellipsis
        let long = "x".repeat(500);
        let s = body_snippet(&long);
        assert!(s.ends_with('…'));
        assert_eq!(s.chars().count(), 201); // 200 chars + ellipsis
    }

    #[test]
    fn escalation_policy() {

        let opus = "claude-opus-4-8";
        // Small Claude frontends escalate to the batch (strong) model.
        assert_eq!(
            resolve_escalation("claude", "claude-haiku-4-5", opus, false),
            Some(("claude", opus.to_string()))
        );
        assert_eq!(
            resolve_escalation("claude", "claude-sonnet-4-6", opus, false),
            Some(("claude", opus.to_string()))
        );
        // An Opus frontend is already frontier — no self-escalation.
        assert_eq!(resolve_escalation("claude", opus, opus, false), None);
        // Grok on its only model: nothing stronger to reach.
        assert_eq!(resolve_escalation("grok", grok::DEFAULT_MODEL, opus, false), None);
        // Local escalates to Claude's strong model, but only with a credential.
        assert_eq!(
            resolve_escalation("local", "", opus, true),
            Some(("claude", opus.to_string()))
        );
        assert_eq!(resolve_escalation("local", "", opus, false), None);
    }
}
