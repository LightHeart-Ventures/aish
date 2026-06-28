//! S9.3 — durable per‑worker conversation store on disk (replay on attach).
//!
//! ## Why
//! A background coordinator's reasoning is ephemeral: its stdout is captured
//! once (capped at 1 MB) and only a 20‑line stderr tail survives a failure. The
//! full turn‑by‑turn audit ([`crate::turn_audit`]) and the conversation aren't
//! persisted per worker in a re‑loadable form. After a worker exits its chain of
//! thought, tool calls, and intermediate results are gone — so `:attach` (S9.2)
//! has nothing to replay, resume‑from‑checkpoint can't reconstruct the model's
//! prior context, and there's no durable "why did the worker do that?" trail.
//!
//! ## What this module is
//! The reader / meta / retention side of a durable, append‑only **session
//! store** per worker under the per‑worker state volume
//! (`~/.aish/workers/<worker-id>/`, the host path that S9.1 bind‑mounts at
//! `/aish/state` inside a container). Each worker dir holds:
//!
//! - **`meta.json`** — [`WorkerMeta`]: ids, task, repo key, backend/model, the
//!   cross‑referenced SQLite `run_id`, status, timestamps, optional kept branch.
//!   Rewritten **atomically** (temp + rename) on every status change so a reader
//!   never sees a half‑written ownership/status field.
//! - **`transcript.jsonl`** — append‑only, one JSON object per turn‑event
//!   ([`TranscriptRecord`]): user/system/model text, tool calls + (redacted)
//!   args + (capped) outputs, narration, synthesis. Crash‑safe (append + flush;
//!   a torn trailing line is tolerated on read) and bounded by a whole‑file cap
//!   with oldest‑turn truncation.
//! - **`result.txt`** — the final answer, capped, the cross‑container‑boundary
//!   result channel a detached run is read back from.
//!
//! ## Relationship to the rest of the codebase
//! - The **writer** is [`TranscriptWriter`] (held on the
//!   [`crate::session::Session`] for a coordinator run): it emits one
//!   [`TranscriptRecord`] per turn-event from the engine’s tool-call /
//!   narration sites and the coordinator’s per-round synthesis, via the
//!   crash-safe [`append_record`] appender, reusing
//!   [`crate::turn_audit::redact_input`] verbatim so secrets in tool inputs
//!   never hit disk (AC8). `meta.json` is written `running` at coordinator start
//!   and flipped to `done`/`failed` at exit, with `result.txt` carrying the
//!   final answer (the cross-boundary result channel).
//! - The **DB** ([`crate::db`] `coordinator_runs`) stays the durable run‑status
//!   store; `meta.run_id` cross‑references it. Files own the streamable
//!   transcript optimized for replay/tail across the container boundary; the DB
//!   owns durable run status — no duplication of truth (AC3).
//! - The **retention sweeper** ([`sweep_worker_dirs`]) is wired beside
//!   [`crate::worker::sweep_worktrees`] at the startup hook
//!   ([`crate::coordinator::rehydrate`]), with the same conservative ethos:
//!   never reclaim an in‑flight (`status=running`) or work‑bearing (kept branch)
//!   dir, regardless of age (AC7).
//!
//! Everything here is best‑effort: a write that fails (full disk, read‑only
//! mount) is swallowed so the store can never sink a live worker — the cost is
//! only a less‑complete transcript, never a crash.

// S9.3 lands the full store. The WRITER side (`TranscriptWriter`,
// `write_meta_atomic`/`set_status`, `write_result`, `append_record`) is wired
// live into `coordinator::drive` + `engine::run_turn`, and `sweep_worker_dirs`
// at startup. The READER side (`iter_transcript`, `tail_transcript`,
// `to_engine_history`, `read_result`, `meta_path`/`result_path`) is the stable
// surface the dependent cards consume (S9.2 `:attach` replay/resume, S9.4
// detached read-back, S9.5 `:forget`/discovery); allow it until those land
// (mirrors the per-item `allow(dead_code)` S9.x notes in `container.rs`).
#![allow(dead_code)]

use crate::backend::{Msg, Role, ToolCall, ToolResult};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

/// Current `meta.json` schema version. Bumped if the shape changes so a reader
/// can migrate / refuse an unknown future version.
pub const META_SCHEMA: u32 = 1;

/// Whole‑file transcript cap (AC6). A transcript at/over this size is rotated:
/// the oldest records are dropped and a `truncation` marker is written, keeping
/// the newest window. Override with `AISH_WORKER_TRANSCRIPT_CAP` (bytes).
const DEFAULT_TRANSCRIPT_CAP: u64 = 32 * 1024 * 1024; // 32 MB

/// Cap on the persisted `result.txt` (AC1/AC6) — mirrors `worker::CAPTURE_CAP`
/// (the parent's stdout capture cap) so the file‑based and pipe‑based result
/// channels bound identically.
const RESULT_CAP: usize = 1024 * 1024; // 1 MB

/// Cap on any single string value embedded in a transcript record (a tool
/// output body, a long message). Keeps one line small and is a second line of
/// defense against dumping a megabyte into the journal. Mirrors
/// `turn_audit::MAX_OUTPUT_LEN`.
const MAX_RECORD_LEN: usize = 4096;

/// Default retention window (AC7): a worker dir untouched for at least this many
/// days is eligible for the startup sweep — UNLESS it is in‑flight
/// (`status=running`) or holds kept work (a branch). Override with
/// `AISH_WORKER_RETENTION_DAYS`.
const DEFAULT_RETENTION_DAYS: u64 = 30;

// ---------------------------------------------------------------------------
// meta.json
// ---------------------------------------------------------------------------

/// Per‑worker metadata persisted to `meta.json` (AC1). Rewritten atomically on
/// status change. `run_id` cross‑references the SQLite `coordinator_runs` row
/// (AC3); `branch` records a kept worktree so retention never reclaims work.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkerMeta {
    pub worker_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub container_id: Option<String>,
    pub session_id: String,
    pub task: String,
    pub repo_key: String,
    pub backend: String,
    pub model: String,
    pub run_id: String,
    /// `running` | `done` | `failed`.
    pub status: String,
    pub created_at: String,
    pub updated_at: String,
    #[serde(default = "default_schema")]
    pub schema: u32,
    /// The `aish/<id>` branch a worktree‑isolated worker left changes on, if any.
    /// Present ⇒ kept work ⇒ never swept by retention (AC7).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
}

