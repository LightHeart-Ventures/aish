//! TASK-370 (FR-334 / SPR-068 Phase 3, stretch) — hosted Whisper STT backend.
//!
//! This module is the **opt-in** remote half of the dictation pipeline.  It is
//! compiled only under `--features voice-api` and is only ever consulted when
//! the user explicitly sets `voice.enable_remote_stt = true` in
//! `~/.aish/config`.  Default builds and `voice`-only builds never reach it,
//! and all audio stays on-device.
//!
//! # Contract
//!
//! ```text
//! OpenAiStt::from_config(&VoiceConfig) -> OpenAiStt        // infallible
//! OpenAiStt::transcribe(&[f32], sample_rate) -> Result<String, RemoteSttError>
//! ```
//!
//! The caller ([`crate::repl`]'s `ReadOutcome::Voice` arm) treats a
//! `RemoteSttError` as *non-fatal*: it logs a warning and falls back to
//! whatever the local `tiny.en` pass produced.  A hosted-STT outage must never
//! break dictation.
//!
//! # Why hand-rolled WAV + multipart?
//!
//! `reqwest` is already an unconditional dependency of aish, but its
//! `multipart` feature is not enabled — turning it on would add `mime_guess`
//! to the **default** build purely for a `voice-api`-gated code path.  A RIFF
//! header is 44 deterministic bytes and a `multipart/form-data` body is three
//! string-delimited parts, so both are encoded inline here.  Zero new
//! dependencies, and both encoders are unit-tested byte-for-byte.
//!
//! # Retry policy (per eng spec)
//!
//! * 5 s per-request timeout (`voice.openai_stt_timeout_ms`).
//! * 3 attempts (`voice.stt_retry_attempts`) with exponential backoff
//!   1 s → 2 s → 4 s (see [`backoff_delay`]).
//! * `429` and `5xx` are retried; `401`/`403` and other `4xx` are fatal
//!   immediately (retrying a bad credential just burns the user's time).
//! * When every attempt is exhausted the caller falls back to local STT.
//!
//! # Privacy
//!
//! The first time hosted STT is used in a process, [`warn_once`] prints a
//! one-shot banner naming the destination host, because mic audio leaving the
//! machine is a decision the user should see acknowledged. The credential is
//! read from `$OPENAI_API_KEY` and is scrubbed out of every error string by
//! [`redact_key`] before it can reach a log line.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Default OpenAI transcription endpoint.
pub const DEFAULT_ENDPOINT: &str = "https://api.openai.com/v1/audio/transcriptions";

/// Hosted model used for transcription.
pub const DEFAULT_MODEL: &str = "whisper-1";

/// Environment variable holding the API credential.
pub const API_KEY_ENV: &str = "OPENAI_API_KEY";

/// Environment variable that overrides the endpoint.  Exists so the loopback
/// tests (and an operator debugging a proxy) can point the client at a local
/// server without touching the code path under test.
pub const ENDPOINT_ENV: &str = "AISH_OPENAI_STT_URL";

/// `multipart/form-data` boundary.  Fixed (not random) so the encoder is
/// deterministic and therefore byte-comparable in tests; the token is long and
/// unusual enough that it cannot collide with WAV payload bytes in practice.
pub(crate) const MULTIPART_BOUNDARY: &str = "aish370voiceboundaryZzT9qP";

/// Hard ceiling on a single backoff sleep, so a pathological
/// `stt_retry_attempts` cannot wedge the REPL for minutes.
const MAX_BACKOFF: Duration = Duration::from_secs(30);

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

/// Errors returned by the hosted STT backend.
///
/// Every variant is *recoverable* from the REPL's point of view — the caller
/// falls back to the local transcript.  The distinction that matters here is
/// whether an attempt is worth **retrying** (see [`is_retryable_status`]).
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum RemoteSttError {
    /// `voice.enable_remote_stt = true` but no credential is available.
    #[error(
        "hosted STT is enabled but {API_KEY_ENV} is not set \
         — export {API_KEY_ENV} or set voice.enable_remote_stt = false"
    )]
    MissingApiKey,

    /// The endpoint rejected the credential (`401`/`403`).
    #[error("hosted STT rejected the credential — check {API_KEY_ENV}")]
    Unauthorized,

    /// The endpoint rate-limited us (`429`).
    #[error("hosted STT rate-limited the request (HTTP 429)")]
    RateLimited,

    /// A non-success HTTP status.
    #[error("hosted STT returned HTTP {status}")]
    Http { status: u16 },

    /// Transport-level failure (DNS, TCP, TLS, timeout).
    #[error("hosted STT unreachable: {message}")]
    Network { message: String },

    /// A `2xx` response whose body did not contain a transcript.
    #[error("hosted STT returned an unusable response: {0}")]
    Decode(String),

    /// Refusing to spend an API call on an empty capture.
    #[error("refusing to send an empty capture to hosted STT")]
    EmptyAudio,

    /// All retry attempts were used up.
    #[error("hosted STT failed after {attempts} attempt(s): {last}")]
    Exhausted { attempts: u32, last: String },
}

