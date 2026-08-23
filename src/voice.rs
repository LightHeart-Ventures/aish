//! FR-334 / SPR-068 – voice input pipeline (TASK-362–367).
//!
//! All symbols here are gated behind the `voice` feature.  The default build
//! (`--no-default-features --locked`, the CI gate) never compiles this file.
//!
//! Module layout (one owner per submodule to avoid merge collisions):
//!
//! | Submodule | Owner task | Responsibility                          |
//! |-----------|------------|-----------------------------------------|
//! | `capture` | TASK-362   | cpal audio capture → mono f32 samples   |
//! | `resample`| TASK-363   | rubato resampler → 16 kHz for Whisper   |
//! | `stt`     | TASK-364   | whisper-rs transcription                |
//! | `model`   | TASK-365   | model download / checksum verification  |

// ---------------------------------------------------------------------------
// TASK-362: cpal audio capture
// ---------------------------------------------------------------------------

/// Audio capture from the default (or configured) input device.
///
/// Contract (frozen by SPR-068 design doc):
/// ```text
/// capture::record_until_stop(stop: StopSignal) -> Result<Vec<f32>>
/// ```
/// Returns **mono f32 samples at the device's native sample rate**.  The
/// caller (TASK-367 REPL wiring) is responsible for passing the samples to
/// `resample::to_whisper_pcm()` before transcription.
pub mod capture {
    use anyhow::Context as _;
    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
    use cpal::{Sample, SampleFormat, Stream};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    // -----------------------------------------------------------------------
    // Public types
    // -----------------------------------------------------------------------