fn default_schema() -> u32 {
    META_SCHEMA
}

impl WorkerMeta {
    /// A fresh `running` meta for a starting worker. Timestamps are set to now.
    pub fn new(
        worker_id: &str,
        session_id: &str,
        task: &str,
        repo_key: &str,
        backend: &str,
        model: &str,
        run_id: &str,
    ) -> Self {
        let now = now_iso8601();
        Self {
            worker_id: worker_id.to_string(),
            container_id: None,
            session_id: session_id.to_string(),
            task: task.to_string(),
            repo_key: repo_key.to_string(),
            backend: backend.to_string(),
            model: model.to_string(),
            run_id: run_id.to_string(),
            status: "running".to_string(),
            created_at: now.clone(),
            updated_at: now,
            schema: META_SCHEMA,
            branch: None,
        }
    }
}

// ---------------------------------------------------------------------------
// transcript.jsonl
// ---------------------------------------------------------------------------

/// One transcript turn‑event (AC2). Serialized as a single JSON line in
/// `transcript.jsonl`. Optional fields are omitted when absent so a record stays
/// compact and a partial trailing line is the only torn‑write failure mode.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TranscriptRecord {
    /// Monotonic sequence number across the whole worker run.
    pub seq: u64,
    pub ts: String,
    /// `user` | `assistant` | `system` | `tool`.
    pub role: String,
    /// `text` | `tool_call` | `tool_result` | `narration` | `synthesis` |
    /// `truncation`.
    pub kind: String,
    /// Correlates a `tool_call` with its `tool_result` (the tool‑call id).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// Tool name (for `tool_call` / `tool_result`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Redacted tool input (for `tool_call`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<Value>,
    /// Text body (for `text`/`narration`/`synthesis`) or the capped tool output
    /// (for `tool_result`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
    /// Whether a `tool_result` was an error.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub is_error: bool,
    /// Token usage for the turn, when the backend reported it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens: Option<TokenUsage>,
    /// For a `truncation` marker: how many oldest records were dropped on
    /// rotation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dropped: Option<u64>,
}

/// Per‑turn token usage embedded in a transcript record.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct TokenUsage {
    #[serde(rename = "in")]
    pub input: u64,
    #[serde(rename = "out")]
    pub output: u64,
}

impl TranscriptRecord {
    /// A plain text turn (user/assistant/system message, narration, synthesis).
    pub fn text(seq: u64, role: &str, kind: &str, text: &str) -> Self {
        Self {
            seq,
            ts: now_iso8601(),
            role: role.to_string(),
            kind: kind.to_string(),
            id: None,
            name: None,
            input: None,
            output: Some(truncate(text, MAX_RECORD_LEN)),
            is_error: false,
            tokens: None,
            dropped: None,
        }
    }

    /// A tool‑call request. `input` is redacted via
    /// [`crate::turn_audit::redact_input`] before it is stored (AC8).
    pub fn tool_call(seq: u64, id: &str, name: &str, input: &Value) -> Self {
        Self {
            seq,
            ts: now_iso8601(),
            role: "assistant".to_string(),
            kind: "tool_call".to_string(),
            id: Some(id.to_string()),
            name: Some(name.to_string()),
            input: Some(crate::turn_audit::redact_input(input)),
            output: None,
            is_error: false,
            tokens: None,
            dropped: None,
        }
    }

    /// A tool‑call result. `output` is capped head‑first.
    pub fn tool_result(seq: u64, id: &str, name: &str, output: &str, is_error: bool) -> Self {
        Self {
            seq,
            ts: now_iso8601(),
            role: "tool".to_string(),
            kind: "tool_result".to_string(),
            id: Some(id.to_string()),
            name: Some(name.to_string()),
            input: None,
            output: Some(truncate(output, MAX_RECORD_LEN)),
            is_error,
            tokens: None,
            dropped: None,
        }
    }
}

// ---------------------------------------------------------------------------
// transcript WRITER (live wiring, AC2)
// ---------------------------------------------------------------------------

/// Append-only transcript WRITER for one worker run — the live wiring (AC2) that
/// emits a [`TranscriptRecord`] per turn-event into `transcript.jsonl`. Held on
/// the [`crate::session::Session`] (as `worker_transcript`) for a headless
/// coordinator run so [`crate::engine::run_turn`] can record each user message,
/// tool call, tool result, and narration, and [`crate::coordinator::drive`] each
/// round’s synthesis. `None` for an interactive session (no per-worker store
/// there) — exactly the same `Some`-only-for-a-coordinator shape as
/// [`crate::turn_audit::TurnAudit`].
///
/// Owns a monotonic `seq`; on a RESUME [`TranscriptWriter::attach`] seeds it
/// past the highest seq already on disk so numbering stays monotonic across the
/// restart boundary (mirrors `turn_audit`’s `position`). Every append is
/// best-effort via [`append_record`] — a write error is swallowed so the store
/// never sinks a live worker.
pub struct TranscriptWriter {
    id: String,
    seq: u64,
}

impl TranscriptWriter {
    /// A writer for worker `id`, continuing the seq sequence past any records an
    /// earlier (crashed) run already wrote — so a resumed run never reuses a seq
    /// and `:attach`/replay sees one ordered stream across the restart.
    pub fn attach(id: &str) -> Self {
        let next = read_records(&transcript_path(id))
            .iter()
            .map(|r| r.seq)
            .max()
            .map(|m| m + 1)
            .unwrap_or(0);
        Self {
            id: id.to_string(),
            seq: next,
        }
    }

    /// The worker id this writer appends for.
    pub fn id(&self) -> &str {
        &self.id
    }

    /// The next seq this writer would assign (also == count of records it has
    /// written this process when starting fresh). Exposed for tests/observability.
    pub fn seq(&self) -> u64 {
        self.seq
    }

