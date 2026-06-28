//! Tier‑1 turn‑audit logging — crash‑resilient, replay‑on‑reconnect tool
//! journaling for the background coordinator.
//!
//! ## Why
//! A background coordinator (`crate::coordinator`) runs a multi‑round agentic
//! loop whose transcript lives only in memory. If the process dies mid‑round
//! (a connection reset, an OOM kill, a host restart), the in‑memory transcript
//! is gone and — without a journal — every already‑executed tool call would be
//! re‑run on resume: a second `git push`, a duplicate file write, a re‑sent
//! notification. That is the failure this module closes.
//!
//! ## What it does
//! On every tool invocation the coordinator writes an append‑only,
//! newline‑delimited JSON record to `.atum/run-${runId}.jsonl` **inside the
//! git worktree** (so it survives a restart and is trivially inspectable with
//! `jq`):
//!
//! ```jsonl
//! {"runId":"run_ab12","turn":0,"tool":"read_file","input":{"path":"Cargo.toml"},"status":"pending","timestamp":"2026-06-01T12:00:00Z"}
//! {"runId":"run_ab12","turn":0,"tool":"read_file","output":{"is_error":false,"content":"[package]\n…"},"status":"complete","timestamp":"2026-06-01T12:00:00Z"}
//! {"runId":"run_ab12","turn":1,"tool":"run_program","input":{"program":"cargo","args":["build"]},"status":"pending","timestamp":"2026-06-01T12:00:03Z"}
//! {"runId":"run_ab12","turn":1,"tool":"run_program","error":"failed to exec cargo: …","status":"failed","timestamp":"2026-06-01T12:00:04Z"}
//! ```
//!
//! Each tool call is one *turn* (a monotonically increasing index across the
//! whole run). A turn is `pending` once started and `complete`/`failed` once
//! the tool returns — so a `pending` with no matching terminal record is the
//! signature of the call that was in flight when the process died.
//!
//! ## Resume contract
//! On reconnect the coordinator re‑attaches to the same `runId`, so
//! [`TurnAudit::attach`] re‑opens the existing journal and parses every
//! *completed* turn into an in‑order replay queue. The coordinator's model is
//! necessarily re‑invoked (its transcript was in memory), but as it re‑issues
//! the same tool calls, [`TurnAudit::begin`] matches each one against the head
//! of the replay queue: a match returns the **recorded** output and the tool is
//! NOT executed again (no duplicate side effect); a divergence (the model took
//! a different path) drains the queue and the run continues live from there.
//! The first live turn keeps the next sequential index, so turn numbers are
//! stable across the resume boundary.
//!
//! ## Safety
//! Tool inputs can carry secrets (an `env` map, an auth header, a large file
//! body). [`redact_input`] strips secret‑shaped values and truncates long
//! strings before anything touches disk — the journal records *that* a call
//! happened and its shape, never a credential or a megabyte of file content.
//!
//! Everything here is best‑effort: a journal write that fails (full disk,
//! read‑only mount) is swallowed so logging can never break a live run — the
//! cost is only a less‑complete audit trail, never a crash.

use crate::backend::ToolResult;
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

/// Cap on any single string value embedded in a journal record. Keeps the
/// `.jsonl` small (a `write_file` content body or a 50 KB program capture would
/// otherwise bloat it) and is a second line of defense against dumping secrets.
const MAX_VALUE_LEN: usize = 512;

/// Cap on a recorded tool *output* body. The output is only needed to feed the
/// model the same result on replay; an over‑long capture is truncated head‑first.
const MAX_OUTPUT_LEN: usize = 4096;

/// One completed turn recovered from a prior run's journal, awaiting replay.
#[derive(Debug, Clone)]
struct Replay {
    turn: u64,
    tool: String,
    /// The *redacted* input that was journaled — matched against the incoming
    /// (also redacted) call to decide replay‑vs‑divergence.
    input: Value,
    output: String,
    is_error: bool,
    /// True once BOTH the `pending` (input) and a terminal (output) record for
    /// this turn have been seen — only then is the turn replayable.
    finalized: bool,
}

/// What [`TurnAudit::begin`] tells the caller to do with a tool call.
pub enum Step {
    /// This call matched a recorded completed turn — skip execution and feed the
    /// model the recorded result. Carries the journaled output + error flag.
    Replay { output: String, is_error: bool },
    /// This is a live call — execute it, then report the result back via
    /// [`TurnAudit::complete`] with this turn index.
    Execute { turn: u64 },
}

