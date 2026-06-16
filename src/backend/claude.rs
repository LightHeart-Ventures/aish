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
            // A whole-file rewrite via write_file plus adaptive-thinking output can
            // exceed a tight cap and trip stop_reason:"max_tokens" mid-tool-call,
            // truncating the tool_use JSON. 32k fits a large file rewrite and is
            // well within Opus/Sonnet 4.x's documented max output (64k).
            "max_tokens": 32000,
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
        parse_response(&v)
    }

    async fn post_with_retry(&self, body: &Value) -> Result<Value> {
        // A headless coordinator can run for many minutes; a transient
        // `Connection reset by peer` / timeout burst must not be fatal and lose
        // all progress. Retry generously (6 attempts) with exponential backoff,
        // but cap each sleep at MAX_DELAY so the total wait stays bounded.
        const MAX_ATTEMPTS: u32 = 6;
        const MAX_DELAY: Duration = Duration::from_secs(30);
        let mut delay = Duration::from_secs(2);
        for attempt in 0..MAX_ATTEMPTS {
            let last = attempt + 1 == MAX_ATTEMPTS;
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
                    // Retry only what's retryable: rate limits (429) and 5xx.
                    // Other 4xx (bad request, auth, …) fail fast — retrying can't help.
                    if (status == 429 || status >= 500) && !last {
                        eprintln!("\x1b[2m  api {kind} ({status}), retrying…\x1b[0m");
                        tokio::time::sleep(delay).await;
                        delay = (delay * 2).min(MAX_DELAY);
                        continue;
                    }
                    bail!("claude api {kind} ({status}): {msg}");
                }
                // Transport-level error (connect reset, timeout, dns): transient,
                // retry until attempts are exhausted.
                Err(e) if !last => {
                    eprintln!("\x1b[2m  network error ({e}), retrying…\x1b[0m");
                    tokio::time::sleep(delay).await;
                    delay = (delay * 2).min(MAX_DELAY);
                }
                Err(e) => return Err(e).context("request to claude api failed"),
            }
        }
        unreachable!()
    }
}

/// Parse a Claude `/messages` response body into a normalized `Turn`. Pure (no
/// IO) so the truncation handling is unit-testable. Splits content into text +
/// tool calls, preserves `raw` for adaptive-thinking history, and applies the
/// max_tokens truncation policy (see below).
fn parse_response(v: &Value) -> Result<Turn> {
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
    // `raw` echoes the assistant content verbatim into history (preserves
    // thinking signatures with adaptive thinking + tools). `None` falls back to
    // rebuilding from text+tool_calls — used by the truncation path below.
    let mut raw: Option<Value> = Some(v["content"].clone());
    let mut truncated_tool_call = false;
    if stop_reason == "max_tokens" {
        if tool_calls.is_empty() {
            // Plain text got cut off — note it and let it stand.
            text.push_str("\n[response truncated: hit max_tokens]");
        } else {
            // A tool call was cut off mid-emit: its `input` JSON is truncated, so
            // executing it would run a malformed call (e.g. a 0-byte write) and the
            // model would just re-emit the same giant call and truncate again — an
            // infinite corrupt-write loop. Instead, DROP the tool calls so nothing
            // executes, and DROP `raw` too: keeping the raw content would re-feed
            // the partial `tool_use` block (an assistant `tool_use` with no matching
            // `tool_result` is an API error next round, and it carries the broken
            // JSON), while a raw array with its tool_use stripped could be empty
            // (also invalid). With raw None, `render_messages` rebuilds a clean
            // text-only assistant message from `text`. We forfeit this turn's
            // thinking block — fine, since that reasoning produced the oversized
            // call we're correcting.
            tool_calls.clear();
            truncated_tool_call = true;
            raw = None;
            text.push_str(
                "\n[your previous response was cut off mid-tool-call (hit the output limit), so \
it was NOT executed. Re-do it as a SMALLER, targeted change: prefer a focused `edit` over a \
full-file `write_file` rewrite, or split the work across several tool calls. Do not re-emit the \
same oversized call.]",
            );
        }
    }

    Ok(Turn { text, tool_calls, raw, truncated_tool_call })
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_normal_turn_keeps_tool_calls_and_raw() {
        let v = json!({
            "stop_reason": "tool_use",
            "content": [
                {"type": "text", "text": "doing it"},
                {"type": "tool_use", "id": "t1", "name": "write_file", "input": {"path": "a", "content": "x"}}
            ]
        });
        let turn = parse_response(&v).unwrap();
        assert_eq!(turn.tool_calls.len(), 1);
        assert!(!turn.truncated_tool_call);
        assert!(turn.raw.is_some(), "normal turns keep raw for thinking history");
        assert_eq!(turn.text, "doing it");
    }

    #[test]
    fn truncated_tool_call_is_dropped_and_flagged() {
        // max_tokens WHILE emitting a tool call: the input JSON is truncated.
        let v = json!({
            "stop_reason": "max_tokens",
            "content": [
                {"type": "text", "text": "rewriting the file"},
                {"type": "tool_use", "id": "t1", "name": "write_file", "input": {"path": "big.rs", "content": "fn main() {"}}
            ]
        });
        let turn = parse_response(&v).unwrap();
        // The partial tool call must NOT be surfaced for execution.
        assert!(turn.tool_calls.is_empty(), "truncated tool call must not execute");
        assert!(turn.truncated_tool_call, "must flag so the loop continues");
        // raw is dropped so the broken/empty tool_use isn't re-fed to the API.
        assert!(turn.raw.is_none());
        // The model is told to retry smaller.
        assert!(turn.text.contains("cut off mid-tool-call"));
        assert!(turn.text.contains("SMALLER"));
    }

    #[test]
    fn truncated_plain_text_is_noted_but_stands() {
        // max_tokens with NO tool call: just note it; nothing to drop.
        let v = json!({
            "stop_reason": "max_tokens",
            "content": [{"type": "text", "text": "a very long answer"}]
        });
        let turn = parse_response(&v).unwrap();
        assert!(turn.tool_calls.is_empty());
        assert!(!turn.truncated_tool_call, "plain-text truncation isn't a dropped tool call");
        assert!(turn.raw.is_some());
        assert!(turn.text.contains("response truncated"));
    }
}
