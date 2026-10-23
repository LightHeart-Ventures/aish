//! Local inference backend using llama.cpp for GGUF model support.
//!
//! This module provides lightweight, embedded inference via llama-cpp-2 bindings.
//! Models are lazy-loaded on first use. The model is chosen by the hardware-aware
//! selector (see `crate::hwdetect`), with overrides via environment variables.
//!
//! Environment variables:
//! - `AISH_LOCAL_MODEL_PATH`: Full path to a .gguf model file (operator pin)
//! - `AISH_LOCAL_MODEL_ID`: Model id from the selection ladder (e.g., "qwen2.5-0.5b-instruct")
//! - `AISH_LOCAL_N_GPU_LAYERS`: Number of layers to offload to GPU (default: 0, CPU-only)
//! - `AISH_LOCAL_N_CTX`: Context window in tokens (default: 4096, clamped 512..=131072)
//! - `AISH_LOCAL_MAX_OUTPUT_TOKENS`: Tokens reserved for the reply (default: 512, clamped 16..=8192)
//! - `AISH_LOCAL_BATCH_SIZE`: Prompt tokens per `decode` call (default: 512, clamped 32..=512)
//!
//! Prompt feeding is planned by [`super::local_plan`]: prompts larger than one
//! batch are decoded in chunks, and prompts larger than the context window are
//! trimmed from the middle (system prompt and most recent turns survive).

#[cfg(feature = "local")]
use anyhow::{Context, anyhow};

#[cfg(feature = "local")]
use super::local_plan;
use anyhow::Result;
#[cfg(feature = "local")]
use llama_cpp_2::{
    context::params::LlamaContextParams,
    llama_backend::LlamaBackend,
    llama_batch::LlamaBatch,
    model::{LlamaModel, params::LlamaModelParams},
    sampling::LlamaSampler,
};
#[cfg(feature = "local")]
use std::num::NonZeroU32;
#[cfg(feature = "local")]
use std::path::PathBuf;
#[cfg(feature = "local")]
use tokio::sync::OnceCell;

use super::{Msg, Role, ToolDef, Turn};

#[cfg(feature = "local")]
pub struct LocalBackend {
    /// An operator pin from `AISH_LOCAL_MODEL_PATH`, if set. When present it is
    /// authoritative and must already exist on disk. When `None`, the GGUF is
    /// resolved (and downloaded on first use) from the hardware-detected
    /// selection by `crate::modelfetch::ensure_model_file`.
    pinned_path: Option<PathBuf>,
    n_gpu_layers: u32,
    model: OnceCell<LlamaModel>,
    backend: LlamaBackend,
}

