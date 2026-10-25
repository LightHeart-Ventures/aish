//! Durable tray for worker-proposed operator decisions (FR-431 choice blocks).
//!
//! # Why this exists
//!
//! `choice.rs` gives an agent a way to ASK: it emits a fenced ```choice block,
//! `first_choice` parses it, and `prompt_choice` paints a popup and hands the
//! selected option id(s) back. In an INTERACTIVE turn that round-trip is whole
//! — the operator is at the keyboard, the popup opens, the answer feeds
//! straight back into the live turn over `next_input` + `continue 'resume`.
//!
//! In a BACKGROUND COORDINATOR it is not whole, and that is the bug this
//! module closes. `prompt_choice` opens with `isatty(0)` and returns `None` the
//! moment it finds no controlling terminal — which is exactly a worker's
//! situation. The `if let Some(spec) … && let Some(answer) …` chain in
//! `repl.rs` then falls through, the block is printed as ordinary text, and the
//! question is GONE: scrolled past in a pane the operator may not even be
//! attached to. Detach, start a new session, or `:attach` later and there is
//! nothing left to answer. A question nobody can see is not a question.
//!
//! So a worker does not try to prompt. It **parks** the question here, where it
//! survives, and the operator's interactive aish re-surfaces it — at session
//! start, and on every `:attach` / Shift-Tab cycle — until it is either
//! answered or explicitly dismissed.
//!
//! # Transport: the filesystem, deliberately
//!
//! Same reasoning (and the same shape) as [`crate::approval`]: the asker and
//! the answerer are DIFFERENT PROCESSES, possibly several coordinator levels
//! apart, and the child's stderr is one-way. So the store is a directory of
//! small flat files under `~/.aish/questions/`:
//!
//! ```text
//! ~/.aish/questions/
//! ├── <id>.question   written by the WORKER, read by the operator's REPL
//! └── <id>.answer     written by the OPERATOR's REPL, read by the worker
//! ```
//!
//! Boring on purpose. It survives a restart of either side, needs no
//! socket/port/daemon, resolves identically at any nesting depth (every
//! process sees the same `$HOME`), and you can `cat` it when something goes
//! wrong. Reusing the broker shape next door also means one mental model for
//! two parked-work queues instead of two.
//!
//! # Resolution is explicit, never incidental
//!
//! A pending question leaves the tray on exactly two paths, both operator
//! actions: [`answer`] (routed back to the worker) and [`dismiss`] (discarded
//! unanswered). Esc out of the popup is NEITHER — it closes the popup and
//! leaves the row pending, because "not right now" is not an answer and losing
//! the question to a stray keystroke is the failure we are fixing. Only the
//! stale sweep removes a row without the operator, and it is deliberately
//! generous.

use crate::choice::{ChoiceMode, ChoiceOption, ChoiceSpec};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Directory holding the question/answer files: `~/.aish/questions/`.
///
/// Best-effort created on every call (idempotent), mirroring
/// [`crate::approval::approvals_dir`]. A creation failure is never fatal: the
/// worst case is that a question cannot be parked, which is the behaviour we
/// already had before this module existed.
pub fn questions_dir() -> PathBuf {
    let dir = PathBuf::from(std::env::var("HOME").unwrap_or_default())
        .join(".aish")
        .join("questions");
    let _ = std::fs::create_dir_all(&dir);
    dir
}

/// Pending questions older than this are swept. Far more generous than the
/// approval broker's window: an approval BLOCKS a worker that is sitting there
/// waiting, so a dead one should clear quickly, whereas a parked question is
/// the operator's to answer whenever they next sit down. A week survives a
/// weekend plus a holiday Monday.
const STALE_AFTER: Duration = Duration::from_secs(7 * 24 * 3600);

