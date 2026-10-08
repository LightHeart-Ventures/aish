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
        // TASK-291: a round-cap run parked for review / `:resume` — terminal but
        // not a failure, so a distinct "paused" glyph rather than ✅/❌.
        "checkpoint" | "checkpointed" | "parked" | "paused" => ("⏸", Color::Blue),
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

/// The glyph marking operator `:alert` activity — an alarm clock. Single-sourced
/// here so the `:workers` activity icon ([`job_activity_emoji`]) and the
/// fired-alert SecondStatusLine badge ([`alert_badge`]) always render the same
/// symbol; change it once and both surfaces move together.
pub const ALERT_GLYPH: &str = "⏰";

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
        return ALERT_GLYPH;
    }
    // Background `:goal` loop turns are spawned with the GOAL_DIRECTIVE_PREFIX,
    // whose opening line is "Work toward this goal …". That anchor survives the
    // task-text truncation the `:workers` rows apply, so a goal reads as a
    // bullseye/target rather than the generic robot.
    if t.contains("work toward this goal") {
        return "🎯";
    }
    // Ordinary background work — reuse the generic worker glyph so the robot
    // emoji stays single-sourced with `job_type_emoji`.
    job_type_emoji("worker")
}

/// Render the compact fired-`:alert` badge that claims the head of the
/// SecondStatusLine until the next prompt.
///
/// When an operator `:alert` fires, the presenter turns the short banner into
/// this badge and stores it as the SecondStatusLine's single flash message
/// (`session.flash`, most-recent wins) — prefixed with the same alarm-clock
/// [`ALERT_GLYPH`] the
/// `:workers` table uses for `:alert` monitors, so the fire is instantly legible
/// as an alert. Bold yellow when color is on (it stands apart from the worker
/// badges); plain glyph + text otherwise. Pure, so it's unit-testable.
#[allow(dead_code)]
pub fn alert_badge(banner: &str, color_on: bool) -> String {
    if color_on {
        format!("\x1b[1;33m{ALERT_GLYPH} {banner}\x1b[0m")
    } else {
        format!("{ALERT_GLYPH} {banner}")
    }
}

/// Severity tier for a fired `:alert` / `:activity` entry — drives badge color.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Severity {
    Info,
    Warn,
    Critical,
}

impl Severity {
    /// Canonical lowercase tag (persisted in the activity store).
    pub fn as_str(self) -> &'static str {
        match self {
            Severity::Info => "info",
            Severity::Warn => "warn",
            Severity::Critical => "critical",
        }
    }

    /// Parse a persisted tag back into a tier (unknown → Info).
    pub fn from_tag(s: &str) -> Severity {
        match s.trim().to_lowercase().as_str() {
            "critical" | "crit" | "fatal" | "error" => Severity::Critical,
            "warn" | "warning" => Severity::Warn,
            _ => Severity::Info,
        }
    }

    /// Infer a tier from free banner/detail text by keyword.
    pub fn infer(text: &str) -> Severity {
        let t = text.to_lowercase();
        if t.contains("critical")
            || t.contains("fatal")
            || t.contains("error")
            || t.contains("fail")
            || t.contains("panic")
            || t.contains('❌')
        {
            Severity::Critical
        } else if t.contains("warn")
            || t.contains("slow")
            || t.contains("retry")
            || t.contains("degrad")
        {
            Severity::Warn
        } else {
            Severity::Info
        }
    }

    /// SGR prefix (bold + color) for this tier.
    fn sgr(self) -> &'static str {
        match self {
            Severity::Info => "1;36",     // cyan
            Severity::Warn => "1;33",     // yellow
            Severity::Critical => "1;31", // red
        }
    }
}

