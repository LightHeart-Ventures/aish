use super::{Msg, Role, ToolCall, ToolDef, Turn};
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

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

/// Where the Grok backend gets its bearer token. A SuperGrok/X Premium
/// subscription (via the Grok CLI's `~/.grok/auth.json`) is preferred over a
/// metered API key — mirroring the Claude Max precedence (subscription first).
enum GrokAuth {
    /// A metered `XAI_API_KEY` (env or ~/.aishrc) — sent verbatim as the bearer.
    ApiKey(String),
    /// The Grok CLI's OAuth token store (`~/.grok/auth.json`), holding a
    /// subscription token. Read FRESH on each request so a token the Grok CLI has
    /// refreshed is picked up without restarting aish (the CLI owns login +
    /// refresh; aish only reads).
    OAuthFile(PathBuf),
}

impl GrokAuth {
    /// Prefer the subscription OAuth token (`~/.grok/auth.json`) when present,
    /// else a metered `XAI_API_KEY` from the rc exports or process env.
    fn resolve(extra: &[(String, String)]) -> Result<Self> {
        if let Some(p) = grok_auth_path() {
            if p.exists() {
                return Ok(GrokAuth::OAuthFile(p));
            }
        }
        if let Some(key) = crate::rc::env_value(extra, "XAI_API_KEY") {
            return Ok(GrokAuth::ApiKey(key));
        }
        bail!(
            "no Grok credential — log in with the Grok CLI (creates ~/.grok/auth.json) \
or set XAI_API_KEY in your environment or ~/.aishrc"
        )
    }

    /// The current bearer token. For the OAuth file this reads + parses on every
    /// request, so a token the Grok CLI refreshed is used without a restart.
    fn access_token(&self) -> Result<String> {
        match self {
            GrokAuth::ApiKey(k) => Ok(k.clone()),
            GrokAuth::OAuthFile(p) => Ok(GrokOAuthStore::load(p)?.access_token),
        }
    }

    /// Exchange the stored refresh token for a fresh access token and persist the
    /// result (the refresh token ROTATES on each use, so this writeback is
    /// mandatory). Returns the new access token. Only the OAuth-file credential
    /// can refresh — an API key is used verbatim.
    async fn refresh(&self, client: &reqwest::Client) -> Result<String> {
        match self {
            GrokAuth::ApiKey(_) => bail!("an XAI_API_KEY can't be refreshed"),
            GrokAuth::OAuthFile(p) => GrokOAuthStore::load(p)?.refresh(client).await,
        }
    }

    /// A non-secret label for `describe()` — never the token itself.
    fn label(&self) -> &'static str {
        match self {
            GrokAuth::ApiKey(_) => "api key",
            GrokAuth::OAuthFile(_) => "subscription",
        }
    }
}

/// The Grok CLI's `~/.grok/auth.json` parsed into the fields aish needs: the
/// current access token plus everything required to refresh it. The file maps
/// one `"<issuer>::<client_id>"` key to a login entry; we use the first.
struct GrokOAuthStore {
    path: PathBuf,
    /// The top-level `"<issuer>::<client_id>"` key of the active login entry.
    entry_key: String,
    access_token: String,
    refresh_token: String,
    client_id: String,
    /// `<issuer>/oauth2/token` — the OIDC token endpoint (public PKCE client, so
    /// refresh needs only refresh_token + client_id, no secret).
    token_endpoint: String,
}

impl GrokOAuthStore {
    fn load(path: &Path) -> Result<Self> {
        let contents = std::fs::read_to_string(path)
            .with_context(|| format!("reading Grok token store {}", path.display()))?;
        Self::parse(path, &contents)
    }

    /// Pure parse (no IO) for unit-testability.
    fn parse(path: &Path, contents: &str) -> Result<Self> {
        let v: Value =
            serde_json::from_str(contents).context("~/.grok/auth.json is not valid JSON")?;
        let obj = v
            .as_object()
            .context("~/.grok/auth.json: expected a JSON object of logins")?;
        let (entry_key, entry) = obj
            .iter()
            .next()
            .context("~/.grok/auth.json has no login — run the Grok CLI login")?;
        let access_token = entry["key"]
            .as_str()
            .filter(|s| !s.is_empty())
            .context("~/.grok/auth.json login has no `key` token — re-run the Grok CLI login")?
            .to_string();
        let refresh_token = entry["refresh_token"].as_str().unwrap_or("").to_string();
        let client_id = entry["oidc_client_id"].as_str().unwrap_or("").to_string();
        let issuer = entry["oidc_issuer"].as_str().unwrap_or("https://auth.x.ai");
        let token_endpoint = format!("{}/oauth2/token", issuer.trim_end_matches('/'));
        Ok(Self {
            path: path.to_path_buf(),
            entry_key: entry_key.clone(),
            access_token,
            refresh_token,
            client_id,
            token_endpoint,
        })
    }

