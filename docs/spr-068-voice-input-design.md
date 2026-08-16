# SPR-068 — Voice Input Design Decisions (finalized)

**Status:** Decided (2026-08) · **Supersedes open questions in** `docs/design/voice-input.md` §5
**Tracking:** FR-334 · SPR-068 · TASK-360 · **Feature flag:** `voice` (off by default)
**Prior art:** FR-334 survey/design (PR #567, merged) · feature-gate scaffolding (TASK-361, PR #721, merged)

---

## 0. Purpose

The FR-334 survey (`docs/design/voice-input.md`) left two decisions open and
deferred the implementation contract. This doc **locks** those decisions and
freezes the interfaces that TASK-362…369 build against. No code here — this is
the gating design artifact for the SPR-068 pipeline.

### Decisions locked

| # | Decision | Choice | Rationale |
|---|----------|--------|-----------|
| D1 | Capture control | **Toggle** (`Ctrl-G` press to start, press to stop) | Simplest fit for rustyline's event model; robust on terminals that debounce/repeat keys; accessible (no sustained hold). Hold/push-to-talk rejected — needs key-up events rustyline does not surface. |
| D2 | Default model | **`tiny.en`** (~75 MB) | Fast CPU transcription, "good enough" for command dictation; small first-run download. `base.en` (~148 MB) documented as opt-in via `voice.model` for accuracy. |
| D3 | Insert semantics | **Insert into line buffer only; never auto-submit** | Voice dictates text; the user still edits and presses Enter. `voice.autosubmit` exists but defaults `false`. |
| D4 | Surface | **Foreground interactive REPL only** | Background coordinators have no TTY/mic. No coordinator surface. |

---

## 1. Ctrl-G toggle state machine

```
        Ctrl-G (1st)                stop signal                 transcript ready
 ┌────┐ ───────────► ┌───────────┐ ───────────► ┌──────────────┐ ───────────► ┌────────┐
 │Idle│              │ Recording │              │ Transcribing │              │ Insert │
 └────┘ ◄─────────── └───────────┘              └──────────────┘ ───────────► └────────┘
   ▲   error/empty         │  stop signal =                    │  insert into      │
   │                       │  Ctrl-G(2nd) | Esc(cancel) |       │  line buffer,     │
   └───────────────────────┴──silence-timeout────────────────  └──no auto-submit──►┘
                            cancel path → drop audio → Idle (buffer untouched)
```

| State | Enter action | Exit trigger | Notes |
|-------|--------------|--------------|-------|
| **Idle** | `Ctrl-G` bound → raise voice flag → `ReadOutcome::Voice` | 1st `Ctrl-G` | Default readline otherwise. |
| **Recording** | open input device, stream f32 → ring buffer; show `🎤 listening…` (crossterm raw indicator) | 2nd `Ctrl-G` (stop), `Esc` (cancel), silence-timeout (`voice.silence_ms`, default 2000) | Cancel drops audio, returns to Idle, line buffer untouched. |
| **Transcribing** | resample → whisper on `spawn_blocking`; show spinner | transcript returned or error | REPL stays responsive; Whisper is CPU-heavy. |
| **Insert** | splice transcript at cursor in the pending line buffer | immediate → Idle | Never submits; user edits + Enter as normal. |

Error at any point → single-line message above the prompt, back to **Idle**,
buffer preserved.

---

## 2. Model matrix

| Model | Size (ggml) | CPU latency (short utterance) | Accuracy | Role |
|-------|-------------|-------------------------------|----------|------|
| **`tiny.en`** | ~75 MB | ~0.3–1 s | Good for commands/short dictation | **Default** |
| `base.en` | ~148 MB | ~1–3 s | Noticeably better on long/technical | Opt-in via `voice.model = "base.en"` |
| `small.en` | ~466 MB | ~3–8 s | Best CPU-tier | Documented; not downloaded by default |

- English-only (`.en`) variants are the default lane; multilingual models are
  selectable by name but not first-class in SPR-068.
- Cache: `~/.aish/models/whisper/ggml-<name>.bin`, checksum-verified, mirror the
  llama.cpp GGUF convention. Download is **consent-gated** on first `Ctrl-G`.

---

## 3. Audio pipeline contract

```
cpal input device (native rate, N ch, f32)
        │  capture::record_until_stop() → mono f32 @ device_rate
        ▼
rubato async resampler
        │  resample::to_whisper_pcm() → f32 @ 16 kHz, mono
        ▼
whisper-rs WhisperContext (lazy-loaded once, reused)
        │  stt::Transcriber::transcribe(&[f32]) -> String
        ▼
line buffer splice (no submit)
```

Frozen contract for downstream tasks:

| Symbol | Owner task | Signature (contract) |
|--------|-----------|----------------------|
| `capture::record_until_stop(stop: StopSignal) -> Result<Vec<f32>>` | TASK-362 | mono f32 @ device rate |
| `resample::to_whisper_pcm(&[f32], src_rate: u32) -> Result<Vec<f32>>` | TASK-363 | f32 @ 16 kHz mono |
| `stt::Transcriber::new(model_path) / .transcribe(&[f32]) -> Result<String>` | TASK-364 | lazy `WhisperContext`, reused |
| `model::ensure_model(name) -> Result<PathBuf>` | TASK-365 | resolve/download/verify into cache |
| `ReadOutcome::Voice` + `Ctrl-G` bind | TASK-366 | editor seam (see survey §3) |
| repl `Voice` arm: capture→resample→transcribe→insert | TASK-367 | `spawn_blocking` for whisper |

All symbols live in `src/voice.rs` behind `#[cfg(feature = "voice")]`; the
default/CI build (`--no-default-features --locked`) never compiles them.

---

## 4. Graceful-degradation matrix

| Condition | Behavior | Editor/buffer |
|-----------|----------|---------------|
| Feature not built (`default`) | `Ctrl-G` unbound → default readline | untouched |
| Feature built, **no mic** / device open fails | single-line error above prompt: `voice: no input device` | untouched |
| Feature built, **no model** + user declines download | single-line error: `voice: model not available` | untouched |
| Model download fails / checksum mismatch | error + keep Idle; retriable next `Ctrl-G` | untouched |
| Empty / silence-only capture | no-op back to Idle (optional `(no speech)` hint) | untouched |
| Transcribe error | single-line error; buffer preserved | untouched |

Invariant: **voice never panics the editor and never mutates the line buffer on
any failure path.** Only a successful transcript splices text.

---

## 5. Config keys (`~/.aish/config`)

| Key | Default | Meaning |
|-----|---------|---------|
| `voice.model` | `tiny.en` | ggml model name (resolves to `~/.aish/models/whisper/ggml-<name>.bin`) |
| `voice.device` | *(system default)* | input device name; empty → default input |
| `voice.language` | `en` | Whisper language hint |
| `voice.autosubmit` | `false` | if `true`, press Enter automatically after insert (off by design D3) |
| `voice.silence_ms` | `2000` | silence-timeout that auto-stops Recording |

Unknown/missing keys fall back to defaults; a malformed value logs a warning and
uses the default (never fails the prompt).

---

## 6. Downstream gating

This doc is the contract for the remaining SPR-068 tasks:

- **TASK-362** cpal capture · **TASK-363** rubato resample · **TASK-364** whisper-rs STT ·
  **TASK-365** model fetch → all in `src/voice.rs` (one module, one owner to avoid collisions).
- **TASK-366** editor seam (`ReadOutcome::Voice` + `Ctrl-G`) · **TASK-367** repl wiring (depends on 362–366).
- **TASK-368** config keys + degradation (§4/§5) · **TASK-369** docs + CI feature-build check.

Out of scope (unchanged from survey §6): TTS, wake-word, voice control of `:` commands.
