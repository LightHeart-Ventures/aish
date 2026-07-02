//! Bottom-anchored statusline via a DECSTBM scroll region.
//!
//! The REPL pins a three-row footer to the very bottom of the terminal:
//!
//! ```text
//! rows 1..H-3   scrolling REPL area (command output, history, the prompt)
//! row  H-2      ────────────────────────────────────────────────  (solid rule)
//! row  H-1      ⇄ attached to w_YM7YyIHV (2/2 · Shift-Tab to cycle, :detach)  (status msg)
//! row  H        aish v0.23.0 · claude (sonnet)              2026-07-01 21:15   (statusline)
//! ```
//!
//! The footer is held fixed with a DECSTBM scroll region (`ESC[top;bottomr`):
//! the region covers rows `1..=H-3`, so everything the shell prints scrolls
//! *above* the footer while rows `H-2..=H` stay put. Each [`Terminal::draw_footer`]
//! re-asserts the region before painting, which makes a terminal *resize*
//! between prompts self-healing (the bottom margin tracks the new height) even
//! without catching SIGWINCH.
//!
//! Off a tty (piped / redirected stdout) the whole module is inert — no escape
//! sequences leak into a file or a downstream program. On a terminal too short
//! to carve out the footer plus a couple of body rows (height ≤ 4) we refuse to
//! install the region and the caller falls back to inline statusline printing.
//!
//! Cursor save/restore uses DECSC/DECRC (`ESC7`/`ESC8`) rather than the
//! `ESC[s`/`ESC[u` SCO variants, which some terminals treat as scroll-region
//! margins — DECSC/DECRC is the portable pair.

use std::io::Write;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// Rows reserved at the bottom for the footer: separator + status message +
/// statusline.
pub const FOOTER_ROWS: u16 = 3;

/// Minimum terminal height to render the footer. The footer needs 3 rows and we
/// insist on at least 2 scrolling rows above it, so height must be ≥ 5. At or
/// below 4 the caller falls back to inline printing.
pub const MIN_FOOTER_ROWS: u16 = 5;

/// Whether a scroll region is currently installed. Read by the panic hook (to
/// decide whether it must reset margins on unwind) and by [`restore_after_clear`].
static ACTIVE: AtomicBool = AtomicBool::new(false);

/// Whether the alternate screen buffer is currently active — a worker attach
/// view has taken over the screen via [`enter_alt_screen`]. Tracked so the
/// enter/leave transitions are idempotent and the panic hook can restore the
/// primary buffer on unwind.
static ALT_SCREEN: AtomicBool = AtomicBool::new(false);

/// Last footer content painted `(status_msg, statusline)`, so a screen-clear
/// (Shift-Tab worker cycle, etc.) can repaint the footer without the caller
/// threading the strings back through.
static LAST_FOOTER: Mutex<(String, String)> = Mutex::new((String::new(), String::new()));

// ---------------------------------------------------------------------------
// Heartbeat footer repaint (idle-timeout self-heal).
//
// A terminal *scroll* (mouse wheel, trackpad, PageUp) moves the viewport
// without sending aish any input, so the shell never learns the footer scrolled
// out of view — the classic "I scrolled and the footer disappeared" complaint.
// The fix is a low-frequency heartbeat: while the REPL is parked at the prompt
// (a blocking line read) and nothing has repainted the footer for
// `HEARTBEAT_IDLE`, a background thread repaints it from the cached content. The
// repaint is cursor-safe (DECSC/DECRC in `footer_seq` saves + restores the
// caller's cursor, so the in-progress input line is untouched) and only fires in
// the idle-at-prompt window, so it never races the engine's output writes.
// ---------------------------------------------------------------------------

/// Idle gap after which the heartbeat repaints the footer. Chosen at 3s: long
/// enough to be invisible during normal typing/output, short enough that a
/// scrolled-away footer snaps back almost immediately.
pub const HEARTBEAT_IDLE: Duration = Duration::from_secs(3);

/// True only while the REPL is blocked in a line read (idle at the prompt). The
/// heartbeat repaints ONLY in this window, so it can never interleave with the
/// engine's output writes on the main thread. Toggled by [`set_reading_line`].
static READING_LINE: AtomicBool = AtomicBool::new(false);