    fn next_seq(&mut self) -> u64 {
        let s = self.seq;
        self.seq += 1;
        s
    }

    /// Record a plain text turn-event: a user/assistant/system message, model
    /// `narration`, or round `synthesis`. Empty/whitespace text is skipped (no
    /// empty record), mirroring `turn_audit::synthesis`.
    pub fn record_message(&mut self, role: &str, kind: &str, text: &str) {
        if text.trim().is_empty() {
            return;
        }
        let seq = self.next_seq();
        let _ = append_record(&self.id, &TranscriptRecord::text(seq, role, kind, text));
    }

    /// Record a tool-call request. `input` is redacted inside the record ctor via
    /// [`crate::turn_audit::redact_input`] so secrets never hit disk (AC8).
    pub fn record_tool_call(&mut self, call_id: &str, name: &str, input: &serde_json::Value) {
        let seq = self.next_seq();
        let _ = append_record(
            &self.id,
            &TranscriptRecord::tool_call(seq, call_id, name, input),
        );
    }

    /// Record a tool-call result. `output` is capped head-first inside the ctor.
    pub fn record_tool_result(&mut self, call_id: &str, name: &str, output: &str, is_error: bool) {
        let seq = self.next_seq();
        let _ = append_record(
            &self.id,
            &TranscriptRecord::tool_result(seq, call_id, name, output, is_error),
        );
    }
}

// ---------------------------------------------------------------------------
// paths
// ---------------------------------------------------------------------------

/// The per‑worker state dir: `<state-root>/<sanitized-id>/` (AC1). The root is
/// [`crate::worker::worker_state_root`] (`$AISH_WORKER_STATE_DIR`, else
/// `~/.aish/workers`, else a temp fallback) — the SAME host path S9.1 mounts at
/// `/aish/state`, so the host reader and the in‑container writer see one dir.
pub fn worker_dir(id: &str) -> PathBuf {
    crate::worker::worker_state_root().join(sanitize_id(id))
}

/// `meta.json` path for a worker.
pub fn meta_path(id: &str) -> PathBuf {
    worker_dir(id).join("meta.json")
}

/// `transcript.jsonl` path for a worker.
pub fn transcript_path(id: &str) -> PathBuf {
    worker_dir(id).join("transcript.jsonl")
}

/// `result.txt` path for a worker.
pub fn result_path(id: &str) -> PathBuf {
    worker_dir(id).join("result.txt")
}

/// Sanitize a worker id into a filesystem‑safe leaf (mirrors
/// `WorkerSpec::id_for_state` / container name sanitization). Pure.
fn sanitize_id(id: &str) -> String {
    id.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '-'
            }
        })
        .collect()
}

// ---------------------------------------------------------------------------
// meta IO
// ---------------------------------------------------------------------------

/// Create the worker dir `0700` (AC8). Best‑effort.
fn ensure_worker_dir(id: &str) -> std::io::Result<PathBuf> {
    let dir = worker_dir(id);
    std::fs::create_dir_all(&dir)?;
    set_0700(&dir);
    Ok(dir)
}

/// Tighten a path to owner‑only `0700`. Best‑effort (no‑op on failure).
fn set_0700(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700));
}

/// Load a worker's `meta.json`, or an error if it's missing/unreadable/malformed
/// (the caller treats an error as "history unavailable", AC edge).
pub fn load_meta(id: &str) -> std::io::Result<WorkerMeta> {
    let raw = std::fs::read_to_string(meta_path(id))?;
    serde_json::from_str(&raw).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}

/// Write `meta` atomically (temp + rename, AC technical‑approach): a reader sees
/// either the old file or the new one, never a partial write. `updated_at` is
/// refreshed to now. Best‑effort dir creation with `0700` perms.
pub fn write_meta_atomic(meta: &WorkerMeta) -> std::io::Result<()> {
    let dir = ensure_worker_dir(&meta.worker_id)?;
    let mut to_write = meta.clone();
    to_write.updated_at = now_iso8601();
    let body = serde_json::to_string_pretty(&to_write)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    let tmp = dir.join(format!(".meta.json.tmp.{}", std::process::id()));
    {
        let mut f = File::create(&tmp)?;
        f.write_all(body.as_bytes())?;
        f.flush()?;
    }
    set_0700(&tmp);
    std::fs::rename(&tmp, dir.join("meta.json"))
}

/// Update a worker's status in `meta.json` (atomic rewrite). Convenience for the
/// running→done/failed transition. Returns an error if no meta exists yet.
pub fn set_status(id: &str, status: &str) -> std::io::Result<()> {
    let mut meta = load_meta(id)?;
    meta.status = status.to_string();
    write_meta_atomic(&meta)
}

// ---------------------------------------------------------------------------
// result.txt
// ---------------------------------------------------------------------------

/// Write the worker's final answer to `result.txt`, capped at [`RESULT_CAP`]
/// (AC1). This is the cross‑container‑boundary result channel a detached run is
/// read back from. Best‑effort.
pub fn write_result(id: &str, answer: &str) -> std::io::Result<()> {
    ensure_worker_dir(id)?;
    let capped = truncate(answer, RESULT_CAP);
    std::fs::write(result_path(id), capped.as_bytes())
}

/// Read back the worker's `result.txt`, or `None` when absent/unreadable
/// (returns cleanly so a missing volume reads as "no result yet", AC edge).
pub fn read_result(id: &str) -> Option<String> {
    std::fs::read_to_string(result_path(id)).ok()
}

// ---------------------------------------------------------------------------
// transcript IO
// ---------------------------------------------------------------------------

/// The effective whole‑file transcript cap (env override, else default).
fn transcript_cap() -> u64 {
    std::env::var("AISH_WORKER_TRANSCRIPT_CAP")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(DEFAULT_TRANSCRIPT_CAP)
}

