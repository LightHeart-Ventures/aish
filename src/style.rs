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
        "running" | "working" | "in_progress" | "in-progress" | "active" | "executing"
        | "busy" => ("🔄", Color::Yellow),
        "queued" | "pending" | "dispatched" | "starting" | "waiting" | "scheduled" | "new" => {
            ("⏳", Color::Blue)
        }
        _ => {
            // Coordinator phase strings are free-form; treat anything that reads
            // like active work as running, otherwise a neutral dim bullet.
            const ACTIVE: &[&str] = &[
                "plan", "review", "push", "build", "test", "implement", "fix", "research",
                "writ", "edit", "run",
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

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(styled_result_with("✗ broke", true), "\x1b[31m✗ broke\x1b[0m");
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
