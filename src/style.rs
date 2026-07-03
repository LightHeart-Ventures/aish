//! Terminal color + emoji styling for status output.
//!
//! One place that decides (a) WHETHER to emit ANSI right now and (b) WHICH
//! color/emoji a given status deserves. Everything that paints a status table —
//! today the `:workers` listing — routes through here so the scheme stays
//! consistent and a single `--no-color` switch (or a piped stdout, or the
//! `NO_COLOR` convention) turns the whole thing off.
//!
//! The styling helpers come in two shapes: a runtime form (`styled_status`,
//! `paint`, …) that queries [`colors_enabled`], and a pure `_with(…, color: bool)`
//! form that takes the decision explicitly so the color/emoji mapping is
//! unit-testable without a TTY.

use std::sync::atomic::{AtomicBool, Ordering};

/// The SGR reset that closes every painted span.
pub const RESET: &str = "\x1b[0m";

/// Process-wide override set by the `--no-color` CLI flag. When true, every
/// helper emits plain text regardless of TTY / `NO_COLOR`.
static FORCE_NO_COLOR: AtomicBool = AtomicBool::new(false);

/// Honor `--no-color`: disable all ANSI styling for the rest of the process.
pub fn set_no_color(on: bool) {
    FORCE_NO_COLOR.store(on, Ordering::Relaxed);
}

/// Whether ANSI color/emoji styling should be emitted right now. Off when
/// `--no-color` was passed, when `NO_COLOR` is set in the environment (any
/// value — the no-color.org convention), or when stdout is NOT a TTY (piped or
/// redirected) so escape codes never leak into a file or a downstream program.
pub fn colors_enabled() -> bool {
    if FORCE_NO_COLOR.load(Ordering::Relaxed) {
        return false;
    }
    if std::env::var_os("NO_COLOR").is_some() {
        return false;
    }
    // SAFETY: plain isatty query on stdout (fd 1).
    unsafe { libc::isatty(1) == 1 }
}

/// The palette. Each variant maps to one SGR prefix; `paint` wraps a string in
/// it and a [`RESET`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Color {
    Green,
    Yellow,
    Red,
    Blue,
    Cyan,
    Dim,
}

impl Color {
    /// The SGR escape prefix for this color.
    pub fn code(self) -> &'static str {
        match self {
            Color::Green => "\x1b[32m",
            Color::Yellow => "\x1b[33m",
            Color::Red => "\x1b[31m",
            Color::Blue => "\x1b[34m",
            Color::Cyan => "\x1b[36m",
            Color::Dim => "\x1b[2m",
        }
    }
}

/// Wrap `text` in `color` when color is enabled, else return it untouched.
pub fn paint(text: &str, color: Color) -> String {
    paint_with(text, color, colors_enabled())
}

/// Pure form of [`paint`]: the caller supplies the on/off decision, so the
/// mapping is testable without a TTY.
pub fn paint_with(text: &str, color: Color, color_on: bool) -> String {
    if color_on {
        format!("{}{text}{RESET}", color.code())
    } else {
        text.to_string()
    }
}

/// Shorthand for dim/secondary text (footnotes, hints).
pub fn dim(text: &str) -> String {
    paint(text, Color::Dim)
}

/// Map a status/phase string to its `(emoji, color)`. Pure — the heart of the
/// scheme, exhaustively unit-tested. Recognizes the canonical job statuses
/// (`done`/`running`/`failed`/`queued`) plus the free-form phase strings a
/// coordinator records (`planning`, `reviewing`, `pushing`, …) by substring.
pub fn classify_status(status: &str) -> (&'static str, Color) {
    let s = status.trim().to_ascii_lowercase();
    match s.as_str() {
        "done" | "success" | "succeeded" | "complete" | "completed" | "finished" | "merged"
        | "ok" => ("✅", Color::Green),
        "failed" | "error" | "errored" | "cancelled" | "canceled" | "aborted" | "timeout"
        | "timed_out" => ("❌", Color::Red),
        "running" | "working" | "in_progress" | "in-progress" | "active" | "executing" | "busy" => {
            ("🔄", Color::Yellow)
        }
        "queued" | "pending" | "dispatched" | "starting" | "waiting" | "scheduled" | "new" => {
            ("⏳", Color::Blue)
        }
        _ => {
            // Coordinator phase strings are free-form; treat anything that reads
            // like active work as running, otherwise a neutral dim bullet.
            const ACTIVE: &[&str] = &[
                "plan",
                "review",
                "push",
                "build",
                "test",
                "implement",
                "fix",
                "research",
                "writ",
                "edit",
                "run",
            ];
            if ACTIVE.iter().any(|k| s.contains(k)) {
                ("🔄", Color::Yellow)
            } else {
                ("•", Color::Dim)
            }
        }
    }
}

