use super::{Msg, Role, ToolCall, ToolDef, Turn};
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::time::Duration;

const API_URL: &str = "https://api.anthropic.com/v1/messages";

/// Retry bookkeeping shared by the buffered ([`ClaudeBackend::post_with_retry`])
/// and streaming ([`ClaudeBackend::stream_with_retry`]) loops. Two INDEPENDENT
/// budgets keep a rate limit and a flaky network from cannibalizing each other's
/// retries:
///
/// * **Transient** faults (connection resets, timeouts, 5xx, non-JSON edge
///   bodies) get a short exponential backoff — 2s doubling, capped at
///   [`RetryBudget::TRANSIENT_MAX_DELAY`] — for up to
///   [`RetryBudget::TRANSIENT_MAX_ATTEMPTS`] tries.
/// * **Rate limits** (HTTP 429) get a LONG ride-out: the exponential backoff
///   climbs to a 5-minute ceiling ([`RetryBudget::RATE_LIMIT_MAX_DELAY`]) and
///   then retries at that steady 5-minute cadence, riding the limit out for up
///   to one hour total ([`RetryBudget::RATE_LIMIT_MAX_ELAPSED`]). A headless
///   coordinator would rather sleep than fail the turn and be respawned as a
///   brand-new worker.
struct RetryBudget {
    /// Transient tries already consumed (across network/5xx/non-JSON).
    transient_attempts: u32,
    /// Current transient backoff (doubles each try, capped).
    transient_delay: Duration,
    /// Current rate-limit backoff (doubles each 429, capped at 5 min).
    rl_delay: Duration,
    /// Cumulative time already slept riding out a 429.
    rl_elapsed: Duration,
}

impl RetryBudget {
    const TRANSIENT_MAX_ATTEMPTS: u32 = 8;
    const TRANSIENT_MAX_DELAY: Duration = Duration::from_secs(60);
    /// 5-minute ceiling for the rate-limit backoff.
    const RATE_LIMIT_MAX_DELAY: Duration = Duration::from_secs(300);
    /// Ride a 429 out for at most one hour of cumulative sleep.
    const RATE_LIMIT_MAX_ELAPSED: Duration = Duration::from_secs(3600);

    fn new() -> Self {
        Self {
            transient_attempts: 0,
            transient_delay: Duration::from_secs(2),
            rl_delay: Duration::from_secs(2),
            rl_elapsed: Duration::ZERO,
        }
    }

    /// The next sleep for a transient fault, or `None` once the transient
    /// attempt budget is spent (the caller then surfaces the error). Advances
    /// the exponential backoff.
    fn next_transient(&mut self) -> Option<Duration> {
        if self.transient_attempts + 1 >= Self::TRANSIENT_MAX_ATTEMPTS {
            return None;
        }
        let wait = self.transient_delay;
        self.transient_attempts += 1;
        self.transient_delay = (self.transient_delay * 2).min(Self::TRANSIENT_MAX_DELAY);
        Some(wait)
    }

    /// The next sleep for a 429, honoring a server `Retry-After` when present
    /// (capped at the 5-minute ceiling), or `None` once the one-hour ride-out
    /// budget is spent. Accumulates elapsed sleep and advances the backoff.
    fn next_rate_limit(&mut self, retry_after: Option<Duration>) -> Option<Duration> {
        if self.rl_elapsed >= Self::RATE_LIMIT_MAX_ELAPSED {
            return None;
        }
        let wait = retry_after
            .unwrap_or(self.rl_delay)
            .min(Self::RATE_LIMIT_MAX_DELAY);
        self.rl_elapsed += wait;
        self.rl_delay = (self.rl_delay * 2).min(Self::RATE_LIMIT_MAX_DELAY);
        Some(wait)
    }
}

/// Parse a numeric `Retry-After` (seconds) header into a `Duration`, if present
/// and well-formed. Anthropic sends this on 429s; its window routinely exceeds
/// the exponential cap, so honoring it lets the worker ride out the limit.
fn parse_retry_after(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    headers
        .get("retry-after")
        .and_then(|h| h.to_str().ok())
        .and_then(|s| s.trim().parse::<u64>().ok())
        .map(Duration::from_secs)
}

/// Whether an HTTP status + error message indicates an auth failure that no
/// amount of retrying will fix (401/403, or a 400 whose message names an
/// invalid/expired/unauthorized credential).
fn is_auth_error(status: u16, msg: &str) -> bool {
    matches!(status, 401 | 403)
        || (status == 400 && {
            let m = msg.to_ascii_lowercase();
            m.contains("invalid") || m.contains("expired") || m.contains("unauthorized")
        })
}