/// Millis since the process heartbeat epoch of the last footer paint (via
/// [`note_footer_activity`]). The heartbeat compares `now - this >= HEARTBEAT_IDLE`.
static LAST_FOOTER_ACTIVITY_MS: AtomicU64 = AtomicU64::new(0);

/// Ensures the heartbeat thread is spawned at most once per process.
static HEARTBEAT_SPAWNED: AtomicBool = AtomicBool::new(false);

/// Monotonic milliseconds since a fixed process epoch — cheap, thread-shared,
/// and immune to wall-clock jumps (unlike `SystemTime`).
fn heartbeat_now_ms() -> u64 {
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    EPOCH.get_or_init(Instant::now).elapsed().as_millis() as u64
}

/// Record that the footer was just painted, resetting the idle timer so the
/// heartbeat defers its next repaint by a full [`HEARTBEAT_IDLE`].
pub fn note_footer_activity() {
    LAST_FOOTER_ACTIVITY_MS.store(heartbeat_now_ms(), Ordering::Relaxed);
}

/// Mark whether the REPL is parked in a blocking line read. The editor calls
/// this `true` immediately before reading and `false` after. Entering a read
/// also refreshes the idle timer so the heartbeat waits a full interval before
/// its first repaint at a fresh prompt.
pub fn set_reading_line(reading: bool) {
    READING_LINE.store(reading, Ordering::Relaxed);
    if reading {
        note_footer_activity();
    }
}

/// Spawn the footer heartbeat thread (idempotent — only the first call spawns).
/// The thread wakes on a short cadence and repaints the cached footer whenever
/// the REPL has been idle at the prompt for [`HEARTBEAT_IDLE`], self-healing a
/// footer that scrolled out of view. No-op unless a footer region is installed;
/// safe to call once at REPL startup.
pub fn spawn_footer_heartbeat() {
    if HEARTBEAT_SPAWNED.swap(true, Ordering::Relaxed) {
        return;
    }
    std::thread::Builder::new()
        .name("aish-footer-heartbeat".into())
        .spawn(|| {
            // Poll well under HEARTBEAT_IDLE so the actual repaint lands within
            // ~half a second of the 3s idle mark.
            let tick = Duration::from_millis(500);
            let idle_ms = HEARTBEAT_IDLE.as_millis() as u64;
            loop {
                std::thread::sleep(tick);
                // Only when a footer region is live, we're parked at the prompt,
                // and no worker alt-screen view owns the terminal.
                if !ACTIVE.load(Ordering::Relaxed)
                    || !READING_LINE.load(Ordering::Relaxed)
                    || ALT_SCREEN.load(Ordering::Relaxed)
                {
                    continue;
                }
                let idle =
                    heartbeat_now_ms().saturating_sub(LAST_FOOTER_ACTIVITY_MS.load(Ordering::Relaxed));
                if idle >= idle_ms {
                    // Cursor-safe repaint (no body-home): DECSC/DECRC restores
                    // the input cursor exactly where the user left it.
                    paint_cached_footer(false);
                }
            }
        })
        .ok();
}

/// Repaint the footer from cached content. When `home_body` is true the cursor
/// is dropped into the last body row afterwards (post-clear / alt-screen use);
/// when false the cursor is left wherever `footer_seq`'s DECSC/DECRC restored it
/// — the cursor-safe form the idle heartbeat uses so it never disturbs an
/// in-progress input line. No-op when no region is installed or the terminal is
/// too short. Records footer activity so the heartbeat re-arms.
fn paint_cached_footer(home_body: bool) {
    if !ACTIVE.load(Ordering::Relaxed) {
        return;
    }
    let Some((rows, cols)) = term_size() else {
        return;
    };
    if rows < MIN_FOOTER_ROWS {
        return;
    }
    let (msg, bar) = LAST_FOOTER.lock().map(|l| l.clone()).unwrap_or_default();
    let utf8 = utf8_locale();
    let sep = separator_line(cols, utf8, crate::style::colors_enabled());
    // footer_seq re-asserts the scroll region internally (inside its DECSC/DECRC
    // save-restore).
    let mut buf = footer_seq(rows, cols, &sep, &msg, &bar);
    if home_body {
        // Override the restored cursor with an explicit home into the body so
        // the post-clear view grows up from the bottom.
        let body_bottom = rows.saturating_sub(FOOTER_ROWS).max(1);
        buf.push_str(&format!("\x1b[{body_bottom};1H"));
    }
    let mut out = std::io::stdout();
    let _ = write!(out, "{buf}");
    let _ = out.flush();
    note_footer_activity();
}

