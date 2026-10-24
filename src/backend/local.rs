//! Local inference backend using llama.cpp for GGUF model support.
//!
//! This module provides lightweight, embedded inference via llama-cpp-2 bindings.
//! Models are lazy-loaded on first use. The model is chosen by the hardware-aware
//! selector (see `crate::hwdetect`), with overrides via environment variables.
//!
//! Environment variables:
//! - `AISH_LOCAL_MODEL_PATH`: Full path to a .gguf model file (operator pin)
//! - `AISH_LOCAL_MODEL_ID`: Model id from the selection ladder (e.g., "qwen2.5-0.5b-instruct")
//! - `AISH_LOCAL_N_GPU_LAYERS`: Number of layers to offload to GPU (default: 999
//!   on macOS, where every Mac with a GGUF-sized model has a Metal device; 0
//!   elsewhere, where a GPU is not a given)
//! - `AISH_LOCAL_N_CTX`: Context window in tokens (default: 8192, clamped 512..=131072)
//! - `AISH_LOCAL_MAX_OUTPUT_TOKENS`: Tokens reserved for the reply (default: 1024, clamped 16..=8192)
//! - `AISH_LOCAL_BATCH_SIZE`: Prompt tokens per `decode` call, and the context's
//!   `n_batch`/`n_ubatch` (default: 512, clamped 32..=4096)
//!
//! Prompt feeding is planned by [`super::local_plan`]: prompts larger than one
//! batch are decoded in chunks, and prompts larger than the context window are
//! trimmed from the middle (system prompt and most recent turns survive).
//!
//! # Why the context outlives the turn
//!
//! The first cut created a fresh `LlamaContext` per [`LocalBackend::complete`]
//! call, so every turn re-prefilled the *entire* conversation from token 0 —
//! O(prompt) GPU work per message, growing with the session. Since a turn's
//! prompt is almost always the previous turn's prompt plus a suffix, the
//! context (and its KV cache) is now cached in [`LocalBackend::session`] and
//! reused: the common token prefix stays resident, positions at and beyond the
//! first divergence are evicted with `clear_kv_cache_seq`, and only the new
//! tail is decoded. On a 32B model that turns a multi-second prefill per turn
//! into a few hundred tokens of work.

#[cfg(feature = "local")]
use anyhow::{Context, anyhow};

#[cfg(feature = "local")]
use super::local_plan;
use anyhow::Result;
#[cfg(feature = "local")]
use llama_cpp_2::{
    context::{LlamaContext, params::LlamaContextParams},
    llama_backend::LlamaBackend,
    llama_batch::LlamaBatch,
    model::{LlamaModel, params::LlamaModelParams},
    sampling::LlamaSampler,
    token::LlamaToken,
};
#[cfg(feature = "local")]
use std::num::NonZeroU32;
#[cfg(feature = "local")]
use std::path::PathBuf;
#[cfg(feature = "local")]
use std::sync::Mutex;
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
    /// The loaded weights, leaked to `'static` so [`Session::ctx`] — which
    /// borrows the model — can outlive the turn that created it.
    model: OnceCell<&'static LlamaModel>,
    backend: LlamaBackend,
    /// The live inference context and the prompt tokens currently resident in
    /// its KV cache. `None` until the first turn, and rebuilt whenever a
    /// context-shaping tunable changes.
    session: Mutex<Option<Session>>,
}

/// A warm `LlamaContext` plus the prompt tokens its KV cache holds at positions
/// `0..cached.len()`. Reused across turns so a conversation prefills once.
#[cfg(feature = "local")]
struct Session {
    ctx: LlamaContext<'static>,
    /// Exactly the tokens resident in the KV cache, in position order.
    cached: Vec<LlamaToken>,
    /// Tunables this context was built with; a change forces a rebuild.
    n_ctx: usize,
    n_batch: usize,
}

