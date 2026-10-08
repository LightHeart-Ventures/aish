//! Bounded activity ticker — the transient, self-erasing window that mid-turn
//! tool activity scrolls through instead of accumulating in scrollback.
//!
//! # Why
//!
//! Every mid-turn activity row used to be a PERMANENT scrollback row: a `✓ 🛠️
//! <desc>` line per tool call, a `… N lines of output — Ctrl-O to expand`
//! summary beneath it, a line of model narration between rounds, and (before
//! TASK-\* Fix 1) a duplicate `cache: …` telemetry line per API round. A 20-call
//! turn therefore burned ~90–110 rows of screen that the user had already read
//! and would never scroll back to. The answer they were waiting for landed
//! below the fold.
//!
//! The ticker bounds that cost: activity rows are painted into a window of at
//! most [`MAX_ROWS`] rows directly ABOVE the cursor. Pushing row N+1 erases the
//! window and repaints the last K rows, so the oldest row scrolls out of
//! existence rather than out of view. At turn end the window is erased
//! entirely. Net: K rows of screen and ZERO rows of scrollback, whether the turn
//! made 4 tool calls or 44.
//!
//! # The invariant (read this before touching any mid-turn writer)
//!
//! The erase walks UP from the anchor row erasing one row at a time —
//! `CSI 1 F` (cursor-previous-line) + `CSI 2 K` (erase line), repeated
//! `painted` times — the SAME bounded pattern
//! [`crate::engine::render_raw_toggle`] uses for its in-place Ctrl-O block. See
//! [`erase_rows_above`] for why it is NOT `CSI 0 J`. It is only correct while:
//!
//! 1. **The cursor sits at column 1 of the row immediately below the window.**
//!    Every painted row is terminated with `\n`, so this holds by construction
//!    after a paint — and it is why ANY other writer that emits a line mid-turn
//!    MUST call [`teardown`] first. A stray `eprintln!` below the window shifts
//!    the cursor down one row, the next erase lands one row low, and the window
//!    visibly tears (a stale top row with the live window drifting beneath it).
//!    That is the only failure mode this design has, and it is always caused by
//!    an unaudited writer. See the write-discipline list in `engine.rs`.
//! 2. **One logical row is exactly one PHYSICAL row.** A soft-wrapped row would
//!    make `painted` under-count the real height. [`clamp_visible`] enforces
//!    this inside [`push`], so callers cannot get it wrong — the ticker does not
//!    trust callers to pre-clamp.
//! 3. **The window never exceeds the viewport.** [`rows_cap`] clamps K to
//!    `rows - 2`, so a 6-row tmux pane gets a 4-row window and the cursor-up can
//!    never reach into the conversation above. This clamp is the whole reason a
//!    bounded window works where erasing a whole turn's output cannot: the erase
//!    region is bounded by the screen, not by the turn's length.
//!
//! # Opting out
//!
//! `:ticker off` (or `AISH_ACTIVITY_TICKER=off`) restores the old permanent
//! scrollback for forensics — erased rows are gone for good, so "what did it run
//! three rounds ago?" is answerable only from the turn audit or `Ctrl-O`, not
//! from the screen. The ticker is also bypassed entirely when stderr is not a
//! TTY (piped `aish -c`, background coordinators), when `:raw` is on (the user
//! explicitly asked for verbose permanence), and under quiet-summary turns
//! (which print no activity at all).

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use unicode_width::UnicodeWidthChar;

/// Maximum rows the activity window may occupy. Five is enough to read the
/// current call plus the three or four before it (the "what is it doing right
/// now" question) without the window becoming the page.
pub const MAX_ROWS: usize = 5;

/// Whether the ticker is enabled for this process. Default ON; flipped by
/// `:ticker off` / `AISH_ACTIVITY_TICKER=off` at startup.
static ENABLED: AtomicBool = AtomicBool::new(true);

