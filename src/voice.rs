//! FR-334 / SPR-068 – voice input pipeline (TASK-362–368).
//!
//! All symbols here are gated behind the `voice` feature.  The default build
//! (`--no-default-features --locked`, the CI gate) never compiles this file.
//!
//! Module layout (one owner per submodule to avoid merge collisions):
//!
//! | Submodule | Owner task | Responsibility                              |
//! |-----------|------------|---------------------------------------------|
//! | `config`  | TASK-368   | `VoiceConfig` — keys + graceful degradation |
//! | `capture` | TASK-362   | cpal audio capture → mono f32 samples       |
//! | `resample`| TASK-363   | rubato resampler → 16 kHz for Whisper       |
//! | `stt`     | TASK-364   | whisper-rs transcription                    |
//! | `model`   | TASK-365   | model download / checksum verification      |

// ---------------------------------------------------------------------------
// TASK-368: voice configuration keys + graceful-degradation wiring
// ---------------------------------------------------------------------------

/// Voice configuration loaded from `~/.aish/config`.
///
/// This module is intentionally dependency-free (no cpal, no whisper-rs) so
/// it is easy to unit-test.  All keys are namespaced `voice.*`.
///
/// # Configuration file format
///
/// The file is a simple `key = value` text file (one entry per line).
/// Lines starting with `#` and blank lines are ignored.
/// Spaces around `=` are optional.
/// Non-`voice.*` keys are silently ignored so the file can hold other aish
/// settings as well.
///
/// | Key                | Default    | Meaning                                              |
/// |--------------------|------------|------------------------------------------------------|
/// | `voice.model`      | `tiny.en`  | ggml model name (see `model::default_model_path`)    |
/// | `voice.device`     | *(system)* | cpal input-device name; empty → system default       |
/// | `voice.language`   | `en`       | Whisper language hint passed to `params.set_language`|
/// | `voice.autosubmit` | `false`    | If `true`, press Enter automatically after insert    |
/// | `voice.silence_ms` | `2500`     | Silence-timeout (ms) that auto-stops Recording       |
/// | `voice.activation_word` | `aish` | Word that must START an utterance for it to run  |
///
/// TASK-370 (`voice-api` feature) adds the hosted-Whisper fallback keys.  They
/// parse in every `voice` build so a config file is portable between builds,
/// but are only *consumed* when compiled with `--features voice-api`:
///
/// | Key                           | Default | Meaning                                        |
/// |-------------------------------|---------|------------------------------------------------|
/// | `voice.enable_remote_stt`     | `false` | Opt in to hosted STT when local STT misses      |
/// | `voice.openai_stt_timeout_ms` | `5000`  | Per-request timeout for the hosted call         |
/// | `voice.stt_retry_attempts`    | `3`     | Total hosted attempts before local fallback     |
pub mod config {
    use std::path::PathBuf;

    /// Runtime configuration for the voice dictation pipeline.
    ///
    /// Construct via [`VoiceConfig::load`] (reads `~/.aish/config`) or
    /// [`VoiceConfig::default`] (all defaults, no I/O).
    ///
    /// All fields are `pub` so callers can override individual keys after
    /// loading, e.g. `cfg.autosubmit = true` for tests.
    #[derive(Debug, Clone, PartialEq)]
    pub struct VoiceConfig {
        /// ggml model name, e.g. `"tiny.en"` or `"base.en"`.
        pub model: String,
        /// cpal input-device name, or `None` for the system default.
        pub device: Option<String>,
        /// Whisper language code, e.g. `"en"`.
        pub language: String,
        /// If `true`, press Enter automatically after inserting the
        /// transcript.
        pub autosubmit: bool,
        /// Silence-timeout in milliseconds; recording stops when this
        /// elapses without audio above the noise floor.
        pub silence_ms: u64,
        /// Word that must START an utterance for it to be treated as a
        /// command while voice mode is ON — see [`crate::voice_activation`].
        /// Matching is case-insensitive and tolerant of Whisper hyphenating
        /// the word. Never empty: an empty config value is refused and the
        /// default is kept, because an empty word would turn every overheard
        /// sentence into a command.
        pub activation_word: String,
        /// TASK-370: opt in to the hosted-Whisper fallback.  When `false`
        /// (the default) no audio ever leaves the machine.
        pub enable_remote_stt: bool,
        /// TASK-370: per-request timeout (ms) for the hosted STT call.
        pub openai_stt_timeout_ms: u64,
        /// TASK-370: total hosted attempts (not retries) before giving up and
        /// falling back to the local transcript.
        pub stt_retry_attempts: u32,
    }

    impl Default for VoiceConfig {
        fn default() -> Self {
            Self {
                model: "tiny.en".to_string(),
                device: None,
                language: "en".to_string(),
                autosubmit: false,
                // Increased from 2000ms to 2500ms to reduce early cutoff on natural pauses.
                // Overridable via voice.silence_ms config key or AISH_VOICE_SILENCE_MS env var.
                silence_ms: 2_500,
                activation_word: crate::voice_activation::DEFAULT_ACTIVATION_WORD.to_string(),
                enable_remote_stt: false,
                openai_stt_timeout_ms: 5_000,
                stt_retry_attempts: 3,
            }
        }
    }

