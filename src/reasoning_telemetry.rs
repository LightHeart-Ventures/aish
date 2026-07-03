//! Reasoning-quality telemetry — a ground-truth log of *when aish's own
//! reasoning is good enough* vs. *when it should reach for the stronger model*.
//!
//! ## Why
//! aish (especially on a weaker/faster frontend) is offered an `escalate` tool
//! that hands a hard sub-problem to a stronger model. The failure mode this
//! module illuminates is the SILENT one: the agent *guesses* through an
//! ambiguous, complex, or risky step instead of escalating, takes a wrong turn,
//! and nobody ever measures it. Without data, the escalate-vs-guess boundary is
//! flown blind — the tool is underused (or overused) and no one can tell.
//!
//! ## What it does
//! It records one append-only, newline-delimited JSON record per *reasoning
//! decision* to `~/.aish/reasoning-telemetry.jsonl` (override with the
//! `AISH_REASONING_LOG` env var). Two record shapes share the file:
//!
//! ```jsonl
//! {"kind":"event","id":"rz_ab12","ts":"2026-06-01T12:00:00Z","decision":"escalated","topic":"diagnose confusing borrow-checker error","complexity":"high","ambiguity":"medium","risk":"medium","rationale":"multi-step lifetime reasoning","outcome":"pending","source":"escalate_tool"}
//! {"kind":"outcome","id":"rz_ab12","ts":"2026-06-01T12:03:00Z","outcome":"correct"}
//! ```
//!
//! An `event` captures the decision at reasoning time (what was reasoned about,
//! its complexity/ambiguity/risk, whether the agent escalated or guessed). A
//! later `outcome` record — keyed by the same `id` — closes the loop once the
//! result is known (did the guess cause a wrong turn?). [`summarize`] folds the
//! updates onto their events and computes the ground-truth model: escalate
//! rate, guess→wrong-turn rate, and both broken down by complexity and risk so
//! the boundary becomes legible ("at HIGH complexity I guess wrong 60% of the
//! time — always escalate there").
//!
//! ## Safety & robustness
//! Everything here is best-effort: a write that fails (full disk, read-only
//! mount) is swallowed so telemetry can never break a live turn. Reads skip
//! malformed lines, so a torn final write degrades to "summarize what parses".
//! Topic/rationale strings are truncated before they touch disk. This module
//! is purely OBSERVATIONAL — it records what happened and never changes agent
//! behavior.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

/// Cap on any recorded free-text field (topic / rationale). Keeps the `.jsonl`
/// small and bounds accidental payloads.
const MAX_TEXT_LEN: usize = 400;

/// A qualitative level for the three reasoning dimensions. Ordered so a summary
/// can bucket low→high. Defaults to [`Level::Medium`] when the caller omits it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Level {
    Low,
    Medium,
    High,
}

impl Level {
    /// Parse a case-insensitive level word; unknown/empty → `Medium` (the safe
    /// middle bucket) so a sloppy caller still logs a usable data point.
    pub fn parse(s: &str) -> Level {
        match s.trim().to_ascii_lowercase().as_str() {
            "low" | "lo" | "l" => Level::Low,
            "high" | "hi" | "h" => Level::High,
            _ => Level::Medium,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Level::Low => "low",
            Level::Medium => "medium",
            Level::High => "high",
        }
    }
}

/// Whether the agent reached for the stronger model or reasoned on its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Decision {
    /// Handed the sub-problem to the stronger model (`escalate` / a deliberate
    /// consult).
    Escalated,
    /// Reasoned through it on the current model without escalating.
    Guessed,
}

impl Decision {
    pub fn parse(s: &str) -> Option<Decision> {
        match s.trim().to_ascii_lowercase().as_str() {
            "escalated" | "escalate" | "consult" | "consulted" => Some(Decision::Escalated),
            "guessed" | "guess" | "self" | "reasoned" => Some(Decision::Guessed),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Decision::Escalated => "escalated",
            Decision::Guessed => "guessed",
        }
    }
}

/// How a reasoning decision panned out. `Pending` is the open state at record
/// time; the loop is closed with `Correct` or `WrongTurn`. `Unknown` marks a
/// decision whose result is genuinely unknowable (never converged).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    /// Result not yet known — the default at record time.
    Pending,
    /// The reasoning held up (right call, no wrong turn).
    Correct,
    /// The reasoning caused a wrong turn (a guess that had to be undone/redone).
    WrongTurn,
    /// The result can't be attributed either way.
    Unknown,
}

impl Outcome {
    pub fn parse(s: &str) -> Option<Outcome> {
        match s.trim().to_ascii_lowercase().replace(['-', ' '], "_").as_str() {
            "pending" | "open" => Some(Outcome::Pending),
            "correct" | "right" | "good" | "ok" => Some(Outcome::Correct),
            "wrong_turn" | "wrong" | "wrongturn" | "bad" | "miss" => Some(Outcome::WrongTurn),
            "unknown" | "unclear" => Some(Outcome::Unknown),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Outcome::Pending => "pending",
            Outcome::Correct => "correct",
            Outcome::WrongTurn => "wrong_turn",
            Outcome::Unknown => "unknown",
        }
    }
}

/// One reasoning-decision data point. Serialized as a `kind:"event"` JSONL
/// record; a later [`Outcome`] update (same `id`) is folded onto it by
/// [`summarize`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReasoningEvent {
    /// Discriminates event vs. outcome records in the shared file.
    #[serde(default = "event_kind")]
    pub kind: String,
    /// Short stable id (`rz_<8hex>`), used to link a later outcome update.
    pub id: String,
    /// ISO-8601 / RFC-3339 UTC timestamp.
    pub ts: String,
    pub decision: Decision,
    /// What was being reasoned about (truncated to [`MAX_TEXT_LEN`]).
    pub topic: String,
    pub complexity: Level,
    pub ambiguity: Level,
    pub risk: Level,
    /// Optional short justification for the decision.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rationale: Option<String>,
    pub outcome: Outcome,
    /// Where the record came from (`escalate_tool` | `self_report`).
    pub source: String,
}