/// Append‑only turn journal for one coordinator run, with replay state for a
/// resumed run. Lives on the `Session` (as `turn_audit`) so `engine::run_turn`
/// can wrap each tool call. Absent (`None`) for interactive sessions — only a
/// headless coordinator attaches one.
pub struct TurnAudit {
    run_id: String,
    /// Absolute path to `.atum/run-${runId}.jsonl`, captured once at attach time
    /// so a later `change_dir` can't move where the journal is written.
    path: PathBuf,
    /// Open append handle; `None` if the journal couldn't be opened (logging
    /// then degrades to a no‑op, never an error).
    file: Option<File>,
    /// Completed turns recovered from a prior run, consumed in order on replay.
    replay: VecDeque<Replay>,
    /// Sequential position in the full tool‑call stream (replayed + live).
    position: u64,
    /// How many turns were actually replayed this run (for the summary line).
    replayed: usize,
    /// Total completed turns found in the journal at attach time.
    recovered: usize,
}

impl TurnAudit {
    /// Attach a journal for `run_id` under `base_dir/.atum/`. Re‑opens an
    /// existing journal (the resume path) and parses its completed turns into
    /// the replay queue; a fresh run starts with an empty queue. Never fails —
    /// an unopenable journal yields a no‑op audit so a live run is never blocked.
    pub fn attach(base_dir: &Path, run_id: &str) -> Self {
        let dir = base_dir.join(".atum");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join(format!("run-{run_id}.jsonl"));

        // Parse any prior journal BEFORE opening for append, so we read the
        // completed turns this run will replay.
        let replay = load_completed(&path);
        let recovered = replay.len();
        // The first live turn continues the sequence after all recovered turns,
        // so indices stay monotonic across the resume boundary.
        let position = recovered as u64;

        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .ok();

        Self {
            run_id: run_id.to_string(),
            path,
            file,
            replay: replay.into(),
            position,
            replayed: 0,
            recovered,
        }
    }

    /// Whether this attach recovered any completed turns (i.e. it's a resume).
    #[allow(dead_code)]
    pub fn is_resuming(&self) -> bool {
        self.recovered > 0
    }

    /// A one‑line, human‑readable summary of the resume state, or `None` for a
    /// fresh run. Surfaced by the coordinator at startup so an operator reading
    /// the log sees that prior work is being replayed (AC4).
    pub fn resume_summary(&self) -> Option<String> {
        if self.recovered == 0 {
            return None;
        }
        Some(format!(
            "Audit log loaded from {}: {} completed turn(s) recovered — replaying and resuming from turn {}.",
            self.path.display(),
            self.recovered,
            self.position,
        ))
    }

    /// Begin a tool call. On a resumed run this matches the call against the head
    /// of the replay queue: a match (same tool + same redacted input) returns
    /// [`Step::Replay`] and the caller MUST NOT execute the tool. Otherwise the
    /// queue is drained (the model diverged, or there's nothing left to replay)
    /// and a `pending` record is journaled, returning [`Step::Execute`] with the
    /// turn index to pass back to [`TurnAudit::complete`].
    pub fn begin(&mut self, tool: &str, input: &Value) -> Step {
        let redacted = redact_input(input);

        // Replay path: the next recorded turn must match this call exactly.
        if let Some(front) = self.replay.front() {
            if front.tool == tool && front.input == redacted {
                let entry = self.replay.pop_front().expect("front exists");
                self.replayed += 1;
                self.position = entry.turn + 1;
                return Step::Replay {
                    output: entry.output,
                    is_error: entry.is_error,
                };
            }
            // Divergence: the model took a different path than last time. The
            // remaining recorded turns are stale — drop them and go live.
            self.replay.clear();
        }

        // Live path: assign the next sequential turn, journal a `pending` record.
        let turn = self.position;
        self.position += 1;
        self.write_record(json!({
            "runId": self.run_id,
            "turn": turn,
            "tool": tool,
            "input": redacted,
            "status": "pending",
            "timestamp": now_iso8601(),
        }));
        Step::Execute { turn }
    }