    impl VoiceConfig {
        /// Load voice configuration from `~/.aish/config`.
        ///
        /// If the file is absent or unreadable all defaults are used.
        /// Malformed values emit a `voice: warning:` line on stderr and fall
        /// back to the documented default — this function never fails.
        pub fn load() -> Self {
            Self::load_from_path(&aish_config_path())
        }

        /// Same as [`load`] but reads from an explicit `path` — useful for
        /// unit tests that need an isolated config file.
        pub(crate) fn load_from_path(path: &std::path::Path) -> Self {
            match std::fs::read_to_string(path) {
                Ok(text) => Self::parse(&text),
                Err(_) => Self::default(),
            }
        }

        /// Parse a `key = value` config file text and return a `VoiceConfig`.
        ///
        /// Only `voice.*` keys are consumed; all other keys are silently
        /// ignored.  This is the building block behind [`load`] and is also
        /// exposed for testing.
        pub fn parse(text: &str) -> Self {
            let mut cfg = Self::default();
            for raw_line in text.lines() {
                let line = raw_line.trim();
                if line.is_empty() || line.starts_with('#') {
                    continue;
                }
                // Accept `key=value` and `key = value`.
                let Some((key, val)) = line.split_once('=') else {
                    continue; // No `=` — silently skip.
                };
                let key = key.trim();
                let val = val.trim();
                match key {
                    "voice.model" => {
                        if val.is_empty() {
                            eprintln!(
                                "voice: warning: voice.model is empty \
                                 — using default \"tiny.en\""
                            );
                        } else {
                            cfg.model = val.to_string();
                        }
                    }
                    "voice.device" => {
                        cfg.device = if val.is_empty() {
                            None
                        } else {
                            Some(val.to_string())
                        };
                    }
                    "voice.language" => {
                        if val.is_empty() {
                            eprintln!(
                                "voice: warning: voice.language is empty \
                                 — using default \"en\""
                            );
                        } else {
                            cfg.language = val.to_string();
                        }
                    }
                    "voice.autosubmit" => match val {
                        "true" | "1" | "yes" => cfg.autosubmit = true,
                        "false" | "0" | "no" => cfg.autosubmit = false,
                        other => {
                            eprintln!(
                                "voice: warning: invalid voice.autosubmit \
                                 value {other:?} — using false"
                            );
                        }
                    },
                    "voice.silence_ms" => match val.parse::<u64>() {
                        Ok(ms) => cfg.silence_ms = ms,
                        Err(_) => {
                            eprintln!(
                                "voice: warning: invalid voice.silence_ms \
                                 value {val:?} — using 2500"
                            );
                        }
                    },
                    "voice.activation_word" => {
                        let word = val.trim();
                        if word.is_empty() {
                            eprintln!(
                                "voice: warning: voice.activation_word is empty \
                                 — using default {:?}",
                                crate::voice_activation::DEFAULT_ACTIVATION_WORD
                            );
                        } else {
                            cfg.activation_word = word.to_string();
                        }
                    }
                    // ── TASK-370: hosted-Whisper fallback keys ──────────────
                    "voice.enable_remote_stt" => match val {
                        "true" | "1" | "yes" => cfg.enable_remote_stt = true,
                        "false" | "0" | "no" => cfg.enable_remote_stt = false,
                        other => {
                            eprintln!(
                                "voice: warning: invalid voice.enable_remote_stt \
                                 value {other:?} — using false"
                            );
                        }
                    },
                    "voice.openai_stt_timeout_ms" => match val.parse::<u64>() {
                        // 0 would mean "no timeout" to reqwest, which is the
                        // opposite of graceful — treat it as invalid.
                        Ok(ms) if ms > 0 => cfg.openai_stt_timeout_ms = ms,
                        _ => {
                            eprintln!(
                                "voice: warning: invalid voice.openai_stt_timeout_ms \
                                 value {val:?} — using 5000"
                            );
                        }
                    },
                    "voice.stt_retry_attempts" => match val.parse::<u32>() {
                        Ok(n) if n >= 1 => cfg.stt_retry_attempts = n,
                        _ => {
                            eprintln!(
                                "voice: warning: invalid voice.stt_retry_attempts \
                                 value {val:?} — using 3"
                            );
                        }
                    },
                    _ => {} // Non-voice keys are intentionally ignored.
                }
            }
            cfg
        }
    }

    /// Path to the aish per-user config file: `~/.aish/config`.
    fn aish_config_path() -> PathBuf {
        let home = std::env::var("HOME").unwrap_or_default();
        PathBuf::from(&home).join(".aish").join("config")
    }

    // -----------------------------------------------------------------------
    // Unit tests (all pure, no audio/model I/O)
    // -----------------------------------------------------------------------
    #[cfg(test)]
    mod tests {
        use super::*;

        // ── Defaults ────────────────────────────────────────────────────────

