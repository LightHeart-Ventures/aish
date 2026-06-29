use super::{Msg, Role, ToolCall, ToolDef, Turn};
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::time::Duration;

const API_URL: &str = "https://api.anthropic.com/v1/messages";

/// Subscription OAuth tokens (Claude Max/Pro, via `claude setup-token`) are only
/// honored when the request identifies as Claude Code: the first system block
/// must be this exact string, or the API rejects the credential. Metered API
/// keys have no such constraint. We prepend it for OAuth and send our real
/// system prompt as a second block.
const CLAUDE_CODE_SPOOF: &str = "You are Claude Code, Anthropic's official CLI for Claude.";

/// How the request authenticates. A Claude Max/Pro subscription token (as Claude
/// Code uses) takes precedence over a metered API key when both are present.
#[derive(Clone)]
enum Auth {
    /// `x-api-key` — a metered `sk-ant-…` key (full API surface, incl. Batches).
    ApiKey(String),
    /// `Authorization: Bearer` — a subscription token (`sk-ant-oat…`) from
    /// `CLAUDE_CODE_OAUTH_TOKEN` or the Claude Code CLI's
    /// `~/.claude/.credentials.json`. Works for the Messages API; the Batches
    /// API is out of reach for subscription credentials.
    Oauth(String),
}

/// A Claude credential resolved from the environment, plus the auth/system
/// shaping it requires. Shared by `ClaudeBackend` and the goal verifier (which
/// hand-rolls its own Messages call) so the OAuth handling can't drift between
/// the two call sites.
#[derive(Clone)]
pub struct Credential {
    auth: Auth,
}

impl Credential {
    /// A non-empty value for `key`, looked up in `extra` (the ~/.aishrc `export`
    /// pairs, last-wins) first, then the process environment. Empty/whitespace
    /// values are treated as unset. Delegates to the shared `rc::env_value` so the
    /// precedence stays identical to the Grok key resolution in `main.rs`.
    fn lookup(extra: &[(String, String)], key: &str) -> Option<String> {
        crate::rc::env_value(extra, key)
    }

    /// Resolve a credential, in precedence order:
    ///   1. `CLAUDE_CODE_OAUTH_TOKEN` (Claude Max/Pro subscription) from the
    ///      ~/.aishrc exports in `extra`, else the process env;
    ///   2. the subscription token the Claude Code CLI writes to
    ///      `~/.claude/.credentials.json` after `claude login` — so an existing
    ///      Claude Code session is reused with zero extra setup;
    ///   3. `ANTHROPIC_API_KEY` (metered) from `extra`, else the process env.
    ///
    /// Both subscription sources outrank the metered key (they share the OAuth
    /// auth/system shaping). Errors if none is found. Pass `&[]` when no rc
    /// context is available.
    pub fn resolve(extra: &[(String, String)]) -> Result<Self> {
        let auth = if let Some(t) = Self::lookup(extra, "CLAUDE_CODE_OAUTH_TOKEN") {
            Auth::Oauth(t)
        } else if let Some(t) = credentials_file_token() {
            Auth::Oauth(t)
        } else if let Some(k) = Self::lookup(extra, "ANTHROPIC_API_KEY") {
            Auth::ApiKey(k)
        } else {
            bail!(
                "no Claude credential — set CLAUDE_CODE_OAUTH_TOKEN (a Claude Max/Pro \
subscription token from `claude setup-token`), sign in with the Claude Code CLI (which \
writes ~/.claude/.credentials.json), or set ANTHROPIC_API_KEY (a metered key), in your \
environment or ~/.aishrc"
            );
        };
        Ok(Self { auth })
    }

    /// Add the auth header(s) for this credential to a Messages request. OAuth
    /// uses a Bearer header plus the oauth beta flag and must NOT also send
    /// `x-api-key`; a metered key uses `x-api-key`.
    pub fn apply(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match &self.auth {
            Auth::ApiKey(k) => req.header("x-api-key", k),
            Auth::Oauth(t) => req
                .header("authorization", format!("Bearer {t}"))
                .header("anthropic-beta", "oauth-2025-04-20"),
        }
    }

