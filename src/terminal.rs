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

/// Whether a worker attach view is currently active. The attach view renders on
/// the PRIMARY screen buffer (not the alternate buffer) so the terminal's native
/// scrollback keeps working — see [`open_attach_view`]. Tracked so the footer
/// heartbeat backs off while a worker view owns the foreground.
static ATTACH_ACTIVE: AtomicBool = AtomicBool::new(false);

/// True once we have XTSAVE'd + disabled xterm "alternate scroll" mode (DECSET
/// 1007) for the footer scroll region. Guards the save so region re-asserts
/// (resize / resume) don't clobber the saved *original* setting with the
/// already-disabled value, and gates the paired restore on teardown. See
/// [`suppress_alt_scroll_seq`] for the why.
static ALT_SCROLL_SUPPRESSED: AtomicBool = AtomicBool::new(false);

/// Last footer content painted `(status_msg, statusline)`, so a screen-clear
/// (Shift-Tab worker cycle, etc.) can repaint the footer without the caller
/// threading the strings back through.
static LAST_FOOTER: Mutex<(String, String)> = Mutex::new((String::new(), String::new()));

/// Mid-turn type-ahead override for the footer's status-message row (row H-1).
/// When `Some`, the footer paints THIS string in the message row instead of the
/// cached coordinator status message — so the operator sees the line they are
/// typing WHILE a model turn runs (thinking / mid tool-call). Set/cleared by the
/// keywatch reader thread via [`set_midturn_input`] / [`clear_midturn_input`].
static MIDTURN_INPUT: Mutex<Option<String>> = Mutex::new(None);

/// The status-message row to actually paint: the mid-turn type-ahead line when
/// one is active, otherwise the caller's cached coordinator status message. The
/// raw status message is always what gets cached in `LAST_FOOTER`; the override
/// is applied only at paint time so a resize/idle repaint reflects live typing.
fn effective_status_msg(cached: &str) -> String {
    match MIDTURN_INPUT.lock().unwrap().as_ref() {
        Some(s) => s.clone(),
        None => cached.to_string(),
    }
}

/// Paint the operator's mid-turn prompt + type-ahead line into the footer's
/// message row. `styled_prompt` is emitted verbatim (already colour-styled)
/// before `text`. An empty `text` now means "nothing typed yet" and surfaces the
/// BARE PROMPT sigil (so the operator can SEE there is a prompt to type into
/// during thinking / tool-calls) rather than removing the override — the normal
/// status message is restored only on turn teardown via [`clear_midturn_input`].
/// No visible effect when no footer region is installed (short terminal /
/// non-tty), but the override slot is still updated. Safe to call from the
/// keywatch reader thread — it locks stdout + a mutex exactly like the idle
/// heartbeat repaint.
pub fn set_midturn_input(styled_prompt: &str, text: &str) {
    {
        let mut slot = MIDTURN_INPUT.lock().unwrap();
        // Always surface at least the bare prompt affordance during a turn; append
        // the live line as the operator types. `clear_midturn_input` (turn
        // teardown) is what restores the cached status message.
        *slot = Some(format!("{styled_prompt}{text}"));
    }
    paint_cached_footer(false);
}

/// Clear any mid-turn type-ahead override and repaint the normal footer. Called
/// when a turn ends (guard teardown) and after each submitted line. Idempotent —
/// returns early (no repaint) when nothing was overridden.
pub fn clear_midturn_input() {
    {
        let mut slot = MIDTURN_INPUT.lock().unwrap();
        if slot.is_none() {
            return;
        }
        *slot = None;
    }
    paint_cached_footer(false);
}

/// Build the escape sequence that renders the mid-turn prompt affordance
/// INLINE, for terminals with no pinned footer message row (height <
/// [`MIN_FOOTER_ROWS`], or a non-footer tty). Returns
/// `\r\x1b[2K{styled_prompt}{text}`: carriage-return to column 0, erase the
/// whole line, then paint the (already colour-styled) bare prompt sigil plus any
/// typed text. Pure builder so it can be unit-tested without a real terminal.
pub fn midturn_inline_seq(styled_prompt: &str, text: &str) -> String {
    format!("\r\x1b[2K{styled_prompt}{text}")
}

