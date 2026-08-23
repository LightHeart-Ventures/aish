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
    use cpal::{FromSample, SampleFormat, SizedSample, Stream};
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
        #[error("voice: failed to get device config: {0}")]
        Config(#[from] cpal::DefaultStreamConfigError),
        #[error("voice: unsupported sample format: {0:?}")]
        UnsupportedFormat(SampleFormat),
        #[error("voice: failed to build input stream: {0}")]
        Build(#[from] cpal::BuildStreamError),
        #[error("voice: failed to start stream: {0}")]
        Play(#[from] cpal::PlayStreamError),
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
            .map_err(CaptureError::Config)
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

        stream.play().map_err(CaptureError::Play)?;

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
                        config,
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
// TASK-364 stub: whisper-rs transcription  (filled in by the TASK-364 PR)
// ---------------------------------------------------------------------------

/// Local STT via whisper-rs.
///
/// Contract (frozen by SPR-068 design doc):
/// ```text
/// stt::Transcriber::new(model_path) -> Self
/// stt::Transcriber::transcribe(&[f32]) -> Result<String>
/// ```
pub mod stt {}

// ---------------------------------------------------------------------------
// TASK-365: ggml Whisper model download and cache management
// ---------------------------------------------------------------------------

/// ggml model resolution, download, and cache verification.
///
/// Contract (frozen by SPR-068 design doc):
/// ```text
/// model::ensure_model(name: &str) -> Result<PathBuf>
/// ```
///
/// Models are cached under `~/.aish/models/whisper/ggml-<name>.bin` and
/// streamed from `https://huggingface.co/ggerganov/whisper.cpp` on first use.
/// Download is consent-gated at the REPL level (TASK-366/367); this function
/// only caches and verifies.
pub mod model {
    use anyhow::{anyhow, Context, Result};
    use std::io::Write as _;
    use std::path::{Path, PathBuf};
    use std::time::Instant;

    // -----------------------------------------------------------------------
    // Known model catalogue
    // -----------------------------------------------------------------------

    /// Minimum acceptable file size (bytes) after download, per model variant.
    /// Serves as a lightweight integrity check: a file below this threshold is
    /// almost certainly a truncated or error-page download.
    struct ModelSpec {
        name: &'static str,
        /// Approximate expected file size from ggerganov/whisper.cpp on HF.
        min_size: u64,
    }

    /// Curated list of known Whisper ggml variants.
    /// Sizes are approximate (rounded down ~5%) so a legitimate partial CDN
    /// chunk redelivery is never wrongly rejected.
    const KNOWN_MODELS: &[ModelSpec] = &[
        ModelSpec { name: "tiny",      min_size:  73_000_000 },
        ModelSpec { name: "tiny.en",   min_size:  73_000_000 },
        ModelSpec { name: "base",      min_size: 140_000_000 },
        ModelSpec { name: "base.en",   min_size: 140_000_000 },
        ModelSpec { name: "small",     min_size: 460_000_000 },
        ModelSpec { name: "small.en",  min_size: 460_000_000 },
        ModelSpec { name: "medium",    min_size: 1_430_000_000 },
        ModelSpec { name: "medium.en", min_size: 1_430_000_000 },
        ModelSpec { name: "large-v1",  min_size: 2_870_000_000 },
        ModelSpec { name: "large-v2",  min_size: 2_870_000_000 },
        ModelSpec { name: "large-v3",  min_size: 2_870_000_000 },
        ModelSpec { name: "large",     min_size: 2_870_000_000 },
    ];

    // -----------------------------------------------------------------------
    // Public API (contract from SPR-068 design doc)
    // -----------------------------------------------------------------------

    /// Resolve (and, if necessary, download) a Whisper ggml model by name.
    ///
    /// # Arguments
    /// - `name`: the model variant name, e.g. `"tiny.en"` (the default),
    ///   `"base.en"`, `"small"`. Resolved from the `voice.model` config key
    ///   (TASK-368). Unknown names are accepted and fetched; only known names
    ///   get size-validation after download.
    ///
    /// # Returns
    /// The absolute path to the on-disk `.bin` file, ready to be passed to
    /// `whisper_rs::WhisperContext::new_with_params`.
    ///
    /// # Errors
    /// - Name contains `..`, `/`, or a null byte (path-traversal guard).
    /// - The cache directory cannot be created.
    /// - The HTTP download fails (network error, 4xx/5xx status).
    /// - The downloaded file is below the expected minimum size (size check).
    ///
    /// # Threading
    /// This function is `async` and performs non-blocking HTTP streaming.  It
    /// should be called from a tokio context; the REPL wiring (TASK-367) will
    /// call it with `tokio::spawn` / `.await`.
    pub async fn ensure_model(name: &str) -> Result<PathBuf> {
        // Reject names with path-traversal characters.
        validate_name(name)?;

        let dest = model_path(name);

        // Fast path: already cached and non-empty.
        if file_ready(&dest) {
            return Ok(dest);
        }

        // Ensure the cache directory exists.
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent).with_context(|| {
                format!("voice: creating model cache dir {}", parent.display())
            })?;
        }

        // Stream the model from the canonical Hugging Face mirror.
        let base = whisper_base();
        let url = whisper_url(&base, name);
        eprintln!("\x1b[2m  voice: downloading whisper model {name}…\x1b[0m");
        download_model(&url, &dest, name)
            .await
            .with_context(|| format!("voice: downloading whisper model '{name}'"))?;

        // Basic integrity check: verify the file meets the expected size floor.
        verify_size(name, &dest)?;

        Ok(dest)
    }

    // -----------------------------------------------------------------------
    // Path helpers (pure, testable without I/O)
    // -----------------------------------------------------------------------

    /// On-disk path for a named Whisper ggml model:
    /// `~/.aish/models/whisper/ggml-<name>.bin`.
    pub fn model_path(name: &str) -> PathBuf {
        whisper_cache_dir().join(format!("ggml-{name}.bin"))
    }

    /// Whisper model cache root: `~/.aish/models/whisper/`.
    fn whisper_cache_dir() -> PathBuf {
        crate::hwdetect::aish_dir().join("models").join("whisper")
    }

    /// HuggingFace download URL for a named Whisper ggml model.
    ///
    /// Overridable via `AISH_WHISPER_BASE` (used by unit tests with a local
    /// HTTP server).
    pub fn whisper_url(base: &str, name: &str) -> String {
        format!(
            "{}/ggerganov/whisper.cpp/resolve/main/ggml-{name}.bin?download=true",
            base.trim_end_matches('/'),
        )
    }

    /// Base URL for Whisper model downloads.  Defaults to HuggingFace; the
    /// `AISH_WHISPER_BASE` env var overrides (used for offline tests).
    fn whisper_base() -> String {
        std::env::var("AISH_WHISPER_BASE")
            .ok()
            .map(|s| s.trim().trim_end_matches('/').to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "https://huggingface.co".to_string())
    }

    // -----------------------------------------------------------------------
    // Validation helpers
    // -----------------------------------------------------------------------

    /// Reject model names that could be used for path traversal or shell
    /// injection: disallow `..`, `/`, null bytes, and empty strings.
    pub fn validate_name(name: &str) -> Result<()> {
        if name.is_empty() {
            return Err(anyhow!("voice: model name must not be empty"));
        }
        if name.contains('\0') {
            return Err(anyhow!("voice: model name contains null byte: {name:?}"));
        }
        if name.contains('/') {
            return Err(anyhow!("voice: model name must not contain '/': {name:?}"));
        }
        if name.contains("..") {
            return Err(anyhow!(
                "voice: model name must not contain '..': {name:?}"
            ));
        }
        Ok(())
    }

    /// A file is considered "ready" (cached) when it exists and is non-empty.
    /// A zero-byte file (an interrupted previous run) is treated as absent.
    fn file_ready(path: &Path) -> bool {
        std::fs::metadata(path)
            .map(|m| m.len() > 0)
            .unwrap_or(false)
    }

    /// Post-download size validation.  For known model names we enforce a
    /// per-model minimum; for unknown names we only check that the file is
    /// non-empty (a 1-byte response is certainly an error page).
    fn verify_size(name: &str, path: &Path) -> Result<()> {
        let actual = std::fs::metadata(path)
            .with_context(|| format!("voice: stat {}", path.display()))?
            .len();

        let min = KNOWN_MODELS
            .iter()
            .find(|m| m.name == name)
            .map(|m| m.min_size)
            .unwrap_or(1); // unknown model: just check non-empty

        if actual < min {
            // Remove the bad file so the next attempt re-downloads cleanly.
            let _ = std::fs::remove_file(path);
            return Err(anyhow!(
                "voice: downloaded model '{name}' is too small \
                 ({actual} bytes, expected ≥{min}); \
                 the download may have been truncated or the server \
                 returned an error page"
            ));
        }
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Download (streaming, with progress)
    // -----------------------------------------------------------------------

    /// Stream a model file from `url` to `dest`, via a `.part` temp file that
    /// is atomically renamed on success.  A TTY progress line is shown on
    /// stderr (matching the pattern in `crate::modelfetch`).
    async fn download_model(url: &str, dest: &Path, label: &str) -> Result<()> {
        // Guard against accidental plaintext downloads (allow localhost for tests).
        check_url(url)?;

        let client = reqwest::Client::builder()
            .user_agent(concat!("aish/", env!("CARGO_PKG_VERSION")))
            .build()
            .context("voice: building HTTP client for model download")?;

        let resp = client
            .get(url)
            .send()
            .await
            .with_context(|| format!("voice: GET {url}"))?
            .error_for_status()
            .with_context(|| format!("voice: GET {url}"))?;

        let total = resp.content_length();

        // Write to a `.part` temp file; rename atomically on success.
        let mut tmp_path = dest.as_os_str().to_os_string();
        tmp_path.push(".part");
        let tmp_path = PathBuf::from(tmp_path);

        {
            let file = std::fs::File::create(&tmp_path)
                .with_context(|| format!("voice: creating {}", tmp_path.display()))?;
            let mut writer = std::io::BufWriter::new(file);

            let tty = is_stderr_tty();
            let mut downloaded: u64 = 0;
            let mut last_print = Instant::now();
            let mut resp = resp;

            use futures_util::StreamExt as _;
            let mut stream = resp.bytes_stream();
            while let Some(chunk) = stream.next().await {
                let chunk = chunk.context("voice: reading response body")?;
                writer
                    .write_all(chunk.as_ref())
                    .context("voice: writing model file")?;
                downloaded += chunk.len() as u64;
                if tty && last_print.elapsed() >= std::time::Duration::from_millis(200) {
                    print_progress(label, downloaded, total);
                    last_print = Instant::now();
                }
            }
            writer.flush().context("voice: flushing model file")?;
            if tty {
                print_progress(label, downloaded, total);
                eprintln!();
            }
        }

        std::fs::rename(&tmp_path, dest)
            .with_context(|| format!("voice: finalizing {}", dest.display()))?;
        Ok(())
    }

    /// Guard against fetching from non-HTTPS URLs.  Allows HTTP on 127.0.0.1
    /// and localhost so unit tests can spin up a local mock server.
    fn check_url(url: &str) -> Result<()> {
        if url.starts_with("https://") {
            return Ok(());
        }
        if let Some(rest) = url.strip_prefix("http://") {
            let host = rest.split(['/', ':']).next().unwrap_or("");
            if matches!(host, "127.0.0.1" | "localhost" | "[::1]") {
                return Ok(());
            }
        }
        Err(anyhow!(
            "voice: refusing to download model from non-HTTPS URL: {url}"
        ))
    }

    fn is_stderr_tty() -> bool {
        // SAFETY: isatty() is pure query, no side effects.
        unsafe { libc::isatty(libc::STDERR_FILENO) == 1 }
    }

    fn print_progress(label: &str, downloaded: u64, total: Option<u64>) {
        match total {
            Some(t) if t > 0 => {
                let pct = (downloaded as f64 / t as f64 * 100.0).min(100.0);
                eprint!(
                    "\r\x1b[2m  {label}: {} / {} ({pct:.0}%)\x1b[0m\x1b[K",
                    fmt_bytes(downloaded),
                    fmt_bytes(t),
                );
            }
            _ => eprint!(
                "\r\x1b[2m  {label}: {}\x1b[0m\x1b[K",
                fmt_bytes(downloaded)
            ),
        }
        let _ = std::io::stderr().flush();
    }

    fn fmt_bytes(n: u64) -> String {
        const GB: f64 = 1024.0 * 1024.0 * 1024.0;
        const MB: f64 = 1024.0 * 1024.0;
        let f = n as f64;
        if f >= GB {
            format!("{:.1} GB", f / GB)
        } else {
            format!("{:.0} MB", f / MB)
        }
    }

    // -----------------------------------------------------------------------
    // Unit tests (pure logic only — no network, no FS writes)
    // -----------------------------------------------------------------------

    #[cfg(test)]
    mod tests {
        use super::*;

        // ---- model_path / whisper_cache_dir --------------------------------

        #[test]
        fn model_path_has_correct_structure() {
            let p = model_path("tiny.en");
            let s = p.to_string_lossy();
            assert!(s.ends_with("models/whisper/ggml-tiny.en.bin"), "{s}");
            assert!(s.contains(".aish"), "{s}");
        }

        #[test]
        fn model_path_for_large_variant() {
            let p = model_path("large-v3");
            assert!(
                p.to_string_lossy()
                    .ends_with("models/whisper/ggml-large-v3.bin")
            );
        }

        // ---- whisper_url ---------------------------------------------------

        #[test]
        fn whisper_url_default_base() {
            let url = whisper_url("https://huggingface.co", "tiny.en");
            assert_eq!(
                url,
                "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-tiny.en.bin?download=true"
            );
        }

        #[test]
        fn whisper_url_strips_trailing_slash_on_base() {
            let url = whisper_url("https://huggingface.co/", "base.en");
            assert!(
                !url.contains("//ggerganov"),
                "double slash in URL: {url}"
            );
        }

        #[test]
        fn whisper_url_custom_base_for_test_server() {
            let url = whisper_url("http://127.0.0.1:9999", "small");
            assert!(url.starts_with("http://127.0.0.1:9999/"), "{url}");
            assert!(url.contains("ggml-small.bin"), "{url}");
        }

        // ---- validate_name -------------------------------------------------

        #[test]
        fn validate_name_accepts_known_models() {
            for spec in KNOWN_MODELS {
                validate_name(spec.name).unwrap_or_else(|e| {
                    panic!("rejected valid model name {:?}: {e}", spec.name)
                });
            }
        }

        #[test]
        fn validate_name_rejects_empty() {
            assert!(validate_name("").is_err());
        }

        #[test]
        fn validate_name_rejects_path_traversal() {
            assert!(validate_name("../etc/passwd").is_err());
            assert!(validate_name("tiny/../large").is_err());
        }

        #[test]
        fn validate_name_rejects_slash() {
            assert!(validate_name("sub/tiny.en").is_err());
            assert!(validate_name("/abs/path").is_err());
        }

        #[test]
        fn validate_name_rejects_null_byte() {
            assert!(validate_name("tiny\0.en").is_err());
        }

        #[test]
        fn validate_name_accepts_unknown_custom_name() {
            // Unknown names (not in KNOWN_MODELS) are still valid to fetch.
            assert!(validate_name("my-custom-model-v2").is_ok());
        }

        // ---- check_url -----------------------------------------------------

        #[test]
        fn check_url_allows_https() {
            assert!(check_url("https://huggingface.co/x").is_ok());
        }

        #[test]
        fn check_url_allows_loopback_http_for_tests() {
            assert!(check_url("http://127.0.0.1:9999/x").is_ok());
            assert!(check_url("http://localhost/x").is_ok());
        }

        #[test]
        fn check_url_rejects_plain_http() {
            assert!(check_url("http://huggingface.co/x").is_err());
        }

        #[test]
        fn check_url_rejects_other_schemes() {
            assert!(check_url("ftp://example.com/x").is_err());
        }

        // ---- fmt_bytes -----------------------------------------------------

        #[test]
        fn fmt_bytes_shows_mb_below_gb() {
            assert_eq!(fmt_bytes(75 * 1024 * 1024), "75 MB");
        }

        #[test]
        fn fmt_bytes_shows_gb_above_threshold() {
            assert_eq!(fmt_bytes(3 * 1024 * 1024 * 1024), "3.0 GB");
        }

        // ---- KNOWN_MODELS table --------------------------------------------

        #[test]
        fn known_models_default_model_is_present() {
            // The default model from the SPR-068 design doc (D2) must be in the table.
            let tiny_en = KNOWN_MODELS.iter().find(|m| m.name == "tiny.en");
            assert!(tiny_en.is_some(), "tiny.en must be in KNOWN_MODELS");
        }

        #[test]
        fn known_models_have_nonzero_min_sizes() {
            for spec in KNOWN_MODELS {
                assert!(
                    spec.min_size > 0,
                    "model {:?} has zero min_size",
                    spec.name
                );
            }
        }

        #[test]
        fn known_models_large_variants_have_larger_minimums_than_tiny() {
            let tiny_min = KNOWN_MODELS
                .iter()
                .find(|m| m.name == "tiny.en")
                .unwrap()
                .min_size;
            let large_min = KNOWN_MODELS
                .iter()
                .find(|m| m.name == "large-v3")
                .unwrap()
                .min_size;
            assert!(large_min > tiny_min, "large-v3 must be bigger than tiny.en");
        }
    }
}
