//! OpenAI-compatible chat-completions backend for aish.
//!
//! This backend speaks the same `/v1/chat/completions` wire protocol as
//! [`super::grok`] (which targets xAI, itself OpenAI-compatible), but instead of
//! xAI's subscription-OAuth token store it authenticates with a plain metered
//! API key — the model both OpenAI and OpenRouter (and, by base-URL override,
//! any other OpenAI-compatible endpoint) use.
//!
//! A single [`Provider`] enum parameterizes the three things that differ between
//! compatible providers: the default base URL, the default model, and the
//! API-key env var(s). The message rendering, tool-schema sanitization, and
//! response parsing are shared verbatim with the Grok backend
//! ([`super::grok::render_messages`], [`super::grok::sanitize_schema`],
//! [`super::grok::parse_response`]) — that IS the OpenAI wire format.

use super::{Msg, ToolDef, Turn};
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::time::Duration;

/// Upper bound on output tokens per turn. Matches the Grok/Claude budget: a
/// whole-file `write_file` rewrite can be large; 32k comfortably fits one and is
/// within every current OpenAI/OpenRouter model's completion budget. Sent as
/// `max_completion_tokens` (the OpenAI-current field; `max_tokens` is deprecated
/// and rejected by newer OpenAI reasoning models).
const MAX_COMPLETION_TOKENS: u64 = 32000;

/// Default OpenAI model — broadly available, supports tool/function calling, and
/// carries a 128k context window.
pub const OPENAI_DEFAULT_MODEL: &str = "gpt-4o";

/// Default OpenRouter model — the OpenRouter slug form (`vendor/model`). Points
/// at the same GPT-4o class model but routed through OpenRouter's gateway.
pub const OPENROUTER_DEFAULT_MODEL: &str = "openai/gpt-4o";

/// Which OpenAI-compatible provider a backend instance targets. The variants
/// differ only in defaults (base URL, model, key env) — the wire format is
/// identical.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provider {
    OpenAi,
    OpenRouter,
}

impl Provider {
    /// Parse the `--backend` / `:backend` token into a provider. Accepts a few
    /// friendly aliases.
    pub fn parse(name: &str) -> Option<Self> {
        match name.to_ascii_lowercase().as_str() {
            "openai" | "oai" | "gpt" => Some(Provider::OpenAi),
            "openrouter" | "or" => Some(Provider::OpenRouter),
            _ => None,
        }
    }

    /// Stable short name — the `backend_kind` string threaded to coordinators and
    /// matched on across the codebase.
    pub fn kind(&self) -> &'static str {
        match self {
            Provider::OpenAi => "openai",
            Provider::OpenRouter => "openrouter",
        }
    }

    /// Human label for `describe()`.
    pub fn label(&self) -> &'static str {
        match self {
            Provider::OpenAi => "openai",
            Provider::OpenRouter => "openrouter",
        }
    }

    /// The default model when the operator didn't pin one with `--model`.
    pub fn default_model(&self) -> &'static str {
        match self {
            Provider::OpenAi => OPENAI_DEFAULT_MODEL,
            Provider::OpenRouter => OPENROUTER_DEFAULT_MODEL,
        }
    }

    /// The default API base (no trailing `/chat/completions`).
    fn default_base_url(&self) -> &'static str {
        match self {
            Provider::OpenAi => "https://api.openai.com/v1",
            Provider::OpenRouter => "https://openrouter.ai/api/v1",
        }
    }

    /// Env var(s) checked (in order) for the base-URL override, letting an
    /// operator point the OpenAI provider at ANY OpenAI-compatible endpoint
    /// (Azure OpenAI, a local vLLM/Ollama shim, Together, Groq, …).
    fn base_url_env(&self) -> &'static [&'static str] {
        match self {
            Provider::OpenAi => &["OPENAI_BASE_URL", "OPENAI_API_BASE"],
            Provider::OpenRouter => &["OPENROUTER_BASE_URL"],
        }
    }

    /// Env var(s) checked (in order) for the bearer API key.
    fn key_env(&self) -> &'static [&'static str] {
        match self {
            Provider::OpenAi => &["OPENAI_API_KEY"],
            Provider::OpenRouter => &["OPENROUTER_API_KEY"],
        }
    }
}

/// Resolve the provider from a `backend_kind` string (the inverse of
/// [`Provider::kind`]). Used by the coordinator-model + credential plumbing which
/// only carry the kind string.
pub fn provider_for_kind(kind: &str) -> Option<Provider> {
    match kind {
        "openai" => Some(Provider::OpenAi),
        "openrouter" => Some(Provider::OpenRouter),
        _ => None,
    }
}

/// The default coordinator model for an OpenAI-compatible `backend_kind`. Mirrors
/// [`super::grok::DEFAULT_MODEL`]'s role for Grok coordinators.
pub fn default_model_for_kind(kind: &str) -> Option<&'static str> {
    provider_for_kind(kind).map(|p| p.default_model())
}