/// Paint the mid-turn prompt affordance INLINE on the current cursor row (for
/// short / non-footer terminals). Writes straight to stdout from the keywatch
/// reader thread. This is the Gate #1 path behind `AISH_MIDTURN_INLINE`: it is
/// best-effort (it can race the engine's output writes on the main thread), so
/// the footer path ([`set_midturn_input`]) is always preferred when a footer
/// region exists. No MIDTURN_INPUT slot is touched — the affordance is drawn,
/// not cached, since there is no footer to repaint it into.
pub fn set_midturn_inline(styled_prompt: &str, text: &str) {
    let mut out = std::io::stdout();
    let _ = write!(out, "{}", midturn_inline_seq(styled_prompt, text));
    let _ = out.flush();
    note_footer_activity();
}

/// Erase an inline mid-turn prompt affordance at turn teardown (carriage-return
/// + erase-line). Pairs with [`set_midturn_inline`]; a no-op-looking write that
/// keeps the flag-gated inline path from leaving a stale `❯` on the row.
pub fn clear_midturn_inline() {
    let mut out = std::io::stdout();
    let _ = write!(out, "\r\x1b[2K");
    let _ = out.flush();
}

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

/// Terminal size `(rows, cols)` at the last footer paint, packed as
/// `(rows << 16) | cols`. The heartbeat compares the LIVE size against this and
/// repaints IMMEDIATELY on a change — bypassing the [`HEARTBEAT_IDLE`] gate — so
/// a window resize refreshes the footer within one heartbeat tick (~500ms)
/// instead of waiting out the full idle interval or the next REPL idle pass.
/// `0` = never painted. Updated inside [`paint_cached_footer`] so every paint
/// path (idle self-heal, mid-turn override, resize) keeps it current.
static LAST_PAINTED_SIZE: AtomicU64 = AtomicU64::new(0);

/// Pack `(rows, cols)` into a single u64 for atomic storage/compare.
fn pack_size(rows: u16, cols: u16) -> u64 {
    ((rows as u64) << 16) | cols as u64
}

/// True when the live terminal size differs from the last painted size (i.e. the
/// window was resized since the last footer paint). Off-tty / unknown size ⇒
/// `false` (nothing to refresh). Cheap: one TIOCGWINSZ ioctl + an atomic load.
fn size_changed_since_paint() -> bool {
    match term_size() {
        Some((rows, cols)) => {
            pack_size(rows, cols) != LAST_PAINTED_SIZE.load(Ordering::Relaxed)
        }
        None => false,
    }
}

/// True while the in-progress input line is non-empty. The heartbeat NEVER
/// repaints while this is set, so a partially-typed command can never be
/// clobbered by a footer repaint racing rustyline's own line render (the
/// "prompt eaten by the cursor" bug). Set from the highlighter on every redraw
/// (via [`set_input_dirty`]) and cleared when a fresh read begins.
static INPUT_DIRTY: AtomicBool = AtomicBool::new(false);

