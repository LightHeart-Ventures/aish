/// Minimal markdown → ANSI for model replies. Models speak markdown even when
/// asked not to; rather than fight it, render the small subset they actually
/// emit in shell answers: **bold**, *italic*, `code`, # headers, - bullets,
/// and ``` fences. Not a spec parser — anything unmatched stays literal.
///
/// `base` is the style to re-assert after a reset (e.g. "\x1b[2m" when the
/// caller prints the whole line dim) — SGR 22 turns off bold *and* dim, so a
/// plain bold-off would otherwise un-dim the rest of the line.
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
        if let Some(item) = trimmed.strip_prefix("- ") {
            out.push(format!("{indent}• {}", inline(item, base)));
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
    t.starts_with('|')
        && t.contains('-')
        && t.chars().all(|c| matches!(c, '|' | '-' | ':' | ' '))
}

#[derive(Clone, Copy)]
enum Align {
    Left,
    Right,
    Center,
}

/// Markdown table → aligned columns with dim `│`/`─` rules, bold header.
/// Column widths use marker-stripped cell text so `**bold**` cells line up.
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

    let ncols = rows.iter().map(Vec::len).chain([header.len()]).max().unwrap_or(0);
    let mut natural = vec![0usize; ncols];
    for row in std::iter::once(&header).chain(rows.iter()) {
        for (c, cell) in row.iter().enumerate() {
            natural[c] = natural[c].max(visible_width(cell));
        }
    }

    // Fit to the terminal: if the natural table would overflow the width, shrink
    // the widest columns and WORD-WRAP their cells into the narrowed columns —
    // multi-line rows with aligned `│` rules — rather than letting the terminal
    // hard-wrap whole rows (which mangles the column structure). When it already
    // fits (the common case), `fit_widths` returns the natural widths unchanged
    // and every cell wraps to a single line, so output is identical to before.
    let indent_w = visible_width(indent);
    let sep_w = 3 * ncols.saturating_sub(1); // " │ " between columns ≈ 3 cols
    let avail = term_width().saturating_sub(indent_w + sep_w);
    let widths = fit_widths(&natural, avail);

    let bar = format!(" \x1b[2m│\x1b[22m{base} ");
    // One logical row → one or more physical lines (each wrapped cell line).
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
                            format!("\x1b[1m{}\x1b[22m{base}", inline(raw, &format!("{base}\x1b[1m")))
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
                format!("{indent}{}", parts.join(&bar))
            })
            .collect()
    };

    out.extend(row_lines(&header, true));
    let rule: Vec<String> = widths.iter().map(|w| "─".repeat(*w)).collect();
    out.push(format!("{indent}\x1b[2m{}\x1b[22m{base}", rule.join("─┼─")));
    for row in &rows {
        out.extend(row_lines(row, false));
    }
}

