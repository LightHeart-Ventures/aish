//! The PINNED ANCHOR — a bottom-anchored stack of footer rows that keeps every
//! in-flight background escalation AND every queued mid-turn command on screen
//! until the agent has actually dealt with it.
//!
//! THE PROBLEM. Two different "it'll happen later" signals used to evaporate the
//! moment they were printed:
//!
//! * An ESCALATION to a background coordinator (`run_in_background`) printed its
//!   launch notice ONCE into the scrolling body ("🚀 escalated to a background
//!   coordinator …") and then scrolled away under the next command's output.
//!   Thirty seconds later the operator had no on-screen evidence anything was
//!   running — the only surviving signal was the `⟳N` prompt pulse and the 🤖
//!   statusline badge, neither of which says WHAT was escalated or HOW it's
//!   doing.
//! * A QUEUED mid-turn line (type-ahead submitted while a turn is running) got a
//!   one-shot `⏳ queued → ls -la` receipt in the body — which likewise scrolls
//!   away. The operator is left wondering whether the shell still intends to run
//!   what they typed, with no way to see the depth of the queue.
//!
//! Both are the SAME shape of promise: *work the shell has accepted but not yet
//! finished*. So both live in the same place.
//!
//! THE FIX. Pin them to the footer instead of the body, as ONE stack that is
//! ANCHORED AT THE BOTTOM and BUILDS UPWARD. While anything is pending,
//! [`crate::terminal::footer_rows_for`] grows the pinned footer by
//! [`extra_footer_rows`] rows and `footer_seq` paints the stack:
//!
//! ```text
//!   🚀 escalated → w_a7k3m2 · build and open pr  <- OLDEST pending (top)
//!      ↳ 1m12s · coordinating · 🔧 read_file …   <- its latest worker status
//!   ✅ queued → git status                       <- processed: green ✓, dwelling
//!   ⏳ queued #2 → cargo test                    <- NEWEST pending (the anchor)
//!   ─────────────────────────────────────────   <- separator (the statusline lid)
//!   ⇄ detached — back to interactive …           <- SecondStatusLine
//!   aish v0.9 · sonnet …           12:04:51      <- statusline
//! ```
//!
//! The stack sits ABOVE the footer's top horizontal bar, and its BOTTOM edge is
//! welded to that bar: new entries land on the bottom row and push older ones up,
//! so the freshest promise is always in the operator's eye-line — exactly where
//! it already lives — and the block grows bottom→up into the body. The rule is
//! the LID of the statusline block, so painting a pinned row UNDER it read as a
//! row wedged inside the statusline frame; above it the stack reads as the last
//! thing the body said, while the rule stays welded to the two statusline rows it
//! opens whether or not anything is pinned. Because the stack is inside the
//! DECSTBM-reserved region it can never scroll away.
//!
//! ANIMATION. A pending entry's leading glyph cycles in place — escalations
//! through [`FRAMES`] on a [`FRAME_MS`] cadence, queued lines through
//! [`QUEUED_FRAMES`] on the slower [`QUEUED_FRAME_MS`] — the same "something is
//! happening" affordance as the thinking spinner (`ThinkingSpinner`). The frame
//! is a PURE function of elapsed time ([`frame_at`] / [`queued_frame_at`]), so
//! every repaint path (idle heartbeat, mid-turn draw, resize) lands on the same
//! frame without any shared animation cursor. Every frame is a 2-column emoji so
//! the text after it never jitters (pinned by `frames_are_uniform_width`).
//!
//! LIFECYCLE. Motion means "still pending". The instant an entry is DONE — the
//! coordinator reached a terminal state, or the queued line was popped and run —
//! its glyph freezes to a STATIC GREEN ✅ (a failed escalation freezes to ⚠️
//! instead: green must never claim success it didn't earn), and the entry
//! self-retires [`DWELL`] (30s) later so the verdict is readable but the footer
//! shrinks back on its own. Entry-level hooks:
//!
//! | event                        | call                              |
//! |------------------------------|-----------------------------------|
//! | escalation launched          | [`pin`]                           |
//! | worker status changed        | [`set_status`]                    |
//! | coordinator finished         | [`mark_terminal`] / [`note_terminal`] |
//! | mid-turn line submitted      | [`pin_queued`]                    |
//! | queued line popped and run   | [`resolve_queued_text`]           |
//! | every footer refresh         | [`sweep`]                         |
//!
//! BOUNDED. A burst of type-ahead must never eat the screen, so the stack is
//! capped two ways: [`MAX_ENTRIES`] live entries are retained at all (oldest
//! pending entries are dropped first), and [`render_rows`] takes a row BUDGET
//! derived from the real terminal height — when the stack doesn't fit, the
//! NEWEST entries win and the top row degrades to a `⋯ +N more` marker.

use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Footer rows a single ESCALATION entry occupies: the escalation message and
/// the worker-status row directly below it.
pub const ROWS: u16 = 2;

/// Footer rows a single QUEUED-INPUT entry occupies. One: the command text is
/// the whole story, and type-ahead arrives in bursts, so a second row per entry
/// would cost twice the screen for no extra signal.
pub const QUEUED_ROWS: u16 = 1;

