/// Minimal markdown → ANSI for model replies. Models speak markdown even when
/// asked not to; rather than fight it, render the small subset they actually
/// emit in shell answers: **bold**, *italic*, `code`, ~~strike~~, [links](url),
/// # headers, - bullets, 1. ordered lists, > quotes, --- rules, | tables |, and
/// ``` fences. Not a spec parser — anything unmatched stays literal.
///
/// `base` is the style to re-assert after a reset (e.g. "\x1b[2m" when the
/// caller prints the whole line dim) — SGR 22 turns off bold *and* dim, so a
/// plain bold-off would otherwise un-dim the rest of the line.
use std::cell::Cell;

thread_local! {
    /// When set, `term_width()` returns this instead of querying the tty/$COLUMNS
    /// — the mechanism `render_within` / `render_pane` use to fit tables & rules
    /// to a width other than the raw terminal (e.g. terminal-minus-pane-gutter).
    /// `None` = query the terminal as usual.
    static WIDTH_OVERRIDE: Cell<Option<usize>> = const { Cell::new(None) };
}

/// Columns to assume for pane-bound markdown when the terminal width is unknown
/// (a headless coordinator with no `$COLUMNS`). Conservative on purpose so a
/// forwarded table wraps its cells rather than running off a typical terminal.
const DEFAULT_PANE_COLS: usize = 100;

/// Columns the coordinator-output pane gutter (`┃ [label] `) steals from every
/// row. Reserved when rendering markdown destined for a pane so tables/rules fit
/// the REMAINING width — otherwise the gutter pushes each row past the terminal
/// edge and the terminal hard-wraps the whole box. Short run-id labels
/// (`w_` + 8 hex ⇒ gutter 15); 16 leaves a column of slack.
const PANE_GUTTER_COLS: usize = 16;

/// Like [`render`], but fits tables & horizontal rules within `max_cols` display
/// columns instead of the full terminal width. Restores the previous width on
/// return, so it nests safely.
pub fn render_within(text: &str, base: &str, max_cols: usize) -> String {
    let prev = WIDTH_OVERRIDE.with(|c| c.replace(Some(max_cols)));
    let out = render(text, base);
    WIDTH_OVERRIDE.with(|c| c.set(prev));
    out
}

/// Render markdown to ANSI for placement inside the coordinator-output pane:
/// tables/rules fit within the terminal width MINUS the pane gutter, so the
/// `┃ [label] ` border prepended to every line doesn't push the box past the
/// edge (which makes the terminal hard-wrap the whole table — the bug this
/// avoids). Falls back to a bounded default width when the terminal width is
/// unknown (headless coordinator with no `$COLUMNS`) so a forwarded table is
/// never rendered unbounded-wide.
pub fn render_pane(text: &str, base: &str) -> String {
    let term = match term_width() {
        usize::MAX => DEFAULT_PANE_COLS,
        w => w,
    };
    render_within(text, base, term.saturating_sub(PANE_GUTTER_COLS).max(24))
}

