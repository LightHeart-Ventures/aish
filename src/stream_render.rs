//! Token-by-token live preview rendering for the S6 suggestion / rewrite
//! surfaces (S8.2 / TASK-144).
//!
//! S8.1 ([`crate::backend::Backend::complete_streaming`]) gave the backend trait
//! a streaming path that hands text deltas to a sink the instant they decode off
//! the wire. This module turns that stream into a **single-line, in-place**
//! terminal preview: the user watches the candidate command assemble
//! token-by-token where the `⚙ rewriting…` / `⚙ suggesting…` spinner used to sit
//! statically, then it lands in the line editor for the same accept / edit /
//! cancel trust surface (nothing runs unconfirmed).
//!
//! Everything shape-deciding here is **pure and unit-tested** — [`preview_line`]
//! collapses the growing raw reply into one clean line, and [`LivePreview`]
//! decides *when* a redraw is warranted and hands back the exact ANSI frame to
//! write. The only terminal I/O (printing the frame, flushing) stays in
//! `repl::run`, so this module never touches a file descriptor and tests need no
//! tty.

/// The maximum width [`preview_width`] will report, so an ultra-wide terminal
/// doesn't let a runaway single-line reply scroll off into nonsense. Commands
/// are short; this is plenty.
const MAX_PREVIEW_WIDTH: usize = 160;

/// Columns available for the streamed preview text, i.e. terminal width minus
/// the `  ⚙ <label> ` gutter, clamped to a sane band. Queried from the tty via
/// `TIOCGWINSZ` on stdout; off a tty we honour `$COLUMNS`, else fall back to 80.
///
/// `gutter` is the visible (non-ANSI) width of the label prefix the caller draws
/// ahead of the preview, so the composed line can't wrap and defeat the
/// `\r\x1b[2K` in-place redraw.
pub fn preview_width(gutter: usize) -> usize {
    let cols = term_cols();
    // Leave a 1-col safety margin off the right edge; floor so a narrow window
    // still shows a useful sliver rather than nothing.
    cols.saturating_sub(gutter + 1).clamp(20, MAX_PREVIEW_WIDTH)
}

/// Stdout terminal width in columns, or a fallback. Mirrors the pattern used
/// elsewhere in the tree (`style`, `md`): tty → `TIOCGWINSZ`, else `$COLUMNS`,
/// else 80.
fn term_cols() -> usize {
    // SAFETY: isatty + a read-only TIOCGWINSZ ioctl on stdout (fd 1).
    unsafe {
        if libc::isatty(1) == 1 {
            let mut ws: libc::winsize = std::mem::zeroed();
            if libc::ioctl(1, libc::TIOCGWINSZ, &mut ws) == 0 && ws.ws_col > 0 {
                return ws.ws_col as usize;
            }
        }
    }
    std::env::var("COLUMNS")
        .ok()
        .and_then(|c| c.parse::<usize>().ok())
        .filter(|w| *w > 0)
        .unwrap_or(80)
}

/// Collapse the accumulated streamed reply into a SINGLE clean line suitable for
/// in-place redraw as tokens arrive. Pure.
///
/// The model is instructed to emit exactly one bare command line, but *while
/// streaming* the partial text can transiently carry a leading markdown code
/// fence, a prompt sigil, or a not-yet-complete first line. This mirrors the
/// final [`crate::rewrite::sanitize_candidate`] shaping closely enough for a
/// faithful live preview:
///
/// - whole-line code fences (```` ``` ````-prefixed) are dropped;
/// - the first non-blank remaining line is taken (the command);
/// - a leading `$ ` / `% ` prompt sigil is stripped;
/// - any control characters (a stray `\r`, `\t`, escape byte) are removed so the
///   preview can't smash the redraw;
/// - the result is clipped to `max_width` graphemes-ish (chars), with a `…`
///   marker when truncated.
///
/// Returns an empty string when nothing renderable has arrived yet (e.g. only a
/// fence line so far), so the caller shows just the spinner label.
pub fn preview_line(accumulated: &str, max_width: usize) -> String {
    let first = accumulated
        .lines()
        .find(|l| !l.trim_start().starts_with("```") && !l.trim().is_empty());
    let Some(line) = first else {
        return String::new();
    };
    let line = line.trim();
    let line = line
        .strip_prefix("$ ")
        .or_else(|| line.strip_prefix("% "))
        .unwrap_or(line);
    // Strip control chars (keep normal printable text incl. spaces).
    let cleaned: String = line.chars().filter(|c| !c.is_control()).collect();
    let cleaned = cleaned.trim_end();
    clip_chars(cleaned, max_width)
}

/// Keep the leading `max` chars of `s` (char-boundary safe), appending `…` when
/// anything was dropped. A `max` of 0 yields an empty string. Pure.
fn clip_chars(s: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    if s.chars().count() <= max {
        return s.to_string();
    }
    // Reserve one column for the ellipsis marker.
    let take = max.saturating_sub(1).max(1);
    let head: String = s.chars().take(take).collect();
    format!("{head}…")
}

/// The ANSI frame that redraws the whole preview line in place: carriage return,
/// clear-to-end-of-line, then a dim `  ⚙ <label> <preview>`. When `preview` is
/// empty this is just the spinner label — identical in spirit to the static
/// `⚙ rewriting…` it replaces. Pure (returns the bytes; the caller writes them).
pub fn frame(label: &str, preview: &str) -> String {
    if preview.is_empty() {
        format!("\r\x1b[2K\x1b[2m  ⚙ {label}\x1b[0m")
    } else {
        format!("\r\x1b[2K\x1b[2m  ⚙ {label} {preview}\x1b[0m")
    }
}