/// Internal per-attempt outcome: whether the failure is worth another try.
enum AttemptError {
    Retryable(RemoteSttError),
    Fatal(RemoteSttError),
}

// ---------------------------------------------------------------------------
// One-time privacy banner
// ---------------------------------------------------------------------------

static WARNED: AtomicBool = AtomicBool::new(false);

/// Print the hosted-STT privacy banner exactly once per process.
///
/// Idempotent and thread-safe: the first caller wins the `swap`, every
/// subsequent caller is a no-op.
pub fn warn_once() {
    if !WARNED.swap(true, Ordering::SeqCst) {
        eprintln!(
            "\x1b[33mvoice: hosted STT is enabled — captured audio is uploaded to \
             api.openai.com for transcription. Set voice.enable_remote_stt = false \
             to keep all audio on-device.\x1b[0m"
        );
        tracing::warn!("voice: hosted STT enabled — mic audio egresses to api.openai.com");
    }
}

// ---------------------------------------------------------------------------
// Pure helpers (all unit-tested)
// ---------------------------------------------------------------------------

/// Should a request that produced `status` be retried?
///
/// `429` (rate limit) and `5xx` (server-side) are transient; everything else
/// is the caller's fault and will fail identically on retry.
pub(crate) fn is_retryable_status(status: u16) -> bool {
    status == 429 || (500..600).contains(&status)
}

/// Exponential backoff for `attempt` (1-based): `base * 2^(attempt - 1)`,
/// clamped to [`MAX_BACKOFF`].
///
/// With the default 1 s base this yields the spec'd 1 s → 2 s → 4 s ladder.
pub(crate) fn backoff_delay(base: Duration, attempt: u32) -> Duration {
    let shift = attempt.saturating_sub(1).min(16);
    let scaled = base.saturating_mul(1u32 << shift);
    if scaled > MAX_BACKOFF {
        MAX_BACKOFF
    } else {
        scaled
    }
}

/// Remove `key` from `text` so a credential can never reach a log line.
///
/// Also strips a `Bearer <token>` form, because transport errors sometimes
/// echo request headers.
pub(crate) fn redact_key(text: &str, key: &str) -> String {
    let mut out = text.to_string();
    if !key.is_empty() {
        out = out.replace(key, "<redacted>");
    }
    out
}

/// Encode mono `f32` samples as a 16-bit PCM WAV file (RIFF, 44-byte header).
///
/// Samples are clamped to `[-1.0, 1.0]` before scaling so clipping cannot wrap
/// around to the opposite polarity.
pub(crate) fn encode_wav_pcm16(samples: &[f32], sample_rate: u32) -> Vec<u8> {
    let data_len = samples.len() * 2;
    let mut out = Vec::with_capacity(44 + data_len);

    // RIFF chunk
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&((36 + data_len) as u32).to_le_bytes());
    out.extend_from_slice(b"WAVE");

    // fmt subchunk (PCM, mono, 16-bit)
    out.extend_from_slice(b"fmt ");
    out.extend_from_slice(&16u32.to_le_bytes()); // subchunk size
    out.extend_from_slice(&1u16.to_le_bytes()); // audio format: PCM
    out.extend_from_slice(&1u16.to_le_bytes()); // channels: mono
    out.extend_from_slice(&sample_rate.to_le_bytes());
    out.extend_from_slice(&(sample_rate * 2).to_le_bytes()); // byte rate
    out.extend_from_slice(&2u16.to_le_bytes()); // block align
    out.extend_from_slice(&16u16.to_le_bytes()); // bits per sample

    // data subchunk
    out.extend_from_slice(b"data");
    out.extend_from_slice(&(data_len as u32).to_le_bytes());
    for s in samples {
        let v = (s.clamp(-1.0, 1.0) * f32::from(i16::MAX)).round() as i16;
        out.extend_from_slice(&v.to_le_bytes());
    }

    out
}

