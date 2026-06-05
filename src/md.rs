/// Minimal markdown → ANSI for model replies. Models speak markdown even when
/// asked not to; rather than fight it, render the small subset they actually
/// emit in shell answers: **bold**, *italic*, `code`, # headers, - bullets,
/// and ``` fences. Not a spec parser — anything unmatched stays literal.
///
/// `base` is the style to re-assert after a reset (e.g. "\x1b[2m" when the
/// caller prints the whole line dim) — SGR 22 turns off bold *and* dim, so a
/// plain bold-off would otherwise un-dim the rest of the line.
pub fn render(text: &str, base: &str) -> String {
    let mut out = String::with_capacity(text.len() + 32);
    let mut in_fence = false;
    for (idx, line) in text.lines().enumerate() {
        if idx > 0 {
            out.push('\n');
        }
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") {
            in_fence = !in_fence;
            out.push_str("\x1b[2m");
            out.push_str(line);
            out.push_str("\x1b[22m");
            out.push_str(base);
            continue;
        }
        if in_fence {
            // code verbatim — no inline markup inside fences
            out.push_str(line);
            continue;
        }
        let indent = &line[..line.len() - trimmed.len()];
        // `# Header` → bold line (inline runs with bold in the base so an
        // embedded `code` span doesn't end the header style early)
        let hashes = trimmed.bytes().take_while(|&b| b == b'#').count();
        if (1..=6).contains(&hashes) {
            if let Some(title) = trimmed[hashes..].strip_prefix(' ') {
                out.push_str(indent);
                out.push_str("\x1b[1m");
                out.push_str(&inline(title, &format!("{base}\x1b[1m")));
                out.push_str("\x1b[22m");
                out.push_str(base);
                continue;
            }
        }
        if let Some(item) = trimmed.strip_prefix("- ") {
            out.push_str(indent);
            out.push_str("• ");
            out.push_str(&inline(item, base));
            continue;
        }
        out.push_str(indent);
        out.push_str(&inline(trimmed, base));
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
}