/// A status table cell: `"<emoji> <colored-status>"`. Runtime form.
pub fn styled_status(status: &str) -> String {
    styled_status_with(status, colors_enabled())
}

/// Pure form of [`styled_status`].
pub fn styled_status_with(status: &str, color_on: bool) -> String {
    let (emoji, color) = classify_status(status);
    format!("{emoji} {}", paint_with(status, color, color_on))
}

/// Color a result cell produced elsewhere (`"✓ #42"`, `"✗ build broke"`, `"—"`)
/// by its leading glyph: green for success, red for failure, dim for none.
/// Runtime form.
pub fn styled_result(cell: &str) -> String {
    styled_result_with(cell, colors_enabled())
}

/// Pure form of [`styled_result`].
pub fn styled_result_with(cell: &str, color_on: bool) -> String {
    let t = cell.trim();
    if t.is_empty() || t == "—" {
        return paint_with("—", Color::Dim, color_on);
    }
    if t.starts_with('✓') || t.starts_with('✅') {
        return paint_with(t, Color::Green, color_on);
    }
    if t.starts_with('✗') || t.starts_with('❌') {
        return paint_with(t, Color::Red, color_on);
    }
    t.to_string()
}

/// Emoji marking a background job's KIND, for legends and mixed listings.
pub fn job_type_emoji(kind: &str) -> &'static str {
    match kind.trim().to_ascii_lowercase().as_str() {
        "worker" | "coordinator" => "🤖",
        "batch" => "📦",
        "goal" => "🎯",
        _ => "•",
    }
}

/// Emoji marking what a background worker is DOING, inferred from its task text.
///
/// The `:workers` table used to stamp every row with the same generic robot
/// (`job_type_emoji("worker"|"coordinator")`), so an operator couldn't tell an
/// `:alert` monitor apart from a code task at a glance. This classifier inspects
/// the worker's task string for the distinctive markers each special worker
/// class carries and returns a purpose-fitting glyph, falling back to the
/// generic worker robot when the task is just ordinary background work.
///
/// It keys on stable, multi-word anchors (not single common words) to avoid
/// false positives, and is pure so it's cheap and unit-testable. Extend by
/// adding a new anchor → glyph arm before the fallback.
pub fn job_activity_emoji(task: &str) -> &'static str {
    let t = task.to_ascii_lowercase();
    // Operator `:alert` monitors are spawned by `spawn_alert_coordinator` with a
    // fixed task prefix — "resolving an operator ALERT (the aish `:alert`
    // feature)" — whose whole job is to call the `set_alert` tool when a
    // condition is met. Alarm clock.
    if t.contains("operator alert") || t.contains("`:alert` feature") || t.contains("set_alert") {
        return "⏰";
    }
    // Ordinary background work — reuse the generic worker glyph so the robot
    // emoji stays single-sourced with `job_type_emoji`.
    job_type_emoji("worker")
}

// ---------------------------------------------------------------------------
// Time formatting — start/stop timestamps + durations for the `:workers` table
// ---------------------------------------------------------------------------