#[cfg(feature = "local")]
impl LocalBackend {
    pub fn new() -> Result<Self> {
        // An explicit operator pin wins outright; otherwise the GGUF is resolved
        // (and downloaded on first use) from the hardware-detected selection when
        // the model is actually loaded — see `resolve_path`.
        let pinned_path = std::env::var("AISH_LOCAL_MODEL_PATH")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .map(PathBuf::from);

        let n_gpu_layers = std::env::var("AISH_LOCAL_N_GPU_LAYERS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);

        // Must precede `init()`: the Metal device probe logs during that call.
        super::llama_log::init_backend_logging();
        let backend = LlamaBackend::init().context("failed to initialize llama.cpp backend")?;

        Ok(Self {
            pinned_path,
            n_gpu_layers,
            model: OnceCell::new(),
            backend,
        })
    }

    /// Force the lazy load before inference. Called before the spinner starts
    /// so any download/load output owns stderr.
    pub async fn prepare(&self) -> Result<()> {
        self.model().await.map(|_| ())
    }

    /// Resolve the concrete GGUF path to load. An operator pin
    /// (`AISH_LOCAL_MODEL_PATH`) must already exist; otherwise the
    /// hardware-detected selection is resolved to a cached file, downloading the
    /// weights from Hugging Face on first use (`crate::modelfetch`).
    async fn resolve_path(&self) -> Result<PathBuf> {
        if let Some(p) = &self.pinned_path {
            if !p.is_file() {
                return Err(anyhow!(
                    "pinned local model file not found: {} (AISH_LOCAL_MODEL_PATH)",
                    p.display()
                ));
            }
            return Ok(p.clone());
        }
        let sel = crate::hwdetect::load_selection();
        crate::modelfetch::ensure_model_file(sel.as_ref())
            .await
            .context("failed to resolve local model file")
    }

    async fn model(&self) -> Result<&LlamaModel> {
        self.model
            .get_or_try_init(|| async {
                let path = self.resolve_path().await?;
                eprintln!("\x1b[2m  loading model from {}…\x1b[0m", path.display());

                let model_params = LlamaModelParams::default().with_n_gpu_layers(self.n_gpu_layers);

                let model = LlamaModel::load_from_file(&self.backend, &path, &model_params)
                    .map_err(|e| anyhow!("failed to load local model: {e}"))?;

                eprintln!("\x1b[2m  model ready\x1b[0m");
                Ok::<_, anyhow::Error>(model)
            })
            .await
    }

    pub async fn complete(
        &self,
        system: &str,
        history: &[Msg],
        _tools: &[ToolDef],
    ) -> Result<Turn> {
        let model = self.model().await?;

        // Build the prompt from system + history.
        let system_header = format!("[INST] {system}\n\n");
        let mut prompt = system_header.clone();

        for msg in history {
            match msg.role {
                Role::User => {
                    prompt.push_str(&format!("{} [/INST]\n", msg.text));
                }
                Role::Assistant => {
                    prompt.push_str(&format!("{}\n\n[INST] ", msg.text));
                }
            }
        }

        // Tokenize prompt. As of llama-cpp-2 0.1.158 the token helpers live on
        // `LlamaVocab` instead of `LlamaModel`, and the `AddBos`/`Special` enums
        // were replaced by plain bools. `tokenize(.., add_special, parse_special)`
        // is the exact equivalent of the old `str_to_token(.., AddBos::Always)`.
        let vocab = model.vocab();
        let tokens = vocab.tokenize(prompt.as_bytes(), true, true);

        // Tunables. Garbage or out-of-range values clamp to a sane window
        // instead of failing the turn (see `local_plan::env_usize`).
        let n_ctx =
            local_plan::env_usize("AISH_LOCAL_N_CTX", local_plan::DEFAULT_N_CTX, 512, 131_072);
        let max_tokens = local_plan::env_usize(
            "AISH_LOCAL_MAX_OUTPUT_TOKENS",
            local_plan::DEFAULT_MAX_OUTPUT_TOKENS,
            16,
            8192,
        );
        // Capped at llama.cpp's default `n_batch` (512): a chunk larger than the
        // context's batch size would be rejected by `decode`, and we don't touch
        // `n_batch` here.
        let batch_capacity = local_plan::env_usize(
            "AISH_LOCAL_BATCH_SIZE",
            local_plan::DEFAULT_BATCH_CAPACITY,
            32,
            512,
        );

        // Plan the feed BEFORE allocating the context: the plan tells us how
        // many prompt tokens actually survive the window, chunked so no single
        // `decode` exceeds the batch.
        let keep_head = vocab.tokenize(system_header.as_bytes(), true, true).len();
        let plan =
            local_plan::plan_prompt(tokens.len(), keep_head, n_ctx, max_tokens, batch_capacity);

        if plan.is_empty() {
            return Err(anyhow!("local backend: empty prompt, nothing to decode"));
        }
        if plan.trimmed() {
            eprintln!(
                "\x1b[2m  prompt trimmed: dropped {} of {} tokens to fit n_ctx={} (reserved {} for output)\x1b[0m",
                plan.dropped,
                plan.dropped + plan.kept,
                n_ctx,
                max_tokens
            );
        }

        // Create context and batch.
        let ctx_params = LlamaContextParams::default().with_n_ctx(NonZeroU32::new(n_ctx as u32));
        let mut ctx = model
            .new_context(&self.backend, ctx_params)
            .context("failed to create inference context")?;

        // Feed the prompt chunk by chunk. Only the final token of the final
        // chunk needs logits — that's where sampling starts.
        let mut batch = LlamaBatch::new(plan.batch_capacity, 1);
        let last_chunk = plan.chunks.len() - 1;
        for (ci, chunk) in plan.chunks.iter().enumerate() {
            batch.clear();
            for i in 0..chunk.len {
                let wants_logits = ci == last_chunk && i + 1 == chunk.len;
                batch
                    .add(
                        tokens[chunk.src_start + i],
                        (chunk.pos_start + i) as i32,
                        &[0],
                        wants_logits,
                    )
                    .context("failed to add prompt token to batch")?;
            }
            ctx.decode(&mut batch)
                .context("failed to decode prompt chunk")?;
        }

        // Generate output tokens with greedy sampling.
        let mut sampler = LlamaSampler::greedy();
        let mut output = String::new();
        // Sampling continues at the position after the last *kept* prompt token
        // (positions are renumbered over the kept tokens when trimming).
        let mut n_cur = plan.next_position() as i32;
        let mut generated = 0usize;

        for _ in 0..max_tokens {
            let next_token = sampler.sample(&ctx, batch.n_tokens() - 1);
            sampler.accept(next_token);

            // Check for end-of-sequence.
            if vocab.is_eog(next_token) {
                break;
            }

            // Decode token to bytes, then lossily to UTF-8 and add to output.
            // Proper streaming decode wants a stateful `encoding_rs` decoder to
            // stitch multi-token UTF-8 sequences (emoji); to avoid pulling in
            // that dependency we take the raw bytes per token and decode
            // lossily — adequate for the experimental local backend.
            // `special: false` is the 0.1.158 replacement for the old
            // `Special`/bool arg on `LlamaModel::token_to_piece_bytes`.
            let token_bytes = vocab.token_to_piece(next_token, false, None);
            output.push_str(&String::from_utf8_lossy(&token_bytes));
            generated += 1;

            // Prepare next batch.
            batch.clear();
            batch
                .add(next_token, n_cur, &[0], true)
                .context("failed to add next token to batch")?;
            n_cur += 1;

            ctx.decode(&mut batch).context("failed to decode batch")?;
        }

        Ok(Turn {
            text: output.trim().to_string(),
            tool_calls: vec![], // Local model doesn't support tool calling yet.
            raw: None,
            truncated_tool_call: false,
            usage: None,
            truncated_text: generated >= max_tokens,
        })
    }
}

#[cfg(not(feature = "local"))]
pub struct LocalBackend;

#[cfg(not(feature = "local"))]
impl LocalBackend {
    pub fn new() -> Result<Self> {
        Err(anyhow!("aish was built without the 'local' feature"))
    }

    pub async fn prepare(&self) -> Result<()> {
        unreachable!()
    }

    pub async fn complete(
        &self,
        _system: &str,
        _history: &[Msg],
        _tools: &[ToolDef],
    ) -> Result<Turn> {
        unreachable!()
    }
}