    async fn refresh(&self, client: &reqwest::Client) -> Result<String> {
        if self.refresh_token.is_empty() || self.client_id.is_empty() {
            bail!(
                "~/.grok/auth.json has no refresh_token/client_id to refresh with — \
re-run the Grok CLI login"
            );
        }
        let form = format!(
            "grant_type=refresh_token&refresh_token={}&client_id={}",
            form_encode(&self.refresh_token),
            form_encode(&self.client_id),
        );
        let resp = client
            .post(&self.token_endpoint)
            .header("content-type", "application/x-www-form-urlencoded")
            .body(form)
            .send()
            .await
            .context("grok token refresh request failed")?;
        let status = resp.status().as_u16();
        let v: Value = resp
            .json()
            .await
            .context("grok token endpoint returned non-JSON")?;
        if status != 200 {
            let msg = v["error_description"]
                .as_str()
                .or_else(|| v["error"].as_str())
                .unwrap_or("unknown error");
            bail!("grok token refresh failed ({status}): {msg} — re-run the Grok CLI login");
        }
        let access = v["access_token"]
            .as_str()
            .filter(|s| !s.is_empty())
            .context("grok token refresh response had no access_token")?
            .to_string();
        // Refresh tokens rotate: if a new one came back we MUST persist it, or the
        // old one (now invalidated server-side) breaks the next refresh.
        let new_refresh = v["refresh_token"].as_str().filter(|s| !s.is_empty());
        let expires_at = v["expires_in"]
            .as_u64()
            .map(|secs| unix_to_rfc3339(now_unix().saturating_add(secs)));
        self.write_back(&access, new_refresh, expires_at.as_deref())?;
        Ok(access)
    }

    /// Persist refreshed credentials back into auth.json, preserving every other
    /// field, atomically (temp + rename) and 0600 so a partial/loose write can't
    /// corrupt or expose the token store.
    fn write_back(
        &self,
        access: &str,
        refresh: Option<&str>,
        expires_at: Option<&str>,
    ) -> Result<()> {
        let contents = std::fs::read_to_string(&self.path)
            .with_context(|| format!("re-reading {} for write-back", self.path.display()))?;
        let mut v: Value =
            serde_json::from_str(&contents).context("auth.json became invalid JSON")?;
        apply_refresh_to_entry(&mut v, &self.entry_key, access, refresh, expires_at)?;
        let pretty = serde_json::to_string_pretty(&v).context("serializing refreshed auth.json")?;
        atomic_write_0600(&self.path, &pretty)
    }
}

/// Merge refreshed credentials into the parsed auth.json `Value` in place,
/// touching only the active entry's `key`/`refresh_token`/`expires_at`. Pure so
/// the merge (and its field-preservation) is unit-testable.
fn apply_refresh_to_entry(
    v: &mut Value,
    entry_key: &str,
    access: &str,
    refresh: Option<&str>,
    expires_at: Option<&str>,
) -> Result<()> {
    let entry = v
        .get_mut(entry_key)
        .and_then(|e| e.as_object_mut())
        .context("auth.json login entry vanished before write-back")?;
    entry.insert("key".into(), json!(access));
    if let Some(rt) = refresh {
        entry.insert("refresh_token".into(), json!(rt));
    }
    if let Some(ea) = expires_at {
        entry.insert("expires_at".into(), json!(ea));
    }
    Ok(())
}

/// Percent-encode a value for an `application/x-www-form-urlencoded` body,
/// leaving the RFC 3986 unreserved set untouched. Used to build the token-refresh
/// request without depending on reqwest's optional form support.
fn form_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Seconds since the Unix epoch (0 if the clock is before it — never panics).
fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Format Unix seconds as a UTC RFC 3339 timestamp (`YYYY-MM-DDTHH:MM:SSZ`), so
/// we can rewrite `expires_at` without pulling in a date crate. Uses Hinnant's
/// days-from-civil algorithm.
fn unix_to_rfc3339(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let tod = secs % 86_400;
    let (h, mi, s) = (tod / 3600, (tod % 3600) / 60, tod % 60);
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + if m <= 2 { 1 } else { 0 };
    format!("{y:04}-{m:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z")
}