/// The one live window. Process-global rather than a `Session` field on purpose:
/// teardown must be callable from every mid-turn writer (confirm prompts, raw
/// dumps, the TTY handoff in `run_interactive`, backend telemetry), and several
/// of those have no `&mut Session` in scope. aish is one session per process, so
/// a global is the honest representation.
static STATE: Mutex<Ticker> = Mutex::new(Ticker::new());

/// How many rows the window may use on a terminal of `term_rows` rows.
///
/// Clamped to `term_rows - 2` so the cursor-up erase can never reach above the
/// viewport into already-scrolled conversation: one row is reserved for the
/// cursor's own row below the window, one for breathing room. Returns 0 for a
/// terminal too short to host a window at all (callers then bypass the ticker
/// and print normally).
pub fn rows_cap(term_rows: usize) -> usize {
    MAX_ROWS.min(term_rows.saturating_sub(2))
}

/// Erase the `n` physical rows immediately ABOVE the cursor, leaving the cursor
/// at column 1 of the topmost erased row (so the caller repaints into the rows
/// it just reclaimed).
///
/// # Why this is a row walk and not `CSI {n} F` + `CSI 0 J`
///
/// ED (erase-in-display) is **not bounded by the DECSTBM scroll region** — `CSI
/// 0 J` erases from the cursor to the end of the *screen*, and aish's
/// bottom-anchored footer (separator, status message, statusline, and the
/// escalation/queued tray that rides above the separator) lives in the rows
/// BELOW the body region. So an anchor-relative `0 J` wiped the entire footer on
/// every single repaint; it only reappeared when the next statusline tick or the
/// idle heartbeat happened to repaint it, which reads as "the statusline keeps
/// disappearing while output streams" (the v0.53.2 regression).
///
/// EL (`CSI 2 K`) erases only the cursor's own row, so a walk touches exactly
/// the rows the window owns and nothing below them. The walk goes UP (CPL then
/// EL, `n` times) rather than up-then-down on purpose: CPL saturates at the top
/// margin, so if `n` ever exceeds the rows actually above the cursor the walk
/// harmlessly re-erases the top row instead of marching back DOWN past the
/// anchor and into the footer — the failure mode is clamped, not inverted.
pub fn erase_rows_above(n: usize) -> String {
    let mut out = String::with_capacity(n * 8);
    for _ in 0..n {
        out.push_str("\x1b[1F\x1b[2K");
    }
    out
}

/// The pure window transform. Holds the last K rows and how many physical rows
/// are currently painted on screen; every method returns the exact byte string
/// to write to stderr, so the whole state machine is unit-testable without a
/// terminal.
pub struct Ticker {
    window: Vec<String>,
    painted: usize,
}

impl Default for Ticker {
    fn default() -> Self {
        Self::new()
    }
}

impl Ticker {
    pub const fn new() -> Self {
        Self {
            window: Vec::new(),
            painted: 0,
        }
    }

    /// Rows currently painted on screen (the erase distance). Test-only: the
    /// production path reads `painted` directly through the `STATE` mutex.
    #[cfg(test)]
    pub fn painted(&self) -> usize {
        self.painted
    }

    /// The live window contents, oldest first. Test-only, as above.
    #[cfg(test)]
    pub fn window(&self) -> &[String] {
        &self.window
    }

    /// Append `row` and return the payload that repaints the window in place:
    /// erase the previously-painted rows, then print the (at most `cap`) newest
    /// rows. `row` must already be a single logical line — [`push`] splits and
    /// clamps before calling this.
    ///
    /// `cap == 0` degrades to a plain append (no erase, no window) so a terminal
    /// too short for a window still shows activity.
    pub fn push(&mut self, row: &str, cap: usize) -> String {
        if cap == 0 {
            return format!("{row}\n");
        }
        self.window.push(row.to_string());
        while self.window.len() > cap {
            self.window.remove(0);
        }
        let mut out = String::with_capacity(row.len() + 16 * self.window.len());
        if self.painted > 0 {
            out.push_str(&erase_rows_above(self.painted));
        }
        for line in &self.window {
            out.push_str(line);
            out.push('\n');
        }
        self.painted = self.window.len();
        out
    }