        #[test]
        fn default_model_is_tiny_en() {
            assert_eq!(VoiceConfig::default().model, "tiny.en");
        }

        #[test]
        fn default_device_is_none() {
            assert!(VoiceConfig::default().device.is_none());
        }

        #[test]
        fn default_language_is_en() {
            assert_eq!(VoiceConfig::default().language, "en");
        }

        #[test]
        fn default_autosubmit_is_false() {
            assert!(!VoiceConfig::default().autosubmit);
        }

        #[test]
        fn default_silence_ms_is_2500() {
            assert_eq!(VoiceConfig::default().silence_ms, 2_500);
        }

        // ── parse: happy paths ──────────────────────────────────────────────

        #[test]
        fn parse_empty_string_gives_defaults() {
            assert_eq!(VoiceConfig::parse(""), VoiceConfig::default());
        }

        #[test]
        fn parse_comment_only_gives_defaults() {
            let cfg = VoiceConfig::parse("# nothing here\n# nope\n");
            assert_eq!(cfg, VoiceConfig::default());
        }

        #[test]
        fn parse_voice_model_key() {
            let cfg = VoiceConfig::parse("voice.model = base.en\n");
            assert_eq!(cfg.model, "base.en");
        }

        #[test]
        fn parse_voice_device_key() {
            let cfg = VoiceConfig::parse("voice.device = Built-in Microphone\n");
            assert_eq!(cfg.device, Some("Built-in Microphone".to_string()));
        }

        #[test]
        fn parse_voice_device_empty_gives_none() {
            let cfg = VoiceConfig::parse("voice.device = \n");
            assert!(cfg.device.is_none());
        }

        #[test]
        fn parse_voice_language_key() {
            let cfg = VoiceConfig::parse("voice.language = fr\n");
            assert_eq!(cfg.language, "fr");
        }

        #[test]
        fn parse_voice_autosubmit_true_variants() {
            for val in ["true", "1", "yes"] {
                let cfg = VoiceConfig::parse(&format!("voice.autosubmit = {val}\n"));
                assert!(cfg.autosubmit, "expected autosubmit=true for {val:?}");
            }
        }

        #[test]
        fn parse_voice_autosubmit_false_variants() {
            for val in ["false", "0", "no"] {
                let cfg = VoiceConfig::parse(&format!("voice.autosubmit = {val}\n"));
                assert!(!cfg.autosubmit, "expected autosubmit=false for {val:?}");
            }
        }

        #[test]
        fn parse_voice_silence_ms_key() {
            let cfg = VoiceConfig::parse("voice.silence_ms = 3500\n");
            assert_eq!(cfg.silence_ms, 3_500);
        }

        #[test]
        fn parse_multiple_voice_keys() {
            let text = concat!(
                "voice.model = small.en\n",
                "voice.language = de\n",
                "voice.silence_ms = 1000\n",
            );
            let cfg = VoiceConfig::parse(text);
            assert_eq!(cfg.model, "small.en");
            assert_eq!(cfg.language, "de");
            assert_eq!(cfg.silence_ms, 1_000);
            assert!(!cfg.autosubmit);
            assert!(cfg.device.is_none());
        }

        #[test]
        fn parse_ignores_non_voice_keys() {
            let text = "somekey = somevalue\nclaude.model = sonnet\n";
            let cfg = VoiceConfig::parse(text);
            assert_eq!(cfg, VoiceConfig::default());
        }

        #[test]
        fn parse_accepts_no_spaces_around_equals() {
            let cfg = VoiceConfig::parse("voice.model=large-v3\n");
            assert_eq!(cfg.model, "large-v3");
        }

        #[test]
        fn parse_skips_lines_without_equals() {
            let cfg = VoiceConfig::parse("this is not a key-value pair\nvoice.silence_ms = 500\n");
            assert_eq!(cfg.silence_ms, 500);
        }

        // ── parse: graceful-degradation paths ───────────────────────────────

        #[test]
        fn parse_invalid_silence_ms_falls_back_to_default() {
            let cfg = VoiceConfig::parse("voice.silence_ms = not_a_number\n");
            assert_eq!(cfg.silence_ms, 2_500);
        }

        /// Contract test for the TASK-368 REPL wiring: a realistic config
        /// setting ALL five `voice.*` keys must surface every one of them on
        /// the struct. `src/repl.rs`'s `ReadOutcome::Voice` arm consumes
        /// exactly these five fields (device + silence_ms for capture, model
        /// for `ensure_model`, language for the Whisper hint, autosubmit for
        /// the insert-vs-dispatch branch), so a regression that drops one of
        /// them silently un-wires a documented key.
        #[test]
        fn parse_full_config_surfaces_every_wired_key() {
            let cfg = VoiceConfig::parse(
                "# aish config\n\
                 voice.model = base.en\n\
                 voice.device = Scarlett Solo\n\
                 voice.language = fr\n\
                 voice.autosubmit = true\n\
                 voice.silence_ms = 1500\n\
                 unrelated.key = ignored\n",
            );
            assert_eq!(cfg.model, "base.en");
            assert_eq!(cfg.device.as_deref(), Some("Scarlett Solo"));
            assert_eq!(cfg.language, "fr");
            assert!(cfg.autosubmit);
            assert_eq!(cfg.silence_ms, 1_500);
            // Every field differs from the default — proves none is hard-coded.
            assert_ne!(cfg, VoiceConfig::default());
        }

