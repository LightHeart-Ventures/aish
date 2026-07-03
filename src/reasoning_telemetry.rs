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
use std::io::{BufRead, BufReader, Write};
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
    file.write_all(buf.as_bytes()).is_ok() && file.flush().is_ok()
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

// ─────────────────────────── Memoized summary (Phase 3) ───────────────────────────
//
// `:reasoning` used to re-scan the entire `.jsonl` (parsing every line, rebuilding
// the fold map) on every invocation — O(file). At 10k+ decisions that scan is
// noticeable, and a user running `:reasoning` twice in a minute paid it twice.
//
// We now persist a compact **aggregate memo** (`reasoning-telemetry-memo.json`)
// next to the log. It carries only the folded counters plus the source
// mtime/byte-length and how many lines it was computed from:
//
//   { version, source_mtime, source_len, lines_consumed,
//     total, overall, by_complexity, by_risk }
//
// Fast path — nothing changed since the memo (same mtime AND byte length):
// deserialize the tiny memo and return it. No `.jsonl` scan at all → O(1)-ish
// regardless of log size.
//
// Incremental path — the log only grew (append-only): fold ONLY the new lines
// (from `lines_consumed` onward) onto the memo. Cost is O(new lines), not O(file).
//
// Full recompute — memo missing/stale/wrong-version, the log shrank/rotated, or
// `AISH_REASONING_MEMO_FORCE_RESCAN=1`: scan the whole file and rewrite the memo
// (the original behavior), then serve from it.
//
// Correctly folding a *late outcome onto an old event* requires per-event state
// (which bucket the event fell in, and its current outcome) that the aggregate
// memo alone cannot reconstruct. So the folded per-event state is kept in a
// **sidecar** (`…-memo-events.json`) that is loaded only on the incremental/full
// paths — never on the O(1) fast read. This keeps the aggregate memo small (and
// the hot read genuinely cheap) while remaining exactly correct.

/// Memo format version. Bump when the on-disk schema changes so an old memo is
/// treated as stale (→ full recompute) instead of misread.
const MEMO_VERSION: u32 = 1;

/// The three per-level buckets, stored as explicit named fields so the memo
/// serializes to a plain JSON object (`{"low":…,"medium":…,"high":…}`) without
/// relying on enum-keyed maps.
#[derive(Debug, Default, Clone, Copy, Serialize, Deserialize)]
struct LevelBuckets {
    #[serde(default)]
    low: Bucket,
    #[serde(default)]
    medium: Bucket,
    #[serde(default)]
    high: Bucket,
}

impl LevelBuckets {
    fn get(&self, l: Level) -> Bucket {
        match l {
            Level::Low => self.low,
            Level::Medium => self.medium,
            Level::High => self.high,
        }
    }
    fn get_mut(&mut self, l: Level) -> &mut Bucket {
        match l {
            Level::Low => &mut self.low,
            Level::Medium => &mut self.medium,
            Level::High => &mut self.high,
        }
    }
}

/// The persisted aggregate memo (the small, O(1)-readable head). Mirrors the
/// [`Summary`] counters plus the source-freshness fingerprint.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct MemoHead {
    #[serde(default)]
    version: u32,
    /// Source mtime (nanoseconds since the UNIX epoch) at compute time.
    #[serde(default)]
    source_mtime: u64,
    /// Source byte length at compute time — the primary append/rotation signal.
    #[serde(default)]
    source_len: u64,
    /// How many raw lines of the source were consumed (the incremental cursor).
    #[serde(default)]
    lines_consumed: u64,
    #[serde(default)]
    total: usize,
    #[serde(default)]
    overall: Bucket,
    #[serde(default)]
    by_complexity: LevelBuckets,
    #[serde(default)]
    by_risk: LevelBuckets,
}

impl MemoHead {
    fn to_summary(&self) -> Summary {
        let mut by_complexity = BTreeMap::new();
        let mut by_risk = BTreeMap::new();
        for l in [Level::Low, Level::Medium, Level::High] {
            by_complexity.insert(l, self.by_complexity.get(l));
            by_risk.insert(l, self.by_risk.get(l));
        }
        Summary {
            total: self.total,
            overall: self.overall,
            by_complexity,
            by_risk,
        }
    }
}

