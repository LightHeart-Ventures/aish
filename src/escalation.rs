//! The PINNED ESCALATION BANNER — two footer rows PER live background
//! escalation that keep it on screen for as long as its coordinator is working.
//!
//! THE PROBLEM. When the model escalates work to a background coordinator
//! (`run_in_background`), the launch notice is printed ONCE into the scrolling
//! body ("🚀 escalated to a background coordinator …") and then immediately
//! scrolls away under the next command's output. Thirty seconds later the
//! operator has no on-screen evidence that anything is running — the only
//! surviving signal is the `⟳N` prompt pulse and the 🤖 statusline badge, neither
//! of which says WHAT was escalated or HOW it's doing. The operator has to run
//! `:workers` to answer "is that thing still going?".
//!
//! THE FIX. Pin the escalation to the footer instead of the body. While
//! coordinators are live, [`crate::terminal::FooterLayout`] grows the pinned
//! footer by [`row_count`] rows and `footer_seq` paints:
//!
//! ```text
//!   🚀 escalated → w_a7k3m2 · build and open pr  <- stable identity, never flickers
//!      ⠹ ♥ 1m12s · 🔧 read_file …               <- spinner + heart + worker status
//!   🚀 escalated → w_b2c9f1 · audit the deps     <- a SECOND live escalation
//!      ⠧ ♥ 18s · 🔧 grep_files                  <- …its own spinner, heart, status
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
//! it. At most [`MAX_VISIBLE`] banners paint — beyond that the oldest visible
//! status row carries a `+N more` tail, so the footer can never eat the window;
//! [`crate::terminal::FooterLayout`] additionally drops whole banners (newest
//! kept) on a window too short to hold them all.
//!
//! ANIMATION — IDENTITY AND MOTION ARE SEPARATE CELLS. The head row's glyph is
//! the banner's IDENTITY and it does not move: a fixed [`LIVE_GLYPH`] (`🚀`,
//! the same rocket the body's launch notice prints, so the operator can key the
//! pinned banner to the line that announced it) while the coordinator works,
//! frozen to ✅/⚠️ once it reaches a terminal state. The MOTION lives one row
//! down, in the status row's prefix cell, as a [`SPIN_FRAMES`] braille frame on
//! a [`FRAME_MS`] cadence.
//!
//! That split is deliberate, and it is a FIX. The head glyph used to animate
//! through four unrelated pictographs (`🚀 🛸 🌠 ✨`), which the eye reads not as
//! one thing moving but as the thing CHANGING INTO a different thing four times
//! a second — identity flicker, not animation — while strobing four dominant
//! hues (orange → grey → blue → yellow) through an otherwise dim/cyan footer,
//! out-shouting the heart beside it, which is the glyph that actually carries
//! information. It also leaked width: `✨` (U+2728) is `East_Asian_Width=Wide`
//! in the Unicode table but is routinely drawn single-column by fallback
//! monospace fonts, so the text after it jittered on real terminals even though
//! the width guard passed. Braille (U+28xx) is unambiguously one column
//! everywhere, carries no colour of its own, and — crucially — ADJACENT FRAMES
//! DIFFER BY ONE DOT, which is what makes a cycle read as rotation instead of
//! replacement. It is also the frame set every other spinner in aish already
//! uses (`engine`'s thinking + tool spinners, `update`, `worker`); the escalation
//! banner was the lone site with a bespoke emoji slideshow.
//!
//! Putting the spinner in the status row also puts it directly beside the heart,
//! so the two liveness signals the LIVENESS note below describes — "the UI is
//! repainting" and "the WORKER is alive" — now sit in one glance instead of on
//! separate rows. The cadence is unchanged, so the footer heartbeat's wake rate
//! and repaint cost are exactly what they were.
//!
//! The frame is a PURE function of elapsed time ([`frame_at`]), so every repaint
//! path (idle heartbeat, mid-turn draw, resize) lands on the same frame without
//! any shared animation cursor. Each banner animates off its OWN pin clock, so
//! two escalations started seconds apart visibly tick independently. A finished
//! banner drops the spinner for a static `↳` — motion means "still working".
//!
//! LIVENESS. The animation only proves the SHELL is still repainting — it keeps
//! cycling just as happily when the coordinator behind it is wedged, rate-limited
//! or dead, which makes a moving glyph the most confident lie the footer can
//! tell. So each status row also carries a traffic-light HEART fed by that
//! coordinator's own DURABLE heartbeat (`coordinator_runs.heartbeat_at`, written
//! every 30s): green `♥` while the beat is current, yellow after ONE missed beat,
//! red after two or more, and a dim hollow `♡` when no beat is on record yet
//! ([`crate::style::heartbeat_heart`]). Two independent signals in one block:
//! motion = the UI is live, heart = the WORKER is live. The REPL polls the store
//! on a throttle and hands each banner its absolute timestamp via [`set_beat`];
//! the age (and therefore the colour) is derived on every paint, so a worker
//! that stops beating goes yellow then red on its own without further polling.
//! Like every other mutator, the beat is ID-SCOPED: each banner shows its OWN
//! worker's liveness.
//!
//! LIFECYCLE. [`pin`] on escalation, [`set_status`] on every footer paint (the
//! REPL owns the worker list, so it composes each status row), [`note_terminal`]
//! when a coordinator finishes, and each banner self-retires [`DWELL`] after
//! that so the final verdict is readable but the footer shrinks back on its own.
//! Every mutator is ID-SCOPED: with several banners live, a finishing worker
//! must only ever freeze its OWN row.