// ---------------------------------------------------------------------------
// Pure escape-sequence builders (unit-tested without a real terminal).
// ---------------------------------------------------------------------------

/// DECSTBM: set the scroll region to rows `1..=(rows - FOOTER_ROWS)`, reserving
/// the bottom [`FOOTER_ROWS`] rows for the footer.
pub fn scroll_region_seq(rows: u16) -> String {
    let bottom = rows.saturating_sub(FOOTER_ROWS).max(1);
    format!("\x1b[1;{bottom}r")
}

/// Reset the scroll region to the full screen (`DECSTBM` with no params).
pub const RESET_REGION: &str = "\x1b[r";

/// A solid horizontal rule `cols` wide. Uses `─` (U+2500) when `utf8`, else the
/// ASCII `-`. Wrapped in dim SGR when `color_on`.
pub fn separator_line(cols: u16, utf8: bool, color_on: bool) -> String {
    let ch = if utf8 { '─' } else { '-' };
    let body: String = std::iter::repeat(ch).take(cols.max(1) as usize).collect();
    if color_on {
        format!("\x1b[2m{body}\x1b[0m")
    } else {
        body
    }
}

/// Build the full footer paint: save cursor, position + clear + draw each of the
/// three footer rows, restore cursor. `separator`, `status_msg`, and `statusline`
/// are painted verbatim (already styled by the caller) after clipping each to
/// `cols` visible columns so nothing wraps and corrupts the region.
pub fn footer_seq(
    rows: u16,
    cols: u16,
    separator: &str,
    status_msg: &str,
    statusline: &str,
) -> String {
    let sep_row = rows.saturating_sub(2);
    let msg_row = rows.saturating_sub(1);
    let bar_row = rows;
    let max = cols as usize;
    let sep = clip_visible(separator, max);
    let msg = clip_visible(status_msg, max);
    let bar = clip_visible(statusline, max);
    let mut s = String::with_capacity(sep.len() + msg.len() + bar.len() + 48);
    s.push_str("\x1b7"); // DECSC — save cursor + attrs
    // Re-assert the scroll region INSIDE the save/restore. DECSTBM homes the
    // cursor to the top-left as a documented side effect, so it MUST run after
    // the DECSC save above — otherwise the DECRC below restores the homed
    // (top-left) position instead of the caller's real cursor, stranding the
    // next prompt at the top of the screen instead of two lines below the last
    // output. Re-asserting every paint also makes a resize between prompts
    // self-healing without depending on the SIGWINCH watcher.
    s.push_str(&scroll_region_seq(rows));
    s.push_str(&format!("\x1b[{sep_row};1H\x1b[2K{sep}"));
    s.push_str(&format!("\x1b[{msg_row};1H\x1b[2K{msg}"));
    s.push_str(&format!("\x1b[{bar_row};1H\x1b[2K{bar}"));
    s.push_str("\x1b8"); // DECRC — restore cursor + attrs
    s
}