    /// Erase the window and forget it. Idempotent — a second call (or a call on
    /// a never-painted ticker) returns an empty payload and writes nothing, so
    /// every writer can call it unconditionally without tracking whether someone
    /// else already did.
    pub fn teardown(&mut self) -> String {
        let out = erase_rows_above(self.painted);
        self.window.clear();
        self.painted = 0;
        out
    }
}

/// Visible (ANSI-stripped, Unicode display) width of `s`.
fn visible_width(s: &str) -> usize {
    let mut w = 0usize;
    let mut chars = s.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\x1b' {
            skip_escape(&mut chars, None);
            continue;
        }
        w += ch.width().unwrap_or(0);
    }
    w
}

/// Consume one escape sequence (the ESC is already taken), optionally copying it
/// into `out` — escapes have zero display width, so they are preserved verbatim
/// when truncating.
fn skip_escape(chars: &mut std::iter::Peekable<std::str::Chars<'_>>, mut out: Option<&mut String>) {
    if let Some(o) = out.as_deref_mut() {
        o.push('\x1b');
    }
    // CSI (`ESC [ … final`) is the only form the activity lines use; a bare
    // two-character escape falls through after its single intermediate byte.
    if chars.peek() == Some(&'[') {
        chars.next();
        if let Some(o) = out.as_deref_mut() {
            o.push('[');
        }
        for c in chars.by_ref() {
            if let Some(o) = out.as_deref_mut() {
                o.push(c);
            }
            if ('\x40'..='\x7e').contains(&c) {
                break;
            }
        }
    } else if let Some(c) = chars.next()
        && let Some(o) = out
    {
        o.push(c);
    }
}

/// Truncate `s` to at most `cols` visible columns, preserving ANSI escapes
/// (zero width) and appending `…` + a reset when anything was cut.
///
/// This is what guarantees invariant 2 — one logical row is one physical row —
/// so a long `desc` (an unclamped `gh pr create --body` payload, say) cannot
/// soft-wrap and desynchronise the erase distance.
pub fn clamp_visible(s: &str, cols: usize) -> String {
    if cols == 0 {
        return String::new();
    }
    if visible_width(s) <= cols {
        return s.to_string();
    }
    let budget = cols.saturating_sub(1); // room for the ellipsis
    let mut out = String::with_capacity(s.len());
    let mut w = 0usize;
    let mut chars = s.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\x1b' {
            skip_escape(&mut chars, Some(&mut out));
            continue;
        }
        let cw = ch.width().unwrap_or(0);
        if w + cw > budget {
            break;
        }
        out.push(ch);
        w += cw;
    }
    out.push('…');
    out.push_str("\x1b[0m");
    out
}

/// Apply `AISH_ACTIVITY_TICKER` once at startup. Accepts `off`/`0`/`false`/`no`
/// to disable; anything else (including unset) leaves the default ON. Exists so
/// an operator who wants permanent scrollback can set it in their rc file
/// instead of typing `:ticker off` every session.
pub fn init_from_env() {
    if let Ok(v) = std::env::var("AISH_ACTIVITY_TICKER") {
        let v = v.trim().to_ascii_lowercase();
        if matches!(v.as_str(), "off" | "0" | "false" | "no") {
            set_enabled(false);
        }
    }
}

/// Enable/disable the ticker for this process (`:ticker`, `AISH_ACTIVITY_TICKER`).
pub fn set_enabled(on: bool) {
    ENABLED.store(on, Ordering::Relaxed);
}

/// Whether the ticker is enabled (ignores TTY/viewport gating — see [`active`]).
pub fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

/// Whether activity should actually be routed through the window right now:
/// enabled, stderr is a terminal, and the viewport is tall enough to host a
/// window. False means callers print permanently, exactly as before the ticker.
pub fn active() -> bool {
    enabled() && crate::engine::stderr_is_tty() && rows_cap(crate::engine::stderr_rows()) > 0
}

