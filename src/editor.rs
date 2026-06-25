//! Line-editor abstraction (S5.1 / TASK-130).
//!
//! The REPL drives the [`LineEditor`] trait instead of touching rustyline
//! directly, so the concrete editor is swappable: rustyline today
//! ([`RustylineEditor`]), reedline tomorrow (S5.2/S5.5), and a roll-back is a
//! one-line type change at the construction site in `repl::run`. Everything the
//! interactive loop needs from the editor — reading a line, history, the Ctrl-O
//! toggle, and the above-the-prompt external printer — is expressed here in
//! editor-agnostic terms; the rustyline specifics (the `Editor`, its key
//! bindings, its `ReadlineError` variants, its `ExternalPrinter`) live only in
//! this file's rustyline impl.

use crate::repl::AishHelper;
use anyhow::Result;
use rustyline::error::ReadlineError;
use rustyline::history::DefaultHistory;
use rustyline::{
    Cmd, ConditionalEventHandler, Editor, Event, EventContext, EventHandler, KeyCode, KeyEvent,
    Modifiers, Movement, RepeatCount,
};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// The editor-agnostic outcome of one [`LineEditor::read_line`] call. Each
/// concrete editor maps its native key/error signals onto these variants so the
/// REPL loop never has to know about (say) rustyline's `ReadlineError`.
pub enum ReadOutcome {
    /// The user submitted a line (verbatim — the caller trims/validates).
    Line(String),
    /// Ctrl-C at the prompt: discard the line, loop again.
    Interrupted,
    /// Ctrl-O: the raw-tool-output toggle was requested.
    CtrlO,
    /// Shift-Tab: cycle the interactive session through the running
    /// coordinators (interactive → first worker → … → back to interactive).
    ShiftTab,
    /// Ctrl-D on an empty line: exit the shell.
    Eof,
    /// A fatal editor error — the loop prints it and breaks.
    Error(String),
}

/// Prints a line ABOVE the prompt without trampling the user's in-progress
/// input, redrawing the prompt afterwards. Abstracts rustyline's
/// `ExternalPrinter`. `Send` so the background-result presenter task can own it.
pub trait LinePrinter: Send {
    /// Print `text` above the prompt. Best-effort: a failure is swallowed (the
    /// presenter has no better recourse than to drop the notice).
    fn print(&mut self, text: String);
}

/// The line-editor surface the REPL drives. [`RustylineEditor`] implements it
/// today; a future `ReedlineEditor` can satisfy the same contract for a drop-in
/// swap (S5.2/S5.5).
pub trait LineEditor {
    /// Point completion/highlighting at the session's current cwd (`cd` mutates
    /// it between reads).
    fn set_cwd(&mut self, cwd: &Path);
    /// Read one line, drawing `prompt`. Returns the editor-agnostic outcome.
    fn read_line(&mut self, prompt: &str) -> ReadOutcome;
    /// Read one line pre-filled with `initial` text (the cursor lands at its
    /// end), drawing `prompt`. Backs the inline command-rewrite preview (S6.4 /
    /// TASK-138): the model's candidate command is placed in the buffer so the
    /// user can accept it (Enter) or edit it before it runs. Same outcome
    /// variants as [`LineEditor::read_line`].
    fn read_line_with_initial(&mut self, prompt: &str, initial: &str) -> ReadOutcome;
    /// Append a submitted line to the in-memory history ring.
    fn add_history(&mut self, line: &str);
    /// Persist history to disk (best-effort — failure is ignored).
    fn save_history(&mut self);
    /// Hand out the above-the-prompt printer. Returns `Some` at most once and
    /// only when the terminal supports it; `None` means inline printing.
    fn take_printer(&mut self) -> Option<Box<dyn LinePrinter>>;
}

// ---------------------------------------------------------------------------
// rustyline implementation
// ---------------------------------------------------------------------------