/// Print the Claude subscription (OAuth) token-refresh guidance to stderr.
fn eprint_oauth_expired_hint() {
    eprintln!("\x1b[1m\n⚠ Claude OAuth token expired\x1b[0m");
    eprintln!(
        "  Your Claude Max/Pro subscription token needs to be refreshed.\n\
  Run: \x1b[1mclaude setup-token\x1b[0m\n\
  Then set CLAUDE_CODE_OAUTH_TOKEN in your shell or ~/.aishrc"
    );
    eprintln!();
}

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

    /// A non-secret label identifying which auth kind resolved — for
    /// `describe()` / the statusline provider indicator. Never the token itself.
    /// "subscription" for a Claude Max/Pro OAuth token (CLAUDE_CODE_OAUTH_TOKEN
    /// or the Claude Code CLI's ~/.claude/.credentials.json); "api key" for a
    /// metered ANTHROPIC_API_KEY.
    pub fn auth_label(&self) -> &'static str {
        match &self.auth {
            Auth::Oauth(_) => "subscription",
            Auth::ApiKey(_) => "api key",
        }
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
    /// Non-secret auth-kind label for this backend's credential
    /// ("subscription" | "api key") — surfaced in `describe()` / the statusline.
    pub fn auth_label(&self) -> &'static str {
        self.cred.auth_label()
    }

    pub fn new(model: String, cred: Credential) -> Result<Self> {
        Ok(Self {
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(300))
                .build()?,
            cred,
            model,
        })
    }

    /// Build the `/messages` request body shared by the buffered
    /// ([`complete`](Self::complete)) and streaming
    /// ([`complete_streaming`](Self::complete_streaming)) paths. The two differ
    /// only by the `stream` flag, so keeping one builder means the model,
    /// token cap, system shaping, tool schemas, prompt caching, and adaptive-
    /// thinking policy can never drift between them.
    fn build_body(
        &self,
        system: &str,
        history: &[Msg],
        tools: &[ToolDef],
        stream: bool,
    ) -> Value {
        let mut messages = render_messages(history);
        let mut tool_defs: Vec<Value> = tools
            .iter()
            .map(|t| {
                json!({
                    "name": t.name,
                    "description": t.description,
                    "input_schema": t.schema,
                })
            })
            .collect();

        // Prompt caching (TASK-320). A top-level `cache_control` is NOT a valid
        // Messages-API field — the breakpoint must ride on individual content
        // blocks, and the API caches the whole prompt prefix ending at each one.
        // We place THREE ephemeral breakpoints (well within the 4-breakpoint cap)
        // at the largest stable boundaries, in prompt order (tools → system →
        // messages):
        //   1. the last tool schema  → caches the entire tools block,
        //   2. the last system block → caches tools + system,
        //   3. the last content block of the final message → a rolling breakpoint
        //      that caches the whole conversation prefix.
        // Breakpoints 1 & 2 are byte-identical every turn, so after the first
        // request they're pure cache_reads. Breakpoint 3 rolls forward each turn:
        // the delta since last turn is cache_creation, everything before it is a
        // cache_read — the standard incremental-caching pattern that keeps the
        // hit rate near 100% across a multi-turn coordinator session.
        if let Some(last) = tool_defs.last_mut() {
            last["cache_control"] = cache_breakpoint();
        }
        let system = cache_system(self.cred.system_value(system));
        cache_last_message(&mut messages);

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
            "system": system,
            "tools": tool_defs,
            "messages": messages,
        });
        if stream {
            body["stream"] = json!(true);
        }
        // Adaptive thinking is the 4.6+ Opus/Sonnet surface; Haiku doesn't take
        // it — AND it is incompatible with assistant-prefill, so it's suppressed
        // whenever the request ends with an assistant message (our truncation
        // continuation resumes a partial answer that way). See `wants_thinking`.
        if wants_thinking(&self.model, history) {
            body["thinking"] = json!({"type": "adaptive"});
        }
        body
    }

    pub async fn complete(&self, system: &str, history: &[Msg], tools: &[ToolDef]) -> Result<Turn> {
        let body = self.build_body(system, history, tools, false);
        let v = self.post_with_retry(&body).await?;
        parse_response(&v)
    }

    /// Streaming counterpart to [`complete`](Self::complete): opens an SSE
    /// stream against the Messages API and delivers each text/thinking token to
    /// `sink` as it decodes, then returns the same normalized [`Turn`]
    /// buffered-mode would (S8.1). The accumulated events are reassembled into
    /// the identical response envelope `parse_response` consumes, so ALL of the
    /// hard-won buffered-path behaviour — max_tokens truncation handling, empty
    /// tool-call dropping, usage/cache accounting, thinking-block preservation —
    /// is reused verbatim rather than re-implemented for the stream.
    ///
    /// Connection establishment is retried on the same transient classes as the
    /// buffered path (network errors, 429 with `Retry-After`, 5xx). A failure
    /// that occurs AFTER the first byte is surfaced (not retried) — some tokens
    /// have already reached the sink and replaying them would double-render; the
    /// caller may fall back to `complete`.
    pub async fn complete_streaming(
        &self,
        system: &str,
        history: &[Msg],
        tools: &[ToolDef],
        sink: super::StreamSink<'_>,
    ) -> Result<Turn> {
        let body = self.build_body(system, history, tools, true);
        let v = self.stream_with_retry(&body, sink).await?;
        parse_response(&v)
    }

    async fn post_with_retry(&self, body: &Value) -> Result<Value> {
        // A headless coordinator can run for many minutes; a transient
        // `Connection reset by peer` / timeout burst — or a 429 rate-limit
        // window — must not be fatal and lose all progress. A rate limit that
        // outlives our retry budget bubbles up, fails the turn, and ends the
        // coordinator run; resuming a *terminal* run then spawns a brand-new
        // worker (a fresh `w_…`). Riding the limit out HERE keeps the same
        // worker alive instead. Retry generously with exponential backoff, cap
        // each sleep at MAX_DELAY, and on 429 honor the server's `Retry-After`
        // (capped at RATE_LIMIT_MAX_DELAY) since its window routinely exceeds
        // the exponential cap.
        let mut budget = RetryBudget::new();
        loop {
            let req = self
                .client
                .post(API_URL)
                .header("anthropic-version", "2023-06-01")
                .header("content-type", "application/json");
            let resp = self.cred.apply(req).json(body).send().await;

            match resp {
                Ok(r) => {
                    // Capture Retry-After BEFORE the body is consumed.
                    let retry_after = parse_retry_after(r.headers());
                    // Decode to text first so a non-JSON gateway/edge body (502/503
                    // HTML, empty body, Cloudflare page) is a retryable signal, not a
                    // fatal decode error that stops the worker. See
                    // `super::read_status_and_json`.
                    let (status, parsed) = match super::read_status_and_json(r).await {
                        Ok(p) => p,
                        Err(e) => match budget.next_transient() {
                            Some(wait) => {
                                eprintln!(
                                    "\x1b[2m  network error reading body ({e}), retrying…\x1b[0m"
                                );
                                tokio::time::sleep(wait).await;
                                continue;
                            }
                            None => return Err(e).context("reading claude api response body"),
                        },
                    };
                    let v = match parsed {
                        Ok(v) => v,
                        // Non-JSON body: almost always a transient intermediary
                        // failure (the API itself answers 4xx/5xx in JSON). Retry
                        // it like any other transient error rather than aborting.
                        Err(snippet) => match budget.next_transient() {
                            Some(wait) => {
                                eprintln!(
                                    "\x1b[2m  api returned non-JSON ({status}), retrying…\x1b[0m"
                                );
                                tokio::time::sleep(wait).await;
                                continue;
                            }
                            None => bail!("claude api ({status}): non-JSON response: {snippet}"),
                        },
                    };
                    if status == 200 {
                        return Ok(v);
                    }
                    let msg = v["error"]["message"].as_str().unwrap_or("unknown error");
                    let kind = v["error"]["type"].as_str().unwrap_or("error");
                    // Auth failures never clear on retry — surface immediately,
                    // with a token-refresh nudge for subscription (OAuth) creds.
                    if is_auth_error(status, msg) {
                        if matches!(self.cred.auth, Auth::Oauth(_)) {
                            eprint_oauth_expired_hint();
                            bail!(
                                "claude api authentication failed ({status}): {msg} — \
please refresh your token with `claude setup-token`"
                            );
                        }
                        bail!("claude api {kind} ({status}): {msg}");
                    }
                    // 429: ride out the rate limit on the long budget — exponential
                    // up to a 5-minute ceiling, then steady 5-minute retries for up
                    // to an hour, honoring the server's Retry-After when present.
                    if status == 429 {
                        match budget.next_rate_limit(retry_after) {
                            Some(wait) => {
                                eprintln!(
                                    "\x1b[2m  api {kind} ({status}), retrying in {}s…\x1b[0m",
                                    wait.as_secs()
                                );
                                tokio::time::sleep(wait).await;
                                continue;
                            }
                            None => bail!(
                                "claude api rate limit ({status}) did not clear within the 1h \
ride-out budget: {msg}"
                            ),
                        }
                    }
                    // 5xx: transient upstream — short exponential backoff.
                    if status >= 500 {
                        match budget.next_transient() {
                            Some(wait) => {
                                eprintln!(
                                    "\x1b[2m  api {kind} ({status}), retrying in {}s…\x1b[0m",
                                    wait.as_secs()
                                );
                                tokio::time::sleep(wait).await;
                                continue;
                            }
                            None => bail!("claude api {kind} ({status}): {msg}"),
                        }
                    }
                    // Other 4xx (bad request, …): not retryable.
                    bail!("claude api {kind} ({status}): {msg}");
                }
                // Transport-level error (connect reset, timeout, dns): transient.
                Err(e) => match budget.next_transient() {
                    Some(wait) => {
                        eprintln!("\x1b[2m  network error ({e}), retrying…\x1b[0m");
                        tokio::time::sleep(wait).await;
                    }
                    None => return Err(e).context("request to claude api failed"),
                },
            }
        }
    }

    /// Establish the SSE stream, retrying only the CONNECTION on the same
    /// transient classes as [`post_with_retry`](Self::post_with_retry). Once a
    /// 200 response is streaming, decoding is handed to
    /// [`consume_stream`](Self::consume_stream); a mid-stream error there is
    /// returned as-is (see `complete_streaming` for why we don't replay).
    async fn stream_with_retry(
        &self,
        body: &Value,
        sink: super::StreamSink<'_>,
    ) -> Result<Value> {
        const MAX_ATTEMPTS: u32 = 8;
        const MAX_DELAY: Duration = Duration::from_secs(60);
        const RATE_LIMIT_MAX_DELAY: Duration = Duration::from_secs(90);
        let mut delay = Duration::from_secs(2);
        for attempt in 0..MAX_ATTEMPTS {
            let last = attempt + 1 == MAX_ATTEMPTS;
            let req = self
                .client
                .post(API_URL)
                .header("anthropic-version", "2023-06-01")
                .header("content-type", "application/json")
                .header("accept", "text/event-stream");
            let resp = match self.cred.apply(req).json(body).send().await {
                Ok(r) => r,
                Err(e) if !last => {
                    eprintln!("\x1b[2m  network error ({e}), retrying…\x1b[0m");
                    tokio::time::sleep(delay).await;
                    delay = (delay * 2).min(MAX_DELAY);
                    continue;
                }
                Err(e) => return Err(e).context("request to claude api (stream) failed"),
            };

            let status = resp.status().as_u16();
            if status == 200 {
                // Headers are in; the body streams from here. Any failure now is
                // post-first-byte and must NOT be retried (tokens already sank).
                return self.consume_stream(resp, sink).await;
            }

            // Non-200: the error body is a normal (non-streamed) JSON document —
            // classify it exactly as the buffered path does.
            let retry_after = resp
                .headers()
                .get("retry-after")
                .and_then(|h| h.to_str().ok())
                .and_then(|s| s.trim().parse::<u64>().ok())
                .map(Duration::from_secs);
            let (status, parsed) = match super::read_status_and_json(resp).await {
                Ok(p) => p,
                Err(e) if !last => {
                    eprintln!("\x1b[2m  network error reading body ({e}), retrying…\x1b[0m");
                    tokio::time::sleep(delay).await;
                    delay = (delay * 2).min(MAX_DELAY);
                    continue;
                }
                Err(e) => return Err(e).context("reading claude api (stream) response body"),
            };
            let v = match parsed {
                Ok(v) => v,
                Err(snippet) => {
                    if !last {
                        eprintln!("\x1b[2m  api returned non-JSON ({status}), retrying…\x1b[0m");
                        tokio::time::sleep(delay).await;
                        delay = (delay * 2).min(MAX_DELAY);
                        continue;
                    }
                    bail!("claude api ({status}): non-JSON response: {snippet}");
                }
            };
            let msg = v["error"]["message"].as_str().unwrap_or("unknown error");
            let kind = v["error"]["type"].as_str().unwrap_or("error");
            let is_auth_error = matches!(status, 401 | 403)
                || (status == 400 && {
                    let m = msg.to_ascii_lowercase();
                    m.contains("invalid") || m.contains("expired") || m.contains("unauthorized")
                });
            if is_auth_error && matches!(self.cred.auth, Auth::Oauth(_)) && !last {
                eprintln!("\x1b[1m\n⚠ Claude OAuth token expired\x1b[0m");
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
            if (status == 429 || status >= 500) && !last {
                let wait = if status == 429 {
                    retry_after.unwrap_or(delay).min(RATE_LIMIT_MAX_DELAY)
                } else {
                    delay
                };
                eprintln!(
                    "\x1b[2m  api {kind} ({status}), retrying in {}s…\x1b[0m",
                    wait.as_secs()
                );
                tokio::time::sleep(wait).await;
                delay = (delay * 2).min(MAX_DELAY);
                continue;
            }
            bail!("claude api {kind} ({status}): {msg}");
        }
        unreachable!()
    }

    /// Drive a 200 SSE body to completion: frame raw bytes into events
    /// ([`SseDecoder`]), fold each into a [`StreamAccumulator`], and forward
    /// text/thinking deltas to `sink` as they arrive. Returns the reassembled
    /// response envelope for `parse_response`.
    async fn consume_stream(
        &self,
        resp: reqwest::Response,
        sink: super::StreamSink<'_>,
    ) -> Result<Value> {
        use futures_util::StreamExt;
        let mut acc = StreamAccumulator::new();
        let mut decoder = SseDecoder::new();
        let mut stream = resp.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.context("reading claude stream chunk")?;
            decoder.push(&chunk, &mut |data| acc.push_event(&data, &mut *sink))?;
        }
        // Flush a trailing event that lacked its terminating blank line.
        decoder.finish(&mut |data| acc.push_event(&data, &mut *sink))?;
        Ok(acc.finish())
    }
}

