//! Local inference backend using llama.cpp for GGUF model support.
//!
//! This module provides lightweight, embedded inference via llama-cpp-2 bindings.
//! Models are lazy-loaded on first use. The default model is Mistral 7B Instruct
//! (quantized), with model selection via environment variables.
//!
//! Environment variables:
//! - `AISH_LOCAL_MODEL_PATH`: Full path to a .gguf model file (required if not using HF Hub)
//! - `AISH_LOCAL_MODEL_ID`: Hugging Face model ID (e.g., "mistralai/Mistral-7B-Instruct-v0.2")
//! - `AISH_LOCAL_N_GPU_LAYERS`: Number of layers to offload to GPU (default: 0, CPU-only)

#[cfg(feature = "local")]
use anyhow::{anyhow, Context};

use anyhow::Result;
#[cfg(feature = "local")]
use llama_cpp_2::{
    context::params::LlamaContextParams,
    llama_backend::LlamaBackend,
    llama_batch::LlamaBatch,
    model::{params::LlamaModelParams, AddBos, LlamaModel},
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
    model_path: PathBuf,
    n_gpu_layers: u32,
    model: OnceCell<LlamaModel>,
    backend: LlamaBackend,
}

#[cfg(feature = "local")]
impl LocalBackend {
    pub fn new() -> Result<Self> {
        // Resolve the GGUF path. Precedence:
        //   1. AISH_LOCAL_MODEL_PATH — an explicit operator-pinned file.
        //   2. The persisted hardware-detected selection's recorded path, if
        //      the operator downloaded a GGUF and recorded it.
        //   3. A conventional `<selected-model-id>.gguf` in the cwd, so the
        //      hardware-detected model id maps to a discoverable filename.
        let model_path = std::env::var("AISH_LOCAL_MODEL_PATH").unwrap_or_else(|_| {
            let sel = crate::hwdetect::load_selection();
            if let Some(path) = sel.as_ref().and_then(|s| s.model_path.clone()) {
                path
            } else {
                let id = sel
                    .map(|s| s.model_id)
                    .unwrap_or_else(|| crate::hwdetect::DEFAULT_MODEL_ID.to_string());
                format!("{id}.gguf")
            }
        });

        let n_gpu_layers = std::env::var("AISH_LOCAL_N_GPU_LAYERS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);

        let backend =
            LlamaBackend::init().context("failed to initialize llama.cpp backend")?;

        Ok(Self {
            model_path: PathBuf::from(model_path),
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

    async fn model(&self) -> Result<&LlamaModel> {
        self.model
            .get_or_try_init(|| async {
                eprintln!("\x1b[2m  loading model from {}…\x1b[0m", self.model_path.display());

                let model_params = LlamaModelParams::default()
                    .with_n_gpu_layers(self.n_gpu_layers);

                let model = LlamaModel::load_from_file(&self.backend, &self.model_path, &model_params)
                    .map_err(|e| anyhow!("failed to load local model: {e}"))?;

                eprintln!("\x1b[2m  model ready\x1b[0m");
                Ok::<_, anyhow::Error>(model)
            })
            .await
    }

    pub async fn complete(&self, system: &str, history: &[Msg], _tools: &[ToolDef]) -> Result<Turn> {
        let model = self.model().await?;

        // Build the prompt from system + history.
        let mut prompt = format!("[INST] {system}\n\n");

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

        // Tokenize prompt.
        let tokens = model.str_to_token(&prompt, AddBos::Always)
            .context("failed to tokenize prompt")?;

        // Create context and batch.
        let ctx_params = LlamaContextParams::default()
            .with_n_ctx(Some(NonZeroU32::new(4096).unwrap()));
        let mut ctx = model.new_context(&self.backend, ctx_params)
            .context("failed to create inference context")?;

        let mut batch = LlamaBatch::new(512, 1);
        let last_token_idx = (tokens.len() - 1) as i32;
        for (i, &token) in tokens.iter().enumerate() {
            batch.add(token, i as i32, &[0], i as i32 == last_token_idx)
                .context("failed to add token to batch")?;
        }

        ctx.decode(&mut batch).context("failed to decode initial batch")?;

        // Generate output tokens with greedy sampling.
        let mut sampler = LlamaSampler::greedy();
        let mut output = String::new();
        let mut n_cur = batch.n_tokens();
        let max_tokens = 512; // Limit output length.

        for _ in 0..max_tokens {
            let next_token = sampler.sample(&ctx, batch.n_tokens() - 1);
            sampler.accept(next_token);

            // Check for end-of-sequence.
            if model.is_eog_token(next_token) {
                break;
            }

            // Decode token to bytes, then lossily to UTF-8 and add to output.
            // llama-cpp-2's safe `token_to_piece` requires a stateful
            // `encoding_rs` decoder; to avoid pulling in that dependency we use
            // the raw byte API with a generous per-token buffer (a single token
            // piece is only a few bytes) and decode lossily — adequate for the
            // experimental local backend.
            let token_bytes = model
                .token_to_piece_bytes(next_token, 32, false, None)
                .unwrap_or_default();
            output.push_str(&String::from_utf8_lossy(&token_bytes));

            // Prepare next batch.
            batch.clear();
            batch.add(next_token, n_cur, &[0], true)
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
            truncated_text: output.len() >= max_tokens as usize,
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

    pub async fn complete(&self, _system: &str, _history: &[Msg], _tools: &[ToolDef]) -> Result<Turn> {
        unreachable!()
    }
}
