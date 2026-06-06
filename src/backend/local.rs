use super::{Msg, Role, ToolCall, ToolDef, Turn};
use anyhow::{Context, Result};
use hf_hub::api::tokio::{ApiBuilder, Progress};
use hf_hub::{Cache, Repo, RepoType};
use mistralrs::{
    CalledFunction, Function, GgufModelBuilder, Model, RequestBuilder, TextMessageRole, Tool,
    ToolCallResponse, ToolCallType, ToolChoice, ToolType,
};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::sync::OnceCell;

/// Default local model. Override with AISH_LOCAL_MODEL_ID /
/// AISH_LOCAL_MODEL_FILE / AISH_LOCAL_TOK_ID; for overridden repos the
/// tokenizer and filename are derived from the Qwen repo convention.
const DEFAULT_MODEL_ID: &str = "Qwen/Qwen3-1.7B-GGUF";
/// The only quant Qwen ships in that repo (no Q4_K_M there).
const DEFAULT_FILE: &str = "Qwen3-1.7B-Q8_0.gguf";

/// In-process inference via mistral.rs — the node-llama-cpp analog.
/// Weights are lazy-loaded on first use, not at shell startup.
pub struct LocalBackend {
    pub model_id: String,
    pub file: String,
    pub tok_model_id: String,
    model: OnceCell<Model>,
}

impl LocalBackend {
    pub fn new() -> Self {
        let env = |name: &str| std::env::var(name).ok().filter(|v| !v.is_empty());
        let (model_id, file, tok_model_id) = resolve(
            env("AISH_LOCAL_MODEL_ID"),
            env("AISH_LOCAL_MODEL_FILE"),
            env("AISH_LOCAL_TOK_ID"),
        );
        Self { model_id, file, tok_model_id, model: OnceCell::new() }
    }

    /// Force the lazy load now. The engine calls this before its "thinking"
    /// spinner starts, so the download progress line owns stderr.
    pub async fn prepare(&self) -> Result<()> {
        self.model().await.map(|_| ())
    }

    async fn model(&self) -> Result<&Model> {
        self.model
            .get_or_try_init(|| async {
                self.ensure_gguf_cached().await?;
                eprintln!("\x1b[2m  loading {} / {}…\x1b[0m", self.model_id, self.file);
                let model = GgufModelBuilder::new(self.model_id.clone(), vec![self.file.clone()])
                    .with_tok_model_id(self.tok_model_id.clone())
                    .build()
                    .await
                    .map_err(|e| anyhow::anyhow!("failed to load local model: {e}"))?;
                eprintln!("\x1b[2m  model ready\x1b[0m");
                Ok::<_, anyhow::Error>(model)
            })
            .await
    }

    /// Prefetch the GGUF into the HF cache with a visible progress line —
    /// mistral.rs would otherwise download it invisibly inside build(). Once
    /// the blob is cached, build() finds it and skips the network.
    ///
    /// Cache root: `Cache::default()`, NOT `from_env()` — mistral.rs resolves
    /// its cache via `GLOBAL_HF_CACHE.get().unwrap_or_default()` (ignoring
    /// HF_HOME), and prefetching anywhere build() won't look means a silent
    /// second 2.5GB download.
    async fn ensure_gguf_cached(&self) -> Result<()> {
        let repo = Repo::with_revision(self.model_id.clone(), RepoType::Model, "main".into());
        let cache = Cache::default();
        if cache.repo(repo.clone()).get(&self.file).is_some() {
            return Ok(());
        }
        let api = ApiBuilder::from_cache(cache)
            .with_progress(false) // we draw our own
            .build()
            .context("failed to init hf-hub client")?;
        api.repo(repo)
            .download_with_progress(&self.file, DownloadBar::new(&self.file))
            .await
            .map_err(|e| {
                anyhow::anyhow!("failed to download {}/{}: {e}", self.model_id, self.file)
            })?;
        Ok(())
    }