/// Severity-tiered variant of [`alert_badge`]: same glyph + layout, but the
/// color reflects the tier (cyan=info, yellow=warn, red=critical). Pure.
pub fn severity_badge(banner: &str, sev: Severity, color_on: bool) -> String {
    if color_on {
        format!("\x1b[{}m{ALERT_GLYPH} {banner}\x1b[0m", sev.sgr())
    } else {
        format!("{ALERT_GLYPH} {banner}")
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

/// Display threshold mirroring `coordinator::ORPHAN_STALE_AFTER` (15 min). A
/// running coordinator whose last heartbeat is older than this is flagged stale
/// in status listings. Kept local so this pure formatter carries no coordinator
/// dependency; a coordinator-side test asserts the two stay equal.
pub const HEARTBEAT_STALE_AFTER_SECS: i64 = 15 * 60;

/// Freshness cell for a coordinator's last heartbeat — the at-a-glance
/// "alive-and-quiet vs actually-hung" signal for `background_status` /
/// `:workers`. `heartbeat_at` is the SQLite UTC string; `terminal` is true for
/// done/failed/checkpoint rows (which have no live beat). `now` is epoch secs.
/// Returns:
///   * `—`            — missing/unparseable beat (nothing is known)
///   * `· 22m`        — terminal row: how long ago it last beat (i.e. finished)
///   * `♥ 12s`/`♥ 4m` — alive: age since last beat, under the stale threshold
///   * `⚠ 22m`        — stale: beat older than `HEARTBEAT_STALE_AFTER_SECS`
///
/// Terminal rows used to render a flat `—`, which made the Beat column a second
/// echo of the phase column rather than independent evidence — an empty beat
/// next to a `done` phase reads as two sources agreeing when it is really one
/// fact printed twice. Showing the real age keeps the column informative
/// ("finished 22m ago") and keeps `—` meaning exactly one thing: no data.
/// Pure — unit-tested, no chrono.
pub fn fmt_heartbeat_age(heartbeat_at: Option<&str>, terminal: bool, now: i64) -> String {
    let Some(beat) = heartbeat_at.and_then(parse_sqlite_utc) else {
        return "—".to_string();
    };
    let age = (now - beat).max(0);
    let label = compact_age(age);
    if terminal {
        // Finished: no liveness claim, just "last beat was N ago".
        format!("· {label}")
    } else if age > HEARTBEAT_STALE_AFTER_SECS {
        format!("⚠ {label}")
    } else {
        format!("♥ {label}")
    }
}

/// Single-unit compact age: `12s` · `4m` · `2h` · `3d`. The shared vocabulary
/// behind every heartbeat cell so the `:workers` table, `background_status`, and
/// the pinned escalation banner all spell "how long ago" the same way.
pub fn compact_age(age_secs: i64) -> String {
    const MIN: i64 = 60;
    const HOUR: i64 = 60 * MIN;
    const DAY: i64 = 24 * HOUR;
    let age = age_secs.max(0);
    if age < MIN {
        format!("{age}s")
    } else if age < HOUR {
        format!("{}m", age / MIN)
    } else if age < DAY {
        format!("{}h", age / HOUR)
    } else {
        format!("{}d", age / DAY)
    }
}

/// The coordinator's durable heartbeat cadence — a display-side mirror of
/// `coordinator::HEARTBEAT_INTERVAL` (30s), kept here so these pure formatters
/// carry no coordinator dependency. A coordinator-side test
/// (`heartbeat_interval_matches_display_const`) asserts the two stay equal.
pub const HEARTBEAT_INTERVAL_SECS: i64 = 30;

/// Jitter grace before a LATE beat counts as a MISSED beat — half an interval.
/// A beat that lands a couple of seconds behind schedule (scheduler jitter, a
/// long-running tool call holding the loop) must not flip a healthy worker's
/// heart to yellow; a genuinely skipped 30s window must.
pub const HEARTBEAT_GRACE_SECS: i64 = HEARTBEAT_INTERVAL_SECS / 2;

/// How many heartbeats a coordinator has MISSED, given the age of its last
/// beat. `0` while the beat is fresh (age under one interval + [`HEARTBEAT_GRACE_SECS`]),
/// then one per fully-elapsed interval after that: 45s → 1, 75s → 2, 105s → 3.
/// Pure integer math — the single definition of "missed a beat" shared by every
/// liveness indicator.
pub fn missed_heartbeats(age_secs: i64) -> u32 {
    let overdue = age_secs - HEARTBEAT_GRACE_SECS;
    if overdue < HEARTBEAT_INTERVAL_SECS {
        return 0;
    }
    (overdue / HEARTBEAT_INTERVAL_SECS).clamp(0, u32::MAX as i64) as u32
}

/// Traffic-light classification for a worker's pulse: the glyph plus the colour
/// that answers "is anyone home?" at a glance.
///   * `None` age          → dim `♡` — no beat on record yet (just launched, or
///     a pre-heartbeat row). Hollow heart = no claim.
///   * 0 missed            → GREEN `♥` — beat is current.
///   * 1 missed            → YELLOW `♥` — one window skipped; usually a long
///     tool call, worth watching but not yet alarming.
///   * 2+ missed           → RED `♥` — two or more windows skipped; the worker
///     is wedged, rate-limited, or dead.
///     Pure, so the tiers are unit-testable without a clock or a TTY.
pub fn heartbeat_tier(beat_age_secs: Option<i64>) -> (&'static str, Color) {
    match beat_age_secs.map(missed_heartbeats) {
        None => ("♡", Color::Dim),
        Some(0) => ("♥", Color::Green),
        Some(1) => ("♥", Color::Yellow),
        Some(_) => ("♥", Color::Red),
    }
}

/// The painted liveness HEART for a live worker, ready to drop into a status
/// row. A HEALTHY heart renders as the bare glyph — a quiet green `♥` is the
/// whole signal, and stamping an age onto every frame is noise that trains the
/// eye to ignore the cell. Once a beat is MISSED the age is appended (`♥ 45s`)
/// because then the operator needs the evidence, not just the alarm.
pub fn heartbeat_heart(beat_age_secs: Option<i64>, color_on: bool) -> String {
    let (glyph, color) = heartbeat_tier(beat_age_secs);
    let text = match beat_age_secs {
        Some(age) if missed_heartbeats(age) > 0 => format!("{glyph} {}", compact_age(age)),
        _ => glyph.to_string(),
    };
    paint_with(&text, color, color_on)
}

// ---------------------------------------------------------------------------
// Statusline — a left/right-justified info bar printed above the REPL prompt
// ---------------------------------------------------------------------------

/// Narrowest width the statusline zone solver will lay out against. Below this
/// even a shed-to-the-bone bar (`aish vX` + clock) can't both fit, so the solver
/// clips rather than shedding further — there is nothing left worth dropping.
pub const MIN_STATUSLINE_COLS: usize = 20;

/// Width assumed when the real terminal width is UNKNOWN (not a tty, no
/// `$COLUMNS`) — the classic 80-column default.
const ASSUMED_STATUSLINE_COLS: usize = 80;

/// Stdout terminal width in columns. A tty is queried via TIOCGWINSZ; off a tty
/// we honor `$COLUMNS`; when both are unknown we assume 80.
///
/// This used to floor the ANSWER at 80 even on a 60-column terminal — the bar
/// was then laid out 80 wide and the footer painter clipped the overhang off the
/// RIGHT edge, which is exactly where the clock and session stats live. The
/// highest-value zone was the one silently destroyed. We now report the REAL
/// width and let [`statusline_at`] shed low-value chrome to fit it, so the clock
/// survives every terminal size.
fn statusline_width() -> usize {
    // SAFETY: isatty + a read-only TIOCGWINSZ ioctl on stdout (fd 1).
    unsafe {
        if libc::isatty(1) == 1 {
            let mut ws: libc::winsize = std::mem::zeroed();
            if libc::ioctl(1, libc::TIOCGWINSZ, &mut ws) == 0 && ws.ws_col > 0 {
                return (ws.ws_col as usize).max(MIN_STATUSLINE_COLS);
            }
        }
    }
    std::env::var("COLUMNS")
        .ok()
        .and_then(|c| c.parse::<usize>().ok())
        .filter(|w| *w > 0)
        .map(|w| w.max(MIN_STATUSLINE_COLS))
        .unwrap_or(ASSUMED_STATUSLINE_COLS)
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

/// Local timezone offset in seconds east of UTC for the given epoch instant.
/// Resolved via libc `localtime_r`, so it honors `$TZ` and the system zoneinfo
/// database — including the correct DST rule for that specific instant. Falls
/// back to `0` (UTC) if the C call fails. Not pure (reads the system TZ), so it
/// lives outside the unit-tested date helpers.
fn local_offset_secs(epoch: i64) -> i64 {
    unsafe {
        let t = epoch as libc::time_t;
        let mut tm: libc::tm = std::mem::zeroed();
        if libc::localtime_r(&t, &mut tm).is_null() {
            return 0;
        }
        tm.tm_gmtoff as i64
    }
}

/// Render the REPL statusline: version + shell tagline + model on the LEFT,
/// the current LOCAL date/time (`YYYY-MM-DD HH:MM`) right-justified on the RIGHT,
/// separated by enough spaces to fill the terminal width. Runtime form — reads
/// the wall clock, terminal width, and [`colors_enabled`]. Off a tty (piped /
/// `NO_COLOR`) it still returns plain text; the caller decides whether to print.
pub fn statusline(version: &str, model: &str, stats: &str) -> String {
    let epoch = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    // Shift the instant by the local UTC offset so the clock shows system/wall
    // time (e.g. Central) rather than UTC. `statusline_at`/`fmt_datetime_utc`
    // stay pure UTC formatters — feeding them the offset-adjusted epoch renders
    // local wall-clock time without a chrono dependency.
    let local = epoch + local_offset_secs(epoch);
    statusline_at(
        version,
        model,
        stats,
        local,
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
            } else if chars.peek() == Some(&']') {
                // Skip an OSC string: ESC ] ... terminated by BEL or ST (ESC \).
                // OSC 8 hyperlinks ride this shape; without the skip a link's
                // URL would be counted as visible columns and knock the
                // statusline out of alignment.
                chars.next();
                while let Some(n) = chars.next() {
                    if n == '\x07' {
                        break;
                    }
                    if n == '\x1b' {
                        if chars.peek() == Some(&'\\') {
                            chars.next();
                        }
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
pub fn second_statusline_at(
    left: &str,
    name: Option<&str>,
    width: usize,
    color_on: bool,
) -> String {
    let name = match name {
        Some(n) if !n.is_empty() => n,
        _ => return left.to_string(),
    };
    let width = width.max(MIN_STATUSLINE_COLS);
    // `visible_cols` (not chars) so emoji status badges appended to the name —
    // 🤖 workers / ⏰ alert / 🎯 goal, each 2 display cols — don't push the
    // right edge past the terminal width.
    let rw = visible_cols(name);
    // The RIGHT zone (session name + live badges) is the row's anchored signal:
    // an armed ⏰ alert or a 🤖 working coordinator must stay visible. So the
    // LEFT (coordinator hint + plugin segments) is what yields when the two
    // collide — clipped escape-aware with `…` — instead of letting the overflow
    // run off the edge and take the badges with it.
    let left_budget = width.saturating_sub(rw + 1);
    let clipped = clip_cols_styled(left, left_budget);
    let left = clipped.as_str();
    let lw = visible_cols(left);
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

/// Clip a PLAIN (escape-free) string to at most `max` display columns, marking
/// a cut with a trailing `…`. Measured by unicode display width, so a CJK or
/// emoji glyph counts its true 2 columns and a clip never lands mid-glyph.
pub fn clip_cols(s: &str, max: usize) -> String {
    use unicode_width::UnicodeWidthChar;
    if visible_cols(s) <= max {
        return s.to_string();
    }
    if max == 0 {
        return String::new();
    }
    // Reserve one column for the ellipsis so the result still fits `max`.
    let budget = max - 1;
    let mut out = String::with_capacity(s.len());
    let mut width = 0usize;
    for c in s.chars() {
        let w = c.width().unwrap_or(0);
        if width + w > budget {
            break;
        }
        width += w;
        out.push(c);
    }
    out.push('…');
    out
}

/// Clip a possibly-ANSI-styled string to at most `max` display columns, marking
/// a cut with a trailing `…`. Escape-aware (delegates the hard part to
/// [`crate::terminal::clip_visible`], which never splits an escape and resets
/// SGR on a cut), so a colorized plugin segment stays well-formed when clipped.
pub fn clip_cols_styled(s: &str, max: usize) -> String {
    if visible_cols(s) <= max {
        return s.to_string();
    }
    if max == 0 {
        return String::new();
    }
    format!("{}…", crate::terminal::clip_visible(s, max - 1))
}

/// Separator between statusline segments: a middle dot with flanking double
/// spaces. 5 display columns.
const SEG_SEP: &str = "  \u{b7}  ";
const SEG_SEP_COLS: usize = 5;

/// Join statusline segments into `budget` columns by giving every segment an
/// EQUAL share rather than letting the first ones eat the row.
///
/// The old behavior clipped the already-joined run at a hard ceiling, so a
/// chatty first segment pushed every later segment off the row entirely — the
/// quota was global, which means it was really "first come, first served". Each
/// segment now gets `budget / n` columns (after accounting for separators) and
/// is individually clipped with `…`, so a verbose plugin degrades itself instead
/// of silencing its neighbors. Segments that fit whole donate their slack back
/// to the ones that don't, in a single redistribution pass.
///
/// Pure: `budget` is supplied, nothing is read from the environment.
pub fn segments_with_quota(segs: &[String], budget: usize) -> String {
    let segs: Vec<&String> = segs.iter().filter(|s| !s.is_empty()).collect();
    if segs.is_empty() || budget == 0 {
        return String::new();
    }
    let joined = segs
        .iter()
        .map(|s| s.as_str())
        .collect::<Vec<_>>()
        .join(SEG_SEP);
    if visible_cols(&joined) <= budget {
        return joined;
    }
    // Columns left for segment TEXT once the separators are paid for. When the
    // separators alone would blow the budget there's no honest multi-segment
    // render — fall back to clipping the first segment into what we have.
    let sep_cols = SEG_SEP_COLS * segs.len().saturating_sub(1);
    let text_budget = match budget.checked_sub(sep_cols) {
        Some(b) if b >= segs.len() => b,
        _ => return clip_cols_styled(segs[0], budget),
    };
    let n = segs.len();
    let fair = text_budget / n;
    // Segments under their fair share free up columns; hand that slack to the
    // over-budget ones so a short badge next to a long one isn't padded while
    // its neighbor gets truncated.
    let widths: Vec<usize> = segs.iter().map(|s| visible_cols(s)).collect();
    let slack: usize = widths.iter().filter(|w| **w < fair).map(|w| fair - w).sum();
    let over = widths.iter().filter(|w| **w > fair).count().max(1);
    let bonus = slack / over;
    let parts: Vec<String> = segs
        .iter()
        .zip(&widths)
        .map(|(s, w)| {
            if *w <= fair {
                s.to_string()
            } else {
                clip_cols_styled(s, fair + bonus)
            }
        })
        .collect();
    parts.join(SEG_SEP)
}

/// A solved statusline: the LEFT zone split into its `badge` (`aish vX`, the one
/// never-shed token) and `frame` (tagline/model chrome), the RIGHT zone, and the
/// `gap` of spaces that right-justifies it. Returned by [`solve_statusline`] so
/// both the plain and colored renders lay out from one identical solution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatuslineZones {
    pub badge: String,
    pub frame: String,
    pub right: String,
    pub gap: usize,
}

impl StatuslineZones {
    /// Plain (escape-free) render — what a piped/`NO_COLOR` session prints, and
    /// the exact visible text the colored render reproduces.
    pub fn plain(&self) -> String {
        format!(
            "{}{}{}{}",
            self.badge,
            self.frame,
            " ".repeat(self.gap),
            self.right
        )
    }
}

/// Lay the statusline out as THREE zones that shed by priority instead of one
/// left/right pair that overflows.
///
/// The bar carries four things, and they are NOT equally valuable:
///
/// | Zone | Content | Shed order |
/// |---|---|---|
/// | tagline | `— AI-native shell` | 1st — pure branding chrome, zero session info |
/// | stats | tokens / tool calls / turns | 2nd — transient, also on `:stats` |
/// | model | `· claude (sonnet)` | 3rd — slow-changing, also on `:model` |
/// | badge + clock | `aish vX` … `YYYY-MM-DD HH:MM` | never — clipped only as a last resort |
///
/// Previously every zone was always composed and the overflow was cut off the
/// RIGHT edge by the footer painter, which destroyed the clock first and the
/// branding never. This sheds from the cheap end until the row fits, so a narrow
/// terminal loses `— AI-native shell` and keeps the information.
///
/// Pure — width and the clock instant are supplied, so every degradation step is
/// unit-testable without a TTY.
pub fn solve_statusline(
    version: &str,
    model: &str,
    stats: &str,
    time: &str,
    width: usize,
) -> StatuslineZones {
    let width = width.max(MIN_STATUSLINE_COLS);
    let badge = format!("aish v{version}");
    let mut tagline = true;
    let mut show_model = !model.is_empty();
    let mut show_stats = !stats.is_empty();
    loop {
        let mut frame = String::new();
        if tagline {
            frame.push_str(" — AI-native shell");
        }
        if show_model {
            frame.push_str(&format!(" · {model}"));
        }
        let right = if show_stats {
            format!("{stats} · {time}")
        } else {
            time.to_string()
        };
        // At least one space between the zones when they'd otherwise collide.
        let need = visible_cols(&badge) + visible_cols(&frame) + visible_cols(&right) + 1;
        if need <= width {
            let gap = width - (visible_cols(&badge) + visible_cols(&frame) + visible_cols(&right));
            return StatuslineZones {
                badge,
                frame,
                right,
                gap,
            };
        }
        // Shed the cheapest surviving zone and re-solve.
        if tagline {
            tagline = false;
        } else if show_stats {
            show_stats = false;
        } else if show_model {
            show_model = false;
        } else {
            // Bone dry: `aish vX` + clock alone still overflow. Keep the clock
            // whole (it's the live signal) and clip the badge into what's left.
            let rw = visible_cols(&right);
            let badge = clip_cols(&badge, width.saturating_sub(rw + 1));
            let gap = width.saturating_sub(visible_cols(&badge) + rw).max(1);
            return StatuslineZones {
                badge,
                frame: String::new(),
                right,
                gap,
            };
        }
    }
}

/// Pure form of [`statusline`]: the caller supplies the instant, width, and
/// color decision, so alignment + padding are unit-testable without a TTY.
/// Layout is delegated to [`solve_statusline`] — the plain and colored renders
/// are two paints of ONE solved zone plan, so they can never disagree on width.
pub fn statusline_at(
    version: &str,
    model: &str,
    stats: &str,
    epoch: i64,
    width: usize,
    color_on: bool,
) -> String {
    let time = fmt_datetime_utc(epoch);
    let zones = solve_statusline(version, model, stats, &time, width);
    if color_on {
        // Subtle accents rather than one flat dim wash: a cyan version badge,
        // a dim tagline/model frame, and a dim right-justified stats+clock. The
        // gap comes from the PLAIN zone widths, so coloring never disturbs the
        // alignment.
        let spaces = " ".repeat(zones.gap);
        let badge = format!("\x1b[36m{}{RESET}", zones.badge);
        let frame = if zones.frame.is_empty() {
            String::new()
        } else {
            format!("\x1b[2m{}{RESET}", zones.frame)
        };
        let right_dim = format!("\x1b[2m{}{RESET}", zones.right);
        format!("{badge}{frame}{spaces}{right_dim}")
    } else {
        zones.plain()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fmt_heartbeat_age_fresh_stale_terminal() {
        let beat = "2026-01-01 00:00:00";
        let base = parse_sqlite_utc(beat).expect("parse beat");
        // Fresh: under the 15-min stale threshold → ♥ + single-unit age.
        assert_eq!(fmt_heartbeat_age(Some(beat), false, base + 10), "♥ 10s");
        assert_eq!(fmt_heartbeat_age(Some(beat), false, base + 5 * 60), "♥ 5m");
        assert_eq!(
            fmt_heartbeat_age(Some(beat), false, base + 14 * 60),
            "♥ 14m"
        );
        // Stale: beat older than HEARTBEAT_STALE_AFTER_SECS → ⚠.
        assert_eq!(
            fmt_heartbeat_age(Some(beat), false, base + 20 * 60),
            "⚠ 20m"
        );
        assert_eq!(
            fmt_heartbeat_age(Some(beat), false, base + 3 * 3600),
            "⚠ 3h"
        );
        // Terminal rows, missing/unparseable beats, and clock skew degrade safely.
        // Terminal rows report the real age of their last beat ("finished 20m
        // ago"), NOT a blank. A blank made the Beat column silently re-state
        // the phase column, manufacturing fake corroboration for a stale row.
        assert_eq!(fmt_heartbeat_age(Some(beat), true, base + 20 * 60), "· 20m");
        assert_eq!(fmt_heartbeat_age(Some(beat), true, base + 30), "· 30s");
        // `—` now means exactly one thing: no beat data at all.
        assert_eq!(fmt_heartbeat_age(None, true, base), "—");
        assert_eq!(fmt_heartbeat_age(None, false, base), "—");
        assert_eq!(fmt_heartbeat_age(Some("not-a-date"), false, base), "—");
        assert_eq!(fmt_heartbeat_age(Some(beat), false, base - 100), "♥ 0s");
    }

    #[test]
    fn missed_heartbeats_counts_whole_skipped_windows() {
        // Fresh: anything inside one interval + the jitter grace is ZERO missed
        // beats. A beat that lands a second or two late must NOT be reported as
        // a miss — that false positive is what makes a liveness lamp useless.
        assert_eq!(missed_heartbeats(0), 0);
        assert_eq!(missed_heartbeats(29), 0);
        assert_eq!(missed_heartbeats(30), 0);
        assert_eq!(missed_heartbeats(44), 0);
        // One full window skipped past the grace.
        assert_eq!(missed_heartbeats(45), 1);
        assert_eq!(missed_heartbeats(74), 1);
        // Two or more.
        assert_eq!(missed_heartbeats(75), 2);
        assert_eq!(missed_heartbeats(104), 2);
        assert_eq!(missed_heartbeats(105), 3);
        assert_eq!(missed_heartbeats(15 * 60), 29);
        // Clock skew (a beat stamped in the future) degrades to "fresh", never
        // to a negative/overflowing count.
        assert_eq!(missed_heartbeats(-500), 0);
    }

    #[test]
    fn heartbeat_tier_is_a_traffic_light() {
        // No beat on record: hollow heart, no colour claim.
        assert_eq!(heartbeat_tier(None), ("♡", Color::Dim));
        // Beating.
        assert_eq!(heartbeat_tier(Some(0)), ("♥", Color::Green));
        assert_eq!(heartbeat_tier(Some(44)), ("♥", Color::Green));
        // One missed beat → yellow (watch it).
        assert_eq!(heartbeat_tier(Some(45)), ("♥", Color::Yellow));
        assert_eq!(heartbeat_tier(Some(74)), ("♥", Color::Yellow));
        // Two or more → red (wedged/dead).
        assert_eq!(heartbeat_tier(Some(75)), ("♥", Color::Red));
        assert_eq!(heartbeat_tier(Some(9_999)), ("♥", Color::Red));
    }

    #[test]
    fn heartbeat_heart_shows_age_only_once_a_beat_is_missed() {
        // Healthy: the bare glyph. An age on every repaint is noise.
        assert_eq!(heartbeat_heart(Some(10), false), "♥");
        assert_eq!(heartbeat_heart(None, false), "♡");
        // Missed: the age IS the evidence, so it joins the glyph.
        assert_eq!(heartbeat_heart(Some(45), false), "♥ 45s");
        assert_eq!(heartbeat_heart(Some(600), false), "♥ 10m");
        // Colour is carried by the SGR prefix, not by a different glyph, so a
        // monochrome terminal still gets the age text.
        let painted = heartbeat_heart(Some(10), true);
        assert!(painted.starts_with(Color::Green.code()), "{painted:?}");
        let painted = heartbeat_heart(Some(45), true);
        assert!(painted.starts_with(Color::Yellow.code()), "{painted:?}");
        let painted = heartbeat_heart(Some(300), true);
        assert!(painted.starts_with(Color::Red.code()), "{painted:?}");
    }

    #[test]
    fn compact_age_single_unit() {
        assert_eq!(compact_age(0), "0s");
        assert_eq!(compact_age(59), "59s");
        assert_eq!(compact_age(60), "1m");
        assert_eq!(compact_age(3599), "59m");
        assert_eq!(compact_age(3600), "1h");
        assert_eq!(compact_age(86_400), "1d");
        assert_eq!(compact_age(-5), "0s");
    }

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
        assert_eq!(
            time_cells(None, None, 9999),
            ("—".to_string(), "—".to_string())
        );
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
    fn statusline_narrow_width_sheds_chrome_and_keeps_the_clock() {
        // 50 cols: the branding tagline is the first thing to go, and the clock
        // — the zone the OLD right-edge clip destroyed first — survives whole.
        let s = statusline_at("0.21.1", "claude (sonnet)", "", 0, 50, false);
        assert!(!s.contains("AI-native shell"), "tagline sheds first: {s}");
        assert!(s.starts_with("aish v0.21.1"));
        assert!(s.ends_with("1970-01-01 00:00"));
        assert_eq!(visible_cols(&s), 50, "exactly fills the real width: {s}");
    }

    #[test]
    fn statusline_shed_order_is_tagline_then_stats_then_model() {
        let stats = "tokens: 1200 in / 340 out, tool calls: 17, turns: 9";
        let clock = "1970-01-01 00:00";
        // Wide: everything fits.
        let z = solve_statusline("0.21.1", "claude (sonnet)", stats, clock, 160);
        assert!(z.frame.contains("AI-native shell") && z.frame.contains("claude (sonnet)"));
        assert!(z.right.starts_with(stats));
        // Narrower: tagline goes, stats + model stay.
        let z = solve_statusline("0.21.1", "claude (sonnet)", stats, clock, 110);
        assert!(!z.frame.contains("AI-native shell"));
        assert!(z.frame.contains("claude (sonnet)"));
        assert!(z.right.starts_with(stats));
        // Narrower still: stats go, model stays.
        let z = solve_statusline("0.21.1", "claude (sonnet)", stats, clock, 60);
        assert!(z.frame.contains("claude (sonnet)"));
        assert_eq!(z.right, clock);
        // Bone dry: model goes too — badge + clock are what's left.
        let z = solve_statusline("0.21.1", "claude (sonnet)", stats, clock, 32);
        assert_eq!(z.frame, "");
        assert_eq!(z.badge, "aish v0.21.1");
        assert_eq!(z.right, clock);
    }

    #[test]
    fn statusline_zones_never_exceed_the_width_at_any_size() {
        // The whole point of the solver: no size overflows, so the footer painter
        // never has to amputate the right edge. Sweep every plausible width.
        let stats = "tokens: 1200 in / 340 out, tool calls: 17, turns: 9";
        for width in 1..=200usize {
            let z = solve_statusline(
                "0.21.1",
                "claude (sonnet)",
                stats,
                "1970-01-01 00:00",
                width,
            );
            let rendered = visible_cols(&z.plain());
            assert!(
                rendered <= width.max(MIN_STATUSLINE_COLS),
                "width {width} overflowed to {rendered}: {}",
                z.plain()
            );
            // The clock is never sacrificed, and `aish v` always survives.
            assert!(
                z.right.ends_with("1970-01-01 00:00"),
                "lost clock at {width}"
            );
            assert!(!z.badge.is_empty(), "lost badge at {width}");
        }
    }

    #[test]
    fn plain_and_colored_renders_agree_on_visible_width() {
        for width in [40usize, 60, 80, 120] {
            let plain = statusline_at("0.21.1", "claude (sonnet)", "turns: 3", 0, width, false);
            let color = statusline_at("0.21.1", "claude (sonnet)", "turns: 3", 0, width, true);
            assert_eq!(
                visible_cols(&plain),
                visible_cols(&color),
                "color changed the layout at width {width}"
            );
        }
    }

    #[test]
    fn segments_share_the_budget_instead_of_first_come_first_served() {
        let chatty = "a".repeat(60);
        let segs = vec![
            chatty.clone(),
            "ccquota 42%".to_string(),
            "♥ 3m".to_string(),
        ];
        let out = segments_with_quota(&segs, 60);
        assert!(visible_cols(&out) <= 60, "over budget: {out}");
        // Every segment still shows — the verbose one degrades ITSELF.
        assert!(out.contains("ccquota 42%"), "later segment silenced: {out}");
        assert!(out.contains("♥ 3m"), "last segment silenced: {out}");
        assert!(out.contains('…'), "clip marker missing: {out}");
        // Fits whole → returned verbatim, no ellipsis, no reflow.
        let small = vec!["ccquota 42%".to_string(), "♥ 3m".to_string()];
        assert_eq!(segments_with_quota(&small, 60), "ccquota 42%  ·  ♥ 3m");
        assert_eq!(segments_with_quota(&[], 60), "");
    }

    #[test]
    fn segments_quota_keeps_ansi_wellformed() {
        let styled = format!("\x1b[36m{}\x1b[0m", "x".repeat(40));
        let out = segments_with_quota(&[styled, "\x1b[33mwarn\x1b[0m".to_string()], 30);
        assert!(visible_cols(&out) <= 30, "over budget: {out}");
        assert!(out.contains("\x1b[0m"), "SGR left unreset: {out:?}");
        assert!(out.contains("warn"));
    }

    #[test]
    fn clip_cols_marks_the_cut_and_respects_width() {
        assert_eq!(clip_cols("abcdef", 10), "abcdef");
        assert_eq!(clip_cols("abcdef", 4), "abc…");
        assert_eq!(clip_cols("abcdef", 0), "");
        // Wide glyphs count 2 columns, so a clip never lands mid-glyph.
        assert!(visible_cols(&clip_cols("日本語テキスト", 5)) <= 5);
    }

    #[test]
    fn second_statusline_clips_left_before_dropping_the_name_badges() {
        let long = "x".repeat(200);
        let s = second_statusline_at(&long, Some("sprint-42 ⏰"), 80, false);
        assert!(
            visible_cols(&s) <= 80,
            "row overflowed: {}",
            visible_cols(&s)
        );
        // The anchored right zone survives — an armed alert badge must stay visible.
        assert!(s.ends_with("sprint-42 ⏰"), "lost the name/badges: {s}");
        assert!(s.contains('…'), "left zone should be clipped: {s}");
    }

    #[test]
    fn visible_cols_ignores_ansi() {
        assert_eq!(visible_cols("abc"), 3);
        assert_eq!(visible_cols("\x1b[36mabc\x1b[0m"), 3);
        assert_eq!(visible_cols("\x1b[1;33m⇄x \x1b[0m"), 3); // arrow + 'x' + space
        assert_eq!(visible_cols(""), 0);
        // OSC strings (OSC 8 hyperlinks) are zero-width too — only the visible
        // label counts, never the URL, or a linked statusline loses alignment.
        assert_eq!(
            visible_cols("\x1b]8;;https://example.com/long\x1b\\ok\x1b]8;;\x1b\\"),
            2
        );
        assert_eq!(
            visible_cols("\x1b[36m\x1b]8;;https://x.io\x07ok\x1b]8;;\x07\x1b[39m"),
            2
        );
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
        assert_eq!(
            job_activity_emoji("call set_alert when the PR merges"),
            "⏰"
        );
        assert_eq!(
            job_activity_emoji("watch for an operator alert condition"),
            "⏰"
        );
        // Goal-loop turns carry the GOAL_DIRECTIVE_PREFIX opening → bullseye.
        assert_eq!(
            job_activity_emoji("Work toward this goal, then report what you did …"),
            "🎯"
        );
        assert_eq!(job_activity_emoji("work toward this goal"), "🎯");
        // Ordinary background work falls back to the generic worker robot.
        assert_eq!(
            job_activity_emoji("fix the failing CI on branch feat/x"),
            "🤖"
        );
        assert_eq!(job_activity_emoji("refactor the coordinator store"), "🤖");
        assert_eq!(job_activity_emoji(""), "🤖");
    }

    #[test]
    fn alert_badge_carries_alarm_glyph() {
        // Plain mode: alarm glyph + space + banner, no ANSI.
        assert_eq!(alert_badge("PR #42 merged", false), "⏰ PR #42 merged");
        // Colored mode: bold-yellow wrap around glyph + banner.
        assert_eq!(
            alert_badge("PR #42 merged", true),
            "\x1b[1;33m⏰ PR #42 merged\x1b[0m"
        );
        // The badge glyph is the same one `:workers` stamps on `:alert` monitors,
        // so the fired-alert SecondStatusLine and the worker row stay in lockstep.
        assert!(alert_badge("x", false).starts_with(ALERT_GLYPH));
        assert_eq!(job_activity_emoji("set_alert now"), ALERT_GLYPH);
    }
}