fn event_kind() -> String {
    "event".to_string()
}

impl ReasoningEvent {
    /// Build a fresh event with a generated id and `pending` outcome.
    pub fn new(decision: Decision, topic: impl Into<String>, source: impl Into<String>) -> Self {
        ReasoningEvent {
            kind: event_kind(),
            id: gen_id(),
            ts: now_iso8601(),
            decision,
            topic: truncate(&topic.into(), MAX_TEXT_LEN),
            complexity: Level::Medium,
            ambiguity: Level::Medium,
            risk: Level::Medium,
            rationale: None,
            outcome: Outcome::Pending,
            source: source.into(),
        }
    }

    pub fn with_levels(mut self, complexity: Level, ambiguity: Level, risk: Level) -> Self {
        self.complexity = complexity;
        self.ambiguity = ambiguity;
        self.risk = risk;
        self
    }

    pub fn with_rationale(mut self, rationale: Option<String>) -> Self {
        self.rationale = rationale.map(|r| truncate(&r, MAX_TEXT_LEN));
        self
    }

    // Public builder used by tests and available to future callers that record a
    // decision whose outcome is already known.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn with_outcome(mut self, outcome: Outcome) -> Self {
        self.outcome = outcome;
        self
    }
}

/// Resolve the telemetry log path: `$AISH_REASONING_LOG` when set (used by
/// tests), else `~/.aish/reasoning-telemetry.jsonl`.
pub fn log_path() -> PathBuf {
    if let Ok(p) = std::env::var("AISH_REASONING_LOG") {
        if !p.is_empty() {
            return PathBuf::from(p);
        }
    }
    PathBuf::from(std::env::var("HOME").unwrap_or_default())
        .join(".aish")
        .join("reasoning-telemetry.jsonl")
}

/// Append one JSON record as a single line. Best-effort — a failure (missing
/// parent dir it couldn't create, read-only mount) is swallowed and returns
/// `false` so a caller can note "not logged" without ever erroring a turn.
fn append_line(line: &str) -> bool {
    let path = log_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let Ok(mut file) = OpenOptions::new().create(true).append(true).open(&path) else {
        return false;
    };
    let mut buf = line.to_string();
    buf.push('\n');
    let ok = file.write_all(buf.as_bytes()).is_ok() && file.flush().is_ok();
    // Close the handle before any rotation rename/remove.
    drop(file);
    if ok {
        maybe_rotate(&path);
    }
    ok
}

/// Record a fresh reasoning event. Returns the event id on success (for a later
/// outcome update), or `None` if the write was swallowed.
pub fn record(event: &ReasoningEvent) -> Option<String> {
    let line = serde_json::to_string(event).ok()?;
    if append_line(&line) {
        Some(event.id.clone())
    } else {
        None
    }
}

/// Convenience: record an `escalated` decision (used by the `escalate` tool so
/// every consult lands in the telemetry even without an explicit note). Returns
/// the event id.
pub fn record_escalation(topic: &str) -> Option<String> {
    // An escalate call is, by definition, the agent judging the step too hard to
    // guess — record it at HIGH complexity/risk with the outcome left pending.
    let event = ReasoningEvent::new(Decision::Escalated, topic, "escalate_tool")
        .with_levels(Level::High, Level::Medium, Level::High);
    record(&event)
}

/// Append an outcome update for a prior event id. Returns whether it was
/// written.
pub fn update_outcome(id: &str, outcome: Outcome) -> bool {
    let record = serde_json::json!({
        "kind": "outcome",
        "id": id,
        "ts": now_iso8601(),
        "outcome": outcome.as_str(),
    });
    append_line(&record.to_string())
}

/// A per-bucket tally used in the summary: how many decisions fell in this
/// bucket, how many were guesses, and of the guesses with a known result, how
/// many took a wrong turn.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Bucket {
    pub total: usize,
    pub escalated: usize,
    pub guessed: usize,
    pub guess_correct: usize,
    pub guess_wrong: usize,
}

impl Bucket {
    /// Wrong-turn rate among GUESSES with a known outcome, as a percentage.
    /// `None` when there is no closed-loop guess data yet (avoids a misleading
    /// 0%).
    pub fn guess_wrong_pct(&self) -> Option<u32> {
        let known = self.guess_correct + self.guess_wrong;
        if known == 0 {
            None
        } else {
            Some((self.guess_wrong * 100 / known) as u32)
        }
    }
}

/// The folded, computed ground-truth model over the whole telemetry log.
#[derive(Debug, Default, Clone)]
pub struct Summary {
    pub total: usize,
    pub overall: Bucket,
    /// Buckets keyed by complexity level (low/medium/high).
    pub by_complexity: BTreeMap<Level, Bucket>,
    /// Buckets keyed by risk level.
    pub by_risk: BTreeMap<Level, Bucket>,
}

impl Summary {
    /// Escalate rate over all recorded decisions, as a percentage.
    pub fn escalate_pct(&self) -> Option<u32> {
        if self.total == 0 {
            None
        } else {
            Some((self.overall.escalated * 100 / self.total) as u32)
        }
    }
}

/// Read the telemetry log, fold outcome updates onto their events, and compute
/// the [`Summary`]. Malformed lines are skipped. Returns an empty summary when
/// the log is missing.
///
/// TASK-250: this is memoized. The first call folds the whole log and persists
/// a `<log>-memo.json` sidecar (the folded [`Summary`] plus the per-guess index
/// needed to re-fold late outcomes) tagged with the source file's length +
/// mtime and the byte offset consumed. Subsequent calls that see a pure APPEND
/// (the common case — telemetry is append-only) load the memo and fold ONLY the
/// new tail bytes, so cost is O(new lines) rather than O(whole log). A shrink,
/// in-place rewrite, or rotation invalidates the memo and triggers a full
/// rescan. Set `AISH_REASONING_MEMO_FORCE_RESCAN=1` to always full-scan.
pub fn summarize() -> Summary {
    compute().0
}