/// Clip a possibly-ANSI-colored string to at most `max` visible columns without
/// splitting an escape sequence. Non-escape characters are measured by their
/// unicode display width; SGR/CSI escapes pass through with zero width. If any
/// escape was emitted and we truncated, a `RESET` is appended so color never
/// bleeds past the clip.
pub fn clip_visible(s: &str, max: usize) -> String {
    use unicode_width::UnicodeWidthChar;
    let mut out = String::with_capacity(s.len());
    let mut width = 0usize;
    let mut saw_escape = false;
    let mut truncated = false;
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            saw_escape = true;
            out.push(c);
            // Copy the rest of the escape sequence verbatim (zero width).
            if let Some(&n) = chars.peek() {
                if n == '[' {
                    // CSI: ESC [ ... final byte in 0x40..=0x7e
                    out.push(chars.next().unwrap());
                    while let Some(&p) = chars.peek() {
                        out.push(chars.next().unwrap());
                        if ('\x40'..='\x7e').contains(&p) {
                            break;
                        }
                    }
                } else {
                    // Two-char escape (e.g. ESC7 / ESC8 / ESC c) — take one more.
                    out.push(chars.next().unwrap());
                }
            }
            continue;
        }
        let w = UnicodeWidthChar::width(c).unwrap_or(0);
        if width + w > max {
            truncated = true;
            break;
        }
        width += w;
        out.push(c);
    }
    if truncated && saw_escape {
        out.push_str("\x1b[0m");
    }
    out
}

// ---------------------------------------------------------------------------
// Runtime terminal handle.
// ---------------------------------------------------------------------------

/// A handle to the interactive terminal that owns the bottom-anchored footer.
/// Constructed via [`Terminal::detect`] (returns `None` off a tty). The scroll
/// region is torn down on `Drop` so aish never leaves a stuck region behind.
pub struct Terminal {
    /// Terminal height in rows (1-based count).
    pub rows: u16,
    /// Terminal width in columns.
    pub cols: u16,
    /// Whether a scroll region is currently installed.
    pub active: bool,
    /// Whether the locale advertises UTF-8 (drives `─` vs `-`).
    pub utf8: bool,
}

impl Terminal {
    /// Detect the controlling terminal's size. `None` off a tty or when the
    /// window reports a zero size.
    pub fn detect() -> Option<Terminal> {
        let (rows, cols) = term_size()?;
        Some(Terminal {
            rows,
            cols,
            active: false,
            utf8: utf8_locale(),
        })
    }

    /// True when the terminal is tall enough to host the footer.
    pub fn footer_enabled(&self) -> bool {
        self.rows >= MIN_FOOTER_ROWS
    }

    /// Install the DECSTBM scroll region and drop the cursor into the body (the
    /// last scrolling row) so the next output lands above the footer. No-op when
    /// the terminal is too short.
    pub fn init_scroll_region(&mut self) {
        if !self.footer_enabled() {
            return;
        }
        let body_bottom = self.rows.saturating_sub(FOOTER_ROWS).max(1);
        let mut out = std::io::stdout();
        let _ = write!(out, "{}\x1b[{body_bottom};1H", scroll_region_seq(self.rows));
        let _ = out.flush();
        self.active = true;
        ACTIVE.store(true, Ordering::Relaxed);
    }

    /// Reset the scroll region to the whole screen and erase the footer rows so
    /// the shell that inherits the terminal starts clean. Idempotent.
    pub fn reset_scroll_region(&mut self) {
        if !self.active {
            return;
        }
        let sep_row = self.rows.saturating_sub(2).max(1);
        let mut out = std::io::stdout();
        // Reset region first, then clear from the footer's top row to end of
        // screen so no stale statusline is left behind.
        let _ = write!(out, "{RESET_REGION}\x1b[{sep_row};1H\x1b[J");
        let _ = out.flush();
        self.active = false;
        ACTIVE.store(false, Ordering::Relaxed);
    }

    /// Re-assert the scroll region (cheap; makes resize self-healing) and repaint
    /// the three footer rows without disturbing the logical cursor. The strings
    /// are cached so [`restore_after_clear`] can repaint after a screen wipe.
    pub fn draw_footer(&mut self, status_msg: &str, statusline: &str) {
        if !self.active {
            return;
        }
        if let Ok(mut last) = LAST_FOOTER.lock() {
            *last = (status_msg.to_string(), statusline.to_string());
        }
        let sep = separator_line(self.cols, self.utf8, crate::style::colors_enabled());
        let mut buf = String::new();
        // footer_seq re-asserts the scroll region internally, INSIDE its
        // DECSC/DECRC save-restore, so the DECSTBM cursor-home side effect never
        // leaks out and strands the next prompt at the top of the screen.
        buf.push_str(&footer_seq(self.rows, self.cols, &sep, status_msg, statusline));
        let mut out = std::io::stdout();
        let _ = write!(out, "{buf}");
        let _ = out.flush();
        // Reset the heartbeat idle timer — a fresh paint just landed, so the
        // idle repaint defers a full interval.
        note_footer_activity();
    }