pub fn render(text: &str, base: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let mut out: Vec<String> = Vec::with_capacity(lines.len());
    let mut in_fence = false;
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") {
            in_fence = !in_fence;
            out.push(format!("\x1b[2m{line}\x1b[22m{base}"));
            i += 1;
            continue;
        }
        if in_fence {
            // code verbatim — no inline markup inside fences
            out.push(line.to_string());
            i += 1;
            continue;
        }
        let indent = &line[..line.len() - trimmed.len()];
        // Table block: a `| … |` row followed by a `|---|---|` separator.
        if trimmed.starts_with('|') && i + 1 < lines.len() && is_separator_row(lines[i + 1]) {
            let mut end = i + 2;
            while end < lines.len() && lines[end].trim_start().starts_with('|') {
                end += 1;
            }
            render_table(&lines[i..end], indent, base, &mut out);
            i = end;
            continue;
        }
        // Horizontal rule: `---`, `***`, `___` (3+ of one marker, spaces ok) on
        // its own line → a dim full-width rule. Checked before the bullet branch
        // so `- - -` reads as a rule, not a bullet.
        if is_hrule(trimmed) {
            out.push(format!("{indent}\x1b[2m{}\x1b[22m{base}", "─".repeat(hrule_width(indent))));
            i += 1;
            continue;
        }
        // Blockquote: `> text` (nestable `> > `) → a dim gutter bar per level
        // followed by the quoted text rendered with inline markup.
        if trimmed.starts_with('>') {
            let (depth, content) = strip_quote(trimmed);
            let gutter = format!("\x1b[2m▎\x1b[22m{base} ").repeat(depth);
            out.push(format!("{indent}{gutter}{}", inline(content, base)));
            i += 1;
            continue;
        }
        // `# Header` → bold line (inline runs with bold in the base so an
        // embedded `code` span doesn't end the header style early)
        let hashes = trimmed.bytes().take_while(|&b| b == b'#').count();
        if (1..=6).contains(&hashes) {
            if let Some(title) = trimmed[hashes..].strip_prefix(' ') {
                out.push(format!(
                    "{indent}\x1b[1m{}\x1b[22m{base}",
                    inline(title, &format!("{base}\x1b[1m"))
                ));
                i += 1;
                continue;
            }
        }
        // Unordered bullet: `- `, `* `, or `+ ` → a normalized `•` marker. (A
        // lone `*word*` stays italic — the bullet form requires the space.)
        if let Some(item) = trimmed
            .strip_prefix("- ")
            .or_else(|| trimmed.strip_prefix("* "))
            .or_else(|| trimmed.strip_prefix("+ "))
        {
            out.push(format!("{indent}• {}", inline(item, base)));
            i += 1;
            continue;
        }
        // Ordered list: `1. ` / `2) ` → keep the marker, render the item inline.
        if let Some((marker, item)) = split_ordered(trimmed) {
            out.push(format!("{indent}{marker} {}", inline(item, base)));
            i += 1;
            continue;
        }
        out.push(format!("{indent}{}", inline(trimmed, base)));
        i += 1;
    }
    out.join("\n")
}

/// `|---|:--:|---:|` — only pipes, dashes, colons, and spaces.
fn is_separator_row(line: &str) -> bool {
    let t = line.trim();
    t.starts_with('|') && t.contains('-') && t.chars().all(|c| matches!(c, '|' | '-' | ':' | ' '))
}

/// A standalone horizontal rule: at least three of the SAME marker (`-`/`*`/`_`),
/// with only that marker and spaces on the line.
fn is_hrule(trimmed: &str) -> bool {
    let t = trimmed.trim_end();
    let marker = match t.chars().next() {
        Some(c @ ('-' | '*' | '_')) => c,
        _ => return false,
    };
    t.chars().filter(|&c| c == marker).count() >= 3
        && t.chars().all(|c| c == marker || c == ' ')
}

/// Display width for a horizontal rule: the terminal width (capped so a piped,
/// unbounded width doesn't emit a runaway line) minus the line's indent.
fn hrule_width(indent: &str) -> usize {
    term_width()
        .min(64)
        .saturating_sub(visible_width(indent))
        .max(3)
}

/// Peel the leading `>` quote markers off a line, returning the nesting depth and
/// the remaining content. `> > text` → (2, "text"); `>` alone → (1, "").
fn strip_quote(trimmed: &str) -> (usize, &str) {
    let mut depth = 0;
    let mut rest = trimmed;
    while let Some(r) = rest.strip_prefix('>') {
        depth += 1;
        rest = r.strip_prefix(' ').unwrap_or(r);
    }
    (depth.max(1), rest)
}

/// Split an ordered-list line into its `N.`/`N)` marker and the item text.
/// Returns None when the line isn't `<digits><'.'|')'><space>…`.
fn split_ordered(trimmed: &str) -> Option<(&str, &str)> {
    let digits = trimmed.bytes().take_while(u8::is_ascii_digit).count();
    if digits == 0 || digits > 9 {
        return None;
    }
    let after = &trimmed[digits..];
    let delim = after.as_bytes().first().copied()?;
    if delim != b'.' && delim != b')' {
        return None;
    }
    let item = after[1..].strip_prefix(' ')?;
    Some((&trimmed[..digits + 1], item))
}

#[derive(Clone, Copy)]
enum Align {
    Left,
    Right,
    Center,
}

