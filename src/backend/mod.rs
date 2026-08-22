pub mod claude;
pub mod grok;
pub mod openai;
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

/// A reference to a plugin-declared JSON Schema that a tool result's structured
/// payload is expected to conform to (Phase 3.4 runtime enforcement). `plugin_id`
/// selects the discovered plugin; `schema_name` is the file stem under
/// `<plugin>/schemas/<name>.json`. A structured-emitting tool that opts into
/// validation attaches one via [`ToolResult::with_output_schema`]; the engine's
/// post-execution hook ([`crate::engine`]) then validates the payload against it.
/// `None` — the common case — means "no schema declared", and the hook is a
/// zero-cost no-op (it never touches the plugin loader).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputSchemaRef {
    pub plugin_id: String,
    pub schema_name: String,
}

/// The result we feed back for one tool call.
#[derive(Debug, Clone)]
pub struct ToolResult {
    pub id: String,
    pub content: String,
    pub is_error: bool,
    /// Optional plugin-schema declaration for this result's structured payload
    /// (Phase 3.4). `Some` opts the result into runtime validation by the
    /// engine's post-execution hook; `None` (the common case) means the hook
    /// returns immediately with zero overhead. See [`OutputSchemaRef`].
    pub output_schema: Option<OutputSchemaRef>,
    /// Populated by the engine's validation hook when the declared
    /// `output_schema` REJECTED the structured payload (Phase 3.4). Fail-open:
    /// the payload still flows to the model, but [`ToolResult::model_content`]
    /// prepends a `[schema-validation warning]` note so the model is told the
    /// data is off-spec. `None` when no schema was declared or validation
    /// passed.
    pub schema_violation: Option<String>,
    /// Optional typed payload for tools whose output is already structured
    /// (records / tables). `content` stays the rendered, human-readable source
    /// of truth; this is additive JSON the model can consume without
    /// re-parsing the text. `None` for free-form / text-only tools.
    ///
    /// SCOPE GUARDRAIL (S7.4): this payload is a passive, per-call, OPAQUE
    /// attachment the model reads — NOT a value in a programmable pipeline.
    /// aish may *describe* a result in a typed way; it must never *operate* on
    /// these types (no piping/composition, no query language, no persistent
    /// typed store, no schema registry). See docs/S7.4-tests-docs-scope.md §3.
    pub structured: Option<Value>,
}

impl ToolResult {
    /// A text-only result — `structured` is `None`. This is the path every tool
    /// took before S7.2, and the path every text-only tool still takes.
    pub fn text(id: impl Into<String>, content: impl Into<String>, is_error: bool) -> Self {
        Self {
            id: id.into(),
            content: content.into(),
            is_error,
            structured: None,
            output_schema: None,
            schema_violation: None,
        }
    }

    /// A result carrying a typed payload alongside the rendered text (S7.2).
    /// `content` remains the human-readable source of truth; `value` is the
    /// same data as trustworthy JSON for the model.
    pub fn structured(
        id: impl Into<String>,
        content: impl Into<String>,
        value: Value,
        is_error: bool,
    ) -> Self {
        Self {
            id: id.into(),
            content: content.into(),
            is_error,
            structured: Some(value),
            output_schema: None,
            schema_violation: None,
        }
    }

    /// Declare that this result's structured payload should conform to the named
    /// schema shipped by `plugin_id` (Phase 3.4). Builder form — attach it to a
    /// structured result to opt into runtime validation by the engine's
    /// post-execution hook. A no-op in effect until that hook runs; text-only
    /// results (no `structured` payload) are still skipped by the hook.
    #[allow(dead_code)] // producer-side seam — attached by schema-aware tools; exercised by tests.
    pub fn with_output_schema(
        mut self,
        plugin_id: impl Into<String>,
        schema_name: impl Into<String>,
    ) -> Self {
        self.output_schema = Some(OutputSchemaRef {
            plugin_id: plugin_id.into(),
            schema_name: schema_name.into(),
        });
        self
    }

    /// Record that the declared `output_schema` rejected this result's payload
    /// (Phase 3.4). Called by the engine's validation hook; fail-open — the
    /// payload is untouched, only the model-facing note is set.
    pub fn note_schema_violation(&mut self, note: impl Into<String>) {
        self.schema_violation = Some(note.into());
    }