use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Footer rows ONE banner occupies: the escalation message and the worker-status
/// row directly below it. The pinned block is this times the number of visible
/// banners — see [`row_count`].
pub const ROWS_PER_BANNER: u16 = 2;

/// How many escalations paint at once. Three banners = six footer rows, which
/// still leaves a usable body on a standard 24-row window; past that the extras
/// are summarised as a `+N more` tail rather than swallowing the terminal.
pub const MAX_VISIBLE: usize = 3;

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

/// Prefix glyph for a FINISHED banner's status row — the spinner's resting
/// state. Motion is reserved for live work, so a terminal banner shows this
/// static elbow instead. One column, same as a braille frame, so the row's width
/// accounting is identical either way.
pub const IDLE_PREFIX: &str = "↳";

/// Motion frames for the status row's prefix cell — the canonical aish spinner
/// set, shared verbatim with `engine`'s thinking + tool spinners, `update`, and
/// `worker`. Braille (U+28xx) is unambiguously ONE column in every terminal
/// (unlike the pictographs this replaced), carries no colour of its own so it
/// can't out-shout the heart beside it, and differs by a single dot between
/// adjacent frames — which is what makes the cycle read as one glyph ROTATING
/// rather than four glyphs taking turns. Pinned by `frames_are_uniform_width`.
pub const SPIN_FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// Milliseconds per animation frame. ~4.5 fps, UNCHANGED from the emoji cycle it
/// replaced: this also sets the footer heartbeat's wake rate (see
/// `terminal::spawn_footer_heartbeat`, which sleeps `FRAME_MS.min(500)`), so
/// holding it fixed keeps the idle repaint cost exactly where it was while the
/// one-dot-per-frame braille delta does the perceptual work a faster cadence
/// would otherwise have had to buy.
pub const FRAME_MS: u64 = 220;

/// How long a FINISHED escalation stays pinned before its banner retires and the
/// footer shrinks back. Long enough to read the verdict after stepping away,
/// short enough that the footer doesn't stay fat forever.
pub const DWELL: Duration = Duration::from_secs(45);

/// Max visible width of the task hint on the escalation row.
const TASK_HINT_MAX: usize = 56;

/// Max visible width of the composed status row (the terminal clip in
/// `footer_seq` is the hard backstop; this keeps the row from crowding out the
/// `↳` prefix on a narrow window).
pub const STATUS_MAX: usize = 110;