/// Markdown table → a clean boxed table: rounded outer frame, dim `│`/`─` rules,
/// bold header, per-column alignment. Column widths use marker-stripped cell text
/// so `**bold**` cells line up. Cells that would overflow the terminal are
/// word-wrapped into narrowed columns rather than letting the row hard-wrap.
fn render_table(block: &[&str], indent: &str, base: &str, out: &mut Vec<String>) {
    let header = split_row(block[0]);
    let aligns: Vec<Align> = split_row(block[1])
        .iter()
        .map(|c| match (c.starts_with(':'), c.ends_with(':')) {
            (true, true) => Align::Center,
            (false, true) => Align::Right,
            _ => Align::Left,
        })
        .collect();
    let rows: Vec<Vec<String>> = block[2..].iter().map(|l| split_row(l)).collect();

    let ncols = rows
        .iter()
        .map(Vec::len)
        .chain([header.len()])
        .max()
        .unwrap_or(0);
    let mut natural = vec![0usize; ncols];
    for row in std::iter::once(&header).chain(rows.iter()) {
        for (c, cell) in row.iter().enumerate() {
            natural[c] = natural[c].max(visible_width(cell));
        }
    }

    // Fit to the terminal. The box frame costs a fixed number of columns per row:
    // the left "│ " and right " │" borders (4) plus a " │ " divider between each
    // pair of columns (3 each). Subtract that from the terminal width before
    // distributing the remainder to columns; `fit_widths` shrinks the widest
    // column first and `wrap_cell` word-wraps the overflow. When the table fits
    // (the common case) the natural widths are returned unchanged.
    let indent_w = visible_width(indent);
    let frame_w = 4 + 3 * ncols.saturating_sub(1);
    let avail = term_width().saturating_sub(indent_w + frame_w);
    let widths = fit_widths(&natural, avail);

    // Dim vertical border with the base style re-asserted after it.
    let vbar = format!("\x1b[2m│\x1b[22m{base}");
    let left = format!("{indent}{vbar} ");
    let mid = format!(" {vbar} ");
    let right = format!(" {vbar}");

    // One logical row → one or more physical lines (each wrapped cell line),
    // wrapped in the left/right frame borders.
    let row_lines = |cells: &[String], bold: bool| -> Vec<String> {
        let wrapped: Vec<Vec<String>> = (0..ncols)
            .map(|c| wrap_cell(cells.get(c).map(String::as_str).unwrap_or(""), widths[c]))
            .collect();
        let height = wrapped.iter().map(Vec::len).max().unwrap_or(1);
        (0..height)
            .map(|li| {
                let parts: Vec<String> = (0..ncols)
                    .map(|c| {
                        let raw = wrapped[c].get(li).map(String::as_str).unwrap_or("");
                        let rendered = if bold {
                            format!(
                                "\x1b[1m{}\x1b[22m{base}",
                                inline(raw, &format!("{base}\x1b[1m"))
                            )
                        } else {
                            inline(raw, base)
                        };
                        let pad = widths[c].saturating_sub(visible_width(raw));
                        let (l, r) = match aligns.get(c).copied().unwrap_or(Align::Left) {
                            Align::Left => (0, pad),
                            Align::Right => (pad, 0),
                            Align::Center => (pad / 2, pad - pad / 2),
                        };
                        format!("{}{rendered}{}", " ".repeat(l), " ".repeat(r))
                    })
                    .collect();
                format!("{left}{}{right}", parts.join(&mid))
            })
            .collect()
    };

    // A horizontal frame line with the given corner/junction glyphs; each column
    // segment spans its width plus the one-space cell padding on each side, so the
    // junctions land exactly under the `│` dividers.
    let divider = |lc: &str, j: &str, rc: &str| -> String {
        let segs: Vec<String> = widths.iter().map(|w| "─".repeat(w + 2)).collect();
        format!("{indent}\x1b[2m{lc}{}{rc}\x1b[22m{base}", segs.join(j))
    };

    out.push(divider("╭", "┬", "╮"));
    out.extend(row_lines(&header, true));
    out.push(divider("├", "┼", "┤"));
    for row in &rows {
        out.extend(row_lines(row, false));
    }
    out.push(divider("╰", "┴", "╯"));
}

