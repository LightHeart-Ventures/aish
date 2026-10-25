//! The PINNED ESCALATION BANNER — two footer rows PER live background
//! escalation that keep it on screen for as long as its coordinator is working.
//!
//! THE PROBLEM. When the model escalates work to a background coordinator
//! (`run_in_background`), the launch notice is printed ONCE into the scrolling
//! body ("🚀 dispatched to background worker w_xxxxxxxx: …") and then immediately
//! scrolls away under the next command's output. Thirty seconds later the
//! operator has no on-screen evidence that anything is running — the only
//! surviving signal is the `⟳N` prompt pulse, which says neither WHAT was
//! escalated nor HOW it's doing. The operator has to run
//! `:workers` to answer "is that thing still going?".
//!
//! THE FIX. Pin the escalation to the footer instead of the body. While
//! coordinators are live, [`crate::terminal::FooterLayout`] grows the pinned
//! footer by [`row_count`] rows and `footer_seq` paints:
//!
//! ```text
//!   🚀 w_a7k3m2 · build and open pr  <- stable identity, never flickers
//!      ♥ 1m12s · 🔧 read_file …               <- pulsing heart + worker status
//!   🚀 w_b2c9f1 · audit the deps     <- a SECOND live escalation
//!      ♥ 18s · 🔧 grep_files                  <- …its own heart and status
//!   ─────────────────────────────────────────   <- separator (the statusline lid)
//!   ⇄ detached — back to interactive …           <- SecondStatusLine
//!   aish v0.9 · sonnet …           12:04:51      <- statusline
//! ```
//!
//! i.e. the banners are anchored ABOVE the footer's top horizontal bar, each
//! with the worker's latest status directly beneath it, exactly where the
//! operator's eye already lives. The rule is the LID of the statusline block, so
//! painting a banner UNDER it read as a row wedged inside the statusline frame;
//! above it the block reads as the last thing the body said while the rule stays
//! welded to the two statusline rows it opens. Because they're still inside the
//! DECSTBM-reserved region they can never scroll away.
//!
//! STACKING (the bug this module originally had). The banner used to be a SINGLE
//! slot — `Mutex<Option<Banner>>`, where a fresh [`pin`] REPLACED the previous
//! one. With two or more escalations in flight the operator saw exactly one of
//! them (the newest) and had no on-screen evidence the others existed, even
//! though every one of them was still burning tokens. The store is now an
//! ORDERED STACK: every live escalation keeps its own two rows, rendered
//! NEWEST-FIRST (the same ordering `cycle_worker` uses for Shift-Tab, so the
//! newest coordinator is both the top banner and one keypress away). Re-pinning
//! an id that is already on screen REFRESHES it in place instead of duplicating
//! it. At most [`max_visible`] banners paint — HALF the terminal height, in
//! whole banners — and beyond that the oldest visible
//! status row carries a `+N more` tail, so the footer can never eat the window;
//! [`crate::terminal::FooterLayout`] additionally drops whole banners (newest
//! kept) on a window too short to hold them all.
//!
//! SUB-WORKERS (fan-out). A pinned coordinator can escalate in turn — fanning
//! a big task out to sub-workers is the whole point of a coordinator — and those
//! children run in their OWN processes, so the launching session's worker list
//! never sees them. They used to be invisible: the operator saw one banner for a
//! worker that was really driving four. The REPL now discovers them from the
//! durable `coordinator_runs` table (walking `parent_run_id` transitively from
//! the pinned ids) and pins each with [`pin_sub`], which files the child UNDER
//! its parent. The forest walk in [`ordered_rows`] then paints a child directly
//! beneath its parent, indented behind a dim bent arrow, with the same
//! summary-and-heartbeat status row every root gets — minus the id, which a
//! nested row deliberately drops (see [`render_sub`]): the indent and arrow
//! already say whose work it is, and nothing the operator can reach for takes a
//! sub-worker id, so the columns go to the timer, status and summary instead:
//!
//! ```text
//!   🚀 w_a7k3m2 · ship the sprint
//!      ♥ 4m02s · running · fanning out the card work
//!     ↳ 🚀 wire the tray rows                   <- sub-WORKER, one indent in
//!        ♥ 1m12s · running · 🔧 read_file src/escalation.rs
//! ```
//!
//! Nesting stops at [`MAX_SUB_DEPTH`] so a deep chain can't walk the rows off
//! the right edge; deeper descendants are still tracked (they count toward the
//! `+N more` tail). Only a LIVE child is pinned — a finished one keeps its
//! durable row forever, so pinning on phase alone would resurrect a verdict that
//! had already aged out and give the tray an immortal row.
//!
//! SUB-WORK LEDGER (decomposition, NOT fan-out). A worker that keeps a slice
//! list for ITSELF — `crate::subwork`'s ledger, which explicitly tells workers
//! NOT to spawn a child per slice — produces zero `coordinator_runs` rows, so
//! the `parent_run_id` walk above can never find it; before [`pin_subwork`] the
//! ledger escaped only as a `📣` console note in the transcript. The coordinator
//! now also writes the ledger to its own durable row (`coordinator_runs.subwork`)
//! on every change, the REPL reads it back for each visible worker, and
//! [`sync_subwork`] reconciles one STATIC row per item under the parent:
//!
//! ```text
//!   🚀 w_a7k3m2 · ship the sprint
//!      ♥ 4m02s · running · fanning out the card work
//!     ↳ ✅ read the board                       <- sub-WORK ledger item
//!        · 2m41s · done · 7 cards, 3 blocked
//!     ↳ 🧩 write the specs
//!        · 1m09s · running · drafting card 2 of 3
//! ```
//!
//! These rows are DELIBERATELY inert: no heartbeat (nothing is executing per
//! item — the parent's own heart covers them all), not in [`pinned_ids`] (the
//! REPL would look up a worker that doesn't exist), not in [`tray_order_ids`]
//! (Shift-Tab must not cycle onto a row you cannot attach to) and not in
//! [`animating`]. Their ids are namespaced `sw:{parent}:{item}` so they can
//! never be mistaken for a run id.
//!
//! ANIMATION AND LIVENESS. The head row keeps its static rocket identity.
//! The status row uses one heart for both the heartbeat animation and worker
//! liveness. A current durable heartbeat pulses the heart's intensity twice
//! per cycle; missed beats remain static yellow/red with their age, and an
//! unknown beat stays hollow. The pulse never changes glyph width or colour.
//! Each banner uses its own elapsed clock and durable heartbeat timestamp.
//! Plain output keeps the heart without animation escape sequences.
//!
//! QUEUED MID-TURN LINES. The tray tracks a SECOND kind of pending work: a
//! line the operator submitted WHILE a turn was in flight. `keywatch` prints a
//! one-shot `⏳ queued → …` receipt into the body and parks the line in the
//! REPL's type-ahead queue, but that receipt scrolls away under the turn's own
//! output exactly like the escalation launch notice did — leaving the operator
//! with no on-screen evidence that the thing they typed is still coming. So
//! each queued line also gets a [`Kind::Queued`] tray row ([`pin_queued`]) that
//! survives until the line is DEQUEUED for execution
//! ([`unpin_oldest_queued`], FIFO — the same order the REPL drains them):
//!
//! ```text
//!   ⏳ queued · what is the status of ISS-414652
//!      ↳ 8s · 1 of 2 — runs when this turn ends
//! ```
//!
//! Queued rows are deliberately STATIC (no heart, no pulse): nothing is running
//! for them yet, so animating them would imply liveness that does not exist —
//! and it keeps them out of [`animating`], so a queue parked at the prompt can't
//! defeat the footer heartbeat's idle gate. They are also excluded from
//! [`pinned_ids`] and every coordinator-scoped mutator, because there is no
//! worker behind them to refresh.
//!
//! LIFECYCLE. [`pin`] on escalation, [`set_status`] on every footer paint (the
//! REPL owns the worker list, so it composes each status row), [`note_terminal`]
//! when a coordinator finishes, and each banner self-retires [`DWELL_MAX`] after
//! that so the final verdict is readable but the footer shrinks back on its own.
//! Every mutator is ID-SCOPED: with several banners live, a finishing worker
//! must only ever freeze its OWN row.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// Footer rows ONE banner occupies: the escalation message and the worker-status
/// row directly below it. The pinned block is this times the number of visible
/// banners — see [`row_count`].
pub const ROWS_PER_BANNER: u16 = 2;

/// Fraction (`1/N`) of the window the pinned block may grow to. The tray tops
/// out at HALF the terminal: tall enough that a real fan-out is visible all at
/// once (the old fixed three-banner ceiling hid everything past the third worker
/// behind a `+N more` tail even on a 50-row window), bounded enough that the
/// transcript never disappears behind the chrome.
pub const MAX_VISIBLE_FRACTION: u16 = 2;

/// How many escalations paint at once when the terminal height is UNKNOWN — off
/// a tty, or a `TIOCGWINSZ` that reports nothing. Three banners = six footer
/// rows, which still leaves a usable body on a standard 24-row window; past the
/// live cap the extras are summarised as a `+N more` tail rather than swallowing
/// the terminal.
pub const MAX_VISIBLE: usize = 3;

/// How many banners may paint in a window of `rows` rows: half the window
/// ([`MAX_VISIBLE_FRACTION`]) in WHOLE banners, never fewer than one.
///
/// Pure, so the cap can be asserted at any height without a terminal: a 24-row
/// window seats 6 banners (12 rows), an 80-row window seats 20.
///
/// The cap is a CEILING, not a promise — [`crate::terminal::FooterLayout`] still
/// sheds whole banners when the solved footer would starve the scrolling body,
/// so asking for half the window stays safe even at height 6.
pub fn max_visible_for(rows: u16) -> usize {
    ((rows / MAX_VISIBLE_FRACTION.max(1)) / ROWS_PER_BANNER.max(1)).max(1) as usize
}

/// [`max_visible_for`] at the LIVE terminal size, falling back to
/// [`MAX_VISIBLE`] when the window size can't be read.
pub fn max_visible() -> usize {
    crate::terminal::term_size()
        .map(|(rows, _cols)| max_visible_for(rows))
        .unwrap_or(MAX_VISIBLE)
}

/// Hard ceiling on TRACKED banners (visible or summarised). A runaway fan-out
/// can't grow the store without bound; the oldest already-finished banner is
/// evicted first, so live work is never dropped in favour of a stale verdict.
pub const MAX_PINNED: usize = 32;

/// The head row's IDENTITY glyph while a coordinator is working. Deliberately
/// STATIC — see the ANIMATION note in the module docs. It is the same rocket the
/// body's launch notice prints, so the operator can match the pinned banner to
/// the line that announced it; it is `Extended_Pictographic` + East-Asian Wide
/// in every font that ships it, so it holds the 2 columns
/// [`crate::activity_summary::ESCALATION_HEAD_CHROME`] budgets for.
pub const LIVE_GLYPH: &str = "🚀";

/// Static prefix for a finished banner's status row — and for a queued line's,
/// which has nothing running behind it to animate.
pub const IDLE_PREFIX: &str = "↳";

/// The bent arrow that marks a SUB-WORKER's banner — a coordinator spawned BY
/// another pinned coordinator (see the SUB-WORKERS note in the module docs). It
/// leads the child's head row so the nesting reads at a glance:
/// `  ↳ 🚀 reviewing the diff`. The child's own `w_…` is NOT painted — the arrow
/// and indent already attribute the work, so see [`render_sub`].
pub const SUB_ARROW: &str = "↳";

/// Columns a sub-worker's rows are indented by, relative to its parent's. Four
/// on the head row (`"  ↳ "`) and the same four prepended to the already
/// 3-space-indented status row, so the child's heart lands under the child's
/// glyph rather than under its parent's.
pub(crate) const SUB_INDENT: &str = "    ";

/// How deep the tray will nest sub-workers. A sub-worker can itself fan out, and
/// the durable parent chain is walked transitively, but past this depth the rows
/// are mostly indent — so deeper descendants are tracked (they still count
/// toward the `+N more` tail) rather than painted with a runaway margin.
pub const MAX_SUB_DEPTH: usize = 2;

/// The head-row glyph for a QUEUED mid-turn line. Matches the hourglass the
/// body receipt (`keywatch::print_midturn_queued`) prints, so the operator can
/// tie the tray row to the line that acknowledged their keystroke.
pub const QUEUED_GLYPH: &str = "⏳";

/// The head-row label for a queued line (where an escalation shows its `w_…`).
pub const QUEUED_LABEL: &str = "queued";

/// The head-row glyph for an OPEN sub-work ledger item — a slice of the parent's
/// own work, which is what a puzzle piece says and a rocket (a launched worker)
/// would not. Width-2 like 🚀/⏳/✅, so a ledger row's task hint starts on the
/// same column as every other row's.
pub const SUBWORK_GLYPH: &str = "🧩";

/// Pulse frame cadence; the footer repaint task uses this same interval.
pub const FRAME_MS: u64 = 220;

/// FLOOR on a finished escalation's dwell: its verdict is never reclaimed
/// before this, even when the tray is full and a new worker is competing for the
/// slot. A fixed 45s dwell retired verdicts while the operator was still reading
/// them; one minute is the shortest window that survives stepping away from the
/// terminal.
pub const DWELL_MIN: Duration = Duration::from_secs(60);

/// CEILING on a finished escalation's dwell: [`sweep`] retires the banner once
/// this has elapsed, whether or not anything needs the slot. A quiet tray
/// therefore holds its verdicts for a quarter hour — long enough to come back
/// from a meeting and read what happened — instead of the old 45s, while still
/// shrinking the footer on its own so a forgotten session doesn't keep a fat
/// footer forever.
pub const DWELL_MAX: Duration = Duration::from_secs(15 * 60);

/// Max visible width of the task hint on the escalation row.
pub(crate) const TASK_HINT_MAX: usize = 56;

/// Max visible width of the composed status row (the terminal clip in
/// `footer_seq` is the hard backstop; this keeps the row from crowding out the
/// `↳` prefix on a narrow window).
pub const STATUS_MAX: usize = 110;

/// Visible width of the status row's RUNTIME column (`0s` … `1h04m`).
///
/// The status row is a TABLE, not a sentence: runtime, state and message each
/// occupy a fixed cell so the same field lands at the same screen column on
/// every row of the tray. With a ` · `-joined string the message started at a
/// different offset on every banner (`0s · …` vs `1h04m · …`), which is exactly
/// the ragged edge the eye has to re-scan on each paint.
pub(crate) const RUNTIME_COL: usize = 8;