    /// Re-query the terminal size (after a SIGWINCH) and re-establish or tear
    /// down the region as the new height dictates. Returns `true` when the size
    /// changed.
    pub fn handle_resize(&mut self) -> bool {
        let Some((rows, cols)) = term_size() else {
            return false;
        };
        let changed = rows != self.rows || cols != self.cols;
        self.rows = rows;
        self.cols = cols;
        if self.footer_enabled() {
            self.init_scroll_region();
            let (msg, bar) = LAST_FOOTER
                .lock()
                .map(|l| l.clone())
                .unwrap_or_default();
            if !bar.is_empty() || !msg.is_empty() {
                self.draw_footer(&msg, &bar);
            }
        } else if self.active {
            self.reset_scroll_region();
        }
        changed
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        self.reset_scroll_region();
    }
}

/// After `clear_screen` emits `ESC[2J ESC[H` the footer rows were wiped and the
/// cursor homed to row 1 (top of the region). Repaint the footer from the cached
/// content. No-op when no region is installed.
pub fn restore_after_clear() {
    // Home the cursor into the body after the repaint so the post-clear view
    // grows up from the bottom (the idle heartbeat uses the cursor-safe form).
    paint_cached_footer(true);
}

/// Suspend the bottom-anchored footer scroll region for the duration of a
/// foreground child that inherits the terminal (`sudo`, `vim`, `less`, …).
/// Resets DECSTBM to the full screen and erases the three footer rows so the
/// child sees an ordinary terminal with no reserved bottom rows — otherwise a
/// program that writes near the bottom of the screen (most visibly sudo's
/// echo-off password prompt) collides with the footer zone and its output is
/// intermittently hidden until an extra keystroke forces a repaint. The current
/// cursor is preserved: DECSTBM reset homes the cursor to the top-left as a
/// documented side effect, so the reset is wrapped in DECSC/DECRC and the
/// child's first output continues exactly where the command line left off.
/// Returns `true` when a region was actually torn down, so the caller knows to
/// pair it with [`resume_footer_region`] on the way out. No-op (returns `false`)
/// when no footer region is installed.
pub fn suspend_footer_region() -> bool {
    if !ACTIVE.load(Ordering::Relaxed) {
        return false;
    }
    let Some((rows, _cols)) = term_size() else {
        return false;
    };
    let sep_row = rows.saturating_sub(2).max(1);
    // DECSC → reset region to full screen → clear the footer rows → DECRC, so
    // the cursor stays exactly where the command line left it.
    let seq = format!("\x1b7{RESET_REGION}\x1b[{sep_row};1H\x1b[J\x1b8");
    let mut out = std::io::stdout();
    let _ = write!(out, "{seq}");
    let _ = out.flush();
    ACTIVE.store(false, Ordering::Relaxed);
    true
}

/// Re-establish the footer scroll region and repaint the cached footer after a
/// foreground child that inherited the full terminal exits. Pairs with
/// [`suspend_footer_region`]. Homes the cursor into the last body row so the
/// next prompt grows up from just above the footer (mirrors
/// [`Terminal::init_scroll_region`]). No-op when the terminal is now too short
/// to host the footer (e.g. it was resized smaller while the child ran).
pub fn resume_footer_region() {
    let Some((rows, _cols)) = term_size() else {
        return;
    };
    if rows < MIN_FOOTER_ROWS {
        return;
    }
    // Re-assert DECSTBM (homes the cursor to top-left as a side effect), then
    // drop the cursor into the last body row so subsequent output stays above
    // the footer instead of stranding at the top of the screen.
    let body_bottom = rows.saturating_sub(FOOTER_ROWS).max(1);
    let seq = format!("{}\x1b[{body_bottom};1H", scroll_region_seq(rows));
    let mut out = std::io::stdout();
    let _ = write!(out, "{seq}");
    let _ = out.flush();
    ACTIVE.store(true, Ordering::Relaxed);
    // Repaint the pinned footer rows from cache (cursor-safe: footer_seq wraps
    // its paint in DECSC/DECRC).
    paint_cached_footer(false);
}