/// A worker-proposed decision awaiting the operator.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct PendingQuestion {
    /// Short unique id the operator types: `:answer <id>` / `:dismiss <id>`.
    pub id: String,
    /// Worker label (`w_xxxxxxxx`) or `local` for a non-coordinator caller.
    pub worker: String,
    /// The question text from the block's `question:` key.
    pub question: String,
    /// Selection semantics, serialised as `one` / `many` / `compare`.
    pub mode: String,
    /// `(id, label)` per option, in the order the agent listed them.
    pub options: Vec<(String, String)>,
    /// Unix seconds the question was parked.
    pub created: u64,
}

/// Escape the three metacharacters of the flat format.
///
/// Hand-rolled rather than `serde_json` for the same reason
/// [`crate::approval::Request`] is: these fields are agent-authored text and
/// this format cannot be tricked into re-reading a `"` or a `{` in a label as
/// structure. Tab joins fields, newline joins records, backslash escapes both.
fn esc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            _ => out.push(c),
        }
    }
    out
}

/// Inverse of [`esc`]. A single left-to-right walk rather than chained
/// `replace` calls, because chaining decodes a literal `\\n` (an escaped
/// backslash followed by `n`) into a newline — a round-trip bug that only
/// shows up on text containing a Windows path or a regex.
fn unesc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('\\') => out.push('\\'),
            // An unknown escape is kept verbatim rather than swallowed: better
            // to show the operator a stray backslash than to silently eat a
            // character out of the question they are being asked.
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

/// Serialise [`ChoiceMode`] for the on-disk record.
fn mode_str(mode: ChoiceMode) -> &'static str {
    match mode {
        ChoiceMode::One => "one",
        ChoiceMode::Many => "many",
        ChoiceMode::Compare => "compare",
    }
}

/// Parse a serialised mode. Anything unrecognised lands on `One`, the
/// conservative reading: a single pick can always be re-asked, whereas
/// silently widening a radio into a multi-select changes what the agent meant.
fn mode_of(s: &str) -> ChoiceMode {
    match s.trim().to_ascii_lowercase().as_str() {
        "many" | "multi" | "multiple" => ChoiceMode::Many,
        "compare" | "comparison" => ChoiceMode::Compare,
        _ => ChoiceMode::One,
    }
}

impl PendingQuestion {
    /// Serialise to the one-line-per-field flat format used on disk. Options
    /// repeat an `option` key so the record stays line-oriented and a reader
    /// that does not know about options simply ignores them.
    fn encode(&self) -> String {
        let mut out = format!(
            "id\t{}\nworker\t{}\nmode\t{}\nquestion\t{}\ncreated\t{}\n",
            esc(&self.id),
            esc(&self.worker),
            esc(&self.mode),
            esc(&self.question),
            self.created
        );
        for (oid, label) in &self.options {
            out.push_str(&format!("option\t{}\t{}\n", esc(oid), esc(label)));
        }
        out
    }

    /// Parse the flat format. Unknown keys are ignored; a record missing `id`
    /// or `question` is rejected — a half-written file must never surface as
    /// though it were a real ask.
    fn decode(text: &str) -> Option<PendingQuestion> {
        let mut q = PendingQuestion {
            id: String::new(),
            worker: String::new(),
            question: String::new(),
            mode: "one".to_string(),
            options: Vec::new(),
            created: 0,
        };
        for line in text.lines() {
            let Some((k, v)) = line.split_once('\t') else {
                continue;
            };
            match k {
                "id" => q.id = unesc(v),
                "worker" => q.worker = unesc(v),
                "question" => q.question = unesc(v),
                "mode" => q.mode = unesc(v),
                "created" => q.created = v.trim().parse().unwrap_or(0),
                "option" => {
                    // `option\t<id>\t<label>`; a label-less option keeps its id
                    // as the display text, matching `ChoiceOption::display`.
                    let (oid, label) = v.split_once('\t').unwrap_or((v, ""));
                    let oid = unesc(oid);
                    if !oid.is_empty() {
                        q.options.push((oid, unesc(label)));
                    }
                }
                _ => {}
            }
        }
        if q.id.is_empty() || q.question.is_empty() {
            return None;
        }
        Some(q)
    }