/// Push one activity row through the window (or print it permanently when the
/// ticker is inactive). Embedded newlines are split into separate rows, and each
/// row is clamped to a single physical row.
pub fn push(row: &str) {
    if !active() {
        for line in row.split('\n') {
            eprintln!("{line}");
        }
        return;
    }
    let cols = crate::engine::stderr_cols().saturating_sub(1);
    let cap = rows_cap(crate::engine::stderr_rows());
    let mut payload = String::new();
    {
        let mut t = STATE.lock().unwrap_or_else(|e| e.into_inner());
        for line in row.split('\n') {
            payload.push_str(&t.push(&clamp_visible(line, cols), cap));
        }
    }
    eprint!("{payload}");
}

/// Push a multi-line BLOCK (model narration) through the window — but only when
/// it fits. A block taller than the window would be shredded down to its last K
/// rows, and narration is content the user reads, not activity they skim: so an
/// oversized block tears the window down and prints permanently, while a
/// one-or-two-line aside ("Let me check the other file") scrolls transiently
/// like any tool row. Returns true when the block was absorbed by the ticker.
pub fn push_block(text: &str) -> bool {
    if !active() {
        return false;
    }
    let cap = rows_cap(crate::engine::stderr_rows());
    let cols = crate::engine::stderr_cols();
    // Count PHYSICAL rows: a single long paragraph is one logical line but many
    // rows on screen, and it is the physical height that has to fit.
    let height: usize = text
        .split('\n')
        .map(|l| crate::engine::physical_rows(l, cols))
        .sum();
    if height > cap {
        teardown();
        return false;
    }
    push(text);
    true
}

/// Erase the activity window. MUST be called by every mid-turn writer that
/// prints below it (see invariant 1) and at every turn boundary. Idempotent and
/// cheap — a no-op when nothing is painted.
pub fn teardown() {
    if !crate::engine::stderr_is_tty() {
        return;
    }
    let payload = {
        let mut t = STATE.lock().unwrap_or_else(|e| e.into_inner());
        t.teardown()
    };
    if !payload.is_empty() {
        eprint!("{payload}");
    }
}

/// RAII turn guard: tears the window down on drop, so EVERY exit path from a
/// turn — clean answer, `?` error, panic unwind — leaves the screen clean before
/// the caller prints the final answer. Held for the whole logical turn in
/// [`crate::engine::run_turn`].
pub struct TurnGuard;

impl TurnGuard {
    pub fn new() -> Self {
        Self
    }
}

