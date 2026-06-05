pub mod claude;
#[cfg(feature = "local")]
pub mod local;

use anyhow::Result;
use serde_json::Value;

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
#[derive(Debug)]
pub struct Turn {
    pub text: String,
    pub tool_calls: Vec<ToolCall>,
    pub raw: Option<Value>,
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
    #[cfg(feature = "local")]
    Local(local::LocalBackend),
}

impl Backend {
    pub fn new_claude(model: String) -> Result<Self> {
        Ok(Backend::Claude(claude::ClaudeBackend::new(model)?))
    }

    #[cfg(feature = "local")]
    pub fn new_local() -> Self {
        Backend::Local(local::LocalBackend::new())
    }

    pub async fn complete(&self, system: &str, history: &[Msg], tools: &[ToolDef]) -> Result<Turn> {
        match self {
            Backend::Claude(b) => b.complete(system, history, tools).await,
            #[cfg(feature = "local")]
            Backend::Local(b) => b.complete(system, history, tools).await,
        }
    }

    pub fn describe(&self) -> String {
        match self {
            Backend::Claude(b) => format!("claude ({})", b.model),
            #[cfg(feature = "local")]
            Backend::Local(b) => format!("local ({} · in-process)", b.file),
        }
    }

    /// MCP tool schemas can dwarf a small local context window (one server
    /// with 144 tools is ~50k tokens of JSON Schema — past Qwen3-8B's 40960
    /// limit before the conversation even starts), so only the Claude
    /// backend gets them. Local runs with the built-in shell tools.
    pub fn include_mcp_tools(&self) -> bool {
        match self {
            Backend::Claude(_) => true,
            #[cfg(feature = "local")]
            Backend::Local(_) => false,
        }
    }

    pub fn set_model(&mut self, model: String) {
        match self {
            Backend::Claude(b) => b.model = model,
            #[cfg(feature = "local")]
            Backend::Local(_) => {
                eprintln!("(:model on the local backend isn't wired yet — use :backend claude first)")
            }
        }
    }
}