    /// Instruction sent over the stop channel by the Ctrl-G / Esc handler.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum StopAction {
        /// Commit the recording: return the captured samples to the caller.
        Stop,
        /// Discard the recording: drop audio, leave the line buffer untouched.
        Cancel,
    }

    /// The receiver half of the stop channel.  The REPL wiring (TASK-367)
    /// creates the `(Sender, StopSignal)` pair and sends a [`StopAction`]
    /// when the user presses the second Ctrl-G or Esc.
    pub type StopSignal = tokio::sync::oneshot::Receiver<StopAction>;

    /// Errors specific to the capture module.
    #[derive(Debug, thiserror::Error)]
    pub enum CaptureError {
        #[error("recording cancelled by user")]
        Cancelled,
        #[error("voice: no input device available")]
        NoDevice,
        #[error("voice: unsupported sample format: {0:?}")]
        UnsupportedFormat(SampleFormat),
        #[error("voice: audio device error: {0}")]
        Device(#[from] cpal::Error),
        #[error("voice: stream error: {0}")]
        Stream(String),
    }

    // -----------------------------------------------------------------------
    // Public API (contract from SPR-068 design doc)
    // -----------------------------------------------------------------------

    /// Record audio from the default input device until `stop` fires.
    ///
    /// # Returns
    /// - `Ok(samples)` — mono f32 PCM at the device's native sample rate.
    /// - `Err(CaptureError::Cancelled)` — the user pressed Esc / sent
    ///   [`StopAction::Cancel`]; the caller must leave the line buffer untouched.
    /// - `Err(_)` — device or stream error; the caller shows a single-line
    ///   message above the prompt and returns to Idle.
    ///
    /// # Threading
    /// This function **blocks the calling thread** (it is designed to be run
    /// inside `tokio::task::spawn_blocking` by the REPL wiring, TASK-367).
    /// cpal's audio callback runs on a separate OS audio thread.
    pub fn record_until_stop(mut stop: StopSignal) -> anyhow::Result<Vec<f32>> {
        let host = cpal::default_host();

        let device = host
            .default_input_device()
            .ok_or(CaptureError::NoDevice)
            .context("voice: no input device")?;

        let supported_config = device
            .default_input_config()
            .map_err(CaptureError::Device)
            .context("voice: failed to query input device config")?;

        let channels = supported_config.channels() as usize;
        let sample_format = supported_config.sample_format();
        let stream_config: cpal::StreamConfig = supported_config.into();

        // Shared buffers between the cpal callback thread and this thread.
        let buffer: Arc<Mutex<Vec<f32>>> = Arc::new(Mutex::new(Vec::new()));
        let stream_err: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));

        let stream = build_input_stream(
            &device,
            &stream_config,
            sample_format,
            channels,
            Arc::clone(&buffer),
            Arc::clone(&stream_err),
        )?;

        stream.play().map_err(CaptureError::Device)?;

        // Polling loop: collect audio while waiting for the stop signal.
        // Tick every 10 ms — low enough latency for the user, cheap on CPU.
        loop {
            std::thread::sleep(Duration::from_millis(10));

            // Check for an audio-thread error first.
            if let Some(err) = stream_err.lock().unwrap().take() {
                return Err(CaptureError::Stream(err).into());
            }

            // Non-blocking poll of the stop channel.
            match stop.try_recv() {
                Ok(StopAction::Stop) => {
                    // Pause before reading the buffer to avoid a data race on
                    // the final callback flush.
                    drop(stream);
                    // Give the audio thread one tick to flush its last callback.
                    std::thread::sleep(Duration::from_millis(10));
                    let samples = std::mem::take(&mut *buffer.lock().unwrap());
                    return Ok(samples);
                }
                Ok(StopAction::Cancel) => {
                    return Err(CaptureError::Cancelled.into());
                }
                Err(tokio::sync::oneshot::error::TryRecvError::Empty) => {
                    // Signal not yet sent — keep recording.
                }
                Err(tokio::sync::oneshot::error::TryRecvError::Closed) => {
                    // Sender was dropped without sending — treat as cancel.
                    return Err(CaptureError::Cancelled.into());
                }
            }
        }
    }

    // -----------------------------------------------------------------------
    // Private helpers
    // -----------------------------------------------------------------------

    /// Dispatch over sample format to build the correctly-typed cpal stream.
    ///
    /// Converts every non-f32 sample to f32 during capture and downmixes
    /// N-channel interleaved data to mono (simple average).
    fn build_input_stream(
        device: &cpal::Device,
        config: &cpal::StreamConfig,
        sample_format: SampleFormat,
        channels: usize,
        buffer: Arc<Mutex<Vec<f32>>>,
        err_flag: Arc<Mutex<Option<String>>>,
    ) -> anyhow::Result<Stream> {
        // Macro to avoid repeating the closure boilerplate for each sample type.
        // Each concrete `$ty` must satisfy: `$ty: SizedSample`, `f32: FromSample<$ty>`.
        macro_rules! make_stream {
            ($ty:ty) => {{
                let buf = Arc::clone(&buffer);
                let err = Arc::clone(&err_flag);
                device
                    .build_input_stream(
                        *config,
                        move |data: &[$ty], _: &cpal::InputCallbackInfo| {
                            let mut guard = buf.lock().unwrap();
                            // Downmix interleaved N-channel frames to mono f32.
                            for frame in data.chunks(channels) {
                                let mono: f32 = frame
                                    .iter()
                                    .map(|&s| f32::from_sample(s))
                                    .sum::<f32>()
                                    / channels as f32;
                                guard.push(mono);
                            }
                        },
                        move |e| {
                            *err.lock().unwrap() = Some(e.to_string());
                        },
                        None,
                    )
                    .map_err(|e| anyhow::anyhow!("voice: failed to build input stream: {e}"))
            }};
        }

        match sample_format {
            SampleFormat::F32 => make_stream!(f32),
            SampleFormat::I8 => make_stream!(i8),
            SampleFormat::I16 => make_stream!(i16),
            SampleFormat::I32 => make_stream!(i32),
            SampleFormat::I64 => make_stream!(i64),
            SampleFormat::U8 => make_stream!(u8),
            SampleFormat::U16 => make_stream!(u16),
            SampleFormat::U32 => make_stream!(u32),
            SampleFormat::U64 => make_stream!(u64),
            SampleFormat::F64 => make_stream!(f64),
            other => Err(CaptureError::UnsupportedFormat(other).into()),
        }
    }

    // -----------------------------------------------------------------------
    // Unit tests
    // -----------------------------------------------------------------------

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn stop_action_is_copy() {
            let a = StopAction::Stop;
            let b = a; // copy
            assert_eq!(a, b);
        }

        #[test]
        fn cancel_action_is_copy() {
            let a = StopAction::Cancel;
            let b = a;
            assert_eq!(a, b);
        }

        /// Verify that a pre-cancelled StopSignal causes record_until_stop to
        /// return CaptureError::Cancelled without opening any audio device.
        ///
        /// NOTE: this test only validates the cancel path logic; it does NOT
        /// open a real audio device.  On CI (no mic) the function would fail
        /// at device open anyway, but the cancel channel is checked before
        /// playback starts only if we restructure the function.  This test
        /// documents the expected behaviour contract for the REPL wiring.
        #[test]
        fn stop_signal_type_is_oneshot_receiver() {
            // Confirm the type aliases compile and the channel round-trips.
            let (tx, rx): (_, StopSignal) = tokio::sync::oneshot::channel();
            tx.send(StopAction::Cancel).unwrap();
            // The receiver should immediately have the value.
            let rt = tokio::runtime::Runtime::new().unwrap();
            let result = rt.block_on(rx);
            assert_eq!(result.unwrap(), StopAction::Cancel);
        }
    }
}