    /// Journal the terminal record for a live turn. A successful or
    /// soft‑declined result writes `complete` (with the — capped — output body);
    /// an errored result writes `failed` (with the error text), so a resume can
    /// tell which turns truly finished.
    pub fn complete(&mut self, turn: u64, tool: &str, result: &ToolResult) {
        let record = if result.is_error {
            json!({
                "runId": self.run_id,
                "turn": turn,
                "tool": tool,
                "error": truncate(&result.content, MAX_OUTPUT_LEN),
                "status": "failed",
                "timestamp": now_iso8601(),
            })
        } else {
            json!({
                "runId": self.run_id,
                "turn": turn,
                "tool": tool,
                "output": {
                    "is_error": false,
                    "content": truncate(&result.content, MAX_OUTPUT_LEN),
                },
                "status": "complete",
                "timestamp": now_iso8601(),
            })
        };
        self.write_record(record);
    }

    /// Journal the model's end‑of‑round *synthesis* — the final narrative text a
    /// coordinator round produced (its tool‑less answer for that round). Tool
    /// calls are already journaled per turn; this captures the reasoning/answer
    /// between them so the `.jsonl` is a complete record of what the agent did
    /// AND said each round. That makes a loop legible: a run that emits the same
    /// synthesis round after round is visibly spinning, which the bare tool log
    /// can hide. Recorded with a distinct `kind`/`status` of `synthesis` and a
    /// monotonically increasing `round` index; `load_completed` ignores it (it
    /// is not a replayable tool turn), so it never affects the resume contract.
    /// Best‑effort like every other write — a failure is swallowed.
    pub fn synthesis(&mut self, round: u64, text: &str) {
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return; // nothing substantive to record
        }
        self.write_record(json!({
            "runId": self.run_id,
            "round": round,
            "kind": "synthesis",
            "status": "synthesis",
            "text": truncate(trimmed, MAX_OUTPUT_LEN),
            "timestamp": now_iso8601(),
        }));
    }

    /// Number of turns replayed so far this run (for the post‑run summary).
    #[allow(dead_code)]
    pub fn replayed(&self) -> usize {
        self.replayed
    }

    /// Append one record as a single JSON line. Best‑effort: a write error is
    /// swallowed (and disables further writes) so journaling never sinks a run.
    fn write_record(&mut self, record: Value) {
        let Some(file) = self.file.as_mut() else {
            return;
        };
        let mut line = record.to_string();
        line.push('\n');
        if file.write_all(line.as_bytes()).is_err() {
            // Disk full / mount went read‑only — stop trying, keep the run alive.
            self.file = None;
            return;
        }
        let _ = file.flush();
    }
}

/// Parse a journal file into its ordered list of *completed* turns. A turn is
/// completed only when it has a terminal (`complete`/`failed`) record; a lone
/// `pending` (the in‑flight call at crash time) is intentionally NOT replayed
/// so it re‑executes live. Malformed lines are skipped, so a torn final write
/// or a partially‑corrupted journal degrades to "replay what parses, resume the
/// rest live" rather than failing the run (AC5).
fn load_completed(path: &Path) -> Vec<Replay> {
    let Ok(file) = File::open(path) else {
        return Vec::new();
    };
    let mut completed: Vec<Replay> = Vec::new();
    for line in BufReader::new(file).lines() {
        let Ok(line) = line else { continue };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue; // skip a corrupted line
        };
        let status = v.get("status").and_then(Value::as_str).unwrap_or("");
        let Some(turn) = v.get("turn").and_then(Value::as_u64) else {
            continue;
        };
        let tool = v
            .get("tool")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        match status {
            "pending" => {
                // Remember the input keyed by turn so the terminal record (which
                // carries no input) can be paired with it.
                let input = v.get("input").cloned().unwrap_or(Value::Null);
                pending_input(&mut completed, turn, &tool, input);
            }
            "complete" => {
                let output = v
                    .get("output")
                    .and_then(|o| o.get("content"))
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                finalize(&mut completed, turn, &tool, output, false);
            }
            "failed" => {
                let output = v
                    .get("error")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                finalize(&mut completed, turn, &tool, output, true);
            }
            _ => {}
        }
    }
    // Only turns that reached a terminal record (input present + output set) are
    // replayable; sort by turn so replay order matches execution order.
    completed.retain(|r| r.is_finalized());
    completed.sort_by_key(|r| r.turn);
    completed
}