impl Default for TurnGuard {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for TurnGuard {
    fn drop(&mut self) {
        teardown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One row of the bounded erase walk: up one line, erase that line.
    const E: &str = "\x1b[1F\x1b[2K";

    /// The erase NEVER uses erase-in-display: ED is not bounded by the DECSTBM
    /// scroll region, so `CSI 0 J` from a body row wipes the bottom-anchored
    /// footer (separator + statusline + escalation tray) on every repaint. This
    /// pins the v0.53.2 regression shut for both the push and teardown paths.
    #[test]
    fn erase_never_reaches_below_the_window() {
        assert_eq!(erase_rows_above(0), "");
        assert_eq!(erase_rows_above(1), E);
        assert_eq!(erase_rows_above(3), E.repeat(3));
        let mut t = Ticker::new();
        let mut emitted = String::new();
        for r in ["a", "b", "c", "d", "e", "f"] {
            emitted.push_str(&t.push(r, 3));
        }
        emitted.push_str(&t.teardown());
        assert!(
            !emitted.contains("\x1b[J") && !emitted.contains("\x1b[0J"),
            "ticker emitted an unbounded erase-in-display: {emitted:?}"
        );
    }

    #[test]
    fn first_push_paints_without_erasing() {
        let mut t = Ticker::new();
        // Nothing painted yet → no cursor-up, just the row.
        assert_eq!(t.push("a", 3), "a\n");
        assert_eq!(t.painted(), 1);
    }

    #[test]
    fn growing_window_erases_what_it_painted() {
        let mut t = Ticker::new();
        t.push("a", 3);
        // Second push erases the 1 painted row and repaints both.
        assert_eq!(t.push("b", 3), format!("{E}a\nb\n"));
        assert_eq!(t.painted(), 2);
        assert_eq!(t.push("c", 3), format!("{}a\nb\nc\n", E.repeat(2)));
        assert_eq!(t.painted(), 3);
    }

    #[test]
    fn full_window_scrolls_the_oldest_row_out() {
        let mut t = Ticker::new();
        for r in ["a", "b", "c"] {
            t.push(r, 3);
        }
        // At cap: erase 3, repaint the NEWEST 3 — "a" is gone for good.
        assert_eq!(t.push("d", 3), format!("{}b\nc\nd\n", E.repeat(3)));
        assert_eq!(t.painted(), 3);
        assert_eq!(t.window(), ["b", "c", "d"]);
        // Steady state: the payload never grows past cap rows, no matter how
        // many calls the turn makes. This is the whole point.
        for r in ["e", "f", "g", "h", "i", "j"] {
            let out = t.push(r, 3);
            assert_eq!(out.lines().count(), 3);
            assert!(out.starts_with(&E.repeat(3)));
        }
        assert_eq!(t.window(), ["h", "i", "j"]);
    }

    #[test]
    fn teardown_erases_and_is_idempotent() {
        let mut t = Ticker::new();
        t.push("a", 3);
        t.push("b", 3);
        assert_eq!(t.teardown(), E.repeat(2));
        assert_eq!(t.painted(), 0);
        // Second call writes nothing — every writer can call it blind.
        assert_eq!(t.teardown(), "");
        assert_eq!(Ticker::new().teardown(), "");
        // A push after teardown starts a fresh window (no stale erase).
        assert_eq!(t.push("c", 3), "c\n");
    }

    #[test]
    fn cap_zero_degrades_to_plain_append() {
        let mut t = Ticker::new();
        // Terminal too short to own rows → never emit a cursor-up.
        assert_eq!(t.push("a", 0), "a\n");
        assert_eq!(t.push("b", 0), "b\n");
        assert_eq!(t.painted(), 0);
        assert_eq!(t.teardown(), "");
    }

    #[test]
    fn rows_cap_clamps_to_the_viewport() {
        // Tall terminal → the MAX_ROWS ceiling governs.
        assert_eq!(rows_cap(50), MAX_ROWS);
        assert_eq!(rows_cap(7), MAX_ROWS);
        // Short pane → rows-2, so the cursor-up can never leave the viewport.
        assert_eq!(rows_cap(6), 4);
        assert_eq!(rows_cap(3), 1);
        // Degenerate: no window at all rather than an unsafe erase.
        assert_eq!(rows_cap(2), 0);
        assert_eq!(rows_cap(1), 0);
        assert_eq!(rows_cap(0), 0);
    }

    #[test]
    fn clamp_preserves_escapes_and_bounds_width() {
        // Fits → byte-identical passthrough (no needless churn).
        let row = "\x1b[2m  ✓ ok\x1b[0m";
        assert_eq!(clamp_visible(row, 40), row);
        // Over budget → truncated to cols-1 visible cols + ellipsis, with the
        // leading escape preserved so the row stays dim.
        let long = "\x1b[2mabcdefghij\x1b[0m";
        let out = clamp_visible(long, 6);
        assert_eq!(out, "\x1b[2mabcde…\x1b[0m");
        assert_eq!(visible_width(&out), 6);
        // Zero-width escapes don't consume budget.
        assert_eq!(visible_width("\x1b[2m\x1b[31mabc\x1b[0m"), 3);
        // A wide (double-width) glyph counts as two columns.
        assert_eq!(visible_width("🔧"), 2);
        assert_eq!(clamp_visible("", 10), "");
        assert_eq!(clamp_visible("abc", 0), "");
    }
}