/// Whether a bottom-anchored footer scroll region is currently installed. Lets
/// the REPL decide, after a screen clear, whether the cursor was already homed
/// into the body by [`restore_after_clear`] (footer mode) or still needs to be
/// moved to the bottom row for a bottom-anchored view (inline / no-footer mode).
pub fn footer_active() -> bool {
    ACTIVE.load(Ordering::Relaxed)
}

/// Whether the alternate screen buffer is currently active (a worker attach view
/// owns the screen). See [`enter_alt_screen`].
pub fn on_alt_screen() -> bool {
    ALT_SCREEN.load(Ordering::Relaxed)
}

/// Re-anchor the cursor into the bottom of the body after a screen wipe / buffer
/// switch: footer mode re-asserts the region + repaints the footer (which homes
/// into the bottom body row); inline mode homes to the last row. Shared by the
/// alt-screen enter/leave so a worker view grows up from the bottom exactly like
/// a plain clear.
fn anchor_bottom_after_wipe() {
    if ACTIVE.load(Ordering::Relaxed) {
        restore_after_clear();
    } else if let Some((rows, _)) = term_size() {
        let mut out = std::io::stdout();
        let _ = write!(out, "{}", bottom_home_seq(rows));
        let _ = out.flush();
    }
}

/// Enter the alternate screen buffer so a worker attach view can take over the
/// screen WITHOUT destroying the interactive scrollback. `ESC[?1049h` saves the
/// primary buffer (all prior interactive output + cursor); [`leave_alt_screen`]
/// restores it verbatim, so detaching returns the operator to exactly the output
/// that was on screen when they attached. Idempotent — a no-op when already on
/// the alt screen or off a tty.
pub fn enter_alt_screen() {
    // SAFETY: plain isatty query.
    if unsafe { libc::isatty(1) } != 1 {
        return;
    }
    if ALT_SCREEN.swap(true, Ordering::Relaxed) {
        return;
    }
    let mut out = std::io::stdout();
    let _ = write!(out, "\x1b[?1049h");
    let _ = out.flush();
    // Fresh alt buffer: re-assert the footer region + anchor to the bottom so
    // the worker view trails the last row like a clear does.
    anchor_bottom_after_wipe();
}

/// Leave the alternate screen buffer, restoring the primary buffer (the
/// interactive scrollback + cursor as they were before [`enter_alt_screen`]).
/// Idempotent — a no-op when not on the alt screen or off a tty.
pub fn leave_alt_screen() {
    // SAFETY: plain isatty query.
    if unsafe { libc::isatty(1) } != 1 {
        return;
    }
    if !ALT_SCREEN.swap(false, Ordering::Relaxed) {
        return;
    }
    let mut out = std::io::stdout();
    let _ = write!(out, "\x1b[?1049l");
    let _ = out.flush();
    // Restored primary buffer: some terminals drop the DECSTBM margins across a
    // 1049 switch, so re-assert the region + repaint the footer and home into
    // the body — the detached line + next prompt then trail the restored output.
    anchor_bottom_after_wipe();
}

/// The terminal's row count via `TIOCGWINSZ`, or `None` off a tty. Public so the
/// REPL can home the cursor to the bottom row when no footer region is installed.
pub fn screen_rows() -> Option<u16> {
    term_size().map(|(rows, _)| rows)
}

/// Cursor-home sequence to the bottom row, column 1 (`ESC[<rows>;1H`). Used by
/// the inline-mode attach clear to anchor the view to the bottom of the screen
/// (mirroring footer mode, where [`restore_after_clear`] homes to the bottom
/// body row) so the backfill + redrawn prompt trail the last output instead of
/// stranding at the top. Clamped to row 1 for degenerate zero heights.
pub fn bottom_home_seq(rows: u16) -> String {
    format!("\x1b[{};1H", rows.max(1))
}