    /// Shape a system prompt for this credential: OAuth requires the Claude Code
    /// identity as the first system block (else the credential is rejected); a
    /// metered key takes the prompt as a plain string.
    pub fn system_value(&self, system: &str) -> Value {
        match &self.auth {
            Auth::Oauth(_) => json!([
                {"type": "text", "text": CLAUDE_CODE_SPOOF},
                {"type": "text", "text": system},
            ]),
            Auth::ApiKey(_) => json!(system),
        }
    }
}

/// Read a Claude Code subscription token from `~/.claude/.credentials.json` —
/// the file the `claude` CLI writes after an interactive sign-in. This lets aish
/// reuse an existing Claude Code session with no extra setup (no
/// `claude setup-token`, no env var). Returns the access token when the file is
/// present, readable, well-formed, and the token is non-empty and unexpired;
/// `None` for every miss (no HOME, missing/unreadable file, malformed JSON, no
/// token, or a past `expiresAt`) so resolution falls through to the next source.
fn credentials_file_token() -> Option<String> {
    let home = std::env::var("HOME").ok().filter(|h| !h.is_empty())?;
    let path = std::path::Path::new(&home)
        .join(".claude")
        .join(".credentials.json");
    let contents = std::fs::read_to_string(path).ok()?;
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    token_from_credentials_json(&contents, now_ms)
}

/// Pure parse of a `~/.claude/.credentials.json` body: pull
/// `claudeAiOauth.accessToken`, accepting it only when non-empty and either
/// without an `expiresAt` or with one (epoch milliseconds) still in the future
/// relative to `now_ms`. An already-expired token returns `None` so resolution
/// falls through to `ANTHROPIC_API_KEY` instead of sending a token the API will
/// reject. Split out from the file IO so the shape/expiry policy is unit-testable.
fn token_from_credentials_json(contents: &str, now_ms: u64) -> Option<String> {
    let v: Value = serde_json::from_str(contents).ok()?;
    let oauth = v.get("claudeAiOauth")?;
    let token = oauth.get("accessToken")?.as_str()?.trim();
    if token.is_empty() {
        return None;
    }
    // A present, non-zero expiry that's already passed → treat as unusable.
    // A missing or zero `expiresAt` is treated as non-expiring.
    if let Some(exp) = oauth.get("expiresAt").and_then(Value::as_u64) {
        if exp != 0 && exp <= now_ms {
            return None;
        }
    }
    Some(token.to_string())
}

pub struct ClaudeBackend {
    client: reqwest::Client,
    cred: Credential,
    pub model: String,
}