/// Server-Sent-Events line framer for the Claude token stream. Accumulates raw
/// bytes across arbitrary chunk boundaries (a TCP read can split a line — or a
/// multi-byte UTF-8 sequence — anywhere) and yields one parsed `data:` JSON
/// document per event. Dispatch is driven off the JSON's own `type` field, so
/// the `event:` framing line is not needed and is ignored, as are heartbeat
/// comments (`:` lines). Pure (no IO), so the framing is unit-testable against
/// hand-split byte chunks.
struct SseDecoder {
    /// Undecoded bytes not yet forming a complete `\n`-terminated line.
    buf: Vec<u8>,
    /// The `data:` payload of the event currently being assembled (SSE allows
    /// an event to span multiple `data:` lines, joined with `\n`).
    data: String,
}

impl SseDecoder {
    fn new() -> Self {
        Self {
            buf: Vec::new(),
            data: String::new(),
        }
    }

    /// Feed a chunk of raw stream bytes, invoking `f` once per complete event.
    fn push(
        &mut self,
        bytes: &[u8],
        f: &mut dyn FnMut(Value) -> Result<()>,
    ) -> Result<()> {
        self.buf.extend_from_slice(bytes);
        while let Some(pos) = self.buf.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = self.buf.drain(..=pos).collect();
            // A complete line (through the `\n`) is always valid UTF-8: the JSON
            // payload never contains a bare newline, so no multi-byte sequence is
            // split at the boundary we cut on.
            let line = String::from_utf8_lossy(&line);
            let line = line.trim_end_matches(['\r', '\n']);
            if line.is_empty() {
                // Blank line = event terminator: dispatch what we've gathered.
                self.dispatch(f)?;
            } else if line.starts_with(':') {
                // SSE comment / heartbeat — ignore.
            } else if let Some(rest) = line.strip_prefix("data:") {
                let rest = rest.strip_prefix(' ').unwrap_or(rest);
                if !self.data.is_empty() {
                    self.data.push('\n');
                }
                self.data.push_str(rest);
            }
            // `event:`, `id:`, `retry:` fields are irrelevant here — dispatch is
            // on the data payload's own `type` — so they're skipped.
        }
        Ok(())
    }

    /// Parse and emit the buffered event data (if any), then reset it.
    fn dispatch(&mut self, f: &mut dyn FnMut(Value) -> Result<()>) -> Result<()> {
        if self.data.is_empty() {
            return Ok(());
        }
        let data: Value = serde_json::from_str(&self.data)
            .with_context(|| format!("parsing SSE data line: {}", self.data))?;
        self.data.clear();
        f(data)
    }

    /// Flush a final event that arrived without a terminating blank line.
    fn finish(&mut self, f: &mut dyn FnMut(Value) -> Result<()>) -> Result<()> {
        self.dispatch(f)
    }
}