/// Format a whole-second duration compactly, two units at most:
/// `"45s"`, `"2m 30s"`, `"1h 45m"`, `"2d 3h"`. The trailing sub-unit is dropped
/// when zero (`"2m"`, `"1h"`, `"3d"`). Pure — unit-tested.
pub fn fmt_duration(secs: u64) -> String {
    const MIN: u64 = 60;
    const HOUR: u64 = 60 * MIN;
    const DAY: u64 = 24 * HOUR;
    if secs < MIN {
        return format!("{secs}s");
    }
    if secs < HOUR {
        let (m, s) = (secs / MIN, secs % MIN);
        return if s == 0 {
            format!("{m}m")
        } else {
            format!("{m}m {s}s")
        };
    }
    if secs < DAY {
        let (h, m) = (secs / HOUR, (secs % HOUR) / MIN);
        return if m == 0 {
            format!("{h}h")
        } else {
            format!("{h}h {m}m")
        };
    }
    let (d, h) = (secs / DAY, (secs % DAY) / HOUR);
    if h == 0 {
        format!("{d}d")
    } else {
        format!("{d}d {h}h")
    }
}

/// A relative "ago" label for an elapsed delta in whole seconds:
/// `"just now"` (< 5s), `"30s ago"`, `"5m ago"`, `"2h ago"`, `"3d ago"`. Pure.
pub fn fmt_ago(secs: u64) -> String {
    const MIN: u64 = 60;
    const HOUR: u64 = 60 * MIN;
    const DAY: u64 = 24 * HOUR;
    if secs < 5 {
        "just now".to_string()
    } else if secs < MIN {
        format!("{secs}s ago")
    } else if secs < HOUR {
        format!("{}m ago", secs / MIN)
    } else if secs < DAY {
        format!("{}h ago", secs / HOUR)
    } else {
        format!("{}d ago", secs / DAY)
    }
}

/// Parse a SQLite `current_timestamp` UTC string (`"YYYY-MM-DD HH:MM:SS"`, the
/// format `coordinator_runs.created_at` / `heartbeat_at` are stored in) to epoch
/// seconds. Tolerates a `T` date/time separator and a trailing fractional part.
/// `None` on any malformed field. Pure — unit-tested (no chrono dependency).
pub fn parse_sqlite_utc(s: &str) -> Option<i64> {
    let s = s.trim();
    let (date, time) = s.split_once([' ', 'T'])?;
    let mut d = date.split('-');
    let year: i64 = d.next()?.parse().ok()?;
    let month: i64 = d.next()?.parse().ok()?;
    let day: i64 = d.next()?.parse().ok()?;
    // Drop any fractional seconds / timezone suffix on the time part.
    let time = time.split(['.', '+', 'Z']).next().unwrap_or(time);
    let mut t = time.split(':');
    let hour: i64 = t.next()?.parse().ok()?;
    let min: i64 = t.next()?.parse().ok()?;
    let sec: i64 = t.next().unwrap_or("0").parse().ok()?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    let days = days_from_civil(year, month, day);
    Some(((days * 24 + hour) * 60 + min) * 60 + sec)
}

/// Days since the Unix epoch (1970-01-01) for a proleptic-Gregorian civil date.
/// Howard Hinnant's `days_from_civil` algorithm — exact integer math, no deps.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400; // [0, 399]
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    era * 146097 + doe - 719468
}

/// Build the `(Started, Runtime)` cells for a `:workers` row from epoch seconds.
/// `started` is the worker's start time, `finished` its terminal time (None
/// while running), `now` the current time. The Started cell is a relative "ago"
/// label; the Runtime cell is the elapsed-so-far (running) or total (terminal)
/// duration. A missing start renders both as the dim em-dash placeholder. Pure
/// — the single source of truth for both the in-memory and durable rows.
pub fn time_cells(started: Option<i64>, finished: Option<i64>, now: i64) -> (String, String) {
    let Some(start) = started else {
        return ("—".to_string(), "—".to_string());
    };
    let started_cell = fmt_ago((now - start).max(0) as u64);
    let end = finished.unwrap_or(now);
    let runtime_cell = fmt_duration((end - start).max(0) as u64);
    (started_cell, runtime_cell)
}

// ---------------------------------------------------------------------------
// Statusline — a left/right-justified info bar printed above the REPL prompt
// ---------------------------------------------------------------------------