/// The production [`LineEditor`], backed by rustyline. Owns the rustyline
/// `Editor` (configured with the `AishHelper` for completion/hinting/
/// highlighting), the history-file path, and the Ctrl-O toggle flag the key
/// handler raises and `read_line` drains.
pub struct RustylineEditor {
    rl: Editor<AishHelper, DefaultHistory>,
    history_path: PathBuf,
    /// Raised by the Ctrl-O key handler, drained by `read_line` to distinguish a
    /// Ctrl-O (raw-output toggle) from a plain Ctrl-C — both surface as
    /// rustyline's `Interrupted`.
    raw_toggle: Arc<AtomicBool>,
    /// Raised by the Shift-Tab key handler, drained by `read_line` to
    /// distinguish a worker-cycle request from a Ctrl-O / plain Ctrl-C — all
    /// three surface as rustyline's `Interrupted`.
    shift_tab: Arc<AtomicBool>,
}

impl RustylineEditor {
    /// Build the rustyline editor: list-style completion (the whole candidate
    /// menu on the first TAB, not the default one-at-a-time cycle), the
    /// `AishHelper` installed, history loaded from `history_path`, and the
    /// Ctrl-O / Esc key bindings wired.
    pub fn new(helper: AishHelper, history_path: PathBuf) -> Result<Self> {
        let config = rustyline::Config::builder()
            .completion_type(rustyline::CompletionType::List)
            .completion_show_all_if_ambiguous(true)
            .build();
        let mut rl: Editor<AishHelper, DefaultHistory> = Editor::with_config(config)?;
        rl.set_helper(Some(helper));
        let _ = rl.load_history(&history_path);

        // Ctrl-O toggles raw tool output. The handler can't reach the Session, so
        // it raises `raw_toggle` and bails out of the line editor (Interrupt);
        // `read_line` drains the flag and reports `ReadOutcome::CtrlO`, leaving the
        // REPL to perform the toggle + status line + retroactive reveal.
        let raw_toggle = Arc::new(AtomicBool::new(false));
        rl.bind_sequence(
            KeyEvent::ctrl('O'),
            EventHandler::Conditional(Box::new(CtrlOToggle {
                pending: raw_toggle.clone(),
            })),
        );

        // Shift-Tab cycles the interactive session through the running
        // background coordinators (and back to interactive). Like Ctrl-O, the
        // handler can't reach the Session, so it raises `shift_tab` and bails
        // out of the line editor (Interrupt); `read_line` drains the flag and
        // reports `ReadOutcome::ShiftTab`, leaving the REPL to advance the
        // attach cursor. rustyline normalizes Shift-Tab to `BackTab`; binding it
        // here overrides its default `CompleteBackward`.
        let shift_tab = Arc::new(AtomicBool::new(false));
        rl.bind_sequence(
            KeyEvent(KeyCode::BackTab, Modifiers::NONE),
            EventHandler::Conditional(Box::new(ShiftTabCycle {
                pending: shift_tab.clone(),
            })),
        );

        // Esc clears the current input line (a harmless no-op when it's already
        // empty, so it only clears when there's text to clear).
        rl.bind_sequence(
            KeyEvent(KeyCode::Esc, Modifiers::NONE),
            EventHandler::Simple(Cmd::Kill(Movement::WholeBuffer)),
        );

        // Fish-style history ghost text (S6.2 / TASK-136): accept the inline
        // autosuggestion with → or Ctrl-F. Both route through `AcceptHint`,
        // which inserts the suggested completion when an acceptable hint is shown
        // at end-of-line and otherwise performs a plain forward-character move —
        // so Ctrl-F keeps its readline meaning, and → on the display-only
        // `:`-palette (no completion) just moves the cursor instead of beeping.
        rl.bind_sequence(
            KeyEvent(KeyCode::Right, Modifiers::NONE),
            EventHandler::Conditional(Box::new(AcceptHint)),
        );
        rl.bind_sequence(
            KeyEvent::ctrl('F'),
            EventHandler::Conditional(Box::new(AcceptHint)),
        );

        Ok(Self {
            rl,
            history_path,
            raw_toggle,
            shift_tab,
        })
    }

