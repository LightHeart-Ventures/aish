//! Bottom-anchored statusline via a DECSTBM scroll region.
//!
//! The REPL pins a four-row footer to the very bottom of the terminal:
//!
//! ```text
//! rows 1..H-4   scrolling REPL area (command output, history)
//! row  H-3      ────────────────────────────────────────────────  (solid rule)
//! row  H-2      ❯ git stat                                       (INPUT — always here)
//! row  H-1      ⇄ attached to w_YM7YyIHV (2/2 · Shift-Tab to cycle, :detach)  (status msg)
//! row  H        aish v0.23.0 · claude (sonnet)              2026-07-01 21:15   (statusline)
//! ```
//!
//! The INPUT row is the invariant: row `H-2` — one line below the rule, third
//! from the bottom — is where the operator types, in EVERY state of the shell
//! (idle at the prompt, mid-turn type-ahead, attached to a worker, goal mode).
//! It is the last zone the degradation solver will ever shed, so the prompt
//! never moves out from under the operator's fingers. See [`FooterLayout`].
//!
//! The footer is held fixed with a DECSTBM scroll region (`ESC[top;bottomr`):
//! the region covers rows `1..=H-4`, so everything the shell prints scrolls
//! *above* the footer while rows `H-3..=H` stay put. Each [`Terminal::draw_footer`]
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
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

/// Rows reserved at the bottom for the footer: separator + input + status
/// message + statusline.
pub const FOOTER_ROWS: u16 = 4;

/// Scrolling rows the body keeps no matter how hard the footer is squeezed. The
/// footer is chrome; command output is the product — footer zones are shed to
/// protect these rows, never the other way round.
pub const MIN_BODY_ROWS: u16 = 2;

/// Minimum terminal height for the FULL footer: [`FOOTER_ROWS`] of chrome plus
/// the [`MIN_BODY_ROWS`] scrolling rows we insist on keeping above it.
pub const MIN_FOOTER_ROWS: u16 = MIN_BODY_ROWS + FOOTER_ROWS;

/// Smallest terminal height that still gets SOME footer: [`MIN_BODY_ROWS`] of
/// body plus the one un-sheddable INPUT row. Between this and
/// [`MIN_FOOTER_ROWS`] the footer DEGRADES — it drops the rule, then the status
/// message, then the statusline — instead of vanishing outright, and what
/// survives longest is the row the operator types into. See [`FooterLayout`].
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
/// | `:`-command hint tray | 1 each | regenerated on the NEXT keystroke; trims one row at a time |
/// | escalation banners | 2 each | notifications; shed WHOLE, oldest-first |
/// | separator rule | 1 | pure chrome — carries no information at all |
/// | status message | 1 | transient, and the same text also prints inline |
/// | statusline | 1 | version/model/stats/clock — informational, re-derivable |
/// | input | 1 | NEVER shed while a footer exists — the operator's typing surface |
///
/// The input row sheds LAST on purpose: a shell whose prompt relocates (or
/// disappears) under a resize is unusable, while a shell that loses its clock
/// is merely plainer. At the extreme — a 3-row window — the only chrome left is
/// the input row itself.
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
    /// Row separating pinned tray output from the scrolling text above it, and
    /// always present while any tray item is visible. Painted as a horizontal
    /// RULE (the same glyph run as [`Self::sep_row`]) so the pinned block reads
    /// as a framed panel — a rule above the trays, a rule below them — instead
    /// of floating in the transcript. Named `spacer` because that is the row's
    /// JOB (and it was blank originally); the arithmetic is unchanged by what
    /// the paint puts in it.
    pub spacer_row: Option<u16>,
    /// Rows granted to the `:`-command HINT TRAY, which stacks directly above
    /// the rule and directly BELOW the escalation block. `0` whenever no
    /// `:`-palette is open (the overwhelmingly common case) or a short window
    /// shed it. See [`set_hint_tray`].
    pub hint_rows: u16,
    /// Screen row of the horizontal rule BETWEEN the two trays — escalation
    /// banners above it, the `:`-command hint tray below it. `Some` only when
    /// BOTH trays are visible, because that is the only time there is anything
    /// to divide: it costs a row exactly when it has a job to do, and it
    /// disappears for free the moment either tray empties.
    pub tray_sep_row: Option<u16>,
    /// Screen row of the horizontal rule, or `None` when it was shed.
    pub sep_row: Option<u16>,
    /// Screen row of the status message, or `None` when it was shed.
    pub msg_row: Option<u16>,
    /// Screen row of the statusline, or `None` when it was shed.
    pub bar_row: Option<u16>,
    /// Screen row of the INPUT line — where the line editor parks its prompt and
    /// where mid-turn type-ahead paints. `None` only when no footer fits at all,
    /// because this is the last zone the solver sheds.
    pub input_row: Option<u16>,
    /// Last scrolling row — the DECSTBM bottom margin AND the body-home target.
    pub body_bottom: u16,
}

impl FooterLayout {
    /// Solve the row plan for a `rows`-tall window that WANTS `want_banner_rows`
    /// of pinned escalation. Pure — no globals, no I/O — so every window size is
    /// unit-testable without touching a real terminal.
    pub fn solve(rows: u16, want_banner_rows: u16) -> Self {
        Self::solve_with(rows, want_banner_rows, 0)
    }