/// Stdout terminal width in columns, floored at 80. A tty is queried via
/// TIOCGWINSZ; off a tty we honor `$COLUMNS`, else fall back to 80. The floor
/// keeps the statusline from collapsing on a narrow or unknown terminal.
fn statusline_width() -> usize {
    // SAFETY: isatty + a read-only TIOCGWINSZ ioctl on stdout (fd 1).
    unsafe {
        if libc::isatty(1) == 1 {
            let mut ws: libc::winsize = std::mem::zeroed();
            if libc::ioctl(1, libc::TIOCGWINSZ, &mut ws) == 0 && ws.ws_col > 0 {
                return (ws.ws_col as usize).max(80);
            }
        }
    }
    std::env::var("COLUMNS")
        .ok()
        .and_then(|c| c.parse::<usize>().ok())
        .map(|w| w.max(80))
        .unwrap_or(80)
}

/// Civil date `(year, month, day)` from days-since-Unix-epoch. Inverse of
/// [`days_from_civil`] — Howard Hinnant's `civil_from_days`, exact integer math.
fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Format a UTC epoch-second instant as `"YYYY-MM-DD HH:MM"` (minute precision).
/// Pure — no chrono dependency; unit-tested against known instants.
pub fn fmt_datetime_utc(epoch: i64) -> String {
    let days = epoch.div_euclid(86_400);
    let sod = epoch.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    let (hh, mm) = (sod / 3600, (sod % 3600) / 60);
    format!("{y:04}-{m:02}-{d:02} {hh:02}:{mm:02}")
}

/// Render the REPL statusline: version + shell tagline + model on the LEFT,
/// the current UTC date/time (`YYYY-MM-DD HH:MM`) right-justified on the RIGHT,
/// separated by enough spaces to fill the terminal width. Runtime form — reads
/// the wall clock, terminal width, and [`colors_enabled`]. Off a tty (piped /
/// `NO_COLOR`) it still returns plain text; the caller decides whether to print.
pub fn statusline(version: &str, model: &str, stats: &str) -> String {
    let epoch = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    statusline_at(
        version,
        model,
        stats,
        epoch,
        statusline_width(),
        colors_enabled(),
    )
}

/// Current terminal width used for footer rows (floored at 80). Public so the
/// 2nd statusline can right-justify against the same width as the main bar.
pub fn footer_width() -> usize {
    statusline_width()
}

/// Visible column width of a possibly-ANSI-styled string: SGR/CSI escapes count
/// as zero width, everything else by its unicode display width. Used to align
/// the 2nd statusline when its left half carries color codes.
pub fn visible_cols(s: &str) -> usize {
    use unicode_width::UnicodeWidthChar;
    let mut width = 0usize;
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            // Skip a CSI escape: ESC [ ... final byte in 0x40..=0x7e.
            if chars.peek() == Some(&'[') {
                chars.next();
                for n in chars.by_ref() {
                    if ('\x40'..='\x7e').contains(&n) {
                        break;
                    }
                }
            }
            continue;
        }
        width += c.width().unwrap_or(0);
    }
    width
}

/// Compose the footer's 2nd statusline (row H-1): the already-styled `left`
/// coordinator message stays on the LEFT and the session `name` (set via
/// `:rename`) is right-justified on the RIGHT, in bold magenta (the accent it
/// carried as the prompt `[name]` prefix). When there's no name the `left` is returned
/// unchanged. Pure — width/color are supplied so it's unit-testable.
pub fn second_statusline_at(left: &str, name: Option<&str>, width: usize, color_on: bool) -> String {
    let name = match name {
        Some(n) if !n.is_empty() => n,
        _ => return left.to_string(),
    };
    let width = width.max(80);
    let lw = visible_cols(left);
    let rw = name.chars().count();
    let gap = width.saturating_sub(lw + rw).max(1);
    let spaces = " ".repeat(gap);
    if color_on {
        // Bold magenta — same accent the name carried as the prompt `[name]`
        // prefix before it moved onto this row (kept deliberately, not dimmed).
        format!("{left}{spaces}\x1b[1;35m{name}{RESET}")
    } else {
        format!("{left}{spaces}{name}")
    }
}