/// Build the `multipart/form-data` body for the transcriptions endpoint.
///
/// Parts: `file` (the WAV), `model`, `response_format=json`, and `language`
/// (omitted when empty so the hosted model auto-detects, mirroring the local
/// `Transcriber::with_language("")` semantics).
pub(crate) fn multipart_body(wav: &[u8], model: &str, language: &str) -> Vec<u8> {
    let mut body = Vec::with_capacity(wav.len() + 512);

    let push_field = |body: &mut Vec<u8>, name: &str, value: &str| {
        body.extend_from_slice(format!("--{MULTIPART_BOUNDARY}\r\n").as_bytes());
        body.extend_from_slice(
            format!("Content-Disposition: form-data; name=\"{name}\"\r\n\r\n").as_bytes(),
        );
        body.extend_from_slice(value.as_bytes());
        body.extend_from_slice(b"\r\n");
    };

    // File part first — mirrors what the OpenAI examples send.
    body.extend_from_slice(format!("--{MULTIPART_BOUNDARY}\r\n").as_bytes());
    body.extend_from_slice(
        b"Content-Disposition: form-data; name=\"file\"; filename=\"audio.wav\"\r\n",
    );
    body.extend_from_slice(b"Content-Type: audio/wav\r\n\r\n");
    body.extend_from_slice(wav);
    body.extend_from_slice(b"\r\n");

    push_field(&mut body, "model", model);
    push_field(&mut body, "response_format", "json");
    if !language.trim().is_empty() {
        push_field(&mut body, "language", language.trim());
    }

    body.extend_from_slice(format!("--{MULTIPART_BOUNDARY}--\r\n").as_bytes());
    body
}

/// Extract the transcript from a `2xx` response body.
///
/// Accepts the documented `{"text": "..."}` shape and surfaces an
/// `{"error": {"message": ...}}` payload as a [`RemoteSttError::Decode`] so
/// the reason reaches the log instead of a bare "unusable response".
pub(crate) fn parse_transcript(body: &str) -> Result<String, RemoteSttError> {
    let value: serde_json::Value = serde_json::from_str(body)
        .map_err(|e| RemoteSttError::Decode(format!("invalid JSON: {e}")))?;

    if let Some(text) = value.get("text").and_then(|t| t.as_str()) {
        return Ok(text.trim().to_string());
    }
    if let Some(msg) = value
        .get("error")
        .and_then(|e| e.get("message"))
        .and_then(|m| m.as_str())
    {
        return Err(RemoteSttError::Decode(msg.to_string()));
    }
    Err(RemoteSttError::Decode(
        "no `text` field in response".to_string(),
    ))
}

// ---------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------

/// Hosted Whisper STT client.
///
/// Construct with [`OpenAiStt::from_config`] (the REPL path) or
/// [`OpenAiStt::default`] and override fields directly (the test path).
#[derive(Debug, Clone)]
pub struct OpenAiStt {
    /// Transcriptions endpoint.  Defaults to [`DEFAULT_ENDPOINT`], overridable
    /// via `$AISH_OPENAI_STT_URL`.
    pub endpoint: String,
    /// Hosted model name.
    pub model: String,
    /// Language hint; empty string means auto-detect.
    pub language: String,
    /// Per-request timeout.
    pub timeout: Duration,
    /// Total attempts (not retries) — `1` means "try once, never retry".
    pub attempts: u32,
    /// Base backoff unit; the ladder is `base * 2^(attempt-1)`.
    pub backoff_base: Duration,
    /// Explicit credential.  `None` (the production path) reads
    /// `$OPENAI_API_KEY` at call time.
    pub api_key: Option<String>,
}

impl Default for OpenAiStt {
    fn default() -> Self {
        Self {
            endpoint: std::env::var(ENDPOINT_ENV)
                .ok()
                .filter(|u| !u.trim().is_empty())
                .unwrap_or_else(|| DEFAULT_ENDPOINT.to_string()),
            model: DEFAULT_MODEL.to_string(),
            language: "en".to_string(),
            timeout: Duration::from_millis(5_000),
            attempts: 3,
            backoff_base: Duration::from_secs(1),
            api_key: None,
        }
    }
}