/// Record whether the input buffer currently holds text. The rustyline
/// highlighter calls this on every line render, so the heartbeat can back off
/// the moment the user starts typing and re-arm once the buffer is empty again.
pub fn set_input_dirty(dirty: bool) {
    INPUT_DIRTY.store(dirty, Ordering::Relaxed);
}

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
        // A fresh read starts with an empty buffer; clear any stale dirty flag
        // and re-arm the idle timer so the heartbeat waits a full interval.
        set_input_dirty(false);
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
                    || ATTACH_ACTIVE.load(Ordering::Relaxed)
                    // Never repaint over a line the user is mid-editing — a
                    // non-empty buffer means rustyline owns the visible cursor
                    // and a racing footer repaint would eat the prompt.
                    || INPUT_DIRTY.load(Ordering::Relaxed)
                {
                    continue;
                }
                let idle =
                    heartbeat_now_ms().saturating_sub(LAST_FOOTER_ACTIVITY_MS.load(Ordering::Relaxed));
                // Repaint when idle-timed-out OR the terminal was resized since
                // the last paint. The resize case bypasses the idle gate so the
                // footer tracks the new canvas size within one tick rather than
                // waiting out HEARTBEAT_IDLE — "update accordingly on resize".
                if idle >= idle_ms || size_changed_since_paint() {
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
    // Record the size we're painting at so the heartbeat can detect a later
    // resize and refresh the footer to the new canvas dimensions on sight.
    LAST_PAINTED_SIZE.store(pack_size(rows, cols), Ordering::Relaxed);
    let (msg, bar) = LAST_FOOTER.lock().map(|l| l.clone()).unwrap_or_default();
    // Mid-turn type-ahead, when active, takes over the message row.
    let msg = effective_status_msg(&msg);
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

// ---------------------------------------------------------------------------
// Mouse-wheel scrollback fix (xterm "alternate scroll", DECSET 1007).
//
// aish enables NO mouse tracking of its own, but the bottom-anchored footer
// keeps a DECSTBM scroll region installed for the whole session. On terminals
// where xterm "alternate scroll" mode (private mode 1007) is enabled — the
// default on VTE / gnome-terminal and several others — a live, less-than-full
// screen scroll region makes the terminal translate mouse-wheel ticks into
// cursor-key (Up / Down) events instead of scrolling its own scrollback. Those
// arrow keys land in rustyline at the prompt, so every wheel tick scrolled
// *input history* instead of the output area — the "mouse scroll scrolls
// history" complaint.
//
// The fix: while the footer region is installed, disable mode 1007 so the wheel
// drives the terminal's native scrollback (the output field the user wants to
// scroll). We XTSAVE the user's prior setting first and XTRESTORE it when the
// region is torn down (session exit, foreground-child suspend, panic) so we
// never permanently change the terminal's wheel behavior for child programs.
// Terminals that don't implement 1007 / XTSAVE simply ignore these sequences.
// ---------------------------------------------------------------------------

/// XTRESTORE private mode 1007 (pop the alternate-scroll setting pushed by the
/// paired XTSAVE `\x1b[?1007s` in [`suppress_alt_scroll_seq`]). The suppress
/// side XTSAVEs (`\x1b[?1007s`) then DECRSTs (`\x1b[?1007l`, disable) mode 1007.
const ALT_SCROLL_RESTORE: &str = "\x1b[?1007r";

/// The escape sequence to suppress alternate-scroll for the footer region: on
/// the FIRST call (per suppression cycle) it XTSAVEs the user's setting then
/// disables mode 1007; on subsequent calls (region re-asserted on resize /
/// resume) it returns `""` so the already-saved original is preserved rather
/// than overwritten with the disabled value.
fn suppress_alt_scroll_seq() -> &'static str {
    if ALT_SCROLL_SUPPRESSED.swap(true, Ordering::Relaxed) {
        "" // already suppressed — re-saving would clobber the real original
    } else {
        concat!("\x1b[?1007s", "\x1b[?1007l") // = ALT_SCROLL_SAVE + ALT_SCROLL_OFF
    }
}

/// The escape sequence to restore the pre-suppression alternate-scroll setting
/// when the footer region is torn down. Returns the XTRESTORE only when we had
/// actually suppressed (so a spurious teardown never emits a stray restore).
fn restore_alt_scroll_seq() -> &'static str {
    if ALT_SCROLL_SUPPRESSED.swap(false, Ordering::Relaxed) {
        ALT_SCROLL_RESTORE
    } else {
        ""
    }
}

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
        // Install the region, suppress alternate-scroll (so the mouse wheel
        // scrolls native scrollback instead of emitting Up/Down into rustyline),
        // then home into the body.
        let _ = write!(
            out,
            "{}{}\x1b[{body_bottom};1H",
            scroll_region_seq(self.rows),
            suppress_alt_scroll_seq(),
        );
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
        // Restore alternate-scroll, reset region, then clear from the footer's
        // top row to end of screen so no stale statusline is left behind.
        let _ = write!(
            out,
            "{}{RESET_REGION}\x1b[{sep_row};1H\x1b[J",
            restore_alt_scroll_seq(),
        );
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
        // Re-sync cached dimensions from the LIVE terminal size before painting
        // so a resize that lands DURING a turn (when the main loop hasn't yet
        // drained the SIGWINCH flag) is reflected on the very next footer draw:
        // footer_seq re-asserts the scroll region at these dims, so the bottom
        // margin tracks the new height without waiting for the idle pass.
        if let Some((rows, cols)) = term_size() {
            self.rows = rows;
            self.cols = cols;
        }
        // A resize below the footer threshold mid-turn: skip the paint (a footer
        // no longer fits) and let the idle handle_resize tear the region down.
        if self.rows < MIN_FOOTER_ROWS {
            return;
        }
        let sep = separator_line(self.cols, self.utf8, crate::style::colors_enabled());
        let mut buf = String::new();
        // footer_seq re-asserts the scroll region internally, INSIDE its
        // DECSC/DECRC save-restore, so the DECSTBM cursor-home side effect never
        // leaks out and strands the next prompt at the top of the screen.
        buf.push_str(&footer_seq(
            self.rows,
            self.cols,
            &sep,
            &effective_status_msg(status_msg),
            statusline,
        ));
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
    // DECSC → restore alternate-scroll (child gets normal wheel behavior) →
    // reset region to full screen → clear the footer rows → DECRC, so the
    // cursor stays exactly where the command line left it.
    let seq = format!(
        "\x1b7{}{RESET_REGION}\x1b[{sep_row};1H\x1b[J\x1b8",
        restore_alt_scroll_seq(),
    );
    let mut out = std::io::stdout();
    let _ = write!(out, "{seq}");
    let _ = out.flush();
    ACTIVE.store(false, Ordering::Relaxed);
    true
}