/// Animation frames for the escalation glyph, cycled in place like the thinking
/// spinner's braille frames. EVERY frame is a single 2-column emoji
/// (`Extended_Pictographic`, East-Asian Wide) so the text following it never
/// shifts between frames — pinned by `frames_are_uniform_width`.
pub const FRAMES: [&str; 4] = ["🚀", "🛸", "🌠", "✨"];

/// Animation frames for a PENDING queued-input entry: an hourglass flipping back
/// and forth. Same 2-column invariant as [`FRAMES`] (both U+23F3 and U+231B are
/// East-Asian Wide) — pinned by `queued_frames_are_uniform_width`.
pub const QUEUED_FRAMES: [&str; 2] = ["⏳", "⌛"];

/// Milliseconds per escalation animation frame. ~4.5 fps: clearly alive, cheap
/// enough that the footer heartbeat can drive it from a sleep loop (see
/// `terminal::spawn_footer_heartbeat`).
pub const FRAME_MS: u64 = 220;

/// Milliseconds per queued-input frame. Deliberately SLOWER than [`FRAME_MS`]:
/// [`QUEUED_FRAMES`] is a two-frame cycle, and flipping a two-frame set at the
/// escalation cadence reads as a strobe rather than a wait.
pub const QUEUED_FRAME_MS: u64 = FRAME_MS * 2;

/// How long a FINISHED entry stays pinned, showing its static verdict glyph,
/// before it retires from the stack and the footer shrinks back. Long enough to
/// read the outcome after glancing away, short enough that the footer doesn't
/// stay fat forever.
pub const DWELL: Duration = Duration::from_secs(30);

/// Hard cap on retained entries. A runaway escalation loop or a pasted wall of
/// type-ahead must not grow this stack without bound; past the cap the oldest
/// entries are dropped (they are the ones the operator has had longest to see).
pub const MAX_ENTRIES: usize = 12;

/// Scrolling body rows the anchor must always leave above the footer. A cramped
/// window keeps its OUTPUT: the anchor shrinks (or vanishes) rather than letting
/// a notification eat the terminal.
pub const MIN_BODY_ROWS: u16 = 2;

/// Max visible width of the task hint on the escalation row.
const TASK_HINT_MAX: usize = 56;

/// Max visible width of the queued command text. Wider than the task hint: the
/// whole point of the row is to show the operator exactly what will run.
const QUEUED_TEXT_MAX: usize = 88;

/// Max visible width of the composed status row (the terminal clip in
/// `footer_seq` is the hard backstop; this keeps the row from crowding out the
/// `↳` prefix on a narrow window).
pub const STATUS_MAX: usize = 110;

/// What an anchor entry is waiting on. Decides the glyph set, the row count, and
/// the row's leading label.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    /// Work handed to a background coordinator — two rows (message + status).
    Escalation,
    /// A mid-turn line the operator submitted, waiting to be popped and run.
    Queued,
}

impl Kind {
    /// Footer rows an entry of this kind occupies.
    pub fn rows(self) -> u16 {
        match self {
            Kind::Escalation => ROWS,
            Kind::Queued => QUEUED_ROWS,
        }
    }
}

/// One pending promise in the anchor stack.
struct Entry {
    /// Stable key. For escalations the coordinator run id (`w_…`), rendered
    /// short and used by the REPL to find the live worker when it refreshes the
    /// status row. For queued lines a synthetic `q<N>`.
    id: String,
    kind: Kind,
    /// The escalated task, or the queued command text. Rendered as a compact
    /// one-line hint.
    label: String,
    /// When the entry was pinned — drives the animation frame and the runtime.
    pinned_at: Instant,
    /// When the entry reached a terminal state (coordinator done/failed, or the
    /// queued line processed), if it has. Freezes the glyph and starts the
    /// [`DWELL`] retirement clock.
    terminal_at: Option<Instant>,
    /// True when the terminal outcome was a failure (⚠️ instead of green ✅).
    failed: bool,
    /// Latest composed worker-status text for an escalation's second row.
    status: String,
}

impl Entry {
    /// True once the entry has a frozen verdict and is only dwelling.
    fn done(&self) -> bool {
        self.terminal_at.is_some()
    }
}

/// The anchor stack, OLDEST first. Rendered oldest→newest top→bottom, so the
/// newest entry sits at the bottom (the anchor) and the block builds upward.
static STACK: Mutex<Vec<Entry>> = Mutex::new(Vec::new());

/// Monotonic counter behind the synthetic `q<N>` ids for queued entries, so two
/// identical command lines are still distinct entries.
static QUEUE_SEQ: Mutex<u64> = Mutex::new(0);

/// Run `f` against the live stack. Every mutator funnels through here so a
/// poisoned lock degrades to a no-op instead of a panic in a paint path.
fn with<R>(f: impl FnOnce(&mut Vec<Entry>) -> R) -> Option<R> {
    STACK.lock().ok().map(|mut g| f(&mut g))
}

/// Drop the oldest entries until at most [`MAX_ENTRIES`] remain. Finished
/// entries are shed first (their verdict has already been shown) before any
/// still-pending promise is sacrificed.
fn enforce_cap(stack: &mut Vec<Entry>) {
    while stack.len() > MAX_ENTRIES {
        let victim = stack
            .iter()
            .position(Entry::done)
            .unwrap_or(0);
        stack.remove(victim);
    }
}