/// Minimal folded per-event state kept in the sidecar so a later outcome update
/// can be applied to an old event exactly (right decision, right buckets).
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
struct MemoEvent {
    decision: Decision,
    complexity: Level,
    risk: Level,
    outcome: Outcome,
}

/// In-memory working set: the aggregate head plus the per-event fold map.
#[derive(Debug, Default)]
struct Memo {
    head: MemoHead,
    events: BTreeMap<String, MemoEvent>,
}

/// Path of the aggregate memo: `AISH_REASONING_MEMO` when set, else the sibling
/// `…-memo.json` next to the log (default `~/.aish/reasoning-telemetry-memo.json`).
pub fn memo_path() -> PathBuf {
    if let Ok(p) = std::env::var("AISH_REASONING_MEMO") {
        if !p.is_empty() {
            return PathBuf::from(p);
        }
    }
    let log = log_path();
    let parent = log.parent().map(Path::to_path_buf).unwrap_or_default();
    let stem = log
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("reasoning-telemetry");
    parent.join(format!("{stem}-memo.json"))
}

/// Path of the per-event fold sidecar (`…-memo-events.json`).
fn events_path() -> PathBuf {
    let memo = memo_path();
    let parent = memo.parent().map(Path::to_path_buf).unwrap_or_default();
    let stem = memo
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("reasoning-telemetry-memo");
    parent.join(format!("{stem}-events.json"))
}

/// Whether the operator forced a full rescan (`AISH_REASONING_MEMO_FORCE_RESCAN`).
fn force_rescan() -> bool {
    matches!(
        std::env::var("AISH_REASONING_MEMO_FORCE_RESCAN")
            .ok()
            .as_deref(),
        Some("1") | Some("true") | Some("yes") | Some("on")
    )
}

/// mtime (ns since epoch) + byte length of the source log, if it exists.
fn source_fingerprint(path: &Path) -> Option<(u64, u64)> {
    let md = std::fs::metadata(path).ok()?;
    let mtime = md
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    Some((mtime, md.len()))
}

/// Signed contribution of one folded event to a single bucket (`+1` add / `-1`
/// remove). Saturating so a torn memo can never underflow.
fn apply_sign(b: &mut Bucket, me: &MemoEvent, add: bool) {
    let adj = |x: &mut usize| {
        if add {
            *x += 1;
        } else {
            *x = x.saturating_sub(1);
        }
    };
    adj(&mut b.total);
    match me.decision {
        Decision::Escalated => adj(&mut b.escalated),
        Decision::Guessed => {
            adj(&mut b.guessed);
            match me.outcome {
                Outcome::Correct => adj(&mut b.guess_correct),
                Outcome::WrongTurn => adj(&mut b.guess_wrong),
                Outcome::Pending | Outcome::Unknown => {}
            }
        }
    }
}

/// Add/remove an event's contribution across overall + its complexity/risk buckets.
fn contribute(head: &mut MemoHead, me: &MemoEvent, add: bool) {
    if add {
        head.total += 1;
    } else {
        head.total = head.total.saturating_sub(1);
    }
    apply_sign(&mut head.overall, me, add);
    apply_sign(head.by_complexity.get_mut(me.complexity), me, add);
    apply_sign(head.by_risk.get_mut(me.risk), me, add);
}

/// Fold a freshly-parsed event line onto the memo. A duplicate id replaces the
/// prior state (last-write-wins, counted once) — matching the original
/// `BTreeMap::insert` + single tally semantics.
fn add_event(memo: &mut Memo, ev: &ReasoningEvent) {
    let me = MemoEvent {
        decision: ev.decision,
        complexity: ev.complexity,
        risk: ev.risk,
        outcome: ev.outcome,
    };
    if let Some(old) = memo.events.get(&ev.id).copied() {
        contribute(&mut memo.head, &old, false);
    }
    memo.events.insert(ev.id.clone(), me);
    contribute(&mut memo.head, &me, true);
}

/// Fold an outcome update onto a (possibly old) event. Dangling updates — no
/// matching event — are ignored, exactly as before.
fn apply_outcome_line(memo: &mut Memo, id: &str, oc: Outcome) {
    let Some(mut me) = memo.events.get(id).copied() else {
        return;
    };
    contribute(&mut memo.head, &me, false);
    me.outcome = oc;
    memo.events.insert(id.to_string(), me);
    contribute(&mut memo.head, &me, true);
}

