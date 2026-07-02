//! Mid-turn key watcher — makes Shift-Tab (worker-cycle) reachable *while a model
//! turn is running*, i.e. while the agent is thinking or mid tool-call, not only
//! at the idle prompt.
//!
//! ## Why this exists
//! At the idle prompt, rustyline reads keys and surfaces Shift-Tab as
//! [`crate::editor::ReadOutcome::ShiftTab`]. But once a turn starts, rustyline is
//! idle and the tty sits in cooked (canonical) mode: keystrokes are line-buffered
//! and invisible to us until the user hits Enter. So a Shift-Tab pressed while the
//! model streams goes nowhere. Operators wanted to flip their *view* onto a
//! background coordinator without first having to interrupt or wait out the turn.
//!
//! ## How it works
//! For the duration of a turn we put stdin into **cbreak** (ICANON + ECHO off,
//! ISIG left on so Ctrl-C still raises SIGINT) and run a small std-thread reader
//! that `poll()`s fd 0 and scans the byte stream for the CSI `Z` sequence
//! (`ESC [ Z`) that terminals emit for Shift-Tab / back-tab. On a hit it pushes a
//! [`TurnKey::ShiftTab`] onto a tokio channel the REPL's `select!` awaits alongside
//! the turn future and Ctrl-C. An RAII [`TurnKeyWatch`] guard restores cooked mode
//! (and joins the reader) on drop, so the tty is always handed back clean.
//!
//! ## Coordinating with the rest of the turn
//! Two other things legitimately want stdin during a turn, and the reader must get
//! out of their way or it would steal their bytes (or, worse, get SIGTTIN):
//!
//! * **Interactive confirm prompts** (`confirm_tty`'s `y/N/a/d` read). Those need
//!   cooked mode so the answer echoes and line-edits. [`pause_for_prompt`] returns
//!   an RAII pause that flips the tty back to cooked and parks the reader for the
//!   life of the prompt, then restores cbreak on drop.
//! * **TTY hand-off tools** (`run_interactive` → vim/top/ssh). Those put a child
//!   pgrp in the terminal's foreground. The reader detects it is no longer the
//!   foreground pgrp (`tcgetpgrp(0) != getpgrp()`), hands cooked mode back so the
//!   child inherits a sane tty, and parks until aish is foreground again. This is
//!   automatic — no hook in the tool path required — and also sidesteps the
//!   background-read SIGTTIN hazard.
//!
//! ## Non-TTY
//! When stdin is not a terminal (pipe/script), [`TurnKeyWatch::install`] is a
//! no-op guard whose `recv()` future is `Pending` forever, so the `select!` branch
//! simply never fires and nothing about the turn changes.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::thread::JoinHandle;
use std::time::Duration;
use tokio::sync::mpsc;

/// A callback the reader thread invokes DIRECTLY on a mid-turn Shift-Tab, so the
/// worker-cycle happens even when the async turn task is blocked and can't drain
/// the channel. It must only touch `Send + Sync` shared state (the attach-cursor
/// `Arc<Mutex>` handles) — it runs on the keywatch reader thread.
pub type ShiftTabFn = std::sync::Arc<dyn Fn() + Send + Sync>;

/// A key event surfaced by the mid-turn watcher. An enum (rather than a bare
/// unit) so future mid-turn keys can be added without reshaping the channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnKey {
    /// Shift-Tab / back-tab (CSI `Z`) pressed mid-turn — cycle the attach cursor.
    ShiftTab,
}

/// Process-global coordination point shared between the reader thread (which
/// lives on its own OS thread) and the main-thread callers (`confirm_tty`
/// pausing for a prompt; the guard installing/tearing down). It carries the
/// atomics the reader consults plus the saved cooked termios to restore.
struct Gate {
    /// A watcher is installed for the current turn (false ⇒ no-op everywhere).
    installed: AtomicBool,
    /// The reader must park (a confirm prompt owns stdin in cooked mode).
    suspend: AtomicBool,
    /// Tell the reader thread to exit its loop.
    stop: AtomicBool,
    /// The original cooked (canonical) termios to restore for prompts / teardown.
    cooked: Mutex<Option<libc::termios>>,
}

static GATE: OnceLock<Gate> = OnceLock::new();

