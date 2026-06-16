use super::{Msg, Role, ToolCall, ToolDef, Turn};
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::time::Duration;

const API_URL: &str = "https://api.x.ai/v1/chat/completions";

/// Default Grok model — a fast coding model with a 256k context and tool/function
/// calling. Used both as the interactive default and as the coordinator model
/// when the active backend is Grok (xAI has no Batches API, so every background
/// coordinator runs on this directly).
pub const DEFAULT_MODEL: &str = "grok-code-fast-1";

/// Upper bound on output tokens per turn. As with Claude, a whole-file rewrite via
/// write_file can be large; 32k fits a big rewrite and is well within
/// grok-code-fast-1's budget. We send it as `max_completion_tokens` (the xAI/
/// OpenAI-current field; `max_tokens` is deprecated).
const MAX_COMPLETION_TOKENS: u64 = 32000;

pub struct GrokBackend {
    client: reqwest::Client,
    api_key: String,
    pub model: String,
}

impl GrokBackend {
    pub fn new(model: String, api_key: String) -> Result<Self> {
        Ok(Self {
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(300))
                .build()?,
            api_key,
            model,
        })
    }

    pub async fn complete(&self, system: &str, history: &[Msg], tools: &[ToolDef]) -> Result<Turn> {
        let mut messages = Vec::with_capacity(history.len() + 1);
        // OpenAI-style: the system prompt is the FIRST message, not a top-level
        // field (Anthropic's shape).
        messages.push(json!({"role": "system", "content": system}));
        messages.extend(render_messages(history));

        let mut body = json!({
            "model": self.model,
            "max_completion_tokens": MAX_COMPLETION_TOKENS,
            "messages": messages,
        });
        // Only attach `tools` when there are any — an empty array can trip some
        // OpenAI-compatible validators, and it's wasted bytes regardless.
        if !tools.is_empty() {
            let tool_defs: Vec<Value> = tools
                .iter()
                .map(|t| {
                    json!({
                        "type": "function",
                        "function": {
                            "name": t.name,
                            "description": t.description,
                            "parameters": sanitize_schema(&t.schema),
                        },
                    })
                })
                .collect();
            body["tools"] = Value::Array(tool_defs);
        }

        let v = self.post_with_retry(&body).await?;
        parse_response(&v)
    }

    async fn post_with_retry(&self, body: &Value) -> Result<Value> {
        // A headless coordinator can run for many minutes; a transient network
        // burst must not be fatal and lose all progress. Retry generously (6
        // attempts) with exponential backoff, each sleep capped so the total
        // wait stays bounded. Mirrors claude.rs's resilience.
        const MAX_ATTEMPTS: u32 = 6;
        const MAX_DELAY: Duration = Duration::from_secs(30);
        let mut delay = Duration::from_secs(2);
        for attempt in 0..MAX_ATTEMPTS {
            let last = attempt + 1 == MAX_ATTEMPTS;
            let resp = self
                .client
                .post(API_URL)
                .header("authorization", format!("Bearer {}", self.api_key))
                .header("content-type", "application/json")
                .json(body)
                .send()
                .await;

            match resp {
                Ok(r) => {
                    let status = r.status().as_u16();
                    let v: Value = r.json().await.context("API returned non-JSON")?;
                    if status == 200 {
                        return Ok(v);
                    }
                    // OpenAI error shape: {"error": {"message": ..., "type": ...}}.
                    let msg = v["error"]["message"].as_str().unwrap_or("unknown error");
                    let kind = v["error"]["type"].as_str().unwrap_or("error");
                    // Retry only what's retryable: rate limits (429) and 5xx.
                    if (status == 429 || status >= 500) && !last {
                        eprintln!("\x1b[2m  api {kind} ({status}), retrying…\x1b[0m");
                        tokio::time::sleep(delay).await;
                        delay = (delay * 2).min(MAX_DELAY);
                        continue;
                    }
                    bail!("grok api {kind} ({status}): {msg}");
                }
                // Transport-level error (connect reset, timeout, dns): transient.
                Err(e) if !last => {
                    eprintln!("\x1b[2m  network error ({e}), retrying…\x1b[0m");
                    tokio::time::sleep(delay).await;
                    delay = (delay * 2).min(MAX_DELAY);
                }
                Err(e) => return Err(e).context("request to grok api failed"),
            }
        }
        unreachable!()
    }
}