    /// Rebuild a [`ChoiceSpec`] so the parked question can be re-prompted with
    /// the ordinary `choice.rs` popup. `span` is `(0, 0)`: there is no source
    /// text to strip here, the record IS the question.
    pub fn spec(&self) -> ChoiceSpec {
        ChoiceSpec {
            mode: mode_of(&self.mode),
            question: self.question.clone(),
            options: self
                .options
                .iter()
                .map(|(id, label)| ChoiceOption {
                    id: id.clone(),
                    label: label.clone(),
                    summary: String::new(),
                    attrs: Vec::new(),
                })
                .collect(),
            span: (0, 0),
        }
    }

    /// One row for the operator's `:questions` table.
    pub fn row(&self) -> String {
        format!(
            "{:<10} {:<12} {:<8} {} \x1b[2m({} option{})\x1b[0m",
            self.id,
            self.worker,
            self.mode,
            crate::escalation::one_line(&self.question, 60),
            self.options.len(),
            if self.options.len() == 1 { "" } else { "s" }
        )
    }

    /// The loud note surfaced on the operator's console the moment a worker
    /// parks a question, including the exact commands to answer with — an
    /// operator who cannot remember the verb cannot answer anything.
    pub fn console_note(&self) -> String {
        format!(
            "DECISION NEEDED — worker is asking you to choose\n   question: {}\n   options:  {}\n   answer:   :answer {}   ·   dismiss: :dismiss {}",
            crate::escalation::one_line(&self.question, 72),
            self.options
                .iter()
                .map(|(id, _)| id.as_str())
                .collect::<Vec<_>>()
                .join(", "),
            self.id,
            self.id
        )
    }
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Mint a short, collision-resistant question id. Not a secret — a human types
/// it — so it only has to be unique among the handful of live questions. The
/// `q` prefix keeps it visually distinct from an approval's `a` id.
fn mint_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64 ^ d.as_secs())
        .unwrap_or(0);
    let pid = std::process::id() as u64;
    format!("q{:06x}", (nanos ^ (pid << 12)) & 0xff_ffff)
}

fn question_path(dir: &Path, id: &str) -> PathBuf {
    dir.join(format!("{id}.question"))
}

fn answer_path(dir: &Path, id: &str) -> PathBuf {
    dir.join(format!("{id}.answer"))
}

/// Park a worker-proposed question so it outlives the turn that asked it.
///
/// Returns the stored record (whose `id` is what the operator types) or `None`
/// when the spec is unusable or the store cannot be written. A failure here is
/// deliberately non-fatal: the caller falls back to printing the raw block,
/// which is exactly the pre-existing behaviour.
pub fn park(worker: &str, spec: &ChoiceSpec) -> Option<PendingQuestion> {
    // An option-less block is not a decision the operator can make, and a
    // question-less one gives them nothing to decide about. Either way there is
    // nothing worth persisting — let the raw text print instead.
    if spec.options.is_empty() || spec.question.trim().is_empty() {
        return None;
    }
    let worker = worker.trim();
    // A coordinator that re-emits the SAME block every round (a common shape:
    // the model repeats its ask until something answers it) must not stack a
    // new row per round — the operator would face a tray full of identical
    // questions with different ids, only one of which routes anywhere useful.
    // The first park wins; later identical asks return the live record so the
    // caller can re-surface the SAME id.
    let asking = spec.question.trim();
    if let Some(dup) = pending_for(worker).into_iter().find(|q| q.question == asking) {
        return Some(dup);
    }
    let q = PendingQuestion {
        id: mint_id(),
        worker: if worker.is_empty() {
            "local".to_string()
        } else {
            worker.to_string()
        },
        question: spec.question.trim().to_string(),
        mode: mode_str(spec.mode).to_string(),
        options: spec
            .options
            .iter()
            .map(|o| (o.id.clone(), o.label.clone()))
            .collect(),
        created: now_secs(),
    };
    let dir = questions_dir();
    std::fs::write(question_path(&dir, &q.id), q.encode()).ok()?;
    Some(q)
}