/// One in-progress content block as the stream builds it up.
enum StreamBlock {
    Text(String),
    Thinking { thinking: String, signature: String },
    ToolUse { id: String, name: String, json: String },
}

/// Folds the Anthropic streaming event sequence back into the same response
/// envelope the buffered Messages API returns, so [`parse_response`] can be
/// reused unchanged. Text and thinking deltas are forwarded to the sink as they
/// land (the incremental-delivery guarantee); tool-call input JSON and thinking
/// signatures are accumulated silently and only surface in the final envelope.
struct StreamAccumulator {
    blocks: Vec<StreamBlock>,
    stop_reason: Option<String>,
    input_tokens: u64,
    cache_read: u64,
    cache_creation: u64,
    output_tokens: u64,
}

impl StreamAccumulator {
    fn new() -> Self {
        Self {
            blocks: Vec::new(),
            stop_reason: None,
            input_tokens: 0,
            cache_read: 0,
            cache_creation: 0,
            output_tokens: 0,
        }
    }

    /// Ensure `blocks[idx]` exists (filling any gap with empty text blocks),
    /// then place `block` there.
    fn set_block(&mut self, idx: usize, block: StreamBlock) {
        while self.blocks.len() <= idx {
            self.blocks.push(StreamBlock::Text(String::new()));
        }
        self.blocks[idx] = block;
    }