/// Look up the API key for `provider` from the rc/process env, honoring the
/// provider's key-env precedence list.
fn resolve_key(provider: Provider, extra: &[(String, String)]) -> Option<String> {
    provider
        .key_env()
        .iter()
        .find_map(|k| crate::rc::env_value(extra, k))
        .filter(|v| !v.is_empty())
}

/// True when an API key for `provider` is resolvable. Used by the
/// background-dispatch guards when the active backend is OpenAI/OpenRouter.
pub fn credential_available(provider: Provider, extra: &[(String, String)]) -> bool {
    resolve_key(provider, extra).is_some()
}

pub struct OpenAiBackend {
    client: reqwest::Client,
    provider: Provider,
    api_key: String,
    /// Full endpoint URL (base + `/chat/completions`), resolved once at build.
    endpoint: String,
    /// Optional extra headers (OpenRouter attribution). Sent on every request.
    extra_headers: Vec<(String, String)>,
    pub model: String,
}

impl OpenAiBackend {
    /// Resolve the API key (and any base-URL override) for `provider` and build
    /// the backend.
    pub fn new(provider: Provider, model: String, extra_env: &[(String, String)]) -> Result<Self> {
        let api_key = resolve_key(provider, extra_env).ok_or_else(|| {
            let keys = provider.key_env().join(" or ");
            anyhow::anyhow!(
                "no {} credential — set {} in your environment or ~/.aishrc",
                provider.label(),
                keys
            )
        })?;

        // Base URL: an operator override (env) wins, else the provider default.
        let base = provider
            .base_url_env()
            .iter()
            .find_map(|k| crate::rc::env_value(extra_env, k))
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| provider.default_base_url().to_string());
        let endpoint = build_endpoint(&base);

        // OpenRouter recommends (optional) attribution headers so requests are
        // ranked/labeled on their dashboard. HTTP-Referer is overridable via env.
        let mut extra_headers = Vec::new();
        if provider == Provider::OpenRouter {
            let referer = crate::rc::env_value(extra_env, "OPENROUTER_REFERER")
                .filter(|v| !v.is_empty())
                .unwrap_or_else(|| "https://github.com/LightHeart-Ventures/aish".to_string());
            extra_headers.push(("HTTP-Referer".to_string(), referer));
            extra_headers.push(("X-Title".to_string(), "aish".to_string()));
        }