/// Every pending question, oldest first — the operator's `:questions` list.
pub fn pending() -> Vec<PendingQuestion> {
    let dir = questions_dir();
    sweep_stale(&dir);
    let mut out: Vec<PendingQuestion> = Vec::new();
    let Ok(rd) = std::fs::read_dir(&dir) else {
        return out;
    };
    for entry in rd.flatten() {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("question") {
            continue;
        }
        if let Ok(text) = std::fs::read_to_string(&path)
            && let Some(q) = PendingQuestion::decode(&text)
        {
            out.push(q);
        }
    }
    out.sort_by(|a, b| a.created.cmp(&b.created).then_with(|| a.id.cmp(&b.id)));
    out
}

/// The pending questions parked BY one coordinator, oldest first.
///
/// `run_id` may be a full run id or the short prefix the operator sees in the
/// console, matched in either direction so a short-vs-long mismatch still
/// resolves — same tolerance as [`crate::approval::pending_for`].
pub fn pending_for(run_id: &str) -> Vec<PendingQuestion> {
    let run_id = run_id.trim();
    if run_id.is_empty() {
        return Vec::new();
    }
    pending()
        .into_iter()
        .filter(|q| worker_matches(&q.worker, run_id))
        .collect()
}

/// Does a recorded `worker` label denote the coordinator `run_id`?
///
/// Prefix-tolerant in BOTH directions. `local` and the empty label never match
/// a real run id — otherwise attaching to any worker would replay every
/// non-coordinator ask.
fn worker_matches(worker: &str, run_id: &str) -> bool {
    if worker.is_empty() || worker == "local" || run_id.is_empty() {
        return false;
    }
    worker == run_id || worker.starts_with(run_id) || run_id.starts_with(worker)
}

/// The block re-printed when the operator ATTACHES to a coordinator that has a
/// parked question — `None` when it has none.
///
/// The console note fired once, when the worker asked; `:attach` then wipes the
/// pane and replays the transcript, so by the time the operator is looking at
/// the worker the id they need has scrolled away. Re-surfacing here is what
/// makes the ask PERSIST: every attach reprints it, and it stops appearing the
/// moment it is answered or dismissed.
pub fn attach_notice(run_id: &str) -> Option<String> {
    render_notice(
        &pending_for(run_id),
        "from this coordinator — it is waiting on your decision",
    )
}

/// The block printed once at interactive session start, covering questions
/// parked by ANY worker — including ones whose process has long since exited.
///
/// This is the half that survives a restart: without it, a question parked
/// while the operator was away would only reappear if they happened to attach
/// to the right worker, and a question from a finished worker would never
/// reappear at all.
pub fn startup_notice() -> Option<String> {
    render_notice(&pending(), "waiting on your decision")
}

/// Pure renderer shared by [`attach_notice`] and [`startup_notice`] — `None`
/// for an empty slice, so a caller can `if let Some(..)` and print nothing.
fn render_notice(qs: &[PendingQuestion], scope: &str) -> Option<String> {
    if qs.is_empty() {
        return None;
    }
    let mut out = format!(
        "\x1b[1;36m◆ {} pending question{} {scope}\x1b[0m",
        qs.len(),
        if qs.len() == 1 { "" } else { "s" }
    );
    for q in qs {
        out.push_str(&format!(
            "\n  \x1b[1m{}\x1b[0m \x1b[2m{}\x1b[0m {}\n     \x1b[2moptions: {}\x1b[0m\n     answer: \x1b[32m:answer {}\x1b[0m · \x1b[2m:dismiss {}\x1b[0m",
            q.id,
            q.worker,
            crate::escalation::one_line(&q.question, 68),
            q.options
                .iter()
                .map(|(id, _)| id.as_str())
                .collect::<Vec<_>>()
                .join(", "),
            q.id,
            q.id
        ));
    }
    Some(out)
}