    /// [`Self::solve`] for a window that ALSO wants a `want_hint_rows`-tall
    /// `:`-command hint tray.
    ///
    /// The tray lands between the escalation block and the rule, and it sheds
    /// FIRST — before even a banner — for two reasons: it is the only zone the
    /// shell rebuilds from scratch on the very next keystroke, and the palette
    /// itself already degrades gracefully (its last row collapses into a
    /// `… and N more` summary), so trimming it costs the operator the least.
    /// Unlike banners, it trims ONE ROW AT A TIME: a menu is a list, and half a
    /// list is still a useful list.
    ///
    /// Because the tray sheds before the rule, `hint_rows > 0` implies
    /// `sep_row.is_some()` — which is what makes [`Self::hint_top_row`] able to
    /// anchor the tray off the rule's row.
    pub fn solve_with(rows: u16, want_banner_rows: u16, want_hint_rows: u16) -> Self {
        let per = crate::escalation::ROWS_PER_BANNER.max(1);
        let mut hint_rows = want_hint_rows;
        // Normalize DOWN to whole banners first: a banner is an indivisible
        // 2-row unit (the escalation message + that worker's latest status), so
        // half a banner must never become reservable.
        let mut banner_rows = want_banner_rows - (want_banner_rows % per);
        let (mut sep, mut msg, mut bar, mut input) = (true, true, true, true);
        // The rule BETWEEN the trays is pure decoration, so it is the very first
        // thing a short window gives up — a row of tray CONTENT always outranks
        // a row that merely divides it.
        let mut tray_sep = true;
        // Budget = every row except the body rows we refuse to give up.
        let budget = rows.saturating_sub(MIN_BODY_ROWS);
        let height = loop {
            let h = banner_rows
                + hint_rows
                + u16::from(sep)
                + u16::from(msg)
                + u16::from(bar)
                + u16::from(input)
                + u16::from(banner_rows > 0 || hint_rows > 0)
                // The rule BETWEEN the trays: charged only while both are
                // non-empty, so it is free in every single-tray case.
                + u16::from(tray_sep && banner_rows > 0 && hint_rows > 0);
            if h <= budget {
                break h;
            }
            // Shed strictly in priority order. `input` is last, and shedding it
            // yields h == 0, which fits any budget — so the loop terminates.
            if tray_sep && banner_rows > 0 && hint_rows > 0 {
                tray_sep = false;
            } else if hint_rows > 0 {
                hint_rows -= 1;
            } else if banner_rows > 0 {
                banner_rows -= per;
            } else if sep {
                sep = false;
            } else if msg {
                msg = false;
            } else if bar {
                bar = false;
            } else {
                input = false;
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
            sep && msg && bar && input,
            rows >= MIN_FOOTER_ROWS,
            "full-footer threshold drifted from the solver at {rows} rows"
        );
        // The input row outlives every other zone: whenever ANY footer fits, the
        // operator has a place to type. This is the invariant the whole feature
        // rests on, so assert it rather than trusting the shed order to stay put.
        debug_assert_eq!(
            input,
            height > 0,
            "input row must survive exactly as long as the footer does ({rows} rows)"
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
        let input_row = take(input);
        let sep_row = take(sep);
        // The tray is anchored off the rule, so it MUST sit flush against the
        // bottom of the banner block — a gap would mean the paint wrote outside
        // the rows the region reserved.
        debug_assert!(
            hint_rows == 0
                || sep_row.map(|r| r - hint_rows)
                    == Some(
                        rows.saturating_sub(height.saturating_sub(1)).max(1)
                            + u16::from(banner_rows > 0 || hint_rows > 0)
                            + banner_rows
                            + u16::from(tray_sep && banner_rows > 0 && hint_rows > 0)
                    ),
            "hint tray must pack flush between the banners and the rule ({rows} rows)"
        );
        let spacer_row =
            (banner_rows > 0 || hint_rows > 0).then_some(rows.saturating_sub(height) + 1);
        // The between-trays rule sits directly under the banner block, which
        // itself starts one row below the top rule. Derived from `spacer_row`
        // rather than counted from the bottom so it can only ever land inside
        // the block the solver just reserved.
        let tray_sep_row = match spacer_row {
            Some(top) if tray_sep && banner_rows > 0 && hint_rows > 0 => {
                Some(top + banner_rows + 1)
            }
            _ => None,
        };
        // Both rules bracket the pinned block: the top rule is the first row the
        // footer owns, and the tray rule (when charged) sits strictly between
        // the two trays — never on top of either.
        debug_assert!(
            tray_sep_row.is_none_or(|r| Some(r) > spacer_row && sep_row.is_some_and(|s| r < s)),
            "tray rule must land strictly between the two rules ({rows} rows)"
        );
        Self {
            rows,
            height,
            spacer_row,
            banner_rows,
            hint_rows,
            tray_sep_row,
            sep_row,
            msg_row,
            bar_row,
            input_row,
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
        Self::solve_with(rows, want, hint_tray_rows())
    }

    /// True when any footer — full or degraded — fits this window.
    pub fn enabled(&self) -> bool {
        self.height > 0
    }

    /// How many WHOLE banners the plan granted.
    pub fn banner_count(&self) -> usize {
        (self.banner_rows / crate::escalation::ROWS_PER_BANNER.max(1)) as usize
    }

    /// First screen row of the `:`-command hint tray, or `None` when the plan
    /// granted it no rows. The tray runs from here for [`Self::hint_rows`] rows
    /// and ends one row above the rule.
    ///
    /// Anchored off `sep_row` rather than computed from the top, because that is
    /// the invariant the operator actually sees: the menu hangs UNDER the
    /// escalation tray and SITS ON the rule, whatever else the window shed.
    pub fn hint_top_row(&self) -> Option<u16> {
        if self.hint_rows == 0 {
            return None;
        }
        self.sep_row.map(|r| r.saturating_sub(self.hint_rows))
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

/// Mid-turn type-ahead content for the footer's INPUT row (row H-2). When
/// `Some`, the footer paints THIS string in the input row — so the operator sees
/// the line they are typing WHILE a model turn runs (thinking / mid tool-call).
/// Set/cleared by the keywatch reader thread via [`set_midturn_input`] /
/// [`clear_midturn_input`].
static MIDTURN_INPUT: Mutex<Option<String>> = Mutex::new(None);

/// The last prompt string the line editor parked on the input row (see
/// [`park_input_row`]). Cached so every footer repaint — idle heartbeat, resize,
/// post-clear restore, mid-turn status update — can re-draw the prompt affordance
/// on row H-2 even when nothing is being typed. THIS is what makes the input row
/// unconditional: the row always has content, in every state of the shell.
static LAST_PROMPT: Mutex<String> = Mutex::new(String::new());

/// The `:`-command HINT TRAY: the dim palette rows shown while the operator
/// types a `:`-command, one `String` per screen row (already styled, no
/// newlines).
///
/// ## Why a footer tray instead of an inline hint
/// The palette used to ride along as rustyline's inline hint — N newline-joined
/// rows painted immediately after the input line. That worked only while the
/// prompt lived at the END of the transcript, where there was room below it.
/// With the input line pinned to row H-2 ([`park_input_row`]) there are exactly
/// two rows beneath it, and they belong to the status message and the
/// statusline: an N-row inline hint overwrites both and then line-feeds off the
/// bottom of the screen, which scrolls the WHOLE viewport — footer included —
/// out from under rustyline's relative cursor arithmetic. The visible symptom is
/// the prompt jumping the instant `:` is typed.
///
/// Routing the menu into its own footer zone fixes it by construction: the rows
/// are reserved by [`FooterLayout::solve_with`] before they are painted, so the
/// tray grows UPWARD into the body (above the rule, below the escalation block)
/// and the input row never moves.
static HINT_TRAY: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// Ceiling on the hint tray as a fraction (`1/N`) of the window. The solver
/// already guarantees [`MIN_BODY_ROWS`] of scrolling body, but "2 rows of body"
/// is a technically-correct, practically-useless screen: a menu is a glance, not
/// a page, so cap it well short of what merely fits.
const HINT_TRAY_MAX_FRACTION: u16 = 3;

/// Rows the live hint tray currently holds. Read by [`FooterLayout::for_rows`],
/// so every region install and every paint sees the same tray height.
pub fn hint_tray_rows() -> u16 {
    HINT_TRAY
        .lock()
        .map(|t| t.len() as u16)
        .unwrap_or_else(|e| e.into_inner().len() as u16)
}

/// How many rows the hint tray may use at the LIVE terminal size, or `None` when
/// there is no footer to hang a tray on (off-tty, short window, region not yet
/// installed) — in which case the caller should keep the classic inline hint.
pub fn hint_tray_capacity() -> Option<usize> {
    if !ACTIVE.load(Ordering::Relaxed) {
        return None;
    }
    let (rows, _cols) = term_size()?;
    if !FooterLayout::solve(rows, 0).enabled() {
        return None;
    }
    // Everything the solver would hand a tray, then the "a glance, not a page"
    // cap. `max(1)` so a tray that exists is never zero rows tall.
    let fits = rows
        .saturating_sub(MIN_BODY_ROWS)
        .saturating_sub(FOOTER_ROWS + 1);
    Some(fits.min((rows / HINT_TRAY_MAX_FRACTION).max(1)) as usize)
}

/// Rows a bottom-anchored tray may claim RIGHT NOW — the solver's full grant,
/// without the "a glance, not a page" fraction [`hint_tray_capacity`] applies to
/// the `:`-command palette. `None` when there is no footer to hang a tray on
/// (off-tty, short window, region not installed), in which case the caller must
/// fall back to its own in-place rendering.
///
/// The live escalation stack is reserved FIRST (`solve_with(.., want, 0)` seats
/// the banners as if no tray existed, and we hand the tray only what is left).
/// That reservation is what lets a tall tray and a live escalation banner
/// coexist: the tray packs flush between the banners and the rule, so growing it
/// PUSHES the banner stack up with the rule instead of overwriting it.
pub fn tray_capacity_full() -> Option<usize> {
    if !ACTIVE.load(Ordering::Relaxed) {
        return None;
    }
    let (rows, _cols) = term_size()?;
    let want = if crate::escalation::active() {
        crate::escalation::row_count()
    } else {
        0
    };
    let plan = FooterLayout::solve_with(rows, want, 0);
    if !plan.enabled() {
        return None;
    }
    // `FOOTER_ROWS + 1` is the footer proper plus the top rule the solver puts
    // above a non-empty banner/tray block; `banner_rows` is the escalation stack
    // we must not sit on top of; the final row is the divider rule the solver
    // charges between the two trays, which exists exactly when banners do.
    let fits = rows
        .saturating_sub(MIN_BODY_ROWS)
        .saturating_sub(FOOTER_ROWS + 1)
        .saturating_sub(plan.banner_rows)
        .saturating_sub(u16::from(plan.banner_rows > 0));
    Some(fits.max(1) as usize)
}

/// Install `rows` as the hint tray and repaint the footer around the change.
///
/// Returns `false` when no footer region is installed — the tray does not exist
/// in that mode, and the caller is expected to fall back to the inline hint
/// rather than silently dropping the menu. Returns `true` when the tray owns the
/// menu (including when the content was already identical, which short-circuits
/// the repaint so holding a key down doesn't restorm the footer).
///
/// Safe to call from rustyline's `Hinter`, which is exactly where it runs: the
/// repaint is DECSC/DECRC-wrapped and touches only rows at or above the rule, so
/// the editor's line and cursor on row H-2 are untouched.
pub fn set_hint_tray(rows: Vec<String>) -> bool {
    if !ACTIVE.load(Ordering::Relaxed) {
        return false;
    }
    let prev = {
        let mut slot = HINT_TRAY.lock().unwrap_or_else(|e| e.into_inner());
        if *slot == rows {
            return true;
        }
        let prev = slot.len() as u16;
        *slot = rows;
        prev
    };
    repaint_hint_tray(prev);
    true
}

/// Retire the hint tray and repaint, shrinking the footer back. No-op when the
/// tray is already empty, so the common keystroke (no palette open) costs one
/// atomic load and a lock.
pub fn clear_hint_tray() {
    let prev = {
        let mut slot = HINT_TRAY.lock().unwrap_or_else(|e| e.into_inner());
        if slot.is_empty() {
            return;
        }
        let prev = slot.len() as u16;
        slot.clear();
        prev
    };
    if ACTIVE.load(Ordering::Relaxed) {
        repaint_hint_tray(prev);
    }
}

/// What a tray height change costs the body: the scroll choreography plus the
/// rows to wipe. Solved from the SAME before/after layout pair so the scroll,
/// the clear range, and the body bookkeeping can never be computed against
/// drifted globals.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TrayDelta {
    /// Rows the body must scroll UP (`SU`) before a GROWING tray paints over
    /// them. Zero when the tray shrank or held still.
    pub grow: u16,
    /// Rows a SHRINKING tray handed BACK to the body, which must therefore be
    /// scrolled DOWN (`SD`) so the transcript stays glued to the body's new
    /// bottom edge. Zero when the tray grew or held still.
    pub release: u16,
    /// First row of the stale-tray-text wipe (half-open with `clear_to`).
    pub clear_from: u16,
    /// One past the last row to wipe. `clear_to > clear_from` only when the
    /// tray shrank.
    pub clear_to: u16,
    /// The body's bottom row AFTER the change — the clamp for [`BODY_ROW`] and
    /// the DECSTBM bottom margin the re-glue must run inside.
    pub body_bottom: u16,
}

/// Body rows a tray change claims or releases, given the tray height
/// `prev_rows` the CURRENT region was installed for. Pure, so the choreography
/// below is unit-testable at every window size.
///
/// The two directions are SYMMETRIC by construction: `grow` rows lifted on the
/// way up are `release` rows pushed back on the way down. Treating the shrink
/// as "just clear some rows" is what left the body stranded N rows above the
/// rule after a palette retired.
#[cfg(test)]
pub fn hint_tray_delta(
    rows: u16,
    want_banner_rows: u16,
    prev_rows: u16,
    next_rows: u16,
) -> TrayDelta {
    let before = FooterLayout::solve_with(rows, want_banner_rows, prev_rows);
    let after = FooterLayout::solve_with(rows, want_banner_rows, next_rows);
    TrayDelta {
        grow: before.body_bottom.saturating_sub(after.body_bottom),
        release: after.body_bottom.saturating_sub(before.body_bottom),
        clear_from: before.top_row(),
        clear_to: after.top_row(),
        body_bottom: after.body_bottom,
    }
}

/// Region-scoped `SD` choreography for a SHRINKING tray: widen the scroll
/// region to the body's NEW bottom edge FIRST, then scroll the transcript down
/// into the rows the tray just gave back.
///
/// Order is load-bearing. `SD` is clipped to the CURRENT region, so emitting it
/// before the DECSTBM widen would scroll inside the old, shorter region and
/// leave the freed rows blank — exactly the gap this is here to close.
fn tray_reglue_seq(region_bottom: u16, release: u16) -> String {
    if release == 0 || region_bottom == 0 {
        return String::new();
    }
    // DECSTBM homes the cursor. Save BEFORE widening, otherwise the editor
    // resumes at row 1 instead of its input row when the palette shrinks.
    format!("\x1b7\x1b[1;{region_bottom}r\x1b[{release}T\x1b8")
}

/// Repaint the footer for a tray that just went from `prev_rows` rows to
/// whatever [`HINT_TRAY`] now holds.
///
/// ## Why `SU` and not the line-feed choreography banners use
/// [`banner_growth_seq`] scrolls the body with LFs issued FROM THE CURSOR, which
/// only works when the cursor is in the body. A tray opens while the operator is
/// mid-edit, with the cursor parked on the input row BELOW the scroll region —
/// line feeds from there would walk onto the status rows and then scroll the
/// entire screen, which is the very bug this tray exists to fix. `SU` (`ESC[nS`)
/// scrolls the region's contents regardless of where the cursor is, so the body
/// lifts and the editor's row does not move.
///
/// The trade-off is deliberate: a region-scoped `SU` discards the rows it lifts
/// past the region's top edge instead of pushing them to native scrollback. A
/// transient menu is worth a few rows of already-scrolled transcript, and the
/// alternative (full-screen scroll for scrollback's sake) would move the
/// editor's line and desync rustyline.
fn repaint_hint_tray(_prev_rows: u16) {
    paint_cached_footer(false);
}

/// What to paint on the INPUT row, or `None` to LEAVE THE ROW ALONE.
///
/// Precedence:
/// 1. `None` while the line editor is actively reading — rustyline owns the row
///    and is drawing the live buffer + cursor there; a footer repaint that
///    rewrote it would fight the editor and strand the cursor.
/// 2. The mid-turn type-ahead line, when a turn is running and the keywatch
///    thread has installed one (always at least the bare prompt sigil).
/// 3. Otherwise the cached prompt — the resting affordance, so the row is never
///    blank between turns.
fn effective_input_line() -> Option<String> {
    if READING_LINE.load(Ordering::Relaxed) {
        return None;
    }
    if let Some(s) = MIDTURN_INPUT.lock().ok().and_then(|g| g.clone()) {
        return Some(s);
    }
    Some(cached_prompt())
}

/// The resting prompt, as cached by [`park_input_row`].
fn cached_prompt() -> String {
    LAST_PROMPT
        .lock()
        .map(|p| p.clone())
        .unwrap_or_else(|e| e.into_inner().clone())
}

/// [`effective_input_line`], except a RESIZE overrides the "leave the row alone
/// while reading" rule.
///
/// THE BUG THIS FIXES: the input row is pinned at an ABSOLUTE row (H-2), but the
/// editor is blocked inside a read for nearly all of an idle shell's life — and
/// `handle_resize` only runs BETWEEN prompts. So a window resize while parked at
/// the prompt repainted the separator/message/statusline at the new bottom and
/// deliberately skipped the input row, leaving the prompt (and the cursor)
/// stranded on the OLD row: a dead prompt mid-screen on a grow, a prompt buried
/// under the footer on a shrink, and a blank input row either way. It read as
/// "aish doesn't redraw on resize".
///
/// The hazard the `None` exists for — fighting rustyline's own line render — is
/// `input_dirty`, and the heartbeat already refuses to paint at all while that is
/// set at the prompt. So when the geometry moved under a read with an EMPTY
/// buffer, repainting the resting prompt on the NEW input row is both safe and
/// the only thing that can heal it. The caller then leaves the cursor there (see
/// [`footer_seq_resized`]) so rustyline's next render — `\r`, erase, prompt —
/// lands on the right row.
fn effective_input_line_resized(resized: bool) -> Option<String> {
    if resized && READING_LINE.load(Ordering::Relaxed) && !INPUT_DIRTY.load(Ordering::Relaxed) {
        return Some(cached_prompt());
    }
    effective_input_line()
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

/// Serializes the two writers that can move the body/footer boundary: the
/// heartbeat's cached repaint ([`paint_cached_footer`]) and the growth absorb a
/// pinning banner performs ([`repaint_escalation_around`]).
///
/// Without it the two race in the one window that matters. `escalation::pin`
/// measures the footer's row demand, grows the store, then scrolls the body out
/// of the rows the footer is about to claim. If a heartbeat paint lands between
/// the store mutation and the scroll it installs the SHORTER region first, and
/// the absorb's line feeds then scroll inside the already-shrunk body — losing a
/// body row and leaving the cursor one row off, which is the same class of
/// corruption the absorb exists to prevent. Holding this across the whole
/// measure → mutate → measure → write transition makes it atomic with respect to
/// paints.
///
/// LOCK ORDER (never invert): `FOOTER_PAINT` → `escalation::BANNERS`. Both
/// holders reach into the escalation store while holding this; nothing takes
/// `BANNERS` first and then this.
static FOOTER_PAINT: Mutex<()> = Mutex::new(());
/// The layout actually installed on screen, rather than a recomputation from
/// mutated tray state. Every production paint reconciles this before drawing.
static INSTALLED_LAYOUT: Mutex<Option<FooterLayout>> = Mutex::new(None);

fn layout_delta(before: FooterLayout, after: FooterLayout) -> TrayDelta {
    let old_bottom = before.body_bottom.min(after.rows).max(1);
    TrayDelta {
        grow: old_bottom.saturating_sub(after.body_bottom),
        release: after.body_bottom.saturating_sub(old_bottom),
        clear_from: before.top_row().min(after.rows.saturating_add(1)),
        clear_to: after.top_row(),
        body_bottom: after.body_bottom,
    }
}

/// Move the transcript before painting into claimed rows, or restore it into
/// released rows. Input editing uses region-scoped scrolling, preserving its
/// cursor outside the body; active body output follows the moved content.
fn layout_transition_seq(before: FooterLayout, after: FooterLayout, editing: bool) -> String {
    let delta = layout_delta(before, after);
    let mut seq = String::new();
    if delta.grow > 0 {
        // Scroll from the old body floor, regardless of the caller's cursor.
        // Saving before DECSTBM preserves the editor row and text column.
        // This also handles a body cursor above existing transcript content.
        let bottom = before.body_bottom.min(after.rows).max(1);
        seq.push_str(&format!(
            "\x1b7\x1b[1;{bottom}r\x1b[{bottom};1H{}\x1b8",
            "\n".repeat(delta.grow as usize)
        ));
        if !editing {
            seq.push_str(&format!("\x1b[{}A", delta.grow));
        }
    }
    if delta.clear_to > delta.clear_from {
        seq.push_str("\x1b7");
        for row in delta.clear_from..delta.clear_to {
            seq.push_str(&format!("\x1b[{row};1H\x1b[2K"));
        }
        seq.push_str("\x1b8");
    }
    if delta.release > 0 {
        seq.push_str(&tray_reglue_seq(after.body_bottom, delta.release));
        if !editing {
            seq.push_str(&format!("\x1b[{}B", delta.release));
        }
    }
    seq
}

/// Hook the heartbeat calls immediately before a repaint so the footer's
/// *content* can be refreshed, not just redrawn.
///
/// The heartbeat thread paints the CACHED footer (see [`paint_cached_footer`]),
/// which is why a pinned escalation banner's clock and emoji animate from it —
/// those are re-derived from the escalation store on every paint — while the
/// banner's STATUS ROW (worker phase + activity summary) stayed frozen: that row
/// is only rewritten by the REPL's idle pass, which never runs while the session
/// is parked in a blocking read (or attached to a worker). The REPL installs a
/// closure here that re-reads the live worker list / coordinator store and
/// rewrites every pinned banner's status, so a heartbeat frame carries fresh
/// text instead of a stale snapshot. No hook installed (tests, non-footer
/// sessions) = the previous paint-only behavior.
static FOOTER_REFRESH: Mutex<Option<Box<dyn Fn() + Send>>> = Mutex::new(None);

/// Install the pre-paint footer content refresher. Called once at REPL startup
/// when the footer region is live; replaces any prior hook.
pub fn set_footer_refresher(f: Box<dyn Fn() + Send>) {
    if let Ok(mut slot) = FOOTER_REFRESH.lock() {
        *slot = Some(f);
    }
}

/// Run the installed pre-paint refresher, if any.
///
/// Deliberately called OUTSIDE [`FOOTER_PAINT`]: the hook takes the worker-jobs
/// lock and writes into the escalation store, so running it under the paint lock
/// would add a third lock to the ordering for no benefit. It only mutates TEXT —
/// never the banner COUNT — so it cannot change the footer's row demand and
/// cannot race the growth absorb.
fn run_footer_refresher() {
    if let Ok(slot) = FOOTER_REFRESH.lock()
        && let Some(f) = slot.as_ref()
    {
        f();
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
///
/// THE SECOND BUG THIS FIXES: an `ATTACH_ACTIVE` veto used to sit alongside
/// `region_active` here, on the assumption that a worker `:attach` view owned an
/// ALTERNATE screen buffer. It does not — [`open_attach_view`] deliberately
/// renders on the PRIMARY buffer (so worker output keeps native scrollback) and
/// re-asserts the footer region on entry, which means the footer — including the
/// pinned escalation tray — is live and meant to be visible during an attach.
/// With the veto in place the tray's clock and emoji FROZE for as long as the
/// operator watched a worker, which is exactly when an escalation matters most.
/// The repaint is cursor-safe either way, so attach is no longer a veto.
/// Body-SCROLLING writes are a different matter and still skip under attach —
/// see [`repaint_escalation_around`].
fn heartbeat_should_paint(s: HeartbeatState) -> bool {
    // No region to paint — `suspend_footer_region` clears ACTIVE, so this also
    // covers a foreground TTY child (vim/sudo/less) owning the screen.
    if !s.region_active {
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
                    reading_line: READING_LINE.load(Ordering::Relaxed),
                    input_dirty: INPUT_DIRTY.load(Ordering::Relaxed),
                    idle_elapsed: idle >= idle_ms,
                    size_changed: size_changed_since_paint(),
                    animating: crate::escalation::animating(),
                }) {
                    // Refresh the footer's CONTENT before redrawing it, so a
                    // pinned banner's status row tracks the live worker list
                    // instead of freezing until the next operator keystroke.
                    run_footer_refresher();
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
    // Hold the boundary lock for the whole paint so a banner pinning on another
    // thread cannot slip its body-scroll between this paint's region install and
    // its footer write (see `FOOTER_PAINT`). Poisoning is not fatal here — the
    // worst case is a redundant repaint — so recover rather than propagate.
    let _boundary = FOOTER_PAINT.lock().unwrap_or_else(|e| e.into_inner());
    paint_cached_footer_locked(home_body, &_boundary);
}

/// Paint with the boundary already held. Tray updates must use this entry point:
/// taking the non-reentrant mutex again would deadlock rustyline's hinter.
fn paint_cached_footer_locked(home_body: bool, _boundary: &MutexGuard<'_, ()>) {
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
    // resize and refresh the footer to the new canvas dimensions on sight. The
    // comparison against the PREVIOUS paint is also what tells `footer_seq` this
    // is the first paint at a new geometry: on a resize the absolute input row
    // moved, so the parked prompt has to be re-painted (and the cursor re-homed)
    // even though the editor is blocked inside a read and normally owns that row.
    let packed = pack_size(rows, cols);
    let prev = LAST_PAINTED_SIZE.swap(packed, Ordering::Relaxed);
    let resized = prev != 0 && prev != packed;
    let (msg, bar) = LAST_FOOTER.lock().map(|l| l.clone()).unwrap_or_default();
    // The INPUT row is resolved inside `footer_seq` (type-ahead → cached prompt
    // → leave-alone-while-reading), so the status message row stays the status
    // message in every state.
    let utf8 = utf8_locale();
    let sep = separator_line(cols, utf8, crate::style::colors_enabled());
    // footer_seq re-asserts the scroll region internally (inside its DECSC/DECRC
    // save-restore).
    if home_body {
        // The screen was wiped: there is no transcript or stale tray to move.
        *INSTALLED_LAYOUT.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }
    let mut buf = footer_seq_resized(rows, cols, &sep, &msg, &bar, resized);
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
/// footer rows, restore cursor. `separator`, `status_msg`, and `statusline`
/// are painted verbatim (already styled by the caller) after clipping each to
/// `cols` visible columns so nothing wraps and corrupts the region.
///
/// The INPUT row (H-2) is resolved from process state by [`effective_input_line`]
/// — live type-ahead, resting prompt, or "leave it alone" while the line editor
/// owns it.
pub fn footer_seq(
    rows: u16,
    cols: u16,
    separator: &str,
    status_msg: &str,
    statusline: &str,
) -> String {
    footer_seq_resized(rows, cols, separator, status_msg, statusline, false)
}

/// [`footer_seq`], told whether this paint is the first one at a NEW terminal
/// size.
///
/// `resized` is what makes a window resize self-heal while the editor is parked
/// in a read: it promotes the input row from "leave it alone" to "repaint the
/// resting prompt at the new H-2" (see [`effective_input_line_resized`]) and
/// leaves the cursor on that row instead of letting DECRC restore it to the row
/// the editor used to live on. Only the resize paint passes `true`; every other
/// paint path keeps the editor's cursor untouched.
pub fn footer_seq_resized(
    rows: u16,
    cols: u16,
    separator: &str,
    status_msg: &str,
    statusline: &str,
    resized: bool,
) -> String {
    // Snapshot the pinned escalations ONCE — asking for only as many banners as
    // this window can hold — and hand them to the pure builder, so the rows we
    // reserve and the rows we paint agree even if a banner retires mid-paint.
    let keep = escalation_rows_that_fit(rows, crate::escalation::row_count())
        / crate::escalation::ROWS_PER_BANNER;
    let banners = crate::escalation::rows(crate::style::colors_enabled(), keep as usize);
    // Snapshot the hint tray the same way and for the same reason: the row plan
    // and the paint must agree even if the operator's next keystroke swaps the
    // menu out mid-paint.
    let hints = HINT_TRAY
        .lock()
        .map(|t| t.clone())
        .unwrap_or_else(|e| e.into_inner().clone());
    let layout = FooterLayout::solve_with(
        rows,
        banners.len() as u16 * crate::escalation::ROWS_PER_BANNER,
        hints.len() as u16,
    );
    let mut seq = String::new();
    // Set when this paint is the first one at a NEW terminal height — see the
    // band erase below.
    let mut geometry_changed = false;
    if ACTIVE.load(Ordering::Relaxed) {
        let mut installed = INSTALLED_LAYOUT.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(before) = *installed {
            // A WINDOW RESIZE IS NOT A TRAY TRANSITION. `before` was solved for
            // the old height, so every row in it — body floor included — is
            // offset from the new plan by the height delta. Feeding that into
            // the tray choreography makes it believe the tray grew or shrank by
            // the whole delta and emit a region-scoped `SU`/`SD`, which drags
            // the transcript across the screen and smears the chrome the
            // emulator left behind at the old rows: the "ghost" escalation
            // banner that reappears above the live footer (often doubled) after
            // navigating away from the window and back. On a geometry change
            // there is nothing to reconcile — the emulator already reflowed the
            // screen — so just re-anchor the body cursor inside the new body and
            // re-record the plan.
            if before.rows == layout.rows {
                let editing = reading_line();
                seq.push_str(&layout_transition_seq(before, layout, editing));
                let delta = layout_delta(before, layout);
                let row = BODY_ROW.load(Ordering::Relaxed);
                if row > 0 {
                    BODY_ROW.store(
                        row.saturating_sub(delta.grow)
                            .saturating_add(delta.release)
                            .clamp(1, layout.body_bottom),
                        Ordering::Relaxed,
                    );
                }
            } else {
                geometry_changed = true;
                let row = BODY_ROW.load(Ordering::Relaxed);
                if row > 0 {
                    BODY_ROW.store(row.clamp(1, layout.body_bottom), Ordering::Relaxed);
                }
            }
        }
        *installed = Some(layout);
    }
    // The emulator reflowed the screen under us, so the rows the NEW footer band
    // covers can still hold fragments of the OLD, differently-sized footer —
    // most visibly a banner row that the shorter/taller plan no longer paints,
    // left sitting inside the band as a ghost. Erase the whole band once before
    // repainting it. `ED` is NOT clipped by the DECSTBM margins, so this clears
    // exactly the absolute rows [top_row..rows] and can never touch transcript
    // above the footer; the DECSC/DECRC wrapper keeps the caller's cursor.
    if geometry_changed && layout.height > 0 {
        let top = layout.top_row();
        seq.push_str(&format!("\x1b7\x1b[{top};1H\x1b[J\x1b8"));
    }
    let input = effective_input_line_resized(resized);
    seq.push_str(&footer_seq_with(
        rows,
        cols,
        separator,
        status_msg,
        statusline,
        banners,
        input.as_deref(),
        &hints,
    ));
    // A resize paint that re-parked the prompt must also MOVE the cursor: the
    // editor is blocked in a read and still believes its line lives on the old
    // row, and `footer_seq_with`'s DECRC would put the caret right back there.
    // Landing it just past the re-painted prompt makes the visible caret correct
    // immediately and gives rustyline's next render the right row to erase.
    if resized
        && READING_LINE.load(Ordering::Relaxed)
        && let (Some(row), Some(text)) = (layout.input_row, input.as_deref())
    {
        let col = visible_cols(&clip_visible(text, cols as usize)).saturating_add(1);
        seq.push_str(&format!("\x1b[{row};{col}H"));
    }
    seq
}

/// [`footer_seq`] with the escalation banners AND the input-row content passed in
/// instead of read from process-global state — the whole row plan is a pure
/// function of `(rows, cols, banners, input)`, so the geometry is unit-testable
/// without mutating shared state.
///
/// `input == None` means "do not touch the input row": the line editor is
/// drawing there and owns the cursor. `Some(text)` paints `text` on row H-2.
///
/// `hints` are the `:`-command tray rows (see [`set_hint_tray`]), painted
/// between the banner block and the rule — one row each, already styled.
#[allow(clippy::too_many_arguments)]
pub fn footer_seq_with(
    rows: u16,
    cols: u16,
    separator: &str,
    status_msg: &str,
    statusline: &str,
    banners: Vec<(String, String)>,
    input: Option<&str>,
    hints: &[String],
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
    let layout = FooterLayout::solve_with(
        rows,
        (banners.len() as u16) * crate::escalation::ROWS_PER_BANNER,
        hints.len() as u16,
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
        let esc_row = top_row
            + u16::from(layout.spacer_row.is_some())
            + (i as u16) * crate::escalation::ROWS_PER_BANNER;
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
    // TOP rule — the lid of the pinned block. Same glyph run as the bottom
    // rule, so the trays read as a framed panel between two rules instead of
    // text floating at the end of the transcript. It occupies the row the layout
    // already reserved as the tray spacer, so framing the block costs nothing.
    if let Some(row) = layout.spacer_row {
        s.push_str(&format!("\x1b[{row};1H\x1b[2K{sep}"));
    }
    // BETWEEN-TRAYS rule — present only when both trays are up, dividing the
    // escalation banners above from the `:`-hint tray below.
    if let Some(row) = layout.tray_sep_row {
        s.push_str(&format!("\x1b[{row};1H\x1b[2K{sep}"));
    }
    // The `:`-command hint tray: directly BELOW the escalation block and
    // directly ABOVE the rule, so the menu reads as the last thing the body said
    // and — crucially — grows UPWARD. The input row below the rule cannot move,
    // which is the whole point of the tray (see [`HINT_TRAY`]).
    //
    // `layout.hint_rows` clamps to what the window granted; a resize that shrank
    // the tray mid-keystroke drops the TAIL rows (the palette's own `… and N
    // more` summary is the first casualty) rather than painting outside the
    // reserved region. The next keystroke rebuilds the menu at the new cap.
    if let Some(hint_top) = layout.hint_top_row() {
        for (i, row) in hints.iter().take(layout.hint_rows as usize).enumerate() {
            let hint_row = hint_top + i as u16;
            s.push_str(&format!(
                "\x1b[{hint_row};1H\x1b[2K{}",
                clip_visible(row, max)
            ));
        }
    }
    // Each zone paints ONLY if the plan granted it a row. A degraded footer
    // (short window) silently drops the rule, then the message, then the
    // statusline — and keeps the INPUT row — rather than dropping the whole
    // footer off a cliff.
    if let Some(sep_row) = layout.sep_row {
        s.push_str(&format!("\x1b[{sep_row};1H\x1b[2K{sep}"));
    }
    // The input row: ONE row below the rule, third from the bottom. Painted only
    // when the caller passed content — `None` means the line editor is live on
    // that row and rewriting it would fight the editor's own redraw.
    if let (Some(input_row), Some(text)) = (layout.input_row, input) {
        s.push_str(&format!(
            "\x1b[{input_row};1H\x1b[2K{}",
            clip_visible(text, max)
        ));
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

/// Visible width of a possibly-ANSI-colored string, in terminal columns: SGR/CSI
/// escapes count zero, everything else by its unicode display width. Used to
/// place the caret just past a re-painted prompt.
fn visible_cols(s: &str) -> u16 {
    use unicode_width::UnicodeWidthChar;
    let mut width = 0usize;
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            if let Some(&n) = chars.peek() {
                if n == '[' {
                    chars.next();
                    while let Some(&p) = chars.peek() {
                        chars.next();
                        if ('\x40'..='\x7e').contains(&p) {
                            break;
                        }
                    }
                } else {
                    chars.next();
                }
            }
            continue;
        }
        width += UnicodeWidthChar::width(c).unwrap_or(0);
    }
    width.min(u16::MAX as usize) as u16
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
        let _boundary = FOOTER_PAINT.lock().unwrap_or_else(|e| e.into_inner());
        let layout = FooterLayout::for_rows(self.rows);
        *INSTALLED_LAYOUT.lock().unwrap_or_else(|e| e.into_inner()) = Some(layout);
        let body_bottom = layout.body_bottom;
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
        let _boundary = FOOTER_PAINT.lock().unwrap_or_else(|e| e.into_inner());
        let top_row = INSTALLED_LAYOUT
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
            .map_or_else(|| footer_top_row(self.rows), |layout| layout.top_row());
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
        let _boundary = FOOTER_PAINT.lock().unwrap_or_else(|e| e.into_inner());
        crate::escalation::sweep();
        let sep = separator_line(self.cols, self.utf8, crate::style::colors_enabled());
        let mut buf = String::new();
        // footer_seq re-asserts the scroll region internally, INSIDE its
        // DECSC/DECRC save-restore, so the DECSTBM cursor-home side effect never
        // leaks out and strands the next prompt at the top of the screen.
        buf.push_str(&footer_seq(
            self.rows, self.cols, &sep, status_msg, statusline,
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
            if !self.active {
                self.init_scroll_region();
            }
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
    let _boundary = FOOTER_PAINT.lock().unwrap_or_else(|e| e.into_inner());
    if !ACTIVE.load(Ordering::Relaxed) {
        return false;
    }
    let Some((rows, _cols)) = term_size() else {
        return false;
    };
    let top_row = INSTALLED_LAYOUT
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .take()
        .map_or_else(|| footer_top_row(rows), |layout| layout.top_row());
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
#[cfg(test)]
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
#[cfg(test)]
pub fn banner_growth_seq(grow: u16) -> String {
    if grow == 0 {
        return String::new();
    }
    format!("{}\x1b[{grow}A", "\n".repeat(grow as usize))
}

/// Mutate escalation items and reconcile the installed layout atomically.
/// The same paint path handles pinning, dismissal, and timed retirement,
/// including while the editor owns the input row.
pub fn repaint_escalation_around<T>(mutate: impl FnOnce() -> T) -> T {
    let _boundary = FOOTER_PAINT.lock().unwrap_or_else(|e| e.into_inner());
    let result = mutate();
    paint_cached_footer_locked(false, &_boundary);
    result
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
    let _boundary = FOOTER_PAINT.lock().unwrap_or_else(|e| e.into_inner());
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
    *INSTALLED_LAYOUT.lock().unwrap_or_else(|e| e.into_inner()) =
        Some(FooterLayout::for_rows(rows));
    // Repaint the pinned footer rows from cache (cursor-safe: footer_seq wraps
    // its paint in DECSC/DECRC).
    paint_cached_footer_locked(false, &_boundary);
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

/// Whether the REPL is parked in a blocking line read — i.e. the line editor
/// owns the INPUT row and the real cursor sits there, OUTSIDE the scroll region.
/// Callers that want to print transcript text consult this to route the write
/// into the body ([`print_in_body`]) instead of emitting it at the cursor.
pub fn reading_line() -> bool {
    READING_LINE.load(Ordering::Relaxed)
}

/// Last body row the cursor was known to occupy (1-based; `0` = unknown).
///
/// The INPUT row lives in the footer, OUTSIDE the DECSTBM region, so parking the
/// line editor there takes the cursor out of the transcript — and something has
/// to remember where the transcript left off. A fresh prompt anchors writes at
/// the body's bottom row; this memory is advanced by every
/// [`print_in_body`] write, and clamped into the live body on read (a resize can
/// shrink the body under a stale value).
static BODY_ROW: AtomicU16 = AtomicU16::new(0);

/// The footer plan for the LIVE terminal, or `None` when no footer region is
/// installed or the size is unknown — in which case every input-row helper
/// no-ops and the editor keeps its classic inline behavior.
fn live_layout() -> Option<FooterLayout> {
    if !ACTIVE.load(Ordering::Relaxed) {
        return None;
    }
    term_size().map(|(rows, _)| FooterLayout::for_rows(rows))
}

/// The remembered body row, clamped into `layout`'s body. Unknown (or a row the
/// current geometry can't hold) resolves to the LAST body row — where an
/// append-only transcript ends up anyway, so the fallback degrades to "keep
/// writing at the bottom" rather than to corruption.
fn body_cursor_row(layout: &FooterLayout) -> u16 {
    let row = BODY_ROW.load(Ordering::Relaxed);
    if row == 0 {
        layout.body_bottom
    } else {
        row.clamp(1, layout.body_bottom)
    }
}

/// Park the line editor on the footer's INPUT row (`H-2`) and cache `prompt` so
/// every later repaint can redraw the resting affordance there.
///
/// Called immediately before the editor blocks on a read. Two effects:
/// 1. the prompt string is cached in `LAST_PROMPT`, which is what makes the input
///    row unconditional — the footer can repaint it in ANY state;
/// 2. transcript writes are anchored to the bottom of the body, and the cursor
///    moves to the cleared input row so rustyline renders the live buffer there.
///    This path must not read stdin: a cursor-position probe would consume keys
///    typed during a Shift-Tab/attach transition before the editor sees them.
///
/// No-ops when no footer is installed: off-tty, in a short window, or before the
/// region is enabled the editor prints inline exactly as it always did.
///
/// Known limitation: an input line that wraps past the bottom row scrolls the
/// screen, because the editor is drawing outside the scroll region. Two columns'
/// worth of typing is the practical ceiling, and the next heartbeat repaint heals
/// the footer rows.
pub fn park_input_row(prompt: &str) {
    match LAST_PROMPT.lock() {
        Ok(mut slot) => *slot = prompt.to_string(),
        Err(e) => *e.into_inner() = prompt.to_string(),
    }
    let Some(layout) = live_layout() else { return };
    let Some(row) = layout.input_row else { return };
    BODY_ROW.store(layout.body_bottom, Ordering::Relaxed);
    let mut out = std::io::stdout();
    let _ = write!(out, "\x1b[{row};1H\x1b[2K");
    let _ = out.flush();
}

/// Release the INPUT row after a read and return the cursor to the transcript,
/// optionally echoing the submitted line into the body first.
///
/// The echo is what keeps the scrollback honest: with the editor parked in the
/// footer, the submitted command would otherwise be overwritten by the next
/// prompt and vanish from the transcript. `echo` should be the prompt + the
/// line, exactly as it appeared; `None` (Ctrl-C, EOF, an empty submit) echoes
/// nothing. Either way the cursor lands back in the body, so the command's own
/// output scrolls inside the region as usual.
pub fn unpark_input_row(echo: Option<&str>) {
    // The read is over, so any `:`-palette the operator was typing is stale —
    // retire the tray (and give its rows back to the body) BEFORE the echo, so
    // the submitted line lands in a body that is already its final height.
    // Covers submit, Ctrl-C and EOF alike, which is why it lives here rather
    // than in the editor's success path.
    clear_hint_tray();
    let Some(layout) = live_layout() else { return };
    let Some(input_row) = layout.input_row else {
        return;
    };
    let row = body_cursor_row(&layout);
    let mut out = std::io::stdout();
    // Wipe the editor's leftovers so the resting-prompt repaint starts clean.
    let _ = write!(out, "\x1b[{input_row};1H\x1b[2K\x1b[{row};1H");
    if let Some(line) = echo {
        // At the last body row this `\r\n` scrolls the region (correct); above it
        // it just steps down a row.
        let _ = write!(out, "{line}\r\n");
        BODY_ROW.store((row + 1).min(layout.body_bottom), Ordering::Relaxed);
    } else {
        BODY_ROW.store(row, Ordering::Relaxed);
    }
    let _ = out.flush();
}

/// Write already-CRLF `text` into the BODY while the line editor is parked on the
/// input row, leaving the editor's cursor untouched (DECSC/DECRC around the
/// write). Returns `false` when no footer is installed, so the caller can fall
/// back to its ordinary print path.
///
/// This is the counterpart to parking: because the input row is a DIFFERENT row
/// from the transcript, a body write needs no erase-and-redraw of the prompt at
/// all — the editor's line simply stays where it is. `text` must carry its own
/// trailing newline and use CRLF (the tty is in raw mode while the editor reads).
pub fn print_in_body(text: &str) -> bool {
    let _boundary = FOOTER_PAINT.lock().unwrap_or_else(|e| e.into_inner());
    let Some(layout) = live_layout() else {
        return false;
    };
    if layout.input_row.is_none() {
        return false;
    }
    let row = body_cursor_row(&layout);
    let consumed = text.matches('\n').count() as u16;
    let mut out = std::io::stdout();
    let _ = write!(out, "\x1b7\x1b[{row};1H{text}\x1b8");
    let _ = out.flush();
    BODY_ROW.store(
        row.saturating_add(consumed).min(layout.body_bottom),
        Ordering::Relaxed,
    );
    true
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
    fn hint_tray_repaint_completes_on_tty() {
        const CHILD: &str = "AISH_TEST_HINT_TRAY_CHILD";
        if std::env::var_os(CHILD).is_some() {
            assert_eq!(term_size(), Some((24, 80)));
            ACTIVE.store(true, Ordering::Relaxed);
            set_reading_line(true);
            assert!(set_hint_tray(vec![
                ":quit leave".into(),
                ":jobs list".into()
            ]));
            assert!(set_hint_tray(vec![":quit leave".into()]));
            clear_hint_tray();
            assert_eq!(hint_tray_rows(), 0);
            assert!(FOOTER_PAINT.try_lock().is_ok());
            ACTIVE.store(false, Ordering::Relaxed);
            return;
        }
        run_pty_test("hint_tray_repaint_completes_on_tty", CHILD, None);
    }

    #[test]
    fn attach_return_preserves_pending_input() {
        const CHILD: &str = "AISH_TEST_ATTACH_INPUT_CHILD";
        if std::env::var_os(CHILD).is_some() {
            use crate::editor::{LineEditor, ReadOutcome, RustylineEditor};
            let mut term = Terminal::detect().expect("PTY required");
            term.init_scroll_region();
            open_attach_view();
            close_attach_view();
            let helper = crate::repl::AishHelper::new(
                std::env::temp_dir(),
                String::new(),
                std::sync::Arc::new(std::collections::HashMap::new()),
            );
            let mut editor = RustylineEditor::new(
                helper,
                std::env::temp_dir().join("aish-input-test-unused-history"),
            )
            .unwrap();
            // Input queued during the view transition must reach rustyline,
            // even when this terminal never answers a cursor-position query.
            match editor.read_line("returned> ") {
                ReadOutcome::Line(line) => assert_eq!(line, "hello"),
                _ => panic!("input did not reach the editor"),
            }
            return;
        }
        run_pty_test(
            "attach_return_preserves_pending_input",
            CHILD,
            Some(b"hello\r"),
        );
    }

    #[test]
    fn background_completion_between_reads_keeps_prompt_on_input_row() {
        const CHILD: &str = "AISH_TEST_COMPLETION_PROMPT_CHILD";
        if std::env::var_os(CHILD).is_some() {
            use crate::editor::{LineEditor, ReadOutcome, RustylineEditor};
            let mut term = Terminal::detect().expect("PTY required");
            term.init_scroll_region();
            let helper = crate::repl::AishHelper::new(
                std::env::temp_dir(),
                String::new(),
                std::sync::Arc::new(std::collections::HashMap::new()),
            );
            let mut editor = RustylineEditor::new(
                helper,
                std::env::temp_dir().join("aish-completion-test-unused-history"),
            )
            .unwrap();
            struct QueuedPrinter;
            impl crate::editor::LinePrinter for QueuedPrinter {
                fn print(&mut self, _: String) {
                    panic!("completion was queued for an inline prompt redraw");
                }
            }
            crate::tools::install_external_printer(Box::new(QueuedPrinter));
            set_reading_line(false);
            assert!(crate::tools::print_above_prompt(
                "Reviewing and summarizing result\n".into()
            ));
            match editor.read_line("completed> ") {
                ReadOutcome::Line(line) => assert_eq!(line, "hello"),
                _ => panic!("input did not reach editor"),
            }
            return;
        }
        let bytes = run_pty_test(
            "background_completion_between_reads_keeps_prompt_on_input_row",
            CHILD,
            Some(b"hello\r"),
        );
        let output = String::from_utf8_lossy(&bytes);
        let notice = output
            .find("Reviewing and summarizing result")
            .expect("completion notice must print immediately");
        let rest = &output[notice..];
        let prompt = rest.find("completed> ").expect("prompt after completion");
        assert!(
            rest[..prompt].contains("\x1b[22;1H"),
            "prompt must anchor on H-2: {output}"
        );
    }

    #[test]
    fn escalation_and_hint_layout_changes_reconcile_on_tty() {
        const CHILD: &str = "AISH_TEST_LAYOUT_TRANSITIONS_CHILD";
        if std::env::var_os(CHILD).is_some() {
            let mut term = Terminal::detect().expect("PTY required");
            term.init_scroll_region();
            term.draw_footer("msg", "bar");
            set_reading_line(true);
            park_input_row("input> ");
            set_input_dirty(true);
            let check = || {
                let installed = INSTALLED_LAYOUT.lock().unwrap().unwrap();
                let live = FooterLayout::for_rows(term_size().unwrap().0);
                assert_eq!(installed, live);
                if live.banner_rows > 0 || live.hint_rows > 0 {
                    assert_eq!(live.spacer_row, Some(live.body_bottom + 1));
                }
                assert_eq!(BODY_ROW.load(Ordering::Relaxed), live.body_bottom);
                assert!(FOOTER_PAINT.try_lock().is_ok());
            };
            crate::escalation::pin("w_first", "first");
            check();
            crate::escalation::pin("w_second", "second");
            check();
            set_hint_tray(vec!["hint".into(); 3]);
            check();
            crate::escalation::unpin("w_first");
            check();
            clear_hint_tray();
            check();
            // Resize with a live tray: reconcile against the installed layout,
            // not a freshly installed margin that forgot the old body floor.
            let size = libc::winsize {
                ws_row: 12,
                ws_col: 80,
                ws_xpixel: 0,
                ws_ypixel: 0,
            };
            assert_eq!(
                unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCSWINSZ, &size) },
                0
            );
            assert!(term.handle_resize());
            check();
            crate::escalation::unpin("w_second");
            check();
            set_reading_line(false);
            return;
        }
        run_pty_test(
            "escalation_and_hint_layout_changes_reconcile_on_tty",
            CHILD,
            None,
        );
    }

    /// REGRESSION: a window resize while the editor is parked at the prompt must
    /// re-park the resting prompt — and the cursor — on the NEW input row.
    ///
    /// The input row is an ABSOLUTE row (H-2) and `handle_resize` only runs
    /// BETWEEN prompts, so at an idle prompt the heartbeat's repaint is the only
    /// thing that sees the new geometry. It used to deliberately skip the input
    /// row while `reading_line`, which left the prompt and the caret stranded on
    /// the old row: a dead prompt mid-screen (grow) or one buried under the
    /// footer (shrink), with a blank input row either way — "aish doesn't redraw
    /// on resize".
    #[test]
    fn resize_at_the_prompt_reparks_the_input_row() {
        const CHILD: &str = "AISH_TEST_RESIZE_PROMPT_CHILD";
        const PROMPT: &str = "resize> ";
        if std::env::var_os(CHILD).is_some() {
            let mut term = Terminal::detect().expect("PTY required");
            term.init_scroll_region();
            term.draw_footer("msg", "bar");
            park_input_row(PROMPT);
            // The editor is now blocked inside a read with an EMPTY buffer: the
            // exact state an idle shell spends nearly all of its life in.
            set_reading_line(true);
            set_input_dirty(false);
            // One paint at the ORIGINAL geometry, exactly as the idle heartbeat
            // does, so the next paint can tell the size actually moved.
            paint_cached_footer(false);
            let before = FooterLayout::for_rows(24).input_row.expect("24-row input");
            // Shrink the window under the read. No `handle_resize` call here —
            // that only runs BETWEEN prompts, which is the whole point.
            let size = libc::winsize {
                ws_row: 18,
                ws_col: 80,
                ws_xpixel: 0,
                ws_ypixel: 0,
            };
            assert_eq!(
                unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCSWINSZ, &size) },
                0
            );
            let after = FooterLayout::for_rows(18).input_row.expect("18-row input");
            assert_ne!(before, after, "resize must move the absolute input row");
            // Tell the parent where to look, then let the heartbeat's
            // cursor-safe repaint run — the only paint available at an idle
            // prompt.
            print!("<<MARK:{after}:{}>>", PROMPT.len() + 1);
            let _ = std::io::stdout().flush();
            paint_cached_footer(false);
            set_reading_line(false);
            return;
        }
        let bytes = run_pty_test("resize_at_the_prompt_reparks_the_input_row", CHILD, None);
        let output = String::from_utf8_lossy(&bytes);
        let (_, tail) = output
            .split_once("<<MARK:")
            .expect("child marker missing from PTY output");
        let (coords, tail) = tail.split_once(">>").expect("unterminated marker");
        let (row, col) = coords.split_once(':').expect("malformed marker");
        assert!(
            tail.contains(&format!("\x1b[{row};1H\x1b[2K")),
            "resize paint did not clear the NEW input row {row}: {tail:?}"
        );
        assert!(
            tail.contains(PROMPT),
            "resize paint did not re-park the prompt: {tail:?}"
        );
        assert!(
            tail.contains(&format!("\x1b[{row};{col}H")),
            "cursor was not re-homed past the prompt at {row};{col}: {tail:?}"
        );
    }

    /// REGRESSION (ghost escalation banner): a WINDOW RESIZE is not a tray
    /// transition.
    ///
    /// The installed layout was solved for the OLD height, so after the
    /// emulator reflows the screen every row in it is off by the height delta.
    /// Feeding that into the tray choreography made it believe the tray grew by
    /// the whole delta and emit a region-scoped scroll (`ESC[1;<old>r` + LFs),
    /// which dragged the transcript across the screen and smeared the chrome the
    /// emulator had left at the old rows — the duplicated "ghost" escalation
    /// banner that shows up above the live footer after navigating away from the
    /// window and back. The resize paint must instead erase the new footer band
    /// once and repaint it, with no transcript scrolling at all.
    #[test]
    fn resize_with_pinned_banner_erases_band_without_scrolling() {
        const CHILD: &str = "AISH_TEST_RESIZE_GHOST_CHILD";
        if std::env::var_os(CHILD).is_some() {
            let mut term = Terminal::detect().expect("PTY required");
            term.init_scroll_region();
            term.draw_footer("msg", "bar");
            crate::escalation::pin("w_ghost", "ghost worker");
            // One paint at the ORIGINAL geometry so the next paint can tell the
            // size actually moved.
            paint_cached_footer(false);
            let old_bottom = FooterLayout::for_rows(24).body_bottom;
            let size = libc::winsize {
                ws_row: 18,
                ws_col: 80,
                ws_xpixel: 0,
                ws_ypixel: 0,
            };
            assert_eq!(
                unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCSWINSZ, &size) },
                0
            );
            let new_top = FooterLayout::for_rows(18).top_row();
            print!("<<GHOST:{old_bottom}:{new_top}>>");
            let _ = std::io::stdout().flush();
            paint_cached_footer(false);
            crate::escalation::unpin("w_ghost");
            return;
        }
        let bytes = run_pty_test(
            "resize_with_pinned_banner_erases_band_without_scrolling",
            CHILD,
            None,
        );
        let output = String::from_utf8_lossy(&bytes);
        let (_, tail) = output
            .split_once("<<GHOST:")
            .expect("child marker missing from PTY output");
        let (coords, tail) = tail.split_once(">>").expect("unterminated marker");
        let (old_bottom, new_top) = coords.split_once(':').expect("malformed marker");
        assert!(
            tail.contains(&format!("\x1b[{new_top};1H\x1b[J")),
            "resize paint did not erase the new footer band at row {new_top}: {tail:?}"
        );
        assert!(
            !tail.contains(&format!("\x1b[1;{old_bottom}r")),
            "resize paint scrolled the transcript through the old body floor \
             {old_bottom} (ghost-banner regression): {tail:?}"
        );
    }

    #[test]
    fn multiline_clipboard_paste_stays_compact() {
        const CHILD: &str = "AISH_TEST_CLIPBOARD_CHILD";
        if std::env::var_os(CHILD).is_some() {
            use crate::editor::{LineEditor, ReadOutcome, RustylineEditor};
            let mut term = Terminal::detect().expect("PTY required");
            term.init_scroll_region();
            let helper = crate::repl::AishHelper::new(
                std::env::temp_dir(),
                String::new(),
                std::sync::Arc::new(std::collections::HashMap::new()),
            );
            let mut editor = RustylineEditor::new(
                helper,
                std::env::temp_dir().join("aish-paste-test-unused-history"),
            )
            .unwrap();
            match editor.read_line("paste> ") {
                ReadOutcome::Line(line) => assert_eq!(line, "prefix alpha\nbeta\ngamma suffix"),
                _ => panic!("paste did not reach editor"),
            }
            return;
        }
        let bytes = run_pty_test(
            "multiline_clipboard_paste_stays_compact",
            CHILD,
            Some(b"prefix \x1b[200~alpha\nbeta\ngamma\x1b[201~ suffix\r"),
        );
        let output = String::from_utf8_lossy(&bytes);
        assert!(output.contains("[pasted 3 lines from clipboard]"));
        assert!(
            !output.contains("beta"),
            "clipboard payload leaked into prompt: {output}"
        );
    }

    fn run_pty_test(name: &str, child_env: &str, input: Option<&[u8]>) -> Vec<u8> {
        // Exercise the real ioctl + repaint path in an isolated process. A pipe
        // would return before the nested lock, hiding the regression. Bound the
        // wait so a deadlock fails the test instead of hanging the entire suite.
        use std::os::fd::FromRawFd;
        use std::process::{Command, Stdio};
        let mut master = -1;
        let mut slave = -1;
        let mut size = libc::winsize {
            ws_row: 24,
            ws_col: 80,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        // SAFETY: valid out-pointers; successful descriptors are owned below.
        assert_eq!(
            unsafe {
                libc::openpty(
                    &mut master,
                    &mut slave,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    &raw mut size,
                )
            },
            0
        );
        // SAFETY: openpty returned fresh, owned file descriptors.
        let mut master = unsafe { std::fs::File::from_raw_fd(master) };
        let slave = unsafe { std::fs::File::from_raw_fd(slave) };
        if let Some(input) = input {
            use std::os::fd::AsRawFd;
            // Queue keystrokes before the child parks the editor. Raw mode
            // prevents echo and canonical buffering from masking input loss.
            let mut attrs: libc::termios = unsafe { std::mem::zeroed() };
            assert_eq!(unsafe { libc::tcgetattr(slave.as_raw_fd(), &mut attrs) }, 0);
            unsafe { libc::cfmakeraw(&mut attrs) };
            assert_eq!(
                unsafe { libc::tcsetattr(slave.as_raw_fd(), libc::TCSANOW, &attrs) },
                0
            );
            master.write_all(input).unwrap();
        }
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                &format!("terminal::tests::{name}"),
                "--nocapture",
            ])
            .env(child_env, "1")
            .env("TERM", "xterm-256color")
            .stdin(Stdio::from(slave.try_clone().unwrap()))
            .stdout(Stdio::from(slave))
            .spawn()
            .unwrap();
        // Drain output concurrently, so PTY capacity cannot masquerade as a
        // repaint deadlock even if the footer gains more content later.
        let reader = std::thread::spawn(move || {
            let mut master = master;
            let mut bytes = Vec::new();
            let _ = std::io::Read::read_to_end(&mut master, &mut bytes);
            bytes
        });
        let deadline = Instant::now() + Duration::from_secs(10);
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break Some(status);
            }
            if Instant::now() >= deadline {
                child.kill().unwrap();
                child.wait().unwrap();
                break None;
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        let bytes = reader.join().unwrap();
        assert!(
            status.is_some_and(|s| s.success()),
            "PTY test {name} hung or failed: {}",
            String::from_utf8_lossy(&bytes)
        );
        bytes
    }

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
    fn scroll_region_reserves_four_bottom_rows() {
        // 24-row terminal → region rows 1..=20, footer at 21 (rule), 22 (input),
        // 23 (status message), 24 (statusline).
        assert_eq!(scroll_region_seq(24), "\x1b[1;20r");
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
        // 24-row terminal → body is rows 1..=20. A cursor at or above the last
        // body row needs no scroll: nothing of the output is sitting in the
        // rows the footer is about to reclaim. This is the alt-screen case —
        // vim/less restore the pre-launch cursor, so leaving them never jerks
        // the viewport.
        // Solved explicitly rather than through `footer_overflow_rows`, which
        // reads the LIVE escalation stack: banner rows shrink the body, and a
        // sibling test pinning a banner would otherwise flip these numbers.
        let plan = FooterLayout::solve(24, 0);
        assert_eq!(plan.overflow_rows(20), 0);
        assert_eq!(plan.overflow_rows(10), 0);
        assert_eq!(plan.overflow_rows(1), 0);
    }

    #[test]
    fn footer_overflow_measures_rows_spilled_into_the_footer() {
        // THE BUG, in numbers: `ls -al` on a 24-row terminal left the cursor at
        // row 24 while the region was suspended, so 4 rows of output sat under
        // the footer and got painted over. Lift exactly that many.
        let plan = FooterLayout::solve(24, 0);
        assert_eq!(plan.overflow_rows(21), 1);
        assert_eq!(plan.overflow_rows(22), 2);
        assert_eq!(plan.overflow_rows(23), 3);
        assert_eq!(plan.overflow_rows(24), 4);
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
        // 24-row window: no banner → body ends at 20; one banner → 17.
        // The banner and spacer claim three body rows.
        assert_eq!(FooterLayout::solve(24, 0).body_bottom, 20);
        assert_eq!(FooterLayout::solve(24, 2).body_bottom, 17);
        assert_eq!(banner_growth_rows(24, 0, 2), 3);
        // Second banner takes two more; the first→second step is still 2.
        assert_eq!(banner_growth_rows(24, 2, 4), 2);
        assert_eq!(banner_growth_rows(24, 0, 4), 5);
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
            let layout = FooterLayout::solve(rows, 2);
            let granted = layout.banner_rows + u16::from(layout.spacer_row.is_some());
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
    fn pinned_banner_and_an_animating_body_row_never_share_cells() {
        // THE INTERACTION the two halves of this fix have to survive together:
        // an escalation pins (footer grows, body floor rises) WHILE the in-place
        // `thinking…` row is still rewriting itself on a cadence. Half a fix
        // either way leaves the bug: without the absorb the spinner paints onto
        // the tray, and without the mid-turn heartbeat the tray's clock/emoji
        // freeze until a keystroke.
        let rows = 24u16;
        let before = FooterLayout::solve(rows, 0);
        let after = FooterLayout::solve(rows, 2);
        let grow = banner_growth_rows(rows, 0, 2);

        // The rows the tray claims == the rows the absorb lifts the body by.
        assert_eq!(grow, after.banner_rows + 1);
        assert_eq!(before.body_bottom - after.body_bottom, grow);
        let seq = banner_growth_seq(grow);
        assert_eq!(seq.matches('\n').count(), grow as usize);
        assert_eq!(seq, format!("{}\x1b[{grow}A", "\n".repeat(grow as usize)));

        // The cursor started on the OLD last body row, got scrolled up by
        // `grow`, then walked back up `grow` to the same CONTENT line — which
        // now sits ON the new body floor, i.e. strictly ABOVE the banner rows
        // [after.body_bottom + 1 ..= before.body_bottom]. So the next in-place
        // `thinking…` rewrite cannot land in the tray.
        let cursor_content_row = before.body_bottom - grow;
        assert_eq!(cursor_content_row, after.body_bottom);
        assert!(
            cursor_content_row < after.body_bottom + 1,
            "animated body row would land inside the pinned tray"
        );

        // …and the tray keeps ticking with NO keystroke: mid-turn, with a live
        // banner animating, the heartbeat paints even before the idle gate.
        assert!(heartbeat_should_paint(HeartbeatState {
            reading_line: false,
            idle_elapsed: false,
            animating: true,
            ..beat()
        }));
        // The two mechanisms stay disjoint on a no-op transition: a heartbeat
        // repaint that changes no banner count scrolls nothing.
        assert!(banner_growth_seq(banner_growth_rows(rows, 2, 2)).is_empty());
    }

    #[test]
    fn resume_lifts_overflow_before_reclaiming_the_rows() {
        // Order is load-bearing: SU (ESC[nS) must come BEFORE the DECSTBM
        // re-assert, while the region is still full-screen, so the whole
        // viewport scrolls and the lifted rows reach native scrollback.
        let seq = resume_region_seq_with_cursor(24, Some(24));
        assert_eq!(seq, "\x1b[4S\x1b[1;20r\x1b[20;1H");
        assert!(seq.find("\x1b[4S").unwrap() < seq.find("\x1b[1;20r").unwrap());

        // One spilled row scrolls one row.
        assert_eq!(
            resume_region_seq_with_cursor(24, Some(21)),
            "\x1b[1S\x1b[1;20r\x1b[20;1H"
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
            "\x1b[1;20r\x1b[20;1H"
        );
        // Cursor already clear of the footer zone → also no scroll emitted.
        assert_eq!(
            resume_region_seq_with_cursor(24, Some(20)),
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
        // 24-row terminal: re-assert region 1..=20 then home into row 20 (the
        // last body row) so the next prompt grows up from just above the footer.
        assert_eq!(resume_region_seq(24), "\x1b[1;20r\x1b[20;1H");
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
    fn footer_positions_four_rows_bottom_up() {
        // No banner → 4-row footer: rule, input, status message, statusline.
        let seq = footer_seq_with(24, 10, "----------", "msg", "bar", vec![], Some("❯ "), &[]);
        assert!(seq.starts_with("\x1b7")); // DECSC
        assert!(seq.ends_with("\x1b8")); // DECRC
        // The scroll-region re-assert (DECSTBM) must be saved-then-emitted: it
        // homes the cursor, so it has to sit AFTER the DECSC save and BEFORE the
        // first absolute row paint, or DECRC would restore the homed position
        // and strand the next prompt at the top of the screen.
        let decsc = seq.find("\x1b7").unwrap();
        let region = seq.find("\x1b[1;20r").expect("region re-asserted"); // 24 - 4 = 20
        let first_paint = seq.find("\x1b[21;1H").unwrap();
        assert!(decsc < region && region < first_paint);
        assert!(seq.contains("\x1b[21;1H")); // separator row = H-3
        assert!(seq.contains("\x1b[22;1H")); // input row = H-2
        assert!(seq.contains("\x1b[23;1H")); // status message row = H-1
        assert!(seq.contains("\x1b[24;1H")); // statusline row = H
        assert!(seq.contains("\x1b[2K")); // each row cleared first
        // The input row is the third row up from the bottom, directly BELOW the
        // rule — the fixed home of the prompt in every aish state.
        let rule = seq.find("\x1b[21;1H").unwrap();
        let input = seq.find("\x1b[22;1H").unwrap();
        let msg = seq.find("\x1b[23;1H").unwrap();
        assert!(seq[rule..input].contains("----------"));
        assert!(seq[input..msg].contains("❯ "));
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

        // 24-row window, 6-row footer: banner 19-20, rule 21, input 22, msg 23,
        // bar 24.
        let seq = footer_seq_with(24, 80, "----------", "msg", "bar", banner, Some("❯ "), &[]);
        let esc = seq.find("\x1b[19;1H").expect("escalation row = H-5");
        let worker = seq.find("\x1b[20;1H").expect("worker status row = H-4");
        let rule = seq.find("\x1b[21;1H").expect("separator row = H-3");
        let input = seq.find("\x1b[22;1H").expect("input row = H-2");
        let msg = seq.find("\x1b[23;1H").expect("status message row = H-1");
        let bar = seq.find("\x1b[24;1H").expect("statusline row = H");
        assert!(
            esc < worker && worker < rule && rule < input && input < msg && msg < bar,
            "footer must paint escalation → worker → rule → input → message → statusline"
        );
        // The rule really is the lid: row H-3 carries the separator, and the
        // escalation text lands two rows ABOVE it.
        assert!(
            seq[rule..input].contains("----------"),
            "row H-3 must carry the separator, got {:?}",
            &seq[rule..input]
        );
        assert!(
            seq[esc..worker].contains("escalated"),
            "row H-5 must carry the escalation message, got {:?}",
            &seq[esc..worker]
        );
        // The reserved region includes the spacer (24 - 7 = 17), so the
        // banner can never be scrolled away by body output.
        assert!(
            seq.contains("\x1b[1;17r"),
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
    }

    #[test]
    fn heartbeat_keeps_painting_under_an_attach_view() {
        // THE REGRESSION: a `:attach` worker view used to veto the heartbeat on
        // a stale alternate-screen assumption. `open_attach_view` renders on the
        // PRIMARY buffer and re-asserts the footer region, so the escalation
        // tray is on screen during an attach and its clock/emoji must keep
        // advancing. `ATTACH_ACTIVE` is no longer part of the paint gate at all.
        assert!(heartbeat_should_paint(beat()));
        assert!(heartbeat_should_paint(HeartbeatState {
            idle_elapsed: false,
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

        // Cache a resting prompt the way the line editor does on every read. No
        // footer region is installed under `cargo test`, so `park_input_row`
        // stops right after the cache write and never touches a real terminal.
        park_input_row(prompt);
        set_reading_line(false);

        // Baseline: no override → cached status shows through.
        clear_midturn_input();
        assert_eq!(effective_input_line().as_deref(), Some(prompt));

        // Turn start, nothing typed yet → bare prompt is surfaced.
        set_midturn_input(prompt, "");
        assert_eq!(effective_input_line().as_deref(), Some(prompt));

        // Operator types → prompt + live line replaces the status row.
        set_midturn_input(prompt, "ls -la");
        assert_eq!(effective_input_line(), Some(format!("{prompt}ls -la")));

        // Turn teardown → cached status message restored.
        clear_midturn_input();
        assert_eq!(effective_input_line().as_deref(), Some(prompt));

        // Idempotent: a second clear is a no-op (no panic, stays cleared).
        clear_midturn_input();
        assert_eq!(effective_input_line().as_deref(), Some(prompt));

        // While the editor is actively reading it OWNS the row (it draws the
        // live buffer AND the real cursor there), so a footer repaint must leave
        // the row alone rather than fight the editor for it.
        set_reading_line(true);
        assert_eq!(effective_input_line(), None);
        set_reading_line(false);
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

        // Cache a resting prompt the way the line editor does on every read. No
        // footer region is installed under `cargo test`, so `park_input_row`
        // stops right after the cache write and never touches a real terminal.
        park_input_row(prompt);
        set_reading_line(false);

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
        assert_eq!(effective_input_line().as_deref(), Some(prompt));
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
        // 24 rows: everything fits — rule, input, message, statusline, in that
        // order upward from the bottom.
        let full = FooterLayout::solve(24, 0);
        assert_eq!(full.height, FOOTER_ROWS);
        assert_eq!(
            (full.sep_row, full.input_row, full.msg_row, full.bar_row),
            (Some(21), Some(22), Some(23), Some(24))
        );
        assert_eq!(full.body_bottom, 20);

        // 5 rows: the separator — pure chrome, zero information — sheds FIRST,
        // and the survivors pack contiguously upward so no hole is left behind.
        let tight = FooterLayout::solve(5, 0);
        assert_eq!(tight.height, 3);
        assert_eq!(tight.sep_row, None);
        assert_eq!(
            (tight.input_row, tight.msg_row, tight.bar_row),
            (Some(3), Some(4), Some(5))
        );
        assert_eq!(tight.body_bottom, MIN_BODY_ROWS);

        // 4 rows: the transient status message sheds next (it also prints
        // inline, so nothing is actually lost), leaving statusline + input.
        let tighter = FooterLayout::solve(4, 0);
        assert_eq!(tighter.height, 2);
        assert_eq!((tighter.sep_row, tighter.msg_row), (None, None));
        assert_eq!((tighter.input_row, tighter.bar_row), (Some(3), Some(4)));
        assert_eq!(tighter.body_bottom, MIN_BODY_ROWS);

        // 3 rows: the statusline goes too — informational and re-derivable —
        // and what survives is the one row the operator types into.
        let bare = FooterLayout::solve(3, 0);
        assert_eq!(bare.height, 1);
        assert_eq!(
            (bare.sep_row, bare.msg_row, bare.bar_row),
            (None, None, None)
        );
        assert_eq!(bare.input_row, Some(3));
        assert_eq!(bare.body_bottom, MIN_BODY_ROWS);

        // 2 rows and below: even the input row goes, footer height hits 0, and
        // the caller falls back to inline printing (readline at the body cursor).
        let none = FooterLayout::solve(2, 0);
        assert_eq!(none.height, 0);
        assert_eq!((none.bar_row, none.input_row), (None, None));
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
        assert_eq!(roomy.height, FOOTER_ROWS + per * 3 + 1);
        // top_row must cover the BANNER block, not just the rule — teardown
        // clears from there, and clearing from the rule would strand banners.
        assert_eq!(roomy.top_row(), 60 - roomy.height + 1);

        // A half banner is never reservable: an odd want normalizes DOWN.
        assert_eq!(FooterLayout::solve(60, per * 2 + 1).banner_rows, per * 2);

        // Squeeze: banners shed BEFORE the separator, whole units at a time,
        // and the full 3-row footer survives intact.
        let squeezed = FooterLayout::solve(MIN_FOOTER_ROWS + per + 1, per * 4);
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
                for row in [l.spacer_row, l.sep_row, l.input_row, l.msg_row, l.bar_row]
                    .into_iter()
                    .flatten()
                {
                    assert!(
                        row > l.body_bottom,
                        "rows={rows} want={want}: painted row {row} is inside the body (bottom {})",
                        l.body_bottom
                    );
                    assert!(row <= rows, "rows={rows}: painted row {row} is off-screen");
                }
                // Survivors are contiguous and strictly ordered upward.
                if let (Some(s), Some(i)) = (l.sep_row, l.input_row) {
                    assert_eq!(s + 1, i);
                }
                if let (Some(i), Some(m)) = (l.input_row, l.msg_row) {
                    assert_eq!(i + 1, m);
                }
                if let (Some(m), Some(b)) = (l.msg_row, l.bar_row) {
                    assert_eq!(m + 1, b);
                }
                // A footer that exists ALWAYS keeps the INPUT row — it is the
                // last zone shed, so the operator never loses their prompt.
                assert_eq!(l.enabled(), l.input_row.is_some());
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
        // 24-row window, 3 banners → 10-row footer: rows 15..20 banners, 21
        // rule, 22 input, 23 msg, 24 bar.
        let seq = footer_seq_with(24, 80, "----------", "msg", "bar", banners, Some("❯ "), &[]);
        let newest = seq.find("\x1b[15;1H").expect("newest escalation row");
        let newest_status = seq.find("\x1b[16;1H").expect("newest status row");
        let middle = seq.find("\x1b[17;1H").expect("middle escalation row");
        let oldest = seq.find("\x1b[19;1H").expect("oldest escalation row");
        let rule = seq.find("\x1b[21;1H").expect("separator row = H-3");
        assert!(newest < newest_status && newest_status < middle && middle < oldest);
        assert!(oldest < rule, "banners must all sit above the rule");
        assert!(seq[newest..newest_status].contains("newest"));
        assert!(seq[newest_status..middle].contains("newest status"));
        assert!(seq[rule..].contains("----------"));
        // DECSTBM must reserve eleven footer rows including the spacer.
        assert!(
            seq.contains("\x1b[1;13r"),
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
        // A 24-row window holds three banners, a 10-row window one,
        // and an 8-row window none: the mandatory spacer needs its own row.
        assert_eq!(escalation_rows_that_fit(24, 6), 6);
        assert_eq!(escalation_rows_that_fit(10, 6), 2);
        assert_eq!(escalation_rows_that_fit(8, 6), 0);
        assert_eq!(escalation_rows_that_fit(7, 6), 0);
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

    #[test]
    fn tray_spacer_separates_body_and_pinned_output_even_in_short_windows() {
        for (banners, hints) in [(2, 0), (0, 2), (2, 2)] {
            let layout = FooterLayout::solve_with(24, banners, hints);
            assert_eq!(layout.spacer_row, Some(layout.body_bottom + 1));
            assert_eq!(layout.top_row(), layout.spacer_row.unwrap());
            assert_eq!(layout.input_row, Some(22));
            let seq = footer_seq_with(
                24,
                80,
                "----",
                "msg",
                "bar",
                if banners > 0 {
                    vec![("escalation".into(), "worker".into())]
                } else {
                    vec![]
                },
                None,
                &vec!["hint".into(); hints as usize],
            );
            assert!(seq.contains(&format!("\x1b[{};1H\x1b[2K", layout.spacer_row.unwrap())));
            let first_output = layout.top_row() + 1;
            assert!(seq.contains(&format!(
                "\x1b[{first_output};1H\x1b[2K{}",
                if banners > 0 { "escalation" } else { "hint" }
            )));
            let tight = FooterLayout::solve_with(
                MIN_BODY_ROWS + FOOTER_ROWS + banners + hints + 1,
                banners,
                hints,
            );
            assert!(tight.spacer_row.is_some());
            assert_eq!(tight.banner_rows, banners);
            assert_eq!(tight.hint_rows, hints);
        }
        assert_eq!(FooterLayout::solve(24, 0).spacer_row, None);
    }

    #[test]
    fn hint_tray_packs_flush_between_banners_and_rule() {
        // 24 rows, no banners, 2-row tray → 7-row footer: 18 spacer, 19-20 tray, 21 rule,
        // 22 input, 23 msg, 24 bar. The tray must END one row above the rule.
        let l = FooterLayout::solve_with(24, 0, 2);
        assert_eq!(l.hint_rows, 2);
        assert_eq!(l.sep_row, Some(21));
        assert_eq!(l.hint_top_row(), Some(19));
        assert_eq!(l.input_row, Some(22), "input row must NOT move for a tray");
        assert_eq!(l.height, 7);

        // With a banner pinned, the tray slides down under it — still flush
        // against the rule, still leaving the input row alone.
        let b = FooterLayout::solve_with(24, crate::escalation::ROWS_PER_BANNER, 3);
        assert_eq!(b.hint_rows, 3);
        assert_eq!(b.hint_top_row(), Some(b.sep_row.unwrap() - 3));
        assert_eq!(b.input_row, Some(22));
        // Two visible trays → a rule between them, charged inside the block:
        // 14 spacer, 15-16 banners, 17 RULE, 18-20 tray, 21 rule.
        assert_eq!(b.tray_sep_row, Some(b.top_row() + 1 + b.banner_rows));
        assert_eq!(
            b.hint_top_row(),
            b.tray_sep_row.map(|r| r + 1),
            "tray must hang directly off the rule under the banner block"
        );
        // ...and a single visible tray never pays for a divider.
        assert_eq!(FooterLayout::solve_with(24, 0, 2).tray_sep_row, None);
        assert_eq!(
            FooterLayout::solve_with(24, crate::escalation::ROWS_PER_BANNER, 0).tray_sep_row,
            None
        );
    }

    #[test]
    fn growing_tray_pushes_the_escalation_stack_up() {
        // The `:workers` tray renders THROUGH this zone, so "a tall worker list
        // must push the escalation tray up instead of painting over it" is a
        // property of the solver, not of the caller. Each extra tray row has to
        // lift the whole banner block by exactly one row — with the banners
        // intact, the tray hanging flush off their bottom edge, and the body
        // giving up the row (the footer grows upward into the transcript).
        let per = crate::escalation::ROWS_PER_BANNER;
        let base = FooterLayout::solve_with(40, per, 0);
        for hints in 1..=6u16 {
            let l = FooterLayout::solve_with(40, per, hints);
            assert_eq!(l.hint_rows, hints, "40 rows is roomy enough for {hints}");
            assert_eq!(l.banner_rows, per, "a tray must never cost a banner");
            // Banner block starts one row below the spacer at `top_row()`.
            // A second visible tray also buys the rule BETWEEN them, so the
            // block rises by the tray rows PLUS that one divider row.
            assert_eq!(
                l.top_row() + 1,
                base.top_row() + 1 - (hints + 1),
                "banner block must rise exactly {} row(s)",
                hints + 1
            );
            // ...and the tray fills precisely the gap that opened up, ending on
            // the rule, which itself has not moved.
            assert_eq!(l.tray_sep_row, Some(l.top_row() + 1 + l.banner_rows));
            assert_eq!(l.hint_top_row(), l.tray_sep_row.map(|r| r + 1));
            assert_eq!(l.sep_row, base.sep_row, "the rule is the fixed anchor");
            assert_eq!(l.input_row, base.input_row);
            // The rows came from the transcript, not from another footer zone.
            assert_eq!(l.body_bottom, base.body_bottom - (hints + 1));
        }
    }

    #[test]
    fn hint_tray_sheds_first_and_one_row_at_a_time() {
        // The tray is the cheapest zone to lose (the next keystroke rebuilds
        // it), so a window too short for everything trims the TRAY before it
        // touches a banner, the rule, or the input row.
        let per = crate::escalation::ROWS_PER_BANNER;
        for rows in 0..40u16 {
            for want_hints in [0u16, 1, 4, 12] {
                let l = FooterLayout::solve_with(rows, per, want_hints);
                assert!(l.hint_rows <= want_hints, "{rows}/{want_hints}");
                assert!(l.height <= rows.saturating_sub(MIN_BODY_ROWS));
                // A granted tray implies a rule to anchor it to.
                if l.hint_rows > 0 {
                    assert!(l.sep_row.is_some(), "{rows}/{want_hints}");
                    assert!(l.hint_top_row().is_some());
                }
                // Shed order: no tray row survives once a banner row is gone.
                if l.banner_rows < per {
                    assert_eq!(l.hint_rows, 0, "{rows}/{want_hints}");
                }
            }
        }
        // Concretely: a 10-row window keeps a banner (6-row footer) and trims a
        // 12-row ask down to one row while retaining the mandatory spacer.
        let tight = FooterLayout::solve_with(10, per, 12);
        assert_eq!(tight.banner_rows, per);
        assert_eq!(tight.hint_rows, 1);
        assert_eq!(tight.input_row, Some(8));
    }

    #[test]
    fn zero_hint_rows_is_exactly_the_old_layout() {
        // Back-compat guard: every window size must solve identically with an
        // empty tray, so the common (no palette) path is bit-for-bit unchanged.
        for rows in 0..60u16 {
            for want in [0u16, 2, 4, 6] {
                assert_eq!(
                    FooterLayout::solve_with(rows, want, 0),
                    FooterLayout::solve(rows, want),
                    "{rows}/{want}"
                );
            }
        }
    }

    #[test]
    fn hint_tray_delta_claims_and_releases_body_rows() {
        // Opening a 3-row tray claims 4 body rows including its spacer
        // before the tray paints over them.
        let open = hint_tray_delta(24, 0, 0, 3);
        assert_eq!(open.grow, 4);
        assert_eq!(open.release, 0, "a growing tray releases nothing");
        assert!(
            open.clear_to < open.clear_from,
            "a growing tray moves the footer top UP"
        );

        // Closing it releases them and hands back the rows to clear.
        let close = hint_tray_delta(24, 0, 3, 0);
        assert_eq!(close.grow, 0);
        assert_eq!(close.release, 4, "a shrinking tray gives 3 rows BACK");
        assert!(
            close.clear_from < close.clear_to,
            "a shrinking tray leaves stale rows to clear"
        );
        assert_eq!(
            close.clear_to - close.clear_from,
            4,
            "exactly the rows the tray gave back"
        );

        // No change ⇒ no scroll, no clear.
        let still = hint_tray_delta(24, 0, 2, 2);
        assert_eq!((still.grow, still.release), (0, 0));
        assert_eq!(still.clear_from, still.clear_to);
    }

    #[test]
    fn tray_transitions_preserve_transcript_cells_and_editor_cursor() {
        // Interpret the emitted VT operations, checking screen cells rather
        // than only counts. This catches ordering mistakes in clear/SD/DECSTBM.
        struct Screen {
            cells: Vec<Vec<u8>>,
            row: usize,
            col: usize,
            saved: (usize, usize),
            bottom: usize,
        }
        impl Screen {
            fn scroll(&mut self, down: bool, count: usize) {
                for _ in 0..count.min(self.bottom) {
                    if down {
                        self.cells[..self.bottom].rotate_right(1);
                        self.cells[0].fill(b' ');
                    } else {
                        self.cells[..self.bottom].rotate_left(1);
                        self.cells[self.bottom - 1].fill(b' ');
                    }
                }
            }
            fn apply(&mut self, seq: &str) {
                let bytes = seq.as_bytes();
                let mut i = 0;
                while i < bytes.len() {
                    if bytes[i] == 27 {
                        match bytes[i + 1] {
                            b'7' => {
                                self.saved = (self.row, self.col);
                                i += 2;
                            }
                            b'8' => {
                                (self.row, self.col) = self.saved;
                                i += 2;
                            }
                            b'[' => {
                                let start = i + 2;
                                let mut end = start;
                                while !bytes[end].is_ascii_alphabetic() {
                                    end += 1;
                                }
                                let args: Vec<usize> = seq[start..end]
                                    .split(';')
                                    .map(|n| n.parse().unwrap_or(1))
                                    .collect();
                                let n = args[0];
                                match bytes[end] {
                                    b'H' => {
                                        self.row = n;
                                        self.col = *args.get(1).unwrap_or(&1);
                                    }
                                    b'r' => {
                                        self.bottom = args[1];
                                        self.row = 1;
                                        self.col = 1;
                                    }
                                    b'K' => self.cells[self.row - 1].fill(b' '),
                                    b'S' => self.scroll(false, n),
                                    b'T' => self.scroll(true, n),
                                    b'A' => self.row = self.row.saturating_sub(n).max(1),
                                    b'B' => self.row = (self.row + n).min(self.cells.len()),
                                    other => panic!("unsupported VT operation {other}"),
                                }
                                i = end + 1;
                            }
                            _ => panic!("unsupported escape"),
                        }
                    } else {
                        match bytes[i] {
                            b'\n' if self.row == self.bottom => self.scroll(false, 1),
                            b'\n' => self.row += 1,
                            b'\r' => self.col = 1,
                            c => {
                                self.cells[self.row - 1][self.col - 1] = c;
                                self.col += 1;
                            }
                        }
                        i += 1;
                    }
                }
            }
            fn line(&self, row: u16) -> String {
                String::from_utf8(self.cells[row as usize - 1].clone()).unwrap()
            }
        }
        for (editing, cursor_offset) in [(false, 0usize), (false, 4), (true, 0)] {
            let mut before = FooterLayout::solve(24, 0);
            let mut screen = Screen {
                cells: vec![vec![b' '; 40]; 24],
                row: 20,
                col: 1,
                saved: (20, 1),
                bottom: 20,
            };
            screen.apply("TEXT");
            screen.row -= cursor_offset;
            if editing {
                screen.row = 22;
                screen.col = 7;
            }
            for (banners, hints) in [(2, 0), (4, 0), (4, 3), (2, 3), (0, 3), (0, 1), (0, 0)] {
                let after = FooterLayout::solve_with(24, banners, hints);
                screen.apply(&layout_transition_seq(before, after, editing));
                screen.apply(&footer_seq_with(
                    24,
                    40,
                    "----",
                    "msg",
                    "bar",
                    vec![("banner".into(), "worker".into()); banners as usize / 2],
                    None,
                    &vec!["hint".into(); hints as usize],
                ));
                assert!(screen.line(after.body_bottom).starts_with("TEXT"));
                for row in 1..=after.body_bottom {
                    let line = screen.line(row);
                    assert!(
                        !line.contains("banner")
                            && !line.contains("worker")
                            && !line.contains("hint")
                    );
                }
                // The row that used to be a blank spacer now carries the TOP
                // rule — the lid of the pinned block — so it must hold the rule
                // glyph and never tray text.
                if let Some(row) = after.spacer_row {
                    assert_eq!(screen.line(row).trim(), "----");
                }
                assert_eq!(
                    screen.row,
                    if editing {
                        22
                    } else {
                        after.body_bottom as usize - cursor_offset
                    }
                );
                assert_eq!(screen.col, if editing { 7 } else { 5 });
                before = after;
            }
        }
    }

    #[test]
    fn visible_trays_always_have_exactly_one_gap_at_every_size() {
        for rows in 0..80 {
            for banners in [0, 2, 4, 6] {
                for hints in 0..12 {
                    let layout = FooterLayout::solve_with(rows, banners, hints);
                    if layout.banner_rows > 0 || layout.hint_rows > 0 {
                        assert_eq!(layout.spacer_row, Some(layout.body_bottom + 1));
                        // The tray rule, when charged, is part of the pinned
                        // block and so shifts the first tray row down by one.
                        let first = layout.sep_row.unwrap()
                            - layout.hint_rows
                            - layout.banner_rows
                            - u16::from(layout.tray_sep_row.is_some());
                        assert_eq!(first, layout.body_bottom + 2);
                        if let Some(rule) = layout.tray_sep_row {
                            // Strictly between the trays: last banner row, rule,
                            // first hint row.
                            assert_eq!(rule, layout.body_bottom + 2 + layout.banner_rows);
                            assert!(layout.banner_rows > 0 && layout.hint_rows > 0);
                        }
                    } else {
                        assert_eq!(layout.spacer_row, None);
                    }
                    assert!(layout.height <= rows.saturating_sub(MIN_BODY_ROWS));
                }
            }
        }
    }

    #[test]
    fn tray_open_and_close_are_scroll_symmetric() {
        // The regression: `grow` on open MUST be matched by `release` on close
        // at every window size and tray height, or the body is left parked N
        // rows above the rule and later writes (the operator's own echoed
        // input) land in a blank band above the separator.
        for rows in 8..40u16 {
            for banners in [0u16, 2] {
                for tray in 1..=6u16 {
                    let open = hint_tray_delta(rows, banners, 0, tray);
                    let close = hint_tray_delta(rows, banners, tray, 0);
                    assert_eq!(
                        open.grow, close.release,
                        "rows={rows} banners={banners} tray={tray}"
                    );
                    assert_eq!(open.release, 0);
                    assert_eq!(close.grow, 0);
                    // And the body must land back exactly where it started.
                    assert_eq!(
                        close.body_bottom,
                        FooterLayout::solve_with(rows, banners, 0).body_bottom,
                        "closing the tray restores the original body bottom"
                    );
                }
            }
        }
    }

    #[test]
    fn tray_reglue_widens_the_region_before_scrolling_down() {
        // `SD` is clipped to the CURRENT scroll region, so the DECSTBM widen has
        // to precede it — otherwise the freed rows stay blank.
        let seq = tray_reglue_seq(21, 3);
        let widen = seq.find("\x1b[1;21r").expect("widens the region first");
        let down = seq.find("\x1b[3T").expect("scrolls the body down");
        assert!(widen < down, "region widen must precede the SD");
        let save = seq.find("\x1b7").expect("saves the editor cursor");
        assert!(save < widen, "save must precede DECSTBM's cursor home");
        // Cursor is saved/restored around the scroll so the caller's position
        // (and the footer paint that follows) is untouched.
        assert!(seq.contains("\x1b7") && seq.contains("\x1b8"));

        // Degenerate inputs emit nothing at all.
        assert!(tray_reglue_seq(21, 0).is_empty());
        assert!(tray_reglue_seq(0, 3).is_empty());
    }

    #[test]
    fn footer_paints_hint_rows_above_the_rule() {
        let hints = vec![":quit  leave".to_string(), ":jobs  list".to_string()];
        let seq = footer_seq_with(
            24,
            80,
            "----------",
            "msg",
            "bar",
            vec![],
            Some("❯ "),
            &hints,
        );
        let first = seq.find("\x1b[19;1H").expect("first tray row = H-5");
        let second = seq.find("\x1b[20;1H").expect("second tray row = H-4");
        let rule = seq.find("\x1b[21;1H").expect("separator row = H-3");
        let input = seq.find("\x1b[22;1H").expect("input row = H-2");
        assert!(
            first < second && second < rule,
            "tray stacks above the rule"
        );
        assert!(seq[first..second].contains(":quit"));
        assert!(seq[second..rule].contains(":jobs"));
        assert!(rule < input, "the rule still sits above the input row");
        // The region must reserve the tray and spacer (24 - 7 = 17).
        assert!(
            seq.contains("\x1b[1;17r"),
            "region must cover every tray row"
        );
    }

    #[test]
    fn footer_clips_hints_to_the_rows_the_window_granted() {
        // Ask for more tray than fits: the paint must stop at `hint_rows` rather
        // than write outside the reserved region.
        let hints: Vec<String> = (0..12).map(|i| format!(":cmd{i}")).collect();
        let seq = footer_seq_with(12, 40, "----", "msg", "bar", vec![], Some("❯ "), &hints);
        let layout = FooterLayout::solve_with(12, 0, hints.len() as u16);
        assert!(layout.hint_rows < hints.len() as u16);
        let top = layout.hint_top_row().expect("some tray survived");
        for i in 0..layout.hint_rows {
            assert!(seq.contains(&format!("\x1b[{};1H", top + i)), "row {i}");
        }
        // One past the plan must never be addressed.
        assert!(seq.contains(&format!("\x1b[{};1H\x1b[2K", top - 1)));
    }
}