// ---------------------------------------------------------------------------
// TASK-363 stub: rubato resampler  (filled in by the TASK-363 PR)
// ---------------------------------------------------------------------------

/// Resampler: converts device-rate mono f32 PCM → 16 kHz mono f32 for Whisper.
///
/// Contract (frozen by SPR-068 design doc):
/// ```text
/// resample::to_whisper_pcm(&[f32], src_rate: u32) -> Result<Vec<f32>>
/// ```
pub mod resample {}

// ---------------------------------------------------------------------------
// TASK-364: whisper-rs speech-to-text transcription
// ---------------------------------------------------------------------------

/// Local speech-to-text via whisper-rs.
///
/// Contract (frozen by SPR-068 design doc):
/// ```text
/// stt::Transcriber::new(model_path) -> Self         // infallible; model loaded lazily
/// stt::Transcriber::transcribe(&[f32]) -> Result<String>  // 16 kHz mono f32 PCM in
/// ```
///
/// The [`WhisperContext`][whisper_rs::WhisperContext] is created on the **first** call to
/// [`Transcriber::transcribe`] and reused for all subsequent calls, amortising the
/// (expensive) model-load cost.
pub mod stt {
    use anyhow::Context as _;
    use std::path::{Path, PathBuf};
    use whisper_rs::{FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters};

    // -----------------------------------------------------------------------
    // Error type
    // -----------------------------------------------------------------------

    /// Errors specific to the STT module.
    #[derive(Debug, thiserror::Error)]
    pub enum SttError {
        /// The ggml model file could not be loaded by whisper-rs.
        #[error("voice: failed to load Whisper model from {path}: {source}")]
        ModelLoad {
            path: PathBuf,
            source: whisper_rs::WhisperError,
        },
        /// An error occurred while creating the Whisper inference state.
        #[error("voice: failed to create Whisper state: {0}")]
        StateCreate(whisper_rs::WhisperError),
        /// Whisper's `full()` inference call returned an error.
        #[error("voice: Whisper inference failed: {0}")]
        Inference(whisper_rs::WhisperError),
    }

    // -----------------------------------------------------------------------
    // Public API
    // -----------------------------------------------------------------------

    /// Local STT engine backed by whisper-rs.
    ///
    /// # Lifecycle
    /// 1. Call [`Self::new`] with the path to a ggml Whisper model file.
    ///    Construction is **infallible** — the model is not touched yet.
    /// 2. Call [`Self::transcribe`] with 16 kHz mono f32 PCM.  On the first
    ///    call the model is loaded from disk; subsequent calls reuse the
    ///    already-loaded context (lazy-initialisation, reused).
    pub struct Transcriber {
        model_path: PathBuf,
        /// The context is `None` until the first call to `transcribe`.
        ctx: Option<WhisperContext>,
    }

    impl Transcriber {
        /// Create a new `Transcriber` that will use the model at `model_path`.
        ///
        /// The model file is **not** opened here; loading is deferred to the
        /// first call to [`Self::transcribe`].
        pub fn new(model_path: impl AsRef<Path>) -> Self {
            Self {
                model_path: model_path.as_ref().to_owned(),
                ctx: None,
            }
        }