/// Build the cursor/region choreography that [`resume_footer_region`] writes to
/// reclaim the bottom [`FOOTER_ROWS`] rows for the footer after a full-screen
/// foreground child exits: re-assert DECSTBM (which homes the cursor to the
/// top-left as a side effect) then drop the cursor into the last body row so the
/// next prompt grows up from just above the footer. Kept pure (no I/O, no shared
/// state) so the sequence is unit-testable byte-for-byte.
///
/// KNOWN LIMITATION — output-clobber on resume (root cause of the "`ls -al`
/// loses its last few lines" report): while the child ran, the footer region
/// was suspended to the FULL screen (see [`suspend_footer_region`]), so any
/// output that scrolled into the bottom `FOOTER_ROWS` rows now lives there. This
/// resume paints the footer straight over those rows, hiding the command's last
/// lines from the viewport (they survive in native scrollback — aish never
/// emits ESC[3J). It is NOT the off-by-one once suspected in `scroll_region_seq`:
/// that math is correct (24 → `ESC[1;21r`, and 21 body + 3 footer = 24, pinned
/// by `scroll_region_reserves_three_bottom_rows`).
///
/// The correct fix is to scroll the body UP by the overflow
/// (`cursor_row - body_bottom`, clamped to `0..=FOOTER_ROWS`) BEFORE reclaiming
/// the rows — but the overflow is only knowable from a DSR (ESC[6n)
/// cursor-position query, which must run with a bounded, non-blocking read and a
/// total fallback to this behavior so a non-conforming terminal can never hang
/// the shell. An UNCONDITIONAL scroll-up is NOT viable: the prompt is
/// bottom-anchored, so nearly every command (even `echo hi`) leaves the cursor
/// in the footer zone, and a fixed scroll-up would jerk the screen up on every
/// command and after every alt-screen app. Deferred until it can be verified
/// against a real TTY.
pub fn resume_region_seq(rows: u16) -> String {
    let body_bottom = rows.saturating_sub(FOOTER_ROWS).max(1);
    format!("{}\x1b[{body_bottom};1H", scroll_region_seq(rows))
}