/// Terminal width in columns for table fitting. A tty is queried via TIOCGWINSZ.
/// Otherwise (piped/captured) we honor an explicit `$COLUMNS`, and with none we
/// return a huge width so captured output is never wrapped — the wrapping then
/// happens later when a tty actually renders it.
pub(crate) fn term_width() -> usize {
    // A caller inside `render_within` / `render_pane` pins the fitting width so
    // markdown bound for a bordered pane fits the width LEFT after the gutter.
    if let Some(w) = WIDTH_OVERRIDE.with(|c| c.get()) {
        return w;
    }
    // SAFETY: isatty + a TIOCGWINSZ ioctl on stdout, both read-only.
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
        .and_then(|c| c.parse().ok())
        .unwrap_or(usize::MAX)
}

/// Shrink natural column widths to fit `avail` total display columns, taking from
/// the widest column first (down to a soft floor) so a wide free-text column
/// wraps before a narrow key column disappears. Returns the input unchanged when
/// it already fits.
fn fit_widths(natural: &[usize], avail: usize) -> Vec<usize> {
    let mut w = natural.to_vec();
    const FLOOR: usize = 8;
    while w.iter().sum::<usize>() > avail {
        let idx = (0..w.len())
            .filter(|&i| w[i] > FLOOR)
            .max_by_key(|&i| w[i])
            .or_else(|| (0..w.len()).max_by_key(|&i| w[i]));
        match idx {
            Some(i) if w[i] > 0 => w[i] -= 1,
            _ => break, // every column at 0 — nothing left to give
        }
    }
    w
}

/// Word-wrap a raw cell to `width` display columns, returning ≥1 lines. Wraps at
/// spaces; a single word longer than `width` is hard-broken by display columns.
/// Operates on the raw (markdown) text so each wrapped line still renders its own
/// inline markers — fine for the short, mostly-plain cells in shell tables.
fn wrap_cell(raw: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    if visible_width(raw) <= width {
        return vec![raw.to_string()];
    }
    let mut lines: Vec<String> = Vec::new();
    let mut cur = String::new();
    for word in raw.split_whitespace() {
        let candidate = if cur.is_empty() {
            word.to_string()
        } else {
            format!("{cur} {word}")
        };
        if visible_width(&candidate) <= width {
            cur = candidate;
        } else {
            if !cur.is_empty() {
                lines.push(std::mem::take(&mut cur));
            }
            if visible_width(word) > width {
                lines.extend(hard_break(word, width));
                cur = lines.pop().unwrap_or_default(); // keep the tail open to fill
            } else {
                cur = word.to_string();
            }
        }
    }
    if !cur.is_empty() {
        lines.push(cur);
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

/// Break a too-long word into chunks of at most `width` display columns.
fn hard_break(word: &str, width: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    for ch in word.chars() {
        if !cur.is_empty() && visible_width(&format!("{cur}{ch}")) > width {
            out.push(std::mem::take(&mut cur));
        }
        cur.push(ch);
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// Split a `| a | b |` row into trimmed cells; `\|` escapes a literal pipe.
fn split_row(line: &str) -> Vec<String> {
    let t = line.trim();
    let t = t.strip_prefix('|').unwrap_or(t);
    let t = t.strip_suffix('|').unwrap_or(t);
    let mut cells = Vec::new();
    let mut cur = String::new();
    let mut chars = t.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' if chars.peek() == Some(&'|') => {
                cur.push('|');
                chars.next();
            }
            '|' => cells.push(std::mem::take(&mut cur).trim().to_string()),
            _ => cur.push(c),
        }
    }
    cells.push(cur.trim().to_string());
    cells
}

/// Terminal display width of a cell after markdown markers are consumed and
/// ANSI stripped. Uses `unicode-width` so wide glyphs count as two columns:
/// a single-codepoint emoji (🚀) and a VS16 emoji-presentation sequence
/// (⚙️ = U+2699 U+FE0F) both measure 2, which `chars().count()` got wrong.
fn visible_width(s: &str) -> usize {
    use unicode_width::UnicodeWidthStr;
    strip_ansi(&inline(s, "")).width()
}

fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            if chars.next() == Some('[') {
                for c2 in chars.by_ref() {
                    if c2.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
            continue;
        }
        out.push(c);
    }
    out
}