/// One pinned escalation. Many can be live at once — see the STACKING note in
/// the module docs — so every field is per-banner and every mutator is keyed by
/// [`Banner::id`].
struct Banner {
    /// Coordinator run id (`w_…`) — rendered short, and the key the REPL uses to
    /// find the live worker when it refreshes the status row.
    id: String,
    /// The escalated task, rendered as a compact one-line hint.
    task: String,
    /// When the banner was pinned — drives the animation frame.
    pinned_at: Instant,
    /// When the coordinator reached a terminal state (`done`/`failed`), if it
    /// has. Freezes the animation and starts the [`DWELL`] retirement clock.
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

/// Pin an escalation banner for coordinator `id` running `task`.
///
/// Additive, not replacing: an escalation launched while others are still live
/// gets its OWN two rows (the single-slot store this replaced silently hid every
/// escalation but the newest). Re-pinning an id that is already tracked
/// REFRESHES that banner in place — same task hint, clocks reset, verdict
/// cleared — so a retry can't stack a duplicate row for one coordinator.
/// A pinned banner steals two body rows from the scrolling region. The STORE
/// mutation is [`push_banner`]; this wrapper measures the growth around it and
/// hands it to [`crate::terminal::absorb_banner_growth`], which scrolls the body
/// out of the rows the footer is about to claim and walks the cursor back up
/// with it. Without that, the cursor is left sitting INSIDE the new tray rows
/// and the next body write — the animated `thinking…` row above all — paints
/// over the banner, so "thinking" bleeds into the tray. Measured outside the
/// `BANNERS` lock (both `row_count` calls take it themselves) so the terminal
/// write never happens while the stack is held.
pub fn pin(id: &str, task: &str) {
    let before = row_count();
    push_banner(id, task);
    crate::terminal::absorb_banner_growth(before, row_count());
}

fn push_banner(id: &str, task: &str) {
    let Ok(mut banners) = BANNERS.lock() else {
        return;
    };
    let fresh = Banner {
        id: id.to_string(),
        task: task.to_string(),
        pinned_at: Instant::now(),
        terminal_at: None,
        failed: false,
        status: "queued — waiting for the coordinator to start".into(),
        beat: None,
    };
    if let Some(slot) = banners.iter_mut().find(|b| b.id == id) {
        // Same coordinator: its durable heartbeat is still valid evidence, so
        // keep it rather than flashing the hollow "no claim" heart on a retry.
        let beat = slot.beat;
        *slot = Banner { beat, ..fresh };
        return;
    }
    banners.push(fresh);
    // Bound the store. Evict the oldest FINISHED banner first so a burst of
    // fan-out never retires a coordinator that is still working.
    while banners.len() > MAX_PINNED {
        let victim = banners
            .iter()
            .position(|b| b.terminal_at.is_some())
            .unwrap_or(0);
        banners.remove(victim);
    }
}

/// Remove coordinator `id`'s banner from the tray RIGHT NOW, reporting whether
/// one was actually pinned for it.
///
/// This is the DISMISSAL path, and it exists because [`sweep`] can't serve it:
/// sweep only retires a banner that already reached a terminal state AND then
/// outlived [`DWELL`]. A `:close`d coordinator that is still RUNNING never
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
    let Ok(mut banners) = BANNERS.lock() else {
        return false;
    };
    let before = banners.len();
    banners.retain(|b| b.id != id);
    banners.len() != before
}

/// Retire EVERY banner (the footer shrinks back on the next paint).
#[allow(dead_code)] // completes the BANNERS push/remove/clear API; no caller wired yet.
pub fn clear() {
    if let Ok(mut banners) = BANNERS.lock() {
        banners.clear();
    }
}

/// True while ANY banner is pinned — the gate
/// [`crate::terminal::FooterLayout`] consults to decide the footer height.
pub fn active() -> bool {
    BANNERS.lock().map(|b| !b.is_empty()).unwrap_or(false)
}

/// How many banners are tracked right now (including ones summarised past
/// [`MAX_VISIBLE`]).
pub fn count() -> usize {
    BANNERS.lock().map(|b| b.len()).unwrap_or(0)
}

/// Footer rows the pinned block WANTS: two per visible banner, capped at
/// [`MAX_VISIBLE`] banners. Zero when nothing is pinned.
///
/// [`crate::terminal::FooterLayout`] may hand back fewer on a short window —
/// it drops whole banners rather than splitting one across the rule.
pub fn row_count() -> u16 {
    (count().min(MAX_VISIBLE) as u16) * ROWS_PER_BANNER
}

/// True while at least one pinned banner is still ANIMATING (pinned and not yet
/// terminal). The footer heartbeat consults this to bypass its idle gate and
/// repaint on the [`FRAME_MS`] cadence — motion is the "still working" signal,
/// so it must keep ticking while the shell sits idle at the prompt.
pub fn animating() -> bool {
    BANNERS
        .lock()
        .map(|banners| banners.iter().any(|b| b.terminal_at.is_none()))
        .unwrap_or(false)
}

/// Every pinned coordinator's run id, NEWEST FIRST. The REPL walks these to
/// locate each live worker whose status it then feeds back via [`set_status`].
pub fn pinned_ids() -> Vec<String> {
    BANNERS
        .lock()
        .map(|banners| banners.iter().rev().map(|b| b.id.clone()).collect())
        .unwrap_or_default()
}