/// Pin an escalation for coordinator `id` running `task`. Re-pinning the SAME id
/// refreshes it in place rather than stacking a duplicate; a new id lands at the
/// bottom of the stack as the newest promise.
pub fn pin(id: &str, task: &str) {
    with(|stack| {
        if let Some(e) = stack.iter_mut().find(|e| e.id == id) {
            e.label = task.to_string();
            e.terminal_at = None;
            e.failed = false;
            return;
        }
        stack.push(Entry {
            id: id.to_string(),
            kind: Kind::Escalation,
            label: task.to_string(),
            pinned_at: Instant::now(),
            terminal_at: None,
            failed: false,
            status: "queued — waiting for the coordinator to start".into(),
        });
        enforce_cap(stack);
    });
}

/// Pin a QUEUED mid-turn command line, returning its synthetic entry id.
///
/// Called the instant the operator presses Enter during a turn (see
/// `keywatch`'s `Action::Submit`), alongside the durable body receipt from
/// [`crate::terminal::print_midturn_queued`]. The body receipt is the scrollback
/// record; THIS is the live "still waiting to run" affordance, and it survives
/// until the REPL actually pops the line.
pub fn pin_queued(text: &str) -> String {
    let n = QUEUE_SEQ
        .lock()
        .map(|mut g| {
            *g += 1;
            *g
        })
        .unwrap_or(0);
    let id = format!("q{n}");
    let key = id.clone();
    with(|stack| {
        stack.push(Entry {
            id: key,
            kind: Kind::Queued,
            label: text.to_string(),
            pinned_at: Instant::now(),
            terminal_at: None,
            failed: false,
            status: String::new(),
        });
        enforce_cap(stack);
    });
    id
}

/// Mark a queued entry PROCESSED by id: freezes it to the static green ✅ and
/// starts the [`DWELL`] retirement clock.
///
/// The REPL drains type-ahead as bare strings, so [`resolve_queued_text`] is the
/// hook that actually fires today; this id-scoped twin is kept for callers that
/// DO hold the id handed back by [`pin_queued`].
#[allow(dead_code)]
pub fn resolve_queued(id: &str) {
    with(|stack| {
        if let Some(e) = stack
            .iter_mut()
            .find(|e| e.kind == Kind::Queued && e.id == id)
        {
            e.terminal_at.get_or_insert_with(Instant::now);
        }
    });
}

/// Mark a queued entry processed by its TEXT — the practical hook, because the
/// REPL pops a bare `String` off its type-ahead deque and never carried the
/// entry id along. Resolves the OLDEST still-pending entry whose label matches,
/// which is exactly right: type-ahead drains in submission order, so two
/// identical lines retire in the order they were typed.
pub fn resolve_queued_text(text: &str) {
    with(|stack| {
        if let Some(e) = stack
            .iter_mut()
            .find(|e| e.kind == Kind::Queued && !e.done() && e.label == text)
        {
            e.terminal_at = Some(Instant::now());
        }
    });
}

/// Retire the whole stack (footer shrinks back on the next paint). Teardown
/// escape hatch — entries normally retire themselves via [`sweep`].
#[allow(dead_code)]
pub fn clear() {
    with(|stack| stack.clear());
}

/// True while ANYTHING is in the stack — the gate
/// [`crate::terminal::footer_rows_for`] consults to decide the footer height.
/// The height decision now routes through [`extra_footer_rows`] (which needs the
/// DEPTH, not just a yes/no), so this stays as the cheap emptiness probe.
#[allow(dead_code)]
pub fn active() -> bool {
    with(|stack| !stack.is_empty()).unwrap_or(false)
}

/// True while ANY entry is still pending (pinned and not yet terminal). The
/// footer heartbeat consults this to bypass its idle gate and repaint on the
/// [`FRAME_MS`] cadence — motion is the "still pending" signal, so it must keep
/// ticking while the shell sits idle at the prompt. A stack of nothing but
/// dwelling ✅s is static, so the heartbeat can stand down.
pub fn animating() -> bool {
    with(|stack| stack.iter().any(|e| !e.done())).unwrap_or(false)
}

/// The run id of the newest still-live ESCALATION, if any. The REPL uses it to
/// locate the live worker whose status it then feeds back via [`set_status`].
pub fn pinned_id() -> Option<String> {
    with(|stack| {
        stack
            .iter()
            .rev()
            .find(|e| e.kind == Kind::Escalation && !e.done())
            .or_else(|| stack.iter().rev().find(|e| e.kind == Kind::Escalation))
            .map(|e| e.id.clone())
    })
    .flatten()
}

/// Total footer rows the stack WANTS, before any terminal-height clamp.
pub fn rows_used() -> u16 {
    with(|stack| stack.iter().map(|e| e.kind.rows()).sum())
        .unwrap_or(0)
}

/// Replace the status text of the newest live escalation (already composed by
/// the caller — the REPL owns the worker list). No-op when no escalation is
/// pinned.
pub fn set_status(status: &str) {
    with(|stack| {
        if let Some(e) = stack
            .iter_mut()
            .rev()
            .find(|e| e.kind == Kind::Escalation && !e.done())
        {
            e.status = status.to_string();
        }
    });
}