/// Install a panic hook that resets the scroll region on unwind, so a crash
/// mid-session doesn't leave the user's terminal with a stuck footer region.
/// Chains the previous hook.
pub fn install_panic_hook() {
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        // Restore the primary screen buffer first (if a worker attach view had
        // taken over the alt buffer), so the crash + backtrace land on the
        // operator's real scrollback rather than a stranded alt screen.
        if ALT_SCREEN.swap(false, Ordering::Relaxed) {
            let mut out = std::io::stdout();
            let _ = write!(out, "\x1b[?1049l");
            let _ = out.flush();
        }
        if ACTIVE.load(Ordering::Relaxed) {
            let mut out = std::io::stdout();
            let _ = write!(out, "{RESET_REGION}\r\n");
            let _ = out.flush();
            ACTIVE.store(false, Ordering::Relaxed);
        }
        prev(info);
    }));
}

/// Query the terminal's `(rows, cols)` via TIOCGWINSZ on stdout. `None` off a
/// tty or when the ioctl reports a zero-sized window.
fn term_size() -> Option<(u16, u16)> {
    // SAFETY: isatty + a read-only TIOCGWINSZ ioctl on stdout (fd 1).
    unsafe {
        if libc::isatty(1) != 1 {
            return None;
        }
        let mut ws: libc::winsize = std::mem::zeroed();
        if libc::ioctl(1, libc::TIOCGWINSZ, &mut ws) == 0 && ws.ws_row > 0 && ws.ws_col > 0 {
            return Some((ws.ws_row, ws.ws_col));
        }
    }
    None
}