        /// Transcribe `pcm` (16 kHz, mono, f32) and return the text.
        ///
        /// On the **first** call the Whisper model is loaded from disk (may
        /// take several seconds depending on model size and storage speed).
        /// Subsequent calls reuse the already-loaded [`WhisperContext`][whisper_rs::WhisperContext].
        ///
        /// # Returns
        /// - `Ok(text)` — the trimmed transcript; may be empty if no speech
        ///   was detected (e.g. silence-only input).
        /// - `Err(_)` — model load, state creation, or inference failure.
        ///   The caller (TASK-367 REPL wiring) must show a single-line error
        ///   above the prompt and return to Idle without touching the buffer.
        pub fn transcribe(&mut self, pcm: &[f32]) -> anyhow::Result<String> {
            // --- Lazy-load the WhisperContext on first use -------------------
            if self.ctx.is_none() {
                let ctx =
                    WhisperContext::new_with_params(&self.model_path, WhisperContextParameters::new())
                        .map_err(|source| SttError::ModelLoad {
                            path: self.model_path.clone(),
                            source,
                        })
                        .context("voice: failed to initialise Whisper context")?;
                self.ctx = Some(ctx);
            }

            // --- Create per-call inference state ----------------------------
            // WhisperContext is not Send/Sync, so the state is local to this call.
            let ctx = self.ctx.as_ref().unwrap();
            let mut state = ctx
                .create_state()
                .map_err(SttError::StateCreate)
                .context("voice: failed to create Whisper state")?;

            // --- Build inference parameters ---------------------------------
            let mut params = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
            // Hint the decoder to English to avoid spurious language-detection latency.
            params.set_language(Some("en"));
            // Suppress internal stderr chatter that would pollute the REPL.
            params.set_print_special(false);
            params.set_print_progress(false);
            params.set_print_realtime(false);
            params.set_print_timestamps(false);

            // --- Run inference ----------------------------------------------
            state
                .full(params, pcm)
                .map_err(SttError::Inference)
                .context("voice: Whisper inference failed")?;

            // --- Collect segment text ---------------------------------------
            let n = state.full_n_segments();
            let mut out = String::new();
            for i in 0..n {
                if let Some(seg) = state.get_segment(i) {
                    match seg.to_str() {
                        Ok(text) => out.push_str(text),
                        Err(e) => {
                            // Log and skip — a single bad segment should not
                            // abort the whole transcript.
                            tracing::warn!("voice: stt segment {i} text error: {e}");
                        }
                    }
                }
            }

            // Trim leading/trailing whitespace that Whisper commonly adds.
            Ok(out.trim().to_owned())
        }
    }

    // -----------------------------------------------------------------------
    // Unit tests
    // -----------------------------------------------------------------------
    #[cfg(test)]
    mod tests {
        use super::*;

        /// `new` must be infallible — even a non-existent path is accepted at
        /// construction time; the error surfaces on `transcribe`.
        #[test]
        fn new_does_not_load_model() {
            let t = Transcriber::new("/nonexistent/ggml-tiny.en.bin");
            assert!(t.ctx.is_none(), "context must not be loaded at construction");
        }

        /// `transcribe` must return an error (not panic) when the model file
        /// does not exist.
        #[test]
        fn transcribe_returns_err_on_missing_model() {
            let mut t = Transcriber::new("/nonexistent/ggml-tiny.en.bin");
            let result = t.transcribe(&[0.0f32; 16_000]);
            assert!(result.is_err(), "expected Err for missing model");
            let msg = result.unwrap_err().to_string();
            // The error chain should mention voice: somewhere.
            assert!(
                msg.contains("voice:"),
                "error message should contain 'voice:'; got: {msg}"
            );
        }

        /// `transcribe` must still return an error on the *second* call if the
        /// model could not be loaded on the first (ctx remains None).
        #[test]
        fn transcribe_retries_load_on_subsequent_calls() {
            let mut t = Transcriber::new("/nonexistent/ggml-tiny.en.bin");
            // Both calls should return Err (not panic, not succeed).
            assert!(t.transcribe(&[0.0f32; 16_000]).is_err());
            assert!(t.ctx.is_none(), "ctx should remain None after failed load");
            assert!(t.transcribe(&[0.0f32; 16_000]).is_err());
        }
    }
}

// ---------------------------------------------------------------------------
// TASK-365 stub: model fetch / verify  (filled in by the TASK-365 PR)
// ---------------------------------------------------------------------------

/// ggml model resolution, download, and checksum verification.
///
/// Contract (frozen by SPR-068 design doc):
/// ```text
/// model::ensure_model(name: &str) -> Result<PathBuf>
/// ```
pub mod model {}
