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
    let mut widths = vec![0usize; ncols];
    for row in std::iter::once(&header).chain(rows.iter()) {
        for (c, cell) in row.iter().enumerate() {
            widths[c] = widths[c].max(visible_width(cell));
        }
    }

    let bar = format!(" \x1b[2m│\x1b[22m{base} ");
    let fmt_row = |cells: &[String], bold: bool| -> String {
        let parts: Vec<String> = (0..ncols)
            .map(|c| {
                let raw = cells.get(c).map(String::as_str).unwrap_or("");
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
    };

    out.push(fmt_row(&header, true));
    let rule: Vec<String> = widths.iter().map(|w| "─".repeat(*w)).collect();
    out.push(format!("{indent}\x1b[2m{}\x1b[22m{base}", rule.join("─┼─")));
    for row in &rows {
        out.push(fmt_row(row, false));
    }
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