fn gate() -> &'static Gate {
    GATE.get_or_init(|| Gate {
        installed: AtomicBool::new(false),
        suspend: AtomicBool::new(false),
        stop: AtomicBool::new(false),
        cooked: Mutex::new(None),
    })
}

fn stdin_is_tty() -> bool {
    // SAFETY: plain isatty query on fd 0.
    unsafe { libc::isatty(0) == 1 }
}

/// Read the current termios of fd 0, or `None` if it can't be queried.
fn get_termios() -> Option<libc::termios> {
    // SAFETY: tcgetattr into a zeroed termios; we check the return code.
    unsafe {
        let mut t: libc::termios = std::mem::zeroed();
        if libc::tcgetattr(0, &mut t) == 0 {
            Some(t)
        } else {
            None
        }
    }
}

fn set_termios(t: &libc::termios) {
    // SAFETY: tcsetattr on fd 0 with a valid termios.
    unsafe {
        libc::tcsetattr(0, libc::TCSANOW, t);
    }
}

/// Derive cbreak attrs from cooked: ICANON + ECHO off (unbuffered, silent) while
/// keeping ISIG on so Ctrl-C still generates SIGINT. VMIN=1/VTIME=0 so a `read`
/// after a readable `poll` returns promptly.
fn cbreak_from(cooked: &libc::termios) -> libc::termios {
    let mut raw = *cooked;
    raw.c_lflag &= !(libc::ICANON | libc::ECHO);
    raw.c_cc[libc::VMIN] = 1;
    raw.c_cc[libc::VTIME] = 0;
    raw
}

fn apply_cbreak() {
    if let Some(c) = *gate().cooked.lock().unwrap() {
        set_termios(&cbreak_from(&c));
    }
}

fn apply_cooked() {
    if let Some(c) = *gate().cooked.lock().unwrap() {
        set_termios(&c);
    }
}

/// Are we (aish's pgrp) the terminal's foreground process group? False while a
/// TTY-hand-off child (`run_interactive`) owns the terminal.
fn we_are_foreground() -> bool {
    // SAFETY: plain pgrp queries; tcgetpgrp<0 (no controlling tty) ⇒ treat as
    // foreground so a detached-tty edge case doesn't wedge the reader parked.
    unsafe {
        let fg = libc::tcgetpgrp(0);
        fg < 0 || fg == libc::getpgrp()
    }
}

/// Feed `bytes` through the CSI parser starting from `state` (0 = ground, 1 =
/// saw ESC, 2 = saw ESC '['). Returns the new carry-over state and how many
/// Shift-Tab (`ESC [ Z`) sequences completed in this chunk. Split out as a pure
/// fn so the byte-fragmentation handling is unit-testable without a real tty.
fn scan_csi_z(mut state: u8, bytes: &[u8]) -> (u8, usize) {
    let mut hits = 0usize;
    for &b in bytes {
        state = match state {
            0 => {
                if b == 0x1b {
                    1
                } else {
                    0
                }
            }
            1 => {
                if b == b'[' {
                    2
                } else if b == 0x1b {
                    1
                } else {
                    0
                }
            }
            _ => {
                if b == b'Z' {
                    hits += 1;
                }
                if b == 0x1b {
                    1
                } else {
                    0
                }
            }
        };
    }
    (state, hits)
}