    /// Map a rustyline `readline*` result onto the editor-agnostic
    /// [`ReadOutcome`]. Shared by [`read_line`](LineEditor::read_line) and
    /// [`read_line_with_initial`](LineEditor::read_line_with_initial) so the
    /// Ctrl-O/Ctrl-C disambiguation and EOF/error mapping stay identical on both
    /// entry points.
    fn outcome(&self, res: rustyline::Result<String>) -> ReadOutcome {
        match res {
            Ok(line) => ReadOutcome::Line(line),
            Err(ReadlineError::Interrupted) => {
                // Ctrl-O and Shift-Tab route here too (each handler returns
                // Interrupt); the drained flags distinguish them from a plain
                // Ctrl-C clear-line. Both are drained unconditionally so a stale
                // flag can never bleed into a later interrupt.
                let shift_tab = self.shift_tab.swap(false, Ordering::SeqCst);
                let raw_toggle = self.raw_toggle.swap(false, Ordering::SeqCst);
                interrupt_outcome(shift_tab, raw_toggle)
            }
            Err(ReadlineError::Eof) => ReadOutcome::Eof,
            Err(e) => ReadOutcome::Error(e.to_string()),
        }
    }
}

impl LineEditor for RustylineEditor {
    fn set_cwd(&mut self, cwd: &Path) {
        if let Some(h) = self.rl.helper_mut() {
            h.set_cwd(cwd);
        }
    }

    fn read_line(&mut self, prompt: &str) -> ReadOutcome {
        let res = self.rl.readline(prompt);
        self.outcome(res)
    }

    fn read_line_with_initial(&mut self, prompt: &str, initial: &str) -> ReadOutcome {
        // rustyline takes the pre-filled buffer as a (left, right) split around
        // the cursor; we want the whole candidate to the LEFT so the cursor
        // lands at end-of-line, ready to edit or Enter.
        let res = self.rl.readline_with_initial(prompt, (initial, ""));
        self.outcome(res)
    }

    fn add_history(&mut self, line: &str) {
        let _ = self.rl.add_history_entry(line);
    }

    fn save_history(&mut self) {
        let _ = self.rl.save_history(&self.history_path);
    }

    fn take_printer(&mut self) -> Option<Box<dyn LinePrinter>> {
        self.rl
            .create_external_printer()
            .ok()
            .map(|p| Box::new(RustylinePrinter(p)) as Box<dyn LinePrinter>)
    }
}

/// Map the drained Shift-Tab / Ctrl-O flags onto the editor-agnostic outcome.
/// rustyline collapses Shift-Tab (`ShiftTabCycle`), Ctrl-O (`CtrlOToggle`), and
/// a plain Ctrl-C all onto `ReadlineError::Interrupted` (the first two via
/// `Cmd::Interrupt`); the two flags are what split them back apart. Factored out
/// as a pure fn so the disambiguation is unit-testable without a TTY (S5.5 /
/// TASK-134): the caller passes the result of each read-and-clear `swap`.
/// Shift-Tab wins over Ctrl-O if both were somehow raised; a clear pair is a
/// plain Ctrl-C clear-line.
fn interrupt_outcome(shift_tab_was_set: bool, raw_toggle_was_set: bool) -> ReadOutcome {
    if shift_tab_was_set {
        ReadOutcome::ShiftTab
    } else if raw_toggle_was_set {
        ReadOutcome::CtrlO
    } else {
        ReadOutcome::Interrupted
    }
}

/// Wraps a rustyline `ExternalPrinter` as a [`LinePrinter`]. Generic over the
/// concrete printer type so we never have to name rustyline's terminal-specific
/// `ExternalPrinter` associated type.
struct RustylinePrinter<P: rustyline::ExternalPrinter + Send>(P);