impl Replay {
    /// A recovered turn is replayable only once both its `pending` (for the
    /// input) and its terminal record (for the output) have been seen.
    fn is_finalized(&self) -> bool {
        self.finalized
    }
}

/// Stash the input from a `pending` record so a later terminal record for the
/// same turn can be paired with it.
fn pending_input(acc: &mut Vec<Replay>, turn: u64, tool: &str, input: Value) {
    if let Some(existing) = acc.iter_mut().find(|r| r.turn == turn) {
        existing.input = input;
        existing.tool = tool.to_string();
    } else {
        acc.push(Replay {
            turn,
            tool: tool.to_string(),
            input,
            output: String::new(),
            is_error: false,
            finalized: false,
        });
    }
}

/// Apply a terminal (`complete`/`failed`) record to the matching pending turn,
/// marking it replayable. A terminal record with no preceding `pending` (a torn
/// journal) is recorded too, but stays non‑finalized (no input to match on) so
/// it is dropped from the replay set.
fn finalize(acc: &mut Vec<Replay>, turn: u64, tool: &str, output: String, is_error: bool) {
    if let Some(existing) = acc.iter_mut().find(|r| r.turn == turn) {
        existing.output = output;
        existing.is_error = is_error;
        // Finalized only if we have the input from a prior `pending` record.
        existing.finalized = existing.input != Value::Null;
    } else {
        acc.push(Replay {
            turn,
            tool: tool.to_string(),
            input: Value::Null,
            output,
            is_error,
            finalized: false,
        });
    }
}

/// Redact secret‑shaped values and truncate long strings in a tool input before
/// it is journaled. Two rules:
///   * any object key that *looks* like a secret (token / secret / password /
///     api key / auth / credential), and every value under an `env` map, is
///     replaced with `"[redacted]"`;
///   * any string longer than [`MAX_VALUE_LEN`] is truncated with a marker.
/// Applied recursively. The redacted form is also what `begin` matches on, so
/// replay matching is consistent with what was journaled.
pub fn redact_input(input: &Value) -> Value {
    redact_value(input, false)
}

fn redact_value(v: &Value, under_env: bool) -> Value {
    match v {
        Value::Object(map) => {
            let mut out = serde_json::Map::with_capacity(map.len());
            for (k, val) in map {
                if under_env || is_secret_key(k) {
                    out.insert(k.clone(), Value::String("[redacted]".into()));
                } else {
                    let child_under_env = k.eq_ignore_ascii_case("env");
                    out.insert(k.clone(), redact_value(val, child_under_env));
                }
            }
            Value::Object(out)
        }
        Value::Array(items) => {
            Value::Array(items.iter().map(|i| redact_value(i, under_env)).collect())
        }
        Value::String(s) => Value::String(truncate(s, MAX_VALUE_LEN)),
        other => other.clone(),
    }
}

/// Whether an input key name looks like it holds a credential.
fn is_secret_key(key: &str) -> bool {
    let k = key.to_ascii_lowercase();
    const NEEDLES: &[&str] = &[
        "token",
        "secret",
        "password",
        "passwd",
        "apikey",
        "api_key",
        "auth",
        "credential",
        "private_key",
    ];
    NEEDLES.iter().any(|n| k.contains(n))
}