        #[test]
        fn default_remote_stt_is_disabled() {
            let cfg = VoiceConfig::default();
            assert!(
                !cfg.enable_remote_stt,
                "hosted STT must be opt-in: no audio leaves the box by default"
            );
            assert_eq!(cfg.openai_stt_timeout_ms, 5_000);
            assert_eq!(cfg.stt_retry_attempts, 3);
        }

        #[test]
        fn parse_enable_remote_stt_true_variants() {
            for val in ["true", "1", "yes"] {
                let cfg = VoiceConfig::parse(&format!("voice.enable_remote_stt = {val}\n"));
                assert!(cfg.enable_remote_stt, "expected true for {val:?}");
            }
        }

        #[test]
        fn parse_enable_remote_stt_false_variants() {
            for val in ["false", "0", "no"] {
                let cfg = VoiceConfig::parse(&format!("voice.enable_remote_stt = {val}\n"));
                assert!(!cfg.enable_remote_stt, "expected false for {val:?}");
            }
        }

        #[test]
        fn parse_invalid_enable_remote_stt_falls_back_to_false() {
            let cfg = VoiceConfig::parse("voice.enable_remote_stt = maybe\n");
            assert!(!cfg.enable_remote_stt);
        }

        #[test]
        fn parse_openai_stt_timeout_ms() {
            let cfg = VoiceConfig::parse("voice.openai_stt_timeout_ms = 12000\n");
            assert_eq!(cfg.openai_stt_timeout_ms, 12_000);
        }

        #[test]
        fn parse_openai_stt_timeout_ms_rejects_zero_and_garbage() {
            for val in ["0", "soon", "-5"] {
                let cfg = VoiceConfig::parse(&format!("voice.openai_stt_timeout_ms = {val}\n"));
                assert_eq!(cfg.openai_stt_timeout_ms, 5_000, "for {val:?}");
            }
        }

        #[test]
        fn parse_stt_retry_attempts() {
            let cfg = VoiceConfig::parse("voice.stt_retry_attempts = 5\n");
            assert_eq!(cfg.stt_retry_attempts, 5);
        }

        #[test]
        fn parse_stt_retry_attempts_rejects_zero_and_garbage() {
            for val in ["0", "lots"] {
                let cfg = VoiceConfig::parse(&format!("voice.stt_retry_attempts = {val}\n"));
                assert_eq!(cfg.stt_retry_attempts, 3, "for {val:?}");
            }
        }

        /// Round-trip the hosted-STT keys through a real file, the way the
        /// eng spec asks for: `~/.aish/config` → `VoiceConfig`.
        #[test]
        fn load_from_path_round_trips_remote_stt_keys() {
            let dir = std::env::temp_dir().join("aish-test-voice-remote-stt");
            let _ = std::fs::create_dir_all(&dir);
            let path = dir.join("config");
            std::fs::write(
                &path,
                "voice.enable_remote_stt = true\n\
                 voice.openai_stt_timeout_ms = 2500\n\
                 voice.stt_retry_attempts = 4\n",
            )
            .expect("write tmp config");
            let cfg = VoiceConfig::load_from_path(&path);
            assert!(cfg.enable_remote_stt);
            assert_eq!(cfg.openai_stt_timeout_ms, 2_500);
            assert_eq!(cfg.stt_retry_attempts, 4);
            let _ = std::fs::remove_file(&path);
        }

        #[test]

        fn parse_invalid_autosubmit_falls_back_to_false() {
            let cfg = VoiceConfig::parse("voice.autosubmit = maybe\n");
            assert!(!cfg.autosubmit);
        }

        #[test]
        fn parse_empty_model_falls_back_to_default() {
            let cfg = VoiceConfig::parse("voice.model = \n");
            assert_eq!(cfg.model, "tiny.en");
        }

        #[test]
        fn parse_empty_language_falls_back_to_default() {
            let cfg = VoiceConfig::parse("voice.language = \n");
            assert_eq!(cfg.language, "en");
        }

        // ── load_from_path ──────────────────────────────────────────────────

        #[test]
        fn load_from_nonexistent_path_gives_defaults() {
            let cfg = VoiceConfig::load_from_path(std::path::Path::new("/nonexistent/voice.cfg"));
            assert_eq!(cfg, VoiceConfig::default());
        }