/// Fold one raw JSONL line onto the memo (event → tally; outcome → re-fold;
/// blank/corrupt → skip). Mirrors the original per-line parsing.
fn fold_line(memo: &mut Memo, line: &str) {
    let line = line.trim();
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
            if let Some(oc) = Outcome::parse(oc) {
                apply_outcome_line(memo, id, oc);
            }
        }
    } else if let Ok(ev) = serde_json::from_value::<ReasoningEvent>(v) {
        add_event(memo, &ev);
    }
}

/// Load the aggregate memo head; `None` if missing, unreadable, or a wrong version.
fn load_head() -> Option<MemoHead> {
    let data = std::fs::read_to_string(memo_path()).ok()?;
    let head: MemoHead = serde_json::from_str(&data).ok()?;
    if head.version != MEMO_VERSION {
        return None;
    }
    Some(head)
}

/// Load the per-event fold sidecar (empty map when missing/unreadable).
fn load_events() -> BTreeMap<String, MemoEvent> {
    std::fs::read_to_string(events_path())
        .ok()
        .and_then(|d| serde_json::from_str(&d).ok())
        .unwrap_or_default()
}

/// Best-effort atomic write (temp + rename); swallows every error so telemetry
/// can never break a turn.
fn write_atomic(path: &Path, data: &[u8]) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let tmp = path.with_extension("json.tmp");
    if std::fs::write(&tmp, data).is_ok() {
        let _ = std::fs::rename(&tmp, path);
    }
}

/// Persist the memo head + event sidecar (best-effort).
fn write_memo(memo: &Memo) {
    if let Ok(data) = serde_json::to_string(&memo.head) {
        write_atomic(&memo_path(), data.as_bytes());
    }
    if let Ok(data) = serde_json::to_string(&memo.events) {
        write_atomic(&events_path(), data.as_bytes());
    }
}

/// Scan the whole log from scratch, building both the aggregate head and the
/// event fold map. Returns `None` only when the log file is absent.
fn full_recompute() -> Option<Memo> {
    let path = log_path();
    let file = std::fs::File::open(&path).ok()?;
    let mut memo = Memo {
        head: MemoHead {
            version: MEMO_VERSION,
            ..Default::default()
        },
        events: BTreeMap::new(),
    };
    let mut lines: u64 = 0;
    for line_res in BufReader::new(file).lines() {
        // Count every yielded line (Ok or Err) so the incremental cursor lines
        // up with a future `.lines()` skip, even across an unreadable line.
        lines += 1;
        if let Ok(line) = line_res {
            fold_line(&mut memo, &line);
        }
    }
    memo.head.lines_consumed = lines;
    if let Some((mtime, len)) = source_fingerprint(&path) {
        memo.head.source_mtime = mtime;
        memo.head.source_len = len;
    }
    Some(memo)
}

/// Fold only the lines appended past `head.lines_consumed` onto a loaded memo.
fn incremental(head: MemoHead) -> Memo {
    let mut memo = Memo {
        events: load_events(),
        head,
    };
    let path = log_path();
    if let Ok(file) = std::fs::File::open(&path) {
        let skip = memo.head.lines_consumed;
        let mut idx: u64 = 0;
        for line_res in BufReader::new(file).lines() {
            idx += 1;
            if idx <= skip {
                continue;
            }
            if let Ok(line) = line_res {
                fold_line(&mut memo, &line);
            }
        }
        memo.head.lines_consumed = idx.max(skip);
        if let Some((mtime, len)) = source_fingerprint(&path) {
            memo.head.source_mtime = mtime;
            memo.head.source_len = len;
        }
    }
    memo
}