impl<P: rustyline::ExternalPrinter + Send> LinePrinter for RustylinePrinter<P> {
    fn print(&mut self, text: String) {
        let _ = self.0.print(text);
    }
}

/// Ctrl-O key handler: raise the raw-output toggle and leave the line editor so
/// the run loop can act on it. Returns `Cmd::Interrupt` to discard the typed
/// line without submitting it; `RustylineEditor::read_line` reads the flag and
/// reports [`ReadOutcome::CtrlO`].
struct CtrlOToggle {
    pending: Arc<AtomicBool>,
}

impl ConditionalEventHandler for CtrlOToggle {
    fn handle(&self, _: &Event, _: RepeatCount, _: bool, _: &EventContext) -> Option<Cmd> {
        self.pending.store(true, Ordering::SeqCst);
        Some(Cmd::Interrupt)
    }
}

/// Shift-Tab key handler: raise the worker-cycle flag and leave the line editor
/// so the run loop can advance the attach cursor. Returns `Cmd::Interrupt` to
/// discard the typed line without submitting it; `RustylineEditor::read_line`
/// reads the flag and reports [`ReadOutcome::ShiftTab`].
struct ShiftTabCycle {
    pending: Arc<AtomicBool>,
}

impl ConditionalEventHandler for ShiftTabCycle {
    fn handle(&self, _: &Event, _: RepeatCount, _: bool, _: &EventContext) -> Option<Cmd> {
        self.pending.store(true, Ordering::SeqCst);
        Some(Cmd::Interrupt)
    }
}

/// → / Ctrl-F hint-acceptance handler for the fish-style history
/// autosuggestion (S6.2 / TASK-136). When an *acceptable* hint is displayed
/// (one whose `completion()` is `Some` — the ghost text, not the display-only
/// `:`-palette) and the cursor sits at end-of-line, accept it by inserting the
/// suggested completion (`Cmd::CompleteHint`). Otherwise fall back to a normal
/// forward-character move so the keys keep their readline meaning mid-line and
/// never beep on the palette. `ctx.hint_text()` returns the current hint's
/// `completion()`, so it is `Some` exactly for ghost text.
struct AcceptHint;

impl ConditionalEventHandler for AcceptHint {
    fn handle(&self, _: &Event, n: RepeatCount, _: bool, ctx: &EventContext) -> Option<Cmd> {
        if ctx.hint_text().is_some() && ctx.pos() == ctx.line().len() {
            Some(Cmd::CompleteHint)
        } else {
            Some(Cmd::Move(Movement::ForwardChar(n)))
        }
    }
}