/// Replace coordinator `id`'s status row text (already composed by the caller —
/// the REPL owns the worker list). No-op when that id isn't pinned, so a
/// finished-and-retired worker can't resurrect a row or write into another
/// banner's.
pub fn set_status(id: &str, status: &str) {
    if let Ok(mut banners) = BANNERS.lock()
        && let Some(b) = banners.iter_mut().find(|b| b.id == id)
    {
        b.status = status.to_string();
    }
}

/// Record coordinator `id`'s last durable heartbeat (epoch seconds from
/// `coordinator_runs.heartbeat_at`), which drives the liveness heart on THAT
/// banner's status row. `None` clears it back to "no beat on record". No-op when
/// the id isn't pinned. The REPL polls this on a throttle (see
/// `repl::refresh_escalation_beats`) because the beat only moves every 30s.
pub fn set_beat(id: &str, beat: Option<i64>) {
    if let Ok(mut banners) = BANNERS.lock()
        && let Some(b) = banners.iter_mut().find(|b| b.id == id)
    {
        b.beat = beat;
    }
}

/// Record that coordinator `id` reached a terminal state: freezes THAT banner's
/// animation glyph and starts its [`DWELL`] retirement clock. Idempotent — the
/// first call wins, so the dwell measures from the real finish — and strictly
/// id-scoped, so a worker finishing can never stamp another banner's verdict.
pub fn note_terminal(id: &str, failed: bool) {
    if let Ok(mut banners) = BANNERS.lock()
        && let Some(b) = banners.iter_mut().find(|b| b.id == id)
    {
        b.failed = failed;
        b.terminal_at.get_or_insert_with(Instant::now);
    }
}

/// Retire each finished banner once [`DWELL`] has elapsed since its terminal
/// mark. Called on each footer refresh; cheap no-op while workers are live.
/// Per-banner, so one finished escalation retiring leaves its live siblings
/// pinned.
pub fn sweep() {
    if let Ok(mut banners) = BANNERS.lock() {
        banners.retain(|b| b.terminal_at.is_none_or(|t| t.elapsed() < DWELL));
    }
}

/// The status row's spinner frame for a given elapsed time. Pure, so every
/// repaint path (heartbeat, mid-turn draw, resize) derives the SAME frame from
/// the clock instead of sharing a mutable cursor.
pub fn frame_at(elapsed_ms: u64) -> &'static str {
    SPIN_FRAMES[(elapsed_ms / FRAME_MS) as usize % SPIN_FRAMES.len()]
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
    let secs = ms / 1000;
    match secs {
        s if s < 60 => format!("{s}s"),
        s if s < 3600 => format!("{}m{:02}s", s / 60, s % 60),
        s => format!("{}h{:02}m", s / 3600, (s % 3600) / 60),
    }
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

