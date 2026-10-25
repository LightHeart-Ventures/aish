//! Sub-work decomposition for a dispatched worker (coordinator) run.
//!
//! A dispatched worker used to be a black box: it took one brief, ground on it
//! for N rounds, and emitted one answer at the end. Two things were missing.
//! First, nothing forced the worker to BREAK the brief down, so a large ask was
//! attacked as one undifferentiated blob. Second, nothing checked the pieces:
//! the final answer could narrate "done" while a step had quietly failed, and
//! the operator saw no intermediate signal until the run ended.
//!
//! This module is the structural half of the fix (the prompt half is
//! [`SUBWORK_DECOMPOSITION`], which rides the worker's directive block):
//!
//! * **Decomposition with a HARD cap.** The worker declares its sub-work-items
//!   with `SUBWORK PLAN:` lines. [`SubWorkLedger::ingest`] accepts at most
//!   [`MAX_SUBWORK_ITEMS`]; declaration 6 and beyond are REJECTED in code and
//!   reported, so the cap is not a polite request the model can talk past.
//! * **One shared worktree.** The ledger owns a single worktree path — the
//!   parent worker's own `session.cwd` — and [`SubWorkItem`] has no worktree
//!   field of its own. Every item resolves to the parent's worktree by
//!   construction ([`SubWorkLedger::worktree_for`]); there is no API that
//!   could give a sub-item its own worktree or branch.
//! * **Running progress.** [`SubWorkLedger::progress_if_changed`] renders a
//!   per-item status block (pending / running / done / failed + a one-line
//!   summary) and returns it ONLY when the ledger actually changed, so the
//!   coordinator can emit it every round without spamming identical blocks.
//! * **Verification, not vibes.** [`SubWorkLedger::verify`] requires EVERY
//!   declared item to be `done`; anything failed or still unfinished makes the
//!   run's report say so explicitly ([`SubWorkLedger::finalize`] appends the
//!   block to the final answer) instead of silently marking the ask complete.
//!
//! The marker grammar is deliberately forgiving — models decorate lines with
//! bullets, bold, em dashes and backticks — but the SEMANTICS are strict: an
//! unknown status word is not a status, and an item that never reaches `done`
//! never verifies.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// Hard cap on the number of sub-work-items a dispatched worker may run.
///
/// Enforced in [`SubWorkLedger::ingest`] (not merely in the prompt): the 6th
/// and later declarations are dropped and surfaced in the verification report.
pub const MAX_SUBWORK_ITEMS: usize = 5;

/// The worker-facing directive that teaches the marker grammar this module
/// parses. Held as a `const` so the prompt and the parser can be asserted
/// consistent in unit tests (the cap number appears in both).
pub const SUBWORK_DECOMPOSITION: &str = "DECOMPOSE INTO SUB-WORK-ITEMS (max 5, shared worktree): \
Split your assigned task into AT MOST 5 sub-work-items and run them yourself IN THIS SAME \
WORKTREE — do NOT create a new git worktree or branch per sub-item, and do not dispatch a \
sub-worker for one. Declare them once, up front, one per line:\n\
`SUBWORK PLAN: <id> | <intent>`  (id is a lowercase-kebab slug)\n\
Then, as each one progresses, emit a status line in the SAME turn you do the work:\n\
`SUBWORK RUNNING: <id> | <what you're doing>`\n\
`SUBWORK DONE: <id> | <one-line result>`\n\
`SUBWORK FAILED: <id> | <why it failed>`\n\
The 5-item cap is enforced in code: a 6th declaration is REJECTED and reported, so merge related \
work instead of declaring more. Before you finish, every declared item must be DONE — aish \
verifies the ledger and appends the result to your report, so a failed or unfinished item is \
surfaced, never silently marked done. If a sub-item cannot be completed, mark it FAILED with the \
reason and say so in your final answer.";

/// Lifecycle state of a single sub-work-item.
// `Serialize`/`Deserialize` are what let the ledger ride the durable
// `coordinator_runs.subwork` column out of the coordinator subprocess and into
// the parent shell's escalation tray. `rename_all = "lowercase"` makes the wire
// form identical to [`SubWorkStatus::as_str`], so the JSON and the `📣` console
// note can never disagree about what "done" is called.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SubWorkStatus {
    /// Declared but not started.
    Pending,
    /// In flight this round.
    Running,
    /// Finished successfully.
    Done,
    /// Finished unsuccessfully — reported, never silently swallowed.
    Failed,
}