// ---------------------------------------------------------------------------
// TASK-250: memoized / incremental summary engine.
// ---------------------------------------------------------------------------

/// Memo schema version — bump to invalidate all on-disk memos after a change to
/// the folded representation.
const MEMO_V: u32 = 1;

/// Diagnostic stats about how [`compute`] produced a [`Summary`]. Exposed for
/// tests that assert the incremental hot path only scans NEW lines; the fields
/// are unread in production (`summarize` discards them).
#[derive(Debug, Clone, Copy)]
#[allow(dead_code)]
struct ScanStats {
    /// True when the summary was served from the memo (whole or incrementally).
    incremental: bool,
    /// Number of source lines parsed during this call.
    lines_scanned: usize,
}

/// A remembered guess: enough to SUBTRACT its prior outcome contribution and
/// ADD a new one when a late/updated outcome line arrives incrementally.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
struct GuessRec {
    complexity: Level,
    risk: Level,
    outcome: Outcome,
}

/// On-disk memo sidecar: the folded summary plus the guess index and the source
/// signature (len + mtime + consumed byte/line offset) that lets us decide
/// between "unchanged", "pure append", and "stale → rescan".
#[derive(Debug, Default, Serialize, Deserialize)]
struct Memo {
    #[serde(default)]
    v: u32,
    source_len: u64,
    #[serde(default)]
    source_mtime_ns: u128,
    computed_from_byte: u64,
    #[serde(default)]
    computed_from_line: u64,
    total: usize,
    overall: Bucket,
    #[serde(default)]
    by_complexity: BTreeMap<String, Bucket>,
    #[serde(default)]
    by_risk: BTreeMap<String, Bucket>,
    #[serde(default)]
    guesses: BTreeMap<String, GuessRec>,
}

/// In-memory working accumulator folded over the log (or its new tail).
#[derive(Default)]
struct Acc {
    total: usize,
    overall: Bucket,
    by_complexity: BTreeMap<Level, Bucket>,
    by_risk: BTreeMap<Level, Bucket>,
    guesses: BTreeMap<String, GuessRec>,
    consumed_bytes: u64,
    consumed_lines: u64,
}

/// Memo sidecar path: `<log-dir>/<stem>-memo.json`.
fn memo_path() -> PathBuf {
    let log = log_path();
    let parent = log.parent().map(|p| p.to_path_buf()).unwrap_or_default();
    let name = log
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("reasoning-telemetry.jsonl");
    let base = name.strip_suffix(".jsonl").unwrap_or(name);
    parent.join(format!("{base}-memo.json"))
}

fn mtime_ns(meta: &std::fs::Metadata) -> u128 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

fn load_memo() -> Option<Memo> {
    let data = std::fs::read(memo_path()).ok()?;
    let memo: Memo = serde_json::from_slice(&data).ok()?;
    if memo.v != MEMO_V {
        return None;
    }
    Some(memo)
}

fn save_memo(acc: &Acc, source_len: u64, source_mtime_ns: u128) {
    let memo = Memo {
        v: MEMO_V,
        source_len,
        source_mtime_ns,
        computed_from_byte: acc.consumed_bytes,
        computed_from_line: acc.consumed_lines,
        total: acc.total,
        overall: acc.overall,
        by_complexity: acc
            .by_complexity
            .iter()
            .map(|(k, v)| (k.as_str().to_string(), *v))
            .collect(),
        by_risk: acc
            .by_risk
            .iter()
            .map(|(k, v)| (k.as_str().to_string(), *v))
            .collect(),
        guesses: acc.guesses.clone(),
    };
    let Ok(json) = serde_json::to_string(&memo) else {
        return;
    };
    let path = memo_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    // Write to a temp sidecar then rename for atomicity; best-effort.
    let tmp = path.with_extension("json.tmp");
    if std::fs::write(&tmp, json.as_bytes()).is_ok() {
        let _ = std::fs::rename(&tmp, &path);
    }
}

fn memo_to_acc(m: Memo) -> Acc {
    let mut acc = Acc {
        total: m.total,
        overall: m.overall,
        guesses: m.guesses,
        consumed_bytes: m.computed_from_byte,
        consumed_lines: m.computed_from_line,
        ..Default::default()
    };
    for (k, v) in m.by_complexity {
        acc.by_complexity.insert(Level::parse(&k), v);
    }
    for (k, v) in m.by_risk {
        acc.by_risk.insert(Level::parse(&k), v);
    }
    acc
}

fn acc_to_summary(acc: &Acc) -> Summary {
    Summary {
        total: acc.total,
        overall: acc.overall,
        by_complexity: acc.by_complexity.clone(),
        by_risk: acc.by_risk.clone(),
    }
}

fn memo_to_summary(m: &Memo) -> Summary {
    let mut s = Summary {
        total: m.total,
        overall: m.overall,
        ..Default::default()
    };
    for (k, v) in &m.by_complexity {
        s.by_complexity.insert(Level::parse(k), *v);
    }
    for (k, v) in &m.by_risk {
        s.by_risk.insert(Level::parse(k), *v);
    }
    s
}

/// Read `path` from `offset` to EOF into a byte buffer (the appended tail).
fn read_from(path: &Path, offset: u64) -> Option<Vec<u8>> {
    let mut f = std::fs::File::open(path).ok()?;
    f.seek(SeekFrom::Start(offset)).ok()?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf).ok()?;
    Some(buf)
}

/// Fold every complete (newline-terminated) line in `data` into `acc`,
/// advancing `consumed_bytes`/`consumed_lines`. `start_offset` is the absolute
/// byte position of `data[0]` in the source file. A trailing line WITHOUT a
/// newline is left unconsumed (a torn/partial final write) and folded later
/// once its terminating newline arrives. Returns the number of lines folded.
fn consume_bytes(acc: &mut Acc, data: &[u8], start_offset: u64) -> usize {
    let mut lines = 0usize;
    let mut line_start = 0usize;
    for (i, &b) in data.iter().enumerate() {
        if b == b'\n' {
            process_raw_line(acc, &data[line_start..i]);
            lines += 1;
            acc.consumed_lines += 1;
            acc.consumed_bytes = start_offset + (i as u64) + 1;
            line_start = i + 1;
        }
    }
    lines
}

