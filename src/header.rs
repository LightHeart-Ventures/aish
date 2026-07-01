//! Anchored top-of-terminal header.
//!
//! The REPL shows a two-row header pinned to the very top of the terminal:
//!
//! ```text
//! row 1  aish v0.22.1 — AI-native shell · claude (sonnet)        2026-07-01 21:15
//! row 2  ────────────────────────────────────────────────────────────────────────
//! ```
//!
//! Row 1 is the [`crate::style::statusline`] rendered in **bright white**; row 2
//! is a solid bright-white rule. Both are held fixed via a DECSTBM scroll region
//! (`ESC [ top;bottom r`): the header occupies the top [`HEADER_ROWS`] lines and
//! everything the shell prints — command output, the prompt, background-job
//! notices — scrolls in the region *below* it. The prompt therefore sits at the
//! bottom of the scrolling output (never itself anchored), while the statusline
//! stays put at the top.
//!
//! Off a tty (piped / redirected stdout) this module is entirely inert — no
//! escape sequences leak into a file or a downstream program.

use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

/// Rows reserved at the top of the terminal for the anchored header
/// (row 1 = statusline, row 2 = solid rule).
pub const HEADER_ROWS: usize = 2;

/// Whether an anchored header is currently installed (scroll region set). Lets
/// `clear_screen` know it must re-home the cursor into the body and repaint the
/// header rather than leaving text stomping over rows 1-2.
static ACTIVE: AtomicBool = AtomicBool::new(false);

/// The last statusline string we painted, so a screen-clear (Shift-Tab worker
/// cycle, etc.) can restore the header without the caller threading the
/// version/model through.
static LAST_STATUSLINE: Mutex<String> = Mutex::new(String::new());

/// True once an anchored header has been installed for this session.
pub fn active() -> bool {
    ACTIVE.load(Ordering::Relaxed)
}

/// Query the terminal's `(rows, cols)` via TIOCGWINSZ on stdout. `None` off a
/// tty or when the ioctl reports a zero-sized window.
fn term_size() -> Option<(usize, usize)> {
    // SAFETY: isatty + a read-only TIOCGWINSZ ioctl on stdout (fd 1).
    unsafe {
        if libc::isatty(1) != 1 {
            return None;
        }
        let mut ws: libc::winsize = std::mem::zeroed();
        if libc::ioctl(1, libc::TIOCGWINSZ, &mut ws) == 0 && ws.ws_row > 0 && ws.ws_col > 0 {
            return Some((ws.ws_row as usize, ws.ws_col as usize));
        }
    }
    None
}

fn flush(bytes: &str) {
    let mut so = std::io::stdout();
    let _ = so.write_all(bytes.as_bytes());
    let _ = so.flush();
}

/// A terminal too short to carve out a header + at least one body row. We refuse
/// to install a scroll region in that case (it would leave no usable space).
fn too_short(rows: usize) -> bool {
    rows <= HEADER_ROWS + 1
}

/// Install the anchored header: reserve the top [`HEADER_ROWS`] rows via a
/// DECSTBM scroll region, drop the cursor into the first body row, and paint the
/// header. No-op off a tty or on a terminal too short to reserve the header.
pub fn install(statusline: &str, color_on: bool) {
    let Some((rows, _cols)) = term_size() else {
        return;
    };
    if too_short(rows) {
        return;
    }
    // Scroll region: rows HEADER_ROWS+1 .. rows (1-based, inclusive). Then home
    // the cursor to the top of the body so the first output lands below the rule.
    let setup = format!(
        "\x1b[{};{}r\x1b[{};1H",
        HEADER_ROWS + 1,
        rows,
        HEADER_ROWS + 1
    );
    flush(&setup);
    ACTIVE.store(true, Ordering::Relaxed);
    repaint(statusline, color_on);
}

/// Repaint the two header rows without disturbing the logical cursor. Also
/// (re)asserts the scroll region every call so a terminal *resize* between
/// prompts is picked up (the body's bottom row tracks the new height). No-op off
/// a tty / on a too-short terminal.
pub fn repaint(statusline: &str, color_on: bool) {
    let Some((rows, cols)) = term_size() else {
        return;
    };
    if too_short(rows) {
        return;
    }
    // Cache for restore-after-clear.
    if let Ok(mut last) = LAST_STATUSLINE.lock() {
        *last = statusline.to_string();
    }
    let rule_body = "\u{2500}".repeat(cols); // U+2500 BOX DRAWINGS LIGHT HORIZONTAL
    let rule = if color_on {
        format!("\x1b[1;37m{rule_body}\x1b[0m") // bright white
    } else {
        rule_body
    };
    let mut out = String::new();
    // (Re)assert the scroll region — cheap and makes resize self-healing.
    out.push_str(&format!("\x1b[{};{}r", HEADER_ROWS + 1, rows));
    out.push_str("\x1b7"); // DECSC: save cursor + attrs
    out.push_str("\x1b[1;1H\x1b[2K"); // row 1, clear to EOL
    out.push_str(statusline); // bright-white statusline (already colored)
    out.push_str("\x1b[2;1H\x1b[2K"); // row 2, clear to EOL
    out.push_str(&rule);
    out.push_str("\x1b8"); // DECRC: restore cursor + attrs
    ACTIVE.store(true, Ordering::Relaxed);
    flush(&out);
}

/// Called by `clear_screen` after it emits `ESC[2J ESC[H`. When a header is
/// active the clear wiped rows 1-2 and homed the cursor into the header region,
/// so re-home into the body and repaint the header from the cached statusline.
/// No-op when no header is installed.
pub fn restore_after_clear() {
    if !active() {
        return;
    }
    let Some((rows, _cols)) = term_size() else {
        return;
    };
    if too_short(rows) {
        return;
    }
    let setup = format!(
        "\x1b[{};{}r\x1b[{};1H",
        HEADER_ROWS + 1,
        rows,
        HEADER_ROWS + 1
    );
    flush(&setup);
    let last = LAST_STATUSLINE.lock().map(|s| s.clone()).unwrap_or_default();
    repaint(&last, crate::style::colors_enabled());
}

/// Tear the header down: reset the scroll region to the full screen so the
/// user's terminal isn't left with a stuck two-row header after aish exits.
/// Idempotent; no-op off a tty.
pub fn teardown() {
    if unsafe { libc::isatty(1) } != 1 {
        return;
    }
    if !active() {
        return;
    }
    // ESC[r resets the scroll region to the whole screen; move to a fresh line.
    flush("\x1b[r\r\n");
    ACTIVE.store(false, Ordering::Relaxed);
}
