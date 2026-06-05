use super::{Msg, Role, ToolCall, ToolDef, Turn};
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::time::Duration;

const API_URL: &str = "https://api.anthropic.com/v1/messages";

pub struct ClaudeBackend {
    client: reqwest::Client,
    api_key: String,
    pub model: String,
}

impl ClaudeBackend {
    pub fn new(model: String) -> Result<Self> {
        let api_key = std::env::var("ANTHROPIC_API_KEY")
            .context("ANTHROPIC_API_KEY is not set — the claude backend needs it")?;
        Ok(Self {
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(300))
                .build()?,
            api_key,
            model,
        })
    }

    pub async fn complete(&self, system: &str, history: &[Msg], tools: &[ToolDef]) -> Result<Turn> {
        let messages = render_messages(history);
        let tool_defs: Vec<Value> = tools
            .iter()
            .map(|t| {
                json!({
                    "name": t.name,
                    "description": t.description,
                    "input_schema": t.schema,
                })
            })
            .collect();

        let mut body = json!({
            "model": self.model,
            "max_tokens": 16000,
            "system": system,
            "tools": tool_defs,
            "messages": messages,
            // Auto-cache the growing conversation prefix — a shell session is
            // exactly the multi-turn shape prompt caching is for.
            "cache_control": {"type": "ephemeral"},
        });
        // Adaptive thinking is the 4.6+ Opus/Sonnet surface; Haiku doesn't take it.
        if self.model.contains("opus") || self.model.contains("sonnet") {
            body["thinking"] = json!({"type": "adaptive"});
        }

        let v = self.post_with_retry(&body).await?;

        let stop_reason = v["stop_reason"].as_str().unwrap_or("");
        let content = v["content"]
            .as_array()
            .context("malformed API response: no content array")?;

        let mut text = String::new();
        let mut tool_calls = Vec::new();
        for block in content {
            match block["type"].as_str() {
                Some("text") => {
                    if let Some(t) = block["text"].as_str() {
                        if !text.is_empty() {
                            text.push('\n');
                        }
                        text.push_str(t);
                    }
                }
                Some("tool_use") => tool_calls.push(ToolCall {
                    id: block["id"].as_str().unwrap_or_default().to_string(),
                    name: block["name"].as_str().unwrap_or_default().to_string(),
                    args: block["input"].clone(),
                }),
                _ => {} // thinking blocks etc. — preserved via raw
            }
        }
        if stop_reason == "max_tokens" {
            text.push_str("\n[response truncated: hit max_tokens]");
        }

        Ok(Turn {
            text,
            tool_calls,
            raw: Some(v["content"].clone()),
        })
    }

    async fn post_with_retry(&self, body: &Value) -> Result<Value> {
        let mut delay = Duration::from_secs(2);
        for attempt in 0..3 {
            let resp = self
                .client
                .post(API_URL)
                .header("x-api-key", &self.api_key)
                .header("anthropic-version", "2023-06-01")
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
                    let msg = v["error"]["message"].as_str().unwrap_or("unknown error");
                    let kind = v["error"]["type"].as_str().unwrap_or("error");
                    // Retry only what's retryable.
                    if (status == 429 || status >= 500) && attempt < 2 {
                        eprintln!("\x1b[2m  api {kind} ({status}), retrying…\x1b[0m");
                        tokio::time::sleep(delay).await;
                        delay *= 2;
                        continue;
                    }
                    bail!("claude api {kind} ({status}): {msg}");
                }
                Err(e) if attempt < 2 => {
                    eprintln!("\x1b[2m  network error ({e}), retrying…\x1b[0m");
                    tokio::time::sleep(delay).await;
                    delay *= 2;
                }
                Err(e) => return Err(e).context("request to claude api failed"),
            }
        }
        unreachable!()
    }
}

/// Render normalized history into Claude wire messages.
fn render_messages(history: &[Msg]) -> Vec<Value> {
    let mut out = Vec::with_capacity(history.len());
    for msg in history {
        match msg.role {
            Role::Assistant => {
                // Echo raw content verbatim when we have it — preserves thinking
                // blocks + signatures, required with adaptive thinking + tools.
                let content = msg.raw.clone().unwrap_or_else(|| {
                    let mut blocks = Vec::new();
                    if !msg.text.is_empty() {
                        blocks.push(json!({"type": "text", "text": msg.text}));
                    }
                    for tc in &msg.tool_calls {
                        blocks.push(json!({
                            "type": "tool_use", "id": tc.id, "name": tc.name, "input": tc.args
                        }));
                    }
                    Value::Array(blocks)
                });
                out.push(json!({"role": "assistant", "content": content}));
            }
            Role::User => {
                if msg.tool_results.is_empty() {
                    out.push(json!({"role": "user", "content": msg.text}));
                } else {
                    let blocks: Vec<Value> = msg
                        .tool_results
                        .iter()
                        .map(|r| {
                            json!({
                                "type": "tool_result",
                                "tool_use_id": r.id,
                                "content": r.content,
                                "is_error": r.is_error,
                            })
                        })
                        .collect();
                    out.push(json!({"role": "user", "content": blocks}));
                }
            }
        }
    }
    out
}