/// Parse one raw log line and fold it into `acc`: an `event` bumps the buckets
/// (and, for a guess, records + tallies its embedded outcome); an `outcome`
/// line re-folds a prior guess (subtract old contribution, add new).
fn process_raw_line(acc: &mut Acc, raw: &[u8]) {
    let Ok(text) = std::str::from_utf8(raw) else {
        return;
    };
    let line = text.trim();
    if line.is_empty() {
        return;
    }
    let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
        return; // skip corrupted line
    };
    let kind = v.get("kind").and_then(|k| k.as_str()).unwrap_or("event");
    if kind == "outcome" {
        if let (Some(id), Some(oc)) = (
            v.get("id").and_then(|i| i.as_str()),
            v.get("outcome").and_then(|o| o.as_str()),
        ) {
            if let Some(newoc) = Outcome::parse(oc) {
                if let Some(g) = acc.guesses.get(id).copied() {
                    // Re-fold: retract the old outcome, apply the new one.
                    adjust_guess(acc, g.complexity, g.risk, g.outcome, -1);
                    adjust_guess(acc, g.complexity, g.risk, newoc, 1);
                    if let Some(slot) = acc.guesses.get_mut(id) {
                        slot.outcome = newoc;
                    }
                }
            }
        }
        return;
    }
    if let Ok(ev) = serde_json::from_value::<ReasoningEvent>(v) {
        acc.total += 1;
        bump_counts(acc, &ev);
    }
}

/// Fold a fresh event into the overall/complexity/risk buckets.
fn bump_counts(acc: &mut Acc, ev: &ReasoningEvent) {
    let cx = ev.complexity;
    let rk = ev.risk;
    fn bump(bucket: &mut Bucket, decision: Decision) {
        bucket.total += 1;
        match decision {
            Decision::Escalated => bucket.escalated += 1,
            Decision::Guessed => bucket.guessed += 1,
        }
    }
    bump(&mut acc.overall, ev.decision);
    bump(acc.by_complexity.entry(cx).or_default(), ev.decision);
    bump(acc.by_risk.entry(rk).or_default(), ev.decision);
    if let Decision::Guessed = ev.decision {
        // Apply the event's embedded outcome (usually Pending) and remember the
        // guess so a later outcome line can re-fold it incrementally.
        adjust_guess(acc, cx, rk, ev.outcome, 1);
        acc.guesses.insert(
            ev.id.clone(),
            GuessRec {
                complexity: cx,
                risk: rk,
                outcome: ev.outcome,
            },
        );
    }
}

/// Add (`delta = 1`) or retract (`delta = -1`) a guess outcome's contribution
/// to the correct/wrong counters across the overall + complexity + risk
/// buckets. Pending/Unknown contribute nothing. Counters saturate at zero.
fn adjust_guess(acc: &mut Acc, cx: Level, rk: Level, outcome: Outcome, delta: i64) {
    fn apply(bucket: &mut Bucket, outcome: Outcome, delta: i64) {
        match outcome {
            Outcome::Correct => {
                bucket.guess_correct = (bucket.guess_correct as i64 + delta).max(0) as usize
            }
            Outcome::WrongTurn => {
                bucket.guess_wrong = (bucket.guess_wrong as i64 + delta).max(0) as usize
            }
            Outcome::Pending | Outcome::Unknown => {}
        }
    }
    apply(&mut acc.overall, outcome, delta);
    apply(acc.by_complexity.entry(cx).or_default(), outcome, delta);
    apply(acc.by_risk.entry(rk).or_default(), outcome, delta);
}

/// The memoized core behind [`summarize`]. Returns the folded summary plus
/// [`ScanStats`] describing whether the memo hot path was taken.
fn compute() -> (Summary, ScanStats) {
    let path = log_path();
    let force = std::env::var("AISH_REASONING_MEMO_FORCE_RESCAN")
        .map(|v| {
            let v = v.trim();
            !v.is_empty() && v != "0" && !v.eq_ignore_ascii_case("false")
        })
        .unwrap_or(false);

    // The active file may be missing right after a rotation (it is recreated on
    // the next append) — archives can still hold data, so don't early-return to
    // empty here; fall through to the rotation-aware full recompute below.
    let meta = std::fs::metadata(&path).ok();

    if let (false, Some(meta)) = (force, meta.as_ref()) {
        let cur_len = meta.len();
        let cur_mtime_ns = mtime_ns(meta);
        if let Some(memo) = load_memo() {
            // Unchanged file → serve the memo verbatim (zero new lines).
            if memo.source_len == cur_len && memo.source_mtime_ns == cur_mtime_ns {
                return (
                    memo_to_summary(&memo),
                    ScanStats {
                        incremental: true,
                        lines_scanned: 0,
                    },
                );
            }
            // Pure append (grew, prefix intact) → fold only the new tail.
            if cur_len > memo.source_len && memo.computed_from_byte <= cur_len {
                let mut acc = memo_to_acc(memo);
                let start = acc.consumed_bytes;
                if let Some(tail) = read_from(&path, start) {
                    let scanned = consume_bytes(&mut acc, &tail, start);
                    let summary = acc_to_summary(&acc);
                    save_memo(&acc, cur_len, cur_mtime_ns);
                    return (
                        summary,
                        ScanStats {
                            incremental: true,
                            lines_scanned: scanned,
                        },
                    );
                }
            }
            // Otherwise (shrunk / rewritten / rotated) → fall through to rescan.
        }
    }

    // Full recompute (rotation-aware): fold the retained gzip archives
    // oldest→newest FIRST, then the active file. This reconstructs global append
    // order across a rotation boundary, so a pre-rotation event still counts and
    // an outcome update written after a rotation folds onto its now-archived
    // event. Archive contributions are baked into the memo, so a later pure
    // append only re-scans the active tail (archives can't change without a
    // rotation, and a rotation invalidates the memo anyway).
    let mut acc = Acc::default();
    fold_archives(&mut acc);
    let data = std::fs::read(&path).unwrap_or_default();
    let scanned = consume_bytes(&mut acc, &data, 0);
    let summary = acc_to_summary(&acc);
    // Persist a memo only when the active file exists — its len+mtime signature
    // is what the incremental hot path keys on.
    if let Some(meta) = meta.as_ref() {
        save_memo(&acc, meta.len(), mtime_ns(meta));
    }
    (
        summary,
        ScanStats {
            incremental: false,
            lines_scanned: scanned,
        },
    )
}