/// Read the telemetry log's ground-truth [`Summary`], memoized.
///
/// * **Fast path** (memo fresh — same mtime AND byte length): return the tiny
///   aggregate memo without scanning the log at all — O(1)-ish in log size.
/// * **Incremental** (log only grew): fold just the new lines onto the memo.
/// * **Full recompute** (missing/stale memo, shrink/rotation, or
///   `AISH_REASONING_MEMO_FORCE_RESCAN=1`): rescan the whole file and rewrite.
///
/// Malformed lines are skipped; an absent log yields an empty summary. Every
/// memo read/write is best-effort — a memo failure silently degrades to a full
/// scan and never errors a turn.
pub fn summarize() -> Summary {
    let path = log_path();
    let fp = source_fingerprint(&path);

    // No log → nothing to summarize (don't serve a stale memo for a deleted log).
    let Some((cur_mtime, cur_len)) = fp else {
        return Summary::default();
    };

    if !force_rescan() {
        if let Some(head) = load_head() {
            // Fresh: neither the content nor the mtime moved → O(1) return.
            if head.source_len == cur_len && head.source_mtime == cur_mtime {
                return head.to_summary();
            }
            // Grew (append-only): fold only the new tail.
            if cur_len > head.source_len {
                let memo = incremental(head);
                write_memo(&memo);
                return memo.head.to_summary();
            }
            // Shrank / rewritten-in-place / same-len-new-mtime → fall through.
        }
    }

    // Full recompute (memo missing, stale, forced, truncated, or rotated).
    match full_recompute() {
        Some(memo) => {
            write_memo(&memo);
            memo.head.to_summary()
        }
        None => Summary::default(),
    }
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
            unsafe {
                std::env::set_var("AISH_REASONING_LOG", &path);
                // Start each test from a clean slate: no leaked force-rescan flag
                // and no stale memo/sidecar from a prior (possibly panicked) run.
                std::env::remove_var("AISH_REASONING_MEMO_FORCE_RESCAN");
            }
            let _ = std::fs::remove_file(&path);
            let _ = std::fs::remove_file(memo_path());
            let _ = std::fs::remove_file(events_path());
            TestLog {
                _guard: guard,
                path,
            }
        }
    }

    impl Drop for TestLog {
        fn drop(&mut self) {
            // memo_path()/events_path() resolve off AISH_REASONING_LOG — clean them
            // up while the env var is still set, then unset everything.
            let _ = std::fs::remove_file(memo_path());
            let _ = std::fs::remove_file(events_path());
            let _ = std::fs::remove_file(&self.path);
            unsafe {
                std::env::remove_var("AISH_REASONING_LOG");
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

    // ── Memoization (Phase 3) ──────────────────────────────────────────────

    fn read_head() -> MemoHead {
        serde_json::from_str(&std::fs::read_to_string(memo_path()).unwrap()).unwrap()
    }
    fn write_head(h: &MemoHead) {
        std::fs::write(memo_path(), serde_json::to_string(h).unwrap()).unwrap();
    }

    #[test]
    fn summarize_writes_memo_and_matches_full_scan() {
        let _log = TestLog::new("memo_write");
        record(
            &ReasoningEvent::new(Decision::Escalated, "a", "self_report")
                .with_levels(Level::High, Level::High, Level::High),
        )
        .unwrap();
        record(
            &ReasoningEvent::new(Decision::Guessed, "b", "self_report")
                .with_levels(Level::Low, Level::Low, Level::Low)
                .with_outcome(Outcome::Correct),
        )
        .unwrap();

        let s = summarize();
        assert_eq!(s.total, 2);
        assert_eq!(s.overall.escalated, 1);
        assert_eq!(s.overall.guessed, 1);

        // Both memo files are now on disk and the head mirrors the summary.
        assert!(memo_path().exists(), "aggregate memo written");
        assert!(events_path().exists(), "event sidecar written");
        let head = read_head();
        assert_eq!(head.version, MEMO_VERSION);
        assert_eq!(head.total, 2);
        assert_eq!(head.lines_consumed, 2);
        assert!(head.source_len > 0);
    }

    #[test]
    fn fresh_memo_is_served_without_scanning_the_log() {
        // AC: fresh memo path avoids full-file scan. Prove it by tampering the
        // memo AFTER it's built and leaving the log untouched — a fast-path read
        // returns the (bogus) memo value; a full rescan would return the real 1.
        let _log = TestLog::new("memo_fast");
        record(&ReasoningEvent::new(Decision::Guessed, "x", "self_report")).unwrap();
        assert_eq!(summarize().total, 1);

        let mut head = read_head();
        head.total = 777; // sentinel the log content can never produce
        write_head(&head);

        // Log unchanged (same mtime + len) → fast path returns the tampered memo.
        assert_eq!(summarize().total, 777, "fast path read the memo, not the log");
    }

    #[test]
    fn incremental_folds_only_new_lines() {
        // AC: only new lines processed. Sentinel-offset the memo, append one new
        // event, and confirm the delta is ADDED to the memo (incremental) rather
        // than recomputed from the whole file (which would give 2, not 102).
        let _log = TestLog::new("memo_incr");
        record(&ReasoningEvent::new(Decision::Guessed, "first", "self_report")).unwrap();
        assert_eq!(summarize().total, 1); // memo: total=1, lines_consumed=1

        let mut head = read_head();
        head.total = 101; // +100 sentinel over the true count of 1
        write_head(&head);

        // Append a 2nd event → log grows → incremental path folds just that line.
        record(&ReasoningEvent::new(Decision::Guessed, "second", "self_report")).unwrap();
        let s = summarize();
        assert_eq!(s.total, 102, "incremental added the one new line onto the memo");
        assert_eq!(read_head().lines_consumed, 2);
    }

    #[test]
    fn incremental_outcome_folds_onto_old_memoized_event() {
        // The tricky correctness case: an outcome update lands AFTER the event was
        // already memoized (and its per-event state discarded from RAM). The
        // sidecar restores it so the fold is exact.
        let _log = TestLog::new("memo_late_outcome");
        let id = record(
            &ReasoningEvent::new(Decision::Guessed, "risky", "self_report")
                .with_levels(Level::High, Level::High, Level::High),
        )
        .unwrap();
        let before = summarize(); // memoizes the pending guess
        assert_eq!(before.overall.guess_wrong, 0);
        assert_eq!(before.overall.guess_wrong_pct(), None);

        // Close the loop in a later append → incremental fold onto the old event.
        assert!(update_outcome(&id, Outcome::WrongTurn));
        let after = summarize();
        assert_eq!(after.overall.guessed, 1);
        assert_eq!(after.overall.guess_wrong, 1);
        assert_eq!(after.overall.guess_wrong_pct(), Some(100));
        let high = after.by_complexity.get(&Level::High).copied().unwrap();
        assert_eq!(high.guess_wrong_pct(), Some(100));
    }

    #[test]
    fn shrunk_log_triggers_full_recompute() {
        // AC: stale memo triggers correct full recompute + rewrite. A rotation /
        // truncation makes the log SHORTER than the memo's recorded length.
        let _log = TestLog::new("memo_stale");
        record(&ReasoningEvent::new(Decision::Guessed, "one", "self_report")).unwrap();
        record(&ReasoningEvent::new(Decision::Guessed, "two", "self_report")).unwrap();
        assert_eq!(summarize().total, 2);

        // Sentinel the memo so a stale (fast/incremental) read would be obvious.
        let mut head = read_head();
        head.total = 999;
        write_head(&head);

        // Rotate: truncate the log and write a single fresh event → len shrinks.
        std::fs::write(log_path(), b"").unwrap();
        record(&ReasoningEvent::new(Decision::Escalated, "post-rotate", "self_report")).unwrap();

        let s = summarize();
        assert_eq!(s.total, 1, "full recompute, sentinel discarded");
        assert_eq!(s.overall.escalated, 1);
        // Memo was rewritten to the recomputed truth.
        assert_eq!(read_head().total, 1);
    }

    #[test]
    fn force_rescan_env_bypasses_memo() {
        // AC: AISH_REASONING_MEMO_FORCE_RESCAN=1 forces a full scan.
        let _log = TestLog::new("memo_force");
        record(&ReasoningEvent::new(Decision::Guessed, "x", "self_report")).unwrap();
        assert_eq!(summarize().total, 1);

        let mut head = read_head();
        head.total = 555; // sentinel a fast path would surface
        write_head(&head);

        unsafe {
            std::env::set_var("AISH_REASONING_MEMO_FORCE_RESCAN", "1");
        }
        // Forced → ignore the tampered memo, rescan the (unchanged) log → real 1.
        assert_eq!(summarize().total, 1, "force-rescan ignored the memo");
        unsafe {
            std::env::remove_var("AISH_REASONING_MEMO_FORCE_RESCAN");
        }
    }

    #[test]
    fn missing_memo_falls_back_to_full_scan() {
        let _log = TestLog::new("memo_missing");
        record(&ReasoningEvent::new(Decision::Guessed, "x", "self_report")).unwrap();
        assert_eq!(summarize().total, 1);
        // Delete the memo → next summarize must full-recompute and rebuild it.
        std::fs::remove_file(memo_path()).unwrap();
        let s = summarize();
        assert_eq!(s.total, 1);
        assert!(memo_path().exists(), "memo rebuilt after loss");
    }
}