/// Render for stdout: ANSI when stdout is a TTY, raw markdown when piped.
pub fn render_stdout(text: &str) -> String {
    // SAFETY: plain isatty query.
    if unsafe { libc::isatty(1) } == 1 {
        render(text, "")
    } else {
        text.to_string()
    }
}

/// Width-aware [`render_stdout`]: when stdout is a TTY, fits tables/rules within
/// `max_cols` (use for markdown that will be re-framed inside a bordered pane so
/// the gutter doesn't push rows past the edge); when piped, emits raw markdown
/// unchanged. `max_cols == usize::MAX` reproduces `render_stdout`'s full-width
/// fitting.
pub fn render_stdout_within(text: &str, max_cols: usize) -> String {
    // SAFETY: plain isatty query.
    if unsafe { libc::isatty(1) } == 1 {
        render_within(text, "", max_cols)
    } else {
        text.to_string()
    }
}


/// Inline spans: `code`, **bold**, *italic*, ~~strike~~, [text](url). Underscore
/// emphasis is skipped on purpose — it would mangle snake_case identifiers.
fn inline(s: &str, base: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < s.len() {
        let rest = &s[i..];
        if rest.starts_with('`') {
            if let Some(end) = rest[1..].find('`') {
                out.push_str("\x1b[36m");
                out.push_str(&rest[1..1 + end]);
                out.push_str("\x1b[39m");
                out.push_str(base);
                i += end + 2;
                continue;
            }
        } else if rest.starts_with('[') {
            // [label](url): show the label (cyan), keeping the url dim in parens
            // when it differs — copy-friendly in a terminal, no OSC-8 to leak.
            if let Some(consumed) = render_link(rest, base, &mut out) {
                i += consumed;
                continue;
            }
        } else if rest.starts_with("~~") {
            if let Some(end) = rest[2..].find("~~") {
                if end > 0 {
                    out.push_str("\x1b[9m");
                    out.push_str(&inline(&rest[2..2 + end], &format!("{base}\x1b[9m")));
                    out.push_str("\x1b[29m");
                    out.push_str(base);
                    i += end + 4;
                    continue;
                }
            }
        } else if rest.starts_with("**") {
            if let Some(end) = rest[2..].find("**") {
                if end > 0 {
                    out.push_str("\x1b[1m");
                    out.push_str(&rest[2..2 + end]);
                    out.push_str("\x1b[22m");
                    out.push_str(base);
                    i += end + 4;
                    continue;
                }
            }
        } else if rest.starts_with('*') {
            // require non-space inner edges so `2 * 3` stays literal
            if let Some(end) = rest[1..].find('*') {
                let span = &rest[1..1 + end];
                if !span.is_empty() && !span.starts_with(' ') && !span.ends_with(' ') {
                    out.push_str("\x1b[3m");
                    out.push_str(span);
                    out.push_str("\x1b[23m");
                    i += end + 2;
                    continue;
                }
            }
        }
        let ch = rest.chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// Render a `[label](url)` link starting at `rest`, appending to `out`. Returns
/// the number of bytes consumed, or None when `rest` isn't a well-formed link (so
/// the caller leaves the literal `[` in place). The label renders cyan; a url
/// that differs from the label trails dim in parentheses.
fn render_link(rest: &str, base: &str, out: &mut String) -> Option<usize> {
    let close = rest.find(']')?;
    if !rest[close + 1..].starts_with('(') {
        return None;
    }
    let url_start = close + 2;
    let url_end = url_start + rest[url_start..].find(')')?;
    let label = &rest[1..close];
    let url = &rest[url_start..url_end];
    if label.is_empty() || url.is_empty() {
        return None;
    }
    out.push_str("\x1b[36m");
    out.push_str(&inline(label, &format!("{base}\x1b[36m")));
    out.push_str("\x1b[39m");
    out.push_str(base);
    if url != label {
        out.push_str(&format!(" \x1b[2m({url})\x1b[22m{base}"));
    }
    Some(url_end + 1)
}

#[cfg(test)]
mod tests {
    use super::render;
    use super::{
        fit_widths, render_pane, render_within, visible_width, wrap_cell, DEFAULT_PANE_COLS,
        PANE_GUTTER_COLS,
    };

    // Serializes every test that mutates the process-global `COLUMNS` env var.
    // The test runner executes in parallel, so without this one test's
    // `set_var("COLUMNS", ..)` could race another's `remove_var` between the set
    // and the `render()` call — `term_width()` would then read the wrong width
    // and the wrap/frame assertions flake (see `table_word_wraps_when_narrow`).
    // Poison-tolerant: an assertion panic while the guard is held must not
    // cascade-fail sibling tests, so we recover the guard from a poisoned lock.
    static COLUMNS_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn lock_columns() -> std::sync::MutexGuard<'static, ()> {
        COLUMNS_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    #[test]
    fn fit_widths_shrinks_widest_first() {
        // Already fits → unchanged.
        assert_eq!(fit_widths(&[5, 10, 8], 100), vec![5, 10, 8]);
        // Overflow → the widest column gives until it fits; narrow columns kept.
        let w = fit_widths(&[6, 40, 8], 30);
        assert!(w.iter().sum::<usize>() <= 30);
        assert_eq!(w[0], 6); // narrow key column preserved (above/at floor untouched)
        assert!(w[1] < 40); // wide free-text column shrank
    }

    #[test]
    fn wrap_cell_word_wraps_to_width() {
        let lines = wrap_cell("the quick brown fox jumps", 10);
        assert!(lines.len() > 1);
        assert!(lines.iter().all(|l| visible_width(l) <= 10));
        // Reassembling the words round-trips (wrapping only inserts breaks).
        assert_eq!(
            lines.join(" ").split_whitespace().collect::<Vec<_>>(),
            "the quick brown fox jumps"
                .split_whitespace()
                .collect::<Vec<_>>()
        );
        // A short cell stays one line.
        assert_eq!(wrap_cell("short", 10), vec!["short"]);
        // A single over-long word is hard-broken, never dropped.
        let hb = wrap_cell("supercalifragilistic", 6);
        assert!(hb.iter().all(|l| visible_width(l) <= 6));
        assert_eq!(hb.concat(), "supercalifragilistic");
    }

    #[test]
    fn table_word_wraps_when_narrow() {
        // A table whose natural width exceeds COLUMNS should produce more lines
        // than rows (cells wrapped), with no physical line exceeding the width.
        // term_width() falls back to $COLUMNS when stdout isn't a tty (tests).
        let _cols = lock_columns();
        unsafe { std::env::set_var("COLUMNS", "32") };
        let md = "| Key | Note |\n|---|---|\n| a | one two three four five six seven |";
        let out = render(md, "");
        let lines: Vec<&str> = out.lines().collect();
        // framed (top + header + mid + ≥2 wrapped body lines + bottom)
        assert!(lines.len() >= 6, "expected wrapped+framed rows, got {lines:#?}");
        unsafe { std::env::remove_var("COLUMNS") };
    }

    #[test]
    fn render_within_bounds_table_below_terminal_width() {
        // A table whose natural width dwarfs the budget must be fit to the
        // budget regardless of the real terminal width — this is what keeps a
        // coordinator's forwarded table from overflowing the pane gutter and
        // hard-wrapping. Every physical line must be ≤ the requested width.
        let _cols = lock_columns();
        unsafe { std::env::set_var("COLUMNS", "500") }; // wide "terminal"
        let md = "| Test run | Trigger | What it proves |\n|---|---|---|\n| Reuse path | workflow_dispatch default linux on a commit with a matching release | The setup job detects the existing CI build and skips recompilation entirely |";
        let out = render_within(md, "", 60);
        for line in out.lines() {
            assert!(
                visible_width(line) <= 60,
                "line exceeds 60 cols: {} → {line:?}",
                visible_width(line)
            );
        }
        // Sanity: without the bound the same table blows past 60 (proving the
        // override, not a coincidentally-narrow table, did the work).
        assert!(render(md, "").lines().any(|l| visible_width(l) > 60));
        unsafe { std::env::remove_var("COLUMNS") };
    }

    #[test]
    fn render_pane_reserves_gutter_and_falls_back_when_width_unknown() {
        // No terminal + no $COLUMNS → render_pane uses its bounded default
        // (never usize::MAX), so a forwarded table is never rendered
        // unbounded-wide. Every line fits the default minus the gutter reserve.
        let _cols = lock_columns();
        unsafe { std::env::remove_var("COLUMNS") };
        let md = "| A | B |\n|---|---|\n| one two three four five | six seven eight nine ten eleven twelve |";
        let out = render_pane(md, "");
        let budget = DEFAULT_PANE_COLS - PANE_GUTTER_COLS;
        for line in out.lines() {
            assert!(
                visible_width(line) <= budget,
                "pane line exceeds {budget}: {line:?}"
            );
        }
    }

    #[test]
    fn bold_italic_code() {
        assert_eq!(
            render("it's **09:18:48 AM** in *New York*", ""),
            "it's \x1b[1m09:18:48 AM\x1b[22m in \x1b[3mNew York\x1b[23m"
        );
        assert_eq!(
            render("run `ls -la` now", ""),
            "run \x1b[36mls -la\x1b[39m now"
        );
    }

    #[test]
    fn strikethrough_and_links() {
        // ~~strike~~ → SGR 9/29, with inner inline markup still honored.
        assert_eq!(
            render("~~gone~~ now", ""),
            "\x1b[9mgone\x1b[29m now"
        );
        // [label](url): cyan label + dim (url) when they differ.
        assert_eq!(
            render("see [docs](https://x.io)", ""),
            "see \x1b[36mdocs\x1b[39m \x1b[2m(https://x.io)\x1b[22m"
        );
        // label == url → no duplicate parenthetical.
        assert_eq!(
            render("[https://x.io](https://x.io)", ""),
            "\x1b[36mhttps://x.io\x1b[39m"
        );
        // Malformed link stays literal.
        assert_eq!(render("[oops] not a link", ""), "[oops] not a link");
    }

    #[test]
    fn literals_stay_literal() {
        assert_eq!(render("2 * 3 = 6", ""), "2 * 3 = 6"); // spaced star ≠ italic
        assert_eq!(render("a ** b", ""), "a ** b"); // unmatched/empty bold
        assert_eq!(render("snake_case_name", ""), "snake_case_name");
    }

    #[test]
    fn blocks() {
        assert_eq!(render("# Title", ""), "\x1b[1mTitle\x1b[22m");
        assert_eq!(render("- item", ""), "• item");
        // Alternate bullet markers normalize to •.
        assert_eq!(render("* star", ""), "• star");
        assert_eq!(render("+ plus", ""), "• plus");
        // Ordered list keeps its marker, renders the item inline.
        assert_eq!(render("1. first", ""), "1. first");
        assert_eq!(render("3) third", ""), "3) third");
        assert_eq!(
            render("```\n**raw**\n```", ""),
            "\x1b[2m```\x1b[22m\n**raw**\n\x1b[2m```\x1b[22m"
        );
    }

    #[test]
    fn horizontal_rule() {
        // A standalone rule renders as a dim line of ─; width is bounded.
        let _cols = lock_columns();
        unsafe { std::env::set_var("COLUMNS", "20") };
        for src in ["---", "***", "___", "- - -"] {
            let out = render(src, "");
            assert!(out.starts_with("\x1b[2m"), "{src} → {out:?}");
            assert!(out.contains('─'), "{src} → {out:?}");
            assert!(!out.contains('-'), "rule should not echo dashes: {out:?}");
        }
        unsafe { std::env::remove_var("COLUMNS") };
    }

    #[test]
    fn blockquote() {
        // `> text` → dim gutter + inline-rendered content.
        assert_eq!(
            render("> heads up", ""),
            "\x1b[2m▎\x1b[22m heads up"
        );
        // Nested `> > ` → two gutters; inner markup still renders.
        assert_eq!(
            render("> > **bang**", ""),
            "\x1b[2m▎\x1b[22m \x1b[2m▎\x1b[22m \x1b[1mbang\x1b[22m"
        );
    }

    #[test]
    fn dim_base_reasserted_after_bold() {
        assert_eq!(
            render("a **b** c", "\x1b[2m"),
            "a \x1b[1mb\x1b[22m\x1b[2m c"
        );
    }

    #[test]
    fn table_is_boxed_and_aligned() {
        // term_width() falls back to $COLUMNS off-tty; keep it wide so the small
        // table renders at its natural widths.
        let _cols = lock_columns();
        unsafe { std::env::set_var("COLUMNS", "200") };
        let out = render(
            "| Sprint | Pts |\n|---|---:|\n| SPR-036 | 16 |\n| **S2** | 5 |",
            "",
        );
        let lines: Vec<&str> = out.lines().collect();
        // top + header + mid + 2 body + bottom = 6 framed lines.
        assert_eq!(lines.len(), 6, "{lines:#?}");
        let stripped: Vec<String> = lines.iter().map(|l| super::strip_ansi(l)).collect();
        // Rounded corners on the outer frame.
        assert!(stripped[0].starts_with('╭') && stripped[0].ends_with('╮'));
        assert!(stripped[2].starts_with('├') && stripped[2].ends_with('┤'));
        assert!(stripped[5].starts_with('╰') && stripped[5].ends_with('╯'));
        // Every physical line is the same display width (the frame lines up).
        let w = |l: &str| super::visible_width(l);
        let width0 = w(&stripped[0]);
        for s in &stripped {
            assert_eq!(w(s), width0, "ragged frame: {s:?}");
        }
        // Header is bold, body rows carry the dim │ border.
        assert!(lines[1].contains("\x1b[1mSprint\x1b[22m"));
        assert!(lines[3].contains("\x1b[2m│\x1b[22m"));
        // Right-aligned numeric column: the cell text hugs the right border.
        assert!(stripped[3].contains("16 │"));
        assert!(stripped[4].contains(" 5 │"));
        // Bold cell padded by its stripped width (2), not its raw width (6).
        assert!(stripped[4].contains("│ S2      │"));
        unsafe { std::env::remove_var("COLUMNS") };
    }

    #[test]
    fn table_aligns_emoji_column() {
        // Mixes a single-codepoint emoji (🚀, 1 char / 2 cols) with a VS16
        // emoji-presentation sequence (⚙️ = U+2699 U+FE0F, 2 chars / 2 cols):
        // counting chars would under-pad 🚀 by one and the column would ragged.
        let _cols = lock_columns();
        unsafe { std::env::set_var("COLUMNS", "200") };
        let out = render(
            "| Emoji | Module |\n|---|---|\n| 🚀 | main.rs |\n| ⚙️ | engine.rs |",
            "",
        );
        let lines: Vec<&str> = out.lines().collect();
        // top + header + mid + 2 body + bottom.
        assert_eq!(lines.len(), 6);
        // Display width via the same unicode-width measure render uses.
        let w = |l: &str| super::visible_width(&super::strip_ansi(l));
        let header = w(lines[0]);
        for l in &lines {
            assert_eq!(w(l), header, "row {l:?} display width != header");
        }
        // Every `│`/junction glyph must land at the same display column on every
        // row — the actual symptom of the bug. Measure each separator's column as
        // the display width of the text preceding it (char-by-char would mis-split
        // the ⚙️ VS16 sequence).
        let sep_cols = |l: &str| -> Vec<usize> {
            let stripped = super::strip_ansi(l);
            stripped
                .char_indices()
                .filter(|&(_, ch)| matches!(ch, '│' | '┼' | '┬' | '┴'))
                .map(|(i, _)| super::visible_width(&stripped[..i]))
                .collect::<Vec<_>>()
        };
        // Body/header rows carry the same separator columns as the header row.
        let expected = sep_cols(lines[1]);
        for l in &[lines[1], lines[3], lines[4]] {
            assert_eq!(sep_cols(l), expected, "separator misaligned in {l:?}");
        }
        unsafe { std::env::remove_var("COLUMNS") };
    }

    #[test]
    fn pipe_line_without_separator_is_not_a_table() {
        assert_eq!(render("| just text |", ""), "| just text |");
    }

    #[test]
    fn escaped_pipe_stays_in_cell() {
        let _cols = lock_columns();
        unsafe { std::env::set_var("COLUMNS", "200") };
        let out = render("| Cmd |\n|---|\n| a \\| b |", "");
        assert!(out.contains("a | b"));
        unsafe { std::env::remove_var("COLUMNS") };
    }
}