/// Build ONE banner's two rows. Pure: the caller supplies the elapsed clock, the
/// identity, the status text, and the terminal verdict, so the rendering is
/// unit-testable without touching the global stack or a TTY.
///
/// Returns `(escalation_row, status_row)`. The escalation row leads with the
/// STATIC identity glyph ([`LIVE_GLYPH`] live, ✅/⚠️ terminal); the status row is
/// indented under it and leads with the ANIMATED [`SPIN_FRAMES`] cell while the
/// coordinator works, falling back to a static [`IDLE_PREFIX`] elbow once it
/// finishes. Both prefixes are one column, so the row width is identical either
/// way — see the ANIMATION note in the module docs for why identity and motion
/// are separate cells.
///
/// `beat_age_secs` is the age of the coordinator's last DURABLE heartbeat, which
/// renders as the traffic-light heart (green beating / yellow one missed / red
/// two or more — [`crate::style::heartbeat_heart`]) in a fixed position right
/// after the prefix cell, so the spinner and the heart sit side by side. The
/// spinner only proves the SHELL is repainting; the
/// heart is the independent evidence that the WORKER is alive. A terminal banner
/// renders no heart — the ✅/⚠️ verdict already settles liveness, and a second
/// indicator there would just echo it.
pub fn render(
    elapsed_ms: u64,
    id: &str,
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
    let short = crate::batch::short_id(id);
    let hint = one_line(task, TASK_HINT_MAX);
    let head = if hint.is_empty() {
        format!("{glyph} escalated → {short}")
    } else {
        format!("{glyph} escalated → {short} · {hint}")
    };
    let top = paint_with(&head, Color::Cyan, color_on);
    // The heart is painted on its own (green/yellow/red) and spliced between two
    // dim segments, so the liveness colour survives while the rest of the row
    // stays recessive.
    let heart = match terminal {
        None => format!(
            "{} ",
            crate::style::heartbeat_heart(beat_age_secs, color_on)
        ),
        Some(_) => String::new(),
    };
    // MOTION: a live banner spins its prefix cell; a finished one rests on the
    // static elbow. Both are one column, so `ESCALATION_STATUS_CHROME` holds.
    let prefix = match terminal {
        None => frame_at(elapsed_ms),
        Some(_) => IDLE_PREFIX,
    };
    let bottom = format!(
        "{}{heart}{}",
        paint_with(&format!("   {prefix} "), Color::Dim, color_on),
        paint_with(
            &format!(
                "{} · {}",
                fmt_elapsed(elapsed_ms),
                one_line(status, STATUS_MAX)
            ),
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
/// [`MAX_VISIBLE`]. Any escalation that doesn't fit is not silently dropped: the
/// last painted status row carries a `· +N more escalated` tail, so the operator
/// always sees that more work is in flight.
pub fn rows(color_on: bool, max_banners: usize) -> Vec<(String, String)> {
    let Ok(banners) = BANNERS.lock() else {
        return Vec::new();
    };
    let visible = max_banners.min(MAX_VISIBLE).min(banners.len());
    if visible == 0 {
        return Vec::new();
    }
    let hidden = banners.len() - visible;
    // Newest first: the top banner is the escalation the operator just launched,
    // and it matches `cycle_worker`'s newest-first Shift-Tab ordering.
    banners
        .iter()
        .rev()
        .take(visible)
        .enumerate()
        .map(|(i, b)| {
            let terminal = b.terminal_at.map(|_| b.failed);
            // Age the absolute beat here, on the paint path, so each heart keeps
            // ticking (and can go yellow → red) between the REPL's throttled polls.
            let beat_age = b.beat.map(|t| (now_unix_secs() - t).max(0));
            // The overflow tail rides the LAST painted status row (the oldest
            // visible banner) rather than claiming a row of its own, so the
            // block height stays an exact multiple of ROWS_PER_BANNER.
            let status = if hidden > 0 && i + 1 == visible {
                format!("{} · +{hidden} more escalated", b.status)
            } else {
                b.status.clone()
            };
            render(
                b.pinned_at.elapsed().as_millis() as u64,
                &b.id,
                &b.task,
                &status,
                terminal,
                beat_age,
                color_on,
            )
        })
        .collect()
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

    #[test]
    fn frames_are_uniform_width() {
        // Every motion frame must occupy the SAME number of columns, or the text
        // after it jitters left/right on each tick. Braille is one column in
        // every terminal — the pictographs this replaced were only *nominally*
        // 2-wide (`✨` is East-Asian Wide in the table but routinely drawn
        // single-column by fallback fonts), which is why the old guard passed
        // while the real footer still shifted.
        for f in SPIN_FRAMES {
            assert_eq!(f.width(), 1, "spin frame {f:?} is not 1 column wide");
        }
        // The resting prefix shares the spinner's cell, so swapping between them
        // on a terminal transition must not move the text after it.
        assert_eq!(IDLE_PREFIX.width(), 1);
        // The head row's identity glyph is what ESCALATION_HEAD_CHROME budgets
        // 2 columns for.
        assert_eq!(LIVE_GLYPH.width(), 2);
    }

    #[test]
    fn frame_advances_on_cadence_and_wraps() {
        assert_eq!(frame_at(0), SPIN_FRAMES[0]);
        assert_eq!(frame_at(FRAME_MS - 1), SPIN_FRAMES[0]);
        assert_eq!(frame_at(FRAME_MS), SPIN_FRAMES[1]);
        assert_eq!(frame_at(FRAME_MS * 3), SPIN_FRAMES[3]);
        // Wraps cleanly — a long-running escalation keeps animating forever.
        let n = SPIN_FRAMES.len() as u64;
        assert_eq!(frame_at(FRAME_MS * n), SPIN_FRAMES[0]);
        assert_eq!(frame_at(FRAME_MS * (n + 1)), SPIN_FRAMES[1]);
    }

    #[test]
    fn head_glyph_is_stable_and_motion_lives_in_the_status_row() {
        // THE regression this split fixes: the head glyph used to cycle four
        // unrelated pictographs, so the banner's identity changed 4×/sec. It must
        // now be the SAME glyph at every point in the animation cycle, with the
        // motion carried one row down.
        let at = |ms| {
            render(
                ms,
                "w_abcdef123456",
                "build it",
                "coordinating",
                None,
                Some(0),
                false,
            )
        };
        let (a_top, a_bot) = at(0);
        let (b_top, b_bot) = at(FRAME_MS);
        let (c_top, _) = at(FRAME_MS * 7);

        assert!(a_top.starts_with(LIVE_GLYPH), "{a_top}");
        assert!(b_top.starts_with(LIVE_GLYPH), "{b_top}");
        assert!(c_top.starts_with(LIVE_GLYPH), "{c_top}");
        // No stray pictograph from the old slideshow survives on the head row.
        for stale in ["🛸", "🌠", "✨"] {
            assert!(!a_top.contains(stale), "stale frame {stale} in {a_top}");
        }

        // …and the status row DOES move, on the same cadence.
        assert!(
            a_bot.starts_with(&format!("   {} ", SPIN_FRAMES[0])),
            "{a_bot}"
        );
        assert!(
            b_bot.starts_with(&format!("   {} ", SPIN_FRAMES[1])),
            "{b_bot}"
        );
        assert_ne!(a_bot, b_bot, "the status row must animate");
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
        // And no spin frame leaks into a frozen row at any point in the cycle.
        for f in SPIN_FRAMES {
            assert!(!ok_bot.contains(f), "{ok_bot}");
        }
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
        // The status row is indented under the message, led by the spinner cell.
        assert!(
            bottom.starts_with(&format!("   {} ", frame_at(0))),
            "{bottom}"
        );
        assert!(bottom.contains("coordinating · 1m12s"), "{bottom}");
    }

    #[test]
    fn long_task_and_status_are_truncated() {
        let task = "x".repeat(400);
        let status = "y".repeat(400);
        let (top, bottom) = render(0, "w_a", &task, &status, None, Some(0), false);
        assert!(top.contains('…'), "{top}");
        assert!(top.chars().count() < 120, "escalation row too wide: {top}");
        assert!(bottom.contains('…'), "{bottom}");
        assert!(bottom.chars().count() < STATUS_MAX + 16, "{bottom}");
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
        assert!(top.contains("escalated"), "{top}");
        // Fresh banners state the pre-start truth rather than inventing progress.
        assert!(bottom.contains("queued"), "{bottom}");

        set_status("w_deadbeefcafe", "coordinating · 12s · 🔧 read_file");
        let (_, bottom) = rows(false, MAX_VISIBLE).remove(0);
        assert!(bottom.contains("coordinating · 12s"), "{bottom}");

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
    fn repinning_the_same_id_refreshes_instead_of_duplicating() {
        let _g = lock();
        clear();
        pin("w_dupe00000001", "task");
        pin("w_dupe00000001", "task");
        assert_eq!(count(), 1, "re-pin must not stack a duplicate row");
        assert_eq!(rows(false, MAX_VISIBLE).len(), 1);
        clear();
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
        // Force the finished banner past its dwell without sleeping 45s.
        if let Ok(mut banners) = BANNERS.lock()
            && let Some(b) = banners.iter_mut().find(|b| b.id == "w_done000000001")
        {
            b.terminal_at = Some(Instant::now() - DWELL - Duration::from_secs(1));
        }
        sweep();
        assert_eq!(count(), 1, "only the dwelled-out banner retires");
        assert_eq!(pinned_ids(), vec!["w_live000000001".to_string()]);
        clear();
    }

    #[test]
    fn status_row_carries_a_ticking_runtime() {
        // The runtime is derived from the elapsed clock on every paint, so it
        // advances even when the worker pushes no new activity.
        let (_, a) = render(0, "w_1", "t", "coordinating", None, Some(0), false);
        let (_, b) = render(72_000, "w_1", "t", "coordinating", None, Some(0), false);
        let (_, c) = render(3_900_000, "w_1", "t", "coordinating", None, Some(0), false);
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
        // The heart sits in a FIXED position right after the spinner cell, so the
        // two liveness signals (UI repainting / worker beating) read as one
        // glance. Healthy = bare glyph; a missed beat appends the age.
        let spin = frame_at(0);
        let (_, fresh) = render(0, "w_1", "t", "coordinating", None, Some(3), false);
        assert!(fresh.starts_with(&format!("   {spin} ♥ ")), "{fresh}");
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
        assert!(unknown.starts_with(&format!("   {spin} ♡ ")), "{unknown}");

        // The runtime and status still follow the heart, in that order.
        assert!(fresh.contains("0s · coordinating"), "{fresh}");
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
        assert!(done.starts_with("   ↳ 0s · done"), "{done}");
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
}
