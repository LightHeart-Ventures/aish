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

        // Esc clears the current input line (a harmless no-op when it's already
        // empty, so it only clears when there's text to clear).
        rl.bind_sequence(
            KeyEvent(KeyCode::Esc, Modifiers::NONE),
            EventHandler::Simple(Cmd::Kill(Movement::WholeBuffer)),
        );

        Ok(Self {
            rl,
            history_path,
            raw_toggle,
        })
    }
}

impl LineEditor for RustylineEditor {
    fn set_cwd(&mut self, cwd: &Path) {
        if let Some(h) = self.rl.helper_mut() {
            h.set_cwd(cwd);
        }
    }

    fn read_line(&mut self, prompt: &str) -> ReadOutcome {
        match self.rl.readline(prompt) {
            Ok(line) => ReadOutcome::Line(line),
            Err(ReadlineError::Interrupted) => {
                // Ctrl-O routes here too (handler returns Interrupt); the toggle
                // flag distinguishes it from a plain Ctrl-C clear-line.
                if self.raw_toggle.swap(false, Ordering::SeqCst) {
                    ReadOutcome::CtrlO
                } else {
                    ReadOutcome::Interrupted
                }
            }
            Err(ReadlineError::Eof) => ReadOutcome::Eof,
            Err(e) => ReadOutcome::Error(e.to_string()),
        }
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