/// Whether the active locale advertises UTF-8 (so the `─` rule renders). Checked
/// via the usual `LC_ALL` → `LC_CTYPE` → `LANG` precedence.
fn utf8_locale() -> bool {
    for key in ["LC_ALL", "LC_CTYPE", "LANG"] {
        if let Ok(v) = std::env::var(key) {
            if !v.is_empty() {
                let up = v.to_ascii_uppercase();
                return up.contains("UTF-8") || up.contains("UTF8");
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scroll_region_reserves_three_bottom_rows() {
        // 24-row terminal → region rows 1..=21, footer at 22/23/24.
        assert_eq!(scroll_region_seq(24), "\x1b[1;21r");
    }

    #[test]
    fn bottom_home_targets_last_row() {
        // Anchors the inline-mode attach view to the bottom row (col 1).
        assert_eq!(bottom_home_seq(50), "\x1b[50;1H");
        assert_eq!(bottom_home_seq(24), "\x1b[24;1H");
        // Degenerate zero height clamps to row 1 (never emits ESC[0;1H).
        assert_eq!(bottom_home_seq(0), "\x1b[1;1H");
    }

    #[test]
    fn suspend_footer_region_is_noop_when_inactive() {
        // No footer region installed (the state on every non-interactive
        // `run_on_tty` call path — scripts, pipelines, tests) → suspend is a
        // pure no-op that returns false, so the FooterRegionGuard skips its
        // resume and never emits stray escapes into a child's output stream.
        ACTIVE.store(false, Ordering::Relaxed);
        assert!(!suspend_footer_region());
        assert!(!ACTIVE.load(Ordering::Relaxed));
    }

    #[test]
    fn scroll_region_never_collapses_below_row_one() {
        // Degenerate tiny sizes still emit a valid (row 1) region.
        assert_eq!(scroll_region_seq(3), "\x1b[1;1r");
        assert_eq!(scroll_region_seq(1), "\x1b[1;1r");
    }

    #[test]
    fn separator_uses_box_char_when_utf8() {
        let s = separator_line(5, true, false);
        assert_eq!(s, "─────");
    }

    #[test]
    fn separator_falls_back_to_ascii_without_utf8() {
        let s = separator_line(4, false, false);
        assert_eq!(s, "----");
    }

    #[test]
    fn separator_dim_wraps_when_colored() {
        let s = separator_line(3, true, true);
        assert!(s.starts_with("\x1b[2m"));
        assert!(s.ends_with("\x1b[0m"));
    }

    #[test]
    fn footer_positions_three_rows_bottom_up() {
        let seq = footer_seq(24, 10, "----------", "msg", "bar");
        assert!(seq.starts_with("\x1b7")); // DECSC
        assert!(seq.ends_with("\x1b8")); // DECRC
        // The scroll-region re-assert (DECSTBM) must be saved-then-emitted: it
        // homes the cursor, so it has to sit AFTER the DECSC save and BEFORE the
        // first absolute row paint, or DECRC would restore the homed position
        // and strand the next prompt at the top of the screen.
        let decsc = seq.find("\x1b7").unwrap();
        let region = seq.find("\x1b[1;21r").expect("region re-asserted"); // 24 - 3 = 21
        let first_paint = seq.find("\x1b[22;1H").unwrap();
        assert!(decsc < region && region < first_paint);
        assert!(seq.contains("\x1b[22;1H")); // separator row = H-2
        assert!(seq.contains("\x1b[23;1H")); // status message row = H-1
        assert!(seq.contains("\x1b[24;1H")); // statusline row = H
        assert!(seq.contains("\x1b[2K")); // each row cleared first
    }

    #[test]
    fn clip_visible_truncates_plain_text() {
        assert_eq!(clip_visible("hello world", 5), "hello");
    }

    #[test]
    fn clip_visible_keeps_short_text_intact() {
        assert_eq!(clip_visible("hi", 10), "hi");
    }

    #[test]
    fn clip_visible_does_not_split_escape_and_resets_on_cut() {
        // 3 visible chars of colored text, clipped to 2 → keeps the full SGR
        // escape, two chars, then appends a RESET.
        let colored = "\x1b[1;33mABC\x1b[0m";
        let out = clip_visible(colored, 2);
        assert!(out.starts_with("\x1b[1;33m"));
        assert!(out.contains("AB"));
        assert!(!out.contains('C'));
        assert!(out.ends_with("\x1b[0m"));
    }

    #[test]
    fn clip_visible_counts_wide_chars() {
        // Each CJK char is display-width 2. Into max 3: 世(2) fits, 界 would make
        // 4 → stop. Into max 3 for "世x": 世(2)+x(1)=3 → both fit.
        assert_eq!(clip_visible("世界x", 3), "世");
        assert_eq!(clip_visible("世x", 3), "世x");
    }

    #[test]
    fn heartbeat_activity_resets_idle_timer() {
        // A fresh activity note zeroes the measured idle gap; the heartbeat only
        // repaints once that gap crosses HEARTBEAT_IDLE.
        note_footer_activity();
        let idle =
            heartbeat_now_ms().saturating_sub(LAST_FOOTER_ACTIVITY_MS.load(Ordering::Relaxed));
        assert!(
            idle < HEARTBEAT_IDLE.as_millis() as u64,
            "just-noted activity must read as well under the idle threshold, got {idle}ms"
        );
    }

    #[test]
    fn set_reading_line_toggles_and_arms_timer() {
        // Entering a read marks the idle-at-prompt window AND refreshes the
        // timer (so the first heartbeat waits a full interval at a new prompt).
        set_reading_line(true);
        assert!(READING_LINE.load(Ordering::Relaxed));
        let idle =
            heartbeat_now_ms().saturating_sub(LAST_FOOTER_ACTIVITY_MS.load(Ordering::Relaxed));
        assert!(idle < HEARTBEAT_IDLE.as_millis() as u64);
        // Leaving the read clears the window so the heartbeat stops repainting
        // the moment a line is submitted / an engine turn begins.
        set_reading_line(false);
        assert!(!READING_LINE.load(Ordering::Relaxed));
    }

    #[test]
    fn spawn_footer_heartbeat_is_idempotent() {
        // Guarded by an atomic swap — only the first call spawns; repeats no-op
        // (and never panic), so a re-init on resize can't leak threads.
        spawn_footer_heartbeat();
        spawn_footer_heartbeat();
    }

    #[test]
    fn footer_enabled_threshold() {
        let t = Terminal {
            rows: 5,
            cols: 80,
            active: false,
            utf8: true,
        };
        assert!(t.footer_enabled());
        let short = Terminal {
            rows: 4,
            cols: 80,
            active: false,
            utf8: true,
        };
        assert!(!short.footer_enabled());
    }
}