/// Visible width of the status row's STATE column.
///
/// Sized to the COMMON phases — `running`, `pending`, `queued`, `done`,
/// `failed`, `stopped` all fit in 7 cells. The two long ones (`coordinating`,
/// `awaiting_batch`) clip to `coordi…`, which is still unambiguous and costs
/// seven fewer cells of indent on every other row in the tray.
pub(crate) const STATE_COL: usize = 7;

/// Separator painted between the STATE cell and the free-text MESSAGE cell.
///
/// The state column is padded to [`STATE_COL`], so without a glyph the lifecycle
/// word and the summary read as one run of text with an accidental gap in it —
/// `running   fanning out 3 reads`. A middot pins the boundary to a fixed column
/// and matches the separator the worker list already joins status to activity
/// with, so the tray reads as the same table.
pub(crate) const CELL_SEP: &str = "·";

/// Visible width of the status row's MESSAGE column: the free-text cell keeps
/// the full [`STATUS_MAX`] budget it had before the row became a table, so
/// adding the fixed columns never shortens an activity line.
pub(crate) const MESSAGE_COL: usize = STATUS_MAX;

/// Visible width of the status row's BEAT column — the leading liveness cell
/// (`♥`, `♥ 20m00s`, `⚠ 3h04m`, or the terminal `↳`).
///
/// This cell was the LAST ragged column in the tray, and the worst one: it sits
/// FIRST, so every field to its right inherited its jitter. A healthy heart is
/// one cell (`♥`) while a missed-beat heart carries its age (`♥ 20m00s`), so two
/// rows of the same tray started their runtime column up to seven cells apart —
/// and the width changed under a *single* row as its worker went stale, making
/// the whole tray twitch sideways between paints.
///
/// 8 = glyph (1) + space (1) + the 6-cell ceiling [`crate::style::fmt_duration`]
/// guarantees inside a day. That ceiling is why one formatter for every duration
/// is load-bearing rather than cosmetic: a fixed cell can only be sized once the
/// value that lands in it has a known maximum width.
pub(crate) const BEAT_COL: usize = 8;

/// What a tray row is tracking.
///
/// The tray holds three kinds of PENDING work, and the distinction is
/// load-bearing rather than cosmetic: a [`Kind::Escalation`] has a live
/// coordinator PROCESS behind it (so it animates, carries a liveness heart, and
/// is refreshed from the worker list), while a [`Kind::Queued`] line and a
/// [`Kind::SubWork`] ledger item have no process of their own (so both are
/// static and are skipped by every coordinator-scoped mutator). Mixing them in
/// one store is what lets a single footer block show everything the operator is
/// waiting on.
///
/// Every coordinator-scoped mutator ([`set_status`], [`set_task`], [`set_beat`],
/// [`note_terminal`], [`unpin`], [`sweep`]) and every id-exporting accessor
/// ([`pinned_ids`], [`tray_order_ids`], [`animating`]) already filters on
/// `kind == Kind::Escalation`, so a new process-less kind is excluded from all
/// of them by construction — adding [`Kind::SubWork`] cannot hand the REPL a row
/// id with no worker behind it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    /// A background coordinator launched by `run_in_background`.
    Escalation,
    /// A line the operator submitted mid-turn, parked in the REPL's type-ahead
    /// queue until the current turn ends.
    Queued,
    /// One item from a worker's SUB-WORK LEDGER (`crate::subwork`): a slice of
    /// work the coordinator declared and is doing ITSELF, in its own worktree.
    /// There is no child process and no `coordinator_runs` row — the ledger
    /// rides the parent's durable `subwork` column — so the row is static, has
    /// no heartbeat, and is never a Shift-Tab target.
    SubWork,
}

/// One pinned escalation. Many can be live at once — see the STACKING note in
/// the module docs — so every field is per-banner and every mutator is keyed by
/// [`Banner::id`].
struct Banner {
    /// Which flavour of pending work this row tracks — see [`Kind`].
    kind: Kind,
    /// Coordinator run id (`w_…`) — rendered short, and the key the REPL uses to
    /// find the live worker when it refreshes the status row.
    id: String,
    /// The run id of the coordinator that SPAWNED this one, when this banner
    /// tracks a sub-worker (`coordinator_runs.parent_run_id`). `None` at a root
    /// — an escalation the operator's own session launched, or a queued line.
    ///
    /// Display-only: it decides where the row sorts and how far it indents (see
    /// [`ordered_rows`]). A child whose parent is no longer tracked (evicted, or
    /// retired after its dwell) renders as a root rather than disappearing —
    /// live work is never dropped in favour of a stale verdict.
    parent: Option<String>,
    /// The escalated task, rendered as a compact one-line hint.
    task: String,
    /// When the banner was pinned — drives the animation frame.
    pinned_at: Instant,
    /// When the coordinator reached a terminal state (`done`/`failed`), if it
    /// has. Freezes the animation and starts the dwell retirement clock
    /// ([`DWELL_MIN`] floor, [`DWELL_MAX`] ceiling).
    terminal_at: Option<Instant>,
    /// True when the terminal outcome was a failure (⚠️ instead of ✅).
    failed: bool,
    /// Latest composed worker-status text for the second row.
    status: String,
    /// Epoch seconds of this coordinator's last DURABLE heartbeat (the
    /// `coordinator_runs.heartbeat_at` column), or `None` when no beat is on
    /// record yet. Stored as an ABSOLUTE timestamp rather than a pre-computed
    /// age so the rendered heart keeps aging between the REPL's throttled
    /// polls — the footer repaints ~4.5x/sec but the beat only moves every 30s,
    /// so re-reading SQLite on every frame would be ~136x waste.
    beat: Option<i64>,
}

/// Wall clock in epoch seconds — the reference each banner ages `beat` against.
/// Matches `coordinator::now_unix_secs`; kept local so this module stays free of
/// coordinator imports.
fn now_unix_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Every live escalation, OLDEST FIRST (push order). Rendering reverses this so
/// the newest escalation is the TOP banner — see [`rows`].
static BANNERS: Mutex<Vec<Banner>> = Mutex::new(Vec::new());

/// The worker the operator is currently `:attach`ed to, if any — the coordinator
/// whose transcript is being mirrored and whose turn typed input is folded into.
///
/// Process-global for the same reason [`BANNERS`] is: every paint path needs it
/// (including the idle heartbeat, which has no `Session`), and the REPL's own
/// attach slot lives behind a session lock the footer can't take mid-paint.
static ATTACHED: Mutex<Option<String>> = Mutex::new(None);

/// Record (or clear, with `None`) the worker the operator is attached to, so the
/// tray can highlight that banner. Mirrors the REPL's attach slot — call it
/// wherever that slot is written.
pub fn set_attached(id: Option<&str>) {
    if let Ok(mut cur) = ATTACHED.lock() {
        let next = id.map(str::to_string);
        if *cur != next {
            *cur = next;
            // An attach/detach repaints every tray row's band, and nothing about
            // it animates — so wake the heartbeat the same way a status change
            // does, or the band would wait for the operator's next keystroke.
            mark_dirty();
        }
    }
}

/// The worker id the operator is attached to, if any.
pub fn attached() -> Option<String> {
    ATTACHED.lock().ok().and_then(|cur| cur.clone())
}

/// Set whenever a tracked row's PAINTED CONTENT changes — a new status line, a
/// terminal verdict, a relabelled task.
///
/// The footer's heartbeat only repaints while something is animating, which is
/// the right gate for a spinner but the wrong one for a CONTENT change: a tray
/// of finished rows animates nothing, so a late `done → failed` flip (or a last
/// status line landing after the animation stopped) would sit unpainted until
/// the operator happened to press a key. This flag is that missing wake-up — the
/// mutators raise it, the heartbeat consumes it with [`take_dirty`] and paints
/// one frame.
static DIRTY: AtomicBool = AtomicBool::new(false);

/// Mark the tray as needing one repaint. Cheap and idempotent — many changes
/// between two frames collapse into a single paint.
fn mark_dirty() {
    DIRTY.store(true, Ordering::Relaxed);
}

/// Consume the dirty flag: `true` exactly once per batch of changes.
///
/// Called by the footer heartbeat alongside [`animating`]; the take-and-clear is
/// what keeps an idle tray idle instead of repainting forever after one change.
pub fn take_dirty() -> bool {
    DIRTY.swap(false, Ordering::Relaxed)
}

/// Pin an escalation banner for coordinator `id` running `task`.
///
/// Additive, not replacing: an escalation launched while others are still live
/// gets its OWN two rows (the single-slot store this replaced silently hid every
/// escalation but the newest). Re-pinning an id that is already tracked
/// REFRESHES that banner in place — same task hint, clocks reset, verdict
/// cleared — so a retry can't stack a duplicate row for one coordinator.
/// A pinned banner steals two body rows from the scrolling region. The STORE
/// mutation is [`push_banner`]; [`crate::terminal::repaint_escalation_around`]
/// reconciles the installed body/tray layout and paints the new rows under the
/// footer boundary lock. The transcript moves before tray cells are claimed,
/// including while the operator edits the separate input row. Lock order is
/// `FOOTER_PAINT` → `BANNERS`, so this wrapper runs outside the banner lock.
pub fn pin(id: &str, task: &str) {
    crate::terminal::repaint_escalation_around(|| push_banner(id, task));
}

/// Pin a SUB-WORKER's banner: coordinator `id`, spawned by the already-pinned
/// coordinator `parent`, running `task` and started `age_ms` ago.
///
/// Rendered as an indented `↳` row directly beneath its parent (see
/// [`ordered_rows`]) so a fan-out reads as the parent's work rather than as
/// unexplained new escalations at the top of the tray.
///
/// INSERT-ONLY, unlike [`pin`]. The REPL discovers sub-workers by polling the
/// durable `coordinator_runs` table, so this is called repeatedly for the SAME
/// child; refreshing in place (as `pin` does for an operator-driven retry)
/// would restart the child's elapsed clock and wipe the verdict
/// [`note_terminal`] just stamped, so a finished sub-worker could never age out
/// of the tray. Subsequent passes therefore fall through to
/// [`set_status`]/[`note_terminal`], which are the update path.
///
/// `age_ms` backdates the banner's own clock to the child's real start time, so
/// a sub-worker discovered a minute into its run shows `1m04s`, not `0s`.
pub fn pin_sub(parent: &str, id: &str, task: &str, age_ms: u64) {
    crate::terminal::repaint_escalation_around(|| {
        push_banner_inner(id, task, Some(parent), age_ms);
    });
}

fn push_banner(id: &str, task: &str) {
    push_banner_inner(id, task, None, 0);
}

fn push_banner_inner(id: &str, task: &str, parent: Option<&str>, age_ms: u64) {
    let Ok(mut banners) = BANNERS.lock() else {
        return;
    };
    let fresh = Banner {
        kind: Kind::Escalation,
        id: id.to_string(),
        task: task.to_string(),
        parent: parent.map(str::to_string),
        // Backdate to the run's real start when the caller knows it. Saturating:
        // a clock skew that puts the start in the "future" just reads as 0s
        // rather than panicking on an Instant underflow.
        pinned_at: Instant::now()
            .checked_sub(Duration::from_millis(age_ms))
            .unwrap_or_else(Instant::now),
        terminal_at: None,
        failed: false,
        status: "queued — waiting for the coordinator to start".into(),
        beat: None,
    };
    if let Some(slot) = banners
        .iter_mut()
        .find(|b| b.kind == Kind::Escalation && b.id == id)
    {
        // A sub-worker's pin is a DISCOVERY, not a relaunch — leave the existing
        // row (and its clock, status and verdict) exactly as it is.
        if parent.is_some() {
            return;
        }
        // Same coordinator: its durable heartbeat is still valid evidence, so
        // keep it rather than flashing the hollow "no claim" heart on a retry.
        // The parent link survives too: a re-`pin` of a child (a retry routed
        // through the root path) must not promote it to a root row.
        let beat = slot.beat;
        let parent = slot.parent.take();
        *slot = Banner {
            beat,
            parent,
            ..fresh
        };
        return;
    }
    // A NEW row at a full tray reclaims the stalest verdict that has served its
    // floor (no-op when there's room, or when every verdict is still fresh).
    evict_for_tray_pressure(&mut banners);
    banners.push(fresh);
    // Bound the store. Evict the oldest FINISHED escalation first so a burst of
    // fan-out never retires a coordinator that is still working, then the
    // oldest live escalation, and only then the oldest row of any kind. Queued
    // lines are evicted LAST because their row is the only remaining on-screen
    // evidence that an un-run input exists.
    while banners.len() > MAX_PINNED {
        let victim = banners
            .iter()
            .position(|b| b.kind == Kind::Escalation && b.terminal_at.is_some())
            .or_else(|| banners.iter().position(|b| b.kind == Kind::Escalation))
            .unwrap_or(0);
        banners.remove(victim);
    }
}

/// One item of a worker's SUB-WORK LEDGER, flattened for the tray.
///
/// Deliberately NOT `crate::subwork::SubWorkItem`: the tray is a rendering
/// surface and the ledger is a coordinator-side protocol, so the caller
/// (`repl::refresh_escalation_subworkers`) does the translation and this module
/// stays free of coordinator imports — which is also what keeps the unit tests
/// below able to build a row without a ledger, a store or a subprocess.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SubWorkRow {
    /// The ledger's own kebab slug for the item (`SubWorkItem::id`). Scoped to
    /// its parent, so it only has to be unique within one ledger.
    pub id: String,
    /// What the item is meant to accomplish — the row's task hint.
    pub label: String,
    /// Lifecycle word (`pending` / `running` / `done` / `failed`), matching the
    /// vocabulary [`split_status`] already understands.
    pub status: String,
    /// Latest one-line note from the worker, or empty.
    pub note: String,
}

/// The tray id a ledger item occupies: `sw:{parent}:{item}`.
///
/// NAMESPACED ON PURPOSE. Coordinator ids are `w_…`, queued rows are `q#…`, so
/// an `sw:` prefix can never collide with a real run id — which matters because
/// a collision would let a ledger item answer to a coordinator-scoped mutator
/// (or worse, be handed to the REPL as a worker to look up). The parent id is
/// part of the key so two workers declaring the same slug (`wire-the-tests`)
/// keep separate rows.
fn subwork_row_id(parent: &str, item: &str) -> String {
    format!("sw:{parent}:{item}")
}