/// Record that the newest live escalation reached a terminal state: freezes the
/// glyph and starts the [`DWELL`] retirement clock. Idempotent — the first call
/// wins, so the dwell measures from the real finish. Superseded in-tree by the
/// id-scoped [`note_terminal`] (several escalations can be live at once), kept
/// for callers that only know "the newest one finished".
#[allow(dead_code)]
pub fn mark_terminal(failed: bool) {
    with(|stack| {
        if let Some(e) = stack
            .iter_mut()
            .rev()
            .find(|e| e.kind == Kind::Escalation && !e.done())
        {
            e.failed = failed;
            e.terminal_at.get_or_insert_with(Instant::now);
        }
    });
}

/// Id-scoped [`mark_terminal`]: freeze ONLY the entry whose id is `id`. Called
/// from a worker's own completion paths so a finished worker can't steal another
/// entry's verdict.
pub fn note_terminal(id: &str, failed: bool) {
    with(|stack| {
        if let Some(e) = stack.iter_mut().find(|e| e.id == id) {
            e.failed = failed;
            e.terminal_at.get_or_insert_with(Instant::now);
        }
    });
}

/// Retire every finished entry whose [`DWELL`] has elapsed. Called on each
/// footer refresh; cheap no-op while everything is still pending.
pub fn sweep() {
    with(|stack| {
        stack.retain(|e| match e.terminal_at {
            Some(t) => t.elapsed() < DWELL,
            None => true,
        });
    });
}

/// The escalation animation frame for a given elapsed time. Pure, so every
/// repaint path (heartbeat, mid-turn draw, resize) derives the SAME frame from
/// the clock instead of sharing a mutable cursor.
pub fn frame_at(elapsed_ms: u64) -> &'static str {
    FRAMES[(elapsed_ms / FRAME_MS) as usize % FRAMES.len()]
}

/// The queued-input animation frame for a given elapsed time. Same purity
/// contract as [`frame_at`], on the slower [`QUEUED_FRAME_MS`] cadence.
pub fn queued_frame_at(elapsed_ms: u64) -> &'static str {
    QUEUED_FRAMES[(elapsed_ms / QUEUED_FRAME_MS) as usize % QUEUED_FRAMES.len()]
}

/// Drop ANSI SGR/CSI sequences. The status row is composed from the worker's
/// forwarded activity lines, which arrive pre-colorized; left in place their
/// embedded resets would terminate the row's dim styling mid-line and leak color
/// into the footer. Stripping also keeps the width math honest.
fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\x1b' {
            out.push(c);
            continue;
        }
        // CSI (`ESC [` … final byte @-~) and the short two-char sequences
        // (`ESC 7`, `ESC 8`, …) are the only forms the forwarder emits.
        match chars.peek() {
            Some('[') => {
                chars.next();
                for c in chars.by_ref() {
                    if ('\x40'..='\x7e').contains(&c) {
                        break;
                    }
                }
            }
            Some(_) => {
                chars.next();
            }
            None => break,
        }
    }
    out
}

/// A compact elapsed-time label (`12s`, `1m12s`, `1h04m`) for the status row.
/// Rendered from the entry's own clock on EVERY paint — including the idle
/// heartbeat's — so the runtime ticks even when the REPL isn't repainting.
pub fn fmt_elapsed(ms: u64) -> String {
    let secs = ms / 1000;
    match secs {
        s if s < 60 => format!("{s}s"),
        s if s < 3600 => format!("{}m{:02}s", s / 60, s % 60),
        s => format!("{}h{:02}m", s / 3600, (s % 3600) / 60),
    }
}

/// Collapse `text` to a single line and truncate to `max` visible chars with a
/// trailing ellipsis. Shared by the task hint, the queued command text, and the
/// status row so none of them can wrap the pinned footer.
pub fn one_line(text: &str, max: usize) -> String {
    let plain = strip_ansi(text);
    let collapsed = plain.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() <= max {
        collapsed
    } else {
        let head: String = collapsed.chars().take(max.saturating_sub(1)).collect();
        format!("{}…", head.trim_end())
    }
}

/// The frozen verdict glyph for a finished entry. Success is a STATIC GREEN ✅
/// — static because motion is reserved for "still pending", green because the
/// promise was kept. Failure stays ⚠️: green must never claim an outcome it
/// didn't earn.
fn verdict_glyph(failed: bool) -> &'static str {
    if failed { "⚠️" } else { "✅" }
}

/// Build the two rows of an ESCALATION entry. Pure: the caller supplies the
/// elapsed clock, the identity, the status text, and the terminal verdict, so
/// the rendering is unit-testable without touching the global stack or a TTY.
///
/// Returns `(escalation_row, status_row)`. The escalation row leads with the
/// animated glyph; the status row is indented under it with a `↳` so the two
/// read as one block.
pub fn render(
    elapsed_ms: u64,
    id: &str,
    task: &str,
    status: &str,
    terminal: Option<bool>,
    color_on: bool,
) -> (String, String) {
    use crate::style::{Color, paint_with};
    let glyph = match terminal {
        None => frame_at(elapsed_ms),
        Some(failed) => verdict_glyph(failed),
    };
    let short = crate::batch::short_id(id);
    let hint = one_line(task, TASK_HINT_MAX);
    let head = if hint.is_empty() {
        format!("{glyph} escalated → {short}")
    } else {
        format!("{glyph} escalated → {short} · {hint}")
    };
    // A kept promise goes green; a live one stays cyan; a broken one goes yellow.
    let tint = match terminal {
        None => Color::Cyan,
        Some(false) => Color::Green,
        Some(true) => Color::Yellow,
    };
    let top = paint_with(&head, tint, color_on);
    let body = format!(
        "   ↳ {} · {}",
        fmt_elapsed(elapsed_ms),
        one_line(status, STATUS_MAX)
    );
    let bottom = paint_with(&body, Color::Dim, color_on);
    (top, bottom)
}