/// Pure form of [`statusline`]: the caller supplies the instant, width, and
/// color decision, so alignment + padding are unit-testable without a TTY.
pub fn statusline_at(
    version: &str,
    model: &str,
    stats: &str,
    epoch: i64,
    width: usize,
    color_on: bool,
) -> String {
    let left = format!("aish v{version} — AI-native shell · {model}");
    let time = fmt_datetime_utc(epoch);
    // The running session stats (tokens in/out, tool calls, turns) sit on the
    // RIGHT, immediately to the LEFT of the clock — a middle-dot separator (with
    // flanking spaces) between them. An empty `stats` (a fresh session, nothing
    // run yet) collapses to just the clock.
    let right = if stats.is_empty() {
        time.clone()
    } else {
        format!("{stats} · {time}")
    };
    let width = width.max(80);
    // Char counts, not byte lengths — the em-dash and middle-dot are multi-byte
    // but single-column, so counting chars keeps the right edge aligned.
    let (lw, rw) = (left.chars().count(), right.chars().count());
    // At least one space between the two halves when they'd otherwise collide.
    let gap = width.saturating_sub(lw + rw).max(1);
    let spaces = " ".repeat(gap);
    if color_on {
        // Subtle accents rather than one flat dim wash: a cyan version badge,
        // a dim tagline/model frame, and a dim right-justified stats+clock. The
        // gap above is computed from the PLAIN char widths, so coloring the
        // halves never disturbs the alignment.
        let badge = format!("\x1b[36maish v{version}{RESET}");
        let frame = format!("\x1b[2m — AI-native shell · {model}{RESET}");
        let right_dim = format!("\x1b[2m{right}{RESET}");
        format!("{badge}{frame}{spaces}{right_dim}")
    } else {
        format!("{left}{spaces}{right}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fmt_duration_compact_two_units() {
        assert_eq!(fmt_duration(0), "0s");
        assert_eq!(fmt_duration(45), "45s");
        assert_eq!(fmt_duration(60), "1m");
        assert_eq!(fmt_duration(150), "2m 30s");
        assert_eq!(fmt_duration(3600), "1h");
        assert_eq!(fmt_duration(6300), "1h 45m"); // 1h45m
        assert_eq!(fmt_duration(86_400), "1d");
        assert_eq!(fmt_duration(183_600), "2d 3h"); // 2d3h
    }

    #[test]
    fn fmt_ago_buckets() {
        assert_eq!(fmt_ago(0), "just now");
        assert_eq!(fmt_ago(4), "just now");
        assert_eq!(fmt_ago(30), "30s ago");
        assert_eq!(fmt_ago(300), "5m ago");
        assert_eq!(fmt_ago(7200), "2h ago");
        assert_eq!(fmt_ago(259_200), "3d ago");
    }

    #[test]
    fn parse_sqlite_utc_roundtrips_epoch() {
        // The Unix epoch itself.
        assert_eq!(parse_sqlite_utc("1970-01-01 00:00:00"), Some(0));
        // A known instant: 2021-01-01 00:00:00 UTC = 1609459200.
        assert_eq!(parse_sqlite_utc("2021-01-01 00:00:00"), Some(1_609_459_200));
        // 'T' separator + fractional seconds are tolerated.
        assert_eq!(
            parse_sqlite_utc("2021-01-01T00:00:01.500"),
            Some(1_609_459_201)
        );
        // Malformed input → None (never panics).
        assert_eq!(parse_sqlite_utc("not a timestamp"), None);
        assert_eq!(parse_sqlite_utc("2021-13-01 00:00:00"), None); // bad month
        assert_eq!(parse_sqlite_utc(""), None);
    }

    #[test]
    fn time_cells_running_vs_terminal() {
        // Running: finished=None → runtime is now-start, started shows "ago".
        let (started, runtime) = time_cells(Some(1000), None, 1150);
        assert_eq!(started, "2m ago"); // relative label rounds to the minute
        assert_eq!(runtime, "2m 30s"); // runtime keeps sub-unit precision
        // Terminal: runtime is the FROZEN stop-start span, not now-start.
        let (started, runtime) = time_cells(Some(1000), Some(1090), 5000);
        assert_eq!(runtime, "1m 30s"); // 90s total, regardless of now
        assert!(started.ends_with("ago"));
        // No start → both placeholders.
        assert_eq!(time_cells(None, None, 9999), ("—".to_string(), "—".to_string()));
    }


    #[test]
    fn fmt_datetime_utc_known_instants() {
        assert_eq!(fmt_datetime_utc(0), "1970-01-01 00:00");
        // 2021-01-01 00:00:00 UTC = 1609459200.
        assert_eq!(fmt_datetime_utc(1_609_459_200), "2021-01-01 00:00");
        // Minute precision: 2021-01-01 12:34:56 = 1609504496.
        assert_eq!(fmt_datetime_utc(1_609_504_496), "2021-01-01 12:34");
        // 2023-11-14 22:13:20 UTC.
        assert_eq!(fmt_datetime_utc(1_700_000_000), "2023-11-14 22:13");
    }

    #[test]
    fn statusline_aligns_and_pads_to_width() {
        let s = statusline_at("0.21.1", "claude (sonnet)", "", 1_609_459_200, 80, false);
        assert!(!s.contains('\x1b')); // plain mode: no ANSI
        assert!(s.starts_with("aish v0.21.1 — AI-native shell · claude (sonnet)"));
        assert!(s.ends_with("2021-01-01 00:00"));
        // Dash/dot are single-column; char count fills exactly the width.
        assert_eq!(s.chars().count(), 80);
    }

    #[test]
    fn statusline_stats_sit_left_of_clock() {
        let stats = "tokens: 120 in / 34 out, tool calls: 7, turns: 3";
        let s = statusline_at("0.21.1", "m", stats, 1_609_459_200, 120, false);
        assert!(!s.contains('\x1b'));
        // Stats land immediately to the left of the clock (middle-dot between).
        assert!(s.contains(&format!("{stats} · 2021-01-01 00:00")));
        assert!(s.ends_with("2021-01-01 00:00"));
        assert_eq!(s.chars().count(), 120);
    }

    #[test]
    fn statusline_colored_has_subtle_accents() {
        let s = statusline_at("0.21.1", "m", "", 0, 80, true);
        // Cyan version badge up front, a dim frame after it, RESET at the end.
        assert!(s.starts_with("\x1b[36maish v0.21.1"));
        assert!(s.contains("\x1b[2m")); // dim tagline/clock present
        assert!(s.ends_with(RESET));
        // Plain visible text is unchanged (strip SGR and compare width intent).
        assert!(s.contains("AI-native shell"));
    }

    #[test]
    fn statusline_narrow_width_floors_at_80() {
        let s = statusline_at("0.21.1", "x", "", 0, 10, false);
        assert!(s.chars().count() >= 80);
        assert!(s.contains("  ")); // separating gap present
    }

    #[test]
    fn visible_cols_ignores_ansi() {
        assert_eq!(visible_cols("abc"), 3);
        assert_eq!(visible_cols("\x1b[36mabc\x1b[0m"), 3);
        assert_eq!(visible_cols("\x1b[1;33m⇄x \x1b[0m"), 3); // arrow + 'x' + space
        assert_eq!(visible_cols(""), 0);
    }

    #[test]
    fn second_statusline_right_justifies_name() {
        // No name → left returned unchanged.
        assert_eq!(second_statusline_at("left", None, 80, false), "left");
        assert_eq!(second_statusline_at("left", Some(""), 80, false), "left");
        // Plain: name flush right, whole row exactly `width` columns.
        let s = second_statusline_at("left", Some("myproj"), 80, false);
        assert!(s.starts_with("left"));
        assert!(s.ends_with("myproj"));
        assert_eq!(s.chars().count(), 80);
    }

    #[test]
    fn second_statusline_colored_name_is_magenta() {
        let left = "\x1b[36m⇄ detached\x1b[0m";
        let s = second_statusline_at(left, Some("proj"), 80, true);
        assert!(s.starts_with(left)); // left half untouched
        assert!(s.contains("\x1b[1;35mproj")); // name bold magenta (kept accent)
        assert!(s.ends_with(RESET));
        // Alignment is computed from VISIBLE columns, so ANSI in `left` doesn't
        // push the name off the right edge.
        assert_eq!(visible_cols(&s), 80);
    }

    #[test]
    fn no_color_override_forces_plain() {
        // The override returns colors_enabled() early regardless of TTY/env.
        set_no_color(true);
        assert!(!colors_enabled());
        set_no_color(false); // restore for other tests in the binary
    }

    #[test]
    fn paint_wraps_only_when_enabled() {
        assert_eq!(paint_with("hi", Color::Green, true), "\x1b[32mhi\x1b[0m");
        assert_eq!(paint_with("hi", Color::Green, false), "hi");
        // Reset is always appended when on, never when off.
        assert!(paint_with("x", Color::Red, true).ends_with(RESET));
        assert!(!paint_with("x", Color::Red, false).contains('\x1b'));
    }

    #[test]
    fn classify_status_canonical_buckets() {
        assert_eq!(classify_status("done"), ("✅", Color::Green));
        assert_eq!(classify_status("DONE"), ("✅", Color::Green)); // case-insensitive
        assert_eq!(classify_status("merged"), ("✅", Color::Green));
        assert_eq!(classify_status("failed"), ("❌", Color::Red));
        assert_eq!(classify_status("error"), ("❌", Color::Red));
        assert_eq!(classify_status("running"), ("🔄", Color::Yellow));
        assert_eq!(classify_status("in_progress"), ("🔄", Color::Yellow));
        assert_eq!(classify_status("queued"), ("⏳", Color::Blue));
        assert_eq!(classify_status("dispatched"), ("⏳", Color::Blue));
    }

    #[test]
    fn classify_status_freeform_phases() {
        // Coordinator phase strings classify as active work…
        assert_eq!(classify_status("planning").1, Color::Yellow);
        assert_eq!(classify_status("reviewing PR").1, Color::Yellow);
        assert_eq!(classify_status("pushing branch").1, Color::Yellow);
        // …and a genuinely unknown phase gets the neutral bullet.
        assert_eq!(classify_status("zzz-unknown"), ("•", Color::Dim));
    }

    #[test]
    fn styled_status_shape() {
        // Emoji + colored label when on; emoji + plain label when off.
        assert_eq!(styled_status_with("done", true), "✅ \x1b[32mdone\x1b[0m");
        assert_eq!(styled_status_with("done", false), "✅ done");
        assert!(styled_status_with("running", false).starts_with("🔄 "));
    }

    #[test]
    fn styled_result_by_glyph() {
        assert_eq!(styled_result_with("✓ #42", true), "\x1b[32m✓ #42\x1b[0m");
        assert_eq!(
            styled_result_with("✗ broke", true),
            "\x1b[31m✗ broke\x1b[0m"
        );
        assert_eq!(styled_result_with("—", true), "\x1b[2m—\x1b[0m");
        assert_eq!(styled_result_with("", true), "\x1b[2m—\x1b[0m");
        // Plain mode strips the color but keeps the glyph + text.
        assert_eq!(styled_result_with("✓ #42", false), "✓ #42");
    }

    #[test]
    fn job_type_emoji_known_kinds() {
        assert_eq!(job_type_emoji("worker"), "🤖");
        assert_eq!(job_type_emoji("coordinator"), "🤖");
        assert_eq!(job_type_emoji("batch"), "📦");
        assert_eq!(job_type_emoji("goal"), "🎯");
        assert_eq!(job_type_emoji("mystery"), "•");
    }

    #[test]
    fn job_activity_emoji_alert_vs_generic() {
        // The exact task prefix spawn_alert_coordinator uses → alarm clock.
        let alert_task = "You are resolving an operator ALERT (the aish `:alert` feature). \
Watch for this condition and call the `set_alert` tool with alert_id=7 …";
        assert_eq!(job_activity_emoji(alert_task), "⏰");
        // Any of the anchors alone is enough.
        assert_eq!(job_activity_emoji("call set_alert when the PR merges"), "⏰");
        assert_eq!(job_activity_emoji("watch for an operator alert condition"), "⏰");
        // Ordinary background work falls back to the generic worker robot.
        assert_eq!(job_activity_emoji("fix the failing CI on branch feat/x"), "🤖");
        assert_eq!(job_activity_emoji("refactor the coordinator store"), "🤖");
        assert_eq!(job_activity_emoji(""), "🤖");
    }
}