    pub async fn complete(&self, system: &str, history: &[Msg], tools: &[ToolDef]) -> Result<Turn> {
        let model = self.model().await?;

        // Qwen3 soft-switch: a shell wants answers, not <think> spelunking.
        let system = format!("{system}\n/no_think");

        let mut req = RequestBuilder::new().add_message(TextMessageRole::System, system);
        for msg in history {
            match msg.role {
                Role::User => {
                    if msg.tool_results.is_empty() {
                        req = req.add_message(TextMessageRole::User, &msg.text);
                    } else {
                        for r in &msg.tool_results {
                            req = req.add_tool_message(&r.content, r.id.clone());
                        }
                    }
                }
                Role::Assistant => {
                    if msg.tool_calls.is_empty() {
                        req = req.add_message(TextMessageRole::Assistant, &msg.text);
                    } else {
                        let calls: Vec<ToolCallResponse> = msg
                            .tool_calls
                            .iter()
                            .enumerate()
                            .map(|(i, tc)| ToolCallResponse {
                                index: i,
                                id: tc.id.clone(),
                                tp: ToolCallType::Function,
                                function: CalledFunction {
                                    name: tc.name.clone(),
                                    arguments: tc.args.to_string(),
                                },
                            })
                            .collect();
                        req = req.add_message_with_tool_call(
                            TextMessageRole::Assistant,
                            msg.text.clone(),
                            calls,
                        );
                    }
                }
            }
        }
        req = req.set_tools(render_tools(tools)?).set_tool_choice(ToolChoice::Auto);

        let response = model
            .send_chat_request(req)
            .await
            .map_err(|e| anyhow::anyhow!("local inference failed: {e}"))?;
        let message = &response
            .choices
            .first()
            .context("local model returned no choices")?
            .message;

        let text = strip_think(message.content.as_deref().unwrap_or(""));
        let tool_calls = message
            .tool_calls
            .as_deref()
            .unwrap_or(&[])
            .iter()
            .map(|tc| ToolCall {
                id: tc.id.clone(),
                name: tc.function.name.clone(),
                args: serde_json::from_str(&tc.function.arguments)
                    .unwrap_or_else(|_| Value::String(tc.function.arguments.clone())),
            })
            .collect();

        Ok(Turn { text, tool_calls, raw: None })
    }
}

/// Resolve (repo, gguf file, tokenizer repo) from env overrides, deriving
/// unset ones from the Qwen convention: "<org>/<name>-GGUF" repos pair with
/// an "<org>/<name>" tokenizer and a "<name>-Q4_K_M.gguf" file — so swapping
/// models is usually just AISH_LOCAL_MODEL_ID.
fn resolve(
    model_id: Option<String>,
    file: Option<String>,
    tok: Option<String>,
) -> (String, String, String) {
    // Treat an explicit AISH_LOCAL_MODEL_ID equal to the default the same as
    // unset — the default repo's only quant is Q8_0, not the derived Q4_K_M.
    let default_repo = model_id.as_deref().is_none_or(|m| m == DEFAULT_MODEL_ID);
    let model_id = model_id.unwrap_or_else(|| DEFAULT_MODEL_ID.into());
    let base = model_id.strip_suffix("-GGUF").unwrap_or(&model_id).to_string();
    let name = base.rsplit('/').next().unwrap_or(&base);
    let file = file.unwrap_or_else(|| {
        if default_repo { DEFAULT_FILE.into() } else { format!("{name}-Q4_K_M.gguf") }
    });
    (model_id, file, tok.unwrap_or(base))
}

/// Transient download progress line on stderr — "⇣ file 42% (1.0 / 2.4 GB)".
/// hf-hub clones it once per parallel chunk, so the counters are shared
/// atomics. TTY-gated like the engine spinner; non-TTY stderr gets a plain
/// start/end line instead of \r redraws.
#[derive(Clone)]
struct DownloadBar {
    file: Arc<str>,
    total: Arc<AtomicUsize>,
    got: Arc<AtomicUsize>,
    pct: Arc<AtomicUsize>,
    tty: bool,
}

impl DownloadBar {
    fn new(file: &str) -> Self {
        Self {
            file: file.into(),
            total: Arc::default(),
            got: Arc::default(),
            pct: Arc::new(AtomicUsize::new(usize::MAX)), // so 0% draws too
            // SAFETY: plain isatty query.
            tty: unsafe { libc::isatty(2) } == 1,
        }
    }
}

impl Progress for DownloadBar {
    async fn init(&mut self, size: usize, _filename: &str) {
        self.total.store(size, Ordering::Relaxed);
        if !self.tty {
            eprintln!("  downloading {} ({})…", self.file, fmt_size(size));
        }
    }

    async fn update(&mut self, delta: usize) {
        let got = self.got.fetch_add(delta, Ordering::Relaxed) + delta;
        let total = self.total.load(Ordering::Relaxed);
        if !self.tty || total == 0 {
            return;
        }
        // Redraw only on whole-percent changes — update() fires per chunk.
        let pct = got * 100 / total;
        if self.pct.swap(pct, Ordering::Relaxed) != pct {
            // Bright-cyan glyph, dim-cyan label — matches the spinner (FR-256).
            eprint!(
                "\r\x1b[36m⇣\x1b[0m \x1b[2;36m{} {pct}% ({} / {})\x1b[0m\x1b[K",
                self.file,
                fmt_size(got),
                fmt_size(total)
            );
        }
    }