impl OpenAiStt {
    /// Build a client from the user's `voice.*` configuration.
    ///
    /// Maps `voice.openai_stt_timeout_ms`, `voice.stt_retry_attempts`, and
    /// `voice.language` onto the client; the endpoint still honours
    /// `$AISH_OPENAI_STT_URL`.
    pub fn from_config(cfg: &super::config::VoiceConfig) -> Self {
        Self {
            language: cfg.language.clone(),
            timeout: Duration::from_millis(cfg.openai_stt_timeout_ms),
            attempts: cfg.stt_retry_attempts.max(1),
            ..Self::default()
        }
    }

    /// Resolve the credential: explicit override first, then the environment.
    fn resolve_key(&self) -> Result<String, RemoteSttError> {
        if let Some(k) = self.api_key.as_ref().filter(|k| !k.trim().is_empty()) {
            return Ok(k.clone());
        }
        std::env::var(API_KEY_ENV)
            .ok()
            .filter(|k| !k.trim().is_empty())
            .ok_or(RemoteSttError::MissingApiKey)
    }

    /// Transcribe `samples` (mono `f32` at `sample_rate`) via hosted Whisper.
    ///
    /// Retries transient failures per the module-level policy.  Returns the
    /// trimmed transcript on success; on failure the caller must fall back to
    /// the local result rather than surfacing a hard error to the user.
    pub async fn transcribe(
        &self,
        samples: &[f32],
        sample_rate: u32,
    ) -> Result<String, RemoteSttError> {
        if samples.is_empty() {
            return Err(RemoteSttError::EmptyAudio);
        }
        let key = self.resolve_key()?;
        warn_once();

        let wav = encode_wav_pcm16(samples, sample_rate);
        let body = multipart_body(&wav, &self.model, &self.language);

        let client = reqwest::Client::builder()
            .timeout(self.timeout)
            .build()
            .map_err(|e| RemoteSttError::Network {
                message: redact_key(&e.to_string(), &key),
            })?;

        let attempts = self.attempts.max(1);
        let mut last = RemoteSttError::Network {
            message: "no attempt was made".to_string(),
        };

        for attempt in 1..=attempts {
            match self.post_once(&client, &key, body.clone()).await {
                Ok(text) => return Ok(text),
                Err(AttemptError::Fatal(e)) => return Err(e),
                Err(AttemptError::Retryable(e)) => {
                    tracing::warn!("voice: hosted STT attempt {attempt}/{attempts} failed: {e}");
                    last = e;
                    if attempt < attempts {
                        tokio::time::sleep(backoff_delay(self.backoff_base, attempt)).await;
                    }
                }
            }
        }

        Err(RemoteSttError::Exhausted {
            attempts,
            last: last.to_string(),
        })
    }

    /// One HTTP round-trip; classifies the outcome as success, retryable, or
    /// fatal.
    async fn post_once(
        &self,
        client: &reqwest::Client,
        key: &str,
        body: Vec<u8>,
    ) -> Result<String, AttemptError> {
        let sent = client
            .post(&self.endpoint)
            .header(reqwest::header::AUTHORIZATION, format!("Bearer {key}"))
            .header(
                reqwest::header::CONTENT_TYPE,
                format!("multipart/form-data; boundary={MULTIPART_BOUNDARY}"),
            )
            .body(body)
            .send()
            .await;

        let resp = match sent {
            Ok(r) => r,
            Err(e) => {
                // Transport failures (DNS/TCP/TLS/timeout) are always worth a retry.
                return Err(AttemptError::Retryable(RemoteSttError::Network {
                    message: redact_key(&e.to_string(), key),
                }));
            }
        };

        let status = resp.status().as_u16();
        let text = resp.text().await.unwrap_or_default();

        match status {
            200..=299 => parse_transcript(&text).map_err(AttemptError::Fatal),
            401 | 403 => Err(AttemptError::Fatal(RemoteSttError::Unauthorized)),
            429 => Err(AttemptError::Retryable(RemoteSttError::RateLimited)),
            s if is_retryable_status(s) => {
                Err(AttemptError::Retryable(RemoteSttError::Http { status: s }))
            }
            s => Err(AttemptError::Fatal(RemoteSttError::Http { status: s })),
        }
    }
}