/// Build the single row of a QUEUED-INPUT entry. Pure, same contract as
/// [`render`].
///
/// `position` is the 1-based submission index shown as `#N` (omitted for the
/// first), mirroring [`crate::terminal::midturn_queued_seq`] so the footer row
/// and the body receipt read as the same event. `terminal` is `Some(false)` once
/// the line has been popped and run — the static green ✅ dwell state.
pub fn render_queued(
    elapsed_ms: u64,
    text: &str,
    position: usize,
    terminal: Option<bool>,
    color_on: bool,
) -> String {
    use crate::style::{Color, paint_with};
    let glyph = match terminal {
        None => queued_frame_at(elapsed_ms),
        Some(failed) => verdict_glyph(failed),
    };
    let pos = if position > 1 {
        format!(" #{position}")
    } else {
        String::new()
    };
    let row = format!(
        "{glyph} queued{pos} → {}",
        one_line(text, QUEUED_TEXT_MAX)
    );
    // Green once processed, dim while it waits — a queued line is a promise the
    // shell has made, not an event demanding attention.
    let tint = match terminal {
        None => Color::Dim,
        Some(false) => Color::Green,
        Some(true) => Color::Yellow,
    };
    paint_with(&row, tint, color_on)
}

/// The EXTRA footer rows the anchor should be given for a terminal `term_rows`
/// tall, on top of the footer's `base` height.
///
/// All of the clamping lives here rather than in [`crate::terminal`] so the
/// height decision and the row rendering can never disagree: `footer_rows_for`
/// reserves exactly this many rows and [`render_rows`] is handed the same number
/// as its budget. The anchor never takes the last [`MIN_BODY_ROWS`] scrolling
/// rows — a cramped window keeps its output.
pub fn extra_footer_rows(term_rows: u16, base: u16) -> u16 {
    let budget = term_rows.saturating_sub(base.saturating_add(MIN_BODY_ROWS));
    rows_used().min(budget)
}

/// Render the anchor stack into at most `budget` rows, top row first.
///
/// Order is oldest→newest, so the NEWEST entry occupies the bottom row — the
/// anchor welded to the statusline block — and the stack builds upward. When the
/// stack doesn't fit the budget the newest entries win (they're the ones the
/// operator hasn't absorbed yet) and the top row degrades to a `⋯ +N more`
/// marker so the hidden depth is never silent.
///
/// Reads the global stack and derives each entry's animation frame from its own
/// clock, so any paint path — including the idle heartbeat, which has no
/// `Session` — can render it.
pub fn render_rows(budget: u16, color_on: bool) -> Vec<String> {
    use crate::style::{Color, paint_with};
    if budget == 0 {
        return Vec::new();
    }
    let budget = budget as usize;
    let Some(blocks) = with(|stack| {
        // Number the queued entries in submission order so the `#N` on the row
        // matches the body receipt the operator already saw.
        let mut qpos = 0usize;
        stack
            .iter()
            .map(|e| {
                let elapsed = e.pinned_at.elapsed().as_millis() as u64;
                let terminal = e.terminal_at.map(|_| e.failed);
                match e.kind {
                    Kind::Escalation => {
                        let (top, bottom) =
                            render(elapsed, &e.id, &e.label, &e.status, terminal, color_on);
                        vec![top, bottom]
                    }
                    Kind::Queued => {
                        qpos += 1;
                        vec![render_queued(elapsed, &e.label, qpos, terminal, color_on)]
                    }
                }
            })
            .collect::<Vec<_>>()
    }) else {
        return Vec::new();
    };

    // Take whole entries from the NEWEST end until the budget is spent, so an
    // escalation is never shown as an orphaned status row without its message.
    let mut kept: Vec<Vec<String>> = Vec::new();
    let mut used = 0usize;
    let mut dropped = 0usize;
    for block in blocks.iter().rev() {
        if used + block.len() <= budget {
            used += block.len();
            kept.push(block.clone());
        } else {
            dropped += 1;
        }
    }
    kept.reverse();
    let mut out: Vec<String> = kept.into_iter().flatten().collect();

    if dropped > 0 {
        let marker = paint_with(&format!("   ⋯ +{dropped} more"), Color::Dim, color_on);
        if out.len() < budget {
            out.insert(0, marker);
        } else if !out.is_empty() {
            // Budget is exactly full — spend the top row on the marker rather
            // than hiding the fact that entries are off-screen. The displaced
            // entry's remaining rows would read as an orphan, so drop its block.
            out.remove(0);
            out.insert(0, marker);
        }
    }
    out
}

