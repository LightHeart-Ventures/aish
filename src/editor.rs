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
    /// Ctrl-G: voice dictation key (feature = "voice"). When the user presses
    /// Ctrl-G the line editor is exited and the REPL runs the voice
    /// capture → transcribe → insert pipeline (TASK-367). Present
    /// unconditionally so REPL `match` arms compile with `--no-default-features`;
    /// the binding that raises it is gated behind `#[cfg(feature = "voice")]`.
    Voice,
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
    /// Raised by the Ctrl-G key handler (only compiled when `feature = "voice"`),
    /// drained by `read_line` to distinguish a voice dictation request from the
    /// other interrupt-routed keys. Gated so the default/CI build
    /// (`--no-default-features --locked`) sees no dead-code warnings.
    #[cfg(feature = "voice")]
    voice_flag: Arc<AtomicBool>,
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

        // Ctrl-G voice dictation — bind the key and init the flag only when the
        // `voice` feature is compiled in; neither exists in default/CI builds.
        #[cfg(feature = "voice")]
        let voice_flag = {
            let flag = Arc::new(AtomicBool::new(false));
            rl.bind_sequence(
                KeyEvent::ctrl('G'),
                EventHandler::Conditional(Box::new(CtrlGVoice {
                    pending: flag.clone(),
                })),
            );
            flag
        };

        Ok(Self {
            rl,
            history_path,
            raw_toggle,
            shift_tab,
            #[cfg(feature = "voice")]
            voice_flag,
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
                // Voice flag: drained here so a stale Ctrl-G can never bleed
                // into a subsequent Interrupted. Always false on non-voice builds.
                #[cfg(feature = "voice")]
                let voice = self.voice_flag.swap(false, Ordering::SeqCst);
                #[cfg(not(feature = "voice"))]
                let voice = false;
                interrupt_outcome(shift_tab, raw_toggle, voice)
            }
            Err(ReadlineError::Eof) => ReadOutcome::Eof,
            Err(e) => ReadOutcome::Error(e.to_string()),
        }
    }

    /// The idle prompt read. When this session has outstanding background
    /// workers (`background_pending`) AND stdin is an interactive tty, use an
    /// interruptible poll loop that returns an EMPTY line the instant the
    /// presenter raises the hands-free resume wake (`arm_resume_wake`) — so an
    /// armed auto-resume drains without the human pressing a key, on ANY kernel
    /// (no TIOCSTI dependency). In every other case fall through to the plain
    /// blocking rustyline read, so the common interactive path is unchanged.
    fn blocking_read(&mut self, prompt: &str) -> ReadOutcome {
        #[cfg(unix)]
        {
            let is_tty = unsafe { libc::isatty(libc::STDIN_FILENO) } == 1;
            if is_tty && background_pending() {
                if let Some(outcome) = self.poll_until_wake_or_key(prompt) {
                    return outcome;
                }
                // Raw-mode setup failed — fall through to the plain read.
            }
        }
        let res = self.rl.readline(prompt);
        self.outcome(res)
    }

    /// Poll-loop idle read used only while background workers are outstanding.
    /// Shows the prompt, then alternates short `poll(2)` waits on stdin with
    /// checks of the resume wake. Returns:
    ///   * `Some(Line(""))` — the wake fired: the loop drains the armed resume.
    ///   * `Some(<rustyline outcome>)` — a key arrived: control was handed to
    ///     rustyline for a full-fidelity edit.
    ///   * `None` — raw mode couldn't be established; caller does a plain read.
    #[cfg(unix)]
    fn poll_until_wake_or_key(&mut self, prompt: &str) -> Option<ReadOutcome> {
        use std::io::Write;
        // A resume that armed between the caller's `take_resume_tick` check and
        // now: return immediately so the loop drains it.
        if take_resume_wake() {
            return Some(ReadOutcome::Line(String::new()));
        }
        // Show the prompt while idle (cooked mode → normal output processing).
        {
            let mut out = std::io::stdout();
            let _ = write!(out, "{prompt}");
            let _ = out.flush();
        }
        // Raw mode so poll wakes on the first keystroke, not only on Enter.
        let guard = RawModeGuard::enter()?;
        // Tell `print_above_prompt` an idle prompt is on screen with no rustyline
        // edit-state behind it, so a forwarded worker row erases the prompt line
        // (and flags a repaint) instead of gluing onto it. Cleared at every exit.
        crate::tools::set_idle_prompt_active(true);
        loop {
            match poll_stdin_readable(200) {
                Ok(true) => {
                    // A key is waiting. Restore cooked mode (keeping the pending
                    // byte), erase our idle prompt, and hand the real edit to
                    // rustyline — it reprints the prompt and reads the buffered
                    // input with full line editing. `guard` drops here → cooked.
                    guard.restore();
                    crate::tools::set_idle_prompt_active(false);
                    erase_prompt_line();
                    let res = self.rl.readline(prompt);
                    return Some(self.outcome(res));
                }
                Ok(false) => {
                    if take_resume_wake() {
                        guard.restore();
                        crate::tools::set_idle_prompt_active(false);
                        erase_prompt_line();
                        return Some(ReadOutcome::Line(String::new()));
                    }
                    // A worker row printed above us erased the idle prompt line
                    // (via `print_above_prompt`); repaint it below the new output
                    // so the cursor never sits glued to forwarded worker text.
                    if crate::tools::take_idle_prompt_dirty() {
                        let mut out = std::io::stdout();
                        let _ = write!(out, "\r\x1b[2K{prompt}");
                        let _ = out.flush();
                    }
                }
                Err(_) => {
                    // Hard poll failure — restore, erase, and let the caller do
                    // a plain blocking read (return None).
                    guard.restore();
                    crate::tools::set_idle_prompt_active(false);
                    erase_prompt_line();
                    return None;
                }
            }
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
        // Mark the idle-at-prompt window so the footer heartbeat may repaint a
        // scrolled-away footer while we block here (cleared the moment a line
        // returns).
        crate::terminal::set_reading_line(true);
        let outcome = self.blocking_read(prompt);
        crate::terminal::set_reading_line(false);
        outcome
    }

    fn read_line_with_initial(&mut self, prompt: &str, initial: &str) -> ReadOutcome {
        // rustyline takes the pre-filled buffer as a (left, right) split around
        // the cursor; we want the whole candidate to the LEFT so the cursor
        // lands at end-of-line, ready to edit or Enter.
        crate::terminal::set_reading_line(true);
        let res = self.rl.readline_with_initial(prompt, (initial, ""));
        crate::terminal::set_reading_line(false);
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
fn interrupt_outcome(
    shift_tab_was_set: bool,
    raw_toggle_was_set: bool,
    voice_was_set: bool,
) -> ReadOutcome {
    // Voice takes priority: a deliberate dictation request is more specific
    // than a worker-cycle or raw-toggle key.
    if voice_was_set {
        ReadOutcome::Voice
    } else if shift_tab_was_set {
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

/// Ctrl-G voice dictation key handler (feature = "voice"): raise the voice flag
/// and exit the line editor so the REPL can run the capture → transcribe →
/// insert pipeline (TASK-367). Returns `Cmd::Interrupt` to discard the current
/// draft line; `RustylineEditor::read_line` drains the flag and reports
/// [`ReadOutcome::Voice`].
#[cfg(feature = "voice")]
struct CtrlGVoice {
    pending: Arc<AtomicBool>,
}

#[cfg(feature = "voice")]
impl ConditionalEventHandler for CtrlGVoice {
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
// Hands-free auto-resume WAKE (kernel-independent; supersedes the TIOCSTI nudge
// below for the common interactive path).
//
// The presenter thread, the moment it ARMS a coalesced resume (the last
// fanned-out coordinator finished — see session::ResumeState), raises
// `RESUME_WAKE`. The idle prompt read (`read_line`), while blocked, polls this
// flag between short `poll(2)` waits on stdin and returns an EMPTY line the
// instant it is raised — so the main loop's next pass drains the armed resume
// via `take_resume_tick` with NO keypress required, on ANY kernel. This works
// where `nudge_terminal_return` (TIOCSTI) silently no-ops: Linux >=6.2 gates
// `dev.tty.legacy_tiocsti` OFF by default (CVE-2017-5226).
//
// `BACKGROUND_PENDING` gates the (very slightly heavier) poll path: the main
// loop sets it true right before an idle read whenever this session has
// outstanding fanned-out workers. With no background work outstanding, the idle
// read is the plain blocking rustyline read, so the common interactive path is
// byte-for-byte unchanged.
static RESUME_WAKE: AtomicBool = AtomicBool::new(false);
static BACKGROUND_PENDING: AtomicBool = AtomicBool::new(false);

/// Presenter -> main-loop signal: raise the hands-free resume wake so an idle,
/// blocked `read_line` returns promptly and the next loop pass drains the armed
/// auto-resume. Paired with (and more reliable than) `nudge_terminal_return`.
pub fn arm_resume_wake() {
    RESUME_WAKE.store(true, Ordering::SeqCst);
}

/// Read-and-clear the resume wake. The idle poll read consults this; a raised
/// flag makes it return an empty line so the loop re-checks `take_resume_tick`.
pub fn take_resume_wake() -> bool {
    RESUME_WAKE.swap(false, Ordering::SeqCst)
}

/// Main loop -> editor: record whether this session currently has outstanding
/// fanned-out background workers. Gates the interruptible poll path in
/// `read_line`; when false the idle read is the plain blocking rustyline read.
pub fn set_background_pending(pending: bool) {
    BACKGROUND_PENDING.store(pending, Ordering::Relaxed);
}

fn background_pending() -> bool {
    BACKGROUND_PENDING.load(Ordering::Relaxed)
}

/// Poll stdin for readability up to `timeout_ms`. `Ok(true)` = a byte is ready,
/// `Ok(false)` = timed out (or an EINTR from e.g. SIGWINCH — reported as a
/// timeout so the caller simply loops), `Err` = a hard poll failure (caller
/// falls back to a plain blocking read).
#[cfg(unix)]
fn poll_stdin_readable(timeout_ms: i32) -> std::io::Result<bool> {
    let mut fd = libc::pollfd {
        fd: libc::STDIN_FILENO,
        events: libc::POLLIN,
        revents: 0,
    };
    let rc = unsafe { libc::poll(&mut fd, 1, timeout_ms) };
    if rc < 0 {
        let e = std::io::Error::last_os_error();
        if e.raw_os_error() == Some(libc::EINTR) {
            return Ok(false);
        }
        return Err(e);
    }
    Ok(rc > 0 && (fd.revents & libc::POLLIN) != 0)
}

/// RAII raw-mode guard for the idle poll window. Puts stdin into raw mode
/// (ICANON/ECHO off) so `poll(2)` wakes on the FIRST keystroke rather than only
/// on Enter, and restores the saved cooked termios on drop. `TCSANOW` applies
/// the switch WITHOUT flushing the input queue, so a keystroke already typed
/// survives the transition and is read by rustyline on handoff.
#[cfg(unix)]
struct RawModeGuard {
    orig: libc::termios,
}

#[cfg(unix)]
impl RawModeGuard {
    fn enter() -> Option<Self> {
        unsafe {
            let mut orig: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(libc::STDIN_FILENO, &mut orig) != 0 {
                return None;
            }
            let mut raw = orig;
            libc::cfmakeraw(&mut raw);
            if libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &raw) != 0 {
                return None;
            }
            Some(RawModeGuard { orig })
        }
    }

    /// Restore the saved cooked termios immediately (no input flush). Idempotent.
    fn restore(&self) {
        unsafe {
            libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &self.orig);
        }
    }
}

#[cfg(unix)]
impl Drop for RawModeGuard {
    fn drop(&mut self) {
        self.restore();
    }
}

/// Erase the idle prompt we printed, returning the cursor to column 0 and
/// clearing to end-of-line, so rustyline's own prompt reprint (on handoff) or
/// the auto-resume notice (on wake) starts from a clean line.
#[cfg(unix)]
fn erase_prompt_line() {
    use std::io::Write;
    let mut out = std::io::stdout();
    let _ = write!(out, "\r\x1b[K");
    let _ = out.flush();
}

// ---------------------------------------------------------------------------
// Hands-free auto-resume nudge (follow-up to the "interrupting an idle read"
// Hands-free auto-resume nudge (follow-up to the "interrupting an idle read"
// limitation flagged in repl.rs).
//
// When the last fanned-out coordinator of a session finishes, the presenter
// thread ARMS a coalesced resume (session::ResumeState) and the main loop
// drains it — but only on its *next* pass through the prompt. If the loop is
// already parked inside a blocking `LineEditor::read_line`, nothing fires until
// the human touches a key, so the "resuming to synthesize their results…" notice
// just sits there and the promised auto-resume never runs hands-free.
//
// rustyline 18 exposes no way to make an in-flight `readline` return
// programmatically (its `select()` loop wakes for an ExternalPrint and then
// re-blocks; only a real key ends the read). The pragmatic, terminal-safe way to
// end that blocking read from another thread is to push a newline into the
// controlling terminal's own input queue via the `TIOCSTI` ioctl: `read_line`
// then returns an empty line, the loop `continue`s, and the very next pass hits
// `take_resume_tick` and runs the continuation — no keypress required.
//
// This is best-effort by design. Modern kernels gate `TIOCSTI` behind
// `dev.tty.legacy_tiocsti` (Linux ≥6.2 defaults it OFF, CVE-2017-5226 mitigation),
// and stdin may not be a tty at all (pipes, CI). In every one of those cases the
// ioctl fails, we return `false`, and behaviour falls back EXACTLY to today's —
// the resume drains on the user's next Enter. So this only ever helps; it can
// never regress. The durable, kernel-independent fix is the reedline migration
// (S5.2/S5.5), whose read loop can be woken by an external event without this
// injection hack.
#[cfg(unix)]
pub fn nudge_terminal_return() -> bool {
    // Only meaningful when stdin is an interactive terminal; a pipe/file has no
    // input queue to inject into and the ioctl would just EINVAL/ENOTTY.
    if unsafe { libc::isatty(libc::STDIN_FILENO) } != 1 {
        return false;
    }
    let newline: libc::c_char = b'\n' as libc::c_char;
    // TIOCSTI takes a *pointer to a single byte* to enqueue as terminal input.
    let rc = unsafe { libc::ioctl(libc::STDIN_FILENO, libc::TIOCSTI, &newline) };
    rc == 0
}

/// Non-unix stub: no terminal-input injection primitive, so the auto-resume
/// always waits for the next keypress (unchanged pre-existing behaviour).
#[cfg(not(unix))]
pub fn nudge_terminal_return() -> bool {
    false
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
        // Voice (third flag) takes top priority: a deliberate dictation
        // request beats every other interrupt-routed key.
        assert!(
            matches!(interrupt_outcome(false, false, true), ReadOutcome::Voice),
            "a drained voice flag must surface as Voice"
        );
        // Voice wins even when ShiftTab or CtrlO were also co-raised.
        assert!(
            matches!(interrupt_outcome(true, false, true), ReadOutcome::Voice),
            "Voice wins over a co-raised Shift-Tab"
        );
        assert!(
            matches!(interrupt_outcome(false, true, true), ReadOutcome::Voice),
            "Voice wins over a co-raised Ctrl-O"
        );
        // Shift-Tab (second priority) wins over Ctrl-O when voice is clear.
        assert!(
            matches!(interrupt_outcome(true, false, false), ReadOutcome::ShiftTab),
            "a drained Shift-Tab flag must surface as ShiftTab"
        );
        // Shift-Tab takes precedence even if Ctrl-O was also somehow raised.
        assert!(
            matches!(interrupt_outcome(true, true, false), ReadOutcome::ShiftTab),
            "Shift-Tab wins over a co-raised Ctrl-O"
        );
        // Ctrl-O alone is the raw-output toggle.
        assert!(
            matches!(interrupt_outcome(false, true, false), ReadOutcome::CtrlO),
            "a drained Ctrl-O toggle must surface as CtrlO"
        );
        // Neither flag is a plain Ctrl-C clear-line.
        assert!(
            matches!(interrupt_outcome(false, false, false), ReadOutcome::Interrupted),
            "a clear triple is a plain Ctrl-C clear-line"
        );
    }

    /// The voice flag (like raw_toggle and shift_tab) is a read-and-clear swap:
    /// a Ctrl-G raise is seen exactly once, and the very next `Interrupted`
    /// is therefore a plain Ctrl-C / Shift-Tab / Ctrl-O as appropriate. This pins
    /// the drain contract that prevents a stale voice flag from eating a later
    /// unrelated interrupt.
    #[test]
    fn voice_flag_drains_exactly_once() {
        let flag = Arc::new(AtomicBool::new(false));
        // Simulate CtrlGVoice handler raising it.
        flag.store(true, Ordering::SeqCst);
        assert!(
            flag.swap(false, Ordering::SeqCst),
            "first drain sees the raised voice flag"
        );
        assert!(
            !flag.swap(false, Ordering::SeqCst),
            "second drain is clear (voice already consumed)"
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

    /// The hands-free auto-resume nudge degrades safely: under the test harness
    /// stdin is not an interactive tty (it's a pipe/devnull), so the isatty
    /// guard must short-circuit and return `false` WITHOUT ever issuing the
    /// TIOCSTI ioctl. This pins the zero-regression contract — when the primitive
    /// isn't available the caller falls back to resume-on-next-Enter — and proves
    /// the guard is reached before any ioctl side-effect.
    #[test]
    fn nudge_terminal_return_is_false_without_a_tty() {
        // cargo test runs with stdin detached from a terminal, so this is the
        // exact "no tty" branch the guard protects.
        assert!(
            !nudge_terminal_return(),
            "nudge must no-op (return false) when stdin isn't an interactive tty"
        );
    }

    /// The kernel-independent hands-free wake contract: `arm_resume_wake` raises
    /// a flag that `take_resume_wake` reads-and-clears EXACTLY once, and
    /// `set_background_pending` round-trips through `background_pending`. This is
    /// the seam the idle poll read (`poll_until_wake_or_key`) consults to return
    /// an empty line the instant the presenter arms a resume — so a parked
    /// `read_line` drains the auto-resume with no keypress, on any kernel (no
    /// TIOCSTI dependency). A stale wake must never survive its single drain.
    #[test]
    fn resume_wake_arms_and_drains_exactly_once() {
        // Clean slate — statics are process-global.
        let _ = take_resume_wake();

        // Not armed → drain is false.
        assert!(
            !take_resume_wake(),
            "a cleared wake must read false"
        );

        // Arm → first drain sees it, second drain is already clear.
        arm_resume_wake();
        assert!(
            take_resume_wake(),
            "the armed wake must surface on the first drain"
        );
        assert!(
            !take_resume_wake(),
            "the wake must not survive its single drain (no stale re-fire)"
        );

        // Background-pending gate round-trips.
        set_background_pending(true);
        assert!(
            background_pending(),
            "set_background_pending(true) must gate the poll path on"
        );
        set_background_pending(false);
        assert!(
            !background_pending(),
            "set_background_pending(false) must revert to the plain blocking read"
        );
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