/// The reader loop, run on a dedicated std thread for the life of one turn.
/// Owns cbreak↔cooked flips for the foreground-pgrp handoff; never touches
/// termios while `suspend` is set (the confirm path owns it then).
fn reader_loop(tx: mpsc::UnboundedSender<TurnKey>, on_shift_tab: Option<ShiftTabFn>) {
    let g = gate();
    // We applied cbreak at install; track it so we only flip on transitions.
    let mut have_cbreak = true;
    // CSI parser state: 0 = ground, 1 = saw ESC, 2 = saw ESC '['.
    let mut state: u8 = 0;
    let mut buf = [0u8; 64];
    loop {
        if g.stop.load(Ordering::Acquire) {
            break;
        }
        // A confirm prompt owns stdin in cooked mode — stay fully out of the way.
        if g.suspend.load(Ordering::Acquire) {
            std::thread::sleep(Duration::from_millis(15));
            continue;
        }
        // A TTY-hand-off child (run_interactive) is foreground — give cooked mode
        // back so the child inherits a sane tty, and park until we're foreground.
        if !we_are_foreground() {
            if have_cbreak {
                apply_cooked();
                have_cbreak = false;
            }
            std::thread::sleep(Duration::from_millis(30));
            continue;
        } else if !have_cbreak {
            apply_cbreak();
            have_cbreak = true;
        }

        // Wait (interruptibly) for readable input.
        let mut pfd = libc::pollfd {
            fd: 0,
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: poll on a single fd with a 40ms timeout.
        let r = unsafe { libc::poll(&mut pfd, 1, 40) };
        if r <= 0 {
            continue; // timeout or (EINTR) error — re-check the flags and retry.
        }
        // Re-check the ownership flags after the wait: a confirm/child may have
        // grabbed stdin while we were blocked in poll.
        if g.suspend.load(Ordering::Acquire)
            || g.stop.load(Ordering::Acquire)
            || !we_are_foreground()
        {
            continue;
        }
        // SAFETY: read into a stack buffer; n bounded by buf.len().
        let n = unsafe { libc::read(0, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
        if n <= 0 {
            continue;
        }
        let (next_state, hits) = scan_csi_z(state, &buf[..n as usize]);
        state = next_state;
        for _ in 0..hits {
            // Shift-Tab. Act on it TWO ways so it lands regardless of whether the
            // hosting `select!` gets re-polled: (1) invoke the direct callback on
            // THIS thread — it only touches `Arc<Mutex>` attach-cursor handles, so
            // it cycles the view even while the async turn task is stuck in a
            // blocking / CPU-bound stretch that never yields to drain the channel;
            // (2) also push onto the channel for any `select!` consumer that still
            // wants the event. Best-effort send; a closed channel just drops it.
            if let Some(cb) = on_shift_tab.as_ref() {
                cb();
            }
            let _ = tx.send(TurnKey::ShiftTab);
        }
    }
}

/// RAII guard installed for the duration of one model turn. On construction it
/// flips stdin into cbreak and spawns the reader; on drop it stops the reader,
/// restores cooked mode, and joins the thread. Non-TTY ⇒ inert (`recv()` stays
/// Pending forever).
pub struct TurnKeyWatch {
    active: bool,
    rx: mpsc::UnboundedReceiver<TurnKey>,
    /// Kept alive so the channel never closes while inert — `recv()` then stays
    /// Pending rather than resolving `None` and busy-spinning the `select!`.
    _keepalive: mpsc::UnboundedSender<TurnKey>,
    handle: Option<JoinHandle<()>>,
}

impl TurnKeyWatch {
    /// Install the watcher for the current turn. A no-op inert guard when stdin
    /// isn't a tty, when termios can't be read, or when a watcher is somehow
    /// already installed (re-entrancy guard).
    ///
    /// `on_shift_tab` (when `Some`) is invoked DIRECTLY on the reader thread for
    /// each mid-turn Shift-Tab, so the worker-cycle fires even while the async
    /// turn task is stuck in a blocking / CPU-bound stretch that never yields to
    /// drain the channel `select!` awaits. The channel event is still emitted.
    pub fn install(on_shift_tab: Option<ShiftTabFn>) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        let g = gate();
        if !stdin_is_tty() || g.installed.load(Ordering::Acquire) {
            return TurnKeyWatch {
                active: false,
                rx,
                _keepalive: tx,
                handle: None,
            };
        }
        let Some(cooked) = get_termios() else {
            return TurnKeyWatch {
                active: false,
                rx,
                _keepalive: tx,
                handle: None,
            };
        };
        *g.cooked.lock().unwrap() = Some(cooked);
        g.suspend.store(false, Ordering::Release);
        g.stop.store(false, Ordering::Release);
        g.installed.store(true, Ordering::Release);
        set_termios(&cbreak_from(&cooked));
        let reader_tx = tx.clone();
        let handle = std::thread::Builder::new()
            .name("aish-keywatch".into())
            .spawn(move || reader_loop(reader_tx, on_shift_tab))
            .ok();
        TurnKeyWatch {
            active: handle.is_some(),
            rx,
            _keepalive: tx,
            handle,
        }
    }

    /// Await the next mid-turn key. Resolves only on a real key press; on an
    /// inert guard it is Pending forever (the channel is held open by
    /// `_keepalive`), so the hosting `select!` branch never fires.
    pub async fn recv(&mut self) -> Option<TurnKey> {
        self.rx.recv().await
    }
}

impl Drop for TurnKeyWatch {
    fn drop(&mut self) {
        if !self.active {
            return;
        }
        let g = gate();
        g.stop.store(true, Ordering::Release);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
        apply_cooked();
        g.installed.store(false, Ordering::Release);
    }
}

/// RAII pause returned by [`pause_for_prompt`]. While held, the mid-turn reader
/// is parked and the tty is in cooked mode so an interactive prompt echoes and
/// line-edits normally. On drop it restores cbreak and un-parks the reader.
pub struct PromptPause {
    /// True only when a watcher was actually installed & we flipped it — an
    /// inert pause (no active watcher) restores nothing.
    engaged: bool,
}

/// Pause the mid-turn watcher for a synchronous stdin prompt (e.g. the
/// `confirm_tty` `y/N` read). Returns an RAII guard: hold it across the prompt,
/// drop it after. A no-op when no watcher is installed.
pub fn pause_for_prompt() -> PromptPause {
    let g = gate();
    if !g.installed.load(Ordering::Acquire) {
        return PromptPause { engaged: false };
    }
    // Signal the reader to park, then wait past one poll interval so any in-flight
    // read completes and the reader observes `suspend` before we touch termios.
    g.suspend.store(true, Ordering::Release);
    std::thread::sleep(Duration::from_millis(60));
    apply_cooked();
    PromptPause { engaged: true }
}

impl Drop for PromptPause {
    fn drop(&mut self) {
        if !self.engaged {
            return;
        }
        let g = gate();
        // Restore the turn's watching state: cbreak back on, then un-park.
        apply_cbreak();
        g.suspend.store(false, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::scan_csi_z;

    // A complete `ESC [ Z` in one chunk from the ground state → one Shift-Tab,
    // parser returns to ground.
    #[test]
    fn single_shift_tab() {
        let (state, hits) = scan_csi_z(0, b"\x1b[Z");
        assert_eq!(hits, 1);
        assert_eq!(state, 0);
    }

    // Two back-to-back Shift-Tabs in one read.
    #[test]
    fn two_shift_tabs() {
        let (state, hits) = scan_csi_z(0, b"\x1b[Z\x1b[Z");
        assert_eq!(hits, 2);
        assert_eq!(state, 0);
    }

    // Fragmented across reads: ESC in the first chunk, `[Z` in the next. The
    // carry-over state must survive the split so the sequence still fires.
    #[test]
    fn fragmented_across_reads() {
        let (s1, h1) = scan_csi_z(0, b"\x1b");
        assert_eq!(h1, 0);
        assert_eq!(s1, 1);
        let (s2, h2) = scan_csi_z(s1, b"[");
        assert_eq!(h2, 0);
        assert_eq!(s2, 2);
        let (s3, h3) = scan_csi_z(s2, b"Z");
        assert_eq!(h3, 1);
        assert_eq!(s3, 0);
    }

    // A plain Tab (0x09) and ordinary text must NOT be mistaken for Shift-Tab.
    #[test]
    fn plain_tab_and_text_ignored() {
        let (_, hits) = scan_csi_z(0, b"\thello\tworld");
        assert_eq!(hits, 0);
    }

    // An arrow key (`ESC [ A`) shares the CSI prefix but must not match.
    #[test]
    fn arrow_key_not_matched() {
        let (state, hits) = scan_csi_z(0, b"\x1b[A");
        assert_eq!(hits, 0);
        assert_eq!(state, 0);
    }

    // ESC ESC [ Z — a stray ESC re-arms the parser rather than aborting the
    // following real sequence.
    #[test]
    fn double_esc_rearms() {
        let (state, hits) = scan_csi_z(0, b"\x1b\x1b[Z");
        assert_eq!(hits, 1);
        assert_eq!(state, 0);
    }

    // Shift-Tab embedded in surrounding noise is still counted once.
    #[test]
    fn embedded_in_noise() {
        let (_, hits) = scan_csi_z(0, b"abc\x1b[Zdef");
        assert_eq!(hits, 1);
    }
}