impl SubWorkStatus {
    /// Lowercase wire/report name.
    pub fn as_str(self) -> &'static str {
        match self {
            SubWorkStatus::Pending => "pending",
            SubWorkStatus::Running => "running",
            SubWorkStatus::Done => "done",
            SubWorkStatus::Failed => "failed",
        }
    }

    /// Parse a status keyword (case-insensitive). `None` for anything else —
    /// an unrecognized word is NOT quietly treated as a status.
    pub fn parse(word: &str) -> Option<Self> {
        match word.trim().to_ascii_lowercase().as_str() {
            "pending" | "todo" => Some(SubWorkStatus::Pending),
            "running" | "start" | "started" => Some(SubWorkStatus::Running),
            "done" | "complete" | "completed" | "ok" => Some(SubWorkStatus::Done),
            "failed" | "fail" | "error" | "blocked" => Some(SubWorkStatus::Failed),
            _ => None,
        }
    }

    /// Single-width glyph used in the progress block.
    fn glyph(self) -> &'static str {
        match self {
            SubWorkStatus::Pending => "·",
            SubWorkStatus::Running => "~",
            SubWorkStatus::Done => "+",
            SubWorkStatus::Failed => "x",
        }
    }
}

/// One sub-work-item. Deliberately has NO worktree/branch field: every item
/// runs in the parent worker's worktree, which the [`SubWorkLedger`] owns.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SubWorkItem {
    /// Lowercase-kebab slug the worker uses to refer to this item.
    pub id: String,
    /// What the item is meant to accomplish (from the `PLAN` line).
    pub intent: String,
    /// Current lifecycle state.
    pub status: SubWorkStatus,
    /// One-line summary from the most recent status line.
    pub note: String,
}

/// What a single [`SubWorkLedger::ingest`] pass changed.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct IngestReport {
    /// New items accepted into the ledger.
    pub declared: usize,
    /// Existing items whose status or note changed.
    pub updated: usize,
    /// Declarations rejected by the [`MAX_SUBWORK_ITEMS`] cap.
    pub dropped: usize,
}

/// Outcome of verifying the ledger against the original ask.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verification {
    /// True when every declared item reached `done` (or nothing was declared).
    pub ok: bool,
    /// Ids that ended `failed`.
    pub failed: Vec<String>,
    /// Ids that never reached a terminal state (`pending` / `running`).
    pub unfinished: Vec<String>,
    /// Human-readable block appended to the worker's final answer. Empty when
    /// the worker declared no sub-work at all.
    pub report: String,
}

/// The parent worker's sub-work ledger: the items, the ONE shared worktree they
/// all run in, and the de-duplication state for progress emission.
#[derive(Debug, Clone)]
pub struct SubWorkLedger {
    worktree: PathBuf,
    items: Vec<SubWorkItem>,
    /// Ids rejected by the hard cap, kept (sorted, de-duplicated) so the
    /// verification report can name them.
    dropped: BTreeSet<String>,
    last_emitted: Option<String>,
}

impl SubWorkLedger {
    /// Create a ledger bound to the parent worker's worktree. Every sub-item
    /// this ledger ever holds runs HERE.
    pub fn new(worktree: impl Into<PathBuf>) -> Self {
        Self {
            worktree: worktree.into(),
            items: Vec::new(),
            dropped: BTreeSet::new(),
            last_emitted: None,
        }
    }

    /// The single worktree shared by the parent and all its sub-work-items.
    #[cfg_attr(not(test), allow(dead_code))] // invariant accessor; asserted by tests.
    pub fn worktree(&self) -> &Path {
        &self.worktree
    }

    /// The worktree a given sub-item runs in — always the parent's, by
    /// construction. `None` only when the id was never declared.
    #[cfg_attr(not(test), allow(dead_code))] // invariant accessor; asserted by tests.
    pub fn worktree_for(&self, id: &str) -> Option<&Path> {
        self.get(id).map(|_| self.worktree.as_path())
    }

    /// Declared items, in declaration order.
    #[cfg_attr(not(test), allow(dead_code))] // production renders via `progress`/`finalize`.
    pub fn items(&self) -> &[SubWorkItem] {
        &self.items
    }