/// Append one record to `transcript.jsonl` (AC2), rotating first if the file has
/// reached the whole‑file cap (AC6). Crash‑safe: append + flush; a write error
/// is swallowed (best‑effort) so the store never sinks a live worker. Returns
/// `Ok(())` even on a swallowed write so callers don't branch on transcript IO.
pub fn append_record(id: &str, record: &TranscriptRecord) -> std::io::Result<()> {
    let dir = ensure_worker_dir(id)?;
    let path = dir.join("transcript.jsonl");
    // Whole‑file cap: rotate BEFORE the append so the file never exceeds the cap
    // by more than one record. A stat per append is cheap relative to the model
    // turn it accompanies.
    if let Ok(meta) = std::fs::metadata(&path) {
        if meta.len() >= transcript_cap() {
            rotate_transcript(&path);
        }
    }
    let mut line = match serde_json::to_string(record) {
        Ok(s) => s,
        Err(_) => return Ok(()), // unserializable → drop, never crash
    };
    line.push('\n');
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(&path) {
        if f.write_all(line.as_bytes()).is_ok() {
            let _ = f.flush();
        }
    }
    Ok(())
}

/// Rotate an over‑cap transcript (AC6): keep the newest window of records whose
/// cumulative serialized size is ≤ half the cap, prepend a `truncation` marker
/// recording how many oldest records were dropped, and rewrite the file
/// atomically. Best‑effort — a failure leaves the file as‑is (the next append
/// simply retries the rotation).
fn rotate_transcript(path: &Path) {
    let records = read_records(path);
    if records.is_empty() {
        return;
    }
    let keep_budget = (transcript_cap() / 2) as usize;
    // Walk newest→oldest, keeping until the budget is spent.
    let mut kept_rev: Vec<&TranscriptRecord> = Vec::new();
    let mut used = 0usize;
    for rec in records.iter().rev() {
        let sz = serde_json::to_string(rec).map(|s| s.len() + 1).unwrap_or(0);
        if used + sz > keep_budget && !kept_rev.is_empty() {
            break;
        }
        used += sz;
        kept_rev.push(rec);
    }
    let dropped = records.len().saturating_sub(kept_rev.len()) as u64;
    if dropped == 0 {
        return; // nothing to gain
    }
    let marker_seq = kept_rev.last().map(|r| r.seq).unwrap_or(0);
    let mut out = String::new();
    let marker = TranscriptRecord {
        seq: marker_seq,
        ts: now_iso8601(),
        role: "system".to_string(),
        kind: "truncation".to_string(),
        id: None,
        name: None,
        input: None,
        output: Some(format!(
            "dropped {dropped} oldest record(s) at transcript cap"
        )),
        is_error: false,
        tokens: None,
        dropped: Some(dropped),
    };
    if let Ok(s) = serde_json::to_string(&marker) {
        out.push_str(&s);
        out.push('\n');
    }
    for rec in kept_rev.iter().rev() {
        if let Ok(s) = serde_json::to_string(rec) {
            out.push_str(&s);
            out.push('\n');
        }
    }
    // Atomic replace so a concurrent reader never sees a half‑rotated file.
    if let Some(dir) = path.parent() {
        let tmp = dir.join(format!(".transcript.jsonl.tmp.{}", std::process::id()));
        if std::fs::write(&tmp, out.as_bytes()).is_ok() {
            let _ = std::fs::rename(&tmp, path);
        }
    }
}

/// Parse a transcript file into its ordered records, skipping any corrupt /
/// torn line (crash‑safety, AC2). Empty on a missing file.
fn read_records(path: &Path) -> Vec<TranscriptRecord> {
    let Ok(file) = File::open(path) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for line in BufReader::new(file).lines() {
        let Ok(line) = line else { break }; // torn final read → stop
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Ok(rec) = serde_json::from_str::<TranscriptRecord>(line) {
            out.push(rec);
        }
        // else: a corrupt line is skipped (a partial trailing write degrades to
        // "read what parses").
    }
    out
}

/// All transcript records for a worker, in order (AC4 — `:attach` replay). Empty
/// when the volume is gone/unreadable, so a removed mount reads cleanly as
/// "history unavailable".
pub fn iter_transcript(id: &str) -> Vec<TranscriptRecord> {
    read_records(&transcript_path(id))
}

/// Tail the transcript from a byte offset for live attach (AC4): returns the
/// records that begin at/after `from_offset` plus the new end offset to resume
/// from on the next poll. A reader tails by offset and never locks the writer,
/// so a concurrent in‑container append is tolerated (AC edge). A torn final line
/// is skipped this poll and re‑read once complete (the returned offset only
/// advances past whole lines).
pub fn tail_transcript(id: &str, from_offset: u64) -> (Vec<TranscriptRecord>, u64) {
    let path = transcript_path(id);
    let Ok(mut file) = File::open(&path) else {
        return (Vec::new(), from_offset);
    };
    if file.seek(SeekFrom::Start(from_offset)).is_err() {
        return (Vec::new(), from_offset);
    }
    let mut reader = BufReader::new(file);
    let mut records = Vec::new();
    let mut consumed = from_offset;
    let mut buf = String::new();
    loop {
        buf.clear();
        match reader.read_line(&mut buf) {
            Ok(0) => break, // EOF
            Ok(n) => {
                // Only advance past a COMPLETE line (terminated by '\n'); a
                // partial trailing line is left for the next poll.
                if !buf.ends_with('\n') {
                    break;
                }
                consumed += n as u64;
                let trimmed = buf.trim();
                if trimmed.is_empty() {
                    continue;
                }
                if let Ok(rec) = serde_json::from_str::<TranscriptRecord>(trimmed) {
                    records.push(rec);
                }
            }
            Err(_) => break,
        }
    }
    (records, consumed)
}

/// Deserialize a worker's transcript back into engine conversation history for
/// resume‑from‑checkpoint (AC5), preserving role + tool‑call/tool‑result
/// structure. Fidelity == journal fidelity: tool inputs are the redacted form
/// and outputs are capped, consistent with the existing replay contract.
///
/// Mapping:
/// - `text`/`narration`/`synthesis` + role `user` → a user [`Msg`];
/// - `text`/`narration`/`synthesis` other roles → an assistant [`Msg`];
/// - `tool_call` → an assistant [`Msg`] carrying the [`ToolCall`];
/// - `tool_result` → a user [`Msg`] carrying the [`ToolResult`] (id matched to
///   the preceding call so the backend can pair them).
pub fn to_engine_history(id: &str) -> Vec<Msg> {
    records_to_history(&iter_transcript(id))
}