/// Write `contents` to `path` atomically: a temp file in the same directory,
/// chmod 0600, then rename over the target (rename is atomic on the same fs).
fn atomic_write_0600(path: &Path, contents: &str) -> Result<()> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("auth.json");
    let tmp = dir.join(format!(".{file_name}.{}.tmp", std::process::id()));
    std::fs::write(&tmp, contents)
        .with_context(|| format!("writing temp token file {}", tmp.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
    }
    std::fs::rename(&tmp, path).with_context(|| {
        let _ = std::fs::remove_file(&tmp);
        format!("replacing {}", path.display())
    })
}

/// `~/.grok/auth.json` — the Grok CLI's token store.
fn grok_auth_path() -> Option<PathBuf> {
    let home = std::env::var("HOME").ok()?;
    Some(PathBuf::from(home).join(".grok").join("auth.json"))
}

/// Extract just the access token from the Grok CLI's `auth.json` (the first
/// login entry's `key`). Thin wrapper over the full store parser.
#[cfg(test)]
fn token_from_auth_json(contents: &str) -> Result<String> {
    GrokOAuthStore::parse(Path::new("(memory)"), contents).map(|s| s.access_token)
}

/// True when SOME Grok credential is resolvable (OAuth file or `XAI_API_KEY`).
/// Used by the background-dispatch guards when the active backend is Grok.
pub fn credential_available(extra: &[(String, String)]) -> bool {
    GrokAuth::resolve(extra).is_ok()
}

pub struct GrokBackend {
    client: reqwest::Client,
    auth: GrokAuth,
    pub model: String,
}