    /// Ids rejected by the hard cap.
    pub fn dropped(&self) -> Vec<String> {
        self.dropped.iter().cloned().collect()
    }

    #[cfg_attr(not(test), allow(dead_code))] // only `worktree_for`/tests look items up by id.
    fn get(&self, id: &str) -> Option<&SubWorkItem> {
        let want = normalize_id(id);
        self.items.iter().find(|i| i.id == want)
    }

    /// Fold every `SUBWORK …` marker in `text` into the ledger.
    ///
    /// The [`MAX_SUBWORK_ITEMS`] cap is enforced HERE: once the ledger holds 5
    /// items, further *new* ids are recorded as dropped and ignored. Status
    /// lines for already-declared ids always apply (they are updates, not new
    /// work), so a worker can never lose the ability to report a failure.
    pub fn ingest(&mut self, text: &str) -> IngestReport {
        let mut report = IngestReport::default();
        for line in text.lines() {
            let Some(directive) = parse_directive(line) else {
                continue;
            };
            let SubWorkDirective { id, status, text } = directive;
            if let Some(idx) = self.items.iter().position(|i| i.id == id) {
                let item = &mut self.items[idx];
                let mut changed = false;
                match status {
                    // A re-stated PLAN line only ever fills in a missing intent;
                    // it never resets a status back to pending.
                    None => {
                        if !text.is_empty() && item.intent != text {
                            item.intent = text;
                            changed = true;
                        }
                    }
                    Some(s) => {
                        if item.status != s {
                            item.status = s;
                            changed = true;
                        }
                        if !text.is_empty() && item.note != text {
                            item.note = text;
                            changed = true;
                        }
                    }
                }
                if changed {
                    report.updated += 1;
                }
                continue;
            }

            // New id — subject to the hard cap.
            if self.items.len() >= MAX_SUBWORK_ITEMS {
                if self.dropped.insert(id) {
                    report.dropped += 1;
                }
                continue;
            }
            let status = status.unwrap_or(SubWorkStatus::Pending);
            let (intent, note) = match status {
                SubWorkStatus::Pending => (text, String::new()),
                _ => (text.clone(), text),
            };
            self.items.push(SubWorkItem {
                id,
                intent,
                status,
                note,
            });
            report.declared += 1;
        }
        report
    }

    /// Render the running progress block: a header with the tally and the
    /// shared worktree, then one line per sub-item. `None` when nothing has
    /// been declared yet.
    pub fn progress_summary(&self) -> Option<String> {
        if self.items.is_empty() {
            return None;
        }
        let done = self.count(SubWorkStatus::Done);
        let running = self.count(SubWorkStatus::Running);
        let failed = self.count(SubWorkStatus::Failed);
        let pending = self.count(SubWorkStatus::Pending);
        let mut out = format!(
            "sub-work: {done}/{} done, {running} running, {pending} pending, {failed} failed (shared worktree {})",
            self.items.len(),
            self.worktree.display(),
        );
        for item in &self.items {
            let summary = if item.note.is_empty() {
                item.intent.as_str()
            } else {
                item.note.as_str()
            };
            out.push_str(&format!(
                "\n  {} [{}] {}",
                item.status.glyph(),
                item.status.as_str(),
                item.id
            ));
            if !summary.is_empty() {
                out.push_str(&format!(" — {summary}"));
            }
        }
        if !self.dropped.is_empty() {
            out.push_str(&format!(
                "\n  ! {} declaration(s) rejected by the {MAX_SUBWORK_ITEMS}-item cap: {}",
                self.dropped.len(),
                self.dropped().join(", ")
            ));
        }
        Some(out)
    }

    /// Progress block, but only when it DIFFERS from the last one emitted —
    /// lets the coordinator call this every round without repeating itself.
    pub fn progress_if_changed(&mut self) -> Option<String> {
        let summary = self.progress_summary()?;
        if self.last_emitted.as_deref() == Some(summary.as_str()) {
            return None;
        }
        self.last_emitted = Some(summary.clone());
        Some(summary)
    }

    fn count(&self, status: SubWorkStatus) -> usize {
        self.items.iter().filter(|i| i.status == status).count()
    }