/// Pure core of [`to_engine_history`] (unit‑tested without IO).
fn records_to_history(records: &[TranscriptRecord]) -> Vec<Msg> {
    let mut history = Vec::new();
    // The id of the most recent unmatched tool_call, so a tool_result without an
    // explicit id can still adopt the call's id (keeps the backend pairing).
    let mut last_call_id: Option<String> = None;
    for rec in records {
        match rec.kind.as_str() {
            "tool_call" => {
                let id = rec.id.clone().unwrap_or_else(|| format!("seq{}", rec.seq));
                last_call_id = Some(id.clone());
                let call = ToolCall {
                    id,
                    name: rec.name.clone().unwrap_or_default(),
                    args: rec.input.clone().unwrap_or(Value::Null),
                };
                history.push(Msg {
                    role: Role::Assistant,
                    text: String::new(),
                    tool_calls: vec![call],
                    tool_results: vec![],
                    raw: None,
                });
            }
            "tool_result" => {
                let id = rec
                    .id
                    .clone()
                    .or_else(|| last_call_id.take())
                    .unwrap_or_else(|| format!("seq{}", rec.seq));
                let result =
                    ToolResult::text(id, rec.output.clone().unwrap_or_default(), rec.is_error);
                history.push(Msg::tool_results(vec![result]));
            }
            "truncation" => {} // a rotation marker is not conversational
            // text / narration / synthesis (and any unknown text‑bearing kind).
            _ => {
                let text = rec.output.clone().unwrap_or_default();
                if text.is_empty() {
                    continue;
                }
                if rec.role == "user" {
                    history.push(Msg::user(text));
                } else {
                    history.push(Msg {
                        role: Role::Assistant,
                        text,
                        tool_calls: vec![],
                        tool_results: vec![],
                        raw: None,
                    });
                }
            }
        }
    }
    history
}

// ---------------------------------------------------------------------------
// retention (AC7)
// ---------------------------------------------------------------------------