        Ok(Self {
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(300))
                .build()?,
            provider,
            api_key,
            endpoint,
            extra_headers,
            model,
        })
    }

    /// Stable short name (`"openai"` / `"openrouter"`).
    pub fn kind(&self) -> &'static str {
        self.provider.kind()
    }

    /// Non-secret credential label for `describe()`.
    pub fn auth_label(&self) -> &'static str {
        "api key"
    }

    pub async fn complete(&self, system: &str, history: &[Msg], tools: &[ToolDef]) -> Result<Turn> {
        let mut messages = Vec::with_capacity(history.len() + 1);
        // OpenAI-style: the system prompt is the FIRST message, not a top-level
        // field (Anthropic's shape).
        messages.push(json!({"role": "system", "content": system}));
        messages.extend(super::grok::render_messages(history));

        let mut body = json!({
            "model": self.model,
            "max_completion_tokens": MAX_COMPLETION_TOKENS,
            "messages": messages,
        });
        if !tools.is_empty() {
            let tool_defs: Vec<Value> = tools
                .iter()
                .map(|t| {
                    json!({
                        "type": "function",
                        "function": {
                            "name": t.name,
                            "description": t.description,
                            "parameters": super::grok::sanitize_schema(&t.schema),
                        },
                    })
                })
                .collect();
            body["tools"] = Value::Array(tool_defs);
        }

        let v = self.post_with_retry(&body).await?;
        super::grok::parse_response(&v)
    }

    async fn post_with_retry(&self, body: &Value) -> Result<Value> {
        // A headless coordinator can run for many minutes; a transient network
        // burst must not be fatal. Retry generously with capped exponential
        // backoff. Unlike Grok there is no OAuth refresh — the API key is static.
        const MAX_ATTEMPTS: u32 = 6;
        const MAX_DELAY: Duration = Duration::from_secs(30);
        let mut delay = Duration::from_secs(2);
        for attempt in 0..MAX_ATTEMPTS {
            let last = attempt + 1 == MAX_ATTEMPTS;
            let mut req = self
                .client
                .post(&self.endpoint)
                .header("authorization", format!("Bearer {}", self.api_key))
                .header("content-type", "application/json");
            for (k, val) in &self.extra_headers {
                req = req.header(k.as_str(), val.as_str());
            }
            let resp = req.json(body).send().await;

            match resp {
                Ok(r) => {
                    // Decode to text first so a non-JSON gateway/edge body (502/503
                    // HTML, empty body, challenge page) is a retryable signal, not
                    // a fatal decode error. See `super::read_status_and_json`.
                    let (status, parsed) = match super::read_status_and_json(r).await {
                        Ok(p) => p,
                        Err(e) if !last => {
                            eprintln!("\x1b[2m  network error reading body ({e}), retrying…\x1b[0m");
                            tokio::time::sleep(delay).await;
                            delay = (delay * 2).min(MAX_DELAY);
                            continue;
                        }
                        Err(e) => {
                            return Err(e).context("reading openai api response body");
                        }
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
                            bail!(
                                "{} api ({status}): non-JSON response: {snippet}",
                                self.provider.label()
                            );
                        }
                    };
                    if status == 200 {
                        return Ok(v);
                    }
                    // OpenAI/OpenRouter error shape: {"error":{"message","type","code"}}.
                    let msg = v["error"]["message"]
                        .as_str()
                        .or_else(|| v["error"].as_str())
                        .unwrap_or("unknown error");
                    let kind = v["error"]["type"]
                        .as_str()
                        .or_else(|| v["error"]["code"].as_str())
                        .unwrap_or("error");
                    // Retry only what's retryable: rate limits (429) and 5xx.
                    if (status == 429 || status >= 500) && !last {
                        eprintln!("\x1b[2m  api {kind} ({status}), retrying…\x1b[0m");
                        tokio::time::sleep(delay).await;
                        delay = (delay * 2).min(MAX_DELAY);
                        continue;
                    }
                    if matches!(status, 401 | 403) {
                        bail!(
                            "{} api ({status}): {msg} — check your API key ({})",
                            self.provider.label(),
                            self.provider.key_env().join(" / ")
                        );
                    }
                    bail!("{} api {kind} ({status}): {msg}", self.provider.label());
                }
                // Transport-level error (connect reset, timeout, dns): transient.
                Err(e) if !last => {
                    eprintln!("\x1b[2m  network error ({e}), retrying…\x1b[0m");
                    tokio::time::sleep(delay).await;
                    delay = (delay * 2).min(MAX_DELAY);
                }
                Err(e) => return Err(e).context("request to openai api failed"),
            }
        }
        bail!("{} api: exhausted retries", self.provider.label())
    }
}

/// Build the full chat-completions endpoint from a base URL. If the operator's
/// override already includes the `/chat/completions` path, use it verbatim;
/// otherwise append it to the (trailing-slash-trimmed) base.
fn build_endpoint(base: &str) -> String {
    let trimmed = base.trim_end_matches('/');
    if trimmed.ends_with("/chat/completions") {
        trimmed.to_string()
    } else {
        format!("{trimmed}/chat/completions")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_parse_aliases() {
        assert_eq!(Provider::parse("openai"), Some(Provider::OpenAi));
        assert_eq!(Provider::parse("OpenAI"), Some(Provider::OpenAi));
        assert_eq!(Provider::parse("gpt"), Some(Provider::OpenAi));
        assert_eq!(Provider::parse("openrouter"), Some(Provider::OpenRouter));
        assert_eq!(Provider::parse("or"), Some(Provider::OpenRouter));
        assert_eq!(Provider::parse("claude"), None);
    }

    #[test]
    fn kind_roundtrips_to_provider() {
        assert_eq!(provider_for_kind("openai"), Some(Provider::OpenAi));
        assert_eq!(provider_for_kind("openrouter"), Some(Provider::OpenRouter));
        assert_eq!(provider_for_kind("grok"), None);
    }

    #[test]
    fn endpoint_building() {
        assert_eq!(
            build_endpoint("https://api.openai.com/v1"),
            "https://api.openai.com/v1/chat/completions"
        );
        assert_eq!(
            build_endpoint("https://api.openai.com/v1/"),
            "https://api.openai.com/v1/chat/completions"
        );
        // Already-full path is honored verbatim.
        assert_eq!(
            build_endpoint("https://proxy.internal/v1/chat/completions"),
            "https://proxy.internal/v1/chat/completions"
        );
    }

    #[test]
    fn key_resolution_precedence() {
        let env = vec![("OPENAI_API_KEY".to_string(), "sk-test".to_string())];
        assert!(credential_available(Provider::OpenAi, &env));
        assert!(!credential_available(Provider::OpenRouter, &env));
    }

    #[test]
    fn default_models() {
        assert_eq!(Provider::OpenAi.default_model(), "gpt-4o");
        assert_eq!(Provider::OpenRouter.default_model(), "openai/gpt-4o");
        assert_eq!(default_model_for_kind("openai"), Some("gpt-4o"));
        assert_eq!(default_model_for_kind("grok"), None);
    }
}