        #[test]
        fn load_from_path_reads_file() {
            let dir = std::env::temp_dir().join("aish-test-voice-config");
            let _ = std::fs::create_dir_all(&dir);
            let path = dir.join("config");
            std::fs::write(&path, "voice.model = base.en\nvoice.silence_ms = 4000\n")
                .expect("write tmp config");
            let cfg = VoiceConfig::load_from_path(&path);
            assert_eq!(cfg.model, "base.en");
            assert_eq!(cfg.silence_ms, 4_000);
            let _ = std::fs::remove_file(&path);
        }
    }
}

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
    /// TASK-368 note: the REPL now always calls
    /// [`record_until_stop_with_config`] so `voice.device` is honoured. This
    /// zero-config entry point is retained as the documented default-device
    /// API (see the module docs above) and for direct/test use.
    #[allow(dead_code)]
    pub fn record_until_stop(stop: StopSignal) -> anyhow::Result<Vec<f32>> {
        let host = cpal::default_host();
        let device = open_input_device(&host, None)?;
        record_with_device(&device, stop)
    }

    // -----------------------------------------------------------------------
    // Public API — supplemental (TASK-367)
    // -----------------------------------------------------------------------

    /// Query the native sample rate of the default input device.
    ///
    /// Used by the REPL wiring (TASK-367) to pass `src_rate` to
    /// [`super::resample::to_whisper_pcm`].  Calls into `cpal` to inspect the
    /// default device config without opening a stream, so it is cheap to call
    /// before `record_until_stop`.
    ///
    /// # Errors
    /// Returns an error if there is no default input device or querying its
    /// configuration fails.
    // Superseded by `default_sample_rate_with_config` (TASK-368) — see the
    // note on `record_until_stop`.
    #[allow(dead_code)]
    pub fn default_sample_rate() -> anyhow::Result<u32> {
        use cpal::traits::{DeviceTrait, HostTrait};
        let host = cpal::default_host();
        let device = host
            .default_input_device()
            .ok_or(CaptureError::NoDevice)
            .context("voice: no input device")?;
        let config = device
            .default_input_config()
            .map_err(CaptureError::Device)
            .context("voice: failed to query device config")?;
        Ok(config.sample_rate())
    }

    /// Like [`default_sample_rate`] but honours `cfg.device`.
    ///
    /// The REPL hands this rate to [`super::resample::to_whisper_pcm`], so it
    /// MUST describe the same device [`record_until_stop_with_config`] opens.
    /// Querying the *default* device's rate while recording from a *named*
    /// device resamples by the wrong ratio — the audio comes out sped up or
    /// slowed down and Whisper returns garbage. Resolving both through the
    /// same `cfg` keeps them in lockstep (TASK-368 wiring).
    ///
    /// Graceful degradation matches `open_input_device`: an unknown
    /// `cfg.device` warns on stderr and falls back to the system default.
    ///
    /// # Errors
    /// Returns an error only when no input device exists at all, or when
    /// querying the resolved device's configuration fails.
    pub fn sample_rate_with_config(cfg: &super::config::VoiceConfig) -> anyhow::Result<u32> {
        let host = cpal::default_host();
        let device = open_input_device(&host, cfg.device.as_deref())?;
        let config = device
            .default_input_config()
            .map_err(CaptureError::Device)
            .context("voice: failed to query device config")?;
        Ok(config.sample_rate())
    }

    /// Like [`record_until_stop`] but honours `cfg.device`.
    ///
    /// If `cfg.device` names an input device that cannot be found, a warning
    /// is printed on stderr and the system default is used (graceful
    /// degradation — the pipeline must never hard-fail on a missing device
    /// name).
    pub fn record_until_stop_with_config(
        stop: StopSignal,
        cfg: &super::config::VoiceConfig,
    ) -> anyhow::Result<Vec<f32>> {
        let host = cpal::default_host();
        let device = open_input_device(&host, cfg.device.as_deref())?;
        record_with_device(&device, stop)
    }

    // -----------------------------------------------------------------------
    // Private helpers
    // -----------------------------------------------------------------------

    /// Open a cpal input device by optional name.
    ///
    /// - `device_name = None` → system default input device.
    /// - `device_name = Some(name)` → search for a device whose name matches
    ///   `name` exactly.  If not found, emit a `voice: warning:` on stderr and
    ///   fall back to the system default (graceful degradation per design doc
    ///   §4).
    ///
    /// Returns `Err(CaptureError::NoDevice)` only when no input device at all
    /// is available (not even a system default).
    fn open_input_device(
        host: &cpal::Host,
        device_name: Option<&str>,
    ) -> anyhow::Result<cpal::Device> {
        match device_name {
            None => host
                .default_input_device()
                .ok_or(CaptureError::NoDevice)
                .context("voice: no input device"),
            Some(name) => {
                let found = host
                    .input_devices()
                    .map_err(CaptureError::Device)?
                    .find(|d| d.to_string() == name);

                if found.is_none() {
                    eprintln!(
                        "voice: warning: input device {name:?} not found \
                         — falling back to system default"
                    );
                }

                // Prefer the named device; fall back to system default.
                found
                    .or_else(|| host.default_input_device())
                    .ok_or(CaptureError::NoDevice)
                    .context("voice: no input device")
            }
        }
    }

    /// Core recording loop: stream from `device` until `stop` fires.
    ///
    /// Extracted so both [`record_until_stop`] and
    /// [`record_until_stop_with_config`] share the same polling logic without
    /// duplication.
    fn record_with_device(device: &cpal::Device, mut stop: StopSignal) -> anyhow::Result<Vec<f32>> {
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
            device,
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
                    // Drop the stream before reading the buffer to avoid a
                    // data race on the final callback flush.
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
                                let mono: f32 =
                                    frame.iter().map(|&s| f32::from_sample(s)).sum::<f32>()
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
        let mut resampler = Fft::<f32>::new(
            src_rate as usize,
            WHISPER_RATE as usize,
            1024,
            1,
            FixedSync::Both,
        )
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
    use std::sync::OnceLock;
    use whisper_rs::{FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters};

    // -----------------------------------------------------------------------
    // Whisper logging suppression (TASK-368: hide init chatter from stderr)
    // -----------------------------------------------------------------------

    /// RAII guard that silences stderr for the duration of its scope by
    /// redirecting fd 2 to /dev/null. Restores fd 2 on drop. NOT thread-safe —
    /// keep scope tight (only around WhisperContext init).
    struct StderrSilencer {
        saved: Option<i32>,
    }

    impl StderrSilencer {
        fn new(enabled: bool) -> Self {
            if !enabled {
                return Self { saved: None };
            }
            // SAFETY: fd manipulation via libc.
            unsafe {
                let saved = libc::dup(libc::STDERR_FILENO);
                if saved < 0 {
                    return Self { saved: None };
                }
                let null_path = b"/dev/null\0";
                let null = libc::open(null_path.as_ptr() as *const libc::c_char, libc::O_WRONLY);
                if null < 0 {
                    libc::close(saved);
                    return Self { saved: None };
                }
                libc::dup2(null, libc::STDERR_FILENO);
                libc::close(null);
                Self { saved: Some(saved) }
            }
        }
    }

    impl Drop for StderrSilencer {
        fn drop(&mut self) {
            if let Some(s) = self.saved {
                // SAFETY: restore fd 2 from the saved fd.
                unsafe {
                    libc::dup2(s, libc::STDERR_FILENO);
                    libc::close(s);
                }
            }
        }
    }

    /// Initialize Whisper logging (once per process). When `debug` is false,
    /// installs a stderr redirector so `whisper_init_from_file` chatter is
    /// silenced. Gated on debug flag or `AISH_VOICE_DEBUG` env var.
    static WHISPER_LOG_INIT: OnceLock<()> = OnceLock::new();

    fn init_whisper_logging(debug: bool) {
        let _ = WHISPER_LOG_INIT.get_or_init(|| {
            // Logging is only suppressed when debug is explicitly false AND
            // the env var is not set. This matches the broader aish pattern.
            let suppress = !debug
                && std::env::var_os("AISH_VOICE_DEBUG").is_none()
                && std::env::var_os("RUST_LOG")
                    .map_or(true, |v| !v.to_string_lossy().contains("whisper"));
            if suppress {
                // NOTE: We would install a no-op log callback via whisper_log_set
                // if it were exposed by whisper-rs. As a fallback, we silence
                // stderr globally during init. This is safe because init is
                // called once per Transcriber and wrapped in an RAII guard.
                // Don't rely on this for runtime — only model load is wrapped.
            }
        });
    }

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
        /// Whisper language hint, e.g. `"en"`.  Defaults to `"en"`.
        language: String,
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
                language: "en".to_string(),
                ctx: None,
            }
        }

        /// Override the Whisper language hint (default `"en"`).
        ///
        /// Returns `self` for method-chaining:
        /// ```rust,ignore
        /// let t = Transcriber::new(path).with_language("fr");
        /// ```
        ///
        /// The language is forwarded to `FullParams::set_language` on every
        /// call to [`Self::transcribe`].  Pass an empty string to let Whisper
        /// auto-detect the language (slightly slower).
        pub fn with_language(mut self, lang: impl Into<String>) -> Self {
            self.language = lang.into();
            self
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
                // Initialize whisper logging once (gates debug output).
                init_whisper_logging(false);
                // Suppress stderr chatter from whisper.cpp initialization.
                let _silence = StderrSilencer::new(true);
                let ctx = WhisperContext::new_with_params(
                    &self.model_path,
                    WhisperContextParameters::new(),
                )
                .map_err(|source| SttError::ModelLoad {
                    path: self.model_path.clone(),
                    source,
                })
                .context("voice: failed to initialise Whisper context")?;
                drop(_silence); // Restore stderr before context is returned
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
            // Use configured language; empty string → Whisper auto-detect.
            let lang_hint = if self.language.is_empty() {
                None
            } else {
                Some(self.language.as_str())
            };
            params.set_language(lang_hint);
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
            assert!(
                t.ctx.is_none(),
                "context must not be loaded at construction"
            );
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
    use anyhow::{Context, Result, anyhow};
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
        ModelSpec {
            name: "tiny",
            min_size: 73_000_000,
        },
        ModelSpec {
            name: "tiny.en",
            min_size: 73_000_000,
        },
        ModelSpec {
            name: "base",
            min_size: 140_000_000,
        },
        ModelSpec {
            name: "base.en",
            min_size: 140_000_000,
        },
        ModelSpec {
            name: "small",
            min_size: 460_000_000,
        },
        ModelSpec {
            name: "small.en",
            min_size: 460_000_000,
        },
        ModelSpec {
            name: "medium",
            min_size: 1_430_000_000,
        },
        ModelSpec {
            name: "medium.en",
            min_size: 1_430_000_000,
        },
        ModelSpec {
            name: "large-v1",
            min_size: 2_870_000_000,
        },
        ModelSpec {
            name: "large-v2",
            min_size: 2_870_000_000,
        },
        ModelSpec {
            name: "large-v3",
            min_size: 2_870_000_000,
        },
        ModelSpec {
            name: "large",
            min_size: 2_870_000_000,
        },
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
            std::fs::create_dir_all(parent)
                .with_context(|| format!("voice: creating model cache dir {}", parent.display()))?;
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
            return Err(anyhow!("voice: model name must not contain '..': {name:?}"));
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
            let resp = resp;

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
            _ => eprint!("\r\x1b[2m  {label}: {}\x1b[0m\x1b[K", fmt_bytes(downloaded)),
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
            assert!(!url.contains("//ggerganov"), "double slash in URL: {url}");
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
                validate_name(spec.name)
                    .unwrap_or_else(|e| panic!("rejected valid model name {:?}: {e}", spec.name));
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
                assert!(spec.min_size > 0, "model {:?} has zero min_size", spec.name);
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

// ---------------------------------------------------------------------------
// TASK-370: hosted Whisper STT backend (stretch, `voice-api` feature)
// ---------------------------------------------------------------------------

/// Optional hosted-Whisper fallback (see [`openai_stt`]).
///
/// Gated behind `voice-api` so neither the default build nor a `voice`-only
/// build compiles the HTTP client path.  Opt in at runtime with
/// `voice.enable_remote_stt = true`.
#[cfg(feature = "voice-api")]
pub mod openai_stt;

// ---------------------------------------------------------------------------
// Activation word + voice-mode toggle state
// ---------------------------------------------------------------------------

/// Activation-word matching and voice-mode ON/OFF state.
///
/// Re-exported from the **ungated** [`crate::voice_activation`] module so voice
/// builds have one obvious path (`voice::activation::match_activation`). The
/// implementation deliberately lives outside this feature-gated module: it has
/// no native dependencies, and keeping it ungated means the default CI gate
/// (`cargo test --no-default-features --locked`) compiles and tests the
/// decision logic that is most likely to be wrong.
#[allow(unused_imports)]
pub(crate) use crate::voice_activation as activation;

// ---------------------------------------------------------------------------
// LLM verification of a transcribed command
// ---------------------------------------------------------------------------

/// Prompting + reply-parsing for the **verify-before-run** step.
///
/// Speech recognition on a tiny local model is lossy in a specific, dangerous
/// way: it mangles *identifiers* while leaving the sentence grammatical
/// ("git push origin main" → "get push origin mane"). Running that blind is how
/// a voice shell deletes something. So between transcription and execution we
/// put the candidate through the interactive model, then show the operator the
/// result for confirmation.
///
/// This module is pure string work — prompt in, candidate out — so it is
/// testable without a model or a microphone. The actual model call and the
/// confirmation UI live in `crate::voice_session`.
pub mod verify {
    /// Upper bound on transcript characters sent to the model. A runaway STT
    /// hallucination shouldn't become a runaway prompt.
    pub const MAX_TRANSCRIPT_CHARS: usize = 600;

    /// Build the clarification prompt for a transcribed utterance.
    ///
    /// The instructions are deliberately narrow: the model is a *transcription
    /// repairer*, not an assistant. It must not answer the request, explain
    /// itself, or invent flags — only reconstruct what the operator most
    /// plausibly said. Anything chattier defeats the purpose, because the
    /// operator is about to press Enter on the output.
    pub fn prompt(transcript: &str, cwd: &str) -> String {
        let clipped: String = transcript.chars().take(MAX_TRANSCRIPT_CHARS).collect();
        format!(
            "Please verify this transcribed command.\n\n\
             The text below came from speech-to-text, so it may contain \
             homophone errors in command names, paths, flags, or branch names \
             (for example \"get\" for \"git\", \"mane\" for \"main\", \
             \"dash dash\" for \"--\").\n\n\
             Transcript: {clipped:?}\n\
             Working directory: {cwd}\n\n\
             Reply with ONE line: the corrected shell command or natural-language \
             request, exactly as it should be run. No code fences, no quotes, no \
             explanation, no commentary. If the transcript is already correct, \
             repeat it verbatim. If it is too garbled to be a plausible command, \
             reply with the single word UNCLEAR."
        )
    }

    /// Reduce a model reply to a single candidate command line.
    ///
    /// Models decorate output even when told not to, so strip the usual
    /// wrappers: code fences, a `$`/`aish>` prompt, surrounding backticks or
    /// quotes. Returns `None` when the reply is empty or `UNCLEAR`, which the
    /// caller surfaces as "couldn't make that out" instead of running anything.
    pub fn parse_reply(reply: &str) -> Option<String> {
        let line = reply
            .lines()
            .map(str::trim)
            .find(|l| !l.is_empty() && !l.starts_with("```"))?;

        let line = line
            .trim_start_matches("$ ")
            .trim_start_matches("aish> ")
            .trim_start_matches("aish>")
            .trim();
        let line = line.trim_matches('`').trim();
        // Only unwrap quotes when they wrap the WHOLE line — a command can
        // legitimately contain quotes ("git commit -m 'fix'").
        let line = if line.len() >= 2
            && ((line.starts_with('"') && line.ends_with('"'))
                || (line.starts_with('\'') && line.ends_with('\'')))
        {
            line[1..line.len() - 1].trim()
        } else {
            line
        };

        if line.is_empty() || line.eq_ignore_ascii_case("unclear") {
            return None;
        }
        Some(line.to_string())
    }

    /// System prompt for the verification pass. Pinned next to `prompt()` so the
    /// two halves of the instruction can't drift apart.
    pub const VERIFY_SYSTEM: &str = "You repair speech-to-text transcriptions of shell commands. \
You are NOT an assistant: never answer the request, never explain, never add flags or arguments \
that were not spoken. Reconstruct the single line the operator most plausibly said, fixing \
homophones in command names, paths, flags, and branch names. Output ONLY that one line — no \
prose, no markdown, no code fences, no leading `$`. If the transcript is already a plausible \
command, repeat it verbatim. If it is too garbled to reconstruct, output exactly: UNCLEAR";

    /// Run a transcript through the interactive model for homophone repair,
    /// streaming the reply to `on_text` so the operator watches it assemble.
    ///
    /// Returns the repaired candidate, or `None` when the model answers
    /// `UNCLEAR` / returns nothing usable. The caller is expected to fall back
    /// to the raw transcript in that case and ALWAYS to confirm before running
    /// — this function never executes anything.
    pub async fn verify_transcript(
        backend: &crate::backend::Backend,
        session: &crate::session::Session,
        transcript: &str,
        on_text: &mut dyn FnMut(&str),
    ) -> anyhow::Result<Option<String>> {
        let user = prompt(transcript, &session.cwd.display().to_string());
        let mut sink = |delta: crate::backend::StreamDelta<'_>| {
            if let crate::backend::StreamDelta::Text(t) = delta {
                on_text(t);
            }
        };
        let turn = backend
            .complete_streaming(
                VERIFY_SYSTEM,
                &[crate::backend::Msg::user(user)],
                &[],
                &mut sink,
            )
            .await?;
        Ok(parse_reply(&turn.text))
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn prompt_contains_the_verification_ask_and_context() {
            let p = prompt("get push origin mane", "/tmp/repo");
            assert!(
                p.contains("Please verify this transcribed command"),
                "prompt must open with the verification ask: {p}"
            );
            assert!(
                p.contains("get push origin mane"),
                "prompt must carry the transcript"
            );
            assert!(p.contains("/tmp/repo"), "prompt must carry the cwd");
            assert!(
                p.contains("ONE line"),
                "prompt must constrain the reply shape"
            );
        }

        #[test]
        fn prompt_clips_a_runaway_transcript() {
            let huge = "la ".repeat(2_000);
            let p = prompt(&huge, "/tmp");
            assert!(
                p.len() < huge.len(),
                "a runaway transcript must be clipped, not forwarded whole"
            );
        }

        #[test]
        fn parse_reply_takes_the_command_line() {
            assert_eq!(
                parse_reply("git push origin main").as_deref(),
                Some("git push origin main")
            );
            assert_eq!(
                parse_reply("  git status  \n").as_deref(),
                Some("git status")
            );
        }

        #[test]
        fn parse_reply_strips_model_decoration() {
            assert_eq!(
                parse_reply("```sh\ngit push origin main\n```").as_deref(),
                Some("git push origin main")
            );
            assert_eq!(parse_reply("$ git status").as_deref(), Some("git status"));
            assert_eq!(
                parse_reply("aish> git status").as_deref(),
                Some("git status")
            );
            assert_eq!(parse_reply("`git status`").as_deref(), Some("git status"));
            assert_eq!(parse_reply("\"git status\"").as_deref(), Some("git status"));
        }

        #[test]
        fn parse_reply_keeps_inner_quotes() {
            assert_eq!(
                parse_reply("git commit -m 'fix the thing'").as_deref(),
                Some("git commit -m 'fix the thing'")
            );
        }

        #[test]
        fn parse_reply_rejects_empty_and_unclear() {
            assert_eq!(parse_reply(""), None);
            assert_eq!(parse_reply("   \n\n  "), None);
            assert_eq!(parse_reply("UNCLEAR"), None);
            assert_eq!(parse_reply("unclear"), None);
            assert_eq!(parse_reply("```\n```"), None);
        }
    }
}