/// The effective retention window in days (env override, else default).
fn retention_days() -> u64 {
    std::env::var("AISH_WORKER_RETENTION_DAYS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(DEFAULT_RETENTION_DAYS)
}

/// Pure retention predicate (AC7): should a worker dir be swept? NEVER when it is
/// in‑flight (`status=running`) or holds kept work (`has_branch`) — regardless of
/// age. Otherwise eligible once it is at least `max_age_days` old. A `None`
/// status (unreadable/absent meta) is treated as not‑running, so an orphaned dir
/// with no meta still ages out. Unit‑tested in isolation from any IO.
fn should_sweep_worker(
    status: Option<&str>,
    has_branch: bool,
    age_days: u64,
    max_age_days: u64,
) -> bool {
    if status == Some("running") || has_branch {
        return false;
    }
    age_days >= max_age_days
}

/// Age of `path` in whole days from its mtime, or 0 when unknown (a metadata
/// error makes the dir look "fresh", so the age rule alone won't reclaim it).
/// Mirrors `worker::dir_age_days`.
fn dir_age_days(path: &Path) -> u64 {
    let modified = match std::fs::metadata(path).and_then(|m| m.modified()) {
        Ok(t) => t,
        Err(_) => return 0,
    };
    match std::time::SystemTime::now().duration_since(modified) {
        Ok(d) => d.as_secs() / 86_400,
        Err(_) => 0,
    }
}

/// Best‑effort startup sweeper (AC7): reclaim aged‑out worker dirs under the
/// state root so they don't accumulate. Mirrors [`crate::worker::sweep_worktrees`]
/// and is wired beside it at the startup hook. The conservative
/// [`should_sweep_worker`] rule guarantees an in‑flight or work‑bearing dir is
/// NEVER removed. Returns the count reclaimed.
pub fn sweep_worker_dirs() -> usize {
    let root = crate::worker::worker_state_root();
    let max_age = retention_days();
    let entries = match std::fs::read_dir(&root) {
        Ok(e) => e,
        Err(_) => return 0, // nothing created yet
    };
    let mut reclaimed = 0usize;
    for entry in entries.flatten() {
        let dir = entry.path();
        if !dir.is_dir() {
            continue;
        }
        // Read the meta to learn status + kept branch; a missing/unreadable meta
        // is treated as "not running, no branch" so a truly orphaned dir ages out.
        let meta = std::fs::read_to_string(dir.join("meta.json"))
            .ok()
            .and_then(|s| serde_json::from_str::<WorkerMeta>(&s).ok());
        let status = meta.as_ref().map(|m| m.status.as_str());
        let has_branch = meta.as_ref().and_then(|m| m.branch.as_ref()).is_some();
        let age = dir_age_days(&dir);
        if should_sweep_worker(status, has_branch, age, max_age) {
            if std::fs::remove_dir_all(&dir).is_ok() {
                reclaimed += 1;
            }
        }
    }
    reclaimed
}

/// Remove a worker dir immediately, regardless of age/status — the `:forget`
/// (S9.5) path. Best‑effort; `Ok(())` even if the dir was already gone.
pub fn forget(id: &str) -> std::io::Result<()> {
    match std::fs::remove_dir_all(worker_dir(id)) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

/// Truncate a string head‑first with a byte‑count marker, snapping to a char
/// boundary so multibyte text is never split. Mirrors `turn_audit::truncate`.
fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!(
        "{}…[truncated {} chars]",
        &s[..end],
        s.chars().count() - s[..end].chars().count()
    )
}

/// Current UTC time as an ISO‑8601 / RFC‑3339 string (`YYYY-MM-DDTHH:MM:SSZ`),
/// computed without a date crate (Howard Hinnant's civil‑from‑days algorithm).
/// Mirrors `turn_audit::now_iso8601` so timestamps are consistent across the
/// transcript and the tool journal.
fn now_iso8601() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (h, m, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mth = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if mth <= 2 { y + 1 } else { y };
    format!("{year:04}-{mth:02}-{d:02}T{h:02}:{m:02}:{s:02}Z")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::{Mutex, OnceLock};

    /// Tests that touch the on‑disk store mutate the process‑global
    /// `AISH_WORKER_STATE_DIR` (and sometimes the cap / retention knobs), so they
    /// MUST run serially — cargo runs tests multi‑threaded by default. This lock
    /// makes every on‑disk test exclusive; pure tests (no env) don't take it.
    fn env_lock() -> &'static Mutex<()> {
        static L: OnceLock<Mutex<()>> = OnceLock::new();
        L.get_or_init(|| Mutex::new(()))
    }

    /// A held lock + a fresh, unique state root. Bind it for the whole test body
    /// (`let _sb = sandbox(...)`) so the lock is held until the test returns.
    struct Sandbox {
        _guard: std::sync::MutexGuard<'static, ()>,
        root: PathBuf,
    }

    /// Acquire the on‑disk‑test lock, point `AISH_WORKER_STATE_DIR` at a fresh
    /// per‑test root, and clear any stray cap/retention overrides so each test
    /// starts from defaults. Recovers a poisoned lock (a prior test panicked) so
    /// one failure doesn't cascade.
    fn sandbox(name: &str) -> Sandbox {
        let guard = env_lock().lock().unwrap_or_else(|e| e.into_inner());
        let root = std::env::temp_dir().join(format!("aish_wstore_{name}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        // SAFETY: the lock guarantees no other test thread reads/writes the env
        // while this test holds the sandbox; the value is a unique per‑test path.
        unsafe {
            std::env::set_var("AISH_WORKER_STATE_DIR", &root);
            std::env::remove_var("AISH_WORKER_TRANSCRIPT_CAP");
            std::env::remove_var("AISH_WORKER_RETENTION_DAYS");
        }
        Sandbox {
            _guard: guard,
            root,
        }
    }

    #[test]
    fn meta_roundtrips_atomically() {
        let _sb = sandbox("meta");
        let meta = WorkerMeta::new(
            "w_meta1",
            "sess-a",
            "do the thing",
            "owner--repo",
            "claude",
            "opus",
            "run_1",
        );
        write_meta_atomic(&meta).unwrap();
        // The temp file is gone (renamed), only meta.json remains.
        assert!(meta_path("w_meta1").exists());
        let loaded = load_meta("w_meta1").unwrap();
        assert_eq!(loaded.worker_id, "w_meta1");
        assert_eq!(loaded.run_id, "run_1");
        assert_eq!(loaded.status, "running");
        assert_eq!(loaded.schema, META_SCHEMA);
        // Status change rewrites atomically.
        set_status("w_meta1", "done").unwrap();
        assert_eq!(load_meta("w_meta1").unwrap().status, "done");
    }

    #[test]
    fn worker_dir_is_0700() {
        use std::os::unix::fs::PermissionsExt;
        let _sb = sandbox("perms");
        let meta = WorkerMeta::new("w_perm", "s", "t", "r", "claude", "m", "run_p");
        write_meta_atomic(&meta).unwrap();
        let mode = std::fs::metadata(worker_dir("w_perm"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o700, "worker dir must be owner‑only (AC8)");
    }

    #[test]
    fn transcript_appends_and_reads_in_order() {
        let _sb = sandbox("tx");
        append_record(
            "w_tx",
            &TranscriptRecord::text(0, "user", "text", "the task"),
        )
        .unwrap();
        append_record(
            "w_tx",
            &TranscriptRecord::tool_call(1, "c1", "read_file", &json!({"path": "Cargo.toml"})),
        )
        .unwrap();
        append_record(
            "w_tx",
            &TranscriptRecord::tool_result(2, "c1", "read_file", "[package]", false),
        )
        .unwrap();
        append_record(
            "w_tx",
            &TranscriptRecord::text(3, "assistant", "synthesis", "done reading"),
        )
        .unwrap();

        let recs = iter_transcript("w_tx");
        assert_eq!(recs.len(), 4);
        assert_eq!(recs[0].kind, "text");
        assert_eq!(recs[1].kind, "tool_call");
        assert_eq!(recs[1].name.as_deref(), Some("read_file"));
        assert_eq!(recs[2].kind, "tool_result");
        assert_eq!(recs[2].output.as_deref(), Some("[package]"));
        // Each line is independently parseable JSON (newline‑delimited).
        let raw = std::fs::read_to_string(transcript_path("w_tx")).unwrap();
        assert_eq!(raw.lines().count(), 4);
        for line in raw.lines() {
            serde_json::from_str::<TranscriptRecord>(line).unwrap();
        }
    }

    #[test]
    fn secrets_are_redacted_in_tool_call_input() {
        let _sb = sandbox("redact");
        let input = json!({"program": "deploy", "env": {"API_KEY": "sk-supersecret"}, "auth_token": "Bearer xyz"});
        append_record(
            "w_red",
            &TranscriptRecord::tool_call(0, "c0", "run_program", &input),
        )
        .unwrap();
        let raw = std::fs::read_to_string(transcript_path("w_red")).unwrap();
        assert!(
            !raw.contains("sk-supersecret"),
            "secret env value must never hit disk (AC8)"
        );
        assert!(
            !raw.contains("Bearer xyz"),
            "secret‑named key must be redacted (AC8)"
        );
        assert!(raw.contains("[redacted]"));
    }

    #[test]
    fn corrupt_and_torn_lines_are_skipped_on_read() {
        let _sb = sandbox("corrupt");
        ensure_worker_dir("w_c").unwrap();
        let good0 =
            serde_json::to_string(&TranscriptRecord::text(0, "user", "text", "hi")).unwrap();
        let good1 =
            serde_json::to_string(&TranscriptRecord::text(1, "assistant", "text", "yo")).unwrap();
        // garbage line, a good line, then a torn (incomplete) trailing line.
        let body = format!("not json at all\n{good0}\n{good1}\n{{\"seq\":2,\"kind\":\"te");
        std::fs::write(transcript_path("w_c"), body).unwrap();
        let recs = iter_transcript("w_c");
        assert_eq!(recs.len(), 2, "two clean records survive the corruption");
        assert_eq!(recs[0].output.as_deref(), Some("hi"));
        assert_eq!(recs[1].output.as_deref(), Some("yo"));
    }

    #[test]
    fn tail_advances_only_past_complete_lines() {
        let _sb = sandbox("tail");
        append_record("w_t", &TranscriptRecord::text(0, "user", "text", "one")).unwrap();
        let (first, off1) = tail_transcript("w_t", 0);
        assert_eq!(first.len(), 1);
        assert!(off1 > 0);
        // A second poll from the offset sees nothing new yet.
        let (none, off2) = tail_transcript("w_t", off1);
        assert!(none.is_empty());
        assert_eq!(off1, off2);
        // After another append, the tail picks up exactly the new record.
        append_record(
            "w_t",
            &TranscriptRecord::text(1, "assistant", "text", "two"),
        )
        .unwrap();
        let (second, _off3) = tail_transcript("w_t", off2);
        assert_eq!(second.len(), 1);
        assert_eq!(second[0].output.as_deref(), Some("two"));
    }

    #[test]
    fn history_roundtrip_preserves_tool_structure() {
        // AC5: transcript → engine history reconstructs role + tool‑call/result
        // structure for a multi‑turn corpus. Pure (no IO) → no sandbox needed.
        let recs = vec![
            TranscriptRecord::text(0, "user", "text", "fix the bug"),
            TranscriptRecord::tool_call(1, "c1", "read_file", &json!({"path": "a.rs"})),
            TranscriptRecord::tool_result(2, "c1", "read_file", "fn main() {}", false),
            TranscriptRecord::text(3, "assistant", "synthesis", "patched it"),
            TranscriptRecord::tool_call(4, "c2", "run_program", &json!({"program": "cargo"})),
            TranscriptRecord::tool_result(5, "c2", "run_program", "error: boom", true),
        ];
        let history = records_to_history(&recs);
        assert_eq!(history.len(), 6);
        // user message
        assert_eq!(history[0].role, Role::User);
        assert_eq!(history[0].text, "fix the bug");
        // assistant tool call, then user tool result, id paired
        assert_eq!(history[1].role, Role::Assistant);
        assert_eq!(history[1].tool_calls.len(), 1);
        assert_eq!(history[1].tool_calls[0].id, "c1");
        assert_eq!(history[1].tool_calls[0].name, "read_file");
        assert_eq!(history[2].tool_results.len(), 1);
        assert_eq!(history[2].tool_results[0].id, "c1");
        assert!(!history[2].tool_results[0].is_error);
        // assistant synthesis text
        assert_eq!(history[3].role, Role::Assistant);
        assert_eq!(history[3].text, "patched it");
        // the errored tool result keeps its flag
        assert!(history[5].tool_results[0].is_error);
        // turn count fidelity: same number of tool calls in == tool-call msgs out
        let calls_in = recs.iter().filter(|r| r.kind == "tool_call").count();
        let calls_out = history.iter().filter(|m| !m.tool_calls.is_empty()).count();
        assert_eq!(calls_in, calls_out, "100% tool-call round-trip");
    }

    #[test]
    fn rotation_drops_oldest_and_marks_truncation() {
        let _sb = sandbox("rotate");
        // Tiny cap so a handful of records trips rotation.
        unsafe { std::env::set_var("AISH_WORKER_TRANSCRIPT_CAP", "600") };
        for n in 0..40u64 {
            append_record(
                "w_rot",
                &TranscriptRecord::text(
                    n,
                    "assistant",
                    "text",
                    &format!("record number {n} padding padding"),
                ),
            )
            .unwrap();
        }
        let recs = iter_transcript("w_rot");
        // The file was bounded — far fewer than 40 records survive.
        assert!(
            recs.len() < 40,
            "rotation must drop oldest records: kept {}",
            recs.len()
        );
        // A truncation marker is present and reports a positive drop count.
        let marker = recs.iter().find(|r| r.kind == "truncation");
        assert!(
            marker.is_some(),
            "a truncation marker must be written on rotation"
        );
        assert!(marker.unwrap().dropped.unwrap_or(0) > 0);
        // The newest record is retained.
        assert!(
            recs.iter()
                .any(|r| r.output.as_deref() == Some("record number 39 padding padding"))
        );
        // The very oldest is gone.
        assert!(
            !recs
                .iter()
                .any(|r| r.output.as_deref() == Some("record number 0 padding padding"))
        );
    }

    #[test]
    fn result_is_written_capped_and_read_back() {
        let _sb = sandbox("result");
        write_result("w_res", "the final answer").unwrap();
        assert_eq!(read_result("w_res").as_deref(), Some("the final answer"));
        // A huge answer is capped.
        let huge = "x".repeat(RESULT_CAP + 5000);
        write_result("w_res", &huge).unwrap();
        let back = read_result("w_res").unwrap();
        assert!(
            back.len() <= RESULT_CAP + 64,
            "result.txt capped at CAPTURE_CAP (AC6)"
        );
        assert!(back.contains("…[truncated"));
    }

    #[test]
    fn should_sweep_keeps_running_and_kept_work_reclaims_aged() {
        // Pure predicate (no IO).
        // Never sweep an in‑flight run, regardless of age.
        assert!(!should_sweep_worker(Some("running"), false, 999, 30));
        // Never sweep a dir with kept work (a branch), regardless of age.
        assert!(!should_sweep_worker(Some("done"), true, 999, 30));
        assert!(!should_sweep_worker(None, true, 999, 30));
        // A done/failed, branchless dir old enough → reclaim.
        assert!(should_sweep_worker(Some("done"), false, 30, 30));
        assert!(should_sweep_worker(Some("failed"), false, 45, 30));
        // An orphaned (no‑meta) old dir ages out.
        assert!(should_sweep_worker(None, false, 30, 30));
        // Too young → keep.
        assert!(!should_sweep_worker(Some("done"), false, 29, 30));
    }

    #[test]
    fn sweep_reclaims_aged_but_spares_running_and_branch() {
        let _sb = sandbox("sweep");
        unsafe { std::env::set_var("AISH_WORKER_RETENTION_DAYS", "0") }; // everything old enough
        // running → spared.
        let mut running = WorkerMeta::new("w_run", "s", "t", "r", "claude", "m", "run_a");
        running.status = "running".into();
        write_meta_atomic(&running).unwrap();
        // done + kept branch → spared.
        let mut kept = WorkerMeta::new("w_keep", "s", "t", "r", "claude", "m", "run_b");
        kept.status = "done".into();
        kept.branch = Some("aish/w_keep".into());
        write_meta_atomic(&kept).unwrap();
        // done, no branch → reclaimed.
        let mut done = WorkerMeta::new("w_done", "s", "t", "r", "claude", "m", "run_c");
        done.status = "done".into();
        write_meta_atomic(&done).unwrap();

        let reclaimed = sweep_worker_dirs();
        assert_eq!(
            reclaimed, 1,
            "only the aged, branchless, finished dir is swept"
        );
        assert!(worker_dir("w_run").exists(), "running dir spared");
        assert!(worker_dir("w_keep").exists(), "kept‑branch dir spared");
        assert!(
            !worker_dir("w_done").exists(),
            "finished branchless dir reclaimed"
        );
    }

    #[test]
    fn forget_removes_immediately_and_is_idempotent() {
        let _sb = sandbox("forget");
        write_meta_atomic(&WorkerMeta::new(
            "w_f", "s", "t", "r", "claude", "m", "run_f",
        ))
        .unwrap();
        assert!(worker_dir("w_f").exists());
        forget("w_f").unwrap();
        assert!(!worker_dir("w_f").exists());
        // A second forget is a no‑op (no error).
        forget("w_f").unwrap();
    }

    #[test]
    fn missing_volume_reads_cleanly() {
        let _sb = sandbox("missing");
        // No dir/files for this id → history unavailable, not an error/panic.
        assert!(iter_transcript("w_absent").is_empty());
        assert!(to_engine_history("w_absent").is_empty());
        assert!(read_result("w_absent").is_none());
        assert!(load_meta("w_absent").is_err());
        let (recs, off) = tail_transcript("w_absent", 0);
        assert!(recs.is_empty());
        assert_eq!(off, 0);
    }

    #[test]
    fn sanitize_id_is_filesystem_safe() {
        assert_eq!(sanitize_id("w_a7k3m2pQ"), "w_a7k3m2pQ");
        assert_eq!(sanitize_id("se/ss:1"), "se-ss-1");
        assert_eq!(sanitize_id("run.123-x"), "run.123-x");
    }

    #[test]
    fn writer_appends_records_with_monotonic_seq() {
        let _sb = sandbox("writer_seq");
        let mut w = TranscriptWriter::attach("w_w1");
        assert_eq!(w.seq(), 0, "fresh writer starts at seq 0");
        w.record_message("user", "text", "the task");
        w.record_tool_call("c1", "read_file", &json!({"path": "Cargo.toml"}));
        w.record_tool_result("c1", "read_file", "[package]", false);
        w.record_message("assistant", "synthesis", "done");
        // An empty message is dropped (no record written).
        w.record_message("assistant", "narration", "   ");
        let recs = iter_transcript("w_w1");
        assert_eq!(recs.len(), 4, "empty message skipped");
        // seqs are 0..3 in order.
        assert_eq!(
            recs.iter().map(|r| r.seq).collect::<Vec<_>>(),
            vec![0, 1, 2, 3]
        );
        assert_eq!(recs[0].role, "user");
        assert_eq!(recs[1].kind, "tool_call");
        assert_eq!(recs[2].kind, "tool_result");
        assert_eq!(recs[3].kind, "synthesis");
    }

    #[test]
    fn writer_attach_continues_seq_across_resume() {
        let _sb = sandbox("writer_resume");
        {
            let mut w = TranscriptWriter::attach("w_w2");
            w.record_message("user", "text", "first");
            w.record_tool_call("c1", "list_dir", &json!({"path": "."}));
            // process “crashes” — writer dropped after 2 records (seq 0,1).
        }
        // A fresh attach (the resume) must continue at seq 2, not reuse 0.
        let mut w = TranscriptWriter::attach("w_w2");
        assert_eq!(w.seq(), 2, "resume continues past the highest on-disk seq");
        w.record_message("assistant", "synthesis", "resumed answer");
        let recs = iter_transcript("w_w2");
        assert_eq!(recs.len(), 3);
        assert_eq!(
            recs.iter().map(|r| r.seq).collect::<Vec<_>>(),
            vec![0, 1, 2]
        );
        assert_eq!(recs[2].output.as_deref(), Some("resumed answer"));
    }

    #[test]
    fn writer_output_round_trips_to_engine_history() {
        // End-to-end (AC2+AC5): records emitted by the WRITER deserialize back
        // into engine history with tool-call structure intact.
        let _sb = sandbox("writer_rt");
        let mut w = TranscriptWriter::attach("w_w3");
        w.record_message("user", "text", "fix the bug");
        w.record_tool_call("c1", "read_file", &json!({"path": "a.rs"}));
        w.record_tool_result("c1", "read_file", "fn main() {}", false);
        w.record_message("assistant", "synthesis", "patched it");
        let history = to_engine_history("w_w3");
        assert_eq!(history.len(), 4);
        assert_eq!(history[0].role, Role::User);
        assert_eq!(history[1].tool_calls[0].name, "read_file");
        assert_eq!(history[2].tool_results[0].id, "c1");
        assert_eq!(history[3].text, "patched it");
    }

    #[test]
    fn writer_redacts_secret_tool_args() {
        let _sb = sandbox("writer_redact");
        let mut w = TranscriptWriter::attach("w_w4");
        w.record_tool_call(
            "c0",
            "run_program",
            &json!({"env": {"API_KEY": "sk-supersecret"}}),
        );
        let raw = std::fs::read_to_string(transcript_path("w_w4")).unwrap();
        assert!(
            !raw.contains("sk-supersecret"),
            "writer must redact secrets (AC8)"
        );
        assert!(raw.contains("[redacted]"));
    }

    #[test]
    fn timestamp_is_rfc3339_zulu() {
        let ts = now_iso8601();
        assert_eq!(ts.len(), 20);
        assert!(ts.ends_with('Z'));
    }
}