/// Fold every retained gzip archive into `acc`, oldest generation first
/// (`.{ROTATE_KEEP}.gz` … `.1.gz`), reconstructing append order so a later
/// outcome update folds onto an event that has since been archived. Best-effort:
/// an unreadable or truncated archive is skipped rather than losing the rest of
/// the summary.
fn fold_archives(acc: &mut Acc) {
    for n in (1..=ROTATE_KEEP).rev() {
        let Ok(f) = std::fs::File::open(archive_path(n)) else {
            continue;
        };
        let mut buf = Vec::new();
        if flate2::read::GzDecoder::new(f).read_to_end(&mut buf).is_ok() {
            for line in buf.split(|&b| b == b'\n') {
                process_raw_line(acc, line);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// TASK-251: size-threshold rotation of reasoning-telemetry.jsonl.
// ---------------------------------------------------------------------------

/// Default rotation threshold in MB; override with `AISH_REASONING_ROTATE_MB`
/// (`0` or negative disables rotation entirely).
const DEFAULT_ROTATE_MB: f64 = 5.0;

/// Number of compressed archive generations to retain (`.1.gz` … `.N.gz`).
const ROTATE_KEEP: usize = 3;

/// Rotation threshold in bytes; `0` means rotation is disabled.
fn rotate_limit_bytes() -> u64 {
    let mb = std::env::var("AISH_REASONING_ROTATE_MB")
        .ok()
        .and_then(|v| v.trim().parse::<f64>().ok())
        .unwrap_or(DEFAULT_ROTATE_MB);
    if mb <= 0.0 {
        return 0;
    }
    (mb * 1_048_576.0) as u64
}

/// Archive path for rotation generation `n`: `<log>.<n>.gz` next to the log.
fn archive_path(n: usize) -> PathBuf {
    let log = log_path();
    let parent = log.parent().map(|p| p.to_path_buf()).unwrap_or_default();
    let name = log
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("reasoning-telemetry.jsonl");
    parent.join(format!("{name}.{n}.gz"))
}

/// Rotate the live log once it reaches the threshold. Best-effort — any failure
/// leaves the live log in place so telemetry is never lost on a rotation error.
fn maybe_rotate(path: &Path) {
    let limit = rotate_limit_bytes();
    if limit == 0 {
        return;
    }
    let Ok(meta) = std::fs::metadata(path) else {
        return;
    };
    if meta.len() < limit {
        return;
    }
    rotate(path);
}

/// Compress the live log into `.1.gz`, shifting older generations up and
/// dropping anything past [`ROTATE_KEEP`]. The live log is then truncated (a
/// fresh, empty file) and the memo invalidated so the next [`summarize`]
/// recomputes cleanly.
fn rotate(path: &Path) {
    // Shift older generations up: .2.gz -> .3.gz, .1.gz -> .2.gz.
    for n in (1..ROTATE_KEEP).rev() {
        let src = archive_path(n);
        if src.exists() {
            let _ = std::fs::rename(&src, &archive_path(n + 1));
        }
    }
    // Compress the live log into generation .1.gz.
    let Ok(bytes) = std::fs::read(path) else {
        return;
    };
    if gzip_to(&bytes, &archive_path(1)).is_ok() {
        // Start a fresh live log and invalidate the memo (its source signature
        // no longer matches the truncated file).
        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_file(memo_path());
    }
    // Drop any generations beyond the retention window (e.g. leftover from a
    // previously larger ROTATE_KEEP).
    for n in (ROTATE_KEEP + 1)..=(ROTATE_KEEP + 8) {
        let _ = std::fs::remove_file(archive_path(n));
    }
}

/// gzip `bytes` into `dst` (pure-Rust miniz_oxide backend).
fn gzip_to(bytes: &[u8], dst: &Path) -> std::io::Result<()> {
    use flate2::write::GzEncoder;
    use flate2::Compression;
    let file = std::fs::File::create(dst)?;
    let mut enc = GzEncoder::new(file, Compression::default());
    enc.write_all(bytes)?;
    enc.finish()?;
    Ok(())
}

/// Render the summary as a compact, human-readable report for the `:reasoning`
/// REPL command.
pub fn render_report(summary: &Summary) -> String {
    if summary.total == 0 {
        return format!(
            "reasoning telemetry: no data yet.\n  Log decisions with the reasoning_note tool (or every escalate is auto-logged).\n  Store: {}",
            log_path().display()
        );
    }

    let mut out = String::new();
    let esc = summary
        .escalate_pct()
        .map(|p| format!("{p}%"))
        .unwrap_or_else(|| "—".into());
    out.push_str(&format!(
        "reasoning telemetry — {} decision(s): {} escalated, {} guessed (escalate rate {esc})\n",
        summary.total, summary.overall.escalated, summary.overall.guessed,
    ));
    let gw = summary
        .overall
        .guess_wrong_pct()
        .map(|p| format!("{p}%"))
        .unwrap_or_else(|| "no closed-loop data".into());
    out.push_str(&format!(
        "guess outcomes: {} correct, {} wrong-turn (guess wrong-turn rate {gw})\n",
        summary.overall.guess_correct, summary.overall.guess_wrong,
    ));

    out.push_str("\nby complexity (guess wrong-turn rate → escalate more where it's high):\n");
    for lvl in [Level::Low, Level::Medium, Level::High] {
        let b = summary.by_complexity.get(&lvl).copied().unwrap_or_default();
        out.push_str(&render_bucket_row(lvl.as_str(), &b));
    }
    out.push_str("\nby risk:\n");
    for lvl in [Level::Low, Level::Medium, Level::High] {
        let b = summary.by_risk.get(&lvl).copied().unwrap_or_default();
        out.push_str(&render_bucket_row(lvl.as_str(), &b));
    }
    out.push_str(&format!("\nstore: {}", log_path().display()));
    out
}

fn render_bucket_row(label: &str, b: &Bucket) -> String {
    let wrong = b
        .guess_wrong_pct()
        .map(|p| format!("{p}%"))
        .unwrap_or_else(|| "—".into());
    format!(
        "  {label:<7} {} decision(s) · {} esc / {} guess · guess wrong-turn {wrong}\n",
        b.total, b.escalated, b.guessed,
    )
}

/// Truncate a string to `max` chars (char-boundary safe) with a marker.
fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let cut: String = s.chars().take(max).collect();
    format!("{cut}…")
}

/// Short random id `rz_<8 hex>` derived from time + a counter — unique enough for
/// linking outcome updates within a session without pulling in a uuid crate.
fn gen_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let mix = nanos ^ (n.wrapping_mul(0x9E37_79B9_7F4A_7C15));
    format!("rz_{:08x}", (mix & 0xFFFF_FFFF) as u32)
}

/// Current UTC time as an ISO-8601 / RFC-3339 string (`YYYY-MM-DDTHH:MM:SSZ`),
/// computed without a date crate (Howard Hinnant's civil-from-days algorithm) —
/// mirrors `turn_audit::now_iso8601`.
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
    use std::sync::{Mutex, MutexGuard};

    // The log path is process-global (an env var), so tests that touch it must
    // not run concurrently. Serialize them behind this mutex.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    struct TestLog {
        _guard: MutexGuard<'static, ()>,
        path: PathBuf,
    }

    impl TestLog {
        fn new(name: &str) -> Self {
            let guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            let path = std::env::temp_dir().join(format!(
                "aish_reasoning_{name}_{}.jsonl",
                std::process::id()
            ));
            let _ = std::fs::remove_file(&path);
            unsafe {
                std::env::set_var("AISH_REASONING_LOG", &path);
            }
            // Clear any stale sidecar/memo + rotation archives from a prior run
            // that reused this name (same pid), so each test starts clean.
            let _ = std::fs::remove_file(memo_path());
            for n in 1..=(ROTATE_KEEP + 8) {
                let _ = std::fs::remove_file(archive_path(n));
            }
            TestLog {
                _guard: guard,
                path,
            }
        }
    }

    impl Drop for TestLog {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.path);
            let _ = std::fs::remove_file(memo_path());
            for n in 1..=(ROTATE_KEEP + 8) {
                let _ = std::fs::remove_file(archive_path(n));
            }
            unsafe {
                std::env::remove_var("AISH_REASONING_LOG");
                std::env::remove_var("AISH_REASONING_ROTATE_MB");
                std::env::remove_var("AISH_REASONING_MEMO_FORCE_RESCAN");
            }
        }
    }

    #[test]
    fn level_and_decision_and_outcome_parse() {
        assert_eq!(Level::parse("HIGH"), Level::High);
        assert_eq!(Level::parse("lo"), Level::Low);
        assert_eq!(Level::parse("whatever"), Level::Medium);
        assert_eq!(Decision::parse("Escalate"), Some(Decision::Escalated));
        assert_eq!(Decision::parse("guess"), Some(Decision::Guessed));
        assert_eq!(Decision::parse("nope"), None);
        assert_eq!(Outcome::parse("wrong-turn"), Some(Outcome::WrongTurn));
        assert_eq!(Outcome::parse("correct"), Some(Outcome::Correct));
        assert_eq!(Outcome::parse("mystery"), None);
    }

    #[test]
    fn record_then_summarize_counts_escalate_and_guess() {
        let _log = TestLog::new("counts");
        record(
            &ReasoningEvent::new(Decision::Escalated, "hard borrow error", "self_report")
                .with_levels(Level::High, Level::High, Level::High),
        )
        .unwrap();
        record(
            &ReasoningEvent::new(Decision::Guessed, "rename a var", "self_report")
                .with_levels(Level::Low, Level::Low, Level::Low)
                .with_outcome(Outcome::Correct),
        )
        .unwrap();

        let s = summarize();
        assert_eq!(s.total, 2);
        assert_eq!(s.overall.escalated, 1);
        assert_eq!(s.overall.guessed, 1);
        assert_eq!(s.escalate_pct(), Some(50));
        // The low-complexity guess was correct.
        let low = s.by_complexity.get(&Level::Low).copied().unwrap();
        assert_eq!(low.guessed, 1);
        assert_eq!(low.guess_correct, 1);
        assert_eq!(low.guess_wrong, 0);
    }

    #[test]
    fn outcome_update_is_folded_onto_event() {
        let _log = TestLog::new("fold");
        let id = record(
            &ReasoningEvent::new(Decision::Guessed, "risky migration", "self_report")
                .with_levels(Level::High, Level::High, Level::High),
        )
        .unwrap();
        // Loop open: outcome pending → not yet counted as wrong/correct.
        let before = summarize();
        assert_eq!(before.overall.guess_wrong, 0);
        assert_eq!(before.overall.guess_wrong_pct(), None);

        // Close the loop: the guess took a wrong turn.
        assert!(update_outcome(&id, Outcome::WrongTurn));
        let after = summarize();
        assert_eq!(after.overall.guessed, 1);
        assert_eq!(after.overall.guess_wrong, 1);
        assert_eq!(after.overall.guess_wrong_pct(), Some(100));
        // And it shows up in the HIGH complexity bucket — the ground-truth signal.
        let high = after.by_complexity.get(&Level::High).copied().unwrap();
        assert_eq!(high.guess_wrong_pct(), Some(100));
    }

    #[test]
    fn escalation_helper_records_high_complexity_pending() {
        let _log = TestLog::new("escalation");
        let id = record_escalation("plan a multi-step refactor").unwrap();
        assert!(id.starts_with("rz_"));
        let s = summarize();
        assert_eq!(s.total, 1);
        assert_eq!(s.overall.escalated, 1);
        let high = s.by_complexity.get(&Level::High).copied().unwrap();
        assert_eq!(high.escalated, 1);
    }

    #[test]
    fn corrupted_and_dangling_lines_are_skipped() {
        let _log = TestLog::new("corrupt");
        // A dangling outcome (no matching event), a garbage line, then a good event.
        assert!(update_outcome("rz_missing", Outcome::Correct));
        append_line("this is not json");
        record(&ReasoningEvent::new(
            Decision::Guessed,
            "ok event",
            "self_report",
        ))
        .unwrap();
        let s = summarize();
        assert_eq!(s.total, 1, "only the one clean event survives");
        assert_eq!(s.overall.guessed, 1);
    }

    #[test]
    fn missing_log_summarizes_to_empty() {
        let _log = TestLog::new("missing");
        // No writes at all → the file doesn't exist.
        let s = summarize();
        assert_eq!(s.total, 0);
        assert_eq!(s.escalate_pct(), None);
        // Report renders the "no data yet" guidance without panicking.
        assert!(render_report(&s).contains("no data yet"));
    }

    #[test]
    fn report_renders_all_buckets() {
        let _log = TestLog::new("report");
        record(&ReasoningEvent::new(Decision::Escalated, "a", "self_report")).unwrap();
        record(
            &ReasoningEvent::new(Decision::Guessed, "b", "self_report")
                .with_levels(Level::High, Level::High, Level::High)
                .with_outcome(Outcome::WrongTurn),
        )
        .unwrap();
        let report = render_report(&summarize());
        assert!(report.contains("reasoning telemetry"));
        assert!(report.contains("by complexity"));
        assert!(report.contains("by risk"));
        assert!(report.contains("high"));
    }

    #[test]
    fn topic_is_truncated_before_disk() {
        let _log = TestLog::new("trunc");
        let long = "x".repeat(MAX_TEXT_LEN + 100);
        let ev = ReasoningEvent::new(Decision::Guessed, long, "self_report");
        assert!(ev.topic.chars().count() <= MAX_TEXT_LEN + 1); // +1 for the ellipsis
        assert!(ev.topic.ends_with('…'));
    }

    // -------------------------------------------------------------------
    // TASK-250: memoized / incremental summary.
    // -------------------------------------------------------------------

    #[test]
    fn memo_is_written_and_reused_incrementally() {
        let _log = TestLog::new("memo_incr");
        record(&ReasoningEvent::new(Decision::Escalated, "a", "self_report")).unwrap();
        // First summarize computes from scratch and writes the memo sidecar.
        let (s1, st1) = compute();
        assert_eq!(s1.total, 1);
        assert!(!st1.incremental, "first pass is a full scan");
        assert!(memo_path().exists(), "memo sidecar persisted");

        // A pure re-read with no new lines must serve straight from the memo.
        let (s2, st2) = compute();
        assert_eq!(s2.total, 1);
        assert!(st2.incremental);
        assert_eq!(st2.lines_scanned, 0, "unchanged file scans nothing");

        // Append one more event → only the NEW tail line is folded.
        record(&ReasoningEvent::new(Decision::Guessed, "b", "self_report")).unwrap();
        let (s3, st3) = compute();
        assert_eq!(s3.total, 2);
        assert!(st3.incremental, "append is the incremental hot path");
        assert_eq!(st3.lines_scanned, 1, "only the appended line is scanned");
        assert_eq!(s3.overall.escalated, 1);
        assert_eq!(s3.overall.guessed, 1);
    }

    #[test]
    fn incremental_matches_full_rescan_with_late_outcome() {
        let _log = TestLog::new("memo_outcome");
        let id = record(
            &ReasoningEvent::new(Decision::Guessed, "risky", "self_report")
                .with_levels(Level::High, Level::High, Level::High),
        )
        .unwrap();
        // Prime the memo.
        let primed = summarize();
        assert_eq!(primed.overall.guess_wrong, 0);

        // Late outcome arrives → incremental re-fold must reflect the wrong turn.
        assert!(update_outcome(&id, Outcome::WrongTurn));
        let incr = summarize();
        assert_eq!(incr.overall.guess_wrong, 1);
        assert_eq!(incr.overall.guess_wrong_pct(), Some(100));

        // A forced full rescan must agree byte-for-byte with the incremental result.
        unsafe {
            std::env::set_var("AISH_REASONING_MEMO_FORCE_RESCAN", "1");
        }
        let (full, st) = compute();
        unsafe {
            std::env::remove_var("AISH_REASONING_MEMO_FORCE_RESCAN");
        }
        assert!(!st.incremental);
        assert_eq!(full.total, incr.total);
        assert_eq!(full.overall, incr.overall);
        let hi_full = full.by_complexity.get(&Level::High).copied().unwrap();
        let hi_incr = incr.by_complexity.get(&Level::High).copied().unwrap();
        assert_eq!(hi_full, hi_incr);
    }

    #[test]
    fn memo_invalidated_on_shrink_triggers_rescan() {
        let _log = TestLog::new("memo_shrink");
        record(&ReasoningEvent::new(Decision::Guessed, "one", "self_report")).unwrap();
        record(&ReasoningEvent::new(Decision::Guessed, "two", "self_report")).unwrap();
        assert_eq!(summarize().total, 2);

        // Rewrite the log smaller (as a rotation/truncate would) — the memo's
        // source signature no longer matches, so the next compute full-rescans.
        std::fs::write(
            log_path(),
            format!(
                "{}\n",
                serde_json::to_string(&ReasoningEvent::new(
                    Decision::Escalated,
                    "fresh",
                    "self_report"
                ))
                .unwrap()
            ),
        )
        .unwrap();
        let (s, st) = compute();
        assert_eq!(s.total, 1, "recomputed against the shrunken file");
        assert_eq!(s.overall.escalated, 1);
        assert!(!st.incremental, "shrink forces a full rescan");
    }

    // -------------------------------------------------------------------
    // TASK-251: size-threshold rotation.
    // -------------------------------------------------------------------

    #[test]
    fn rotation_archives_and_truncates_at_threshold() {
        let _log = TestLog::new("rotate");
        // Tiny threshold so a couple of records trip it.
        unsafe {
            std::env::set_var("AISH_REASONING_ROTATE_MB", "0.0001"); // ~104 bytes
        }
        // Write enough events to exceed the limit; rotation fires inside append.
        for i in 0..20 {
            record(&ReasoningEvent::new(
                Decision::Guessed,
                format!("event number {i} with some padding text"),
                "self_report",
            ));
        }
        // At least the first archive generation exists and is gzip-magic'd.
        let gz = archive_path(1);
        assert!(gz.exists(), "rotation produced a .1.gz archive");
        let head = std::fs::read(&gz).unwrap();
        assert!(head.len() >= 2 && head[0] == 0x1f && head[1] == 0x8b, "gzip magic");

        // The live log was truncated at least once → it's smaller than the total
        // volume written, and still readable/summarizable.
        let live_len = std::fs::metadata(log_path()).map(|m| m.len()).unwrap_or(0);
        assert!(live_len < 20 * 100, "live log truncated on rotation");
        let _ = summarize(); // must not panic on a freshly-rotated log
    }

    #[test]
    fn rotation_disabled_when_threshold_zero() {
        let _log = TestLog::new("rotate_off");
        unsafe {
            std::env::set_var("AISH_REASONING_ROTATE_MB", "0");
        }
        for i in 0..50 {
            record(&ReasoningEvent::new(
                Decision::Guessed,
                format!("padding padding padding event {i}"),
                "self_report",
            ));
        }
        assert!(
            !archive_path(1).exists(),
            "no archive when rotation disabled"
        );
        assert_eq!(summarize().total, 50);
    }

    #[test]
    fn rotation_retains_only_keep_generations() {
        let _log = TestLog::new("rotate_keep");
        unsafe {
            std::env::set_var("AISH_REASONING_ROTATE_MB", "0.0001");
        }
        // Many events → several rotations.
        for i in 0..200 {
            record(&ReasoningEvent::new(
                Decision::Guessed,
                format!("event {i} with a decent amount of padding to grow the log fast"),
                "self_report",
            ));
        }
        // Never keep more than ROTATE_KEEP generations.
        assert!(archive_path(1).exists());
        assert!(
            !archive_path(ROTATE_KEEP + 1).exists(),
            "generations beyond ROTATE_KEEP are pruned"
        );
    }

    // -------------------------------------------------------------------
    // Rotation + summarize: no data lost across the rotation boundary.
    // (Guards the archive-folding path in `compute`.)
    // -------------------------------------------------------------------

    #[test]
    fn rotation_loses_no_data_across_boundary() {
        let _log = TestLog::new("rotate_nodata");
        unsafe {
            std::env::set_var("AISH_REASONING_ROTATE_MB", "0.0008");
        }
        // A handful of guesses; rotation archives the earliest into .1.gz.
        for i in 0..6 {
            record(&ReasoningEvent::new(
                Decision::Guessed,
                format!("event {i}"),
                "t",
            ))
            .unwrap();
        }
        // Force a full, rotation-aware rescan (archives folded before active).
        unsafe {
            std::env::set_var("AISH_REASONING_MEMO_FORCE_RESCAN", "1");
        }
        let s = summarize();
        unsafe {
            std::env::remove_var("AISH_REASONING_MEMO_FORCE_RESCAN");
        }
        assert_eq!(s.total, 6, "every event counted across the rotation boundary");
        assert_eq!(s.overall.guessed, 6);
    }

    #[test]
    fn outcome_update_folds_onto_archived_event_after_rotation() {
        let _log = TestLog::new("rotate_fold");
        unsafe {
            std::env::set_var("AISH_REASONING_ROTATE_MB", "0.0008");
        }
        // The tracked guess, recorded first so it lands in the first archive.
        let id = record(
            &ReasoningEvent::new(Decision::Guessed, "risky call", "t")
                .with_levels(Level::High, Level::High, Level::High),
        )
        .unwrap();
        // Push fillers until the first rotation archives the early records — we
        // stop at the first archive so the tracked event stays within the
        // retained window (no eviction) regardless of exact line sizes.
        let mut i = 0;
        while !archive_path(1).exists() && i < 100 {
            record(&ReasoningEvent::new(
                Decision::Escalated,
                format!("filler event {i} with padding to grow the active log"),
                "t",
            ));
            i += 1;
        }
        assert!(
            archive_path(1).exists(),
            "rotation should have archived the early records"
        );
        // Close the loop AFTER rotation — the update lands in the fresh active
        // file yet must still fold onto the now-archived event.
        assert!(update_outcome(&id, Outcome::WrongTurn));

        // Force a rescan so we exercise the archive-folding path deterministically.
        unsafe {
            std::env::set_var("AISH_REASONING_MEMO_FORCE_RESCAN", "1");
        }
        let s = summarize();
        unsafe {
            std::env::remove_var("AISH_REASONING_MEMO_FORCE_RESCAN");
        }
        assert_eq!(s.overall.guessed, 1, "only the tracked decision is a guess");
        assert_eq!(
            s.overall.guess_wrong, 1,
            "the post-rotation outcome folded onto the archived event"
        );
        assert_eq!(s.overall.guess_wrong_pct(), Some(100));
        let high = s.by_complexity.get(&Level::High).copied().unwrap();
        assert_eq!(high.guess_wrong_pct(), Some(100));
    }
}