/// Truncate a string head‑first with a byte‑count marker, snapping to a char
/// boundary so multibyte text is never split.
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

    fn tmp_dir(name: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("aish_audit_{name}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn ok_result(content: &str) -> ToolResult {
        ToolResult::text("t", content, false)
    }
    fn err_result(content: &str) -> ToolResult {
        ToolResult::text("t", content, true)
    }

    #[test]
    fn writes_pending_then_complete_as_jsonl() {
        let dir = tmp_dir("write");
        let mut audit = TurnAudit::attach(&dir, "run_w");
        assert!(!audit.is_resuming());
        let step = audit.begin("read_file", &json!({"path": "Cargo.toml"}));
        let turn = match step {
            Step::Execute { turn } => turn,
            Step::Replay { .. } => panic!("fresh run must not replay"),
        };
        assert_eq!(turn, 0);
        audit.complete(turn, "read_file", &ok_result("[package]\nname=aish"));

        // The file is newline‑delimited JSON: every line parses on its own.
        let raw = std::fs::read_to_string(dir.join(".atum/run-run_w.jsonl")).unwrap();
        let lines: Vec<&str> = raw.lines().collect();
        assert_eq!(lines.len(), 2);
        let pending: Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(pending["status"], "pending");
        assert_eq!(pending["turn"], 0);
        assert_eq!(pending["tool"], "read_file");
        assert_eq!(pending["input"]["path"], "Cargo.toml");
        assert!(pending["timestamp"].as_str().unwrap().ends_with('Z'));
        let complete: Value = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(complete["status"], "complete");
        assert_eq!(complete["output"]["is_error"], false);
        assert!(
            complete["output"]["content"]
                .as_str()
                .unwrap()
                .contains("package")
        );
    }

    #[test]
    fn synthesis_is_journaled_and_ignored_on_replay() {
        let dir = tmp_dir("synthesis");
        {
            let mut audit = TurnAudit::attach(&dir, "run_s");
            // A normal completed tool turn...
            let Step::Execute { turn } = audit.begin("read_file", &json!({"path": "a"})) else {
                panic!("expected execute");
            };
            audit.complete(turn, "read_file", &ok_result("A"));
            // ...followed by an end-of-round synthesis line.
            audit.synthesis(0, "  I read the file and found the bug in fn main.  ");
            // An empty/whitespace synthesis is dropped (no record written).
            audit.synthesis(1, "   ");
        }

        // The synthesis line landed as its own `synthesis` record with the text.
        let raw = std::fs::read_to_string(dir.join(".atum/run-run_s.jsonl")).unwrap();
        let synth: Vec<Value> = raw
            .lines()
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .filter(|v| v["status"] == "synthesis")
            .collect();
        assert_eq!(
            synth.len(),
            1,
            "exactly one synthesis record (the empty one skipped)"
        );
        assert_eq!(synth[0]["round"], 0);
        assert!(synth[0]["text"].as_str().unwrap().contains("found the bug"));

        // On reconnect the synthesis record does NOT become a replayable turn —
        // only the one real tool turn is recovered, so the resume contract holds.
        let mut audit = TurnAudit::attach(&dir, "run_s");
        assert_eq!(audit.recovered, 1);
        assert!(matches!(
            audit.begin("read_file", &json!({"path": "a"})),
            Step::Replay { .. }
        ));
        assert!(matches!(
            audit.begin("list_dir", &json!({})),
            Step::Execute { .. }
        ));
    }

    #[test]
    fn failed_tool_call_is_journaled_as_failed() {
        let dir = tmp_dir("failed");
        let mut audit = TurnAudit::attach(&dir, "run_f");
        let Step::Execute { turn } = audit.begin("run_program", &json!({"program": "nope"})) else {
            panic!("expected execute");
        };
        audit.complete(
            turn,
            "run_program",
            &err_result("error: failed to exec nope"),
        );
        let raw = std::fs::read_to_string(dir.join(".atum/run-run_f.jsonl")).unwrap();
        let last: Value = serde_json::from_str(raw.lines().last().unwrap()).unwrap();
        assert_eq!(last["status"], "failed");
        assert!(last["error"].as_str().unwrap().contains("failed to exec"));
    }

    /// The headline resume test: a run records two completed turns, then a fresh
    /// `attach` (the reconnect) replays both — returning recorded output WITHOUT
    /// re‑executing — and assigns the next live turn the following index.
    #[test]
    fn resume_replays_completed_turns_then_continues() {
        let dir = tmp_dir("resume");

        // ── original run: two completed turns, then a crash (process exits) ──
        {
            let mut audit = TurnAudit::attach(&dir, "run_r");
            let Step::Execute { turn } = audit.begin("read_file", &json!({"path": "a.txt"})) else {
                panic!()
            };
            audit.complete(turn, "read_file", &ok_result("contents of a"));
            let Step::Execute { turn } = audit.begin("list_dir", &json!({"path": "."})) else {
                panic!()
            };
            audit.complete(turn, "list_dir", &ok_result("a.txt\nb.txt"));
            // audit dropped here — simulates the process dying after two turns.
        }

        // ── reconnect: same runId, journal recovered ──
        let mut audit = TurnAudit::attach(&dir, "run_r");
        assert!(audit.is_resuming());
        assert_eq!(audit.recovered, 2);
        assert!(
            audit
                .resume_summary()
                .unwrap()
                .contains("resuming from turn 2")
        );

        // The model re‑issues the same first call → replayed, NOT executed.
        match audit.begin("read_file", &json!({"path": "a.txt"})) {
            Step::Replay { output, is_error } => {
                assert_eq!(output, "contents of a");
                assert!(!is_error);
            }
            Step::Execute { .. } => panic!("turn 0 should have replayed"),
        }
        // …and the same second call → replayed too.
        match audit.begin("list_dir", &json!({"path": "."})) {
            Step::Replay { output, .. } => assert_eq!(output, "a.txt\nb.txt"),
            Step::Execute { .. } => panic!("turn 1 should have replayed"),
        }
        assert_eq!(audit.replayed(), 2);

        // The third call is new work → executes live, continuing the sequence.
        match audit.begin("write_file", &json!({"path": "c.txt", "content": "new"})) {
            Step::Execute { turn } => assert_eq!(turn, 2, "first live turn keeps the next index"),
            Step::Replay { .. } => panic!("turn 2 is new — must execute"),
        }
    }

    #[test]
    fn divergence_drains_replay_and_goes_live() {
        let dir = tmp_dir("diverge");
        {
            let mut audit = TurnAudit::attach(&dir, "run_d");
            let Step::Execute { turn } = audit.begin("read_file", &json!({"path": "a.txt"})) else {
                panic!()
            };
            audit.complete(turn, "read_file", &ok_result("a"));
            let Step::Execute { turn } = audit.begin("read_file", &json!({"path": "b.txt"})) else {
                panic!()
            };
            audit.complete(turn, "read_file", &ok_result("b"));
        }
        let mut audit = TurnAudit::attach(&dir, "run_d");
        assert_eq!(audit.recovered, 2);
        // First call matches → replay.
        assert!(matches!(
            audit.begin("read_file", &json!({"path": "a.txt"})),
            Step::Replay { .. }
        ));
        // Second call DIVERGES (different path) → the replay queue is drained and
        // this becomes a live turn, keeping the sequential index.
        match audit.begin(
            "run_program",
            &json!({"program": "cargo", "args": ["test"]}),
        ) {
            Step::Execute { turn } => assert_eq!(turn, 1),
            Step::Replay { .. } => panic!("divergent call must go live"),
        }
        // The stale recorded turn 1 is gone — a subsequent call is live too.
        assert!(matches!(audit.begin("list_dir", &json!({})), Step::Execute { turn } if turn == 2));
    }

    #[test]
    fn in_flight_pending_without_terminal_is_not_replayed() {
        let dir = tmp_dir("inflight");
        // Hand‑craft a journal: turn 0 completed, turn 1 pending (crash mid‑call).
        let path = dir.join(".atum");
        std::fs::create_dir_all(&path).unwrap();
        let jsonl = "\
{\"runId\":\"run_i\",\"turn\":0,\"tool\":\"read_file\",\"input\":{\"path\":\"a\"},\"status\":\"pending\",\"timestamp\":\"2026-01-01T00:00:00Z\"}
{\"runId\":\"run_i\",\"turn\":0,\"tool\":\"read_file\",\"output\":{\"is_error\":false,\"content\":\"A\"},\"status\":\"complete\",\"timestamp\":\"2026-01-01T00:00:00Z\"}
{\"runId\":\"run_i\",\"turn\":1,\"tool\":\"run_program\",\"input\":{\"program\":\"git\",\"args\":[\"push\"]},\"status\":\"pending\",\"timestamp\":\"2026-01-01T00:00:01Z\"}
";
        std::fs::write(path.join("run-run_i.jsonl"), jsonl).unwrap();

        let mut audit = TurnAudit::attach(&dir, "run_i");
        // Only the completed turn 0 is recovered; the in‑flight `git push` (turn
        // 1, pending‑only) is NOT replayed — it must re‑execute so its result is
        // real, not assumed.
        assert_eq!(audit.recovered, 1);
        assert!(matches!(
            audit.begin("read_file", &json!({"path": "a"})),
            Step::Replay { .. }
        ));
        // The git push re‑executes (live), the critical no‑silent‑skip guarantee.
        match audit.begin("run_program", &json!({"program": "git", "args": ["push"]})) {
            Step::Execute { turn } => assert_eq!(turn, 1),
            Step::Replay { .. } => panic!("an in‑flight (pending‑only) call must not be replayed"),
        }
    }

    #[test]
    fn corrupted_journal_lines_are_skipped() {
        let dir = tmp_dir("corrupt");
        let path = dir.join(".atum");
        std::fs::create_dir_all(&path).unwrap();
        // A mix of garbage, a valid completed turn, and a torn final line.
        let jsonl = "\
not json at all
{\"runId\":\"run_c\",\"turn\":0,\"tool\":\"read_file\",\"input\":{\"path\":\"a\"},\"status\":\"pending\",\"timestamp\":\"x\"}
{\"runId\":\"run_c\",\"turn\":0,\"tool\":\"read_file\",\"output\":{\"is_error\":false,\"content\":\"A\"},\"status\":\"complete\",\"timestamp\":\"x\"}
{\"runId\":\"run_c\",\"turn\":1,\"tool\":\"li
";
        std::fs::write(path.join("run-run_c.jsonl"), jsonl).unwrap();

        let mut audit = TurnAudit::attach(&dir, "run_c");
        // Exactly one clean completed turn survives the corruption.
        assert_eq!(audit.recovered, 1);
        assert!(matches!(
            audit.begin("read_file", &json!({"path": "a"})),
            Step::Replay { .. }
        ));
        // Everything after is fresh.
        assert!(matches!(
            audit.begin("list_dir", &json!({})),
            Step::Execute { .. }
        ));
    }

    #[test]
    fn redacts_secrets_and_truncates_long_values() {
        // env map → all values redacted; secret‑named keys → redacted; long
        // strings → truncated; ordinary fields → preserved.
        let input = json!({
            "program": "deploy",
            "env": {"API_KEY": "sk-supersecret", "REGION": "us-east-1"},
            "auth_token": "Bearer abc123",
            "note": "x".repeat(MAX_VALUE_LEN + 50),
            "path": "/etc/hosts",
        });
        let red = redact_input(&input);
        assert_eq!(red["env"]["API_KEY"], "[redacted]");
        assert_eq!(red["env"]["REGION"], "[redacted]");
        assert_eq!(red["auth_token"], "[redacted]");
        assert_eq!(red["path"], "/etc/hosts");
        assert!(red["note"].as_str().unwrap().contains("…[truncated"));
    }

    #[test]
    fn redacted_inputs_still_match_on_replay() {
        // A call carrying a secret records its redacted form; on replay the
        // incoming call is redacted the same way, so it still matches.
        let dir = tmp_dir("redact_match");
        {
            let mut audit = TurnAudit::attach(&dir, "run_rm");
            let input = json!({"program": "curl", "env": {"TOKEN": "secret"}});
            let Step::Execute { turn } = audit.begin("run_program", &input) else {
                panic!()
            };
            audit.complete(turn, "run_program", &ok_result("ok"));
        }
        let mut audit = TurnAudit::attach(&dir, "run_rm");
        let input = json!({"program": "curl", "env": {"TOKEN": "secret"}});
        assert!(
            matches!(audit.begin("run_program", &input), Step::Replay { .. }),
            "a redacted input must still match its journaled redacted form on replay"
        );
        // And the secret never hit disk.
        let raw = std::fs::read_to_string(dir.join(".atum/run-run_rm.jsonl")).unwrap();
        assert!(
            !raw.contains("secret"),
            "the journal must not contain the secret value"
        );
    }

    #[test]
    fn unopenable_journal_is_a_noop_not_a_crash() {
        // Point the base dir at a path whose `.atum` can't be created (a file
        // where the dir should be). attach must still yield a usable no‑op audit.
        let dir = tmp_dir("noop");
        // Make `.atum` a FILE so create_dir_all/open fail.
        std::fs::write(dir.join(".atum"), "blocker").unwrap();
        let mut audit = TurnAudit::attach(&dir, "run_n");
        assert!(!audit.is_resuming());
        // begin/complete must not panic even though no file is open.
        let Step::Execute { turn } = audit.begin("read_file", &json!({"path": "x"})) else {
            panic!("a no‑op audit still drives execution")
        };
        audit.complete(turn, "read_file", &ok_result("y"));
    }

    #[test]
    fn timestamp_is_rfc3339_zulu() {
        let ts = now_iso8601();
        assert_eq!(ts.len(), 20, "YYYY-MM-DDTHH:MM:SSZ is 20 chars: {ts}");
        assert!(ts.ends_with('Z'));
        assert_eq!(&ts[4..5], "-");
        assert_eq!(&ts[10..11], "T");
    }
}