/// Test-only serialization handle for the process-global anchor stack.
///
/// Shared crate-wide (not private to this module's `tests`) because the
/// footer-paint tests in [`crate::terminal`] mutate the SAME global: a pinned
/// entry changes the footer's height and row plan, so an anchor test
/// interleaving with a footer-geometry test would flap. Every test that pins,
/// clears, or asserts on anchor-dependent geometry takes this lock.
#[cfg(test)]
pub(crate) fn test_lock() -> std::sync::MutexGuard<'static, ()> {
    static L: Mutex<()> = Mutex::new(());
    L.lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use unicode_width::UnicodeWidthStr;

    /// Serializes the tests that mutate the process-global stack so they can't
    /// interleave with each other under the test harness's thread pool.
    fn lock() -> std::sync::MutexGuard<'static, ()> {
        super::test_lock()
    }

    #[test]
    fn frames_are_uniform_width() {
        // Every animation frame must occupy the SAME number of columns, or the
        // text after the emoji jitters left/right on each tick — the whole reason
        // we animate a curated frame set instead of arbitrary emoji.
        for f in FRAMES {
            assert_eq!(f.width(), 2, "frame {f:?} is not 2 columns wide");
        }
    }

    #[test]
    fn queued_frames_are_uniform_width() {
        // Same invariant for the queued hourglass: a width change between frames
        // would shove the command text sideways on every flip.
        for f in QUEUED_FRAMES {
            assert_eq!(f.width(), 2, "queued frame {f:?} is not 2 columns wide");
        }
    }

    #[test]
    fn frame_advances_on_cadence_and_wraps() {
        assert_eq!(frame_at(0), FRAMES[0]);
        assert_eq!(frame_at(FRAME_MS - 1), FRAMES[0]);
        assert_eq!(frame_at(FRAME_MS), FRAMES[1]);
        assert_eq!(frame_at(FRAME_MS * 3), FRAMES[3]);
        // Wraps cleanly — a long-running escalation keeps animating forever.
        assert_eq!(frame_at(FRAME_MS * 4), FRAMES[0]);
        assert_eq!(frame_at(FRAME_MS * 4 + FRAME_MS), FRAMES[1]);
    }

    #[test]
    fn queued_frame_advances_on_its_own_slower_cadence() {
        // The queued cycle is deliberately slower than the escalation cycle: a
        // two-frame set at FRAME_MS strobes.
        assert!(QUEUED_FRAME_MS > FRAME_MS);
        assert_eq!(queued_frame_at(0), QUEUED_FRAMES[0]);
        assert_eq!(queued_frame_at(QUEUED_FRAME_MS - 1), QUEUED_FRAMES[0]);
        assert_eq!(queued_frame_at(QUEUED_FRAME_MS), QUEUED_FRAMES[1]);
        assert_eq!(queued_frame_at(QUEUED_FRAME_MS * 2), QUEUED_FRAMES[0]);
    }

    #[test]
    fn live_banner_animates_and_terminal_banner_freezes() {
        let (top, _) = render(0, "w_abcdef123456", "build it", "coordinating", None, false);
        assert!(top.starts_with(FRAMES[0]), "{top}");
        let (top, _) = render(FRAME_MS, "w_abcdef123456", "build it", "x", None, false);
        assert!(top.starts_with(FRAMES[1]), "{top}");
        // Terminal verdicts freeze the glyph: motion means "still working".
        let (ok, _) = render(
            FRAME_MS * 7,
            "w_abcdef123456",
            "b",
            "done",
            Some(false),
            false,
        );
        assert!(ok.starts_with("✅"), "{ok}");
        let (bad, _) = render(
            FRAME_MS * 7,
            "w_abcdef123456",
            "b",
            "failed",
            Some(true),
            false,
        );
        assert!(bad.starts_with("⚠️"), "{bad}");
    }

    #[test]
    fn finished_escalation_checkmark_is_green_and_static() {
        // The kept-promise glyph must be GREEN (color_on) and must not move as
        // the clock advances — static is how "done" differs from "working".
        let (a, _) = render(0, "w_1", "t", "done", Some(false), true);
        let (b, _) = render(FRAME_MS * 9, "w_1", "t", "done", Some(false), true);
        assert!(a.contains("\x1b[32m"), "verdict row is not green: {a:?}");
        assert!(a.starts_with("\x1b[32m✅"), "{a:?}");
        // Same glyph at a wildly different clock → frozen.
        assert_eq!(
            a.chars().take(8).collect::<String>(),
            b.chars().take(8).collect::<String>()
        );
    }

    #[test]
    fn rows_carry_short_id_task_hint_and_indented_status() {
        let (top, bottom) = render(
            0,
            "w_abcdef123456",
            "build   and\nopen pr",
            "coordinating · 1m12s",
            None,
            false,
        );
        // Short id (not the full run id) keeps the row readable.
        assert!(
            top.contains(&crate::batch::short_id("w_abcdef123456")),
            "{top}"
        );
        // Internal whitespace/newlines collapse so the hint can't wrap the footer.
        assert!(top.contains("build and open pr"), "{top}");
        // The status row is indented under the message with a `↳`.
        assert!(bottom.starts_with("   ↳ "), "{bottom}");
        assert!(bottom.contains("coordinating · 1m12s"), "{bottom}");
    }

    #[test]
    fn long_task_and_status_are_truncated() {
        let task = "x".repeat(400);
        let status = "y".repeat(400);
        let (top, bottom) = render(0, "w_a", &task, &status, None, false);
        assert!(top.contains('…'), "{top}");
        assert!(top.chars().count() < 120, "escalation row too wide: {top}");
        assert!(bottom.contains('…'), "{bottom}");
        assert!(bottom.chars().count() < STATUS_MAX + 16, "{bottom}");
    }

    #[test]
    fn queued_row_animates_then_freezes_green_with_position() {
        let live = render_queued(0, "cargo test", 1, None, false);
        assert!(live.starts_with(QUEUED_FRAMES[0]), "{live}");
        assert!(live.contains("queued → cargo test"), "{live}");
        // Position is omitted for the first submission, shown from #2 on — the
        // same convention as the body receipt.
        assert!(!live.contains('#'), "{live}");
        let second = render_queued(0, "git status", 2, None, false);
        assert!(second.contains("queued #2 → git status"), "{second}");
        // Processed → static green checkmark.
        let done = render_queued(QUEUED_FRAME_MS * 5, "cargo test", 1, Some(false), true);
        assert!(done.starts_with("\x1b[32m✅"), "{done:?}");
    }

    #[test]
    fn long_queued_text_is_truncated_to_one_line() {
        let text = format!("echo {}\nand more", "z".repeat(400));
        let row = render_queued(0, &text, 1, None, false);
        assert!(row.contains('…'), "{row}");
        assert!(row.chars().count() < QUEUED_TEXT_MAX + 24, "{row}");
    }

    #[test]
    fn pin_then_clear_toggles_active_and_rows() {
        let _g = lock();
        clear();
        assert!(!active());
        assert!(render_rows(8, false).is_empty());

        pin("w_deadbeefcafe", "ship the thing");
        assert!(active());
        assert_eq!(pinned_id().as_deref(), Some("w_deadbeefcafe"));
        assert_eq!(rows_used(), ROWS);
        let rows = render_rows(8, false);
        assert_eq!(rows.len(), 2);
        assert!(rows[0].contains("escalated"), "{:?}", rows[0]);
        // Fresh entries state the pre-start truth rather than inventing progress.
        assert!(rows[1].contains("queued"), "{:?}", rows[1]);

        set_status("coordinating · 12s · 🔧 read_file");
        let rows = render_rows(8, false);
        assert!(rows[1].contains("coordinating · 12s"), "{:?}", rows[1]);

        clear();
        assert!(!active());
        assert!(render_rows(8, false).is_empty());
    }

    #[test]
    fn queued_entry_pins_and_resolves_by_text() {
        let _g = lock();
        clear();
        let id = pin_queued("ls -la");
        assert!(active());
        assert_eq!(rows_used(), QUEUED_ROWS);
        assert!(animating(), "a pending queued line must keep the glyph moving");
        let rows = render_rows(8, false);
        assert_eq!(rows.len(), 1);
        assert!(rows[0].contains("queued → ls -la"), "{:?}", rows[0]);

        // Processed → frozen verdict, still pinned for the dwell.
        resolve_queued_text("ls -la");
        assert!(active(), "a processed line dwells before it retires");
        assert!(!animating(), "a processed line must stop animating");
        let rows = render_rows(8, false);
        assert!(rows[0].starts_with("✅"), "{:?}", rows[0]);

        // Resolving by id is idempotent and does not revive the clock.
        resolve_queued(&id);
        assert!(!animating());
        clear();
    }

    #[test]
    fn resolve_queued_text_retires_oldest_duplicate_first() {
        let _g = lock();
        clear();
        pin_queued("make");
        pin_queued("make");
        // Type-ahead drains in submission order, so the FIRST `make` resolves.
        resolve_queued_text("make");
        let rows = render_rows(8, false);
        assert_eq!(rows.len(), 2);
        assert!(rows[0].starts_with("✅"), "oldest should be done: {:?}", rows[0]);
        assert!(
            rows[1].starts_with(QUEUED_FRAMES[0]) || rows[1].starts_with(QUEUED_FRAMES[1]),
            "newest should still be pending: {:?}",
            rows[1]
        );
        assert!(animating());
        clear();
    }

    #[test]
    fn stack_builds_bottom_up_newest_at_the_anchor() {
        let _g = lock();
        clear();
        pin("w_first", "first escalation");
        pin_queued("second thing");
        pin_queued("third thing");
        // Oldest first, newest last: the BOTTOM row is the freshest promise and
        // the block grows upward into the body.
        let rows = render_rows(8, false);
        assert_eq!(rows.len(), 4, "{rows:?}");
        assert!(rows[0].contains("first escalation"), "{:?}", rows[0]);
        assert!(rows[1].starts_with("   ↳ "), "{:?}", rows[1]);
        assert!(rows[2].contains("second thing"), "{:?}", rows[2]);
        assert!(rows[3].contains("third thing"), "{:?}", rows[3]);
        clear();
    }

    #[test]
    fn render_rows_keeps_the_newest_and_marks_the_hidden_depth() {
        let _g = lock();
        clear();
        pin_queued("one");
        pin_queued("two");
        pin_queued("three");
        pin_queued("four");
        // Budget of 2 → the two NEWEST survive, minus the top row spent on the
        // "+N more" marker so hidden depth is never silent.
        let rows = render_rows(2, false);
        assert_eq!(rows.len(), 2, "{rows:?}");
        assert!(rows[0].contains("more"), "{:?}", rows[0]);
        assert!(rows[1].contains("four"), "{:?}", rows[1]);
        // A zero budget renders nothing at all rather than a partial block.
        assert!(render_rows(0, false).is_empty());
        clear();
    }

    #[test]
    fn render_rows_never_orphans_an_escalation_status_row() {
        let _g = lock();
        clear();
        pin("w_a", "two row entry");
        // One row of budget cannot hold a 2-row escalation; rather than paint a
        // bare `↳` status with no message, the entry is dropped and only the
        // marker shows.
        let rows = render_rows(1, false);
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert!(rows[0].contains("more"), "{:?}", rows[0]);
        clear();
    }

    #[test]
    fn extra_footer_rows_leaves_the_body_room_to_breathe() {
        let _g = lock();
        clear();
        // Nothing pinned → the footer stays at its base height.
        assert_eq!(extra_footer_rows(40, 3), 0);
        pin("w_a", "t");
        // Roomy window → the whole stack fits.
        assert_eq!(extra_footer_rows(40, 3), ROWS);
        // Exactly MIN_BODY_ROWS of body left → still fits.
        assert_eq!(extra_footer_rows(3 + MIN_BODY_ROWS + ROWS, 3), ROWS);
        // One row short → the anchor shrinks rather than eating the body.
        assert_eq!(extra_footer_rows(3 + MIN_BODY_ROWS + 1, 3), 1);
        // Cramped window keeps its output entirely.
        assert_eq!(extra_footer_rows(4, 3), 0);
        assert_eq!(extra_footer_rows(0, 3), 0);
        clear();
    }

    #[test]
    fn mark_terminal_is_idempotent_and_sweep_keeps_fresh_verdict() {
        let _g = lock();
        clear();
        pin("w_1", "t");
        mark_terminal(false);
        mark_terminal(true); // first terminal stamp wins for the dwell clock
        // A just-finished entry survives the sweep so the verdict is readable.
        sweep();
        assert!(active());
        clear();
    }

    #[test]
    fn dwell_is_thirty_seconds() {
        // The spec'd verdict window: a static green checkmark for 30s, then the
        // entry leaves the list on its own.
        assert_eq!(DWELL, Duration::from_secs(30));
    }

    #[test]
    fn status_row_carries_a_ticking_runtime() {
        // The runtime is derived from the elapsed clock on every paint, so it
        // advances even when the worker pushes no new activity.
        let (_, a) = render(0, "w_1", "t", "coordinating", None, false);
        let (_, b) = render(72_000, "w_1", "t", "coordinating", None, false);
        let (_, c) = render(3_900_000, "w_1", "t", "coordinating", None, false);
        assert!(a.contains("0s · coordinating"), "{a}");
        assert!(b.contains("1m12s · coordinating"), "{b}");
        assert!(c.contains("1h05m · coordinating"), "{c}");
    }

    #[test]
    fn status_row_strips_colorized_activity() {
        // Activity lines arrive pre-colorized; an embedded reset would end the
        // row's dim styling early and leak color into the footer.
        let (_, bottom) = render(
            0,
            "w_1",
            "t",
            "running · \u{1b}[36m🔧 read_file\u{1b}[0m src/repl.rs",
            None,
            false,
        );
        assert!(!bottom.contains('\u{1b}'), "{bottom:?}");
        assert!(
            bottom.contains("running · 🔧 read_file src/repl.rs"),
            "{bottom}"
        );
    }

    #[test]
    fn note_terminal_only_freezes_the_named_entry() {
        let _g = lock();
        clear();
        pin("w_pinned", "t");
        note_terminal("w_other", true);
        let rows = render_rows(8, false);
        assert!(
            !rows[0].starts_with("⚠️"),
            "another worker froze the entry: {:?}",
            rows[0]
        );
        note_terminal("w_pinned", false);
        let rows = render_rows(8, false);
        assert!(rows[0].starts_with("✅"), "{:?}", rows[0]);
        clear();
    }

    #[test]
    fn repinning_the_same_id_refreshes_instead_of_stacking() {
        let _g = lock();
        clear();
        pin("w_same", "first");
        pin("w_same", "second");
        assert_eq!(rows_used(), ROWS, "a re-pin must not duplicate the entry");
        let rows = render_rows(8, false);
        assert!(rows[0].contains("second"), "{:?}", rows[0]);
        clear();
    }

    #[test]
    fn stack_is_capped_and_sheds_finished_entries_first() {
        let _g = lock();
        clear();
        // One finished entry plus a flood of pending ones: the finished entry is
        // the first thing shed, because its verdict has already been shown.
        let keep = pin_queued("already ran");
        resolve_queued(&keep);
        for i in 0..MAX_ENTRIES {
            pin_queued(&format!("cmd {i}"));
        }
        let n = with(|s| s.len()).unwrap();
        assert!(n <= MAX_ENTRIES, "stack grew past the cap: {n}");
        let rows = render_rows(64, false);
        assert!(
            !rows.iter().any(|r| r.contains("already ran")),
            "finished entry should have been shed first: {rows:?}"
        );
        clear();
    }

    #[test]
    fn mutators_are_noops_on_an_empty_stack() {
        let _g = lock();
        clear();
        set_status("nothing to attach to");
        mark_terminal(true);
        resolve_queued_text("never queued");
        resolve_queued("q999");
        sweep();
        assert!(!active());
        assert!(!animating());
        assert!(pinned_id().is_none());
        assert_eq!(rows_used(), 0);
    }
}