/// Pin (or refresh) ONE ledger item as a static row nested under `parent`.
///
/// UPDATE IN PLACE, unlike [`pin_sub`]: a ledger is polled from the parent's
/// durable `subwork` column and re-read on every pass, so the same key arrives
/// over and over with a MOVING status (`pending` → `running` → `done`). The row
/// therefore keeps its original `pinned_at` (so its clock reads "how long this
/// item has been on screen" rather than restarting every poll) while taking the
/// newest label and status text.
///
/// Ledger rows NEVER displace real work: at a full tray the push is dropped
/// instead of running [`evict_for_tray_pressure`], because a row that merely
/// describes a slice of a worker's own work is worth strictly less than the row
/// of a worker that is actually running. The ledger itself is capped
/// (`crate::subwork::MAX_SUBWORK_ITEMS`), so the drop is a backstop, not the
/// normal path.
pub fn pin_subwork(parent: &str, key: &str, label: &str, status: &str) {
    crate::terminal::repaint_escalation_around(|| {
        let Ok(mut banners) = BANNERS.lock() else {
            return;
        };
        let id = subwork_row_id(parent, key);
        if let Some(slot) = banners
            .iter_mut()
            .find(|b| b.kind == Kind::SubWork && b.id == id)
        {
            slot.task = label.to_string();
            slot.status = status.to_string();
            slot.parent = Some(parent.to_string());
            return;
        }
        if banners.len() >= MAX_PINNED {
            return;
        }
        banners.push(Banner {
            kind: Kind::SubWork,
            id,
            task: label.to_string(),
            parent: Some(parent.to_string()),
            pinned_at: Instant::now(),
            // No process, so no verdict to stamp and no heartbeat to claim:
            // `phase` reads this row's lifecycle out of `status` instead (see
            // the SubWork note there), and the renderer draws the static
            // [`IDLE_PREFIX`] where a live row draws its heart.
            terminal_at: None,
            failed: false,
            status: status.to_string(),
            beat: None,
        });
    });
}

/// Reconcile `parent`'s ledger rows against `items` in one pass: pin/refresh
/// everything present, drop every `sw:{parent}:…` row whose item is gone.
///
/// The unpin half is why this exists rather than a bare loop of [`pin_subwork`]:
/// a worker can RETRACT an item (a slice it decided it didn't need), and a row
/// with nothing behind it would otherwise sit in the tray until the parent
/// retired. Scoped to this parent's prefix, so one worker's ledger can never
/// reap another's rows.
///
/// An empty `items` is the normal case (most workers declare no sub-work at
/// all): it clears this parent's ledger rows and touches nothing else, so the
/// sub-WORKER rows fed by [`pin_sub`] are undisturbed.
pub fn sync_subwork(parent: &str, items: &[SubWorkRow]) {
    let prefix = format!("sw:{parent}:");
    let stale: Vec<String> = {
        let Ok(banners) = BANNERS.lock() else {
            return;
        };
        banners
            .iter()
            .filter(|b| b.kind == Kind::SubWork && b.id.starts_with(&prefix))
            .filter(|b| {
                !items
                    .iter()
                    .any(|it| b.id == subwork_row_id(parent, &it.id))
            })
            .map(|b| b.id.clone())
            .collect()
    };
    if !stale.is_empty() {
        crate::terminal::repaint_escalation_around(|| {
            if let Ok(mut banners) = BANNERS.lock() {
                banners.retain(|b| !(b.kind == Kind::SubWork && stale.contains(&b.id)));
            }
        });
    }
    for item in items {
        // The status cell wants `state · detail`, which is exactly what
        // [`split_status`] parses back apart on the render path.
        let status = if item.note.trim().is_empty() {
            item.status.clone()
        } else {
            format!("{} · {}", item.status, item.note)
        };
        pin_subwork(parent, &item.id, &item.label, &status);
    }
}

/// Pin a tray row for a line the operator submitted MID-TURN, returning the
/// internal row id so a caller can drop that exact row later if it wants to.
///
/// The REPL's type-ahead queue is FIFO and so is this store, so the normal
/// retirement path is [`unpin_oldest_queued`] (called as each line is dequeued
/// for execution) — no id bookkeeping required at the call site.
///
/// Ids are minted from a private monotonic counter rather than the line's queue
/// POSITION: positions are reused as the queue drains, and a reused id would
/// refresh an unrelated still-pinned row in place instead of adding a new one.
/// Every queued submission therefore gets its own row, even when the operator
/// types the same text twice.
pub fn pin_queued(line: &str) -> String {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    let id = format!(
        "q#{}",
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    );
    let row = id.clone();
    crate::terminal::repaint_escalation_around(|| {
        let Ok(mut banners) = BANNERS.lock() else {
            return;
        };
        evict_for_tray_pressure(&mut banners);
        banners.push(Banner {
            kind: Kind::Queued,
            id,
            task: line.to_string(),
            parent: None,
            pinned_at: Instant::now(),
            terminal_at: None,
            failed: false,
            status: String::new(),
            beat: None,
        });
        while banners.len() > MAX_PINNED {
            let victim = banners
                .iter()
                .position(|b| b.kind == Kind::Escalation && b.terminal_at.is_some())
                .or_else(|| banners.iter().position(|b| b.kind == Kind::Escalation))
                .unwrap_or(0);
            banners.remove(victim);
        }
    });
    row
}

/// Retire the OLDEST queued row — the one whose line the REPL is about to run —
/// reporting whether a row was actually dropped.
///
/// FIFO to match the type-ahead queue's own drain order, so row N always
/// disappears as line N starts executing. A no-op when nothing is queued, which
/// is what makes it safe to call unconditionally from the drain path (lines that
/// predate this tray, or arrived by some other route, simply have no row).
pub fn unpin_oldest_queued() -> bool {
    crate::terminal::repaint_escalation_around(|| {
        let Ok(mut banners) = BANNERS.lock() else {
            return false;
        };
        match banners.iter().position(|b| b.kind == Kind::Queued) {
            Some(i) => {
                banners.remove(i);
                true
            }
            None => false,
        }
    })
}

/// Retire EVERY queued row, leaving escalation banners alone — the wholesale
/// counterpart to [`unpin_oldest_queued`], for a caller that DISCARDS the
/// type-ahead queue in one go (lines that will never run must not leave rows
/// behind). The REPL has no such discard path today, so this is currently
/// exercised only by tests; it stays test-gated until a caller needs it, so the
/// shipped binary carries no unreachable code.
#[cfg(test)]
pub fn clear_queued() {
    crate::terminal::repaint_escalation_around(|| {
        if let Ok(mut banners) = BANNERS.lock() {
            banners.retain(|b| b.kind != Kind::Queued);
        }
    });
}

/// How many queued lines currently have a tray row — the number a caller would
/// reconcile against the REPL's own queue length. Test-gated for the same
/// reason as [`clear_queued`]: nothing in the shipped REPL reads it yet.
#[cfg(test)]
pub fn queued_count() -> usize {
    BANNERS
        .lock()
        .map(|b| b.iter().filter(|b| b.kind == Kind::Queued).count())
        .unwrap_or(0)
}

/// Remove coordinator `id`'s banner from the tray RIGHT NOW, reporting whether
/// one was actually pinned for it.
///
/// This is the DISMISSAL path, and it exists because [`sweep`] can't serve it:
/// sweep only retires a banner that already reached a terminal state AND then
/// outlived its dwell. A `:close`d coordinator that is still RUNNING never
/// reaches that state from this session's point of view — the operator stopped
/// tracking it — so without an explicit removal its two footer rows would keep
/// animating forever, advertising a worker that is gone from `:workers` and the
/// Shift-Tab rotation. `:close` calls this so the banner disappears on the next
/// footer paint instead of never.
///
/// Strictly id-scoped: dismissing one escalation leaves its live siblings
/// pinned, and an id that isn't pinned is a silent no-op (closing a plain
/// `:dispatch` worker that never escalated must not disturb the tray).
pub fn unpin(id: &str) -> bool {
    crate::terminal::repaint_escalation_around(|| {
        let Ok(mut banners) = BANNERS.lock() else {
            return false;
        };
        let before = banners.len();
        banners.retain(|b| !(b.kind == Kind::Escalation && b.id == id));
        banners.len() != before
    })
}

/// Retire EVERY banner (the footer shrinks back on the next paint).
#[allow(dead_code)] // completes the BANNERS push/remove/clear API; no caller wired yet.
pub fn clear() {
    crate::terminal::repaint_escalation_around(|| {
        if let Ok(mut banners) = BANNERS.lock() {
            banners.clear();
        }
    });
}

/// True while ANY banner is pinned — the gate
/// [`crate::terminal::FooterLayout`] consults to decide the footer height.
pub fn active() -> bool {
    BANNERS.lock().map(|b| !b.is_empty()).unwrap_or(false)
}

/// How many banners are tracked right now (including ones summarised past
/// [`max_visible`]).
pub fn count() -> usize {
    BANNERS.lock().map(|b| b.len()).unwrap_or(0)
}

/// Footer rows the pinned block WANTS: two per visible banner, capped at
/// [`max_visible`] banners (half the window). Zero when nothing is pinned.
///
/// [`crate::terminal::FooterLayout`] may hand back fewer on a short window —
/// it drops whole banners rather than splitting one across the rule.
pub fn row_count() -> u16 {
    (count().min(max_visible()) as u16) * ROWS_PER_BANNER
}

/// True while at least one pinned banner is still ANIMATING (an ESCALATION,
/// pinned and not yet terminal). The footer heartbeat consults this to bypass
/// its idle gate and repaint on the [`FRAME_MS`] cadence — motion is the "still
/// working" signal, so it must keep ticking while the shell sits idle at the
/// prompt.
///
/// Queued rows are excluded on purpose: they are static, so a queue parked at
/// the prompt would otherwise hold the shell in the ~4.5 fps repaint loop
/// forever to animate nothing.
pub fn animating() -> bool {
    BANNERS
        .lock()
        .map(|banners| {
            banners
                .iter()
                .any(|b| b.kind == Kind::Escalation && b.terminal_at.is_none())
        })
        .unwrap_or(false)
}

/// Every pinned coordinator's run id, NEWEST FIRST. The REPL walks these to
/// locate each live worker whose status it then feeds back via [`set_status`].
///
/// Queued rows are omitted — there is no worker behind them to look up.
pub fn pinned_ids() -> Vec<String> {
    BANNERS
        .lock()
        .map(|banners| {
            banners
                .iter()
                .rev()
                .filter(|b| b.kind == Kind::Escalation)
                .map(|b| b.id.clone())
                .collect()
        })
        .unwrap_or_default()
}

/// Every pinned coordinator's run id in the tray's own PAINT order — the TOP
/// painted row first, walking DOWN to the row nearest the prompt. Identical
/// ordering to [`rows`] (phase-grouped, newest-first, each parent immediately
/// followed by its sub-workers), minus the visible-rows truncation: hidden
/// overflow rows stay in the list so a caller mapping ids to tray position never
/// loses one.
///
/// Shift-Tab's rotation is built off this (reversed) so the cycle walks UP the
/// tray from the bottom row instead of guessing at spawn order — the tray is
/// what the operator is actually looking at.
///
/// Queued rows are omitted — there is no worker behind them to attach to.
pub fn tray_order_ids() -> Vec<String> {
    BANNERS
        .lock()
        .map(|banners| {
            ordered_rows(&banners)
                .into_iter()
                .map(|(idx, _)| &banners[idx])
                .filter(|b| b.kind == Kind::Escalation)
                .map(|b| b.id.clone())
                .collect()
        })
        .unwrap_or_default()
}

/// Which LIFECYCLE PHASE a tray row sorts into.
///
/// The tray groups before it sorts, because arrival order alone buries the row
/// the operator needs: a still-`running` coordinator is the only row that can
/// still change, a `failed` one is the only row that wants a decision, and a
/// `done`/`starting` row is just bookkeeping. Grouping keeps each class
/// together no matter when it launched.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Phase {
    /// Live work — a coordinator that is running (or coordinating / awaiting a
    /// batch) and has not stamped a verdict.
    Running,
    /// Finished badly: a failed or stopped verdict, still dwelling.
    Failed,
    /// Finished well, still dwelling.
    Done,
    /// Not started yet: a queued line, or a pinned coordinator whose first
    /// status has not arrived.
    Starting,
}

impl Phase {
    /// Sort rank, ASCENDING in paint order (0 paints at the TOP of the tray).
    ///
    /// Read bottom-up — the operator's eye starts at the row nearest the prompt
    /// — this is `starting → done → failed → running`: the freshest, most
    /// actionable work ends up furthest from the prompt only after the noise.
    const fn rank(self) -> u8 {
        match self {
            Phase::Running => 0,
            Phase::Failed => 1,
            Phase::Done => 2,
            Phase::Starting => 3,
        }
    }
}

/// Classify one banner for [`Phase`] grouping.
///
/// The stamped verdict wins when there is one (`terminal_at` is the authority on
/// finished work), otherwise the lifecycle word from the composed status text
/// decides. An unrecognised head is treated as live rather than as `Starting`:
/// a row with a pinned coordinator behind it is something the operator is still
/// waiting on, and mis-sorting it to the bottom would hide active work.
///
/// A [`Kind::SubWork`] row needs no special case and deliberately doesn't get
/// one: it never carries a `terminal_at` (nothing finishes a ledger item but the
/// worker saying so), so it falls straight through to the status-word match,
/// which is exactly the classification its ledger status wants — `done` → Done,
/// `failed` → Failed, `pending` → Starting, `running` → Running.
fn phase(b: &Banner) -> Phase {
    if b.kind == Kind::Queued {
        return Phase::Starting;
    }
    if b.terminal_at.is_some() {
        return if b.failed { Phase::Failed } else { Phase::Done };
    }
    match split_status(&b.status).0 {
        "done" => Phase::Done,
        "failed" | "stopped" => Phase::Failed,
        "" | "queued" | "pending" => Phase::Starting,
        _ => Phase::Running,
    }
}

