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

/// Scrolling rows the body keeps no matter how hard the footer is squeezed. The
/// footer is chrome; command output is the product — footer zones are shed to
/// protect these rows, never the other way round.
pub const MIN_BODY_ROWS: u16 = 2;

/// Minimum terminal height for the FULL footer: [`FOOTER_ROWS`] of chrome plus
/// the [`MIN_BODY_ROWS`] scrolling rows we insist on keeping above it.
pub const MIN_FOOTER_ROWS: u16 = MIN_BODY_ROWS + FOOTER_ROWS;

/// Smallest terminal height that still gets SOME footer: [`MIN_BODY_ROWS`] of
/// body plus the one un-sheddable statusline row. Between this and
/// [`MIN_FOOTER_ROWS`] the footer DEGRADES — it drops the rule, then the status
/// message — instead of vanishing outright. See [`FooterLayout`].
pub const MIN_FOOTER_ROWS_DEGRADED: u16 = MIN_BODY_ROWS + 1;

/// The footer's resolved row plan for one terminal size — the SINGLE source of
/// truth every row-arithmetic site reads.
///
/// ## Why a solver instead of a hard row stack
/// The footer used to be a rigid bottom-up stack: three fixed rows (separator,
/// status message, statusline) reserved and painted unconditionally, with
/// escalation banners layered on top. Two problems fell out of that rigidity:
///
/// 1. **A cliff, not a gradient.** Below [`MIN_FOOTER_ROWS`] the ENTIRE footer
///    was dropped, so a 4-row window got no statusline at all — even though the
///    statusline is the single highest-value row and the separator directly
///    above it is pure chrome carrying zero information. The shed order was
///    effectively "everything or nothing".
/// 2. **Two independent derivations.** A `footer_rows_for` helper computed the reserved
///    height while `footer_seq_with` recomputed its own `height` and hardcoded
///    `msg_row = rows - 1` / `bar_row = rows`. They happened to agree, but
///    nothing structurally forced them to, so any new zone risked a
///    region-vs-paint desync — which shows up as a corrupted viewport.
///
/// [`FooterLayout::solve`] fixes both: zones are shed in PRIORITY order until
/// the plan fits the window, and the survivors are packed contiguously upward
/// from the last row. The DECSTBM bottom margin, the body-home row, the resume
/// choreography and the paint itself all read this one struct, so region and
/// paint cannot disagree by construction.
///
/// ## Shed order (first shed → last)
/// | Zone | Rows | Why it sheds where it does |
/// |---|---|---|
/// | escalation banners | 2 each | notifications; shed WHOLE, oldest-first |
/// | separator rule | 1 | pure chrome — carries no information at all |
/// | status message | 1 | transient, and the same text also prints inline |
/// | statusline | 1 | version/model/stats/clock — never shed while a footer exists |
///
/// A plan with `height == 0` means "no footer fits"; the caller falls back to
/// inline printing exactly as it did below the old threshold.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FooterLayout {
    /// Terminal height this plan was solved for.
    pub rows: u16,
    /// Total rows the footer owns (`0` → no footer fits; print inline).
    pub height: u16,
    /// Rows granted to the pinned escalation block — always a whole multiple of
    /// [`crate::escalation::ROWS_PER_BANNER`].
    pub banner_rows: u16,
    /// Screen row of the horizontal rule, or `None` when it was shed.
    pub sep_row: Option<u16>,
    /// Screen row of the status message, or `None` when it was shed.
    pub msg_row: Option<u16>,
    /// Screen row of the statusline, or `None` only when no footer fits at all.
    pub bar_row: Option<u16>,
    /// Last scrolling row — the DECSTBM bottom margin AND the body-home target.
    pub body_bottom: u16,
}

impl FooterLayout {
    /// Solve the row plan for a `rows`-tall window that WANTS `want_banner_rows`
    /// of pinned escalation. Pure — no globals, no I/O — so every window size is
    /// unit-testable without touching a real terminal.
    pub fn solve(rows: u16, want_banner_rows: u16) -> Self {
        let per = crate::escalation::ROWS_PER_BANNER.max(1);
        // Normalize DOWN to whole banners first: a banner is an indivisible
        // 2-row unit (the escalation message + that worker's latest status), so
        // half a banner must never become reservable.
        let mut banner_rows = want_banner_rows - (want_banner_rows % per);
        let (mut sep, mut msg, mut bar) = (true, true, true);
        // Budget = every row except the body rows we refuse to give up.
        let budget = rows.saturating_sub(MIN_BODY_ROWS);
        let height = loop {
            let h = banner_rows + u16::from(sep) + u16::from(msg) + u16::from(bar);
            if h <= budget {
                break h;
            }
            // Shed strictly in priority order. `bar` is last, and shedding it
            // yields h == 0, which fits any budget — so the loop terminates.
            if banner_rows > 0 {
                banner_rows -= per;
            } else if sep {
                sep = false;
            } else if msg {
                msg = false;
            } else {
                bar = false;
            }
        };
        // The two documented thresholds are DERIVED facts about this solver, not
        // independent knobs — assert they still describe it so the doc table and
        // the code can never quietly disagree.
        debug_assert_eq!(
            height > 0,
            rows >= MIN_FOOTER_ROWS_DEGRADED,
            "degraded-footer threshold drifted from the solver at {rows} rows"
        );
        debug_assert_eq!(
            sep && msg && bar,
            rows >= MIN_FOOTER_ROWS,
            "full-footer threshold drifted from the solver at {rows} rows"
        );
        // Pack the survivors contiguously upward from the last row, so a shed
        // zone closes the gap instead of leaving a hole the body can't use.
        let mut next = rows;
        let mut take = |want: bool| -> Option<u16> {
            if !want || next == 0 {
                return None;
            }
            let row = next;
            next -= 1;
            Some(row)
        };
        let bar_row = take(bar);
        let msg_row = take(msg);
        let sep_row = take(sep);
        Self {
            rows,
            height,
            banner_rows,
            sep_row,
            msg_row,
            bar_row,
            body_bottom: rows.saturating_sub(height).max(1),
        }
    }

    /// [`Self::solve`] against the LIVE escalation stack — the runtime entry
    /// point. Kept separate so the solver itself stays pure.
    pub fn for_rows(rows: u16) -> Self {
        let want = if crate::escalation::active() {
            crate::escalation::row_count()
        } else {
            0 // common case: no escalation, no banner arithmetic
        };
        Self::solve(rows, want)
    }

    /// True when any footer — full or degraded — fits this window.
    pub fn enabled(&self) -> bool {
        self.height > 0
    }

    /// How many WHOLE banners the plan granted.
    pub fn banner_count(&self) -> usize {
        (self.banner_rows / crate::escalation::ROWS_PER_BANNER.max(1)) as usize
    }

    /// The first (topmost) screen row the footer owns. Teardown paths clear from
    /// here to end-of-screen, so it MUST track the banner block — clearing from
    /// the separator alone would strand banner rows on the terminal a child
    /// program (or the exiting shell) inherits.
    pub fn top_row(&self) -> u16 {
        self.rows
            .saturating_sub(self.height.saturating_sub(1))
            .max(1)
    }

    /// The DECSTBM bottom margin for this plan. Identical to
    /// [`Self::body_bottom`] by definition — named separately so region code
    /// reads as region code, and so the two can never drift apart.
    pub fn region_bottom(&self) -> u16 {
        self.body_bottom
    }

    /// Rows of child output sitting UNDER this plan's footer, given the 1-based
    /// `cursor_row` the child left behind. See [`footer_overflow_rows`] for the
    /// bug this measures; keeping it a method on the plan means the resume
    /// choreography and the region arithmetic read the SAME solved layout.
    pub fn overflow_rows(&self, cursor_row: u16) -> u16 {
        cursor_row.saturating_sub(self.body_bottom).min(self.height)
    }
}

/// How many of the `want`ed escalation rows a window of `rows` rows can actually
/// give the pinned block: whole banners only, shed oldest-first (the escalation
/// module renders newest-first, so trimming the tail keeps the newest visible)
/// until at least 2 scrolling rows survive above the footer.
///
/// Both the reserved region and the paint derive their banner count from THIS
/// function, so a resize can never leave the two disagreeing.
fn escalation_rows_that_fit(rows: u16, want: u16) -> u16 {
    FooterLayout::solve(rows, want).banner_rows
}

/// The first (topmost) screen row the footer owns: the escalation banner's first
/// row while one is pinned, otherwise the separator.
///
/// Teardown paths clear from this row to end-of-screen, so it MUST track the
/// banner — clearing from the separator alone would strand the two banner rows
/// on the terminal a child program (or the exiting shell) inherits.
pub fn footer_top_row(rows: u16) -> u16 {
    FooterLayout::for_rows(rows).top_row()
}

/// Hard ceiling on the DSR (`ESC[6n`) cursor-position exchange in
/// [`query_cursor_row`]. Generous for a local pty and still imperceptible, but
/// bounded so a terminal that never answers costs one blink, not a hung shell.
const CURSOR_QUERY_TIMEOUT: Duration = Duration::from_millis(120);

/// Byte ceiling on the same exchange, so a terminal streaming unrelated input
/// can't grow the buffer without bound while we look for the reply.
const CURSOR_QUERY_MAX_BYTES: usize = 256;

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