    /// The representation fed back to the MODEL for this result (S7.3).
    ///
    /// When a typed payload is present (the record/table tools from S7.2), the
    /// model receives it as compact JSON — trustworthy structure it can parse
    /// directly instead of re-deriving it from alignment/ellipsis-corrupted
    /// ASCII. Text-only results (`structured == None`) thread their `content`
    /// verbatim, exactly as before S7.3. This is the **model** half of the
    /// S7.3 split; the Ctrl-O raw view stays on `content` (see
    /// `engine::raw_body`), so the human always sees the verbatim tool output.
    ///
    /// OQ3 cap (S7.3): a hard ceiling on what one tool result may feed the
    /// model. Compact JSON for a large `grep_files`/`glob_expand` payload can be
    /// far heavier than the rendered text — a 500-record grep over generated
    /// files once serialized to 218k tokens and overflowed Claude's 200k window.
    /// When the JSON payload exceeds the budget we fall back to the rendered
    /// `content` (itself capped at `MAX_OUTPUT` by the emitting tool, and
    /// re-capped here for any caller that isn't), so an oversized structured
    /// result degrades to representative text instead of crashing the turn.
    pub fn model_content(&self) -> std::borrow::Cow<'_, str> {
        let body = self.model_body();
        // Phase 3.4: a schema violation prepends a one-line warning so the model
        // knows the payload is off-spec, while the payload itself still flows
        // through unchanged (fail-open). Zero overhead when no violation is set.
        match &self.schema_violation {
            Some(note) => std::borrow::Cow::Owned(format!(
                "[schema-validation warning] {note}\n\n{body}"
            )),
            None => body,
        }
    }

    /// The payload half of [`model_content`] — the structured JSON (capped) or
    /// the verbatim text — WITHOUT the schema-violation banner. Split out so the
    /// banner can be prepended without duplicating the cap logic.
    fn model_body(&self) -> std::borrow::Cow<'_, str> {
        // ~25k tokens. Comfortably below the model's context window while still
        // large enough for a legitimate structured result.
        const MAX_MODEL_RESULT: usize = 100_000;
        match &self.structured {
            // Compact (not pretty) JSON keeps the wire payload tight. Fall back
            // to the rendered text on the (practically impossible) serialize
            // error so the model never receives an empty tool result.
            Some(v) => match serde_json::to_string(v) {
                Ok(json) if json.len() <= MAX_MODEL_RESULT => std::borrow::Cow::Owned(json),
                // Oversized payload: ship the human-rendered text instead, byte-
                // capped so it can't overflow either. Truncating the JSON itself
                // would yield invalid JSON the model couldn't parse, so we hand
                // back valid representative text (head+tail) rather than a broken
                // array.
                Ok(_) => std::borrow::Cow::Owned(crate::tools::truncate_middle(
                    self.content.clone(),
                    MAX_MODEL_RESULT,
                )),
                Err(_) => std::borrow::Cow::Borrowed(self.content.as_str()),
            },
            None => std::borrow::Cow::Borrowed(self.content.as_str()),
        }
    }
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
        Self {
            role: Role::User,
            text: text.into(),
            tool_calls: vec![],
            tool_results: vec![],
            raw: None,
        }
    }
    pub fn tool_results(results: Vec<ToolResult>) -> Self {
        Self {
            role: Role::User,
            text: String::new(),
            tool_calls: vec![],
            tool_results: results,
            raw: None,
        }
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

/// One incremental piece of a streamed completion, handed to a [`StreamSink`]
/// the instant it is decoded off the wire (S8.1). `Text` is visible answer
/// prose; `Thinking` is the model's extended-thinking trace, which a caller may
/// render dimmed or ignore entirely. Deltas are NOT line-buffered — a sink may
/// receive a single character or a whole paragraph in one call — so the
/// acceptance criterion "tokens arrive incrementally" is satisfied at whatever
/// granularity the provider emits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamDelta<'a> {
    Text(&'a str),
    Thinking(&'a str),
}

/// A synchronous callback the backend invokes for every [`StreamDelta`] as a
/// streamed completion is decoded. It runs on the task driving the stream,
/// BETWEEN `await`s (never held across one), so it must not block — the typical
/// sink writes the delta straight to stderr; a test sink pushes onto a `Vec`.
pub type StreamSink<'a> = &'a mut dyn FnMut(StreamDelta<'_>);

/// Enum dispatch — three backends.
pub enum Backend {
    Claude(claude::ClaudeBackend),
    Grok(grok::GrokBackend),
    OpenAi(openai::OpenAiBackend),
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

    pub fn new_openai(
        provider: openai::Provider,
        model: String,
        extra_env: &[(String, String)],
    ) -> Result<Self> {
        Ok(Backend::OpenAi(openai::OpenAiBackend::new(
            provider, model, extra_env,
        )?))
    }

    #[cfg(feature = "local")]
    pub fn new_local() -> Result<Self> {
        Ok(Backend::Local(local::LocalBackend::new()?))
    }

    /// A stable short name for the active backend (`"claude"`/`"grok"`/`"local"`).
    /// Used to thread the active backend through to background coordinators so
    /// they run on the same provider the interactive session does.
    pub fn kind(&self) -> &'static str {
        match self {
            Backend::Claude(_) => "claude",
            Backend::Grok(_) => "grok",
            Backend::OpenAi(b) => b.kind(),
            #[cfg(feature = "local")]
            Backend::Local(_) => "local",
        }
    }

    /// The active model id (e.g. `claude-opus-4-8`, `grok-…`). Used to stamp
    /// the per-worker `meta.json` (S9.3) so a persisted worker session records
    /// which model produced it.
    pub fn model(&self) -> String {
        match self {
            Backend::Claude(b) => b.model.clone(),
            Backend::Grok(b) => b.model.clone(),
            Backend::OpenAi(b) => b.model.clone(),
            #[cfg(feature = "local")]
            Backend::Local(_) => crate::hwdetect::selected_model_id(),
        }
    }

    /// Pre-turn hook: No-op for cloud backends. For local, lazy-loads the model.
    pub async fn prepare(&self) -> Result<()> {
        match self {
            Backend::Claude(_) => Ok(()),
            Backend::Grok(_) => Ok(()),
            Backend::OpenAi(_) => Ok(()),
            #[cfg(feature = "local")]
            Backend::Local(b) => b.prepare().await,
        }
    }

    pub async fn complete(&self, system: &str, history: &[Msg], tools: &[ToolDef]) -> Result<Turn> {
        match self {
            Backend::Claude(b) => b.complete(system, history, tools).await,
            Backend::Grok(b) => b.complete(system, history, tools).await,
            Backend::OpenAi(b) => b.complete(system, history, tools).await,
            #[cfg(feature = "local")]
            Backend::Local(b) => b.complete(system, history, tools).await,
        }
    }

    /// Stream a completion, delivering output to `sink` incrementally as tokens
    /// arrive, and returning the same normalized [`Turn`] as [`Backend::complete`]
    /// once the response finishes (S8.1). Claude decodes the Anthropic SSE token
    /// stream and emits each text/thinking delta the moment it lands — this is
    /// the path the acceptance criterion ("tokens arrive incrementally through
    /// the backend trait") is about. The other backends expose no native token
    /// stream here yet, so they degrade to a single deferred emission: run
    /// `complete`, then hand the whole answer to the sink once. Callers get a
    /// uniform streaming API regardless of provider, and the returned `Turn` is
    /// byte-identical to what `complete` would have produced.
    pub async fn complete_streaming(
        &self,
        system: &str,
        history: &[Msg],
        tools: &[ToolDef],
        sink: StreamSink<'_>,
    ) -> Result<Turn> {
        match self {
            Backend::Claude(b) => b.complete_streaming(system, history, tools, sink).await,
            Backend::Grok(b) => deferred_stream(b.complete(system, history, tools).await?, sink),
            Backend::OpenAi(b) => {
                deferred_stream(b.complete(system, history, tools).await?, sink)
            }
            #[cfg(feature = "local")]
            Backend::Local(b) => {
                deferred_stream(b.complete(system, history, tools).await?, sink)
            }
        }
    }

    /// Whether this backend can resume a truncated PLAIN-TEXT answer via an
    /// assistant-prefill continuation round. Claude's Messages API resumes a
    /// trailing assistant message verbatim, so the engine continues cut-off
    /// answers there.
    pub fn supports_prefill_continuation(&self) -> bool {
        matches!(self, Backend::Claude(_))
    }

    pub fn describe(&self) -> String {
        match self {
            Backend::Claude(b) => format!("claude ({} · {})", b.auth_label(), b.model),
            Backend::Grok(b) => format!("grok ({} · {})", b.auth_label(), b.model),
            Backend::OpenAi(b) => format!("{} ({} · {})", b.kind(), b.auth_label(), b.model),
            #[cfg(feature = "local")]
            Backend::Local(_) => format!("local ({})", crate::hwdetect::selected_model_id()),
        }
    }

    /// The model's approximate context window (tokens) — what the engine and
    /// `:context` measure usage against to decide when to compact history.
    pub fn context_window(&self) -> usize {
        match self {
            Backend::Claude(b) => crate::context::context_window(&b.model),
            Backend::Grok(b) => crate::context::context_window(&b.model),
            Backend::OpenAi(b) => crate::context::context_window(&b.model),
            #[cfg(feature = "local")]
            Backend::Local(_) => 4096, // conservative context floor for local GGUF models
        }
    }

    /// Both cloud backends support MCP tool schemas.
    pub fn include_mcp_tools(&self) -> bool {
        true
    }

    pub fn set_model(&mut self, model: String) {
        match self {
            Backend::Claude(b) => b.model = model,
            Backend::Grok(b) => b.model = model,
            Backend::OpenAi(b) => b.model = model,
            #[cfg(feature = "local")]
            Backend::Local(_) => {} // Model is baked in for local backend
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
        _env: &[(String, String)],
    ) -> Option<(&'static str, String)> {
        match self {
            Backend::Claude(b) => resolve_escalation("claude", &b.model, batch_model),
            Backend::Grok(b) => resolve_escalation("grok", &b.model, batch_model),
            Backend::OpenAi(b) => resolve_escalation(b.kind(), &b.model, batch_model),
            #[cfg(feature = "local")]
            Backend::Local(_) => {
                // Local can escalate to Claude if credentials are available
                Some(("claude", batch_model.to_string()))
            }
        }
    }
}

/// Fallback streaming for backends without a native token stream: the whole
/// answer already exists on `turn`, so emit it to the sink in one shot (only
/// when non-empty) and return the turn unchanged. Keeps `complete_streaming`
/// uniform across providers without pretending to stream token-by-token.
fn deferred_stream(turn: Turn, sink: StreamSink<'_>) -> Result<Turn> {
    if !turn.text.is_empty() {
        sink(StreamDelta::Text(&turn.text));
    }
    Ok(turn)
}

/// Pure escalation policy, split out so it's unit-testable without constructing
/// a live backend (which needs credentials).
fn resolve_escalation(kind: &str, model: &str, batch_model: &str) -> Option<(&'static str, String)> {
    match kind {
        // Opus is already the strongest Claude model — nothing to escalate to.
        "claude" if model.contains("opus") => None,
        "claude" => Some(("claude", batch_model.to_string())),
        // Grok ships a single model here; if we're already on it, no escalation.
        "grok" if model == grok::DEFAULT_MODEL => None,
        "grok" => Some(("grok", grok::DEFAULT_MODEL.to_string())),
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
    fn model_content_threads_compact_json_for_structured_results() {
        // S7.3 / AC1: a structured result feeds the model the COMPACT JSON
        // payload (no spaces), not the rendered text.
        let r = ToolResult::structured(
            "t1",
            "f.txt  3\nsub/",
            serde_json::json!([{"name": "f.txt", "type": "file", "size": 3}]),
            false,
        );
        // JSON is compact (no spaces), and contains all the expected fields.
        let content = r.model_content();
        assert!(content.contains("[{\"name\":\"f.txt\""));
        assert!(content.contains("\"type\":\"file\""));
        assert!(content.contains("\"size\":3"));
        assert!(content.contains("}]"));
        // S7.3 / AC3: a text-only result threads `content` verbatim (the
        // pre-S7.3 behaviour) and borrows it (no JSON allocation).
        let t = ToolResult::text("t2", "plain output", false);
        assert_eq!(t.model_content(), "plain output");
        assert!(matches!(t.model_content(), std::borrow::Cow::Borrowed(_)));
    }

    #[test]
    fn model_content_caps_oversized_structured_payload() {
        // OQ3 cap: a structured payload whose compact JSON exceeds the budget
        // must NOT be fed to the model — it falls back to the rendered text,
        // byte-capped, so an enormous grep payload can't overflow the context
        // window (the 218k-token context-overflow crash). This is the durable
        // guardrail behind the grep skip-dirs + per-line truncation fixes.
        let huge: Vec<_> = (0..50_000)
            .map(|i| serde_json::json!({"path": "target/x.rs", "line": i, "text": "x".repeat(40)}))
            .collect();
        let rendered = "representative rendered text".to_string();
        let r = ToolResult::structured("t", rendered.clone(), serde_json::json!(huge), false);
        let out = r.model_content();
        // Did NOT ship the giant JSON array...
        assert!(!out.starts_with("[{"), "oversized JSON must not be sent");
        // ...shipped the (capped) rendered text instead.
        assert!(out.contains("representative rendered text"), "fell back to text");
        assert!(out.len() <= 100_000, "capped under budget: {}", out.len());
    }

    #[test]
    fn structured_payload_is_additive_never_substitutes_content() {
        // S7.4 / AC1+AC2: the typed payload is ADDITIVE. Attaching it must never
        // mutate or replace `content` or `is_error` — a structured result and a
        // text-only result built from the SAME content+flag are byte-identical
        // in every human-facing field and differ ONLY in the model-facing
        // representation (compact JSON vs verbatim text). This is the invariant
        // the whole S7 structured-results capability rests on; see
        // docs/S7.4-tests-docs-scope.md §3.
        let content = "name  type  size\nf.txt file  3";
        let payload = serde_json::json!([{"name": "f.txt", "type": "file", "size": 3}]);

        let text = ToolResult::text("id", content, false);
        let structured = ToolResult::structured("id", content, payload.clone(), false);

        // String-only path: no payload, content fed to the model verbatim (and
        // BORROWED — no JSON allocation), is_error preserved.
        assert!(text.structured.is_none(), "text-only carries no payload");
        assert_eq!(text.model_content(), content);
        assert!(matches!(text.model_content(), std::borrow::Cow::Borrowed(_)));

        // Structured path: payload present, but content + is_error are UNCHANGED
        // relative to the text-only result — the payload is purely additive.
        assert_eq!(
            structured.content, text.content,
            "payload must not substitute content"
        );
        assert_eq!(
            structured.is_error, text.is_error,
            "payload must not touch is_error"
        );
        assert_eq!(structured.structured.as_ref(), Some(&payload));

        // The ONLY observable difference is the model-facing view: compact JSON
        // for the structured result, verbatim content for the text-only one.
        assert_ne!(structured.model_content(), text.model_content());
        // Verify compact JSON contains all the expected fields (order may vary).
        let json_content = structured.model_content();
        assert!(json_content.contains("[{\"name\":\"f.txt\""));
        assert!(json_content.contains("\"size\":3"));
        assert!(json_content.contains("\"type\":\"file\""));
        assert!(json_content.contains("}]"));

        // is_error is honoured independently of the payload on BOTH paths.
        assert!(ToolResult::text("e", "boom", true).is_error);
        assert!(ToolResult::structured("e", "boom", serde_json::json!({}), true).is_error);
    }

    #[test]
    fn escalation_policy() {
        let opus = "claude-opus-4-9";
        // Small Claude frontends escalate to the batch (strong) model.
        assert_eq!(
            resolve_escalation("claude", "claude-haiku-4-5", opus),
            Some(("claude", opus.to_string()))
        );
        assert_eq!(
            resolve_escalation("claude", "claude-sonnet-4-6", opus),
            Some(("claude", opus.to_string()))
        );
        // An Opus frontend is already frontier — no self-escalation.
        assert_eq!(resolve_escalation("claude", opus, opus), None);
        // Grok on its only model: nothing stronger to reach.
        assert_eq!(resolve_escalation("grok", grok::DEFAULT_MODEL, opus), None);
    }
}
