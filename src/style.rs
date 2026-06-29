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
}