    /// Check every declared sub-item completed and that the set as a whole can
    /// be called done. A worker that declared nothing verifies trivially (plain
    /// single-step runs are unaffected); otherwise EVERY item must be `done`.
    pub fn verify(&self) -> Verification {
        if self.items.is_empty() {
            return Verification {
                ok: true,
                failed: Vec::new(),
                unfinished: Vec::new(),
                report: String::new(),
            };
        }
        let failed: Vec<String> = self
            .items
            .iter()
            .filter(|i| i.status == SubWorkStatus::Failed)
            .map(|i| i.id.clone())
            .collect();
        let unfinished: Vec<String> = self
            .items
            .iter()
            .filter(|i| matches!(i.status, SubWorkStatus::Pending | SubWorkStatus::Running))
            .map(|i| i.id.clone())
            .collect();
        let ok = failed.is_empty() && unfinished.is_empty();

        let mut report = format!(
            "SUB-WORK VERIFICATION — {} item(s), shared worktree {}",
            self.items.len(),
            self.worktree.display()
        );
        for item in &self.items {
            let summary = if item.note.is_empty() {
                item.intent.as_str()
            } else {
                item.note.as_str()
            };
            report.push_str(&format!("\n- [{}] {}", item.status.as_str(), item.id));
            if !summary.is_empty() {
                report.push_str(&format!(" — {summary}"));
            }
        }
        if !self.dropped.is_empty() {
            report.push_str(&format!(
                "\n- {} declaration(s) rejected by the {MAX_SUBWORK_ITEMS}-item cap: {}",
                self.dropped.len(),
                self.dropped().join(", ")
            ));
        }
        if ok {
            report.push_str("\nRESULT: all sub-work-items completed successfully.");
        } else {
            report.push_str(
                "\nRESULT: NOT COMPLETE — the combined result does not yet meet the ask.",
            );
            if !failed.is_empty() {
                report.push_str(&format!(" Failed: {}.", failed.join(", ")));
            }
            if !unfinished.is_empty() {
                report.push_str(&format!(" Unfinished: {}.", unfinished.join(", ")));
            }
        }
        Verification {
            ok,
            failed,
            unfinished,
            report,
        }
    }

    /// Append the verification block to the worker's final answer. A no-op when
    /// no sub-work was declared.
    pub fn finalize(&self, answer: String) -> String {
        let verification = self.verify();
        if verification.report.is_empty() {
            return answer;
        }
        if answer.trim().is_empty() {
            return verification.report;
        }
        format!("{answer}\n\n{}", verification.report)
    }
}

/// One parsed `SUBWORK …` marker. `status: None` means a `PLAN` declaration.
#[derive(Debug, Clone, PartialEq, Eq)]
struct SubWorkDirective {
    id: String,
    status: Option<SubWorkStatus>,
    text: String,
}

/// Normalize an id for matching: strip decoration, lowercase, collapse spaces
/// to hyphens so `Parse Cap`, `` `parse-cap` `` and `parse-cap` are one item.
fn normalize_id(raw: &str) -> String {
    raw.trim()
        .trim_matches(|c: char| {
            c == '`' || c == '*' || c == '"' || c == '\'' || c == '_' || c.is_whitespace()
        })
        .to_ascii_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join("-")
}

/// Parse one line into a directive, tolerating the decoration models add
/// (`- `, `* `, `**bold**`, backticks) around an otherwise strict grammar:
/// `SUBWORK <PLAN|PENDING|RUNNING|DONE|FAILED>: <id> | <text>`.
fn parse_directive(line: &str) -> Option<SubWorkDirective> {
    let mut s = line.trim();
    // Leading markdown bullet / blockquote decoration.
    while let Some(rest) = s
        .strip_prefix("- ")
        .or_else(|| s.strip_prefix("* "))
        .or_else(|| s.strip_prefix("+ "))
        .or_else(|| s.strip_prefix("> "))
    {
        s = rest.trim_start();
    }
    s = s.trim_start_matches(['*', '`', '#', ' ']);
    let rest = strip_ci(s, "SUBWORK")?;
    let rest = rest.trim_start_matches([':', '*', '`', ' ', '-']).trim();
    // Keyword runs up to the first colon.
    let (keyword, body) = rest.split_once(':')?;
    let keyword = keyword.trim().trim_matches(['*', '`']).trim();
    let status = if keyword.eq_ignore_ascii_case("plan") || keyword.eq_ignore_ascii_case("item") {
        None
    } else {
        Some(SubWorkStatus::parse(keyword)?)
    };
    let body = body.trim().trim_end_matches(['*', '`']).trim();
    if body.is_empty() {
        return None;
    }
    let (raw_id, text) = split_id(body);
    let id = normalize_id(raw_id);
    if id.is_empty() {
        return None;
    }
    Some(SubWorkDirective {
        id,
        status,
        text: text.trim().trim_matches(['*', '`']).trim().to_string(),
    })
}