/// Build the line that announces a mid-turn submitted command as QUEUED, for
/// emission into the **text output area** (the scrolling body above the footer).
///
/// The footer's message row only ever shows the line the operator is CURRENTLY
/// typing — on Enter that echo is wiped ([`set_midturn_input`] with empty text),
/// so without this the operator got no confirmation at all that their command
/// was accepted while aish was busy. It looked dropped. This renders a durable
/// receipt into scrollback instead:
///
/// ```text
/// ⏳ queued → ls -la
/// ⏳ queued #2 → git status
/// ```
///
/// `position` is the 1-based submission index within the current turn (queued
/// lines are drained and run in submission order once the turn ends); it is
/// omitted for the first line and shown as `#N` thereafter, so the operator can
/// see how deep the queue is. `utf8`/`color_on` mirror [`separator_line`] so a
/// non-UTF-8 locale or `NO_COLOR` degrades cleanly. The body is clipped to
/// `cols` visible columns via [`clip_visible`] so a long paste can never wrap
/// and scroll the body twice.
///
/// Leading `\r\x1b[2K` (same convention as [`midturn_inline_seq`]) homes the
/// cursor and erases the row before painting, and the trailing `\r\n` scrolls
/// the body by exactly one row — the footer lives OUTSIDE the scroll region, so
/// it is untouched. Pure builder, so the format is unit-tested without a tty.
pub fn midturn_queued_seq(
    text: &str,
    position: usize,
    cols: u16,
    utf8: bool,
    color_on: bool,
) -> String {
    let sigil = if utf8 { "⏳" } else { "[q]" };
    let arrow = if utf8 { "→" } else { "->" };
    let pos = if position > 1 {
        format!(" #{position}")
    } else {
        String::new()
    };
    let body = if color_on {
        format!("\x1b[2m{sigil} queued{pos} {arrow}\x1b[0m {text}")
    } else {
        format!("{sigil} queued{pos} {arrow} {text}")
    };
    let clipped = clip_visible(&body, cols.max(1) as usize);
    format!("\r\x1b[2K{clipped}\x1b[0m\r\n")
}

/// Emit the "queued" receipt for a mid-turn submitted line into the text output
/// area. Called from the keywatch reader thread the instant a line is submitted,
/// BEFORE it is handed to the REPL's type-ahead channel.
///
/// Best-effort, exactly like [`set_midturn_inline`]: this writes to stdout from
/// the reader thread, so it can in principle interleave with the engine's output
/// writes on the main thread (the leading erase-line can clip a partially-written
/// row, e.g. a spinner, which repaints on its next tick). That race is accepted
/// for the same reason the inline affordance accepts it — a visible receipt beats
/// a silently-swallowed command — and it is strictly safer than the inline path
/// because this one ONLY ever appends a fully-terminated row.
pub fn print_midturn_queued(text: &str, position: usize) {
    let cols = term_size().map(|(_, c)| c).unwrap_or(80);
    let seq = midturn_queued_seq(
        text,
        position,
        cols,
        utf8_locale(),
        crate::style::colors_enabled(),
    );
    let mut out = std::io::stdout();
    let _ = write!(out, "{seq}");
    let _ = out.flush();
    note_footer_activity();
}

/// Build the "ran immediately" receipt for a mid-turn colon command that was
/// executed WHILE the turn was still in flight (see
/// [`crate::midturn_input::runs_immediately`]) rather than queued. Same clipping
/// / locale / color degradation rules as [`midturn_queued_seq`]; the distinct
/// sigil is the whole point — the operator must be able to tell at a glance that
/// `:dispatch` fired NOW and is running alongside the turn, not that it is
/// waiting in line behind it.
pub fn midturn_now_seq(text: &str, cols: u16, utf8: bool, color_on: bool) -> String {
    let sigil = if utf8 { "⚡" } else { "[!]" };
    let arrow = if utf8 { "→" } else { "->" };
    let body = if color_on {
        format!("\x1b[2m{sigil} ran {arrow}\x1b[0m {text}")
    } else {
        format!("{sigil} ran {arrow} {text}")
    };
    let clipped = clip_visible(&body, cols.max(1) as usize);
    format!("\r\x1b[2K{clipped}\x1b[0m\r\n")
}

/// Emit the "ran immediately" receipt into the text output area. Called from the
/// keywatch reader thread the instant a whitelisted colon command is submitted
/// mid-turn, BEFORE it is handed to the REPL's immediate channel. Same
/// best-effort stdout-from-the-reader-thread caveat as
/// [`print_midturn_queued`].
pub fn print_midturn_now(text: &str) {
    let cols = term_size().map(|(_, c)| c).unwrap_or(80);
    let seq = midturn_now_seq(text, cols, utf8_locale(), crate::style::colors_enabled());
    let mut out = std::io::stdout();
    let _ = write!(out, "{seq}");
    let _ = out.flush();
    note_footer_activity();
}

/// Erase an inline mid-turn prompt affordance at turn teardown (carriage-return
/// + erase-line). Pairs with [`set_midturn_inline`]; a no-op-looking write that
///   keeps the flag-gated inline path from leaving a stale `❯` on the row.
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
// The fix is a low-frequency heartbeat: whenever nothing has repainted the
// footer for `HEARTBEAT_IDLE`, a background thread repaints it from the cached
// content. The repaint is cursor-safe (DECSC/DECRC in `footer_seq` saves +
// restores the caller's cursor, so an in-progress input line or a live spinner
// row is untouched) and the whole sequence is written with a single buffered
// `write!` + flush, so it can't interleave halfway with the main thread's
// output.
//
// It deliberately runs BOTH at the prompt AND mid-turn (model thinking, tool
// calls): a turn can last minutes, and that is exactly when a scroll or resize
// used to leave the statusline missing until the turn ended. See
// `heartbeat_should_paint` for the gate and the regression it fixes.
// ---------------------------------------------------------------------------

/// Idle gap after which the heartbeat repaints the footer. Chosen at 3s: long
/// enough to be invisible during normal typing/output, short enough that a
/// scrolled-away footer snaps back almost immediately.
pub const HEARTBEAT_IDLE: Duration = Duration::from_secs(3);

/// True only while the REPL is blocked in a line read (idle at the prompt).
/// Scopes the [`INPUT_DIRTY`] back-off to the prompt: while rustyline is
/// rendering a line it owns the visible cursor, so the heartbeat must not
/// repaint over a partially-typed command. Mid-turn (this is `false`) rustyline
/// isn't rendering and the heartbeat keeps healing the footer. Toggled by
/// [`set_reading_line`].
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
        Some((rows, cols)) => pack_size(rows, cols) != LAST_PAINTED_SIZE.load(Ordering::Relaxed),
        None => false,
    }
}

/// True while the in-progress input line is non-empty. The heartbeat does not
/// repaint while this is set AND the REPL is at the prompt ([`READING_LINE`]), so
/// a partially-typed command can never be clobbered by a footer repaint racing
/// rustyline's own line render (the "prompt eaten by the cursor" bug). Set from
/// the highlighter on every redraw (via [`set_input_dirty`]) and cleared when a
/// fresh read begins — mid-turn type-ahead lives in `MIDTURN_INPUT` instead,
/// which the repaint itself draws.
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

/// Everything the heartbeat knows at one tick, sampled from the footer-state
/// atomics. Split out so the repaint decision is a pure function the tests can
/// drive exhaustively (the thread itself is untestable).
#[derive(Debug, Clone, Copy)]
struct HeartbeatState {
    /// A footer scroll region is installed. Also false while a foreground TTY
    /// child owns the terminal — `suspend_footer_region` clears `ACTIVE` — so
    /// vim/sudo/less are never painted over.
    region_active: bool,
    /// A worker alt-screen (`:attach`) view owns the terminal.
    attach_active: bool,
    /// The REPL is parked in a blocking line read (idle at the prompt).
    reading_line: bool,
    /// The in-progress input buffer holds text.
    input_dirty: bool,
    /// Nothing has repainted the footer for [`HEARTBEAT_IDLE`].
    idle_elapsed: bool,
    /// The terminal was resized since the last footer paint.
    size_changed: bool,
    /// A pinned escalation banner is animating and needs frame advances.
    animating: bool,
}

/// Decide whether this heartbeat tick should repaint the footer.
///
/// THE BUG THIS FIXES: the heartbeat used to require `reading_line` — it only
/// self-healed the footer while the REPL was parked at the prompt. So for the
/// entire duration of a turn (model thinking + tool calls), nothing repainted
/// the statusline. Anything that scrolled it out of view or overwrote it during
/// those seconds-to-minutes — a mouse-wheel/trackpad scroll (which sends aish no
/// input at all), a window resize leaving DECSTBM stale, a full-screen program's
/// leftovers — left the footer gone until the turn ENDED and the next prompt
/// painted it. Hence "the statusline keeps disappearing while thinking".
///
/// The `reading_line` requirement was never about safety; the repaint is
/// cursor-safe (DECSC/DECRC in `footer_seq`) and mid-turn the typed-ahead line
/// lives in `MIDTURN_INPUT`, which the repaint itself draws. The one genuine
/// hazard is rustyline rendering an in-progress line: that is `input_dirty`, and
/// it only applies AT the prompt. So `input_dirty` is now scoped to
/// `reading_line` instead of blocking unconditionally, and the heartbeat keeps
/// healing the footer straight through a turn.
fn heartbeat_should_paint(s: HeartbeatState) -> bool {
    // No region to paint, or someone else owns the screen.
    if !s.region_active || s.attach_active {
        return false;
    }
    // Never repaint over a line the user is mid-editing: a non-empty buffer
    // means rustyline owns the visible cursor and a racing repaint would eat the
    // prompt. Mid-turn rustyline is NOT rendering, so the flag is stale there.
    if s.reading_line && s.input_dirty {
        return false;
    }
    // Idle-timed-out heals a scrolled-away footer; the resize and animation
    // cases bypass the idle gate so the footer tracks a new canvas size within
    // one tick and a pinned banner's emoji advances smoothly.
    s.idle_elapsed || s.size_changed || s.animating
}