/// Terminal width in columns for table fitting. A tty is queried via TIOCGWINSZ.
/// Otherwise (piped/captured) we honor an explicit `$COLUMNS`, and with none we
/// return a huge width so captured output is never wrapped — the wrapping then
/// happens later when a tty actually renders it.
fn term_width() -> usize {
    // SAFETY: isatty + a TIOCGWINSZ ioctl on stdout, both read-only.
    unsafe {
        if libc::isatty(1) == 1 {
            let mut ws: libc::winsize = std::mem::zeroed();
            if libc::ioctl(1, libc::TIOCGWINSZ, &mut ws) == 0 && ws.ws_col > 0 {
                return ws.ws_col as usize;
            }
        }
    }
    std::env::var("COLUMNS").ok().and_then(|c| c.parse().ok()).unwrap_or(usize::MAX)
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
        let candidate = if cur.is_empty() { word.to_string() } else { format!("{cur} {word}") };
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

/// Inline spans: `code`, **bold**, *italic*. Underscore emphasis is skipped
/// on purpose — it would mangle snake_case identifiers.
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

#[cfg(test)]
mod tests {
    use super::render;
    use super::{fit_widths, visible_width, wrap_cell};

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
        assert_eq!(lines.join(" ").split_whitespace().collect::<Vec<_>>(),
                   "the quick brown fox jumps".split_whitespace().collect::<Vec<_>>());
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
        unsafe { std::env::set_var("COLUMNS", "32") };
        let md = "| Key | Note |\n|---|---|\n| a | one two three four five six seven |";
        let out = render(md, "");
        let lines: Vec<&str> = out.lines().collect();
        // header + rule + ≥2 wrapped body lines
        assert!(lines.len() >= 4, "expected wrapped rows, got {lines:#?}");
        unsafe { std::env::remove_var("COLUMNS") };
    }

    #[test]
    fn bold_italic_code() {
        assert_eq!(
            render("it's **09:18:48 AM** in *New York*", ""),
            "it's \x1b[1m09:18:48 AM\x1b[22m in \x1b[3mNew York\x1b[23m"
        );
        assert_eq!(render("run `ls -la` now", ""), "run \x1b[36mls -la\x1b[39m now");
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
        assert_eq!(render("```\n**raw**\n```", ""), "\x1b[2m```\x1b[22m\n**raw**\n\x1b[2m```\x1b[22m");
    }

    #[test]
    fn dim_base_reasserted_after_bold() {
        assert_eq!(render("a **b** c", "\x1b[2m"), "a \x1b[1mb\x1b[22m\x1b[2m c");
    }

    #[test]
    fn table_aligns_columns() {
        let out = render("| Sprint | Pts |\n|---|---:|\n| SPR-036 | 16 |\n| **S2** | 5 |", "");
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 4);
        // header bold, dim │ separator
        assert_eq!(lines[0], "\x1b[1mSprint\x1b[22m  \x1b[2m│\x1b[22m \x1b[1mPts\x1b[22m");
        assert_eq!(lines[1], "\x1b[2m────────┼────\x1b[22m");
        // all rows have equal visible width: pad by marker-stripped cell text
        let w = |l: &str| super::strip_ansi(l).chars().count();
        assert_eq!(w(lines[0]), w(lines[1]));
        assert_eq!(w(lines[1]), w(lines[2]));
        assert_eq!(w(lines[2]), w(lines[3]));
        // right-aligned numeric column
        assert!(super::strip_ansi(lines[2]).ends_with(" 16"));
        assert!(super::strip_ansi(lines[3]).ends_with("  5"));
        // bold cell padded by its stripped width (2), not its raw width (6)
        assert!(super::strip_ansi(lines[3]).starts_with("S2      "));
    }

    #[test]
    fn table_aligns_emoji_column() {
        // Mixes a single-codepoint emoji (🚀, 1 char / 2 cols) with a VS16
        // emoji-presentation sequence (⚙️ = U+2699 U+FE0F, 2 chars / 2 cols):
        // counting chars would under-pad 🚀 by one and the column would ragged.
        let out = render(
            "| Emoji | Module |\n|---|---|\n| 🚀 | main.rs |\n| ⚙️ | engine.rs |",
            "",
        );
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 4);
        // Display width via the same unicode-width measure render uses.
        let w = |l: &str| super::visible_width(&super::strip_ansi(l));
        let header = w(lines[0]);
        for l in &lines {
            assert_eq!(w(l), header, "row {l:?} display width != header");
        }
        // The `│` / `┼` separators must land at the same display column on
        // every row — the actual symptom of the bug. Measure each separator's
        // column as the display width of the text preceding it (measuring
        // char-by-char would mis-split the ⚙️ VS16 sequence).
        let sep_cols = |l: &str| -> Vec<usize> {
            let stripped = super::strip_ansi(l);
            stripped
                .char_indices()
                .filter(|&(_, ch)| ch == '│' || ch == '┼')
                .map(|(i, _)| super::visible_width(&stripped[..i]))
                .collect::<Vec<_>>()
        };
        let expected = sep_cols(lines[0]);
        assert_eq!(expected.len(), 1, "one separator per row");
        for l in &lines {
            assert_eq!(sep_cols(l), expected, "separator misaligned in {l:?}");
        }
    }

    #[test]
    fn pipe_line_without_separator_is_not_a_table() {
        assert_eq!(render("| just text |", ""), "| just text |");
    }

    #[test]
    fn escaped_pipe_stays_in_cell() {
        let out = render("| Cmd |\n|---|\n| a \\| b |", "");
        assert!(out.contains("a | b"));
    }
}
