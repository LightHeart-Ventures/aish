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
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

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

/// Last footer content painted `(status_msg, statusline)`, so a screen-clear
/// (Shift-Tab worker cycle, etc.) can repaint the footer without the caller
/// threading the strings back through.
static LAST_FOOTER: Mutex<(String, String)> = Mutex::new((String::new(), String::new()));

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
        // Re-assert the region every paint so a resize between prompts is picked
        // up even if we missed the SIGWINCH.
        buf.push_str(&scroll_region_seq(self.rows));
        buf.push_str(&footer_seq(self.rows, self.cols, &sep, status_msg, statusline));
        let mut out = std::io::stdout();
        let _ = write!(out, "{buf}");
        let _ = out.flush();
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
    let body_bottom = rows.saturating_sub(FOOTER_ROWS).max(1);
    let mut buf = String::new();
    buf.push_str(&scroll_region_seq(rows));
    buf.push_str(&footer_seq(rows, cols, &sep, &msg, &bar));
    // Home the cursor back into the body after the repaint.
    buf.push_str(&format!("\x1b[{body_bottom};1H"));
    let mut out = std::io::stdout();
    let _ = write!(out, "{buf}");
    let _ = out.flush();
}

/// Install a panic hook that resets the scroll region on unwind, so a crash
/// mid-session doesn't leave the user's terminal with a stuck footer region.
/// Chains the previous hook.
pub fn install_panic_hook() {
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
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
