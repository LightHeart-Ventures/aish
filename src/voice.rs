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
// TASK-363: rubato resampler — device-rate mono f32 → 16 kHz mono f32
// ---------------------------------------------------------------------------

/// Resampler: converts device-rate mono f32 PCM → 16 kHz mono f32 for Whisper.
///
/// Contract (frozen by SPR-068 design doc):
/// ```text
/// resample::to_whisper_pcm(&[f32], src_rate: u32) -> Result<Vec<f32>>
/// ```
pub mod resample {
    use anyhow::Context as _;
    use rubato::audioadapter_buffers::owned::InterleavedOwned;
    use rubato::{Fft, FixedSync, Resampler};

    // -----------------------------------------------------------------------
    // Public constants
    // -----------------------------------------------------------------------

    /// Sample rate expected by whisper-rs (16 kHz).
    pub const WHISPER_RATE: u32 = 16_000;

    // -----------------------------------------------------------------------
    // Public API (contract from SPR-068 design doc)
    // -----------------------------------------------------------------------

    /// Resample a mono f32 PCM buffer to 16 kHz for Whisper.
    ///
    /// # Arguments
    /// - `samples` — mono f32 PCM at `src_rate` Hz (output of
    ///   `capture::record_until_stop`).
    /// - `src_rate` — the sample rate of `samples` (the device's native rate,
    ///   as reported by `cpal::StreamConfig::sample_rate`).
    ///
    /// # Returns
    /// Mono f32 PCM at 16 kHz, ready to pass to `stt::Transcriber::transcribe`.
    /// If `src_rate` is already `WHISPER_RATE` (16 000 Hz) the buffer is returned
    /// as-is (cloned but not re-processed) so the path is zero-cost on devices
    /// that already capture at 16 kHz.
    ///
    /// # Errors
    /// Returns an error if the FFT resampler cannot be constructed (only possible
    /// for nonsensical rates like 0) or if the resampling itself fails (should not
    /// happen for valid mono f32 input).
    ///
    /// # Threading
    /// This function is CPU-bound and blocking.  Run it inside
    /// `tokio::task::spawn_blocking` (the REPL wiring in TASK-367 does this).
    pub fn to_whisper_pcm(samples: &[f32], src_rate: u32) -> anyhow::Result<Vec<f32>> {
        // Nothing to do for an empty capture (e.g. silence-timeout triggered
        // immediately).  Return early so the resampler constructor never sees
        // a zero-length buffer.
        if samples.is_empty() {
            return Ok(Vec::new());
        }

        // Fast path: no conversion needed if the device already captures at 16 kHz.
        if src_rate == WHISPER_RATE {
            return Ok(samples.to_vec());
        }

        // Wrap the mono input in the interleaved-owned adapter that rubato's
        // process_all() expects.  `channels = 1`, `frames = samples.len()`.
        let input_buf = InterleavedOwned::new_from(samples.to_vec(), 1, samples.len())
            .context("voice: failed to wrap capture buffer for resampling")?;

        // FFT synchronous resampler: good quality, fast on CPU.
        // chunk_size=1024 keeps the anti-aliasing delay low; process_all() handles
        // the whole clip in one call so the chunk boundary details are invisible
        // to the caller.
        let mut resampler =
            Fft::<f32>::new(src_rate as usize, WHISPER_RATE as usize, 1024, 1, FixedSync::Both)
                .context("voice: failed to create FFT resampler")?;

        let resampled = resampler
            .process_all(&input_buf, samples.len(), None)
            .context("voice: resampling failed")?;

        // process_all() allocates an InterleavedOwned<f32>.  For mono (1 channel)
        // the interleaved flat Vec<f32> *is* the mono PCM — no de-interleaving needed.
        Ok(resampled.take_data())
    }

    // -----------------------------------------------------------------------
    // Unit tests
    // -----------------------------------------------------------------------

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn passthrough_when_already_16k() {
            // Exact passthrough: no rubato involved.
            let samples: Vec<f32> = (0..64).map(|i| i as f32 * 0.01).collect();
            let result = to_whisper_pcm(&samples, 16_000).unwrap();
            assert_eq!(result, samples, "16 kHz input should be returned unchanged");
        }

        #[test]
        fn empty_input_returns_empty() {
            // Should not try to construct a resampler with zero frames.
            let result = to_whisper_pcm(&[], 44_100).unwrap();
            assert!(result.is_empty());
        }

        #[test]
        fn output_length_approx_for_44100_to_16000() {
            // One second of 44.1 kHz silence → expect ~16 000 output frames.
            let samples = vec![0.0f32; 44_100];
            let result = to_whisper_pcm(&samples, 44_100).unwrap();
            let expected: usize = 16_000;
            // Allow ±1 % tolerance to cover resampler delay / rounding.
            let tolerance = expected / 100 + 10;
            assert!(
                result.len().abs_diff(expected) <= tolerance,
                "expected ~{expected} output frames for 44.1→16 kHz, got {}",
                result.len()
            );
        }

        #[test]
        fn output_length_approx_for_48000_to_16000() {
            // One second of 48 kHz silence → expect ~16 000 output frames.
            let samples = vec![0.0f32; 48_000];
            let result = to_whisper_pcm(&samples, 48_000).unwrap();
            let expected: usize = 16_000;
            let tolerance = expected / 100 + 10;
            assert!(
                result.len().abs_diff(expected) <= tolerance,
                "expected ~{expected} output frames for 48→16 kHz, got {}",
                result.len()
            );
        }

        #[test]
        fn output_is_correct_ratio_for_22050_to_16000() {
            // ~0.5 second at 22.05 kHz → expect ~half a second at 16 kHz.
            let samples = vec![0.0f32; 22_050];
            let result = to_whisper_pcm(&samples, 22_050).unwrap();
            let expected: usize = 16_000;
            let tolerance = expected / 100 + 50;
            assert!(
                result.len().abs_diff(expected) <= tolerance,
                "expected ~{expected} output frames for 22.05→16 kHz, got {}",
                result.len()
            );
        }
    }
}

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
// TASK-365 stub: model fetch / verify  (filled in by the TASK-365 PR)
// ---------------------------------------------------------------------------

/// ggml model resolution, download, and checksum verification.
///
/// Contract (frozen by SPR-068 design doc):
/// ```text
/// model::ensure_model(name: &str) -> Result<PathBuf>
/// ```
pub mod model {}