/// The tray's paint order: a depth-first FOREST over the parent links.
///
/// Roots are grouped by [`Phase`] — `running` first, then `failed`, `done` and
/// `starting`, so reading UP from the prompt gives `starting → done → failed →
/// running` — and stay newest-first WITHIN each group. Each root is immediately
/// followed by its sub-workers (same grouping), recursively.
/// Returns `(index, depth)` pairs so the caller can indent without re-walking
/// the chain. A banner whose parent is no longer in the store sorts as a root.
fn ordered_rows(banners: &[Banner]) -> Vec<(usize, usize)> {
    let is_live_parent = |id: &str| {
        banners
            .iter()
            .any(|b| b.kind == Kind::Escalation && b.id == id)
    };
    // Newest first (the flat tray's historical order), then grouped by
    // lifecycle phase. The phase sort is STABLE, so newest-first survives
    // inside each group — grouping re-ranks the classes, it does not reshuffle
    // the rows within one.
    let mut newest_first: Vec<usize> = (0..banners.len()).rev().collect();
    newest_first.sort_by_key(|&i| phase(&banners[i]).rank());
    let newest_first = newest_first;
    let children_of = |parent_id: &str| -> Vec<usize> {
        newest_first
            .iter()
            .copied()
            .filter(|&i| banners[i].parent.as_deref() == Some(parent_id))
            .collect()
    };

    let mut out: Vec<(usize, usize)> = Vec::with_capacity(banners.len());
    // Stack of rows still to emit, each with the depth it renders at.
    let mut stack: Vec<(usize, usize)> = Vec::new();
    for &i in newest_first.iter().rev() {
        let orphan = banners[i]
            .parent
            .as_deref()
            .is_none_or(|p| !is_live_parent(p));
        if orphan {
            // Pushed in reverse so popping yields newest-first roots.
            stack.push((i, 0));
        }
    }
    let mut guard = banners.len().saturating_mul(2) + 8;
    while let Some((idx, depth)) = stack.pop() {
        guard = guard.saturating_sub(1);
        if guard == 0 {
            break;
        }
        if out.iter().any(|&(seen, _)| seen == idx) {
            continue;
        }
        out.push((idx, depth));
        let child_depth = (depth + 1).min(MAX_SUB_DEPTH);
        let kids = children_of(&banners[idx].id);
        for &child in kids.iter().rev() {
            stack.push((child, child_depth));
        }
    }
    // Anything unreachable (a cycle, or a child of an evicted parent we failed
    // to classify) still gets painted — never swallow a live row.
    for i in newest_first {
        if !out.iter().any(|&(seen, _)| seen == i) {
            out.push((i, 0));
        }
    }
    out
}

/// Replace coordinator `id`'s status row text (already composed by the caller —
/// the REPL owns the worker list). No-op when that id isn't pinned, so a
/// finished-and-retired worker can't resurrect a row or write into another
/// banner's.
pub fn set_status(id: &str, status: &str) {
    if let Ok(mut banners) = BANNERS.lock()
        && let Some(b) = banners
            .iter_mut()
            .find(|b| b.kind == Kind::Escalation && b.id == id)
    {
        // Compare before storing: the REPL re-composes this row on EVERY footer
        // paint, so an unconditional `mark_dirty` here would hold the flag high
        // forever and turn the heartbeat's idle gate into a busy loop.
        if b.status != status {
            b.status = status.to_string();
            mark_dirty();
        }
    }
}

/// Replace coordinator `id`'s HEAD-row label — the banner's name for the work.
///
/// [`pin`] seeds it with the raw task brief because that is all the parent knows
/// at launch; the model-written TITLE (see
/// [`crate::activity_summary::summarize_title`]) is stamped onto the durable row
/// a moment later BY THE COORDINATOR SUBPROCESS, so the parent has to swap it in
/// once it appears. The REPL does that from its footer-refresh pass
/// (`repl::refresh_escalation_status`), which is already reading the store for
/// the status row.
///
/// No-op when `id` isn't pinned (same ID-scoping rule as [`set_status`]: a
/// retired worker must never write into another banner's row), and no-op on an
/// empty label so a failed title write can't blank out the brief we already
/// have.
pub fn set_task(id: &str, task: &str) {
    if task.trim().is_empty() {
        return;
    }
    if let Ok(mut banners) = BANNERS.lock()
        && let Some(b) = banners
            .iter_mut()
            .find(|b| b.kind == Kind::Escalation && b.id == id)
        // Same compare-before-store rule as [`set_status`]: the refresh pass
        // re-writes the title on every paint, so only a CHANGE may ask for a
        // frame or the heartbeat never goes idle again.
        && b.task != task
    {
        b.task = task.to_string();
        mark_dirty();
    }
}

/// Record coordinator `id`'s last durable heartbeat (epoch seconds from
/// `coordinator_runs.heartbeat_at`), which drives the liveness heart on THAT
/// banner's status row. `None` clears it back to "no beat on record". No-op when
/// the id isn't pinned. The REPL polls this on a throttle (see
/// `repl::refresh_escalation_beats`) because the beat only moves every 30s.
pub fn set_beat(id: &str, beat: Option<i64>) {
    if let Ok(mut banners) = BANNERS.lock()
        && let Some(b) = banners
            .iter_mut()
            .find(|b| b.kind == Kind::Escalation && b.id == id)
    {
        b.beat = beat;
    }
}

/// Record that coordinator `id` reached a terminal state: freezes THAT banner's
/// animation glyph and starts its dwell retirement clock. Idempotent — the
/// first call wins, so the dwell measures from the real finish — and strictly
/// id-scoped, so a worker finishing can never stamp another banner's verdict.
pub fn note_terminal(id: &str, failed: bool) {
    if let Ok(mut banners) = BANNERS.lock()
        && let Some(b) = banners
            .iter_mut()
            .find(|b| b.kind == Kind::Escalation && b.id == id)
    {
        // A verdict both freezes the glyph and re-ranks the row's [`Phase`], so
        // the tray has to repaint even though nothing is animating any more.
        if b.terminal_at.is_none() {
            b.failed = failed;
            b.terminal_at = Some(Instant::now());
            mark_dirty();
        }
    }
}

/// Retire each finished banner once [`DWELL_MAX`] has elapsed since its
/// terminal mark. Called on each footer refresh; cheap no-op while workers are
/// live. Per-banner, so one finished escalation retiring leaves its live
/// siblings pinned.
///
/// This is the UNCONDITIONAL half of the dwell policy — the deadline a quiet
/// tray retires on with nobody asking for the room. The conditional half lives
/// in [`evict_for_tray_pressure`], which reclaims a verdict EARLY (but never
/// before [`DWELL_MIN`]) when a new row arrives at a full tray.
pub fn sweep() {
    if let Ok(mut banners) = BANNERS.lock() {
        banners.retain(|b| b.terminal_at.is_none_or(|t| t.elapsed() < DWELL_MAX));
        // Reap orphaned ledger rows. A [`Kind::SubWork`] row is only ever
        // refreshed while its parent is pinned (the REPL syncs per VISIBLE
        // worker), so once the parent retires nothing updates — or removes —
        // the children, and [`ordered_rows`] would promote them to roots and
        // paint a slice of work as if it were a worker. Tied to the parent's
        // lifetime here rather than at the unpin call sites so every path that
        // can drop a parent (unpin, dwell expiry, tray pressure) is covered.
        let live: Vec<String> = banners
            .iter()
            .filter(|b| b.kind == Kind::Escalation)
            .map(|b| b.id.clone())
            .collect();
        banners.retain(|b| {
            b.kind != Kind::SubWork
                || b.parent
                    .as_deref()
                    .is_some_and(|p| live.iter().any(|id| id == p))
        });
    }
}

/// Reclaim ONE slot for an incoming row when the tray is already full: drop the
/// oldest finished banner that has served its [`DWELL_MIN`]. No-op when the tray
/// has room, or when every finished banner is still inside its floor — a fresh
/// verdict is never evicted to make way for a newer one, it just waits for the
/// next arrival (or [`sweep`]).
///
/// "Full" is measured against [`max_visible`], the same live cap [`row_count`]
/// paints to: once the tray is at its visible ceiling an extra banner only
/// deepens the `+N more` tail, which is exactly when a stale verdict is worth
/// less than the arriving row. LIVE banners are never candidates — only a
/// `terminal_at` row can be reclaimed — so pressure can't drop running work.
fn evict_for_tray_pressure(banners: &mut Vec<Banner>) {
    if banners.len() < max_visible() {
        return;
    }
    // Oldest-first store, so the first qualifying position is the stalest
    // verdict — the one whose dwell has run longest.
    if let Some(victim) = banners.iter().position(|b| {
        b.kind == Kind::Escalation && b.terminal_at.is_some_and(|t| t.elapsed() >= DWELL_MIN)
    }) {
        banners.remove(victim);
    }
}