/// Split `<id> <sep> <text>` on the first supported separator, falling back to
/// the first whitespace run so `DONE: parse-cap cap enforced` still parses.
fn split_id(body: &str) -> (&str, &str) {
    for sep in ["|", "—", "–", " -- ", " - ", " : "] {
        if let Some((id, text)) = body.split_once(sep) {
            return (id, text);
        }
    }
    match body.split_once(char::is_whitespace) {
        Some((id, text)) => (id, text),
        None => (body, ""),
    }
}

/// Case-insensitive `strip_prefix`.
///
/// Uses `str::get` rather than a raw byte-range index: slicing `s[..n]`
/// panics when `n` lands inside a multi-byte UTF-8 char (e.g. the line
/// `"Phase — kickoff"`, where an em dash occupies bytes 5..8 and
/// `"SUBWORK".len()` is 7). `get` returns `None` on a non-boundary instead.
fn strip_ci<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    let head = s.get(..prefix.len())?;
    if head.eq_ignore_ascii_case(prefix) {
        Some(&s[prefix.len()..])
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// Durable transport for the escalation tray.
//
// The ledger is built and mutated INSIDE the coordinator subprocess, but the
// tray that has to draw it lives in the parent shell. Rather than teach the
// parent to sentinel-parse the child's console stream (fragile, and lost the
// moment the line scrolls or the shell restarts), the coordinator snapshots the
// ledger as JSON into `coordinator_runs.subwork` — the same durable table every
// other tray row is already fed from. These two functions are that wire format.
// ---------------------------------------------------------------------------

impl SubWorkLedger {
    /// Snapshot the live items as a compact JSON array of
    /// `{"id","intent","status","note"}`.
    ///
    /// Dropped (cap-rejected) ids are deliberately NOT included: they are not
    /// work the worker is doing, so they must not appear as tray rows — they
    /// surface in the verification report instead.
    ///
    /// Infallible by construction (a `Vec` of plain strings + a unit enum), but
    /// we still degrade to `[]` rather than panicking: a cosmetic tray row is
    /// never worth taking a worker down for.
    pub fn to_json(&self) -> String {
        serde_json::to_string(&self.items).unwrap_or_else(|_| "[]".to_string())
    }
}

/// Reader half of [`SubWorkLedger::to_json`], for the parent shell.
///
/// Returns an empty vec for anything unparseable — a NULL column, a row written
/// by an older build, or a truncated write. "No ledger" and "bad ledger" collapse
/// to the same safe outcome: draw no sub-work rows.
pub fn parse_json(s: &str) -> Vec<SubWorkItem> {
    serde_json::from_str(s).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    const PARENT: &str = "/tmp/aish-worktrees/w_test";

    fn ledger() -> SubWorkLedger {
        SubWorkLedger::new(PARENT)
    }

    #[test]
    fn parses_plan_and_status_markers() {
        let d = parse_directive("SUBWORK PLAN: parse-cap | enforce the 5-item cap").unwrap();
        assert_eq!(d.id, "parse-cap");
        assert_eq!(d.status, None);
        assert_eq!(d.text, "enforce the 5-item cap");

        let d = parse_directive("- **SUBWORK DONE:** `parse-cap` — cap enforced").unwrap();
        assert_eq!(d.id, "parse-cap");
        assert_eq!(d.status, Some(SubWorkStatus::Done));
        assert_eq!(d.text, "cap enforced");

        // Not a directive: no keyword colon, unknown keyword, unrelated prose.
        assert!(parse_directive("SUBWORK").is_none());
        assert!(parse_directive("SUBWORK MAYBE: x | y").is_none());
        assert!(parse_directive("we will split the subwork into pieces").is_none());
    }

    /// Regression: a line whose byte 7 lands INSIDE a multi-byte char used to
    /// panic in `strip_ci` with
    /// `end byte index 7 is not a char boundary; it is inside '—'`.
    /// Every line of model output flows through `parse_directive`, so an em
    /// dash after a five-char word was enough to kill the process.
    #[test]
    fn non_ascii_lines_do_not_panic_on_char_boundaries() {
        // The exact shape from the crash report: em dash at bytes 5..8.
        assert!(parse_directive("Phase — kickoff").is_none());
        assert!(parse_directive("- Plan — split the work").is_none());
        // Shorter-than-prefix and all-multibyte inputs.
        assert!(parse_directive("—").is_none());
        assert!(parse_directive("héllo").is_none());
        assert!(parse_directive("日本語のテキストです").is_none());
        assert!(parse_directive("“SUBWORK”").is_none());
        // Every byte offset of a multi-byte char, exercised directly.
        for pad in 0..12 {
            let line = format!("{}—{}", "a".repeat(pad), "b".repeat(pad));
            assert!(strip_ci(&line, "SUBWORK").is_none());
            assert!(parse_directive(&line).is_none());
        }
        // And real directives with non-ASCII payloads still parse.
        let d = parse_directive("SUBWORK DONE: parse-cap — café ✅").unwrap();
        assert_eq!(d.id, "parse-cap");
        assert_eq!(d.status, Some(SubWorkStatus::Done));
        assert_eq!(d.text, "café ✅");
    }

    /// Requirement 1: the cap of 5 is enforced in CODE, not just in the prompt.
    #[test]
    fn hard_cap_of_five_is_enforced_in_code() {
        let mut l = ledger();
        let text = (1..=8)
            .map(|n| format!("SUBWORK PLAN: item-{n} | work {n}"))
            .collect::<Vec<_>>()
            .join("\n");
        let report = l.ingest(&text);

        assert_eq!(l.items().len(), MAX_SUBWORK_ITEMS);
        assert_eq!(MAX_SUBWORK_ITEMS, 5);
        assert_eq!(report.declared, 5);
        assert_eq!(report.dropped, 3);
        assert_eq!(l.dropped(), vec!["item-6", "item-7", "item-8"]);
        // The surviving items are the first five declared.
        let ids: Vec<&str> = l.items().iter().map(|i| i.id.as_str()).collect();
        assert_eq!(ids, vec!["item-1", "item-2", "item-3", "item-4", "item-5"]);
        // And the rejection is surfaced, not swallowed.
        assert!(l.verify().report.contains("rejected by the 5-item cap"));
    }

    /// A status line for an ALREADY-declared item is an update, so a worker at
    /// the cap can still report a failure on an existing item.
    #[test]
    fn status_updates_apply_at_the_cap() {
        let mut l = ledger();
        l.ingest("SUBWORK PLAN: a | x\nSUBWORK PLAN: b | x\nSUBWORK PLAN: c | x\nSUBWORK PLAN: d | x\nSUBWORK PLAN: e | x");
        assert_eq!(l.items().len(), 5);
        let report = l.ingest("SUBWORK FAILED: c | compile error");
        assert_eq!(report.updated, 1);
        assert_eq!(report.dropped, 0);
        let c = l.items().iter().find(|i| i.id == "c").unwrap();
        assert_eq!(c.status, SubWorkStatus::Failed);
        assert_eq!(c.note, "compile error");
    }

    /// Requirement 2: every sub-item runs in the PARENT's worktree. There is no
    /// per-item worktree field and no API to give one a different path.
    #[test]
    fn sub_items_share_the_parent_worktree() {
        let mut l = ledger();
        l.ingest("SUBWORK PLAN: one | a\nSUBWORK RUNNING: two | b\nSUBWORK DONE: three | c");
        assert_eq!(l.items().len(), 3);
        for item in l.items() {
            assert_eq!(
                l.worktree_for(&item.id),
                Some(Path::new(PARENT)),
                "sub-item {} must run in the parent worktree",
                item.id
            );
        }
        assert_eq!(l.worktree(), Path::new(PARENT));
        assert_eq!(l.worktree_for("never-declared"), None);
        // The progress block states the shared worktree explicitly.
        assert!(l.progress_summary().unwrap().contains(PARENT));
    }

    /// Requirement 3: a failed sub-item is reported as failed; the run is not
    /// silently marked done.
    #[test]
    fn failed_sub_item_fails_verification_and_is_named() {
        let mut l = ledger();
        l.ingest(
            "SUBWORK PLAN: alpha | a\nSUBWORK PLAN: beta | b\nSUBWORK DONE: alpha | ok\nSUBWORK FAILED: beta | cargo test failed",
        );
        let v = l.verify();
        assert!(!v.ok);
        assert_eq!(v.failed, vec!["beta"]);
        assert!(v.unfinished.is_empty());
        assert!(v.report.contains("NOT COMPLETE"));
        assert!(v.report.contains("Failed: beta"));
        assert!(v.report.contains("cargo test failed"));

        // …and it lands in the worker's final answer rather than being dropped.
        let answer = l.finalize("all set!".to_string());
        assert!(answer.starts_with("all set!"));
        assert!(answer.contains("NOT COMPLETE"));
        assert!(answer.contains("[failed] beta"));
    }

    #[test]
    fn unfinished_sub_item_also_fails_verification() {
        let mut l = ledger();
        l.ingest("SUBWORK PLAN: alpha | a\nSUBWORK PLAN: beta | b\nSUBWORK DONE: alpha | ok");
        let v = l.verify();
        assert!(!v.ok);
        assert_eq!(v.unfinished, vec!["beta"]);
        assert!(v.report.contains("Unfinished: beta"));
    }

    #[test]
    fn all_done_verifies_and_no_subwork_is_a_no_op() {
        let mut l = ledger();
        l.ingest("SUBWORK PLAN: alpha | a\nSUBWORK DONE: alpha | shipped");
        let v = l.verify();
        assert!(v.ok, "{}", v.report);
        assert!(
            v.report
                .contains("all sub-work-items completed successfully")
        );

        // A worker that never decomposed is untouched.
        let empty = ledger();
        let v = empty.verify();
        assert!(v.ok);
        assert!(v.report.is_empty());
        assert_eq!(empty.finalize("plain answer".into()), "plain answer");
        assert!(empty.progress_summary().is_none());
    }

    /// Requirement 4: a running summary is emitted AS the sub-items progress —
    /// once per change, with a per-item status and one-line summary.
    #[test]
    fn progress_summaries_are_emitted_as_work_progresses() {
        let mut l = ledger();

        l.ingest("SUBWORK PLAN: alpha | wire the parser\nSUBWORK PLAN: beta | add tests");
        let first = l
            .progress_if_changed()
            .expect("declaration emits a summary");
        assert!(first.contains("sub-work: 0/2 done"));
        assert!(first.contains("[pending] alpha — wire the parser"));
        assert!(first.contains("[pending] beta — add tests"));

        // No change → no repeat block.
        assert!(l.progress_if_changed().is_none());

        l.ingest("SUBWORK RUNNING: alpha | parsing markers");
        let second = l
            .progress_if_changed()
            .expect("status change emits a summary");
        assert!(second.contains("0/2 done, 1 running"));
        assert!(second.contains("[running] alpha — parsing markers"));

        l.ingest("SUBWORK DONE: alpha | parser landed\nSUBWORK FAILED: beta | 1 assertion failed");
        let third = l
            .progress_if_changed()
            .expect("terminal states emit a summary");
        assert!(third.contains("1/2 done"));
        assert!(third.contains("1 failed"));
        assert!(third.contains("[done] alpha — parser landed"));
        assert!(third.contains("[failed] beta — 1 assertion failed"));
        assert!(l.progress_if_changed().is_none());
    }

    /// The prompt half and the code half must agree about the cap and the
    /// marker grammar — a drifted directive would teach a grammar the parser
    /// cannot read.
    #[test]
    fn directive_documents_the_enforced_grammar() {
        let d = SUBWORK_DECOMPOSITION;
        assert!(d.contains(&format!("max {MAX_SUBWORK_ITEMS}")));
        assert!(d.contains("SAME WORKTREE"));
        assert!(d.contains("enforced in code"));
        for marker in [
            "SUBWORK PLAN:",
            "SUBWORK RUNNING:",
            "SUBWORK DONE:",
            "SUBWORK FAILED:",
        ] {
            assert!(d.contains(marker), "directive is missing {marker}");
            // Every advertised marker must actually parse.
            let line = format!("{marker} demo-id | some text");
            assert!(
                parse_directive(&line).is_some(),
                "parser cannot read advertised marker {marker}"
            );
        }
    }
}