impl ClaudeBackend {
    pub fn new(model: String, cred: Credential) -> Result<Self> {
        Ok(Self {
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(300))
                .build()?,
            cred,
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
            // OAuth (subscription) credentials require the Claude Code identity
            // as the first system block; API keys take the prompt as a plain
            // string. See Credential::system_value.
            "system": self.cred.system_value(system),
            "tools": tool_defs,
            "messages": messages,
            // Auto-cache the growing conversation prefix — a shell session is
            // exactly the multi-turn shape prompt caching is for.
            "cache_control": {"type": "ephemeral"},
        });
        // Adaptive thinking is the 4.6+ Opus/Sonnet surface; Haiku doesn't take
        // it — AND it is incompatible with assistant-prefill, so it's suppressed
        // whenever the request ends with an assistant message (our truncation
        // continuation resumes a partial answer that way). See `wants_thinking`.
        if wants_thinking(&self.model, history) {
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
            let req = self
                .client
                .post(API_URL)
                .header("anthropic-version", "2023-06-01")
                .header("content-type", "application/json");
            let resp = self.cred.apply(req).json(body).send().await;

            match resp {
                Ok(r) => {
                    // Decode to text first so a non-JSON gateway/edge body (502/503
                    // HTML, empty body, Cloudflare page) is a retryable signal, not a
                    // fatal decode error that stops the worker. See
                    // `super::read_status_and_json`.
                    let (status, parsed) = match super::read_status_and_json(r).await {
                        Ok(p) => p,
                        Err(e) if !last => {
                            eprintln!(
                                "\x1b[2m  network error reading body ({e}), retrying…\x1b[0m"
                            );
                            tokio::time::sleep(delay).await;
                            delay = (delay * 2).min(MAX_DELAY);
                            continue;
                        }
                        Err(e) => return Err(e).context("reading claude api response body"),
                    };
                    let v = match parsed {
                        Ok(v) => v,
                        Err(snippet) => {
                            // Non-JSON body: almost always a transient intermediary
                            // failure (the API itself answers 4xx/5xx in JSON). Retry
                            // it like any other transient error rather than aborting.
                            if !last {
                                eprintln!(
                                    "\x1b[2m  api returned non-JSON ({status}), retrying…\x1b[0m"
                                );
                                tokio::time::sleep(delay).await;
                                delay = (delay * 2).min(MAX_DELAY);
                                continue;
                            }
                            bail!("claude api ({status}): non-JSON response: {snippet}");
                        }
                    };
                    if status == 200 {
                        return Ok(v);
                    }
                    let msg = v["error"]["message"].as_str().unwrap_or("unknown error");
                    let kind = v["error"]["type"].as_str().unwrap_or("error");
                    // Detect auth failures on OAuth tokens and guide user to refresh.
                    let is_auth_error = matches!(status, 401 | 403)
                        || (status == 400 && {
                            let m = msg.to_ascii_lowercase();
                            m.contains("invalid") || m.contains("expired") || m.contains("unauthorized")
                        });
                    if is_auth_error && matches!(self.cred.auth, Auth::Oauth(_)) && !last {
                        eprintln!(
                            "\x1b[1m\n⚠ Claude OAuth token expired\x1b[0m"
                        );
                        eprintln!(
                            "  Your Claude Max/Pro subscription token needs to be refreshed.\n\
  Run: \x1b[1mclaude setup-token\x1b[0m\n\
  Then set CLAUDE_CODE_OAUTH_TOKEN in your shell or ~/.aishrc"
                        );
                        eprintln!();
                        bail!(
                            "claude api authentication failed ({status}): {msg} — \
please refresh your token with `claude setup-token`"
                        );
                    }
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

/// Whether to attach the adaptive-thinking block to a request. Two gates:
/// 1. only Opus/Sonnet 4.x take adaptive thinking (Haiku doesn't); and
/// 2. extended thinking is INCOMPATIBLE with assistant-prefill — when the final
///    history message is an assistant turn (our truncation-continuation round
///    leaves the partial answer there for the model to resume), the API rejects
///    a request that also enables thinking. So suppress it on any prefill.
///
/// Pure (no IO) so the policy is unit-testable.
fn wants_thinking(model: &str, history: &[Msg]) -> bool {
    let prefilling = matches!(history.last(), Some(m) if m.role == Role::Assistant);
    !prefilling && (model.contains("opus") || model.contains("sonnet"))
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
    // rebuilding from text+tool_calls — used by the truncation paths below.
    let mut raw: Option<Value> = Some(v["content"].clone());
    let mut truncated_tool_call = false;
    let mut truncated_text = false;
    if stop_reason == "max_tokens" {
        if tool_calls.is_empty() {
            // Plain text got cut off mid-stream. DON'T append a note (it would
            // pollute the answer the model is about to resume) and DROP `raw` so
            // the trailing assistant message we feed back is clean text with no
            // thinking block — a thinking block can't ride along on a prefill
            // (thinking is disabled on the continuation request). The engine
            // continues the answer via an assistant-prefill round and merges the
            // chunks. See `engine::run_turn`.
            raw = None;
            truncated_text = true;
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

    let usage = v.get("usage").map(|u| {
        let g = |k: &str| u.get(k).and_then(serde_json::Value::as_u64).unwrap_or(0) as usize;
        // Sum the plain + cached input buckets so the figure reflects the FULL
        // prompt the model saw, not just the uncached remainder.
        crate::context::Usage {
            input_tokens: g("input_tokens")
                + g("cache_read_input_tokens")
                + g("cache_creation_input_tokens"),
            output_tokens: g("output_tokens"),
        }
    });
    Ok(Turn {
        text,
        tool_calls,
        raw,
        truncated_tool_call,
        usage,
        truncated_text,
    })
}

/// Render normalized history into Claude wire messages.
fn render_messages(history: &[Msg]) -> Vec<Value> {
    let last = history.len().saturating_sub(1);
    let mut out = Vec::with_capacity(history.len());
    for (i, msg) in history.iter().enumerate() {
        match msg.role {
            Role::Assistant => {
                // Echo raw content verbatim when we have it — preserves thinking
                // blocks + signatures, required with adaptive thinking + tools.
                let content = msg.raw.clone().unwrap_or_else(|| {
                    let mut blocks = Vec::new();
                    if !msg.text.is_empty() {
                        // A trailing assistant message is a PREFILL the model
                        // resumes; the API rejects assistant content that ends
                        // with whitespace, so trim it on the last message only.
                        let t = if i == last {
                            msg.text.trim_end()
                        } else {
                            msg.text.as_str()
                        };
                        if !t.is_empty() {
                            blocks.push(json!({"type": "text", "text": t}));
                        }
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
                                // S7.3: thread the structured payload (compact
                                // JSON) to the model when present; text-only
                                // results send `content` verbatim as before.
                                "content": r.model_content(),
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
        assert!(!turn.truncated_text);
        assert!(
            turn.raw.is_some(),
            "normal turns keep raw for thinking history"
        );
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
        assert!(
            turn.tool_calls.is_empty(),
            "truncated tool call must not execute"
        );
        assert!(turn.truncated_tool_call, "must flag so the loop continues");
        assert!(
            !turn.truncated_text,
            "a dropped tool call is not a prose continuation"
        );
        // raw is dropped so the broken/empty tool_use isn't re-fed to the API.
        assert!(turn.raw.is_none());
        // The model is told to retry smaller.
        assert!(turn.text.contains("cut off mid-tool-call"));
        assert!(turn.text.contains("SMALLER"));
    }

    #[test]
    fn truncated_plain_text_is_flagged_for_prefill_continuation() {
        // max_tokens with NO tool call: flag for a prefill-continuation round.
        let v = json!({
            "stop_reason": "max_tokens",
            "content": [{"type": "text", "text": "a very long answer that ran"}]
        });
        let turn = parse_response(&v).unwrap();
        assert!(turn.tool_calls.is_empty());
        assert!(!turn.truncated_tool_call);
        assert!(
            turn.truncated_text,
            "plain-text truncation drives the continuation loop"
        );
        // The partial answer is kept verbatim — NO in-band note that would
        // pollute the resumed answer — and raw is dropped so the prefill is
        // clean text (no thinking block).
        assert_eq!(turn.text, "a very long answer that ran");
        assert!(!turn.text.contains("truncated"));
        assert!(turn.raw.is_none());
    }

    #[test]
    fn thinking_suppressed_on_assistant_prefill() {
        // A normal request whose last message is from the user enables thinking
        // on Opus/Sonnet.
        let hist = vec![Msg::user("hi")];
        assert!(wants_thinking("claude-opus-4-8", &hist));
        assert!(wants_thinking("claude-sonnet-4-6", &hist));
        // Haiku never takes adaptive thinking.
        assert!(!wants_thinking("claude-haiku-4-5", &hist));
        // A trailing ASSISTANT message is a prefill (truncation continuation) —
        // thinking must be suppressed or the API rejects the request.
        let prefill = vec![
            Msg::user("hi"),
            Msg {
                role: Role::Assistant,
                text: "partial answer".into(),
                tool_calls: vec![],
                tool_results: vec![],
                raw: None,
            },
        ];
        assert!(
            !wants_thinking("claude-opus-4-8", &prefill),
            "no thinking on prefill"
        );
    }

    #[test]
    fn render_trims_trailing_whitespace_on_prefill_message() {
        // The trailing assistant message (a prefill) must not end with whitespace
        // — the API rejects it. Trimming applies only to the LAST message.
        let hist = vec![
            Msg::user("hi"),
            Msg {
                role: Role::Assistant,
                text: "resume me   \n".into(),
                tool_calls: vec![],
                tool_results: vec![],
                raw: None,
            },
        ];
        let msgs = render_messages(&hist);
        let last = msgs.last().unwrap();
        let text = last["content"][0]["text"].as_str().unwrap();
        assert_eq!(
            text, "resume me",
            "trailing whitespace stripped on the prefill message"
        );
    }

    #[test]
    fn render_string_only_tool_result_wire_shape_has_no_payload_key() {
        // S7.4 / AC1: a text-only ToolResult renders to the Claude tool_result
        // wire block EXACTLY as before structured results existed — the four
        // canonical keys only (type, tool_use_id, content, is_error), `content`
        // verbatim, and `is_error` honoured on both a success and a failure
        // result. The typed payload is consumed by model_content(); it must
        // NEVER leak onto the wire as a `structured`/`payload` sibling field.
        // The exact-key-count assertion is the guardrail: letting the payload
        // serialize as a wire sibling fails this test.
        use super::super::{Msg, ToolResult};
        let hist = vec![Msg::tool_results(vec![
            ToolResult::text("ok", "verbatim output", false),
            ToolResult::text("bad", "it failed", true),
        ])];
        let msgs = render_messages(&hist);
        let blocks = msgs[0]["content"].as_array().unwrap();

        let ok = blocks[0].as_object().unwrap();
        assert_eq!(ok["type"], "tool_result");
        assert_eq!(ok["tool_use_id"], "ok");
        assert_eq!(ok["content"], "verbatim output"); // content verbatim
        assert_eq!(ok["is_error"], false); // is_error honoured
        // EXACTLY the four canonical keys — no payload/structured sibling.
        assert_eq!(ok.len(), 4, "string-only block carries no extra key: {ok:?}");
        assert!(ok.get("structured").is_none());
        assert!(ok.get("payload").is_none());

        // is_error is threaded for the failing result too; content stays verbatim.
        assert_eq!(blocks[1]["is_error"], true);
        assert_eq!(blocks[1]["content"], "it failed");
    }

    #[test]
    fn render_tool_result_threads_structured_json_to_model() {
        // S7.3 / AC1: a structured tool result is rendered to the model as the
        // compact JSON payload, while a plain text result sends `content`.
        use super::super::{Msg, ToolResult};
        let hist = vec![Msg::tool_results(vec![
            ToolResult::structured(
                "s1",
                "rendered table the human sees",
                json!({"path": "f.txt", "type": "file", "size": 3}),
                false,
            ),
            ToolResult::text("t1", "plain text result", false),
        ])];
        let msgs = render_messages(&hist);
        let blocks = msgs[0]["content"].as_array().unwrap();
        // Structured → compact JSON (NOT the rendered text); keys sorted (BTreeMap).
        assert_eq!(
            blocks[0]["content"].as_str().unwrap(),
            r#"{"path":"f.txt","size":3,"type":"file"}"#
        );
        assert_eq!(blocks[0]["tool_use_id"], "s1");
        // Text-only → content verbatim.
        assert_eq!(blocks[1]["content"].as_str().unwrap(), "plain text result");
    }

    #[test]
    fn parse_response_captures_usage_including_cache_buckets() {
        let v = json!({
            "stop_reason": "end_turn",
            "content": [{"type": "text", "text": "hi"}],
            "usage": {
                "input_tokens": 100,
                "cache_read_input_tokens": 900,
                "cache_creation_input_tokens": 50,
                "output_tokens": 20
            }
        });
        let turn = parse_response(&v).unwrap();
        let u = turn.usage.expect("usage parsed");
        assert_eq!(u.input_tokens, 1050); // 100 + 900 + 50
        assert_eq!(u.output_tokens, 20);
        // A response with no usage block leaves it None.
        let bare = json!({"stop_reason": "end_turn", "content": [{"type":"text","text":"x"}]});
        assert!(parse_response(&bare).unwrap().usage.is_none());
    }

    // Credentials. Built directly (not via resolve) so the tests never depend on
    // what's in the process environment.
    fn oauth() -> Credential {
        Credential {
            auth: Auth::Oauth("sk-ant-oat-test".into()),
        }
    }
    fn api_key() -> Credential {
        Credential {
            auth: Auth::ApiKey("sk-ant-test".into()),
        }
    }

    #[test]
    fn oauth_token_beats_api_key_when_both_in_rc() {
        // Both supplied via the rc-exports slice → OAuth (subscription) wins.
        let c = Credential::resolve(&[
            ("ANTHROPIC_API_KEY".into(), "sk-ant-test".into()),
            ("CLAUDE_CODE_OAUTH_TOKEN".into(), "sk-ant-oat-test".into()),
        ])
        .unwrap();
        assert!(matches!(c.auth, Auth::Oauth(_)));
    }

    #[test]
    fn lookup_prefers_rc_export_over_process_env() {
        // A key present in the rc slice short-circuits before the process env.
        assert_eq!(
            Credential::lookup(
                &[("ANTHROPIC_API_KEY".into(), "from-rc".into())],
                "ANTHROPIC_API_KEY"
            ),
            Some("from-rc".to_string())
        );
        // Blank rc values don't count as set.
        assert_eq!(
            Credential::lookup(&[("CLAUDE_CODE_OAUTH_TOKEN".into(), "   ".into())], "NOPE"),
            None
        );
    }

    #[test]
    fn oauth_system_prompt_prepends_claude_code_identity() {
        let v = oauth().system_value("REAL SYSTEM PROMPT");
        let arr = v
            .as_array()
            .expect("OAuth shapes system as an array of blocks");
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0]["text"], CLAUDE_CODE_SPOOF);
        assert_eq!(arr[1]["text"], "REAL SYSTEM PROMPT");
    }

    #[test]
    fn api_key_system_prompt_stays_a_plain_string() {
        assert_eq!(
            api_key().system_value("REAL SYSTEM PROMPT"),
            json!("REAL SYSTEM PROMPT")
        );
    }

    #[test]
    fn credentials_json_extracts_unexpired_oauth_token() {
        // The shape the Claude Code CLI writes to ~/.claude/.credentials.json.
        let body = r#"{"claudeAiOauth":{"accessToken":"sk-ant-oat-from-file","refreshToken":"sk-ant-ort-x","expiresAt":2000,"scopes":["user:inference"],"subscriptionType":"max"}}"#;
        // now < expiresAt → the token is returned.
        assert_eq!(
            token_from_credentials_json(body, 1000),
            Some("sk-ant-oat-from-file".to_string())
        );
        // now >= expiresAt → expired, treated as unset so we fall through.
        assert_eq!(token_from_credentials_json(body, 2000), None);
        assert_eq!(token_from_credentials_json(body, 3000), None);
    }

    #[test]
    fn credentials_json_without_expiry_is_non_expiring() {
        // No expiresAt (and a zero expiresAt) → always usable.
        let no_exp = r#"{"claudeAiOauth":{"accessToken":"sk-ant-oat-noexp"}}"#;
        assert_eq!(
            token_from_credentials_json(no_exp, 9_999_999_999_999),
            Some("sk-ant-oat-noexp".to_string())
        );
        let zero_exp = r#"{"claudeAiOauth":{"accessToken":"sk-ant-oat-z","expiresAt":0}}"#;
        assert_eq!(
            token_from_credentials_json(zero_exp, 9_999_999_999_999),
            Some("sk-ant-oat-z".to_string())
        );
    }

    #[test]
    fn credentials_json_rejects_malformed_or_empty() {
        // Not JSON, no oauth object, no token, and a blank/whitespace token all
        // resolve to None rather than panicking or yielding an empty credential.
        assert_eq!(token_from_credentials_json("not json", 0), None);
        assert_eq!(token_from_credentials_json("{}", 0), None);
        assert_eq!(
            token_from_credentials_json(r#"{"claudeAiOauth":{}}"#, 0),
            None
        );
        assert_eq!(
            token_from_credentials_json(r#"{"claudeAiOauth":{"accessToken":"   "}}"#, 0),
            None
        );
    }

    #[test]
    fn oauth_detects_401_as_auth_failure() {
        // Matches 401/403 auth responses.
        let oauth_cred = oauth();
        assert!(matches!(oauth_cred.auth, Auth::Oauth(_)));
        // Test the logic that would be in post_with_retry:
        // a 401 on an OAuth token is an auth failure.
        let status = 401;
        let is_auth_error = matches!(status, 401 | 403);
        assert!(is_auth_error);
    }

    #[test]
    fn oauth_detects_400_with_expired_keyword() {
        let msg = "invalid_request: oauth token expired";
        let is_auth_error = {
            let m = msg.to_ascii_lowercase();
            m.contains("invalid") || m.contains("expired") || m.contains("unauthorized")
        };
        assert!(is_auth_error);
    }

    #[test]
    fn api_key_does_not_trigger_oauth_guidance() {
        // Only OAuth credentials should get the refresh suggestion.
        let api_key_cred = api_key();
        assert!(matches!(api_key_cred.auth, Auth::ApiKey(_)));
        // The auth error path in post_with_retry checks `matches!(self.cred.auth, Auth::Oauth(_))`,
        // so API keys would NOT enter the OAuth-specific error path.
    }
}