/// Default GPU offload. On macOS every host that can hold a GGUF has a Metal
/// device, and leaving layers on the CPU is the single largest local-inference
/// footgun (a 32B model runs ~an order of magnitude slower). Elsewhere a GPU
/// is not a given, so the conservative CPU-only default stands.
#[cfg(feature = "local")]
const DEFAULT_N_GPU_LAYERS: u32 = if cfg!(target_os = "macos") { 999 } else { 0 };

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
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(DEFAULT_N_GPU_LAYERS);

        // F3: prove the model is resolvable BEFORE acquiring any device
        // resources. `LlamaBackend::init()` compiles the Metal libraries and
        // reserves a working set (~0.3s on an M-series Mac); doing that for a
        // selection that can never load is pure waste — and it used to happen
        // on every `:backend local` with an unmappable model id.
        let sel = crate::hwdetect::load_selection();
        crate::modelfetch::preflight_local_model(
            sel.as_ref(),
            pinned_path.as_ref().and_then(|p| p.to_str()),
        )?;

        // Must precede `init()`: the Metal device probe logs during that call.
        super::llama_log::init_backend_logging();
        let backend = LlamaBackend::init().context("failed to initialize llama.cpp backend")?;

        Ok(Self {
            pinned_path,
            n_gpu_layers,
            model: OnceCell::new(),
            backend,
            session: Mutex::new(None),
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
                    "pinned local model file not found: {} (AISH_LOCAL_MODEL_PATH). {}",
                    p.display(),
                    crate::modelfetch::LOCAL_RECOVERY_HINT
                ));
            }
            return Ok(p.clone());
        }
        let sel = crate::hwdetect::load_selection();
        crate::modelfetch::ensure_model_file(sel.as_ref())
            .await
            .context("failed to resolve local model file")
    }

    async fn model(&self) -> Result<&'static LlamaModel> {
        self.model
            .get_or_try_init(|| async {
                let path = self.resolve_path().await?;
                eprintln!("\x1b[2m  loading model from {}…\x1b[0m", path.display());

                let model_params = LlamaModelParams::default().with_n_gpu_layers(self.n_gpu_layers);

                let model = LlamaModel::load_from_file(&self.backend, &path, &model_params)
                    .map_err(|e| anyhow!("failed to load local model: {e}"))?;

                eprintln!("\x1b[2m  model ready\x1b[0m");
                // Leak the weights. The cached `LlamaContext` borrows the model
                // and must outlive the turn that built it; a `&'static` model is
                // what makes that sound. `LocalBackend` is process-scoped and the
                // model is loaded at most once, so nothing is reclaimed that
                // would otherwise have been freed.
                Ok::<_, anyhow::Error>(&*Box::leak(Box::new(model)))
            })
            .await
            .copied()
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
        // Chunk size for prompt feeding *and* the context's `n_batch`/`n_ubatch`
        // (set below), so a larger value is actually honoured by `decode`
        // instead of being rejected against llama.cpp's 512 default.
        let batch_capacity = local_plan::env_usize(
            "AISH_LOCAL_BATCH_SIZE",
            local_plan::DEFAULT_BATCH_CAPACITY,
            32,
            4096,
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

        // Materialize the surviving prompt in position order. The plan's chunks
        // may skip a dropped middle, so this is the exact token sequence the KV
        // cache must hold at positions `0..kept` — and therefore the sequence to
        // compare against what is already resident.
        let mut kept_tokens: Vec<LlamaToken> = Vec::with_capacity(plan.kept);
        for chunk in &plan.chunks {
            kept_tokens.extend_from_slice(&tokens[chunk.src_start..chunk.src_start + chunk.len]);
        }

        let mut guard = self
            .session
            .lock()
            .map_err(|_| anyhow!("local backend: inference session mutex poisoned"))?;

        // Rebuild the context only when there isn't one, or when a
        // context-shaping tunable changed under us. Otherwise the warm KV cache
        // survives into this turn, which is the whole point.
        let stale = guard
            .as_ref()
            .is_none_or(|s| s.n_ctx != n_ctx || s.n_batch != plan.batch_capacity);
        if stale {
            // Drop the old context before allocating its replacement so two KV
            // caches are never resident at once (they are the large allocation).
            *guard = None;
            let ctx_params = LlamaContextParams::default()
                .with_n_ctx(NonZeroU32::new(n_ctx as u32))
                // llama.cpp defaults both of these to 512 and *rejects* a decode
                // wider than `n_batch`. Setting them is what lets a raised
                // `AISH_LOCAL_BATCH_SIZE` actually reach the GPU as one dispatch
                // instead of being silently capped at the default.
                .with_n_batch(plan.batch_capacity as u32)
                .with_n_ubatch(plan.batch_capacity as u32);
            let ctx = model
                .new_context(&self.backend, ctx_params)
                .context("failed to create inference context")?;
            *guard = Some(Session {
                ctx,
                cached: Vec::new(),
                n_ctx,
                n_batch: plan.batch_capacity,
            });
        }
        let session = guard
            .as_mut()
            .expect("session is Some: freshly built above or already present");

        // Reuse the longest common prefix of the resident cache and this prompt.
        // Sampling needs logits, which only a decoded token produces, so a
        // prompt that matches the cache outright still re-decodes its last token.
        let mut reuse = local_plan::common_prefix_len(&session.cached, &kept_tokens);
        if reuse == kept_tokens.len() {
            reuse = kept_tokens.len() - 1;
        }
        // Evict from the first divergence onward. This also drops the previous
        // turn's generated tokens, which live past the prompt in the same seq.
        session
            .ctx
            .clear_kv_cache_seq(Some(0), Some(reuse as u32), None)
            .context("failed to evict diverged KV cache entries")?;
        session.cached.truncate(reuse);

        // Feed only the new tail, chunked so no single decode exceeds the batch.
        // Only the very last token needs logits — that's where sampling starts.
        let feed = &kept_tokens[reuse..];
        let mut batch = LlamaBatch::new(plan.batch_capacity, 1);
        let mut fed = 0usize;
        while fed < feed.len() {
            let take = plan.batch_capacity.min(feed.len() - fed);
            batch.clear();
            for i in 0..take {
                let wants_logits = fed + i + 1 == feed.len();
                batch
                    .add(feed[fed + i], (reuse + fed + i) as i32, &[0], wants_logits)
                    .context("failed to add prompt token to batch")?;
            }
            session
                .ctx
                .decode(&mut batch)
                .context("failed to decode prompt chunk")?;
            fed += take;
        }
        session.cached.extend_from_slice(feed);

        // Generate output tokens with greedy sampling.
        let mut sampler = LlamaSampler::greedy();
        let mut output = String::new();
        let mut generated = 0usize;

        // Sampling continues at the position after the last *kept* prompt token
        // (positions are renumbered over the kept tokens when trimming), and
        // each generated token advances one slot.
        for n_cur in (plan.next_position() as i32..).take(max_tokens) {
            let next_token = sampler.sample(&session.ctx, batch.n_tokens() - 1);
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

            // Prepare next batch. The generated token now occupies a KV slot at
            // `n_cur`, so record it: next turn's prefix match must compare
            // against everything resident, not just the prompt.
            batch.clear();
            batch
                .add(next_token, n_cur, &[0], true)
                .context("failed to add next token to batch")?;

            session
                .ctx
                .decode(&mut batch)
                .context("failed to decode batch")?;
            // Recorded only after a successful decode: `cached` must never claim
            // a token the KV cache does not actually hold, or the next turn
            // would reuse a position that was never populated.
            session.cached.push(next_token);
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