    async fn finish(&mut self) {
        if self.tty {
            eprint!("\r\x1b[2K"); // erase the progress line
        }
        eprintln!("\x1b[2m  downloaded {}\x1b[0m", self.file);
    }
}

fn fmt_size(bytes: usize) -> String {
    let mb = bytes as f64 / (1024.0 * 1024.0);
    if mb >= 1000.0 {
        format!("{:.2} GB", mb / 1024.0)
    } else {
        format!("{mb:.0} MB")
    }
}

fn render_tools(tools: &[ToolDef]) -> Result<Vec<Tool>> {
    tools
        .iter()
        .map(|t| {
            let parameters: HashMap<String, Value> = serde_json::from_value(t.schema.clone())
                .context("tool schema must be a JSON object")?;
            Ok(Tool {
                tp: ToolType::Function,
                function: Function {
                    name: t.name.to_string(),
                    description: Some(t.description.clone()),
                    parameters: Some(parameters),
                },
            })
        })
        .collect()
}

/// Qwen3 may still emit <think>…</think> even with /no_think, and mistral.rs
/// leaves the raw <tool_call>…</tool_call> text in content even after parsing
/// it into message.tool_calls — drop both.
fn strip_think(s: &str) -> String {
    let s = strip_tag(s, "<think>", "</think>");
    strip_tag(&s, "<tool_call>", "</tool_call>").trim().to_string()
}

fn strip_tag(s: &str, open: &str, close: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let (Some(a), Some(b)) = (rest.find(open), rest.find(close)) {
        if b < a {
            break;
        }
        out.push_str(&rest[..a]);
        rest = &rest[b + close.len()..];
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_derives_from_qwen_convention() {
        // No overrides → defaults (the default repo ships only a Q8_0 file).
        assert_eq!(
            resolve(None, None, None),
            (
                "Qwen/Qwen3-1.7B-GGUF".into(),
                "Qwen3-1.7B-Q8_0.gguf".into(),
                "Qwen/Qwen3-1.7B".into()
            )
        );
        // Repo override alone derives file + tokenizer.
        assert_eq!(
            resolve(Some("Qwen/Qwen3-8B-GGUF".into()), None, None),
            (
                "Qwen/Qwen3-8B-GGUF".into(),
                "Qwen3-8B-Q4_K_M.gguf".into(),
                "Qwen/Qwen3-8B".into()
            )
        );
        // Explicit overrides win.
        let (m, f, t) = resolve(
            Some("org/other".into()),
            Some("other-Q8_0.gguf".into()),
            Some("org/other-tok".into()),
        );
        assert_eq!((m.as_str(), f.as_str(), t.as_str()), ("org/other", "other-Q8_0.gguf", "org/other-tok"));
        // Non-GGUF-suffixed repo: tokenizer falls back to the repo itself.
        let (_, f, t) = resolve(Some("org/plain".into()), None, None);
        assert_eq!((f.as_str(), t.as_str()), ("plain-Q4_K_M.gguf", "org/plain"));
        // Explicitly setting the default repo behaves like unset (Q8_0 file).
        let (_, f, _) = resolve(Some("Qwen/Qwen3-1.7B-GGUF".into()), None, None);
        assert_eq!(f, "Qwen3-1.7B-Q8_0.gguf");
    }

    #[test]
    fn fmt_size_units() {
        assert_eq!(fmt_size(0), "0 MB");
        assert_eq!(fmt_size(512 * 1024 * 1024), "512 MB");
        assert_eq!(fmt_size(2_560 * 1024 * 1024), "2.50 GB");
    }

    /// Network test (hits huggingface.co with a ~1KB file) — run with
    /// `cargo test -- --ignored`. Exercises the real prefetch path:
    /// download with DownloadBar, then the cache-hit short-circuit.
    #[tokio::test]
    #[ignore]
    async fn download_bar_fetches_and_caches() {
        let repo = Repo::with_revision("Qwen/Qwen3-4B".into(), RepoType::Model, "main".into());
        let api = ApiBuilder::from_cache(Cache::default()).with_progress(false).build().unwrap();
        let path = api
            .repo(repo.clone())
            .download_with_progress("config.json", DownloadBar::new("config.json"))
            .await
            .unwrap();
        assert!(path.exists());
        // ensure_gguf_cached's fast path must now find it without the network.
        assert!(Cache::default().repo(repo).get("config.json").is_some());
    }
}