/// Re-establish the footer scroll region and repaint the cached footer after a
/// foreground child that inherited the full terminal exits. Pairs with
/// [`suspend_footer_region`]. Homes the cursor into the last body row so the
/// next prompt grows up from just above the footer (mirrors
/// [`Terminal::init_scroll_region`]). No-op when the terminal is now too short
/// to host the footer (e.g. it was resized smaller while the child ran). See
/// [`resume_region_seq`] for the choreography and the known output-clobber
/// limitation.
pub fn resume_footer_region() {
    let Some((rows, _cols)) = term_size() else {
        return;
    };
    if rows < MIN_FOOTER_ROWS {
        return;
    }
    // Re-assert the region + home into the last body row, and re-suppress
    // alternate-scroll alongside it (cursor-neutral: `suppress_alt_scroll_seq`
    // only toggles private mode 1007, so its position relative to the home move
    // is immaterial; it also returns "" after the first suppression so the saved
    // original setting is preserved).
    let seq = format!("{}{}", resume_region_seq(rows), suppress_alt_scroll_seq());
    let mut out = std::io::stdout();
    let _ = write!(out, "{seq}");
    let _ = out.flush();
    ACTIVE.store(true, Ordering::Relaxed);
    // Repaint the pinned footer rows from cache (cursor-safe: footer_seq wraps
    // its paint in DECSC/DECRC).
    paint_cached_footer(false);
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

/// Clear the visible screen and home the cursor (`ESC[2J ESC[H`), then flush.
/// The viewport is wiped so the next attach/detach view redraws on a clean
/// screen, while the terminal's native scrollback is preserved — that would
/// need `ESC[3J`, which we deliberately never send — so prior output stays
/// reachable by wheel / PageUp. No-op off a tty. Callers pair it with
/// [`anchor_bottom_after_wipe`] to repaint the footer + home into the body.
fn clear_screen_wipe() {
    // SAFETY: plain isatty query.
    if unsafe { libc::isatty(1) } != 1 {
        return;
    }
    let mut out = std::io::stdout();
    let _ = write!(out, "\x1b[2J\x1b[H");
    let _ = out.flush();
}

/// Open a worker attach view on the PRIMARY screen buffer.
///
/// The attach view deliberately does NOT switch to the alternate screen buffer
/// (`ESC[?1049h`). The alternate buffer has no scrollback, so any worker output
/// that scrolled past the top row was unreachable by the mouse wheel / PageUp —
/// the "can't scroll `:attach` worker output, only interactive" bug. Rendering
/// the attach stream inline on the primary buffer keeps the terminal's native
/// scrollback live (the wheel stays bound to scrollback via the alternate-scroll
/// suppression installed with the footer region), so a worker's output scrolls
/// exactly like interactive output.
///
/// On entry it clears the visible screen (`ESC[2J`, viewport only) so the attach
/// header + backfilled tail redraw cleanly instead of piling under the prior
/// prompt/output, then re-anchors the cursor to the bottom of the body. The wipe
/// is viewport-only: scrollback is preserved (that would need `ESC[3J`, never
/// sent), so the underlying interactive output stays reachable by wheel / PageUp.
/// Idempotent; no-op off a tty.
pub fn open_attach_view() {
    // SAFETY: plain isatty query.
    if unsafe { libc::isatty(1) } != 1 {
        return;
    }
    ATTACH_ACTIVE.store(true, Ordering::Relaxed);
    // Clear the visible screen before the attach header + backfill redraw, so
    // every Shift-Tab / `:attach` hop opens on a fresh screen instead of piling
    // under the prior interactive/worker output. `ESC[2J` blanks the viewport
    // only — the terminal's native scrollback is preserved (that needs `ESC[3J`,
    // which we deliberately never send) so earlier output stays reachable by
    // wheel / PageUp. Centralized here so every attach entry point (`:attach`,
    // `:attach goal`, and the Shift-Tab cycle) gets it uniformly.
    clear_screen_wipe();
    // Re-assert the footer region + home into the bottom body row so the worker
    // view grows up from the bottom, mirroring a fresh clear.
    anchor_bottom_after_wipe();
}

/// Close the worker attach view, returning to the interactive prompt on the same
/// primary buffer. Because [`open_attach_view`] never left the primary buffer,
/// there is nothing to restore — the interactive output is already in scrollback.
/// Just clears the attach flag and re-anchors the cursor to the bottom body row
/// so the detached line + next prompt trail the output. Idempotent; no-op off a
/// tty.
pub fn close_attach_view() {
    // SAFETY: plain isatty query.
    if unsafe { libc::isatty(1) } != 1 {
        return;
    }
    if !ATTACH_ACTIVE.swap(false, Ordering::Relaxed) {
        return;
    }
    // Clear the visible screen before the detach line + interactive backfill
    // redraw, mirroring `open_attach_view` so detaching also opens on a fresh
    // screen (scrollback preserved — `ESC[2J`, not `ESC[3J`).
    clear_screen_wipe();
    anchor_bottom_after_wipe();
}

/// The terminal's row count via `TIOCGWINSZ`, or `None` off a tty. Public so the
/// REPL can home the cursor to the bottom row when no footer region is installed.
pub fn screen_rows() -> Option<u16> {
    term_size().map(|(rows, _)| rows)
}

/// Whether a bottom-anchored footer scroll region is currently installed. True
/// only while the three-row footer (rule + status + statusline) is live, so a
/// caller can decide whether the last scrolling body row (`rows - FOOTER_ROWS`)
/// really sits directly above the horizontal rule. Used by the `:workers` tray
/// to bottom-anchor itself only when there is a rule to anchor above.
pub fn footer_active() -> bool {
    ACTIVE.load(Ordering::Relaxed)
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
        // A worker attach view renders on the primary buffer (no alternate
        // buffer to pop), so a crash mid-attach already lands on real
        // scrollback — just clear the flag.
        ATTACH_ACTIVE.store(false, Ordering::Relaxed);
        if ACTIVE.load(Ordering::Relaxed) {
            let mut out = std::io::stdout();
            let _ = write!(out, "{}{RESET_REGION}\r\n", restore_alt_scroll_seq());
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

    /// Serializes every test that mutates a process-global footer-state
    /// singleton — `MIDTURN_INPUT`, `READING_LINE`, `INPUT_DIRTY`. Cargo runs
    /// unit tests multi-threaded in ONE binary, so these tests otherwise race on
    /// the shared statics: one test's `clear_midturn_input()` /
    /// `set_reading_line(false)` can flip the slot/flag between a sibling's set
    /// and its assertion (observed: `coordinating…` instead of the bare prompt;
    /// `READING_LINE` load failing right after a `set_reading_line(true)`). Every
    /// footer-global test locks this first. Poison-tolerant: a panic while held
    /// must not cascade-fail the siblings.
    static FOOTER_STATE_TEST_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn alt_scroll_suppress_saves_once_then_restores() {
        // Deterministic starting point: not yet suppressed.
        ALT_SCROLL_SUPPRESSED.store(false, Ordering::Relaxed);

        // First suppress: XTSAVE (1007s) then disable (1007l), so the wheel
        // drives native scrollback instead of arrowing rustyline history.
        let first = suppress_alt_scroll_seq();
        assert_eq!(first, "\x1b[?1007s\x1b[?1007l");
        assert!(ALT_SCROLL_SUPPRESSED.load(Ordering::Relaxed));

        // Re-assert (resize/resume) must NOT re-save — that would clobber the
        // user's real original with the already-disabled value.
        assert_eq!(suppress_alt_scroll_seq(), "");
        assert!(ALT_SCROLL_SUPPRESSED.load(Ordering::Relaxed));

        // Teardown restores exactly once (XTRESTORE 1007r) and clears the flag.
        assert_eq!(restore_alt_scroll_seq(), "\x1b[?1007r");
        assert!(!ALT_SCROLL_SUPPRESSED.load(Ordering::Relaxed));

        // A spurious second restore emits nothing (never a stray XTRESTORE).
        assert_eq!(restore_alt_scroll_seq(), "");

        // The disable half is DECRST of private mode 1007 — the mode that,
        // when enabled, turns wheel ticks into cursor keys under a scroll
        // region. Assert the byte-level shape of the restore constant too.
        assert!(suppress_alt_scroll_seq().contains("\x1b[?1007l"));
        assert_eq!(ALT_SCROLL_RESTORE, "\x1b[?1007r");

        // Reset shared state so sibling tests that touch this global start clean.
        ALT_SCROLL_SUPPRESSED.store(false, Ordering::Relaxed);
    }

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
    fn resume_region_reasserts_then_homes_last_body_row() {
        // 24-row terminal: re-assert region 1..=21 then home into row 21 (the
        // last body row) so the next prompt grows up from just above the footer.
        assert_eq!(resume_region_seq(24), "\x1b[1;21r\x1b[21;1H");
    }

    #[test]
    fn resume_region_clamps_tiny_terminals() {
        // Degenerate heights collapse to a row-1 region AND a row-1 home rather
        // than emitting an invalid ESC[0;1H.
        assert_eq!(resume_region_seq(3), "\x1b[1;1r\x1b[1;1H");
        assert_eq!(resume_region_seq(1), "\x1b[1;1r\x1b[1;1H");
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
        let _g = FOOTER_STATE_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
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
    fn input_dirty_flag_round_trips_and_read_clears_it() {
        let _g = FOOTER_STATE_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // A non-empty in-progress line sets the dirty flag; the heartbeat reads
        // it and backs off (verified indirectly — the gate is a plain load).
        set_input_dirty(true);
        assert!(INPUT_DIRTY.load(Ordering::Relaxed));
        // Starting a fresh read always clears the flag — the buffer is empty at
        // the new prompt, so the heartbeat is free to repaint the footer again.
        set_reading_line(true);
        assert!(!INPUT_DIRTY.load(Ordering::Relaxed));
        set_reading_line(false);
    }

    #[test]
    fn midturn_empty_text_surfaces_bare_prompt_then_clears() {
        // Serialize against sibling tests that mutate footer-state globals.
        let _g = FOOTER_STATE_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // Turn-start priming: set_midturn_input with EMPTY text must surface the
        // bare prompt affordance (so the operator SEES a prompt to type into
        // during thinking / tool-calls), overriding the cached status message.
        // Teardown (clear_midturn_input) must restore the cached status message.
        // Locks in the ISS fix for "no visible prompt while the turn runs".
        let prompt = "\x1b[2m❯\x1b[0m ";

        // Baseline: no override → cached status shows through.
        clear_midturn_input();
        assert_eq!(effective_status_msg("coordinating…"), "coordinating…");

        // Turn start, nothing typed yet → bare prompt is surfaced.
        set_midturn_input(prompt, "");
        assert_eq!(effective_status_msg("coordinating…"), prompt);

        // Operator types → prompt + live line replaces the status row.
        set_midturn_input(prompt, "ls -la");
        assert_eq!(
            effective_status_msg("coordinating…"),
            format!("{prompt}ls -la")
        );

        // Turn teardown → cached status message restored.
        clear_midturn_input();
        assert_eq!(effective_status_msg("coordinating…"), "coordinating…");

        // Idempotent: a second clear is a no-op (no panic, stays cleared).
        clear_midturn_input();
        assert_eq!(effective_status_msg("idle"), "idle");
    }

    #[test]
    fn midturn_inline_seq_draws_bare_prompt_then_line() {
        // Serialize against sibling tests that mutate footer-state globals
        // (this test calls clear_midturn_input at the end).
        let _g = FOOTER_STATE_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // Gate #1 (short / non-footer terminals): the inline affordance must
        // carriage-return to col 0, erase the row, then paint the prompt sigil.
        let prompt = "\x1b[2m❯\x1b[0m ";

        // Turn start, nothing typed → CR + erase-line + bare prompt.
        assert_eq!(midturn_inline_seq(prompt, ""), format!("\r\x1b[2K{prompt}"));

        // Operator types → prompt + live line on the same erased row.
        assert_eq!(
            midturn_inline_seq(prompt, "ls -la"),
            format!("\r\x1b[2K{prompt}ls -la")
        );

        // The inline path never touches the footer's MIDTURN_INPUT slot, so the
        // cached status message keeps showing through the footer effective view.
        clear_midturn_input();
        assert_eq!(effective_status_msg("coordinating…"), "coordinating…");
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

    #[test]
    fn pack_size_round_trips_and_discriminates() {
        // Packing is injective over (rows, cols): distinct sizes → distinct keys.
        assert_eq!(pack_size(24, 80), pack_size(24, 80));
        assert_ne!(pack_size(24, 80), pack_size(50, 80)); // height changed
        assert_ne!(pack_size(24, 80), pack_size(24, 120)); // width changed
        assert_ne!(pack_size(24, 80), pack_size(80, 24)); // swapped ≠ same
        // cols occupy the low 16 bits, rows the next 16 — no cross-talk.
        assert_eq!(pack_size(1, 1), (1u64 << 16) | 1);
        assert_eq!(pack_size(0, 0), 0); // matches "never painted" sentinel
    }

    #[test]
    fn size_change_detection_flips_on_new_dims() {
        // A stored size equal to the probe ⇒ unchanged; a different one ⇒ changed.
        LAST_PAINTED_SIZE.store(pack_size(24, 80), Ordering::Relaxed);
        assert_eq!(pack_size(24, 80), LAST_PAINTED_SIZE.load(Ordering::Relaxed));
        assert_ne!(pack_size(30, 100), LAST_PAINTED_SIZE.load(Ordering::Relaxed));
    }
}