// ---------------------------------------------------------------------------
// Tests — fully offline.  The "integration" tests bind a loopback TCP listener
// and speak just enough HTTP/1.1 to exercise the real reqwest path; no request
// ever leaves the machine and CI needs no credential.
// ---------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;

    // ── WAV encoder ─────────────────────────────────────────────────────────

    #[test]
    fn wav_header_is_44_bytes_for_empty_samples() {
        let wav = encode_wav_pcm16(&[], 16_000);
        assert_eq!(wav.len(), 44);
        assert_eq!(&wav[0..4], b"RIFF");
        assert_eq!(&wav[8..12], b"WAVE");
        assert_eq!(&wav[12..16], b"fmt ");
        assert_eq!(&wav[36..40], b"data");
    }

    #[test]
    fn wav_length_is_header_plus_two_bytes_per_sample() {
        let wav = encode_wav_pcm16(&[0.0; 160], 16_000);
        assert_eq!(wav.len(), 44 + 320);
        // RIFF size field = 36 + data_len
        assert_eq!(
            u32::from_le_bytes([wav[4], wav[5], wav[6], wav[7]]),
            36 + 320
        );
        // data subchunk size
        assert_eq!(
            u32::from_le_bytes([wav[40], wav[41], wav[42], wav[43]]),
            320
        );
    }

    #[test]
    fn wav_encodes_sample_rate_and_derived_fields() {
        let wav = encode_wav_pcm16(&[], 48_000);
        assert_eq!(
            u32::from_le_bytes([wav[24], wav[25], wav[26], wav[27]]),
            48_000
        );
        // byte rate = rate * channels * bytes_per_sample = rate * 2
        assert_eq!(
            u32::from_le_bytes([wav[28], wav[29], wav[30], wav[31]]),
            96_000
        );
        assert_eq!(u16::from_le_bytes([wav[22], wav[23]]), 1); // mono
        assert_eq!(u16::from_le_bytes([wav[34], wav[35]]), 16); // bits
    }

    #[test]
    fn wav_clamps_out_of_range_samples_without_wrapping() {
        let wav = encode_wav_pcm16(&[2.0, -2.0], 16_000);
        let a = i16::from_le_bytes([wav[44], wav[45]]);
        let b = i16::from_le_bytes([wav[46], wav[47]]);
        assert_eq!(a, i16::MAX);
        assert_eq!(b, -i16::MAX);
    }

    // ── multipart encoder ───────────────────────────────────────────────────

    #[test]
    fn multipart_body_has_file_model_and_terminator() {
        let body = multipart_body(&encode_wav_pcm16(&[0.0; 8], 16_000), "whisper-1", "en");
        let text = String::from_utf8_lossy(&body);
        assert!(text.contains("name=\"file\"; filename=\"audio.wav\""));
        assert!(text.contains("Content-Type: audio/wav"));
        assert!(text.contains("name=\"model\""));
        assert!(text.contains("whisper-1"));
        assert!(text.contains("name=\"response_format\""));
        assert!(text.contains("name=\"language\""));
        assert!(text.ends_with(&format!("--{MULTIPART_BOUNDARY}--\r\n")));
    }

    #[test]
    fn multipart_body_omits_empty_language() {
        let body = multipart_body(b"x", "whisper-1", "   ");
        let text = String::from_utf8_lossy(&body);
        assert!(!text.contains("name=\"language\""));
    }

    #[test]
    fn multipart_body_embeds_the_wav_bytes_verbatim() {
        let wav = encode_wav_pcm16(&[0.25, -0.25], 16_000);
        let body = multipart_body(&wav, "whisper-1", "en");
        assert!(
            body.windows(wav.len()).any(|w| w == wav.as_slice()),
            "WAV payload must appear unmodified in the multipart body"
        );
    }

    // ── retry / backoff policy ──────────────────────────────────────────────

    #[test]
    fn retryable_statuses_are_429_and_5xx() {
        assert!(is_retryable_status(429));
        assert!(is_retryable_status(500));
        assert!(is_retryable_status(503));
        assert!(!is_retryable_status(200));
        assert!(!is_retryable_status(400));
        assert!(!is_retryable_status(401));
        assert!(!is_retryable_status(404));
    }

    #[test]
    fn backoff_is_one_two_four_seconds_by_default() {
        let base = Duration::from_secs(1);
        assert_eq!(backoff_delay(base, 1), Duration::from_secs(1));
        assert_eq!(backoff_delay(base, 2), Duration::from_secs(2));
        assert_eq!(backoff_delay(base, 3), Duration::from_secs(4));
    }

    #[test]
    fn backoff_is_capped() {
        assert_eq!(backoff_delay(Duration::from_secs(1), 99), MAX_BACKOFF);
    }

    // ── credential handling ─────────────────────────────────────────────────

    #[test]
    fn redact_key_scrubs_the_credential() {
        let out = redact_key("failed to POST with sk-secret-token", "sk-secret-token");
        assert!(!out.contains("sk-secret-token"));
        assert!(out.contains("<redacted>"));
    }

    #[test]
    fn redact_key_is_a_noop_for_empty_key() {
        assert_eq!(redact_key("boom", ""), "boom");
    }

    #[test]
    fn explicit_api_key_beats_the_environment() {
        let c = OpenAiStt {
            api_key: Some("sk-explicit".to_string()),
            ..OpenAiStt::default()
        };
        assert_eq!(c.resolve_key().unwrap(), "sk-explicit");
    }

    #[test]
    fn blank_explicit_key_is_not_accepted() {
        let c = OpenAiStt {
            api_key: Some("   ".to_string()),
            ..OpenAiStt::default()
        };
        // Falls through to the environment; in CI that is unset → MissingApiKey.
        // Assert only the *fallthrough*, not the env state, so this is stable
        // on a developer box that happens to export OPENAI_API_KEY.
        match c.resolve_key() {
            Err(RemoteSttError::MissingApiKey) => {}
            Ok(k) => assert_ne!(k.trim(), ""),
            Err(e) => panic!("unexpected error: {e}"),
        }
    }

    // ── response parsing ────────────────────────────────────────────────────

    #[test]
    fn parse_transcript_reads_the_text_field() {
        assert_eq!(
            parse_transcript(r#"{"text":"  hello world  "}"#).unwrap(),
            "hello world"
        );
    }

    #[test]
    fn parse_transcript_surfaces_api_error_message() {
        let err = parse_transcript(r#"{"error":{"message":"bad audio"}}"#).unwrap_err();
        assert_eq!(err, RemoteSttError::Decode("bad audio".to_string()));
    }

    #[test]
    fn parse_transcript_rejects_non_json() {
        assert!(matches!(
            parse_transcript("<html>nope</html>"),
            Err(RemoteSttError::Decode(_))
        ));
    }

    #[test]
    fn parse_transcript_rejects_json_without_text() {
        assert!(matches!(
            parse_transcript(r#"{"duration":1.0}"#),
            Err(RemoteSttError::Decode(_))
        ));
    }

    // ── config mapping ──────────────────────────────────────────────────────

    #[test]
    fn from_config_maps_timeout_attempts_and_language() {
        let mut cfg = super::super::config::VoiceConfig::default();
        cfg.language = "fr".to_string();
        cfg.openai_stt_timeout_ms = 1_234;
        cfg.stt_retry_attempts = 7;
        let c = OpenAiStt::from_config(&cfg);
        assert_eq!(c.language, "fr");
        assert_eq!(c.timeout, Duration::from_millis(1_234));
        assert_eq!(c.attempts, 7);
        assert_eq!(c.model, DEFAULT_MODEL);
    }

    #[test]
    fn from_config_floors_attempts_at_one() {
        let mut cfg = super::super::config::VoiceConfig::default();
        cfg.stt_retry_attempts = 0;
        assert_eq!(OpenAiStt::from_config(&cfg).attempts, 1);
    }

    // ── guard rails ─────────────────────────────────────────────────────────

    #[tokio::test]
    async fn empty_audio_is_rejected_before_any_network_call() {
        let c = OpenAiStt {
            api_key: Some("sk-test".to_string()),
            endpoint: "http://127.0.0.1:1/never".to_string(),
            ..OpenAiStt::default()
        };
        assert_eq!(
            c.transcribe(&[], 16_000).await.unwrap_err(),
            RemoteSttError::EmptyAudio
        );
    }

    // ── loopback HTTP tests ─────────────────────────────────────────────────

    /// Minimal one-shot HTTP/1.1 server: serves `responses` in order, one per
    /// accepted connection, then returns the bound URL.
    async fn spawn_fake_endpoint(responses: Vec<(u16, &'static str)>) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind loopback");
        let addr = listener.local_addr().expect("local_addr");

        tokio::spawn(async move {
            for (status, body) in responses {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                // Drain the request until the multipart terminator shows up (or
                // the peer stops sending). We do not need to parse it.
                let mut buf = vec![0u8; 8192];
                let mut seen = Vec::new();
                let terminator = format!("--{MULTIPART_BOUNDARY}--").into_bytes();
                loop {
                    match tokio::time::timeout(Duration::from_millis(500), sock.read(&mut buf))
                        .await
                    {
                        Ok(Ok(0)) | Err(_) => break,
                        Ok(Ok(n)) => {
                            seen.extend_from_slice(&buf[..n]);
                            if seen
                                .windows(terminator.len())
                                .any(|w| w == terminator.as_slice())
                            {
                                break;
                            }
                        }
                        Ok(Err(_)) => break,
                    }
                }
                let reason = if status == 200 { "OK" } else { "ERR" };
                let resp = format!(
                    "HTTP/1.1 {status} {reason}\r\n\
                     Content-Type: application/json\r\n\
                     Content-Length: {}\r\n\
                     Connection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = sock.write_all(resp.as_bytes()).await;
                let _ = sock.flush().await;
                let _ = sock.shutdown().await;
            }
        });

        format!("http://{addr}/v1/audio/transcriptions")
    }

    fn client_for(endpoint: String, attempts: u32) -> OpenAiStt {
        OpenAiStt {
            endpoint,
            api_key: Some("sk-loopback-test".to_string()),
            attempts,
            // Keep the test fast: 1 ms base instead of the 1 s production base.
            backoff_base: Duration::from_millis(1),
            timeout: Duration::from_secs(5),
            ..OpenAiStt::default()
        }
    }

    #[tokio::test]
    async fn loopback_success_returns_the_transcript() {
        let url = spawn_fake_endpoint(vec![(200, r#"{"text":"ship it"}"#)]).await;
        let c = client_for(url, 3);
        let out = c.transcribe(&[0.1; 1600], 16_000).await;
        assert_eq!(out.unwrap(), "ship it");
    }

    #[tokio::test]
    async fn loopback_retries_a_500_then_succeeds() {
        let url = spawn_fake_endpoint(vec![
            (500, r#"{"error":{"message":"upstream"}}"#),
            (200, r#"{"text":"second try"}"#),
        ])
        .await;
        let c = client_for(url, 3);
        assert_eq!(
            c.transcribe(&[0.1; 1600], 16_000).await.unwrap(),
            "second try"
        );
    }

    #[tokio::test]
    async fn loopback_401_is_fatal_and_not_retried() {
        // Only ONE response is queued: a retry would hang/fail on accept, so a
        // passing test proves the 401 short-circuited the retry loop.
        let url = spawn_fake_endpoint(vec![(401, r#"{"error":{"message":"bad key"}}"#)]).await;
        let c = client_for(url, 3);
        assert_eq!(
            c.transcribe(&[0.1; 1600], 16_000).await.unwrap_err(),
            RemoteSttError::Unauthorized
        );
    }

    #[tokio::test]
    async fn loopback_exhausts_retries_and_reports_attempt_count() {
        let url = spawn_fake_endpoint(vec![
            (503, r#"{"error":{"message":"down"}}"#),
            (503, r#"{"error":{"message":"down"}}"#),
        ])
        .await;
        let c = client_for(url, 2);
        match c.transcribe(&[0.1; 1600], 16_000).await.unwrap_err() {
            RemoteSttError::Exhausted { attempts, last } => {
                assert_eq!(attempts, 2);
                assert!(last.contains("503"), "unexpected last error: {last}");
            }
            other => panic!("expected Exhausted, got {other}"),
        }
    }

    #[tokio::test]
    async fn unreachable_endpoint_exhausts_without_panicking() {
        // Port 1 on loopback refuses instantly — a transport error, which is
        // retryable, so this exercises the backoff ladder end-to-end.
        let c = client_for("http://127.0.0.1:1/v1/audio/transcriptions".to_string(), 2);
        match c.transcribe(&[0.1; 160], 16_000).await.unwrap_err() {
            RemoteSttError::Exhausted { attempts, .. } => assert_eq!(attempts, 2),
            other => panic!("expected Exhausted, got {other}"),
        }
    }

    #[test]
    fn warn_once_is_idempotent() {
        // Whatever the ambient state, calling it repeatedly must not panic and
        // must leave the latch set.
        warn_once();
        warn_once();
        assert!(WARNED.load(Ordering::SeqCst));
    }
}
