# Voice Input for aish (push-to-talk STT)

**Status:** Draft / survey (2026-07) · **Owner:** TBD · **Feature flag:** `voice` (off by default)
**Tracking:** FR-334 · Sprint SPR-067

---

## 1. Summary

Add opt-in **voice input** to the aish interactive prompt: a push-to-talk key
(`Ctrl-G`) records microphone audio, transcribes it locally with Whisper, and
inserts the resulting text into the line editor as if typed. Entirely
feature-gated (`--features voice`), off by default, and degrades to a clear
error when the mic/model/deps are unavailable — the prompt is never broken for
users who don't opt in.

## 2. Current state (survey, 2026-07)

Ground truth from the tree at this survey:

| Thing | State |
|---|---|
| Audio deps in `Cargo.toml` | **Absent at HEAD (v0.32.0).** Added in commit `8cd8784` (Jul 3: rubato 3.0, whisper-rs 0.16, cpal, crossterm + `voice`/`voice-api` features) then **reverted.** Only the `#[allow(dead_code)]` markers "kept for future voice input" survive. |
| Whisper/STT/TTS logic | **None.** No module, no `whisper` reference in `src/`. `Ctrl-G` unbound. |
| Branch / PR / worker | **None.** No `voice`/`audio`/`stt` branch, no PR, no peer coordinator. |
| Net status | **Ground zero.** Not stalled-mid-impl — the dependency wiring itself was rolled back. Starting from a clean slate. |

**Implication:** this FR restores the feature-gate scaffolding *and* builds the
implementation. Nothing to salvage; nothing to collide with.

## 3. Integration seams (verified in source)

The line editor already has the exact pattern voice capture needs — three
existing `Ctrl-key` bindings prove the seam:

- **`src/editor.rs`** — `LineEditor`/`RustylineEditor`. Ctrl-key bindings are wired
  in `RustylineEditor::new` via `rl.bind_sequence(KeyEvent::ctrl('X'), EventHandler::Conditional(...))`.
  A conditional handler **cannot reach the `Session`**, so (like `Ctrl-O`) it raises an
  `Arc<AtomicBool>`, bails the editor with `Interrupt`, and `read_line` drains the
  flag into a `ReadOutcome` variant.
- **`ReadOutcome`** (editor.rs:28) — add a `Voice` variant alongside `CtrlO` / `ShiftTab`.
- **`src/repl.rs`** — main loop matches `ReadOutcome`; this is where `Voice` triggers
  capture→transcribe→insert. Precedent: `CtrlO` toggle handling.
- **`src/modelfetch.rs` / `src/hwdetect.rs`** — existing model-download + hardware
  selection plumbing; reuse for fetching the Whisper ggml model and picking a device.
- **`~/.aish/models/`** — model cache location (mirror the llama.cpp GGUF convention).

## 4. Design

### 4.1 Feature flags (restore + extend)

```toml
[dependencies]
cpal       = { version = "0.18", optional = true }  # cross-platform mic capture
rubato     = { version = "3.0",  optional = true }  # resample device rate → 16 kHz
whisper-rs = { version = "0.16", optional = true }  # local Whisper (ggml) STT
crossterm  = { version = "0.29", optional = true }  # raw-mode capture indicator

[features]
voice     = ["dep:cpal", "dep:rubato", "dep:whisper-rs", "dep:crossterm"]
voice-api = ["voice"]  # cloud Whisper API instead of local inference (Phase 3)
```

`default = []` — voice stays out of the standard build (heavy native + model DL).
CI `--no-default-features --locked` gate is unaffected.

### 4.2 Module: `src/voice.rs` (feature-gated)

- `capture::record_until_stop()` — open default input device via `cpal`, stream
  f32 samples into a ring buffer until the stop signal (second `Ctrl-G` / `Esc` /
  silence-timeout). Returns mono f32 @ device rate.
- `resample::to_whisper_pcm()` — `rubato` async resampler → 16 kHz mono f32
  (Whisper's required input).
- `stt::Transcriber` — wraps `whisper-rs` `WhisperContext`; lazy-loads the ggml
  model once, reused across captures. `transcribe(&[f32]) -> String`.
- `model::ensure_model()` — resolve/download the ggml model (default
  `ggml-base.en.bin`, ~148 MB) into `~/.aish/models/whisper/` via the
  `modelfetch` plumbing; checksum-verify.

### 4.3 Control flow

```
Ctrl-G (1st)  → editor raises voice flag → ReadOutcome::Voice
repl loop     → show "🎤 listening…" on statusline (crossterm raw indicator)
              → voice::capture::record_until_stop()
Ctrl-G (2nd)  → stop capture
              → resample → transcribe (spawn_blocking; Whisper is CPU-heavy)
              → insert transcript into the pending line buffer (do NOT auto-submit)
              → user edits/confirms, presses Enter as normal
```

Transcription runs on `tokio::task::spawn_blocking` so the async REPL never
stalls. A visible spinner/status covers the (sub-second → few-second) latency.

### 4.4 Config & degradation

- `~/.aish/config`: `voice.model` (ggml name), `voice.device` (input name),
  `voice.language` (default `en`), `voice.autosubmit` (default false).
- **Graceful failure:** no mic / no model / disabled feature → single-line error
  above the prompt, line buffer untouched. Never panics the editor.
- Non-`voice` builds: `Ctrl-G` stays unbound (default readline behavior).

### 4.5 Phase 3 — `voice-api` (optional)

Swap `stt::Transcriber` for an HTTP call to a hosted Whisper endpoint
(OpenAI/Groq) using the existing `reqwest` client + `${profile:*}` credential
refs. Same capture/resample front-end; no local model download. Chosen at
runtime when `voice-api` is built and a key is configured.

## 5. Risks / open questions

1. **Model size / first-run UX** — 148 MB download on first `Ctrl-G`. Prompt for
   consent; cache aggressively. (`tiny.en` ~75 MB as a lighter default?)
2. **Native build weight** — cpal pulls ALSA/CoreAudio; whisper-rs builds C. Keep
   strictly behind `voice`; document `libasound2-dev` on Linux.
3. **Latency** — base.en on CPU ~1–3 s for a short utterance. Acceptable for
   push-to-talk; revisit GPU/`voice-api` if painful.
4. **Interactive-only** — background coordinators have no TTY/mic; `voice` is a
   foreground-REPL feature only. No coordinator surface.
5. **Cross-platform capture stop** — settle push-to-talk (hold) vs toggle
   (press/press). Toggle is simpler with rustyline's event model → default.

## 6. Out of scope

- TTS / spoken output (this FR is input only).
- Wake-word / always-listening.
- Voice control of `:` commands (dictation only inserts text; user still submits).