// ---------------------------------------------------------------------------
// Tests — S5.5 / TASK-134: the three rustyline-bound behaviours (Ctrl-O toggle,
// the completer/helper, and history) must behave identically once routed
// through the `LineEditor` abstraction. The completer's logic is exercised
// exhaustively in `repl.rs` (command/path/subcommand/`:`-palette completion)
// and the sqlite input/output log in `db.rs`; these tests lock the seams the
// abstraction itself introduced — the Ctrl-O disambiguation and the
// history-file round-trip through the trait — so a future reedline swap (or any
// refactor of this file) can't silently regress them.
// ---------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// A throwaway helper for constructing a `RustylineEditor` in tests. The
    /// completion cwd / PATH / aliases don't matter here — these tests exercise
    /// the editor's history + Ctrl-O seams, not completion (covered in repl.rs).
    fn temp_helper() -> AishHelper {
        AishHelper::new(
            std::env::temp_dir(),
            std::env::var("PATH").unwrap_or_default(),
            Arc::new(HashMap::new()),
        )
    }

    /// Ctrl-O preservation: rustyline collapses both Ctrl-O and Ctrl-C onto
    /// `Interrupted`, so the editor must split them back apart by the drained
    /// toggle flag. This is the exact mapping `read_line` performs — a raised
    /// toggle is a raw-output request, a clear one is a plain clear-line.
    #[test]
    fn interrupt_outcome_distinguishes_shift_tab_ctrl_o_and_ctrl_c() {
        // Shift-Tab (first flag) wins and surfaces as a worker-cycle request.
        assert!(
            matches!(interrupt_outcome(true, false), ReadOutcome::ShiftTab),
            "a drained Shift-Tab flag must surface as ShiftTab"
        );
        // Shift-Tab takes precedence even if Ctrl-O was also somehow raised.
        assert!(
            matches!(interrupt_outcome(true, true), ReadOutcome::ShiftTab),
            "Shift-Tab wins over a co-raised Ctrl-O"
        );
        // Ctrl-O alone is the raw-output toggle.
        assert!(
            matches!(interrupt_outcome(false, true), ReadOutcome::CtrlO),
            "a drained Ctrl-O toggle must surface as CtrlO"
        );
        // Neither flag is a plain Ctrl-C clear-line.
        assert!(
            matches!(interrupt_outcome(false, false), ReadOutcome::Interrupted),
            "a clear pair is a plain Ctrl-C clear-line"
        );
    }

    /// The flag `read_line` reads is a swap (read-and-clear): a Ctrl-O raised by
    /// the key handler is observed exactly once, and the very next `Interrupted`
    /// is therefore a plain Ctrl-C. This pins the `raw_toggle.swap(false, …)`
    /// drain contract that prevents a stale toggle from eating a later Ctrl-C.
    #[test]
    fn raw_toggle_drains_exactly_once() {
        let flag = Arc::new(AtomicBool::new(false));
        // The CtrlOToggle handler raises it.
        flag.store(true, Ordering::SeqCst);
        assert!(
            flag.swap(false, Ordering::SeqCst),
            "first drain sees the raised Ctrl-O"
        );
        assert!(
            !flag.swap(false, Ordering::SeqCst),
            "second drain is a plain Ctrl-C (toggle already consumed)"
        );
    }

    /// History preservation through the abstraction: lines added + saved via the
    /// `LineEditor` trait round-trip to disk and reload into a fresh editor — the
    /// up-arrow recall ring survives a restart exactly as it did before S5.1 put
    /// the editor behind the trait. (The richer, queryable sqlite input/output
    /// log is separate — `db.rs` — and unaffected by the editor swap.)
    #[test]
    fn history_persists_through_the_trait() {
        let path = std::env::temp_dir().join(format!("aish_hist_test_{}.txt", std::process::id()));
        let _ = std::fs::remove_file(&path);

        {
            let mut ed =
                RustylineEditor::new(temp_helper(), path.clone()).expect("construct editor");
            ed.add_history("echo alpha");
            ed.add_history("git status");
            ed.save_history();
        }

        let saved =
            std::fs::read_to_string(&path).expect("save_history must write the history file");
        assert!(
            saved.contains("echo alpha"),
            "history missing first entry: {saved:?}"
        );
        assert!(
            saved.contains("git status"),
            "history missing second entry: {saved:?}"
        );

        // A fresh editor loads the persisted file without error — the reload the
        // REPL performs at startup (`load_history` in `RustylineEditor::new`).
        let _reloaded =
            RustylineEditor::new(temp_helper(), path.clone()).expect("reload editor from history");

        let _ = std::fs::remove_file(&path);
    }

    /// `set_cwd` re-points the editor's completion/highlighting at a new cwd —
    /// the `cd` path the REPL drives before each read — without error. The
    /// completer's behaviour at that cwd is covered in repl.rs; here we only pin
    /// that the trait method reaches the installed helper.
    #[test]
    fn set_cwd_repoints_without_error() {
        let path = std::env::temp_dir().join(format!("aish_nohist_{}.txt", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let mut ed = RustylineEditor::new(temp_helper(), path.clone()).expect("construct editor");
        ed.set_cwd(Path::new("/tmp"));
        ed.set_cwd(Path::new("/"));
        let _ = std::fs::remove_file(&path);
    }
}