/// Resolve an operator-typed id (exact, or an unambiguous prefix) to a pending
/// question. An AMBIGUOUS prefix is an error rather than a coin flip —
/// answering the wrong worker's question because two ids shared two characters
/// is exactly the mistake worth one extra keystroke to avoid.
pub fn resolve(id: &str) -> Result<PendingQuestion, String> {
    let id = id.trim();
    if id.is_empty() {
        return Err("no question id given".to_string());
    }
    let all = pending();
    if let Some(exact) = all.iter().find(|q| q.id == id) {
        return Ok(exact.clone());
    }
    let hits: Vec<&PendingQuestion> = all.iter().filter(|q| q.id.starts_with(id)).collect();
    match hits.len() {
        0 => Err(format!("no pending question matches '{id}'")),
        1 => Ok(hits[0].clone()),
        n => Err(format!(
            "'{id}' is ambiguous — {n} pending questions match; use the full id"
        )),
    }
}

/// Record the operator's answer for `id` and clear it from the tray.
///
/// Writes the answer file (so a worker still polling can pick it up) and
/// REMOVES the question file, which is what takes the row out of every
/// subsequent `pending()` / `attach_notice` / `startup_notice`. Returns the
/// question it answered so the caller can echo what was just decided and route
/// the answer on to the worker over the ordinary `tell` channel.
pub fn answer(id: &str, answer: &str) -> Result<PendingQuestion, String> {
    let q = resolve(id)?;
    let dir = questions_dir();
    std::fs::write(answer_path(&dir, &q.id), format!("{}\n", answer.trim()))
        .map_err(|e| format!("could not write answer: {e}"))?;
    // Only unlink AFTER the answer landed. The other order can lose the
    // operator's decision entirely if the write fails — the question would be
    // gone from the tray with nothing recorded in its place.
    std::fs::remove_file(question_path(&dir, &q.id))
        .map_err(|e| format!("could not clear question: {e}"))?;
    Ok(q)
}

/// Discard a pending question WITHOUT answering it — the explicit escape hatch
/// for an ask that has gone stale (the worker died, the decision was made out
/// of band, the question no longer applies).
///
/// Deliberately distinct from [`answer`]: no answer file is written, so nothing
/// downstream can mistake a dismissal for a choice. This is the only
/// operator-driven path that removes a row without deciding it.
pub fn dismiss(id: &str) -> Result<PendingQuestion, String> {
    let q = resolve(id)?;
    let dir = questions_dir();
    std::fs::remove_file(question_path(&dir, &q.id))
        .map_err(|e| format!("could not dismiss question: {e}"))?;
    // Any answer file from a previous half-completed attempt goes too, so a
    // dismissed question cannot be resurrected by a stale verdict.
    let _ = std::fs::remove_file(answer_path(&dir, &q.id));
    Ok(q)
}

/// Read and consume the answer a worker is waiting for, if the operator has
/// given one. `None` means "not answered yet" — the worker keeps waiting.
pub fn take_answer(id: &str) -> Option<String> {
    let dir = questions_dir();
    let path = answer_path(&dir, id);
    let text = std::fs::read_to_string(&path).ok()?;
    let _ = std::fs::remove_file(&path);
    let text = text.trim();
    if text.is_empty() {
        None
    } else {
        Some(text.to_string())
    }
}