impl GrokBackend {
    /// Resolve a credential (subscription OAuth file, else `XAI_API_KEY` from
    /// `extra_env`/process env) and build the backend.
    pub fn new(model: String, extra_env: &[(String, String)]) -> Result<Self> {
        Ok(Self {
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(300))
                .build()?,
            auth: GrokAuth::resolve(extra_env)?,
            model,
        })
    }

    /// Non-secret credential label for `describe()` ("subscription" / "api key").
    pub fn auth_label(&self) -> &'static str {
        self.auth.label()
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
        // Resolve the bearer once per request (fresh-reads the OAuth file, so a
        // CLI-refreshed token is picked up); it won't change across the ~seconds
        // of retry backoff unless WE refresh it on a 401 below.
        let mut bearer = self.auth.access_token()?;
        let mut refreshed = false;
        let mut delay = Duration::from_secs(2);
        for attempt in 0..MAX_ATTEMPTS {
            let last = attempt + 1 == MAX_ATTEMPTS;
            let resp = self
                .client
                .post(API_URL)
                .header("authorization", format!("Bearer {bearer}"))
                .header("content-type", "application/json")
                .json(body)
                .send()
                .await;

            match resp {
                Ok(r) => {
                    // Decode to text first so a non-JSON gateway/edge body (502/503
                    // HTML, empty body, challenge page) is a retryable signal, not a
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
                        Err(e) => return Err(e).context("reading grok api response body"),
                    };
                    let v = match parsed {
                        Ok(v) => v,
                        Err(snippet) => {
                            if !last {
                                eprintln!(
                                    "\x1b[2m  api returned non-JSON ({status}), retrying…\x1b[0m"
                                );
                                tokio::time::sleep(delay).await;
                                delay = (delay * 2).min(MAX_DELAY);
                                continue;
                            }
                            bail!("grok api ({status}): non-JSON response: {snippet}");
                        }
                    };
                    if status == 200 {
                        return Ok(v);
                    }
                    // xAI reports errors in two shapes: the OpenAI
                    // {"error":{"message","type"}} and a flatter {"code","error":
                    // "<string>"} (used for auth failures). Read whichever is present.
                    let msg = v["error"]["message"]
                        .as_str()
                        .or_else(|| v["error"].as_str())
                        .or_else(|| v["code"].as_str())
                        .unwrap_or("unknown error");
                    let kind = v["error"]["type"]
                        .as_str()
                        .or_else(|| v["code"].as_str())
                        .unwrap_or("error");
                    // A bad/expired bearer comes back as 401/403 OR — for xAI — a
                    // 400 "Incorrect API key provided". For a subscription token
                    // that means the access token lapsed: refresh it ONCE (rotating
                    // + persisting the new token) and retry, rather than making the
                    // user re-run the Grok CLI.
                    let auth_failed = matches!(status, 401 | 403)
                        || (status == 400 && {
                            let m = msg.to_ascii_lowercase();
                            m.contains("api key") || m.contains("expired") || m.contains("unauthor")
                        });
                    if auth_failed
                        && !refreshed
                        && !last
                        && matches!(self.auth, GrokAuth::OAuthFile(_))
                    {
                        eprintln!("\x1b[2m  grok token expired — refreshing…\x1b[0m");
                        match self.auth.refresh(&self.client).await {
                            Ok(t) => {
                                bearer = t;
                                refreshed = true;
                                continue; // retry immediately with the fresh token
                            }
                            Err(e) => bail!("{e:#}"),
                        }
                    }
                    // Retry only what's retryable: rate limits (429) and 5xx.
                    if (status == 429 || status >= 500) && !last {
                        eprintln!("\x1b[2m  api {kind} ({status}), retrying…\x1b[0m");
                        tokio::time::sleep(delay).await;
                        delay = (delay * 2).min(MAX_DELAY);
                        continue;
                    }
                    // An auth failure a refresh didn't (or couldn't) fix — or 403,
                    // xAI's SuperGrok-Heavy allowlist. Point the user to the CLI.
                    if auth_failed && matches!(self.auth, GrokAuth::OAuthFile(_)) {
                        bail!(
                            "grok api ({status}): {msg} — your SuperGrok login may have expired or \
lack API access; re-run the Grok CLI to refresh ~/.grok/auth.json"
                        );
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
        bail!("grok api: exhausted retries")
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
            let name = tc["function"]["name"]
                .as_str()
                .unwrap_or_default()
                .to_string();
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
    Ok(Turn {
        text,
        tool_calls,
        raw: None,
        truncated_tool_call,
        usage: None,
        truncated_text: false,
    })
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
                            // S7.3: thread the structured payload (compact JSON)
                            // to the model when present; text-only results send
                            // `content` verbatim as before.
                            "content": r.model_content(),
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
        assert!(
            msgs[0]["content"].is_null(),
            "empty assistant text → null content"
        );
    }

    #[test]
    fn render_tool_results_expand_to_one_message_each() {
        let msg = Msg::tool_results(vec![
            ToolResult::text("call_1", "out1", false),
            ToolResult::text("call_2", "out2", true),
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
    fn render_string_only_tool_result_wire_shape_has_no_payload_key() {
        // S7.4 / AC1: a text-only ToolResult renders to the OpenAI role:"tool"
        // message EXACTLY as before structured results existed — the three
        // canonical keys only (role, tool_call_id, content), `content` verbatim.
        // The typed payload is consumed by model_content(); it must NEVER leak
        // onto the wire as a `structured`/`payload` sibling. The exact-key-count
        // assertion is the guardrail.
        let msg = Msg::tool_results(vec![ToolResult::text("call_1", "verbatim output", false)]);
        let msgs = render_messages(&[msg]);
        let m = msgs[0].as_object().unwrap();
        assert_eq!(m["role"], "tool");
        assert_eq!(m["tool_call_id"], "call_1");
        assert_eq!(m["content"], "verbatim output"); // content verbatim
        assert_eq!(m.len(), 3, "string-only tool message carries no extra key: {m:?}");
        assert!(m.get("structured").is_none());
        assert!(m.get("payload").is_none());
    }

    #[test]
    fn render_tool_result_threads_structured_json_to_model() {
        // S7.3 / AC1: a structured tool result becomes the model-facing content
        // (compact JSON) in its role:"tool" message; a text result stays verbatim.
        let msg = Msg::tool_results(vec![
            ToolResult::structured(
                "s1",
                "rendered table the human sees",
                json!({"path": "f.txt", "type": "file", "size": 3}),
                false,
            ),
            ToolResult::text("t1", "plain text result", false),
        ]);
        let msgs = render_messages(&[msg]);
        assert_eq!(msgs.len(), 2);
        // Structured → compact JSON.
        // Note: JSON key order is determined by the serde_json serializer; we only care that the payload is correct.
        assert_eq!(msgs[0]["tool_call_id"], "s1");
        let content_str = msgs[0]["content"].as_str().unwrap();
        assert!(content_str.contains("\"path\":\"f.txt\""));
        assert!(content_str.contains("\"size\":3"));
        assert!(content_str.contains("\"type\":\"file\""));
        // Text-only → content verbatim.
        assert_eq!(msgs[1]["content"].as_str().unwrap(), "plain text result");
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
        assert!(
            turn.tool_calls.is_empty(),
            "truncated tool call must not execute"
        );
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
        assert!(
            !turn.truncated_tool_call,
            "plain-text truncation isn't a dropped tool call"
        );
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
    fn token_extracted_from_grok_auth_json() {
        // Shape mirrors the real ~/.grok/auth.json: one "<issuer>::<client_id>"
        // login entry whose `key` is the JWT bearer.
        let contents = r#"{
            "https://auth.x.ai::b1a00492-073a-47ea-816f-4c329264a828": {
                "key": "eyJ0eXAiOiJKV1Qif.payload.sig",
                "auth_mode": "oidc",
                "refresh_token": "rt_abc",
                "expires_at": "2026-06-16T11:26:09.947367Z"
            }
        }"#;
        assert_eq!(
            token_from_auth_json(contents).unwrap(),
            "eyJ0eXAiOiJKV1Qif.payload.sig"
        );
    }

    #[test]
    fn token_extraction_errors_are_clear() {
        assert!(token_from_auth_json("not json").is_err());
        assert!(token_from_auth_json("{}").is_err(), "no login entry");
        // entry present but no/blank key
        assert!(token_from_auth_json(r#"{"x::y": {"key": ""}}"#).is_err());
        assert!(token_from_auth_json(r#"{"x::y": {"auth_mode": "oidc"}}"#).is_err());
    }

    #[test]
    fn store_parse_derives_token_endpoint_and_refresh_fields() {
        let contents = r#"{
            "https://auth.x.ai::cid-123": {
                "key": "access.jwt",
                "refresh_token": "rt_xyz",
                "oidc_client_id": "cid-123",
                "oidc_issuer": "https://auth.x.ai"
            }
        }"#;
        let s = GrokOAuthStore::parse(Path::new("(memory)"), contents).unwrap();
        assert_eq!(s.access_token, "access.jwt");
        assert_eq!(s.refresh_token, "rt_xyz");
        assert_eq!(s.client_id, "cid-123");
        assert_eq!(s.token_endpoint, "https://auth.x.ai/oauth2/token");
        assert_eq!(s.entry_key, "https://auth.x.ai::cid-123");
    }

    #[test]
    fn unix_to_rfc3339_matches_known_instants() {
        assert_eq!(unix_to_rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(unix_to_rfc3339(1_700_000_000), "2023-11-14T22:13:20Z");
        // a leap day
        assert_eq!(unix_to_rfc3339(1_582_934_400), "2020-02-29T00:00:00Z");
    }

    #[test]
    fn apply_refresh_updates_entry_and_preserves_other_fields() {
        let mut v: Value = serde_json::from_str(
            r#"{"iss::cid": {"key": "old", "refresh_token": "old_rt", "expires_at": "old_exp", "email": "x@y.z", "user_id": "u1"}}"#,
        )
        .unwrap();
        apply_refresh_to_entry(
            &mut v,
            "iss::cid",
            "new_access",
            Some("new_rt"),
            Some("new_exp"),
        )
        .unwrap();
        let e = &v["iss::cid"];
        assert_eq!(e["key"], "new_access");
        assert_eq!(e["refresh_token"], "new_rt");
        assert_eq!(e["expires_at"], "new_exp");
        // untouched fields survive
        assert_eq!(e["email"], "x@y.z");
        assert_eq!(e["user_id"], "u1");
    }

    #[test]
    fn apply_refresh_keeps_old_refresh_token_when_none_returned() {
        let mut v: Value =
            serde_json::from_str(r#"{"iss::cid": {"key": "old", "refresh_token": "old_rt"}}"#)
                .unwrap();
        apply_refresh_to_entry(&mut v, "iss::cid", "new_access", None, None).unwrap();
        assert_eq!(v["iss::cid"]["key"], "new_access");
        assert_eq!(
            v["iss::cid"]["refresh_token"], "old_rt",
            "no rotation → keep the old token"
        );
    }

    #[test]
    fn apply_refresh_errors_on_missing_entry() {
        let mut v: Value = serde_json::from_str(r#"{"iss::cid": {"key": "old"}}"#).unwrap();
        assert!(apply_refresh_to_entry(&mut v, "nope::nope", "a", None, None).is_err());
    }

    #[test]
    fn sanitize_strips_top_level_schema_key() {
        let s = sanitize_schema(
            &json!({"$schema": "http://json-schema.org/draft-07/schema#", "type": "object", "properties": {}}),
        );
        assert!(s.get("$schema").is_none());
        assert_eq!(s["type"], "object");
    }
}