/// OpenAI function calling requires each tool's `parameters` to be a valid
/// JSON-Schema object. MCP tools ship schemas of varying quality; normalize the
/// common rough edges so xAI's validator doesn't 400 on us:
/// - an object missing `"type"` gets `"type":"object"`;
/// - object schemas without `"properties"` get an empty `{}` (some validators
///   require it);
/// - a top-level `"$schema"` key is stripped (not part of the function-params
///   subset some validators accept).
///
/// Deliberately minimal — it doesn't recurse into nested schemas. Some MCP tool
/// schemas (atum ships 140+) may still need follow-up if xAI's validator rejects
/// a deeper construct.
fn sanitize_schema(schema: &Value) -> Value {
    let Some(obj) = schema.as_object() else {
        // A non-object schema (or a bare value) → wrap as an empty object schema,
        // which is the safe "no parameters" shape.
        return json!({"type": "object", "properties": {}});
    };
    let mut out = obj.clone();
    out.remove("$schema");
    let is_object = out.get("type").and_then(|t| t.as_str()) == Some("object");
    if out.get("type").is_none() {
        out.insert("type".into(), json!("object"));
    }
    // Ensure object schemas carry a properties map.
    if (is_object || out.get("type").and_then(|t| t.as_str()) == Some("object"))
        && out.get("properties").is_none()
    {
        out.insert("properties".into(), json!({}));
    }
    Value::Object(out)
}

/// Parse an xAI/OpenAI chat-completions response body into a normalized `Turn`.
/// Pure (no IO) so the truncation handling is unit-testable. `raw` is always
/// `None`: OpenAI has no thinking-block echo requirement, so `render_messages`
/// rebuilds a clean assistant message from the normalized fields.
fn parse_response(v: &Value) -> Result<Turn> {
    let choice = v["choices"]
        .get(0)
        .context("malformed API response: no choices[0]")?;
    let message = &choice["message"];
    let finish_reason = choice["finish_reason"].as_str().unwrap_or("");

    let mut text = message["content"].as_str().unwrap_or("").to_string();

    let mut tool_calls = Vec::new();
    if let Some(calls) = message["tool_calls"].as_array() {
        for tc in calls {
            let id = tc["id"].as_str().unwrap_or_default().to_string();
            let name = tc["function"]["name"].as_str().unwrap_or_default().to_string();
            // `arguments` is a STRINGIFIED JSON object (OpenAI convention), not an
            // object — parse it back into a Value, defaulting to {} on failure.
            let args = tc["function"]["arguments"]
                .as_str()
                .and_then(|s| serde_json::from_str::<Value>(s).ok())
                .unwrap_or_else(|| json!({}));
            tool_calls.push(ToolCall { id, name, args });
        }
    }

    let mut truncated_tool_call = false;
    if finish_reason == "length" {
        if tool_calls.is_empty() {
            // Plain text got cut off — note it and let it stand.
            text.push_str("\n[response truncated: hit max_tokens]");
        } else {
            // A tool call was cut off mid-emit: its `arguments` string is truncated,
            // so executing it would run a malformed call and the model would just
            // re-emit the same oversized call and truncate again — a corrupt-write
            // loop. Drop the tool calls so nothing executes, flag the turn so the
            // agentic loop keeps going, and feed the model a corrective note.
            tool_calls.clear();
            truncated_tool_call = true;
            text.push_str(
                "\n[your previous response was cut off mid-tool-call (hit the output limit), so \
it was NOT executed. Re-do it as a SMALLER, targeted change: prefer a focused `edit` over a \
full-file `write_file` rewrite, or split the work across several tool calls. Do not re-emit the \
same oversized call.]",
            );
        }
    }

    // raw=None always — OpenAI carries no thinking-block echo requirement, so
    // rebuild-from-normalized in render_messages is correct.
    Ok(Turn { text, tool_calls, raw: None, truncated_tool_call })
}