/// Drop questions whose worker is long gone. Best-effort and silent: a sweep
/// failure must never block a live question.
fn sweep_stale(dir: &Path) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    let cutoff = now_secs().saturating_sub(STALE_AFTER.as_secs());
    for entry in rd.flatten() {
        let path = entry.path();
        let ext = path.extension().and_then(|s| s.to_str());
        if ext != Some("question") && ext != Some("answer") {
            continue;
        }
        let stale = entry
            .metadata()
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_secs() < cutoff)
            .unwrap_or(false);
        if stale {
            let _ = std::fs::remove_file(&path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Point `$HOME` at a scratch dir so the tests drive a real store without
    /// touching the operator's `~/.aish/questions`. Serialised by the caller
    /// (each test gets its own dir, and `questions_dir` reads `$HOME` fresh on
    /// every call, so the only shared state is the env var itself).
    struct TempHome {
        dir: PathBuf,
        prev: Option<String>,
    }

    impl TempHome {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "aish-qtest-{}-{}-{tag}",
                std::process::id(),
                now_nanos()
            ));
            std::fs::create_dir_all(&dir).expect("scratch home");
            let prev = std::env::var("HOME").ok();
            // SAFETY: tests in this module are serialised by the mutex below,
            // so no other thread is reading HOME while it is swapped.
            unsafe { std::env::set_var("HOME", &dir) };
            Self { dir, prev }
        }
    }

    impl Drop for TempHome {
        fn drop(&mut self) {
            match &self.prev {
                Some(p) => unsafe { std::env::set_var("HOME", p) },
                None => unsafe { std::env::remove_var("HOME") },
            }
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn now_nanos() -> u128 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    }

    /// `$HOME` is process-global, so the store tests take a lock rather than
    /// racing each other into the wrong scratch dir.
    static HOME_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn spec(question: &str, mode: ChoiceMode, opts: &[(&str, &str)]) -> ChoiceSpec {
        ChoiceSpec {
            mode,
            question: question.to_string(),
            options: opts
                .iter()
                .map(|(id, label)| ChoiceOption {
                    id: (*id).to_string(),
                    label: (*label).to_string(),
                    summary: String::new(),
                    attrs: Vec::new(),
                })
                .collect(),
            span: (0, 0),
        }
    }

    #[test]
    fn encode_decode_round_trips_every_field() {
        let q = PendingQuestion {
            id: "q0a1b2".into(),
            worker: "w_5RN6BgsW".into(),
            question: "ship it?".into(),
            mode: "many".into(),
            options: vec![
                ("yes".into(), "Ship now".into()),
                ("no".into(), "Hold".into()),
            ],
            created: 1234,
        };
        let back = PendingQuestion::decode(&q.encode()).expect("decodes");
        assert_eq!(back, q);
    }

    /// The metacharacters of the flat format must survive a round trip, since
    /// agent-authored labels routinely contain paths and newlines.
    #[test]
    fn encode_decode_survives_tabs_newlines_and_backslashes() {
        let q = PendingQuestion {
            id: "q0a1b2".into(),
            worker: "local".into(),
            question: "line1\nline2\twith tab \\ and backslash".into(),
            mode: "one".into(),
            options: vec![("a\\b".into(), "C:\\n\tot a newline".into())],
            created: 7,
        };
        let back = PendingQuestion::decode(&q.encode()).expect("decodes");
        assert_eq!(back, q);
        assert_eq!(back.question, "line1\nline2\twith tab \\ and backslash");
        assert_eq!(back.options[0].1, "C:\\n\tot a newline");
    }

    #[test]
    fn decode_rejects_records_missing_id_or_question() {
        assert!(PendingQuestion::decode("worker\tw_1\nquestion\thi\n").is_none());
        assert!(PendingQuestion::decode("id\tq1\nworker\tw_1\n").is_none());
        assert!(PendingQuestion::decode("").is_none());
    }

    #[test]
    fn spec_rebuilds_mode_and_options_for_repromptingx() {
        let q = PendingQuestion {
            id: "q1".into(),
            worker: "w_1".into(),
            question: "pick".into(),
            mode: "compare".into(),
            options: vec![("a".into(), "Alpha".into()), ("b".into(), String::new())],
            created: 0,
        };
        let s = q.spec();
        assert_eq!(s.mode, ChoiceMode::Compare);
        assert_eq!(s.question, "pick");
        assert_eq!(s.options.len(), 2);
        assert_eq!(s.options[0].id, "a");
        assert_eq!(s.options[0].display(), "Alpha");
        // A label-less option falls back to its id, matching ChoiceOption.
        assert_eq!(s.options[1].display(), "b");
    }

    /// An unknown mode must read back as the conservative single-pick rather
    /// than silently widening a radio into a multi-select.
    #[test]
    fn unknown_mode_falls_back_to_one() {
        assert_eq!(mode_of("gibberish"), ChoiceMode::One);
        assert_eq!(mode_of("MANY"), ChoiceMode::Many);
        assert_eq!(mode_of("comparison"), ChoiceMode::Compare);
    }

    /// (a) THE BUG: a question parked by a worker must still be there after the
    /// operator detaches and a brand-new session reloads the store from disk.
    /// Nothing is cached in-process, so re-reading is exactly what a restart
    /// does.
    #[test]
    fn pending_question_survives_detach_and_session_reload() {
        let _g = HOME_LOCK.lock().unwrap();
        let _home = TempHome::new("persist");

        let parked = park("w_5RN6BgsW", &spec("ship it?", ChoiceMode::One, &[("y", "Yes")]))
            .expect("parks");

        // Simulated detach + fresh session: every read goes back to disk.
        let after_reload = pending();
        assert_eq!(after_reload.len(), 1, "question lost across reload");
        assert_eq!(after_reload[0].id, parked.id);
        assert_eq!(after_reload[0].question, "ship it?");

        // And it is re-surfaceable on BOTH re-entry paths.
        assert!(
            startup_notice().is_some(),
            "startup must re-surface the ask"
        );
        let note = attach_notice("w_5RN6BgsW").expect("attach must re-surface the ask");
        assert!(note.contains(&parked.id), "notice must name the id to type");
        assert!(note.contains(":answer"), "notice must name the verb");

        // Re-surfacing is idempotent — it stays pending until resolved, so a
        // second attach shows it again rather than consuming it.
        assert!(attach_notice("w_5RN6BgsW").is_some());
        assert_eq!(pending().len(), 1);
    }

    /// Attach is scoped: another coordinator's question must not show up, and a
    /// short/long run-id mismatch must still resolve.
    #[test]
    fn attach_notice_is_scoped_to_the_asking_worker() {
        let _g = HOME_LOCK.lock().unwrap();
        let _home = TempHome::new("scope");

        park("w_aaaaaaaa", &spec("a?", ChoiceMode::One, &[("x", "X")])).expect("parks");
        park("w_bbbbbbbb", &spec("b?", ChoiceMode::One, &[("y", "Y")])).expect("parks");

        assert_eq!(pending_for("w_aaaaaaaa").len(), 1);
        assert_eq!(pending_for("w_aaaaaaaa")[0].question, "a?");
        // Prefix tolerance in both directions.
        assert_eq!(pending_for("w_aaaa").len(), 1);
        assert_eq!(pending_for("w_bbbbbbbb_extra").len(), 1);
        assert!(pending_for("w_cccccccc").is_empty());
        // A `local` ask belongs to no coordinator.
        park("", &spec("local?", ChoiceMode::One, &[("z", "Z")])).expect("parks");
        assert!(pending_for("local").is_empty());
        // …but it is still in the operator's global tray.
        assert_eq!(pending().len(), 3);
    }

    /// (c) Answering RESOLVES it: the row leaves the tray and the answer is
    /// readable by the worker that asked.
    #[test]
    fn answering_resolves_and_clears_the_pending_entry() {
        let _g = HOME_LOCK.lock().unwrap();
        let _home = TempHome::new("answer");

        let q = park("w_1234abcd", &spec("ship?", ChoiceMode::One, &[("y", "Yes")])).expect("parks");
        assert_eq!(pending().len(), 1);

        let answered = answer(&q.id, "{\"choice\":\"y\"}").expect("answers");
        assert_eq!(answered.id, q.id);

        // Gone from every surface, including after a reload.
        assert!(pending().is_empty(), "answered question still pending");
        assert!(attach_notice("w_1234abcd").is_none());
        assert!(startup_notice().is_none());

        // And the worker can collect it exactly once.
        assert_eq!(take_answer(&q.id).as_deref(), Some("{\"choice\":\"y\"}"));
        assert!(take_answer(&q.id).is_none(), "answer must be consumed once");
    }

    /// (b) Dismissal clears the entry WITHOUT recording an answer — nothing
    /// downstream may mistake a dismissal for a decision.
    #[test]
    fn dismissal_clears_the_entry_without_answering() {
        let _g = HOME_LOCK.lock().unwrap();
        let _home = TempHome::new("dismiss");

        let q = park("w_9999zzzz", &spec("ship?", ChoiceMode::Many, &[("y", "Yes")]))
            .expect("parks");
        assert_eq!(pending().len(), 1);

        let dropped = dismiss(&q.id).expect("dismisses");
        assert_eq!(dropped.id, q.id);
        assert!(pending().is_empty(), "dismissed question still pending");
        assert!(attach_notice("w_9999zzzz").is_none());
        assert!(
            take_answer(&q.id).is_none(),
            "dismissal must not look like an answer"
        );

        // Dismissing twice is an error, not a silent success — the operator
        // should know the id is already gone.
        assert!(dismiss(&q.id).is_err());
    }

    /// Esc out of the popup is NOT a resolution. The REPL models that by simply
    /// not calling `answer`/`dismiss`, so the row must still be pending.
    #[test]
    fn esc_does_not_resolve_the_question() {
        let _g = HOME_LOCK.lock().unwrap();
        let _home = TempHome::new("esc");

        let q = park("w_esc00000", &spec("ship?", ChoiceMode::One, &[("y", "Yes")])).expect("parks");
        // …operator opens the popup and hits Esc: no answer, no dismiss.
        assert_eq!(pending().len(), 1);
        assert_eq!(pending()[0].id, q.id);
        assert!(attach_notice("w_esc00000").is_some());
    }

    #[test]
    fn resolve_accepts_prefixes_and_rejects_ambiguity() {
        let _g = HOME_LOCK.lock().unwrap();
        let _home = TempHome::new("resolve");

        let q = park("w_1", &spec("one?", ChoiceMode::One, &[("y", "Yes")])).expect("parks");
        assert_eq!(resolve(&q.id).expect("exact").id, q.id);
        assert_eq!(resolve(&q.id[..3]).expect("prefix").id, q.id);
        assert!(resolve("nope").is_err());
        assert!(resolve("").is_err());

        // Two ids sharing the `q` prefix must be an error, never a coin flip.
        let dir = questions_dir();
        let twin = PendingQuestion {
            id: format!("{}zz", &q.id[..3]),
            worker: "w_2".into(),
            question: "two?".into(),
            mode: "one".into(),
            options: vec![("y".into(), "Yes".into())],
            created: 1,
        };
        std::fs::write(question_path(&dir, &twin.id), twin.encode()).expect("write twin");
        let err = resolve(&q.id[..3]).expect_err("ambiguous");
        assert!(err.contains("ambiguous"), "got: {err}");
    }

    /// A block with nothing to choose between is not a decision — it must fall
    /// through to the ordinary raw-text path rather than park an empty row.
    #[test]
    fn park_refuses_unusable_specs() {
        let _g = HOME_LOCK.lock().unwrap();
        let _home = TempHome::new("refuse");

        assert!(park("w_1", &spec("no options?", ChoiceMode::One, &[])).is_none());
        assert!(park("w_1", &spec("   ", ChoiceMode::One, &[("y", "Yes")])).is_none());
        assert!(pending().is_empty());
    }

    #[test]
    fn notice_renderers_are_none_when_nothing_is_pending() {
        assert!(render_notice(&[], "scope").is_none());
    }

    #[test]
    fn pending_is_ordered_oldest_first() {
        let _g = HOME_LOCK.lock().unwrap();
        let _home = TempHome::new("order");

        let dir = questions_dir();
        for (id, created) in [("qzzz001", 300u64), ("qaaa002", 100), ("qmmm003", 200)] {
            let q = PendingQuestion {
                id: id.into(),
                worker: "w_1".into(),
                question: format!("q{created}"),
                mode: "one".into(),
                options: vec![("y".into(), "Yes".into())],
                created,
            };
            std::fs::write(question_path(&dir, id), q.encode()).expect("write");
        }
        let ids: Vec<String> = pending().into_iter().map(|q| q.id).collect();
        assert_eq!(ids, vec!["qaaa002", "qmmm003", "qzzz001"]);
    }
}