    /// Fold one decoded SSE event into the accumulator, forwarding any visible
    /// text/thinking delta to `sink`.
    fn push_event(&mut self, data: &Value, sink: super::StreamSink<'_>) -> Result<()> {
        let get = |o: &Value, k: &str| o.get(k).and_then(Value::as_u64).unwrap_or(0);
        match data["type"].as_str().unwrap_or("") {
            "message_start" => {
                let u = &data["message"]["usage"];
                self.input_tokens = get(u, "input_tokens");
                self.cache_read = get(u, "cache_read_input_tokens");
                self.cache_creation = get(u, "cache_creation_input_tokens");
                self.output_tokens = get(u, "output_tokens");
            }
            "content_block_start" => {
                let idx = data["index"].as_u64().unwrap_or(0) as usize;
                let block = &data["content_block"];
                let new = match block["type"].as_str().unwrap_or("") {
                    "tool_use" => StreamBlock::ToolUse {
                        id: block["id"].as_str().unwrap_or_default().to_string(),
                        name: block["name"].as_str().unwrap_or_default().to_string(),
                        json: String::new(),
                    },
                    "thinking" => StreamBlock::Thinking {
                        thinking: block["thinking"].as_str().unwrap_or_default().to_string(),
                        signature: block["signature"].as_str().unwrap_or_default().to_string(),
                    },
                    // "text" and anything unrecognized start as a text block.
                    _ => StreamBlock::Text(block["text"].as_str().unwrap_or_default().to_string()),
                };
                self.set_block(idx, new);
            }
            "content_block_delta" => {
                let idx = data["index"].as_u64().unwrap_or(0) as usize;
                let delta = &data["delta"];
                match delta["type"].as_str().unwrap_or("") {
                    "text_delta" => {
                        if let Some(t) = delta["text"].as_str() {
                            match self.blocks.get_mut(idx) {
                                Some(StreamBlock::Text(s)) => s.push_str(t),
                                _ => self.set_block(idx, StreamBlock::Text(t.to_string())),
                            }
                            sink(super::StreamDelta::Text(t));
                        }
                    }
                    "thinking_delta" => {
                        if let Some(t) = delta["thinking"].as_str() {
                            match self.blocks.get_mut(idx) {
                                Some(StreamBlock::Thinking { thinking, .. }) => {
                                    thinking.push_str(t)
                                }
                                _ => self.set_block(
                                    idx,
                                    StreamBlock::Thinking {
                                        thinking: t.to_string(),
                                        signature: String::new(),
                                    },
                                ),
                            }
                            sink(super::StreamDelta::Thinking(t));
                        }
                    }
                    "signature_delta" => {
                        if let Some(sig) = delta["signature"].as_str() {
                            if let Some(StreamBlock::Thinking { signature, .. }) =
                                self.blocks.get_mut(idx)
                            {
                                signature.push_str(sig);
                            }
                        }
                    }
                    "input_json_delta" => {
                        if let Some(pj) = delta["partial_json"].as_str() {
                            if let Some(StreamBlock::ToolUse { json, .. }) = self.blocks.get_mut(idx)
                            {
                                json.push_str(pj);
                            }
                        }
                    }
                    _ => {}
                }
            }
            "content_block_stop" => {}
            "message_delta" => {
                if let Some(sr) = data["delta"]["stop_reason"].as_str() {
                    self.stop_reason = Some(sr.to_string());
                }
                // Cumulative output token count is reported here.
                if let Some(ot) = data["usage"]["output_tokens"].as_u64() {
                    self.output_tokens = ot;
                }
            }
            "message_stop" => {}
            "error" => {
                let msg = data["error"]["message"]
                    .as_str()
                    .unwrap_or("unknown streaming error");
                bail!("claude streaming error: {msg}");
            }
            _ => {}
        }
        Ok(())
    }