/// A double pulse in the heart's intensity, only while the worker beat is
/// current. Keep stale/unknown hearts static so UI motion cannot imply liveness.
fn animated_heart(elapsed_ms: u64, beat_age_secs: Option<i64>, color_on: bool) -> String {
    let heart = crate::style::heartbeat_heart(beat_age_secs, color_on);
    let healthy = beat_age_secs.is_some_and(|age| crate::style::missed_heartbeats(age) == 0);
    if color_on && healthy {
        let intensity = if matches!((elapsed_ms / FRAME_MS) % 5, 0 | 2) {
            "1"
        } else {
            "22"
        };
        format!("\x1b[{intensity}m{heart}\x1b[22m")
    } else {
        heart
    }
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
/// Rendered from the banner's own clock on EVERY paint — including the idle
/// heartbeat's — so the runtime ticks even when the REPL isn't repainting.
pub fn fmt_elapsed(ms: u64) -> String {
    // Delegates to THE duration vocabulary rather than re-deriving the buckets:
    // the runtime cell and the beat cell sit a space apart on this very row, so
    // any divergence between two formatters is visible side by side (the old
    // `20m` beat beside a `3m40s` runtime). It also caps this cell's width at
    // the same 6 inside a day, which is what [`RUNTIME_COL`] is sized against.
    crate::style::fmt_duration((ms / 1000).min(i64::MAX as u64) as i64)
}

/// Collapse `text` to a single line and truncate to `max` visible chars with a
/// trailing ellipsis. Shared by the task hint and the status row so neither can
/// wrap the pinned footer.
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

/// Left-align `text` in a `width`-wide cell: clipped with an ellipsis when it
/// overflows, space-padded when it falls short. Padding is measured in TERMINAL
/// CELLS (not chars) so a CJK or emoji status can't shove the next column out of
/// alignment.
fn col(text: &str, width: usize) -> String {
    use unicode_width::UnicodeWidthStr;
    let clipped = one_line(text, width);
    let pad = width.saturating_sub(clipped.width());
    format!("{clipped}{}", " ".repeat(pad))
}

/// Pad an ALREADY-STYLED cell out to `width` terminal cells.
///
/// [`col`] can't be used here: it routes through [`one_line`], which strips ANSI
/// to measure — so running the heart through it would discard the colour that
/// IS the liveness signal (green/amber/red). This measures the visible width
/// around the escape codes and only appends spaces, leaving the styling intact.
/// Never clips: callers pass cells whose content has a known ceiling
/// ([`BEAT_COL`]), and silently truncating an escape sequence would bleed colour
/// into the rest of the row.
fn pad_styled(styled: &str, width: usize) -> String {
    use unicode_width::UnicodeWidthStr;
    let visible = strip_ansi(styled).width();
    format!("{styled}{}", " ".repeat(width.saturating_sub(visible)))
}

/// Split a composed status string into its `(state, message)` columns.
///
/// Worker status arrives pre-joined as `"<state> · <activity>"` (and sometimes
/// `"<state> — <detail>"`), because that's the shape `:workers` prints. The tray
/// wants the lifecycle word in its own cell, so peel the leading state token off
/// when it IS one and leave anything else whole as the message — an unrecognised
/// head is free text, not a state, and must not be silently promoted into the
/// state column.
pub(crate) fn split_status(status: &str) -> (&str, &str) {
    /// The lifecycle words the worker list and the tray itself emit.
    const STATES: [&str; 8] = [
        "queued",
        "pending",
        "running",
        "coordinating",
        "awaiting_batch",
        "done",
        "failed",
        "stopped",
    ];
    let s = status.trim();
    let (head, rest) = match s.split_once(char::is_whitespace) {
        Some((h, r)) => (h, r.trim_start()),
        None => (s, ""),
    };
    if STATES.contains(&head) {
        // Drop the separator the state was joined to its detail with, so the
        // message cell starts on the detail itself.
        (
            head,
            rest.trim_start_matches(['·', '—', '-', ':']).trim_start(),
        )
    } else {
        ("", s)
    }
}

/// Compose the status row's three fixed cells: runtime, state, message.
///
/// Shared by [`render`] and [`render_queued`] so a live banner and a queued line
/// line up with each other, not just with their own kind.
///
/// The [`CELL_SEP`] middot pins the state/message boundary — but only when there
/// IS a state to bound. An unrecognised status head leaves the state cell empty
/// (see [`split_status`]), and a separator hanging off an empty column is noise,
/// so it degrades to a space of the same width and the message column stays put.
fn status_cells(elapsed_ms: u64, state: &str, message: &str) -> String {
    let sep = if state.trim().is_empty() {
        " "
    } else {
        CELL_SEP
    };
    // `trim_end`: the message is the LAST cell, so when it is empty the padding
    // of the cells before it would otherwise trail off the row as invisible
    // filler — padding exists to position the NEXT column, and there isn't one.
    format!(
        "{} {} {sep} {}",
        col(&fmt_elapsed(elapsed_ms), RUNTIME_COL),
        col(state, STATE_COL),
        one_line(message, MESSAGE_COL)
    )
    .trim_end()
    .to_string()
}

/// Build ONE banner's two rows. Pure: the caller supplies the elapsed clock, the
/// identity, the status text, and the terminal verdict, so the rendering is
/// unit-testable without touching the global stack or a TTY.
///
/// Returns `(escalation_row, status_row)`. The header carries a static identity
/// glyph, short worker ID and task. The status starts with a single liveness
/// heart: current beats pulse, missed beats show their age and colour, unknown
/// beats stay hollow. Finished banners use the static [`IDLE_PREFIX`] instead.
pub fn render(
    elapsed_ms: u64,
    id: &str,
    task: &str,
    status: &str,
    terminal: Option<bool>,
    beat_age_secs: Option<i64>,
    color_on: bool,
) -> (String, String) {
    render_banner(
        elapsed_ms,
        Some(id),
        task,
        status,
        terminal,
        beat_age_secs,
        color_on,
    )
}

/// Build ONE SUB-WORKER's two rows — identical to [`render`] except the head row
/// carries NO worker id.
///
/// A child's `w_…` is pure noise in the tray: it is already positioned under its
/// parent by the indent and the bent arrow, and nothing the operator can do
/// takes a sub-worker id (Shift-Tab cycles the PINNED roots, `:workers` lists
/// the session's own jobs). What the operator actually reads off a nested row is
/// the TIMER, the STATUS and the SUMMARY — so the row spends its columns on
/// those instead, which also buys back the 9 columns the id was costing at the
/// deepest indent. There is no `id` parameter at all, so a caller cannot
/// accidentally paint one.
pub fn render_sub(
    elapsed_ms: u64,
    task: &str,
    status: &str,
    terminal: Option<bool>,
    beat_age_secs: Option<i64>,
    color_on: bool,
) -> (String, String) {
    render_banner(
        elapsed_ms,
        None,
        task,
        status,
        terminal,
        beat_age_secs,
        color_on,
    )
}

/// Shared body of [`render`] and [`render_sub`]: `id` is `Some` for a root
/// banner and `None` for a nested sub-worker row.
fn render_banner(
    elapsed_ms: u64,
    id: Option<&str>,
    task: &str,
    status: &str,
    terminal: Option<bool>,
    beat_age_secs: Option<i64>,
    color_on: bool,
) -> (String, String) {
    use crate::style::{Color, paint_with};
    // IDENTITY, not motion: the head glyph is stable for the whole life of the
    // banner so the operator's eye can lock onto it. The animation lives in the
    // status row's prefix cell below.
    let glyph = match terminal {
        None => LIVE_GLYPH,
        Some(false) => "✅",
        Some(true) => "⚠️",
    };
    let short = id.map(crate::batch::short_id);
    // Peel an auto-offloaded brief's quoted-conversation digest / resume
    // scaffolding down to the operator's ask before clipping, so the banner
    // reads as the WORK instead of `=== Recent conversation context (for refe…`.
    let hint = one_line(&crate::worker::task_headline(task), TASK_HINT_MAX);
    // A sub-worker row has no id cell at all, so the hint slides left into the
    // glyph — never leaving a ` · ` dangling off an empty column.
    let head = match (short, hint.as_str()) {
        (Some(short), "") => format!("{glyph} {short}"),
        (Some(short), hint) => format!("{glyph} {short} · {hint}"),
        (None, "") => glyph.to_string(),
        (None, hint) => format!("{glyph} {hint}"),
    };
    let top = paint_with(&head, Color::Cyan, color_on);
    let indicator = match terminal {
        None => animated_heart(elapsed_ms, beat_age_secs, color_on),
        Some(_) => paint_with(IDLE_PREFIX, Color::Dim, color_on),
    };
    let (state, message) = split_status(status);
    let bottom = format!(
        "{}{} {}",
        paint_with("   ", Color::Dim, color_on),
        // Fixed cell: a pulsing `♥` and a stale `⚠ 3h04m` must not shift the
        // runtime column that follows them.
        pad_styled(&indicator, BEAT_COL),
        paint_with(
            &status_cells(elapsed_ms, state, message),
            Color::Dim,
            color_on
        )
    );
    (top, bottom)
}

/// Build ONE queued line's two rows. Pure, like [`render`], and deliberately
/// STATIC: nothing is executing for this line yet, so there is no heart and no
/// pulse to imply otherwise — just the hourglass, the operator's own text, and
/// its place in the queue.
///
/// `position`/`total` are 1-based FIFO coordinates (`1 of 2`), matching the
/// order the REPL drains its type-ahead queue, so the operator can see not just
/// THAT their line survived but when it will run relative to the others.
/// `extra_status` carries the block's `· +N more escalated` overflow tail when
/// this row happens to be the last one painted.
pub fn render_queued(
    elapsed_ms: u64,
    line: &str,
    position: usize,
    total: usize,
    extra_status: &str,
    color_on: bool,
) -> (String, String) {
    use crate::style::{Color, paint_with};
    let hint = one_line(line, TASK_HINT_MAX);
    let head = if hint.is_empty() {
        format!("{QUEUED_GLYPH} {QUEUED_LABEL}")
    } else {
        format!("{QUEUED_GLYPH} {QUEUED_LABEL} · {hint}")
    };
    // Dim, not cyan: a queued line is WAITING, and the live escalations above it
    // should keep the eye. The whole pair is dim so the row reads as background.
    let top = paint_with(&head, Color::Dim, color_on);
    // Same three cells as a live banner (runtime · state · message) so a queued
    // line's columns land on the live rows' columns instead of starting its own
    // layout; `queued` IS this row's lifecycle state.
    let detail = format!("{position} of {total} — runs when this turn ends{extra_status}");
    let bottom = format!(
        "{}{} {}",
        paint_with("   ", Color::Dim, color_on),
        // Same fixed BEAT cell as a live row, so a queued line's runtime column
        // lands on the live rows' runtime column.
        pad_styled(&paint_with(IDLE_PREFIX, Color::Dim, color_on), BEAT_COL),
        paint_with(
            &status_cells(elapsed_ms, QUEUED_LABEL, &detail),
            Color::Dim,
            color_on
        )
    );
    (top, bottom)
}

/// Build ONE sub-work ledger item's two rows. Pure, like [`render`], and
/// STATIC like [`render_queued`] — for the same reason: there is no process
/// behind a ledger item, so a pulsing heart would claim liveness that does not
/// exist. The parent's own row carries the heartbeat for all of its sub-work.
///
/// Dim on BOTH rows: a ledger item is the parent's work broken out, not an
/// independent escalation, so it reads as detail under the row above instead of
/// competing with it for the eye. No id cell either — the item's slug is a
/// coordinator-internal key the operator can't reach for (same argument as
/// [`render_sub`], one step further: a ledger item isn't even a worker).
///
/// `elapsed_ms` is how long the ITEM has been on screen, not how long the worker
/// has been on it: the ledger is a declaration, not an instrumented span, so the
/// row's clock starts when the tray first saw the item. It keeps the fixed
/// [`BEAT_COL`] cell so the runtime column lands on the live rows' runtime
/// column.
pub fn render_subwork(
    elapsed_ms: u64,
    label: &str,
    status: &str,
    color_on: bool,
) -> (String, String) {
    use crate::style::{Color, paint_with};
    let (state, message) = split_status(status);
    // Same verdict glyphs as a banner so a finished slice reads the way a
    // finished worker does; everything still open wears the puzzle piece —
    // "part of the thing above", which is precisely what a ledger item is.
    let glyph = match state {
        "done" => "✅",
        "failed" | "stopped" => "⚠️",
        _ => SUBWORK_GLYPH,
    };
    let hint = one_line(label, TASK_HINT_MAX);
    let head = if hint.is_empty() {
        glyph.to_string()
    } else {
        format!("{glyph} {hint}")
    };
    let top = paint_with(&head, Color::Dim, color_on);
    let bottom = format!(
        "{}{} {}",
        paint_with("   ", Color::Dim, color_on),
        pad_styled(&paint_with(IDLE_PREFIX, Color::Dim, color_on), BEAT_COL),
        paint_with(
            &status_cells(elapsed_ms, state, message),
            Color::Dim,
            color_on
        )
    );
    (top, bottom)
}

/// The live banner rows, NEWEST FIRST — one `(escalation_row, status_row)` pair
/// per painted escalation, empty when nothing is pinned. Reads the global stack
/// and derives each animation frame from that banner's own clock, so any paint
/// path — including the idle heartbeat, which has no `Session` — can render it.
///
/// `max_banners` is how many pairs the CALLER can actually fit (the footer knows
/// the window height; this module doesn't). It is additionally capped at
/// [`max_visible`]. Any escalation that doesn't fit is not silently dropped: the
/// last painted status row carries a `· +N more escalated` tail, so the operator
/// always sees that more work is in flight.
pub fn rows(color_on: bool, max_banners: usize) -> Vec<(String, String)> {
    let Ok(banners) = BANNERS.lock() else {
        return Vec::new();
    };
    let visible = max_banners.min(max_visible()).min(banners.len());
    if visible == 0 {
        return Vec::new();
    }
    let hidden = banners.len() - visible;
    // FIFO queue coordinates for the queued rows (`1 of 2`), derived from the
    // store's own order — oldest queued row is `1`, which is the next line the
    // REPL will run. Keyed by row id because the paint order below is REVERSED.
    let queued_total = banners.iter().filter(|b| b.kind == Kind::Queued).count();
    let queued_pos: std::collections::HashMap<&str, usize> = banners
        .iter()
        .filter(|b| b.kind == Kind::Queued)
        .enumerate()
        .map(|(n, b)| (b.id.as_str(), n + 1))
        .collect();
    // Newest first, with each coordinator's sub-workers pulled up directly
    // beneath it: the top banner is the escalation the operator just launched
    // (matching `cycle_worker`'s newest-first Shift-Tab ordering), and a
    // fan-out reads as that banner's own nested work.
    // ATTACH HIGHLIGHT: the worker the operator is `:attach`ed to wears the
    // SAME band as their own submitted input, so "the thing you're talking to"
    // reads identically in the tray and in the transcript. Read once per paint
    // (not per row) — `ATTACHED` is a separate mutex from `BANNERS`, and nothing
    // takes `BANNERS` while holding it, so there is no lock cycle here.
    let attached_id = attached();
    let band_width = crate::style::footer_width();
    ordered_rows(&banners)
        .into_iter()
        .take(visible)
        .enumerate()
        .map(|(i, (idx, depth))| {
            let b = &banners[idx];
            // The overflow tail rides the LAST painted status row (the oldest
            // visible banner) rather than claiming a row of its own, so the
            // block height stays an exact multiple of ROWS_PER_BANNER.
            let extra = if hidden > 0 && i + 1 == visible {
                format!(" · +{hidden} more escalated")
            } else {
                String::new()
            };
            let elapsed_ms = b.pinned_at.elapsed().as_millis() as u64;
            if b.kind == Kind::Queued {
                return render_queued(
                    elapsed_ms,
                    &b.task,
                    queued_pos.get(b.id.as_str()).copied().unwrap_or(1),
                    queued_total.max(1),
                    &extra,
                    color_on,
                );
            }
            if b.kind == Kind::SubWork {
                // Ledger rows are ALWAYS nested: the row describes a slice of
                // the PARENT's work, so even an orphan (parent evicted, or
                // retired after its dwell) keeps its indent rather than posing
                // as a root escalation the operator could attach to.
                let pair = render_subwork(
                    elapsed_ms,
                    &b.task,
                    &format!("{}{extra}", b.status),
                    color_on,
                );
                return indent_pair(pair, depth.max(1), color_on);
            }
            let terminal = b.terminal_at.map(|_| b.failed);
            // Age the absolute beat here, on the paint path, so each heart keeps
            // ticking (and can go yellow → red) between the REPL's throttled polls.
            let beat_age = b.beat.map(|t| (now_unix_secs() - t).max(0));
            // Roots carry their short id; nested sub-workers deliberately do
            // NOT — the indent + arrow already say whose work it is, so the row
            // spends its columns on the timer, status and summary instead.
            let pair = if depth == 0 {
                render(
                    elapsed_ms,
                    &b.id,
                    &b.task,
                    &format!("{}{extra}", b.status),
                    terminal,
                    beat_age,
                    color_on,
                )
            } else {
                render_sub(
                    elapsed_ms,
                    &b.task,
                    &format!("{}{extra}", b.status),
                    terminal,
                    beat_age,
                    color_on,
                )
            };
            let pair = indent_pair(pair, depth, color_on);
            // Attached worker → operator-input band on BOTH of its rows, so the
            // pair reads as one highlighted block. Everything else is untouched.
            if attached_id.as_deref() == Some(b.id.as_str()) {
                (
                    crate::style::highlight_row(&pair.0, band_width, color_on),
                    crate::style::highlight_row(&pair.1, band_width, color_on),
                )
            } else {
                pair
            }
        })
        .collect()
}

/// Nest one already-rendered banner pair under its parent: `depth` levels of
/// indent plus a bent arrow on the escalation row, so a sub-worker reads as the
/// row above's own work rather than as another top-level escalation.
///
/// Depth 0 is returned untouched — a root keeps the tray's established shape,
/// byte for byte.
fn indent_pair(pair: (String, String), depth: usize, color_on: bool) -> (String, String) {
    use crate::style::{Color, paint_with};
    if depth == 0 {
        return pair;
    }
    let (top, bottom) = pair;
    let pad = SUB_INDENT.repeat(depth.min(MAX_SUB_DEPTH));
    // The arrow is dim so the eye reads the worker's own glyph first; the status
    // row gets matching whitespace only, keeping the two rows left-aligned.
    let arrow = paint_with(SUB_ARROW, Color::Dim, color_on);
    (
        format!("{pad}{arrow} {top}"),
        // The two extra columns line the status row up under the escalation
        // row's glyph: the arrow and the space that follows it.
        format!("{pad}  {bottom}"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use unicode_width::UnicodeWidthStr;

    /// Serializes the tests that mutate the process-global stack so they can't
    /// interleave with each other under the test harness's thread pool.
    fn lock() -> std::sync::MutexGuard<'static, ()> {
        static L: Mutex<()> = Mutex::new(());
        L.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// A ledger row DESCRIBES a slice of its parent's work; it has no process
    /// of its own. Three things therefore have to hold at once, and all three
    /// have bitten a nesting feature before: the row paints INDENTED under its
    /// parent and spends no columns on an id (the arrow already says whose work
    /// it is), it draws the STATIC idle prefix rather than a pulsing heart (a
    /// beat would claim a liveness nothing is producing), and it stays out of
    /// `pinned_ids`/`tray_order_ids` — both feed the REPL, which would try to
    /// resolve `sw:…` as a coordinator run id and Shift-Tab onto a worker that
    /// does not exist.
    #[test]
    fn subwork_rows_render_nested_static_and_stay_out_of_the_worker_cycle() {
        let _g = lock();
        let drop_probes = || {
            BANNERS
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .retain(|b| b.id != "w_sw_probe" && !b.id.starts_with("sw:w_sw_probe:"));
        };
        let sw_rows = || {
            BANNERS
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .iter()
                .filter(|b| b.kind == Kind::SubWork)
                .map(|b| (b.id.clone(), b.task.clone(), b.status.clone()))
                .collect::<Vec<_>>()
        };
        drop_probes();
        push_banner("w_sw_probe", "parent worker");

        sync_subwork(
            "w_sw_probe",
            &[SubWorkRow {
                id: "wire-tray".to_string(),
                label: "wire the tray rows".to_string(),
                status: "running".to_string(),
                note: "editing escalation.rs".to_string(),
            }],
        );
        assert_eq!(
            sw_rows(),
            vec![(
                "sw:w_sw_probe:wire-tray".to_string(),
                "wire the tray rows".to_string(),
                "running · editing escalation.rs".to_string()
            )],
            "the note rides the status cell as `state · detail`"
        );

        // Paint and locate the ledger row by its label.
        let painted = rows(false, 8);
        let pair = painted
            .iter()
            .find(|(a, b)| a.contains("wire the tray rows") || b.contains("wire the tray rows"))
            .expect("the ledger row paints");
        let text = format!("{}\n{}", pair.0, pair.1);
        assert!(text.contains(SUB_ARROW), "ledger rows are always nested");
        assert!(
            !text.contains("w_sw_probe"),
            "nested rows carry no id cell: {text}"
        );
        assert!(
            text.contains(IDLE_PREFIX),
            "a ledger row has no process, so it draws the static prefix: {text}"
        );

        // Neither REPL-facing id list may ever hand out a synthetic row id.
        assert!(
            !pinned_ids().iter().any(|id| id.starts_with("sw:")),
            "pinned_ids feeds worker lookups"
        );
        assert!(
            !tray_order_ids().iter().any(|id| id.starts_with("sw:")),
            "tray_order_ids feeds Shift-Tab cycling"
        );

        // Same key, moved status → UPDATE IN PLACE, never a second row.
        sync_subwork(
            "w_sw_probe",
            &[SubWorkRow {
                id: "wire-tray".to_string(),
                label: "wire the tray rows".to_string(),
                status: "done".to_string(),
                note: String::new(),
            }],
        );
        assert_eq!(
            sw_rows(),
            vec![(
                "sw:w_sw_probe:wire-tray".to_string(),
                "wire the tray rows".to_string(),
                "done".to_string()
            )],
            "re-syncing refreshes the row instead of duplicating it"
        );

        // A RETRACTED item (and the empty-ledger case) clears this parent's
        // rows and leaves the parent's own banner alone.
        sync_subwork("w_sw_probe", &[]);
        assert!(sw_rows().is_empty(), "vanished items are unpinned");
        assert!(
            BANNERS
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .iter()
                .any(|b| b.id == "w_sw_probe" && b.kind == Kind::Escalation),
            "syncing a ledger must not evict the parent"
        );
        drop_probes();
    }

    /// A queued line's tray row means "typed, NOT yet run", so the rows must
    /// retire in the same FIFO order the REPL drains the type-ahead queue —
    /// row N disappears as line N starts executing — and a wholesale discard
    /// must drop every queued row at once. Escalation banners are a different
    /// kind and must survive both paths untouched: the tray is shared, and a
    /// queued-line drain that evicted a live worker's banner would lose the
    /// only on-screen trace of that worker.
    #[test]
    fn queued_rows_retire_oldest_first_and_leave_escalations_alone() {
        let _g = lock();
        clear_queued();
        let queued_tasks = || {
            BANNERS
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .iter()
                .filter(|b| b.kind == Kind::Queued)
                .map(|b| b.task.clone())
                .collect::<Vec<_>>()
        };
        let has_escalation = |id: &str| {
            BANNERS
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .iter()
                .any(|b| b.id == id && b.kind == Kind::Escalation)
        };
        let drop_probe = || {
            BANNERS
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .retain(|b| b.id != "w_queued_probe");
        };
        drop_probe();
        push_banner("w_queued_probe", "a live worker");

        // Distinct ids per pin, so the rows can be told apart in the footer.
        let first = pin_queued("what is the status of ISS-414652");
        let second = pin_queued(":workers");
        assert_ne!(first, second, "each queued row needs its own id");
        assert_eq!(queued_count(), 2);
        assert_eq!(
            queued_tasks(),
            vec![
                "what is the status of ISS-414652".to_string(),
                ":workers".to_string()
            ],
            "rows are stored in arrival order"
        );

        // Drain: oldest row goes first, matching the queue's own drain order.
        assert!(unpin_oldest_queued(), "a queued row was pinned");
        assert_eq!(queued_tasks(), vec![":workers".to_string()]);

        // Discard: every remaining queued row goes, the escalation stays.
        clear_queued();
        assert_eq!(queued_count(), 0);
        assert!(
            has_escalation("w_queued_probe"),
            "clearing queued rows must not evict escalation banners"
        );

        // Draining an empty queue is a silent no-op — the drain path calls this
        // unconditionally, including for lines that predate the tray.
        assert!(!unpin_oldest_queued());
        assert!(has_escalation("w_queued_probe"));
        drop_probe();
    }

    /// A RESUMED worker must get its tray row back. `worker::resume_in_place`
    /// re-pins under the worker's visible id, and this locks in the two store
    /// behaviours that depends on: re-pinning an id whose banner was already
    /// retired RE-CREATES a live row, and re-pinning one that is still dwelling
    /// on a terminal verdict REFRESHES that row in place (verdict cleared, no
    /// duplicate). Without both, a worker continued by a follow-up shows up in
    /// `:workers` with no banner — five live workers, one tray row.
    #[test]
    fn repinning_restores_a_retired_banner_and_clears_a_dwelling_verdict() {
        let _g = lock();
        let probe = |id: &str| {
            let banners = BANNERS.lock().unwrap_or_else(|e| e.into_inner());
            banners
                .iter()
                .find(|b| b.id == id)
                .map(|b| (b.terminal_at.is_some(), b.failed, b.task.clone()))
        };
        let count = || {
            BANNERS
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .iter()
                .filter(|b| b.id.starts_with("w_resume"))
                .count()
        };
        let drop_resume_banners = || {
            BANNERS
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .retain(|b| !b.id.starts_with("w_resume"));
        };
        drop_resume_banners();

        // Retired banner (sweep dropped it after DWELL) → re-pin re-creates it,
        // live, with the worker's original task hint.
        push_banner("w_resume1", "fix the thing");
        drop_resume_banners();
        assert_eq!(probe("w_resume1"), None);
        push_banner("w_resume1", "fix the thing");
        assert_eq!(
            probe("w_resume1"),
            Some((false, false, "fix the thing".to_string()))
        );

        // Still-dwelling failure verdict → re-pin refreshes in place: one row,
        // animating again, no stale ⚠️.
        push_banner("w_resume2", "fix the other thing");
        note_terminal("w_resume2", true);
        assert_eq!(probe("w_resume2").map(|p| (p.0, p.1)), Some((true, true)));
        push_banner("w_resume2", "fix the other thing");
        assert_eq!(probe("w_resume2").map(|p| (p.0, p.1)), Some((false, false)));
        assert_eq!(count(), 2, "re-pin must refresh, not stack a duplicate");

        drop_resume_banners();
    }

    #[test]
    fn heart_pulses_without_changing_width_colour_or_head_identity() {
        let at = |ms| render(ms, "w_1", "build it", "coordinating", None, Some(0), true);
        let (head, bright) = at(0);
        let (next_head, quiet) = at(FRAME_MS);
        assert_eq!(head, next_head);
        assert_ne!(bright, quiet);
        assert!(bright.contains("\x1b[1m"));
        assert!(quiet.contains("\x1b[22m"));
        for row in [&bright, &quiet] {
            let plain = strip_ansi(row);
            assert!(plain.starts_with("   ♥ "), "{plain}");
            assert_eq!(plain.matches('♥').count(), 1);
            assert!(
                !plain
                    .chars()
                    .any(|c| ('\u{2800}'..='\u{28ff}').contains(&c))
            );
            assert!(row.contains(crate::style::Color::Green.code()));
        }
        assert_eq!("♥".width(), "♡".width());
        for age in [None, Some(50), Some(300)] {
            assert_eq!(
                animated_heart(0, age, true),
                animated_heart(FRAME_MS, age, true)
            );
        }
        assert_eq!(animated_heart(0, Some(0), false), "♥");
        assert_eq!(animated_heart(FRAME_MS, Some(0), false), "♥");
    }

    #[test]
    fn live_banner_animates_and_terminal_banner_freezes() {
        // Terminal verdicts freeze the glyph: motion means "still working".
        let (ok, ok_bot) = render(
            FRAME_MS * 7,
            "w_abcdef123456",
            "b",
            "done",
            Some(false),
            Some(0),
            false,
        );
        assert!(ok.starts_with("✅"), "{ok}");
        // A finished banner rests on the static elbow — no spinner.
        assert!(
            ok_bot.starts_with(&format!("   {IDLE_PREFIX} ")),
            "{ok_bot}"
        );
        let (bad, bad_bot) = render(
            FRAME_MS * 7,
            "w_abcdef123456",
            "b",
            "failed",
            Some(true),
            Some(0),
            false,
        );
        assert!(bad.starts_with("⚠️"), "{bad}");
        assert!(
            bad_bot.starts_with(&format!("   {IDLE_PREFIX} ")),
            "{bad_bot}"
        );
        assert!(!ok_bot.contains('♥'));
        assert!(!bad_bot.contains('♥'));
    }

    #[test]
    fn rows_carry_short_id_task_hint_and_indented_status() {
        let (top, bottom) = render(
            0,
            "w_abcdef123456",
            "build   and\nopen pr",
            "coordinating · 1m12s",
            None,
            Some(0),
            false,
        );
        // Short id (not the full run id) keeps the row readable.
        assert!(
            top.contains(crate::batch::short_id("w_abcdef123456")),
            "{top}"
        );
        // Internal whitespace/newlines collapse so the hint can't wrap the footer.
        assert!(top.contains("build and open pr"), "{top}");
        // The status row is indented under the message, led by one heart, with
        // runtime and state each in a fixed cell so every banner's message
        // column starts at the same screen position.
        assert_eq!(bottom, "   ♥        0s       coordi… · 1m12s", "{bottom}");
    }

    #[test]
    /// The BEAT cell is FIXED: a healthy `♥`, a stale `♥ 20m00s` and a finished
    /// `↳` all occupy [`BEAT_COL`] cells, so the runtime/state/message columns
    /// behind them start at the same screen offset on every row of the tray.
    /// This is the regression the ragged tray actually showed — hearts of
    /// different ages pushing each row's columns to a different place.
    fn beat_cell_is_fixed_width_so_later_columns_never_shift() {
        use unicode_width::UnicodeWidthStr;
        let row = |beat: Option<i64>, terminal| {
            let (_, bottom) = render(0, "w_abcdef123456", "t", "running", terminal, beat, false);
            bottom
        };
        let rows = [
            row(Some(0), None),        // fresh: bare glyph
            row(Some(50), None),       // one missed beat: `♥ 50s`
            row(Some(1_200), None),    // long stale: `♥ 20m00s` (widest)
            row(Some(11_040), None),   // hours: `♥ 3h04m`
            row(None, None),           // unknown beat: hollow glyph
            row(Some(0), Some(false)), // terminal: the static elbow
        ];
        // Every row's message column ("running") lands on one offset.
        let offsets: Vec<usize> = rows
            .iter()
            .map(|r| {
                let plain = strip_ansi(r);
                let idx = plain.find("running").unwrap_or_else(|| panic!("{plain}"));
                plain[..idx].width()
            })
            .collect();
        assert!(
            offsets.windows(2).all(|w| w[0] == w[1]),
            "state column must not move with beat age: {offsets:?}\n{rows:#?}"
        );
        // And the BEAT cell itself is exactly BEAT_COL wide — the widest value
        // it can hold must not overflow and push the row out.
        for r in &rows {
            let plain = strip_ansi(r);
            let idx = plain.find("running").unwrap();
            assert_eq!(
                plain[..idx].width(),
                3 + BEAT_COL + 1 + RUNTIME_COL + 1,
                "{plain}"
            );
        }
    }

    #[test]
    /// A nested row spends its columns on the TIMER, STATUS and SUMMARY — the
    /// child's own `w_…` is noise under a parent that already locates it.
    fn sub_rows_drop_the_id_and_keep_timer_status_and_summary() {
        let (top, bottom) = render_sub(
            72_000,
            "build   and\nopen pr",
            "coordinating",
            None,
            Some(0),
            false,
        );
        // No id — neither the raw run id nor its short form.
        assert!(!top.contains("w_"), "{top}");
        // …but the summary survives, flush against the glyph (no dangling ` · `).
        assert!(top.contains("build and open pr"), "{top}");
        assert!(!top.contains(" ·  "), "{top}");
        // Timer and status still ride the second row — the state lands in its
        // own fixed cell, so compare against the CELL (clipped by STATE_COL)
        // rather than the raw word.
        assert!(bottom.contains("1m12s"), "{bottom}");
        assert!(
            bottom.contains(col("coordinating", STATE_COL).trim_end()),
            "{bottom}"
        );
    }

    #[test]
    /// Regression (w_pckv64iw): the banner hint clipped the RAW brief, so an
    /// escalation from an auto-offloaded run advertised the quoted-context
    /// header rather than what the worker is stuck on.
    fn row_hint_peels_the_context_digest() {
        let task = "=== Recent conversation context (for reference — the operator's) ===\n\
                    Operator: blah blah\n\
                    === End context ===\n\n\
                    fix the buggy ui";
        let (top, _) = render(0, "w_pckv64iw", task, "coordinating", None, Some(0), false);
        assert!(top.contains("fix the buggy ui"), "{top}");
        assert!(!top.contains("Recent conversation"), "{top}");
    }

    #[test]
    fn long_task_and_status_are_truncated() {
        let task = "x".repeat(400);
        let status = "y".repeat(400);
        let (top, bottom) = render(0, "w_a", &task, &status, None, Some(0), false);
        assert!(top.contains('…'), "{top}");
        assert!(top.chars().count() < 120, "escalation row too wide: {top}");
        assert!(bottom.contains('…'), "{bottom}");
        // The fixed columns add their own width on top of the message budget,
        // so the row's ceiling is the message cell plus the two cells, their
        // gutters, the state/message separator cell, and the indent+indicator.
        assert!(
            bottom.chars().count() <= MESSAGE_COL + RUNTIME_COL + STATE_COL + BEAT_COL + 10,
            "{bottom}"
        );
    }

    #[test]
    fn pin_then_clear_toggles_active_and_rows() {
        let _g = lock();
        clear();
        assert!(!active());
        assert!(rows(false, MAX_VISIBLE).is_empty());
        assert_eq!(row_count(), 0);

        pin("w_deadbeefcafe", "ship the thing");
        assert!(active());
        assert_eq!(pinned_ids(), vec!["w_deadbeefcafe".to_string()]);
        assert_eq!(row_count(), ROWS_PER_BANNER);
        let painted = rows(false, MAX_VISIBLE);
        assert_eq!(painted.len(), 1);
        let (top, bottom) = &painted[0];
        assert!(!top.contains("escalated →"), "{top}");
        // Fresh banners state the pre-start truth rather than inventing progress.
        assert!(bottom.contains("queued"), "{bottom}");

        set_status("w_deadbeefcafe", "coordinating · 12s · 🔧 read_file");
        let (_, bottom) = rows(false, MAX_VISIBLE).remove(0);
        assert!(bottom.contains("coordi… · 12s"), "{bottom}");

        clear();
        assert!(!active());
        assert!(rows(false, MAX_VISIBLE).is_empty());
    }

    #[test]
    fn closing_a_worker_unpins_only_its_own_banner() {
        // `:close` is a dismissal of the whole worker surface, and the tray is
        // part of it. Before `unpin` existed the ONLY removal paths were
        // `sweep` (terminal + dwell) and `clear` (everything), so closing a
        // still-RUNNING coordinator left its two footer rows animating forever
        // for a worker already gone from `:workers`.
        let _g = lock();
        clear();
        pin("w_keepme000001", "sibling still working");
        pin("w_closeme00002", "the one being closed");
        assert_eq!(count(), 2);

        // Removal is immediate (no terminal mark, no dwell wait) and reports
        // that a banner was really there.
        assert!(unpin("w_closeme00002"));
        assert_eq!(count(), 1);
        assert_eq!(row_count(), ROWS_PER_BANNER);

        // Strictly id-scoped: the live sibling keeps its rows and its animation.
        let painted = rows(false, MAX_VISIBLE);
        assert_eq!(painted.len(), 1);
        assert!(
            painted[0].0.contains("sibling still working"),
            "{:?}",
            painted[0].0
        );
        assert!(animating(), "the untouched sibling must still animate");

        // Closing a worker that never escalated must not disturb the tray, and
        // says so by returning false.
        assert!(!unpin("w_neverpinned1"));
        assert_eq!(count(), 1);

        // Last banner out shrinks the footer back to nothing.
        assert!(unpin("w_keepme000001"));
        assert!(!active());
        assert_eq!(row_count(), 0);
        assert!(rows(false, MAX_VISIBLE).is_empty());
        clear();
    }

    #[test]
    /// `:attach` makes ONE worker the thing the operator is typing at, so its
    /// tray banner wears the exact band the submitted-input echo uses — the
    /// highlight is the shared "this is your channel" signal, not a second
    /// vocabulary. Scoped to that id and to BOTH of its rows (the pair reads as
    /// one block); every sibling banner is handed back untouched, and detaching
    /// restores the plain tray.
    fn attaching_bands_only_that_workers_pair() {
        let _g = lock();
        clear();
        set_attached(None);
        pin("w_attached0001", "the attached worker");
        pin("w_sibling00002", "someone else");

        let band = crate::style::input_highlight_bg();
        let banded = |painted: &[(String, String)]| {
            painted
                .iter()
                .filter(|(top, bottom)| top.contains(&band) || bottom.contains(&band))
                .count()
        };

        // Nothing attached → no band anywhere.
        assert_eq!(banded(&rows(true, MAX_VISIBLE)), 0);

        set_attached(Some("w_attached0001"));
        let painted = rows(true, MAX_VISIBLE);
        assert_eq!(painted.len(), 2);
        assert_eq!(banded(&painted), 1, "only the attached pair is banded");
        let (top, bottom) = painted
            .iter()
            .find(|(t, _)| t.contains(&band))
            .expect("attached banner must be banded");
        assert!(top.contains("the attached worker"), "{top}");
        assert!(bottom.contains(&band), "status row bands too: {bottom}");

        // Attaching elsewhere moves the band rather than adding a second one.
        set_attached(Some("w_sibling00002"));
        let painted = rows(true, MAX_VISIBLE);
        assert_eq!(banded(&painted), 1);
        assert!(
            painted
                .iter()
                .find(|(t, _)| t.contains(&band))
                .is_some_and(|(t, _)| t.contains("someone else")),
            "{painted:?}"
        );

        // Colors off degrades to the untouched rows (never mangled escapes).
        assert_eq!(banded(&rows(false, MAX_VISIBLE)), 0);

        // Detaching restores the plain tray.
        set_attached(None);
        assert_eq!(banded(&rows(true, MAX_VISIBLE)), 0);
        clear();
    }

    #[test]
    fn every_live_escalation_gets_its_own_banner_newest_first() {
        // THE regression this module was rewritten for: the store used to be a
        // single `Option<Banner>`, so a second escalation REPLACED the first and
        // the operator had no on-screen evidence the earlier one was still
        // burning tokens. Each live escalation must keep its own two rows.
        let _g = lock();
        clear();
        pin("w_first0000001", "first task");
        pin("w_second000002", "second task");
        set_status("w_first0000001", "coordinating · first status");
        set_status("w_second000002", "coordinating · second status");

        assert_eq!(count(), 2);
        assert_eq!(row_count(), ROWS_PER_BANNER * 2);
        let painted = rows(false, MAX_VISIBLE);
        assert_eq!(painted.len(), 2, "both escalations must paint");
        // Newest first — same ordering as `cycle_worker`'s Shift-Tab walk.
        assert!(painted[0].0.contains("second task"), "{:?}", painted[0].0);
        assert!(painted[1].0.contains("first task"), "{:?}", painted[1].0);
        // Each banner carries its OWN status row, not the newest one's.
        assert!(painted[0].1.contains("second status"), "{:?}", painted[0].1);
        assert!(painted[1].1.contains("first status"), "{:?}", painted[1].1);

        // Status + verdict are strictly id-scoped: finishing one leaves the
        // other animating.
        note_terminal("w_first0000001", true);
        let painted = rows(false, MAX_VISIBLE);
        assert!(painted[1].0.starts_with("⚠️"), "{:?}", painted[1].0);
        assert!(!painted[0].0.starts_with("⚠️"), "{:?}", painted[0].0);
        assert!(animating(), "the unfinished sibling must still animate");
        clear();
    }

    #[test]
    fn tray_groups_rows_by_phase_and_keeps_newest_first_inside_a_group() {
        // Read bottom-up from the prompt the tray must go `starting → done →
        // failed → running`, so in PAINT order (top first) the live work leads
        // and the not-yet-started bookkeeping trails. The tray paints at most
        // MAX_VISIBLE rows, so the ordering is asserted in two passes that each
        // fit under the cap.
        let _g = lock();

        // Pass 1: the three non-running phases, pinned in the WRONG order.
        clear();
        pin("w_start0000001", "starting task");
        pin("w_done00000001", "done task");
        set_status("w_done00000001", "done · wrapped up");
        pin("w_fail00000001", "failed task");
        set_status("w_fail00000001", "failed · blew up");
        assert_eq!(
            phase_order(&["failed task", "done task", "starting task"]),
            vec!["failed task", "done task", "starting task"],
        );

        // Pass 2: running work outranks every other phase, and newest-first
        // survives INSIDE the running group — the phase sort re-ranks classes,
        // it does not reshuffle a class.
        clear();
        pin("w_fail00000001", "failed task");
        set_status("w_fail00000001", "failed · blew up");
        pin("w_runold000001", "older running task");
        set_status("w_runold000001", "running · 🔧 read_file");
        pin("w_runnew000001", "newer running task");
        set_status("w_runnew000001", "coordinating · 🔧 grep_files");
        assert_eq!(
            phase_order(&["newer running task", "older running task", "failed task"]),
            vec!["newer running task", "older running task", "failed task"],
        );
        clear();
    }

    /// Paint the tray and label each row by whichever of `tasks` it carries,
    /// so a phase-ordering assertion reads as a list of task names.
    fn phase_order(tasks: &[&'static str]) -> Vec<&'static str> {
        rows(false, MAX_VISIBLE)
            .iter()
            .map(|(top, _)| {
                tasks
                    .iter()
                    .copied()
                    .find(|t| top.contains(t))
                    .unwrap_or("?")
            })
            .collect()
    }

    #[test]
    fn content_changes_raise_the_repaint_flag_exactly_once() {
        // The heartbeat only repaints while something ANIMATES, so a content
        // change on a still tray needs this flag or it sits unpainted until the
        // operator presses a key. Equally important: a no-op write must NOT
        // raise it — the REPL re-composes every row on every paint, so a
        // dirty-on-write would pin the flag high and busy-loop the heartbeat.
        let _g = lock();
        clear();
        pin("w_dirty0000001", "task");
        take_dirty();

        set_status("w_dirty0000001", "running · 🔧 read_file");
        assert!(take_dirty(), "a new status line must ask for a frame");
        set_status("w_dirty0000001", "running · 🔧 read_file");
        assert!(
            !take_dirty(),
            "re-composing the same status must stay quiet"
        );

        note_terminal("w_dirty0000001", true);
        assert!(
            take_dirty(),
            "a verdict re-ranks the row, so it must repaint"
        );
        note_terminal("w_dirty0000001", true);
        assert!(!take_dirty(), "a repeated verdict must stay quiet");
        clear();
    }

    #[test]
    fn repinning_the_same_id_refreshes_instead_of_duplicating() {
        let _g = lock();
        clear();
        pin("w_dupe00000001", "task");
        pin("w_dupe00000001", "task");
        assert_eq!(count(), 1, "re-pin must not stack a duplicate row");
        assert_eq!(rows(false, MAX_VISIBLE).len(), 1);
        clear();
    }

    /// The tray is sized off the WINDOW, not a constant: half the height in
    /// whole banners, floored at one so even a tiny window still shows the
    /// newest escalation instead of hiding every worker.
    #[test]
    fn visible_cap_tracks_half_the_window_height() {
        // Half the window, in whole 2-row banners.
        assert_eq!(max_visible_for(24), 6, "24 rows → 12 rows of tray");
        assert_eq!(max_visible_for(50), 12);
        assert_eq!(max_visible_for(80), 20);
        // Odd heights round DOWN — never reserve half a banner.
        assert_eq!(max_visible_for(25), 6);
        // …and the floor holds all the way down, including degenerate sizes the
        // footer solver would shed anyway.
        for rows in 0..=5u16 {
            assert_eq!(max_visible_for(rows), 1, "rows={rows}");
        }
        // Strictly better than the old fixed ceiling once the window affords it.
        assert!(max_visible_for(24) > MAX_VISIBLE);
        // Off a tty (this test binary's own case) the cap falls back to the
        // historical constant — which is what every other test here assumes.
        assert_eq!(max_visible(), MAX_VISIBLE);
    }

    #[test]
    fn overflow_past_the_visible_cap_is_summarised_not_dropped() {
        let _g = lock();
        clear();
        for i in 0..MAX_VISIBLE + 2 {
            pin(&format!("w_overflow{i:04}"), &format!("task {i}"));
        }
        assert_eq!(count(), MAX_VISIBLE + 2);
        // The block never grows past the cap…
        assert_eq!(row_count(), ROWS_PER_BANNER * MAX_VISIBLE as u16);
        let painted = rows(false, MAX_VISIBLE);
        assert_eq!(painted.len(), MAX_VISIBLE);
        // …and the hidden ones are still accounted for, on the last status row.
        let tail = &painted[MAX_VISIBLE - 1].1;
        assert!(tail.contains("+2 more escalated"), "{tail}");
        // A caller with room for fewer rows (short window) gets fewer banners,
        // with the tail re-counted against what actually painted.
        let cramped = rows(false, 1);
        assert_eq!(cramped.len(), 1);
        assert!(
            cramped[0].1.contains(&format!("+{} more", MAX_VISIBLE + 1)),
            "{:?}",
            cramped[0].1
        );
        // Zero room means zero rows — never a half-painted banner.
        assert!(rows(false, 0).is_empty());
        clear();
    }

    #[test]
    fn note_terminal_is_idempotent_and_sweep_keeps_fresh_verdict() {
        let _g = lock();
        clear();
        pin("w_1", "t");
        note_terminal("w_1", false);
        note_terminal("w_1", true); // first terminal stamp wins for the dwell clock
        // A just-finished banner survives the sweep so the verdict is readable.
        sweep();
        assert!(active());
        clear();
    }

    #[test]
    fn sweep_retires_one_banner_without_touching_its_siblings() {
        let _g = lock();
        clear();
        pin("w_live000000001", "live");
        pin("w_done000000001", "done");
        note_terminal("w_done000000001", false);
        // Force the finished banner past its dwell without sleeping DWELL_MAX.
        if let Ok(mut banners) = BANNERS.lock()
            && let Some(b) = banners.iter_mut().find(|b| b.id == "w_done000000001")
        {
            b.terminal_at = Some(Instant::now() - DWELL_MAX - Duration::from_secs(1));
        }
        sweep();
        assert_eq!(count(), 1, "only the dwelled-out banner retires");
        assert_eq!(pinned_ids(), vec!["w_live000000001".to_string()]);
        clear();
    }

    /// Age a banner's dwell clock without sleeping.
    fn age_verdict(id: &str, by: Duration) {
        if let Ok(mut banners) = BANNERS.lock()
            && let Some(b) = banners.iter_mut().find(|b| b.id == id)
        {
            b.terminal_at = Some(Instant::now() - by);
        }
    }

    #[test]
    fn a_quiet_tray_holds_a_verdict_until_the_ceiling() {
        let _g = lock();
        clear();
        pin("w_slow000000001", "done");
        note_terminal("w_slow000000001", false);
        // Well past the old 45s dwell, and past the 60s floor, but nothing is
        // asking for the room — so the verdict stays readable.
        age_verdict("w_slow000000001", DWELL_MIN * 9);
        sweep();
        assert_eq!(count(), 1, "no pressure, no retirement before the ceiling");
        // One tick past the ceiling and the unconditional half fires.
        age_verdict("w_slow000000001", DWELL_MAX + Duration::from_secs(1));
        sweep();
        assert_eq!(count(), 0, "the 15m ceiling always retires a verdict");
        clear();
    }

    /// The visible cap is WINDOW-DERIVED ([`max_visible`]), so the pressure
    /// tests fill to whatever this terminal seats rather than hard-coding three.
    /// `None` when the window is so tall the store cap ([`MAX_PINNED`]) lands
    /// first and the tray can never actually fill.
    fn fillable_cap() -> Option<usize> {
        let cap = max_visible();
        (cap <= MAX_PINNED).then_some(cap)
    }

    #[test]
    fn a_full_tray_reclaims_the_stalest_dwelled_verdict() {
        let _g = lock();
        clear();
        let Some(cap) = fillable_cap() else { return };
        // Fill to the visible cap with verdicts that have ALL served the floor,
        // staggered so the first-pinned row is also the stalest.
        for i in 0..cap {
            let id = format!("w_old{i:010}");
            pin(&id, "done");
            note_terminal(&id, false);
            age_verdict(&id, DWELL_MIN * (cap - i) as u32);
        }
        assert_eq!(count(), cap);
        pin("w_new000000001", "live");
        assert_eq!(count(), cap, "the tray stays at its visible cap");
        let ids = pinned_ids();
        assert!(!ids.contains(&"w_old0000000000".to_string()), "{ids:?}");
        assert!(ids.contains(&"w_new000000001".to_string()), "{ids:?}");
        clear();
    }

    #[test]
    fn tray_pressure_spares_fresh_verdicts_and_live_work() {
        let _g = lock();
        clear();
        let Some(cap) = fillable_cap() else { return };
        // Fill every slot but one with LIVE work, which pressure may never
        // touch...
        for i in 0..cap.saturating_sub(1) {
            pin(&format!("w_live{i:010}"), "live");
        }
        // ...and make the last slot a verdict that landed seconds ago. It is
        // INSIDE its floor: the operator has not had time to read it, so an
        // arriving row waits instead of reclaiming the slot.
        pin("w_fresh00000001", "done");
        note_terminal("w_fresh00000001", false);
        age_verdict("w_fresh00000001", DWELL_MIN / 2);
        assert_eq!(count(), cap);
        pin("w_new000000001", "live");
        assert_eq!(
            count(),
            cap + 1,
            "nothing qualified, so the tray overflows into +N more instead"
        );
        assert!(pinned_ids().contains(&"w_fresh00000001".to_string()));
        clear();
    }

    #[test]
    fn status_row_carries_a_ticking_runtime() {
        // The runtime is derived from the elapsed clock on every paint, so it
        // advances even when the worker pushes no new activity.
        let (_, a) = render(0, "w_1", "t", "coordinating", None, Some(0), false);
        let (_, b) = render(72_000, "w_1", "t", "coordinating", None, Some(0), false);
        let (_, c) = render(3_900_000, "w_1", "t", "coordinating", None, Some(0), false);
        // Right-hand padding keeps the state cell at a fixed offset no matter
        // how wide the runtime grows.
        assert!(a.contains("0s       coordi…"), "{a}");
        assert!(b.contains("1m12s    coordi…"), "{b}");
        assert!(c.contains("1h05m    coordi…"), "{c}");
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
            Some(0),
            false,
        );
        assert!(!bottom.contains('\u{1b}'), "{bottom:?}");
        assert!(
            bottom.contains("running · 🔧 read_file src/repl.rs"),
            "{bottom}"
        );
    }

    #[test]
    fn note_terminal_only_freezes_the_named_worker() {
        let _g = lock();
        clear();
        pin("w_pinned", "t");
        note_terminal("w_other", true);
        let (top, _) = rows(false, MAX_VISIBLE).remove(0);
        assert!(
            !top.starts_with("⚠️"),
            "another worker froze the banner: {top}"
        );
        note_terminal("w_pinned", false);
        let (top, _) = rows(false, MAX_VISIBLE).remove(0);
        assert!(top.starts_with("✅"), "{top}");
        clear();
    }

    #[test]
    fn mutators_are_noops_without_a_banner() {
        let _g = lock();
        clear();
        set_status("w_ghost", "nothing to attach to");
        note_terminal("w_ghost", true);
        set_beat("w_ghost", Some(123));
        sweep();
        assert!(!active());
        assert_eq!(count(), 0);
        assert_eq!(row_count(), 0);
        assert!(pinned_ids().is_empty());
    }

    #[test]
    fn status_row_carries_the_liveness_heart() {
        // The heart is the only liveness indicator. Healthy = bare glyph;
        // a missed beat appends the age.
        let (_, fresh) = render(0, "w_1", "t", "coordinating", None, Some(3), false);
        assert!(fresh.starts_with("   ♥ "), "{fresh}");
        assert!(
            !fresh.contains("♥ 3s"),
            "healthy heart must stay quiet: {fresh}"
        );

        let (_, late) = render(0, "w_1", "t", "coordinating", None, Some(50), false);
        assert!(
            late.contains("♥ 50s"),
            "one missed beat shows its age: {late}"
        );

        let (_, dead) = render(0, "w_1", "t", "coordinating", None, Some(600), false);
        assert!(dead.contains("♥ 10m"), "{dead}");

        // No beat on record yet → hollow heart, no liveness claim.
        let (_, unknown) = render(0, "w_1", "t", "coordinating", None, None, false);
        assert!(unknown.starts_with("   ♡ "), "{unknown}");

        // The runtime and status still follow the heart, in that order.
        assert!(fresh.contains("0s       coordi…"), "{fresh}");
    }

    #[test]
    fn heart_is_colored_independently_of_the_dim_row() {
        use crate::style::Color;
        // The row is dim, but the heart carries its own colour — otherwise the
        // traffic light is invisible. Each tier's SGR code must appear.
        let (_, green) = render(0, "w_1", "t", "s", None, Some(1), true);
        assert!(green.contains(Color::Green.code()), "{green:?}");
        let (_, yellow) = render(0, "w_1", "t", "s", None, Some(50), true);
        assert!(yellow.contains(Color::Yellow.code()), "{yellow:?}");
        let (_, red) = render(0, "w_1", "t", "s", None, Some(300), true);
        assert!(red.contains(Color::Red.code()), "{red:?}");
    }

    #[test]
    fn terminal_banner_drops_the_heart() {
        // ✅/⚠️ already settles liveness; a heart next to a finished verdict
        // would be a second indicator echoing the first.
        let (_, done) = render(0, "w_1", "t", "done", Some(false), Some(900), false);
        assert!(!done.contains('♥'), "{done}");
        assert!(!done.contains('♡'), "{done}");
        // `↳` rides the SAME fixed beat cell the heart does, so a finished row's
        // runtime column lands at the same screen offset as a live one's.
        assert!(
            done.starts_with(&format!("   ↳{} 0s", " ".repeat(BEAT_COL - 1))),
            "{done}"
        );
    }

    #[test]
    fn set_beat_drives_that_banners_heart() {
        let _g = lock();
        clear();
        pin("w_beat", "t");
        let status_row = || rows(false, MAX_VISIBLE)[0].1.clone();
        // A freshly pinned banner has no beat on record yet.
        let b = status_row();
        assert!(b.contains('♡'), "{b}");

        // A current beat goes green-and-quiet…
        set_beat("w_beat", Some(now_unix_secs()));
        let b = status_row();
        assert!(b.contains('♥'), "{b}");

        // …and an old beat ages into an annotated heart on the NEXT paint, with
        // no further polling — the banner stores the absolute timestamp.
        set_beat("w_beat", Some(now_unix_secs() - 120));
        let b = status_row();
        assert!(b.contains("♥ 2m"), "{b}");

        // Clearing the beat returns to "no claim".
        set_beat("w_beat", None);
        let b = status_row();
        assert!(b.contains('♡'), "{b}");

        // A beat for an unpinned id is a no-op, not a panic or a stray row.
        set_beat("w_nobody", Some(now_unix_secs()));
        assert_eq!(count(), 1);
        clear();
    }

    #[test]
    fn each_banner_shows_its_own_workers_heart() {
        // Two live escalations: a healthy worker and a silent one. Each status
        // row must carry ITS worker's liveness — the beat is id-scoped like
        // every other mutator, so one worker's heart can't paint another's row.
        let _g = lock();
        clear();
        pin("w_alive", "a");
        pin("w_silent", "b");
        set_beat("w_alive", Some(now_unix_secs()));
        set_beat("w_silent", Some(now_unix_secs() - 600));
        let painted = rows(false, MAX_VISIBLE);
        // Newest first: w_silent was pinned last.
        assert!(painted[0].1.contains("♥ 10m"), "{:?}", painted[0]);
        assert!(painted[1].1.contains('♥'), "{:?}", painted[1]);
        assert!(!painted[1].1.contains("♥ 10m"), "{:?}", painted[1]);

        // Re-pinning a tracked id keeps its known beat instead of flashing ♡.
        pin("w_alive", "a retry");
        let painted = rows(false, MAX_VISIBLE);
        let alive = painted.iter().find(|r| r.0.contains("a retry")).unwrap();
        assert!(alive.1.contains('♥'), "{alive:?}");
        assert!(!alive.1.contains('♡'), "{alive:?}");
        clear();
    }

    /// A fan-out must read as the PARENT's work: each sub-worker paints directly
    /// beneath the coordinator that spawned it, indented behind a bent arrow,
    /// instead of surfacing as another unexplained top-level escalation at the
    /// top of the tray. Siblings keep the tray's newest-first order.
    #[test]
    fn sub_workers_paint_indented_under_their_parent() {
        let _g = lock();
        clear();
        pin("w_parent", "the fan-out");
        pin_sub("w_parent", "w_kid_a", "first child", 0);
        pin_sub("w_parent", "w_kid_b", "second child", 0);

        let painted = rows(false, MAX_VISIBLE);
        let heads: Vec<&str> = painted.iter().map(|(h, _)| h.as_str()).collect();
        assert_eq!(heads.len(), 3, "{heads:#?}");
        // The parent first, then its children newest-first directly under it —
        // a sub-worker never jumps above the coordinator that spawned it, even
        // though it was pinned later than that parent.
        assert!(heads[0].contains("w_parent"), "{heads:#?}");
        assert!(heads[1].contains("second child"), "{heads:#?}");
        assert!(heads[2].contains("first child"), "{heads:#?}");
        // A nested row is identified by its PLACE, not by a `w_…` the operator
        // can't do anything with: the child ids never reach the tray.
        assert!(!heads[1].contains("w_kid_b"), "{heads:#?}");
        assert!(!heads[2].contains("w_kid_a"), "{heads:#?}");

        // Roots keep the flat tray's shape byte for byte; children lead with the
        // indent + arrow, and their status row lines up under their own glyph.
        assert!(!heads[0].starts_with(' '), "{:?}", heads[0]);
        let want = format!("{SUB_INDENT}{SUB_ARROW} ");
        assert!(heads[1].starts_with(&want), "{:?}", heads[1]);
        assert!(heads[2].starts_with(&want), "{:?}", heads[2]);
        assert!(
            painted[1].1.starts_with(&format!("{SUB_INDENT}  ")),
            "{:?}",
            painted[1].1
        );
        clear();
    }

    /// Sub-workers fan out too, so the walk is transitive — but the indent is
    /// CAPPED: past [`MAX_SUB_DEPTH`] a row would be mostly margin, so deeper
    /// descendants still paint (and still count) at the deepest indent rather
    /// than marching off the right edge.
    #[test]
    fn nesting_depth_is_capped_and_orphans_fall_back_to_roots() {
        let _g = lock();
        clear();
        pin("w_g0", "root row");
        pin_sub("w_g0", "w_g1", "kid row", 0);
        pin_sub("w_g1", "w_g2", "deep leaf", 0);

        let painted = rows(false, MAX_VISIBLE);
        // Nested rows carry no id, so a row is located by its task text.
        let depth_of = |needle: &str| {
            let h = painted
                .iter()
                .find(|(h, _)| h.contains(needle))
                .map(|(h, _)| h.clone())
                .unwrap_or_else(|| panic!("{needle} missing from {painted:#?}"));
            (h.len() - h.trim_start().len()) / SUB_INDENT.len()
        };
        assert_eq!(depth_of("root row"), 0);
        assert_eq!(depth_of("kid row"), 1);
        assert_eq!(depth_of("deep leaf"), MAX_SUB_DEPTH);

        // Past the cap the indent STOPS growing — the row still paints (and
        // still counts), it just stops marching toward the right edge.
        let pair = ("row".to_string(), "status".to_string());
        let (deep_top, deep_bottom) = indent_pair(pair.clone(), MAX_SUB_DEPTH + 7, false);
        let (cap_top, cap_bottom) = indent_pair(pair, MAX_SUB_DEPTH, false);
        assert_eq!(deep_top, cap_top);
        assert_eq!(deep_bottom, cap_bottom);

        // A child whose parent isn't tracked (evicted, or already retired) is
        // still LIVE work — it renders as a root rather than vanishing.
        clear();
        pin_sub("w_never_pinned", "w_orphan", "orphaned child", 0);
        let painted = rows(false, MAX_VISIBLE);
        assert_eq!(painted.len(), 1, "{painted:#?}");
        assert!(!painted[0].0.starts_with(' '), "{:?}", painted[0].0);
        assert!(painted[0].0.contains("w_orphan"), "{:?}", painted[0].0);
        clear();
    }

    /// `pin_sub` is INSERT-ONLY. The REPL discovers sub-workers by polling the
    /// durable run table, so it re-pins the same child every tick; refreshing
    /// in place would restart the child's clock and wipe the verdict, leaving a
    /// finished sub-worker that can never age out of the tray.
    #[test]
    fn repinning_a_sub_worker_is_a_discovery_not_a_relaunch() {
        let _g = lock();
        clear();
        pin("w_p", "parent");
        pin_sub("w_p", "w_c", "child task", 90_000);
        set_status("w_c", "turn 3 · building");
        note_terminal("w_c", false);

        // Same child, polled again: no duplicate row, verdict and status intact.
        pin_sub("w_p", "w_c", "child task", 0);
        assert_eq!(count(), 2, "no duplicate row for the same child");
        let painted = rows(false, MAX_VISIBLE);
        let child = painted
            .iter()
            .find(|(h, _)| h.contains("child task"))
            .expect("child row");
        assert!(child.1.contains("turn 3 · building"), "{child:?}");
        // The backdated clock survives too — a child discovered mid-run keeps
        // its real elapsed time instead of resetting to 0s.
        assert!(child.1.contains("1m"), "{child:?}");
        clear();
    }
}