/// Spawn the footer heartbeat thread (idempotent — only the first call spawns).
/// The thread wakes on a short cadence and repaints the cached footer whenever
/// [`heartbeat_should_paint`] says so — self-healing a footer that scrolled out
/// of view, at the prompt AND mid-turn. No-op unless a footer region is
/// installed; safe to call once at REPL startup.
pub fn spawn_footer_heartbeat() {
    if HEARTBEAT_SPAWNED.swap(true, Ordering::Relaxed) {
        return;
    }
    std::thread::Builder::new()
        .name("aish-footer-heartbeat".into())
        .spawn(|| {
            // Poll well under HEARTBEAT_IDLE so the actual repaint lands within
            // ~a fifth of a second of the 3s idle mark. This cadence also drives
            // the pinned-escalation animation, so it must be ≤ the banner's
            // frame interval or the emoji would visibly stutter.
            let tick = Duration::from_millis(crate::escalation::FRAME_MS.min(500));
            let idle_ms = HEARTBEAT_IDLE.as_millis() as u64;
            loop {
                std::thread::sleep(tick);
                let idle = heartbeat_now_ms()
                    .saturating_sub(LAST_FOOTER_ACTIVITY_MS.load(Ordering::Relaxed));
                if heartbeat_should_paint(HeartbeatState {
                    region_active: ACTIVE.load(Ordering::Relaxed),
                    attach_active: ATTACH_ACTIVE.load(Ordering::Relaxed),
                    reading_line: READING_LINE.load(Ordering::Relaxed),
                    input_dirty: INPUT_DIRTY.load(Ordering::Relaxed),
                    idle_elapsed: idle >= idle_ms,
                    size_changed: size_changed_since_paint(),
                    animating: crate::escalation::animating(),
                }) {
                    // Cursor-safe repaint (no body-home): DECSC/DECRC restores
                    // the caller's cursor exactly where it was — the in-progress
                    // input line at the prompt, or the spinner/ticker row
                    // mid-turn.
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
    if !FooterLayout::for_rows(rows).enabled() {
        return;
    }
    // Retire a finished escalation banner once its dwell has elapsed, so the
    // footer shrinks back on its own even while the shell sits idle (this path
    // is what the footer heartbeat drives).
    crate::escalation::sweep();
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
        let body_bottom = FooterLayout::for_rows(rows).body_bottom;
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
    let bottom = FooterLayout::for_rows(rows).region_bottom();
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
    let body: String = std::iter::repeat_n(ch, cols.max(1) as usize).collect();
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
    // Snapshot the pinned escalations ONCE — asking for only as many banners as
    // this window can hold — and hand them to the pure builder, so the rows we
    // reserve and the rows we paint agree even if a banner retires mid-paint.
    let keep = escalation_rows_that_fit(rows, crate::escalation::row_count())
        / crate::escalation::ROWS_PER_BANNER;
    let banners = crate::escalation::rows(crate::style::colors_enabled(), keep as usize);
    footer_seq_with(rows, cols, separator, status_msg, statusline, banners)
}

/// [`footer_seq`] with the escalation banners passed in instead of read from the
/// process-global stack — the whole row plan is a pure function of `(rows, cols,
/// banners)`, so the geometry is unit-testable without mutating shared state.
pub fn footer_seq_with(
    rows: u16,
    cols: u16,
    separator: &str,
    status_msg: &str,
    statusline: &str,
    banners: Vec<(String, String)>,
) -> String {
    // ONE row plan, solved from `(rows, banners.len())`, drives both the
    // reserved region and every painted row — see [`FooterLayout`] for why the
    // old "reserve here, recompute there" split was a desync waiting to happen.
    //
    // The footer occupies the bottom `layout.height` rows: [escalation message,
    // worker status, …per live escalation,] separator, status message,
    // statusline — minus whatever a short window made us shed.
    //
    // The pinned escalation is anchored ABOVE the separator, not below it. The
    // horizontal rule is the LID of the statusline block — it marks where the
    // scrolling body stops — so a banner painted under it looked like a row
    // wedged inside the statusline frame. Above the rule it reads as the last
    // thing the body said, which is where the operator's eye goes for "what is
    // running right now", and the rule stays welded to the statusline rows it
    // opens whether or not a banner is pinned.
    let layout = FooterLayout::solve(
        rows,
        (banners.len() as u16) * crate::escalation::ROWS_PER_BANNER,
    );
    let top_row = layout.top_row();
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
    // Read off the SAME layout as the row plan (not from the global pin) so a
    // banner that retires mid-paint can't desync region from paint.
    let region_bottom = layout.region_bottom();
    s.push_str(&format!("\x1b[1;{region_bottom}r"));
    // The pinned escalations sit ABOVE the separator — for each one the
    // (animated) escalation message then that worker's latest status, newest
    // escalation on top, then the rule that opens the statusline block.
    // `banner_count()` clamps to what the window actually granted: a caller that
    // hands us more banners than fit gets the newest ones painted, never a row
    // written outside the reserved region.
    for (i, (escalation, worker)) in banners.iter().take(layout.banner_count()).enumerate() {
        let esc_row = top_row + (i as u16) * crate::escalation::ROWS_PER_BANNER;
        let worker_row = esc_row + 1;
        s.push_str(&format!(
            "\x1b[{esc_row};1H\x1b[2K{}",
            clip_visible(escalation, max)
        ));
        s.push_str(&format!(
            "\x1b[{worker_row};1H\x1b[2K{}",
            clip_visible(worker, max)
        ));
    }
    // Each zone paints ONLY if the plan granted it a row. A degraded footer
    // (short window) silently drops the rule, then the message, and keeps the
    // statusline — rather than dropping the whole footer off a cliff.
    if let Some(sep_row) = layout.sep_row {
        s.push_str(&format!("\x1b[{sep_row};1H\x1b[2K{sep}"));
    }
    if let Some(msg_row) = layout.msg_row {
        s.push_str(&format!("\x1b[{msg_row};1H\x1b[2K{msg}"));
    }
    if let Some(bar_row) = layout.bar_row {
        s.push_str(&format!("\x1b[{bar_row};1H\x1b[2K{bar}"));
    }
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

    /// True when the terminal is tall enough to host the footer — including the
    /// DEGRADED forms (statusline-only, or rule-less), which is why this asks
    /// the solver instead of comparing against [`MIN_FOOTER_ROWS`].
    pub fn footer_enabled(&self) -> bool {
        FooterLayout::for_rows(self.rows).enabled()
    }

    /// Install the DECSTBM scroll region and drop the cursor into the body (the
    /// last scrolling row) so the next output lands above the footer. No-op when
    /// the terminal is too short.
    pub fn init_scroll_region(&mut self) {
        if !self.footer_enabled() {
            return;
        }
        let body_bottom = FooterLayout::for_rows(self.rows).body_bottom;
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
        let top_row = footer_top_row(self.rows);
        let mut out = std::io::stdout();
        // Restore alternate-scroll, reset region, then clear from the footer's
        // top row to end of screen so no stale statusline — or stale escalation
        // banner, which now sits ABOVE the separator — is left behind.
        let _ = write!(
            out,
            "{}{RESET_REGION}\x1b[{top_row};1H\x1b[J",
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
        if !FooterLayout::for_rows(self.rows).enabled() {
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
            let (msg, bar) = LAST_FOOTER.lock().map(|l| l.clone()).unwrap_or_default();
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

/// Re-sync the footer after an OS suspend/resume — the SIGCONT wake path.
///
/// A laptop that slept and woke (or a `fg` after the process was stopped) is
/// continued with SIGCONT, but nothing else tells aish the world moved: the
/// pinned footer may have scrolled out of view or lost its DECSTBM scroll
/// region while parked, and the monotonic idle clock ([`heartbeat_now_ms`],
/// backed by CLOCK_MONOTONIC) does NOT advance across a suspend, so the
/// heartbeat under-counts the true idle gap and may not self-heal for a while.
///
/// On wake we repaint the cached footer immediately — `footer_seq` re-asserts
/// the scroll region (healing a dropped margin) and its DECSC/DECRC wrapper
/// preserves the input cursor, so an in-progress prompt line is untouched — and
/// reset the idle timer so the heartbeat cadence restarts cleanly from now.
///
/// No-op-but-still-reset-the-timer when no footer region is installed, a worker
/// alt-screen view owns the terminal, or the user is mid-editing a non-empty
/// line: in those cases an immediate repaint would either do nothing useful or
/// risk clobbering the visible cursor, so the heal is deferred to the heartbeat
/// / the main loop's next idle pass while the timer is still re-armed.
pub fn resync_after_wake() {
    if !ACTIVE.load(Ordering::Relaxed)
        || ATTACH_ACTIVE.load(Ordering::Relaxed)
        || INPUT_DIRTY.load(Ordering::Relaxed)
    {
        // Re-arm the idle timer so the post-wake heartbeat waits a clean full
        // interval rather than firing off a frozen (pre-suspend) clock reading.
        note_footer_activity();
        return;
    }
    // Cursor-safe repaint: re-asserts DECSTBM and restores the input cursor.
    // paint_cached_footer records footer activity, resetting the idle timer.
    paint_cached_footer(false);
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
    let top_row = footer_top_row(rows);
    // DECSC → restore alternate-scroll (child gets normal wheel behavior) →
    // reset region to full screen → clear the footer rows (banner rows included
    // — they sit ABOVE the separator) → DECRC, so the cursor stays exactly where
    // the command line left it.
    let seq = format!(
        "\x1b7{}{RESET_REGION}\x1b[{top_row};1H\x1b[J\x1b8",
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
/// This is the BARE reclaim — it assumes nothing of the child's output has
/// landed in the rows the footer is about to repaint. Callers that can measure
/// the cursor should use [`resume_region_seq_with_cursor`], which prepends the
/// corrective scroll-up that keeps the tail of the output visible; see
/// [`footer_overflow_rows`] for the "`ls -al` loses its last few lines" bug this
/// fixes.
pub fn resume_region_seq(rows: u16) -> String {
    let body_bottom = FooterLayout::for_rows(rows).body_bottom;
    format!("{}\x1b[{body_bottom};1H", scroll_region_seq(rows))
}

/// How many rows the body must scroll UP before the footer reclaims the bottom
/// [`FOOTER_ROWS`] rows — the fix for the output-clobber on resume.
///
/// THE BUG: while a foreground child ran, the footer region was suspended to
/// the FULL screen (see [`suspend_footer_region`]), so the child's output was
/// free to scroll into the bottom `FOOTER_ROWS` rows. Resuming used to paint the
/// footer straight over those rows, silently eating the command's last 1..=3
/// lines from the viewport — `ls` (one line) looked fine, `ls -al` (many lines)
/// lost its tail. (It was NOT the off-by-one once suspected in
/// [`scroll_region_seq`]: 24 → `ESC[1;21r`, and 21 body + 3 footer = 24, pinned
/// by `scroll_region_reserves_three_bottom_rows`.)
///
/// THE FIX: lift the overflow — `cursor_row - body_bottom` — out of the footer
/// zone first. `cursor_row` is the 1-based row the child left the cursor on
/// (i.e. where the next prompt would print), measured with a DSR query by
/// [`query_cursor_row`]. Scrolling by exactly that amount is position-preserving
/// in content-space: the line under the cursor lands on `body_bottom`, so the
/// last output line ends up at `body_bottom - 1` and nothing is repainted over.
///
/// The result is clamped to `0..=FOOTER_ROWS`. Zero when the cursor is already
/// at or above the last body row — which is exactly the alt-screen case (`vim`,
/// `less` restore the pre-launch cursor on exit), so a full-screen app never
/// jerks the view. The upper clamp is defensive: `cursor_row <= rows` already
/// caps the overflow at `FOOTER_ROWS`.
///
/// An UNCONDITIONAL scroll-up would NOT work here — the prompt is
/// bottom-anchored, so nearly every command leaves the cursor in the footer
/// zone and a fixed scroll would jerk the screen on every single command. The
/// measured overflow is what makes this safe.
pub fn footer_overflow_rows(rows: u16, cursor_row: u16) -> u16 {
    FooterLayout::for_rows(rows).overflow_rows(cursor_row)
}

/// [`resume_region_seq`] preceded by the corrective scroll-up derived from the
/// measured cursor row (see [`footer_overflow_rows`]). `SU` (`ESC[nS`) is issued
/// BEFORE the DECSTBM re-assert, while the region is still the full screen, so
/// it scrolls the whole viewport and the lifted rows land in the terminal's
/// native scrollback.
///
/// `cursor_row: None` — the terminal did not answer the DSR query, or querying
/// was disabled — degrades to the exact byte sequence [`resume_region_seq`]
/// emitted before this fix, so a non-conforming terminal is never worse off.
/// Kept pure (no I/O, no shared state) so the choreography is unit-testable
/// byte-for-byte.
pub fn resume_region_seq_with_cursor(rows: u16, cursor_row: Option<u16>) -> String {
    let scroll = match cursor_row.map(|cr| footer_overflow_rows(rows, cr)) {
        None | Some(0) => String::new(),
        Some(n) => format!("\x1b[{n}S"),
    };
    format!("{scroll}{}", resume_region_seq(rows))
}

/// Body rows a GROWING footer is about to STEAL, when the pinned escalation
/// block goes from wanting `prev_want_rows` to `next_want_rows` on a
/// `rows`-tall window.
///
/// Pinning an escalation lifts `body_bottom` UP by two rows per banner. Pure —
/// both layouts come from [`FooterLayout::solve`], so this never disagrees with
/// the region arithmetic or the paint. Zero when the growth was shed by a short
/// window (the solver refused the extra banner) or nothing grew.
pub fn banner_growth_rows(rows: u16, prev_want_rows: u16, next_want_rows: u16) -> u16 {
    let before = FooterLayout::solve(rows, prev_want_rows).body_bottom;
    let after = FooterLayout::solve(rows, next_want_rows).body_bottom;
    before.saturating_sub(after)
}

/// The choreography that lifts the body OUT of the `grow` rows a growing footer
/// is about to claim — the mid-turn twin of [`resume_region_seq_with_cursor`].
///
/// THE BUG: when a banner pins mid-turn the footer grows and `body_bottom`
/// moves up two rows, but nothing moves the CURSOR, which was sitting on (or
/// near) the old last body row. It is now INSIDE the escalation tray's rows, and
/// every body write that follows — most visibly the in-place animated
/// `thinking…` row, which rewrites its own line on a cadence — paints straight
/// onto the banner rows. The footer repaint and the spinner then fight over the
/// same cells, so "thinking" bleeds into the escalations tray.
///
/// THE FIX: `grow` line feeds, issued while the OLD (taller) region is still
/// installed, scroll the body up by exactly the rows the footer is taking —
/// the lifted rows land in native scrollback — and `CUU grow` (`ESC[nA`) puts
/// the cursor back on the content line it started on, which is now
/// `new body_bottom` or above. LF (no CR) keeps the column, so a half-written
/// line is not broken. When the cursor is far above the footer no scroll
/// happens: the line feeds just open blank rows below it that the footer paints
/// over, and the CUU restores the position either way — so this needs no DSR
/// round trip to stay position-preserving.
///
/// Pure, so the sequence is unit-testable byte-for-byte. Empty string when
/// `grow == 0` (write nothing at all).
pub fn banner_growth_seq(grow: u16) -> String {
    if grow == 0 {
        return String::new();
    }
    format!("{}\x1b[{grow}A", "\n".repeat(grow as usize))
}

/// Runtime entry point for [`banner_growth_seq`]: absorb the body rows the
/// footer takes when the escalation block grows from `prev_want_rows` to
/// `next_want_rows`. Called by [`crate::escalation::pin`] the moment a banner
/// lands, BEFORE the next footer paint installs the shorter region.
///
/// No-op unless a footer region is actually installed, and skipped while a
/// worker view owns the terminal (`ATTACH_ACTIVE`) or the operator is mid-edit
/// on a non-empty prompt line (`INPUT_DIRTY`) — in those states the line feeds
/// would disturb a cursor this module doesn't own, and the next full repaint
/// heals the layout anyway. Same gating shape as [`resync_after_wake`].
pub fn absorb_banner_growth(prev_want_rows: u16, next_want_rows: u16) {
    if !ACTIVE.load(Ordering::Relaxed)
        || ATTACH_ACTIVE.load(Ordering::Relaxed)
        || INPUT_DIRTY.load(Ordering::Relaxed)
    {
        return;
    }
    let Some((rows, _cols)) = term_size() else {
        return;
    };
    let seq = banner_growth_seq(banner_growth_rows(rows, prev_want_rows, next_want_rows));
    if seq.is_empty() {
        return;
    }
    let mut out = std::io::stdout();
    let _ = write!(out, "{seq}");
    let _ = out.flush();
}

/// Parse a DSR cursor-position reply — `ESC [ row ; col R` — out of a raw read
/// buffer, returning the 1-based `(row, col)`.
///
/// Tolerant by design: the reply is scanned for anywhere in the buffer (a
/// keystroke the operator typed in the instant between the child exiting and the
/// query landing shares the same input stream), the `ESC [ ? … R` extended form
/// some terminals answer with is accepted, and when several complete replies are
/// present the LAST one wins (freshest position). Returns `None` for a buffer
/// with no complete reply, which the caller treats as "terminal didn't answer".
fn parse_dsr_reply(buf: &[u8]) -> Option<(u16, u16)> {
    let digits_end = |from: usize| {
        let mut k = from;
        while buf.get(k).is_some_and(u8::is_ascii_digit) {
            k += 1;
        }
        k
    };
    let mut found = None;
    let mut i = 0usize;
    while i + 1 < buf.len() {
        if buf[i] != 0x1b || buf[i + 1] != b'[' {
            i += 1;
            continue;
        }
        // Optional private-parameter marker in the extended reply form.
        let row_start = i + 2 + usize::from(buf.get(i + 2) == Some(&b'?'));
        let row_end = digits_end(row_start);
        if row_end == row_start || buf.get(row_end) != Some(&b';') {
            i += 1;
            continue;
        }
        let col_start = row_end + 1;
        let col_end = digits_end(col_start);
        if col_end == col_start || buf.get(col_end) != Some(&b'R') {
            i += 1;
            continue;
        }
        let num = |r: std::ops::Range<usize>| {
            std::str::from_utf8(&buf[r])
                .ok()
                .and_then(|s| s.parse::<u16>().ok())
        };
        if let (Some(row), Some(col)) = (num(row_start..row_end), num(col_start..col_end)) {
            found = Some((row, col));
        }
        i = col_end + 1;
    }
    found
}

/// Ask the terminal where the cursor is (DSR, `ESC[6n`) and return the 1-based
/// row. Used by [`resume_footer_region`] to size the corrective scroll-up.
///
/// SAFETY RAILS — this runs on every foreground-command exit, so it must never
/// hang the shell and must never leave the tty in a strange mode:
/// * hard-bounded: at most [`CURSOR_QUERY_TIMEOUT`] of `poll(2)` across the
///   whole exchange, and at most [`CURSOR_QUERY_MAX_BYTES`] consumed;
/// * `None` on ANY doubt (not a tty, `tcgetattr`/`tcsetattr` failure, write
///   failure, timeout, EOF, unparseable reply) — the caller then emits the
///   pre-fix sequence, so a terminal that ignores DSR simply keeps the old
///   behavior;
/// * termios is snapshotted and restored on every exit path (the query needs
///   `ICANON`/`ECHO` off so the reply isn't line-buffered or echoed as visible
///   garbage);
/// * opt-out via `AISH_NO_CURSOR_QUERY=1` for anyone on a terminal where the
///   probe misbehaves.
///
/// Known tradeoff: bytes that arrive ahead of the reply are consumed with it.
/// The window is the few hundred microseconds between the child being reaped and
/// the probe, so type-ahead loss is a theoretical rather than practical concern —
/// and it is the same tradeoff every shell that probes the cursor makes.
fn query_cursor_row() -> Option<u16> {
    if std::env::var_os("AISH_NO_CURSOR_QUERY").is_some() {
        return None;
    }
    // SAFETY: plain isatty queries on the shell's own stdin/stdout.
    if unsafe { libc::isatty(0) } != 1 || unsafe { libc::isatty(1) } != 1 {
        return None;
    }
    // SAFETY: termios is POD; tcgetattr fills it or reports failure.
    let mut saved: libc::termios = unsafe { std::mem::zeroed() };
    if unsafe { libc::tcgetattr(0, &mut saved) } != 0 {
        return None;
    }
    let mut raw = saved;
    raw.c_lflag &= !(libc::ICANON | libc::ECHO);
    raw.c_cc[libc::VMIN] = 0;
    raw.c_cc[libc::VTIME] = 0;
    // SAFETY: applying a minimally-modified copy of the attributes just read.
    if unsafe { libc::tcsetattr(0, libc::TCSANOW, &raw) } != 0 {
        return None;
    }

    let answer = read_dsr_reply();

    // SAFETY: restoring the exact attributes captured above, on every path.
    unsafe { libc::tcsetattr(0, libc::TCSANOW, &saved) };
    answer.map(|(row, _col)| row)
}

/// Write `ESC[6n` and read back the reply under the bounds documented on
/// [`query_cursor_row`]. Split out so the caller owns termios save/restore and
/// this body can return early freely.
fn read_dsr_reply() -> Option<(u16, u16)> {
    let mut out = std::io::stdout();
    write!(out, "\x1b[6n").ok()?;
    out.flush().ok()?;

    let deadline = Instant::now() + CURSOR_QUERY_TIMEOUT;
    let mut buf: Vec<u8> = Vec::with_capacity(32);
    let mut chunk = [0u8; 32];
    loop {
        if let Some(rc) = parse_dsr_reply(&buf) {
            return Some(rc);
        }
        if buf.len() >= CURSOR_QUERY_MAX_BYTES {
            return None;
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return None;
        }
        let mut pfd = libc::pollfd {
            fd: 0,
            events: libc::POLLIN,
            revents: 0,
        };
        let ms = left.as_millis().min(i32::MAX as u128) as libc::c_int;
        // SAFETY: single-entry pollfd array on the shell's stdin, bounded wait.
        if unsafe { libc::poll(&mut pfd, 1, ms) } <= 0 {
            return None; // timeout, EINTR or error → fall back to the old path
        }
        // SAFETY: read into a stack buffer of exactly `chunk.len()` bytes.
        let n = unsafe { libc::read(0, chunk.as_mut_ptr().cast(), chunk.len()) };
        if n <= 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..n as usize]);
    }
}

/// Re-establish the footer scroll region and repaint the cached footer after a
/// foreground child that inherited the full terminal exits. Pairs with
/// [`suspend_footer_region`]. Homes the cursor into the last body row so the
/// next prompt grows up from just above the footer (mirrors
/// [`Terminal::init_scroll_region`]). No-op when the terminal is now too short
/// to host the footer (e.g. it was resized smaller while the child ran).
///
/// Before reclaiming the rows it measures where the child left the cursor
/// ([`query_cursor_row`]) and scrolls any output that spilled into the footer
/// zone clear of it — see [`footer_overflow_rows`] for the choreography and the
/// output-clobber bug it fixes. The measurement happens BEFORE the first byte of
/// the resume sequence is written, while the region is still full-screen.
pub fn resume_footer_region() {
    let Some((rows, _cols)) = term_size() else {
        return;
    };
    if !FooterLayout::for_rows(rows).enabled() {
        return;
    }
    // Measure first: how far did the child's output run into the rows the
    // footer is about to repaint? `None` (terminal ignored the DSR query, or it
    // was disabled) degrades to the pre-fix behavior.
    let cursor_row = query_cursor_row();
    // Scroll the overflow clear, re-assert the region + home into the last body
    // row, and re-suppress alternate-scroll alongside it (cursor-neutral:
    // `suppress_alt_scroll_seq` only toggles private mode 1007, so its position
    // relative to the home move is immaterial; it also returns "" after the
    // first suppression so the saved original setting is preserved).
    let seq = format!(
        "{}{}",
        resume_region_seq_with_cursor(rows, cursor_row),
        suppress_alt_scroll_seq()
    );
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
        if let Ok(v) = std::env::var(key)
            && !v.is_empty()
        {
            let up = v.to_ascii_uppercase();
            return up.contains("UTF-8") || up.contains("UTF8");
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
    fn dsr_reply_parses_row_and_column() {
        // The canonical answer to ESC[6n: ESC [ row ; col R, 1-based.
        assert_eq!(parse_dsr_reply(b"\x1b[12;34R"), Some((12, 34)));
        assert_eq!(parse_dsr_reply(b"\x1b[1;1R"), Some((1, 1)));
        // The extended `ESC [ ? … R` form some terminals answer with.
        assert_eq!(parse_dsr_reply(b"\x1b[?24;80R"), Some((24, 80)));
    }

    #[test]
    fn dsr_reply_survives_interleaved_input() {
        // A keystroke that landed in the same read as the reply must not
        // defeat the parse — the reply is found anywhere in the buffer.
        assert_eq!(parse_dsr_reply(b"q\x1b[12;34R"), Some((12, 34)));
        assert_eq!(parse_dsr_reply(b"\x1b[12;34Rq"), Some((12, 34)));
        // Several replies → the freshest (last complete) position wins.
        assert_eq!(parse_dsr_reply(b"\x1b[1;1R\x1b[9;5R"), Some((9, 5)));
    }

    #[test]
    fn dsr_reply_rejects_incomplete_or_absent() {
        // Nothing usable → None, which the caller reads as "terminal didn't
        // answer" and falls back to the pre-fix resume sequence.
        assert_eq!(parse_dsr_reply(b""), None);
        assert_eq!(parse_dsr_reply(b"hello"), None);
        assert_eq!(parse_dsr_reply(b"\x1b[12;34"), None); // truncated: no final R
        assert_eq!(parse_dsr_reply(b"\x1b[12R"), None); // no ; col
        assert_eq!(parse_dsr_reply(b"\x1b[;34R"), None); // empty row
        assert_eq!(parse_dsr_reply(b"\x1b[12;R"), None); // empty col
    }

    #[test]
    fn footer_overflow_is_zero_above_the_body_floor() {
        // 24-row terminal → body is rows 1..=21. A cursor at or above the last
        // body row needs no scroll: nothing of the output is sitting in the
        // rows the footer is about to reclaim. This is the alt-screen case —
        // vim/less restore the pre-launch cursor, so leaving them never jerks
        // the viewport.
        // Solved explicitly rather than through `footer_overflow_rows`, which
        // reads the LIVE escalation stack: banner rows shrink the body, and a
        // sibling test pinning a banner would otherwise flip these numbers.
        let plan = FooterLayout::solve(24, 0);
        assert_eq!(plan.overflow_rows(21), 0);
        assert_eq!(plan.overflow_rows(10), 0);
        assert_eq!(plan.overflow_rows(1), 0);
    }

    #[test]
    fn footer_overflow_measures_rows_spilled_into_the_footer() {
        // THE BUG, in numbers: `ls -al` on a 24-row terminal left the cursor at
        // row 24 while the region was suspended, so 3 rows of output sat under
        // the footer and got painted over. Lift exactly that many.
        let plan = FooterLayout::solve(24, 0);
        assert_eq!(plan.overflow_rows(22), 1);
        assert_eq!(plan.overflow_rows(23), 2);
        assert_eq!(plan.overflow_rows(24), 3);
        // Never more than the footer's own height — that is all it can hide.
        assert_eq!(plan.overflow_rows(99), FOOTER_ROWS);
        // Tiny terminals read the SAME solved plan as scroll_region_seq, so the
        // two never disagree about where the body ends. A 3-row window runs a
        // DEGRADED 1-row footer (statusline only), so exactly one row can be
        // hidden — not FOOTER_ROWS' worth.
        assert_eq!(FooterLayout::solve(3, 0).height, 1);
        assert_eq!(FooterLayout::solve(3, 0).overflow_rows(3), 1);
    }

    #[test]
    fn banner_growth_equals_the_body_rows_the_footer_takes() {
        // 24-row window: no banner → body ends at 21; one banner → 19. The two
        // rows the tray claims are exactly what the body must give up.
        assert_eq!(FooterLayout::solve(24, 0).body_bottom, 21);
        assert_eq!(FooterLayout::solve(24, 2).body_bottom, 19);
        assert_eq!(banner_growth_rows(24, 0, 2), 2);
        // Second banner takes two more; the first→second step is still 2.
        assert_eq!(banner_growth_rows(24, 2, 4), 2);
        assert_eq!(banner_growth_rows(24, 0, 4), 4);
        // Shrink (a banner retired) is NOT growth — the body gains rows, which
        // leaves a harmless gap, so nothing is scrolled.
        assert_eq!(banner_growth_rows(24, 4, 0), 0);
        assert_eq!(banner_growth_rows(24, 2, 2), 0);
    }

    #[test]
    fn banner_growth_tracks_what_the_solver_actually_granted() {
        // THE invariant that keeps the scroll honest on every window size: the
        // rows we lift the body by must equal the banner rows the layout
        // granted — so on a short window where the solver SHEDS the banner, the
        // body is not scrolled for a tray that was never painted.
        for rows in 1..=60u16 {
            let granted = FooterLayout::solve(rows, 2).banner_rows;
            assert_eq!(
                banner_growth_rows(rows, 0, 2),
                granted,
                "growth disagrees with the granted banner rows at {rows} rows"
            );
        }
    }

    #[test]
    fn banner_growth_seq_scrolls_then_walks_the_cursor_back_up() {
        // Order is load-bearing: the line feeds scroll the body up while the
        // OLD (taller) region is still installed, then CUU returns the cursor
        // to the same CONTENT line — now inside the shrunken body. LF with no
        // CR, so a half-written line keeps its column.
        assert_eq!(banner_growth_seq(2), "\n\n\x1b[2A");
        assert_eq!(banner_growth_seq(1), "\n\x1b[1A");
        assert_eq!(banner_growth_seq(4), "\n\n\n\n\x1b[4A");
        // Nothing grew → write NOTHING (never nudge the body for a no-op).
        assert_eq!(banner_growth_seq(0), "");
        // The feeds always precede the cursor-up, and the counts match.
        let seq = banner_growth_seq(3);
        assert!(seq.find('\n').unwrap() < seq.find('\x1b').unwrap());
        assert_eq!(seq.matches('\n').count(), 3);
    }

    #[test]
    fn resume_lifts_overflow_before_reclaiming_the_rows() {
        // Order is load-bearing: SU (ESC[nS) must come BEFORE the DECSTBM
        // re-assert, while the region is still full-screen, so the whole
        // viewport scrolls and the lifted rows reach native scrollback.
        let seq = resume_region_seq_with_cursor(24, Some(24));
        assert_eq!(seq, "\x1b[3S\x1b[1;21r\x1b[21;1H");
        assert!(seq.find("\x1b[3S").unwrap() < seq.find("\x1b[1;21r").unwrap());

        // One spilled row scrolls one row.
        assert_eq!(
            resume_region_seq_with_cursor(24, Some(22)),
            "\x1b[1S\x1b[1;21r\x1b[21;1H"
        );
    }

    #[test]
    fn resume_without_a_cursor_answer_matches_pre_fix_bytes() {
        // Terminal ignored the DSR query (or AISH_NO_CURSOR_QUERY is set):
        // emit exactly what shipped before this fix, byte for byte. A
        // non-conforming terminal is never made worse.
        assert_eq!(
            resume_region_seq_with_cursor(24, None),
            resume_region_seq(24)
        );
        assert_eq!(
            resume_region_seq_with_cursor(24, None),
            "\x1b[1;21r\x1b[21;1H"
        );
        // Cursor already clear of the footer zone → also no scroll emitted.
        assert_eq!(
            resume_region_seq_with_cursor(24, Some(21)),
            resume_region_seq(24)
        );
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
        // Degenerate tiny sizes still emit a valid (row >= 1) region, and the
        // bottom margin always matches the solved plan's body_bottom.
        // 3 rows: degraded 1-row footer → body is rows 1..=2.
        assert_eq!(scroll_region_seq(3), "\x1b[1;2r");
        // 1 row: nothing fits, so the footer is dropped entirely and the single
        // row stays a scrolling body row rather than becoming an invalid region.
        assert_eq!(scroll_region_seq(1), "\x1b[1;1r");
        assert_eq!(FooterLayout::solve(1, 0).height, 0);
    }

    #[test]
    fn resume_region_reasserts_then_homes_last_body_row() {
        // 24-row terminal: re-assert region 1..=21 then home into row 21 (the
        // last body row) so the next prompt grows up from just above the footer.
        assert_eq!(resume_region_seq(24), "\x1b[1;21r\x1b[21;1H");
    }

    #[test]
    fn resume_region_clamps_tiny_terminals() {
        // Degenerate heights never emit an invalid ESC[0;1H — the home row is
        // the solved body_bottom, which is floored at 1.
        // 3 rows: degraded footer owns row 3, body is 1..=2, home row 2.
        assert_eq!(resume_region_seq(3), "\x1b[1;2r\x1b[2;1H");
        // 1 row: no footer fits, so the lone row is the body and the home row.
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
        // No banner → 3-row footer.
        let seq = footer_seq_with(24, 10, "----------", "msg", "bar", vec![]);
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
    fn pinned_escalation_is_anchored_above_the_separator() {
        // Regression: the banner first shipped BELOW the separator, which read
        // as a row wedged inside the statusline frame. The rule is the LID of
        // the statusline block, so the banner must sit ABOVE it.
        let banner = vec![(
            "🚀 escalated → w_a7k3m2 · build and open pr".to_string(),
            "   ↳ coordinating · 1m12s".to_string(),
        )];

        // 24-row window, 5-row footer: banner 20-21, rule 22, msg 23, bar 24.
        let seq = footer_seq_with(24, 80, "----------", "msg", "bar", banner);
        let esc = seq.find("\x1b[20;1H").expect("escalation row = H-4");
        let worker = seq.find("\x1b[21;1H").expect("worker status row = H-3");
        let rule = seq.find("\x1b[22;1H").expect("separator row = H-2");
        let msg = seq.find("\x1b[23;1H").expect("status message row = H-1");
        let bar = seq.find("\x1b[24;1H").expect("statusline row = H");
        assert!(
            esc < worker && worker < rule && rule < msg && msg < bar,
            "footer must paint escalation → worker → rule → message → statusline"
        );
        // The rule really is the lid: row H-2 carries the separator, and the
        // escalation text lands two rows ABOVE it.
        assert!(
            seq[rule..msg].contains("----------"),
            "row H-2 must carry the separator, got {:?}",
            &seq[rule..msg]
        );
        assert!(
            seq[esc..worker].contains("escalated"),
            "row H-4 must carry the escalation message, got {:?}",
            &seq[esc..worker]
        );
        // The reserved region grew with the taller footer (24 - 5 = 19), so the
        // banner can never be scrolled away by body output.
        assert!(
            seq.contains("\x1b[1;19r"),
            "DECSTBM must reserve the banner rows too"
        );
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
    fn resync_after_wake_resets_idle_timer_when_inactive() {
        let _g = FOOTER_STATE_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        // No footer region installed in the test harness (ACTIVE stays false),
        // so resync_after_wake takes the no-op-but-rearm branch: it must still
        // reset the idle timer so a post-suspend heartbeat waits a clean
        // interval instead of firing off the frozen pre-suspend clock reading.
        assert!(!ACTIVE.load(Ordering::Relaxed));
        LAST_FOOTER_ACTIVITY_MS.store(0, Ordering::Relaxed);
        resync_after_wake();
        let idle =
            heartbeat_now_ms().saturating_sub(LAST_FOOTER_ACTIVITY_MS.load(Ordering::Relaxed));
        assert!(
            idle < HEARTBEAT_IDLE.as_millis() as u64,
            "wake re-sync must re-arm the idle timer, got {idle}ms"
        );
    }

    #[test]
    fn set_reading_line_toggles_and_arms_timer() {
        let _g = FOOTER_STATE_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        // Entering a read marks the idle-at-prompt window AND refreshes the
        // timer (so the first heartbeat waits a full interval at a new prompt).
        set_reading_line(true);
        assert!(READING_LINE.load(Ordering::Relaxed));
        let idle =
            heartbeat_now_ms().saturating_sub(LAST_FOOTER_ACTIVITY_MS.load(Ordering::Relaxed));
        assert!(idle < HEARTBEAT_IDLE.as_millis() as u64);
        // Leaving the read drops the at-prompt marker (the heartbeat keeps
        // painting mid-turn — see heartbeat_paints_mid_turn).
        set_reading_line(false);
        assert!(!READING_LINE.load(Ordering::Relaxed));
    }

    /// Baseline heartbeat state: region live, nothing else going on, and the
    /// idle timer already elapsed — i.e. "should paint".
    fn beat() -> HeartbeatState {
        HeartbeatState {
            region_active: true,
            attach_active: false,
            reading_line: false,
            input_dirty: false,
            idle_elapsed: true,
            size_changed: false,
            animating: false,
        }
    }

    #[test]
    fn heartbeat_paints_mid_turn() {
        // THE REGRESSION: mid-turn (reading_line == false) the heartbeat used to
        // bail unconditionally, so a footer scrolled away during a long thinking
        // phase stayed gone until the turn ended. It must now self-heal.
        assert!(heartbeat_should_paint(HeartbeatState {
            reading_line: false,
            ..beat()
        }));
        // A resize or a live banner mid-turn also repaints, idle gate or not.
        assert!(heartbeat_should_paint(HeartbeatState {
            reading_line: false,
            idle_elapsed: false,
            size_changed: true,
            ..beat()
        }));
        assert!(heartbeat_should_paint(HeartbeatState {
            reading_line: false,
            idle_elapsed: false,
            animating: true,
            ..beat()
        }));
        // Stale INPUT_DIRTY from the previous prompt must NOT suppress a
        // mid-turn repaint — rustyline isn't rendering, so there's no line to
        // clobber (mid-turn type-ahead rides in MIDTURN_INPUT, which the
        // repaint itself draws).
        assert!(heartbeat_should_paint(HeartbeatState {
            reading_line: false,
            input_dirty: true,
            ..beat()
        }));
    }

    #[test]
    fn heartbeat_never_clobbers_an_in_progress_prompt_line() {
        // At the prompt with text in the buffer, rustyline owns the cursor.
        assert!(!heartbeat_should_paint(HeartbeatState {
            reading_line: true,
            input_dirty: true,
            ..beat()
        }));
        // …not even for a resize or an animating banner.
        assert!(!heartbeat_should_paint(HeartbeatState {
            reading_line: true,
            input_dirty: true,
            idle_elapsed: false,
            size_changed: true,
            animating: true,
            ..beat()
        }));
        // Empty buffer at the prompt: free to heal.
        assert!(heartbeat_should_paint(HeartbeatState {
            reading_line: true,
            input_dirty: false,
            ..beat()
        }));
    }

    #[test]
    fn heartbeat_yields_the_screen_to_other_owners() {
        // No region installed — nothing to paint. `suspend_footer_region`
        // clears ACTIVE, so this also covers a foreground TTY child (vim/sudo).
        assert!(!heartbeat_should_paint(HeartbeatState {
            region_active: false,
            size_changed: true,
            animating: true,
            ..beat()
        }));
        // A worker alt-screen view owns the terminal.
        assert!(!heartbeat_should_paint(HeartbeatState {
            attach_active: true,
            size_changed: true,
            animating: true,
            ..beat()
        }));
    }

    #[test]
    fn heartbeat_holds_still_when_nothing_changed() {
        // Idle not elapsed, no resize, no animation ⇒ no repaint. Keeps the
        // mid-turn relaxation from turning into a 2 Hz footer rewrite.
        assert!(!heartbeat_should_paint(HeartbeatState {
            idle_elapsed: false,
            ..beat()
        }));
    }

    #[test]
    fn input_dirty_flag_round_trips_and_read_clears_it() {
        let _g = FOOTER_STATE_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
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
        let _g = FOOTER_STATE_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
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
        let _g = FOOTER_STATE_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
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
    fn midturn_queued_seq_announces_queue_position_and_clips() {
        // A line submitted mid-turn must leave a DURABLE receipt in the text
        // output area — the footer echo is wiped on Enter, so this row is the
        // only thing telling the operator their command was accepted and will
        // run after the turn. Locks in the format, the queue-depth marker, the
        // ASCII/no-color degradations, and the no-wrap clip.
        let wide = 80u16;

        // First submission: no "#N" marker (queue depth of one needs no number).
        assert_eq!(
            midturn_queued_seq("ls -la", 1, wide, true, false),
            "\r\x1b[2K⏳ queued → ls -la\x1b[0m\r\n"
        );

        // Second and later: the 1-based position is surfaced so the operator can
        // see how deep the queue has grown while aish stayed busy.
        assert_eq!(
            midturn_queued_seq("git status", 2, wide, true, false),
            "\r\x1b[2K⏳ queued #2 → git status\x1b[0m\r\n"
        );

        // Colour on → the label is dimmed, the operator's own text is NOT (so it
        // reads as their input, not as chrome).
        assert_eq!(
            midturn_queued_seq("ls", 1, wide, true, true),
            "\r\x1b[2K\x1b[2m⏳ queued →\x1b[0m ls\x1b[0m\r\n"
        );

        // Non-UTF-8 locale → ASCII sigil + arrow, no mojibake.
        assert_eq!(
            midturn_queued_seq("ls", 3, wide, false, false),
            "\r\x1b[2K[q] queued #3 -> ls\x1b[0m\r\n"
        );

        // Long input is clipped to the terminal width so it can never wrap and
        // scroll the body by two rows (which would desync the footer region).
        let long = "x".repeat(200);
        let out = midturn_queued_seq(&long, 1, 20, true, false);
        let visible = out
            .trim_start_matches("\r\x1b[2K")
            .trim_end_matches("\r\n")
            .trim_end_matches("\x1b[0m");
        // Budget is in DISPLAY COLUMNS, not chars: the hourglass is double-width,
        // so the "⏳ queued → " label costs 12 columns (2+1+6+1+1+1) and only 8 of
        // the 20 remain for the operator's text.
        let label_cols = unicode_width::UnicodeWidthStr::width("⏳ queued → ");
        assert_eq!(label_cols, 12, "label width is the clip budget we subtract");
        assert_eq!(
            visible.chars().filter(|c| *c == 'x').count(),
            20 - label_cols,
            "body is clipped to exactly `cols` visible columns"
        );
        assert!(out.ends_with("\r\n"), "row is newline-terminated");

        // A zero-width terminal must not panic (cols.max(1) floor).
        let _ = midturn_queued_seq("ls", 1, 0, true, true);
    }

    #[test]
    fn midturn_now_seq_is_visibly_distinct_from_the_queued_receipt() {
        // The whole point of the immediate path is that the operator can tell at
        // a glance their `:dispatch` fired NOW and is running alongside the turn
        // — not that it is waiting in line behind it. If these two receipts ever
        // render the same, that signal is lost.
        let wide = 80u16;

        assert_eq!(
            midturn_now_seq(":dispatch audit the logs", wide, true, false),
            "\r\x1b[2K⚡ ran → :dispatch audit the logs\x1b[0m\r\n"
        );
        assert_eq!(
            midturn_now_seq(":dispatch go", wide, true, true),
            "\r\x1b[2K\x1b[2m⚡ ran →\x1b[0m :dispatch go\x1b[0m\r\n"
        );
        // Non-UTF-8 locale → ASCII sigil + arrow, no mojibake.
        assert_eq!(
            midturn_now_seq(":dispatch go", wide, false, false),
            "\r\x1b[2K[!] ran -> :dispatch go\x1b[0m\r\n"
        );
        // Never collides with the queued receipt for the same text.
        assert_ne!(
            midturn_now_seq(":dispatch go", wide, true, false),
            midturn_queued_seq(":dispatch go", 1, wide, true, false)
        );

        // Clipped to the terminal width so it can never wrap and desync the
        // footer region, and a zero-width terminal must not panic.
        let long = "x".repeat(200);
        let out = midturn_now_seq(&long, 20, true, false);
        let visible = out
            .trim_start_matches("\r\x1b[2K")
            .trim_end_matches("\r\n")
            .trim_end_matches("\x1b[0m");
        let label_cols = unicode_width::UnicodeWidthStr::width("⚡ ran → ");
        assert_eq!(
            visible.chars().filter(|c| *c == 'x').count(),
            20 - label_cols,
            "body is clipped to exactly `cols` visible columns"
        );
        assert!(out.ends_with("\r\n"), "row is newline-terminated");
        let _ = midturn_now_seq(":dispatch go", 0, true, true);
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
        let at = |rows: u16| {
            Terminal {
                rows,
                cols: 80,
                active: false,
                utf8: true,
            }
            .footer_enabled()
        };
        // MIN_FOOTER_ROWS and up: the FULL footer fits.
        assert!(at(MIN_FOOTER_ROWS));
        assert!(at(24));
        // Between MIN_FOOTER_ROWS_DEGRADED and MIN_FOOTER_ROWS the footer
        // DEGRADES rather than vanishing — a 4-row window keeps the statusline
        // (and the status message), a 3-row window keeps the statusline alone.
        // This is the gradient that replaced the old all-or-nothing cliff.
        assert!(at(4));
        assert!(at(MIN_FOOTER_ROWS_DEGRADED));
        // Below that, MIN_BODY_ROWS wins outright: chrome is shed to zero so
        // command output — the actual product — keeps every row it has.
        assert!(!at(MIN_BODY_ROWS));
        assert!(!at(1));
    }

    #[test]
    fn footer_degrades_by_priority_instead_of_vanishing() {
        // The shed order is load-bearing, so pin it row by row.
        // 24 rows: everything fits — rule, message, statusline, in that order
        // upward from the bottom.
        let full = FooterLayout::solve(24, 0);
        assert_eq!(full.height, FOOTER_ROWS);
        assert_eq!(
            (full.sep_row, full.msg_row, full.bar_row),
            (Some(22), Some(23), Some(24))
        );
        assert_eq!(full.body_bottom, 21);

        // 4 rows: the separator — pure chrome, zero information — sheds FIRST,
        // and the survivors pack contiguously upward so no hole is left behind.
        let tight = FooterLayout::solve(4, 0);
        assert_eq!(tight.height, 2);
        assert_eq!(tight.sep_row, None);
        assert_eq!((tight.msg_row, tight.bar_row), (Some(3), Some(4)));
        assert_eq!(tight.body_bottom, MIN_BODY_ROWS);

        // 3 rows: the transient status message sheds next (it also prints
        // inline, so nothing is actually lost), leaving the statusline.
        let bare = FooterLayout::solve(3, 0);
        assert_eq!(bare.height, 1);
        assert_eq!((bare.sep_row, bare.msg_row), (None, None));
        assert_eq!(bare.bar_row, Some(3));
        assert_eq!(bare.body_bottom, MIN_BODY_ROWS);

        // 2 rows and below: the statusline itself goes, footer height hits 0,
        // and the caller falls back to inline printing.
        let none = FooterLayout::solve(2, 0);
        assert_eq!(none.height, 0);
        assert_eq!(none.bar_row, None);
        assert!(!none.enabled());

        // MIN_BODY_ROWS is never traded away, at ANY size, and the plan always
        // accounts for exactly the whole window: footer + body == rows.
        for rows in 1..=120u16 {
            let l = FooterLayout::solve(rows, 0);
            assert!(
                l.body_bottom >= MIN_BODY_ROWS.min(rows),
                "{rows} rows starved the body: {l:?}"
            );
            assert_eq!(l.height + l.body_bottom, rows, "plan lost a row at {rows}");
        }
    }

    #[test]
    fn banners_shed_whole_and_before_any_chrome() {
        let per = crate::escalation::ROWS_PER_BANNER;
        // Roomy window: three banners fit on top of the full footer.
        let roomy = FooterLayout::solve(60, per * 3);
        assert_eq!(roomy.banner_rows, per * 3);
        assert_eq!(roomy.banner_count(), 3);
        assert_eq!(roomy.height, FOOTER_ROWS + per * 3);
        // top_row must cover the BANNER block, not just the rule — teardown
        // clears from there, and clearing from the rule would strand banners.
        assert_eq!(roomy.top_row(), 60 - roomy.height + 1);

        // A half banner is never reservable: an odd want normalizes DOWN.
        assert_eq!(FooterLayout::solve(60, per * 2 + 1).banner_rows, per * 2);

        // Squeeze: banners shed BEFORE the separator, whole units at a time,
        // and the full 3-row footer survives intact.
        let squeezed = FooterLayout::solve(MIN_FOOTER_ROWS + per, per * 4);
        assert_eq!(squeezed.banner_rows, per);
        assert!(squeezed.sep_row.is_some());
        assert!(squeezed.banner_rows.is_multiple_of(per));

        // No room for any banner → chrome is still fully intact.
        let full_only = FooterLayout::solve(MIN_FOOTER_ROWS, per * 4);
        assert_eq!(full_only.banner_rows, 0);
        assert_eq!(full_only.height, FOOTER_ROWS);
    }

    #[test]
    fn region_and_paint_cannot_disagree_at_any_size() {
        // The regression this refactor exists to prevent: the DECSTBM bottom
        // margin and the painted rows derived from two separate computations.
        // Now both read one plan, so assert the invariant exhaustively — every
        // painted row must sit strictly BELOW the scrolling region.
        for rows in 1..=200u16 {
            for want in [0u16, 2, 4, 8, 20] {
                let l = FooterLayout::solve(rows, want);
                assert_eq!(l.region_bottom(), l.body_bottom);
                for row in [l.sep_row, l.msg_row, l.bar_row].into_iter().flatten() {
                    assert!(
                        row > l.body_bottom,
                        "rows={rows} want={want}: painted row {row} is inside the body (bottom {})",
                        l.body_bottom
                    );
                    assert!(row <= rows, "rows={rows}: painted row {row} is off-screen");
                }
                // Survivors are contiguous and strictly ordered upward.
                if let (Some(s), Some(m)) = (l.sep_row, l.msg_row) {
                    assert_eq!(s + 1, m);
                }
                if let (Some(m), Some(b)) = (l.msg_row, l.bar_row) {
                    assert_eq!(m + 1, b);
                }
                // A footer that exists ALWAYS keeps the statusline.
                assert_eq!(l.enabled(), l.bar_row.is_some());
            }
        }
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
    fn every_pinned_escalation_gets_its_own_two_rows() {
        // Regression: the footer only ever reserved + painted ONE banner, so a
        // second live escalation was invisible. N banners must stack upward from
        // the rule, newest on top, with the region grown to cover all of them.
        let banners = vec![
            // All three lead with the same stable identity glyph — the head row
            // no longer animates (see `escalation::LIVE_GLYPH`); the motion is a
            // braille frame in the status row's prefix cell.
            ("🚀 newest".to_string(), "   ⠋ newest status".to_string()),
            ("🚀 middle".to_string(), "   ⠙ middle status".to_string()),
            ("🚀 oldest".to_string(), "   ⠹ oldest status".to_string()),
        ];
        // 24-row window, 3 banners → 9-row footer: rows 16..21 banners, 22 rule,
        // 23 msg, 24 bar.
        let seq = footer_seq_with(24, 80, "----------", "msg", "bar", banners);
        let newest = seq.find("\x1b[16;1H").expect("newest escalation row");
        let newest_status = seq.find("\x1b[17;1H").expect("newest status row");
        let middle = seq.find("\x1b[18;1H").expect("middle escalation row");
        let oldest = seq.find("\x1b[20;1H").expect("oldest escalation row");
        let rule = seq.find("\x1b[22;1H").expect("separator row = H-2");
        assert!(newest < newest_status && newest_status < middle && middle < oldest);
        assert!(oldest < rule, "banners must all sit above the rule");
        assert!(seq[newest..newest_status].contains("newest"));
        assert!(seq[newest_status..middle].contains("newest status"));
        assert!(seq[rule..].contains("----------"));
        // DECSTBM must reserve all nine footer rows (24 - 9 = 15).
        assert!(
            seq.contains("\x1b[1;15r"),
            "region must cover every banner row"
        );
    }

    #[test]
    fn short_window_sheds_whole_banners_never_half_of_one() {
        // A banner is an indivisible 2-row unit: the fitter must shed in pairs so
        // a status row can never end up orphaned below the rule.
        for want in [0u16, 2, 4, 6] {
            for rows in 0..30u16 {
                let fit = escalation_rows_that_fit(rows, want);
                assert_eq!(fit % crate::escalation::ROWS_PER_BANNER, 0, "{rows}/{want}");
                assert!(fit <= want);
                if fit > 0 {
                    assert!(
                        rows >= MIN_FOOTER_ROWS + fit,
                        "{rows} rows can't hold {fit} banner rows"
                    );
                }
            }
        }
        // Concretely: a 24-row window holds 3 banners; a 10-row window holds 2;
        // a 7-row window holds 1; a 5-row window holds none.
        assert_eq!(escalation_rows_that_fit(24, 6), 6);
        assert_eq!(escalation_rows_that_fit(10, 6), 4);
        assert_eq!(escalation_rows_that_fit(7, 6), 2);
        assert_eq!(escalation_rows_that_fit(5, 6), 0);
    }

    #[test]
    fn size_change_detection_flips_on_new_dims() {
        // A stored size equal to the probe ⇒ unchanged; a different one ⇒ changed.
        LAST_PAINTED_SIZE.store(pack_size(24, 80), Ordering::Relaxed);
        assert_eq!(pack_size(24, 80), LAST_PAINTED_SIZE.load(Ordering::Relaxed));
        assert_ne!(
            pack_size(30, 100),
            LAST_PAINTED_SIZE.load(Ordering::Relaxed)
        );
    }
}