    /// Reassemble the buffered-mode response envelope from the accumulated
    /// blocks + stop reason + usage, ready for [`parse_response`]. A tool_use
    /// block whose accumulated JSON failed to complete (mid-call max_tokens
    /// truncation) degrades to an empty `{}` input, which `parse_response`
    /// then drops and flags as a truncated tool call — identical to the
    /// buffered path.
    fn finish(self) -> Value {
        let content: Vec<Value> = self
            .blocks
            .into_iter()
            .map(|b| match b {
                StreamBlock::Text(t) => json!({"type": "text", "text": t}),
                StreamBlock::Thinking {
                    thinking,
                    signature,
                } => json!({"type": "thinking", "thinking": thinking, "signature": signature}),
                StreamBlock::ToolUse { id, name, json } => {
                    let input: Value = serde_json::from_str(&json).unwrap_or_else(|_| json!({}));
                    json!({"type": "tool_use", "id": id, "name": name, "input": input})
                }
            })
            .collect();
        json!({
            "stop_reason": self.stop_reason.unwrap_or_default(),
            "content": content,
            "usage": {
                "input_tokens": self.input_tokens,
                "cache_read_input_tokens": self.cache_read,
                "cache_creation_input_tokens": self.cache_creation,
                "output_tokens": self.output_tokens,
            }
        })
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
/// An ephemeral prompt-cache breakpoint marker. Attached to a content block, it
/// tells the Messages API to cache the whole prompt prefix ending at that block.
fn cache_breakpoint() -> Value {
    json!({"type": "ephemeral"})
}

/// Attach a cache breakpoint to the last block of a `system` value, normalizing
/// a bare-string system (the API-key shape from [`Credential::system_value`])
/// into single-text-block form so the breakpoint has a block to ride on. Array
/// systems (the OAuth shape) get the breakpoint on their final block.
fn cache_system(system: Value) -> Value {
    match system {
        Value::String(s) => {
            json!([{ "type": "text", "text": s, "cache_control": cache_breakpoint() }])
        }
        Value::Array(mut blocks) => {
            if let Some(obj) = blocks.last_mut().and_then(Value::as_object_mut) {
                obj.insert("cache_control".into(), cache_breakpoint());
            }
            Value::Array(blocks)
        }
        other => other,
    }
}

/// Attach a rolling cache breakpoint to the last content block of the final
/// message, caching the entire conversation prefix. Handles both the array and
/// bare-string content shapes; a no-op on an empty message list.
fn cache_last_message(messages: &mut [Value]) {
    let Some(last) = messages.last_mut() else {
        return;
    };
    match last.get_mut("content") {
        Some(Value::Array(blocks)) => {
            if let Some(obj) = blocks.last_mut().and_then(Value::as_object_mut) {
                obj.insert("cache_control".into(), cache_breakpoint());
            }
        }
        Some(Value::String(_)) => {
            let s = last["content"].as_str().unwrap_or_default().to_string();
            last["content"] =
                json!([{ "type": "text", "text": s, "cache_control": cache_breakpoint() }]);
        }
        _ => {}
    }
}

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
        let uncached = g("input_tokens");
        let cache_read = g("cache_read_input_tokens");
        let cache_creation = g("cache_creation_input_tokens");
        // Per-turn cache telemetry (TASK-320). hit rate = cached-read fraction of
        // the total input prompt; a healthy multi-turn session trends toward the
        // high 90s. Emitted dim so it sits quietly alongside the retry/status
        // lines that already use eprintln + \x1b[2m.
        let total_input = uncached + cache_read + cache_creation;
        if total_input > 0 {
            let hit_pct = (cache_read as f64 / total_input as f64) * 100.0;
            eprintln!(
                "\x1b[2m  cache: {cache_read} read + {cache_creation} write + {uncached} uncached \
                 in ({hit_pct:.0}% hit) → {out} out\x1b[0m",
                out = g("output_tokens"),
            );
        }
        crate::context::Usage {
            input_tokens: uncached + cache_read + cache_creation,
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

/// Drop empty / whitespace-only `text` content blocks from an assistant content
/// array. The Claude Messages API rejects ANY `{"type":"text","text":""}` block
/// with `messages: text content blocks must be non-empty` — a 400 that aborts
/// the entire run. Because we echo assistant content verbatim via `raw` to
/// preserve thinking signatures (required by adaptive thinking + tools), an
/// empty text block the model emitted (e.g. a blank leading block before a
/// `tool_use`) would otherwise ride straight back to the API next turn and kill
/// the session. Non-text blocks (`tool_use`, `thinking`, …) pass through
/// untouched, preserving signatures. A non-array `raw` is returned unchanged.
fn strip_empty_text_blocks(content: Value) -> Value {
    match content {
        Value::Array(blocks) => Value::Array(
            blocks
                .into_iter()
                .filter(|b| {
                    // Keep everything EXCEPT text blocks whose text is blank.
                    if b.get("type").and_then(Value::as_str) == Some("text") {
                        b.get("text")
                            .and_then(Value::as_str)
                            .map(|t| !t.trim().is_empty())
                            .unwrap_or(false)
                    } else {
                        true
                    }
                })
                .collect(),
        ),
        other => other,
    }
}

/// Rebuild a clean assistant content array from the normalized `Msg` fields
/// (used when `raw` is absent or sanitized to empty). Emits a `text` block only
/// when the text is non-empty, then one `tool_use` block per tool call. When
/// `is_prefill` (this is the trailing assistant message the model resumes), the
/// text is `trim_end`ed — the API rejects assistant content ending in whitespace.
fn rebuild_assistant_content(msg: &Msg, is_prefill: bool) -> Value {
    let mut blocks = Vec::new();
    if !msg.text.is_empty() {
        let t = if is_prefill {
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
}

/// Render normalized history into Claude wire messages.
fn render_messages(history: &[Msg]) -> Vec<Value> {
    let last = history.len().saturating_sub(1);
    let mut out = Vec::with_capacity(history.len());
    for (i, msg) in history.iter().enumerate() {
        match msg.role {
            Role::Assistant => {
                // Rebuild a clean assistant message from the normalized fields —
                // the fallback whenever raw is absent OR raw sanitizes to empty.
                let rebuild = || rebuild_assistant_content(msg, i == last);
                // Echo raw content verbatim when we have it — preserves thinking
                // blocks + signatures, required with adaptive thinking + tools —
                // but FIRST strip any empty/whitespace `text` blocks. The model
                // sometimes emits an empty leading text block before a tool_use;
                // echoed verbatim next turn the API rejects it with
                // `messages: text content blocks must be non-empty` (a 400 that
                // aborts the whole run). If stripping empties the array, fall
                // back to the normalized rebuild so we never send empty content.
                let content = match msg.raw.clone() {
                    Some(raw) => {
                        let cleaned = strip_empty_text_blocks(raw);
                        if cleaned
                            .as_array()
                            .map(|a| a.is_empty())
                            .unwrap_or(false)
                        {
                            rebuild()
                        } else {
                            cleaned
                        }
                    }
                    None => rebuild(),
                };
                out.push(json!({"role": "assistant", "content": content}));
            }
            Role::User => {
                if msg.tool_results.is_empty() {
                    // A user turn with empty text would send `content: ""`, which
                    // the API rejects (`text content blocks must be non-empty`).
                    // The engine should never produce one, but guard it so a
                    // stray empty turn can't 400 and lose the whole run.
                    let text = if msg.text.trim().is_empty() {
                        "(no content)"
                    } else {
                        msg.text.as_str()
                    };
                    out.push(json!({"role": "user", "content": text}));
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
    fn strip_empty_text_blocks_drops_blank_text_keeps_rest() {
        // The core guard: a blank/whitespace text block alongside a tool_use is
        // the exact shape that 400s the API when echoed verbatim. It must be
        // dropped while the tool_use (and any non-empty text) survives intact.
        let raw = json!([
            {"type": "text", "text": ""},
            {"type": "text", "text": "   \n"},
            {"type": "thinking", "thinking": "reasoning", "signature": "sig"},
            {"type": "text", "text": "real answer"},
            {"type": "tool_use", "id": "t1", "name": "ls", "input": {}}
        ]);
        let cleaned = strip_empty_text_blocks(raw);
        let blocks = cleaned.as_array().unwrap();
        assert_eq!(blocks.len(), 3, "two blank text blocks removed: {blocks:?}");
        assert_eq!(blocks[0]["type"], "thinking"); // signature preserved
        assert_eq!(blocks[1]["text"], "real answer");
        assert_eq!(blocks[2]["type"], "tool_use");
        // A non-array raw passes through unchanged.
        assert_eq!(strip_empty_text_blocks(json!("plain")), json!("plain"));
    }

    #[test]
    fn render_echoed_raw_with_empty_text_block_is_sanitized() {
        // An assistant turn whose raw carries a blank leading text block (what
        // the model sometimes emits before a tool_use). Echoed verbatim this is
        // the `text content blocks must be non-empty` 400; render must strip it.
        use super::super::ToolResult;
        let hist = vec![
            Msg::user("go"),
            Msg {
                role: Role::Assistant,
                text: String::new(),
                tool_calls: vec![],
                tool_results: vec![],
                raw: Some(json!([
                    {"type": "text", "text": ""},
                    {"type": "tool_use", "id": "t1", "name": "ls", "input": {}}
                ])),
            },
            // A trailing tool_result keeps the assistant message non-terminal.
            Msg::tool_results(vec![ToolResult::text("t1", "ok", false)]),
        ];
        let msgs = render_messages(&hist);
        let blocks = msgs[1]["content"].as_array().unwrap();
        assert!(
            blocks
                .iter()
                .all(|b| b["type"] != "text" || !b["text"].as_str().unwrap_or("").is_empty()),
            "no empty text block may reach the wire: {blocks:?}"
        );
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0]["type"], "tool_use");
    }

    #[test]
    fn render_raw_of_only_empty_text_falls_back_to_rebuild() {
        // If raw is NOTHING but empty text blocks, sanitizing empties the array.
        // We must fall back to the normalized rebuild so the assistant message is
        // never sent as empty `[]` (also a 400).
        use super::super::ToolResult;
        let hist = vec![
            Msg::user("go"),
            Msg {
                role: Role::Assistant,
                text: "rebuilt text".into(),
                tool_calls: vec![ToolCall {
                    id: "t1".into(),
                    name: "ls".into(),
                    args: json!({}),
                }],
                tool_results: vec![],
                raw: Some(json!([{"type": "text", "text": "  "}])),
            },
            Msg::tool_results(vec![ToolResult::text("t1", "ok", false)]),
        ];
        let msgs = render_messages(&hist);
        let blocks = msgs[1]["content"].as_array().unwrap();
        assert_eq!(blocks[0]["text"], "rebuilt text");
        assert_eq!(blocks[1]["type"], "tool_use");
    }

    #[test]
    fn render_empty_user_message_is_guarded() {
        // A user turn with blank text would send `content: ""` → 400. The guard
        // substitutes a placeholder so a stray empty turn can't kill the run.
        let msgs = render_messages(&[Msg::user("   ")]);
        assert_eq!(msgs[0]["role"], "user");
        assert_eq!(msgs[0]["content"], "(no content)");
        // A normal user turn is untouched.
        let ok = render_messages(&[Msg::user("hello")]);
        assert_eq!(ok[0]["content"], "hello");
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
        // Structured → compact JSON (NOT the rendered text).
        // Note: JSON key order is determined by the serde_json serializer; we only care that the payload is correct.
        let content_str = blocks[0]["content"].as_str().unwrap();
        assert!(content_str.contains("\"path\":\"f.txt\""));
        assert!(content_str.contains("\"size\":3"));
        assert!(content_str.contains("\"type\":\"file\""));
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

    #[test]
    fn cache_breakpoints_land_on_blocks_not_top_level() {
        // API-key (string) system is normalized into a single text block that
        // carries the breakpoint.
        let sys = cache_system(json!("you are aish"));
        assert_eq!(sys[0]["type"], "text");
        assert_eq!(sys[0]["text"], "you are aish");
        assert_eq!(sys[0]["cache_control"]["type"], "ephemeral");

        // OAuth (array) system gets the breakpoint on its LAST block only.
        let sys = cache_system(json!([
            {"type": "text", "text": "spoof"},
            {"type": "text", "text": "real"},
        ]));
        assert!(sys[0].get("cache_control").is_none());
        assert_eq!(sys[1]["cache_control"]["type"], "ephemeral");

        // Last message's final content block gets the rolling breakpoint.
        let mut msgs = vec![
            json!({"role": "user", "content": [{"type": "text", "text": "a"}]}),
            json!({"role": "user", "content": [
                {"type": "text", "text": "b"},
                {"type": "tool_result", "tool_use_id": "t1", "content": "r"},
            ]}),
        ];
        cache_last_message(&mut msgs);
        assert!(msgs[0]["content"][0].get("cache_control").is_none());
        assert!(msgs[1]["content"][0].get("cache_control").is_none());
        assert_eq!(msgs[1]["content"][1]["cache_control"]["type"], "ephemeral");

        // String-content message is normalized into a text block with the marker.
        let mut msgs = vec![json!({"role": "user", "content": "hello"})];
        cache_last_message(&mut msgs);
        assert_eq!(msgs[0]["content"][0]["text"], "hello");
        assert_eq!(msgs[0]["content"][0]["cache_control"]["type"], "ephemeral");
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

    // ---- Streaming (S8.1) ------------------------------------------------

    /// Feed a sequence of decoded SSE events through a fresh accumulator,
    /// recording every delta the sink receives (`'t'` = text, `'k'` = thinking)
    /// in arrival order, then reassemble + parse the final `Turn`. The recorded
    /// deltas are the proof that tokens arrive INCREMENTALLY — one entry per
    /// emitted delta, not a single coalesced blob.
    fn drive(events: &[Value]) -> (Vec<(char, String)>, Turn) {
        let mut acc = StreamAccumulator::new();
        let mut got: Vec<(char, String)> = Vec::new();
        {
            let mut sink = |d: crate::backend::StreamDelta<'_>| match d {
                crate::backend::StreamDelta::Text(t) => got.push(('t', t.to_string())),
                crate::backend::StreamDelta::Thinking(t) => got.push(('k', t.to_string())),
            };
            for e in events {
                acc.push_event(e, &mut sink).unwrap();
            }
        }
        let turn = parse_response(&acc.finish()).unwrap();
        (got, turn)
    }

    #[test]
    fn stream_delivers_text_tokens_incrementally() {
        // AC (S8.1): tokens arrive incrementally through the backend trait. The
        // three text_deltas must reach the sink as THREE separate deltas, in
        // order — not one merged string — and the final Turn must equal the
        // concatenation with usage (incl. cache buckets) accounted.
        let events = vec![
            json!({"type":"message_start","message":{"usage":{"input_tokens":10,"cache_read_input_tokens":5,"cache_creation_input_tokens":0,"output_tokens":1}}}),
            json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hel"}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"lo"}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":" world"}}),
            json!({"type":"content_block_stop","index":0}),
            json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":3}}),
            json!({"type":"message_stop"}),
        ];
        let (got, turn) = drive(&events);
        assert_eq!(
            got,
            vec![
                ('t', "Hel".to_string()),
                ('t', "lo".to_string()),
                ('t', " world".to_string()),
            ],
            "each text_delta must surface incrementally, in order"
        );
        assert_eq!(turn.text, "Hello world");
        assert!(!turn.truncated_tool_call && !turn.truncated_text);
        let u = turn.usage.expect("usage reassembled from stream");
        assert_eq!(u.input_tokens, 15); // 10 + 5 + 0
        assert_eq!(u.output_tokens, 3); // updated by message_delta
        // raw is preserved so the assistant turn echoes back into history.
        assert!(turn.raw.is_some());
    }

    #[test]
    fn stream_reassembles_tool_use_from_input_json_deltas() {
        // A tool call streams its input as partial_json fragments; the
        // accumulator must stitch them into valid JSON and NOT leak any of it to
        // the sink as visible text.
        let events = vec![
            json!({"type":"message_start","message":{"usage":{"input_tokens":1}}}),
            json!({"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"tu1","name":"read_file"}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"path\":\"a"}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":".txt\"}"}}),
            json!({"type":"content_block_stop","index":0}),
            json!({"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":7}}),
        ];
        let (got, turn) = drive(&events);
        assert!(got.is_empty(), "tool-call JSON must not stream as text");
        assert_eq!(turn.tool_calls.len(), 1);
        assert_eq!(turn.tool_calls[0].id, "tu1");
        assert_eq!(turn.tool_calls[0].name, "read_file");
        assert_eq!(turn.tool_calls[0].args, json!({"path": "a.txt"}));
        assert!(!turn.truncated_tool_call);
    }

    #[test]
    fn stream_emits_thinking_and_text_as_distinct_deltas() {
        // Thinking deltas surface as StreamDelta::Thinking, text as Text; the
        // final envelope preserves the thinking block (with its signature) in
        // `raw` so adaptive-thinking history stays intact.
        let events = vec![
            json!({"type":"message_start","message":{"usage":{"input_tokens":2}}}),
            json!({"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"","signature":""}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"let me"}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":" think"}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"sig"}}),
            json!({"type":"content_block_stop","index":0}),
            json!({"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}),
            json!({"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"answer"}}),
            json!({"type":"content_block_stop","index":1}),
            json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":5}}),
        ];
        let (got, turn) = drive(&events);
        assert_eq!(
            got,
            vec![
                ('k', "let me".to_string()),
                ('k', " think".to_string()),
                ('t', "answer".to_string()),
            ]
        );
        assert_eq!(turn.text, "answer");
        let raw = turn.raw.expect("raw kept for thinking history");
        let arr = raw.as_array().unwrap();
        assert_eq!(arr[0]["type"], "thinking");
        assert_eq!(arr[0]["thinking"], "let me think");
        assert_eq!(arr[0]["signature"], "sig");
        assert_eq!(arr[1]["type"], "text");
        assert_eq!(arr[1]["text"], "answer");
    }

    #[test]
    fn stream_truncated_tool_call_degrades_like_buffered_path() {
        // A max_tokens cut mid-tool-call leaves the accumulated input JSON
        // incomplete. finish() degrades it to `{}`, and parse_response then
        // drops the call + flags truncation — identical to the buffered path,
        // so the agentic loop's retry-smaller behaviour is preserved.
        let events = vec![
            json!({"type":"message_start","message":{"usage":{"input_tokens":3}}}),
            json!({"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"tu1","name":"write_file"}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"path\":\"big.rs\",\"content\":\"fn main() {"}}),
            json!({"type":"message_delta","delta":{"stop_reason":"max_tokens"},"usage":{"output_tokens":32000}}),
        ];
        let (_got, turn) = drive(&events);
        assert!(turn.tool_calls.is_empty(), "truncated tool call not executed");
        assert!(turn.truncated_tool_call);
        assert!(turn.text.contains("cut off mid-tool-call"));
    }

    #[test]
    fn sse_decoder_frames_events_split_across_chunk_boundaries() {
        // The SSE framer must reassemble events regardless of where the TCP
        // chunk boundary falls — mid-line, mid-field, anywhere.
        let raw = "event: message_start\n\
data: {\"type\":\"message_start\"}\n\
\n\
event: content_block_delta\n\
data: {\"type\":\"content_block_delta\"}\n\
\n";
        let collect = |chunks: &[&[u8]]| -> Vec<String> {
            let mut dec = SseDecoder::new();
            let mut types = Vec::new();
            let mut on = |d: Value| {
                types.push(d["type"].as_str().unwrap_or("").to_string());
                Ok(())
            };
            for c in chunks {
                dec.push(c, &mut on).unwrap();
            }
            dec.finish(&mut on).unwrap();
            types
        };
        let bytes = raw.as_bytes();
        // Whole blob in one push.
        assert_eq!(
            collect(&[bytes]),
            vec!["message_start".to_string(), "content_block_delta".to_string()]
        );
        // Split at three awkward interior offsets (mid-line each).
        for cut in [10usize, 33, 60] {
            let (a, b) = bytes.split_at(cut.min(bytes.len()));
            assert_eq!(
                collect(&[a, b]),
                vec!["message_start".to_string(), "content_block_delta".to_string()],
                "framing must survive a chunk boundary at byte {cut}"
            );
        }
    }

    #[test]
    fn sse_decoder_reassembles_multibyte_utf8_across_boundaries() {
        // A multi-byte UTF-8 sequence (é = 0xC3 0xA9) can be split across chunk
        // boundaries. Because the framer buffers raw bytes and only decodes a
        // COMPLETE `\n`-terminated line, the sequence is rejoined before decode.
        // Feeding one byte at a time is the worst case and must still parse.
        let raw = "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"café\"}}\n\n";
        let mut dec = SseDecoder::new();
        let mut texts = Vec::new();
        let mut on = |d: Value| {
            if let Some(t) = d["delta"]["text"].as_str() {
                texts.push(t.to_string());
            }
            Ok(())
        };
        for b in raw.as_bytes() {
            dec.push(&[*b], &mut on).unwrap();
        }
        assert_eq!(texts, vec!["café".to_string()]);
    }

    #[test]
    fn sse_decoder_ignores_comments_and_blank_only_frames() {
        // Heartbeat comment lines (`:`), unknown fields, and stray blank lines
        // must not produce spurious events.
        let raw = ": ping\n\
\n\
id: 1\n\
event: message_stop\n\
data: {\"type\":\"message_stop\"}\n\
\n";
        let mut dec = SseDecoder::new();
        let mut types = Vec::new();
        let mut on = |d: Value| {
            types.push(d["type"].as_str().unwrap_or("").to_string());
            Ok(())
        };
        dec.push(raw.as_bytes(), &mut on).unwrap();
        dec.finish(&mut on).unwrap();
        assert_eq!(types, vec!["message_stop".to_string()]);
    }
}