/// The ANSI to wipe the transient preview line once streaming ends (carriage
/// return + clear-to-end-of-line), so the caller can draw the real candidate in
/// the editor on a clean line.
pub const CLEAR_LINE: &str = "\r\x1b[2K";

/// Stateful driver for a live token-by-token preview. Accumulates the text
/// deltas from [`crate::backend::Backend::complete_streaming`], and on each push
/// decides whether the *visible* preview changed — returning the ANSI frame to
/// write only then, so redundant deltas (whitespace, trailing prose past the
/// first line) don't cause flicker or wasted writes.
///
/// The struct itself performs no I/O: `push` returns `Some(frame)`; the REPL
/// prints and flushes it. This keeps the redraw policy fully unit-testable.
pub struct LivePreview {
    label: String,
    max_width: usize,
    acc: String,
    last: Option<String>,
}

impl LivePreview {
    /// A new preview labelled `label` (e.g. `"rewriting…"`), rendering up to
    /// `max_width` columns of command text.
    pub fn new(label: impl Into<String>, max_width: usize) -> Self {
        Self {
            label: label.into(),
            max_width,
            acc: String::new(),
            last: None,
        }
    }

    /// Feed one text delta. Returns the terminal frame to write when the visible
    /// preview line changed since the last frame, else `None` (no redraw needed).
    pub fn push(&mut self, delta: &str) -> Option<String> {
        if delta.is_empty() {
            return None;
        }
        self.acc.push_str(delta);
        let line = preview_line(&self.acc, self.max_width);
        if self.last.as_deref() == Some(line.as_str()) {
            return None;
        }
        let f = frame(&self.label, &line);
        self.last = Some(line);
        Some(f)
    }

    /// The full raw text accumulated so far (all deltas concatenated), for
    /// callers that want to sanitise the final reply themselves. Exercised by
    /// the unit tests; retained as a public accessor for future callers.
    #[allow(dead_code)]
    pub fn accumulated(&self) -> &str {
        &self.acc
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preview_line_takes_first_command_line_stripping_fences_and_sigils() {
        assert_eq!(preview_line("ls -la", 80), "ls -la");
        assert_eq!(preview_line("```sh\nrm -rf build", 80), "rm -rf build");
        assert_eq!(preview_line("$ git status", 80), "git status");
        assert_eq!(preview_line("% pwd", 80), "pwd");
        // A trailing second line (stray note) is ignored — command is line one.
        assert_eq!(
            preview_line("find . -name '*.tmp'\n# done", 80),
            "find . -name '*.tmp'"
        );
    }

    #[test]
    fn preview_line_empty_until_something_renderable_arrives() {
        assert_eq!(preview_line("", 80), "");
        assert_eq!(preview_line("```sh", 80), "");
        assert_eq!(preview_line("   \n\t ", 80), "");
    }

    #[test]
    fn preview_line_strips_control_chars_that_would_smash_the_redraw() {
        // A stray CR / escape sequence in the stream must not break the in-place
        // line redraw — control chars are dropped.
        assert_eq!(preview_line("ls\r -la", 80), "ls -la");
        assert_eq!(preview_line("echo \x1b[31mhi", 80), "echo [31mhi");
    }

    #[test]
    fn preview_line_clips_to_width_with_marker() {
        let long = "a".repeat(200);
        let out = preview_line(&long, 10);
        assert!(out.ends_with('…'));
        assert_eq!(out.chars().count(), 10);
        // Short text is untouched.
        assert_eq!(preview_line("short", 10), "short");
    }

    #[test]
    fn clip_chars_is_char_boundary_safe() {
        let mut s = "a".repeat(9);
        s.push('é'); // multibyte at the cap
        s.push_str("bbb");
        let out = clip_chars(&s, 10);
        assert!(out.ends_with('…'));
        // Did not panic / split the code point; length is capped.
        assert_eq!(out.chars().count(), 10);
        assert_eq!(clip_chars("x", 0), "");
    }

    #[test]
    fn frame_shows_label_only_when_preview_empty() {
        assert_eq!(frame("rewriting…", ""), "\r\x1b[2K\x1b[2m  ⚙ rewriting…\x1b[0m");
        assert_eq!(
            frame("rewriting…", "ls -la"),
            "\r\x1b[2K\x1b[2m  ⚙ rewriting… ls -la\x1b[0m"
        );
    }

    #[test]
    fn live_preview_redraws_only_on_visible_change() {
        let mut p = LivePreview::new("rewriting…", 80);
        // First renderable token → a frame.
        let f1 = p.push("ls").expect("first token draws");
        assert!(f1.contains("ls"));
        // Extending the command → a new frame.
        let f2 = p.push(" -la").expect("more text draws");
        assert!(f2.contains("ls -la"));
        // A trailing newline + second line doesn't change the visible (first)
        // line → no redraw.
        assert_eq!(p.push("\n# note"), None);
        // Full raw text is preserved for the caller to sanitise.
        assert_eq!(p.accumulated(), "ls -la\n# note");
    }

    #[test]
    fn live_preview_ignores_empty_deltas() {
        let mut p = LivePreview::new("suggesting…", 80);
        assert_eq!(p.push(""), None);
        assert_eq!(p.accumulated(), "");
    }

    #[test]
    fn preview_width_leaves_room_for_the_gutter() {
        // Whatever the environment, the reported width is within the clamp band
        // and strictly less than a very wide terminal would allow (gutter left).
        let w = preview_width(12);
        assert!((20..=MAX_PREVIEW_WIDTH).contains(&w));
    }
}