/// Render normalized history into OpenAI chat-completions messages. The system
/// prompt is prepended separately by `complete`.
fn render_messages(history: &[Msg]) -> Vec<Value> {
    // A tool-results Msg expands to N `role:"tool"` messages, so we may push more
    // than one entry per history item.
    let mut out = Vec::with_capacity(history.len());
    for msg in history {
        match msg.role {
            Role::Assistant => {
                // content is null when the assistant only emitted tool calls.
                let content: Value = if msg.text.is_empty() {
                    Value::Null
                } else {
                    json!(msg.text)
                };
                let mut m = json!({"role": "assistant", "content": content});
                if !msg.tool_calls.is_empty() {
                    let calls: Vec<Value> = msg
                        .tool_calls
                        .iter()
                        .map(|tc| {
                            json!({
                                "id": tc.id,
                                "type": "function",
                                "function": {
                                    "name": tc.name,
                                    // `arguments` must be a STRINGIFIED JSON object,
                                    // not an object.
                                    "arguments": serde_json::to_string(&tc.args)
                                        .unwrap_or_else(|_| "{}".to_string()),
                                },
                            })
                        })
                        .collect();
                    m["tool_calls"] = Value::Array(calls);
                }
                out.push(m);
            }
            Role::User => {
                if msg.tool_results.is_empty() {
                    out.push(json!({"role": "user", "content": msg.text}));
                } else {
                    // OpenAI needs ONE message per tool result, keyed by the
                    // originating tool call id (our ToolResult.id == ToolCall.id).
                    for r in &msg.tool_results {
                        out.push(json!({
                            "role": "tool",
                            "tool_call_id": r.id,
                            "content": r.content,
                        }));
                    }
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::{ToolCall, ToolResult};

    #[test]
    fn render_system_and_user_shapes() {
        let history = vec![Msg::user("hello")];
        let msgs = render_messages(&history);
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0]["role"], "user");
        assert_eq!(msgs[0]["content"], "hello");
    }

    #[test]
    fn render_assistant_tool_calls_stringify_arguments() {
        let msg = Msg {
            role: Role::Assistant,
            text: "running it".into(),
            tool_calls: vec![ToolCall {
                id: "call_1".into(),
                name: "write_file".into(),
                args: json!({"path": "a.rs", "content": "x"}),
            }],
            tool_results: vec![],
            raw: None,
        };
        let msgs = render_messages(&[msg]);
        assert_eq!(msgs.len(), 1);
        let m = &msgs[0];
        assert_eq!(m["role"], "assistant");
        assert_eq!(m["content"], "running it");
        let calls = m["tool_calls"].as_array().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0]["id"], "call_1");
        assert_eq!(calls[0]["type"], "function");
        assert_eq!(calls[0]["function"]["name"], "write_file");
        // arguments is a STRING, not an object.
        let args = calls[0]["function"]["arguments"].as_str().unwrap();
        let parsed: Value = serde_json::from_str(args).unwrap();
        assert_eq!(parsed["path"], "a.rs");
    }

    #[test]
    fn render_assistant_tool_only_has_null_content() {
        let msg = Msg {
            role: Role::Assistant,
            text: String::new(),
            tool_calls: vec![ToolCall {
                id: "c".into(),
                name: "ls".into(),
                args: json!({}),
            }],
            tool_results: vec![],
            raw: None,
        };
        let msgs = render_messages(&[msg]);
        assert!(msgs[0]["content"].is_null(), "empty assistant text → null content");
    }

    #[test]
    fn render_tool_results_expand_to_one_message_each() {
        let msg = Msg::tool_results(vec![
            ToolResult { id: "call_1".into(), content: "out1".into(), is_error: false },
            ToolResult { id: "call_2".into(), content: "out2".into(), is_error: true },
        ]);
        let msgs = render_messages(&[msg]);
        // One Msg → TWO role:"tool" messages.
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0]["role"], "tool");
        assert_eq!(msgs[0]["tool_call_id"], "call_1");
        assert_eq!(msgs[0]["content"], "out1");
        assert_eq!(msgs[1]["tool_call_id"], "call_2");
        assert_eq!(msgs[1]["content"], "out2");
    }

    #[test]
    fn parse_normal_tool_call_parses_arguments_string() {
        let v = json!({
            "choices": [{
                "finish_reason": "tool_calls",
                "message": {
                    "content": "doing it",
                    "tool_calls": [{
                        "id": "call_1",
                        "type": "function",
                        "function": {
                            "name": "write_file",
                            "arguments": "{\"path\": \"a\", \"content\": \"x\"}"
                        }
                    }]
                }
            }]
        });
        let turn = parse_response(&v).unwrap();
        assert_eq!(turn.text, "doing it");
        assert_eq!(turn.tool_calls.len(), 1);
        assert!(!turn.truncated_tool_call);
        assert!(turn.raw.is_none(), "grok never echoes raw");
        let tc = &turn.tool_calls[0];
        assert_eq!(tc.id, "call_1");
        assert_eq!(tc.name, "write_file");
        // The stringified arguments were parsed back into a Value.
        assert_eq!(tc.args["path"], "a");
        assert_eq!(tc.args["content"], "x");
    }

    #[test]
    fn parse_bad_arguments_string_defaults_to_empty_object() {
        let v = json!({
            "choices": [{
                "finish_reason": "tool_calls",
                "message": {
                    "content": "",
                    "tool_calls": [{
                        "id": "c",
                        "type": "function",
                        "function": {"name": "ls", "arguments": "{not json"}
                    }]
                }
            }]
        });
        let turn = parse_response(&v).unwrap();
        assert_eq!(turn.tool_calls.len(), 1);
        assert_eq!(turn.tool_calls[0].args, json!({}));
    }

    #[test]
    fn parse_truncated_tool_call_is_dropped_and_flagged() {
        // finish_reason "length" WHILE emitting a tool call: the arguments string
        // is truncated.
        let v = json!({
            "choices": [{
                "finish_reason": "length",
                "message": {
                    "content": "rewriting the file",
                    "tool_calls": [{
                        "id": "t1",
                        "type": "function",
                        "function": {"name": "write_file", "arguments": "{\"path\": \"big.rs\", \"content\": \"fn main() {"}
                    }]
                }
            }]
        });
        let turn = parse_response(&v).unwrap();
        assert!(turn.tool_calls.is_empty(), "truncated tool call must not execute");
        assert!(turn.truncated_tool_call, "must flag so the loop continues");
        assert!(turn.raw.is_none());
        assert!(turn.text.contains("cut off mid-tool-call"));
        assert!(turn.text.contains("SMALLER"));
    }

    #[test]
    fn parse_truncated_plain_text_is_noted_but_stands() {
        let v = json!({
            "choices": [{
                "finish_reason": "length",
                "message": {"content": "a very long answer"}
            }]
        });
        let turn = parse_response(&v).unwrap();
        assert!(turn.tool_calls.is_empty());
        assert!(!turn.truncated_tool_call, "plain-text truncation isn't a dropped tool call");
        assert!(turn.text.contains("response truncated"));
    }

    #[test]
    fn sanitize_adds_type_and_properties() {
        let s = sanitize_schema(&json!({"properties": {"x": {"type": "string"}}}));
        assert_eq!(s["type"], "object");
        assert!(s["properties"]["x"].is_object());

        let s2 = sanitize_schema(&json!({"type": "object"}));
        assert_eq!(s2["properties"], json!({}));
    }

    #[test]
    fn sanitize_strips_top_level_schema_key() {
        let s = sanitize_schema(&json!({"$schema": "http://json-schema.org/draft-07/schema#", "type": "object", "properties": {}}));
        assert!(s.get("$schema").is_none());
        assert_eq!(s["type"], "object");
    }
}
