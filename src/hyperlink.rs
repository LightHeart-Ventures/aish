//! OSC 8 terminal hyperlinks — makes URLs in aish output clickable.
//!
//! Modern terminals (iTerm2, WezTerm, Kitty, Ghostty, Windows Terminal,
//! VS Code, GNOME Terminal, Alacritty ≥ 0.11, foot, Konsole) implement the
//! OSC 8 escape:
//!
//! ```text
//! ESC ] 8 ; ; <url> ESC \   <visible text>   ESC ] 8 ; ; ESC \
//! ```
//!
//! The visible text is unchanged, so column widths/alignment are unaffected —
//! the terminal just makes the span click/⌘-click-able. Terminals that don't
//! know OSC 8 swallow the sequence (it's a well-formed OSC string), so the
//! worst case on an unknown-but-conformant terminal is an invisible no-op.
//!
//! Two things make that safe in practice:
//!   • we only emit when [`style::colors_enabled`] already said "yes, this is a
//!     terminal we're painting" (TTY, no `NO_COLOR`, no `--no-color`), and
//!   • a deny-list plus the `AISH_HYPERLINKS` env override gives an escape
//!     hatch for the terminals we know mangle or drop it (Apple Terminal has
//!     never implemented OSC 8 — on that one a hyperlinked label would hide
//!     the URL with nothing to click).
//!
//! `AISH_HYPERLINKS=0|false|off|never` forces plain text; `1|true|on|always`
//! forces hyperlinks even on a deny-listed terminal.
//!
//! Callers ask [`enabled`] once and then use [`open`]/[`close`] (or the
//! all-in-one [`wrap`]). Width-measuring code must skip OSC strings — see
//! `md::strip_ansi` and `style::visible_cols`, both of which do.

use std::sync::OnceLock;

/// Opens an OSC 8 hyperlink: `ESC ] 8 ; ; <url>` + ST. The empty middle field
/// is the (optional) `id=` parameter list, which we don't use.
const OSC8_OPEN: &str = "\x1b]8;;";
/// String Terminator. `ESC \` is preferred over BEL (`\x07`): BEL is the older
/// xterm convention and some terminals ring the bell instead of terminating.
const ST: &str = "\x1b\\";
/// Closing sequence: an OSC 8 with an empty URL field ends the hyperlink span.
const OSC8_CLOSE: &str = "\x1b]8;;\x1b\\";

/// Cached `AISH_HYPERLINKS` / terminal-support decision. Resolved once — the
/// environment doesn't change under a running shell, and `enabled()` sits on
/// the per-character path of the markdown renderer.
static SUPPORTED: OnceLock<bool> = OnceLock::new();

/// Whether to emit OSC 8 hyperlinks right now.
///
/// Rides on [`crate::style::colors_enabled`] (so `--no-color`, `NO_COLOR`, and
/// a piped stdout all keep escapes out of the stream — a hyperlink is an escape
/// sequence like any other) and then applies the terminal-support decision.
pub fn enabled() -> bool {
    crate::style::colors_enabled() && terminal_supported()
}

/// The terminal half of the [`enabled`] decision, cached. Split out so the
/// color/TTY state stays dynamic (tests and `--no-color` flip it at runtime)
/// while the env sniffing happens once.
fn terminal_supported() -> bool {
    *SUPPORTED.get_or_init(|| {
        match std::env::var("AISH_HYPERLINKS")
            .ok()
            .as_deref()
            .map(str::trim)
        {
            Some("0" | "false" | "off" | "never" | "no") => return false,
            Some("1" | "true" | "on" | "always" | "yes") => return true,
            _ => {}
        }
        // `TERM=dumb` is the documented "no escape sequences" contract.
        if matches!(std::env::var("TERM").as_deref(), Ok("dumb") | Ok("")) {
            return false;
        }
        // Apple Terminal (Terminal.app) has never implemented OSC 8: it drops
        // the sequence AND we'd have dropped the visible URL, leaving nothing
        // to click or copy. Opt it out rather than degrade it.
        if std::env::var("TERM_PROGRAM").as_deref() == Ok("Apple_Terminal") {
            return false;
        }
        true
    })
}

/// Opening half of a hyperlink. `url` is sanitized — see [`sanitize`].
pub fn open(url: &str) -> String {
    format!("{OSC8_OPEN}{}{ST}", sanitize(url))
}

/// Closing half of a hyperlink: an OSC 8 with an empty URL ends the span.
pub fn close() -> &'static str {
    OSC8_CLOSE
}

/// Wrap `text` as a hyperlink to `url`. The visible text is emitted verbatim,
/// so this never changes the rendered column width.
pub fn wrap(url: &str, text: &str) -> String {
    format!("{}{text}{}", open(url), close())
}

/// Strip anything from a URL that could break out of the OSC string and inject
/// arbitrary escapes into the user's terminal: C0/C1 controls (ESC, BEL,
/// newline, …) and the DEL byte. Hostile model output and scraped page text
/// both reach this function, so it is the security boundary for OSC 8 — never
/// interpolate a raw URL into the escape.
fn sanitize(url: &str) -> String {
    url.chars()
        .filter(|c| !c.is_control() && *c != '\u{7f}' && (*c as u32) > 0x1f)
        .collect()
}

/// Length in bytes of the URL at the START of `s`, or `None` when `s` doesn't
/// begin with one. Used to autolink bare URLs in prose.
///
/// Deliberately conservative: only the schemes a shell answer actually carries,
/// and trailing sentence punctuation is excluded so `see https://x.io.` links
/// `https://x.io` and leaves the full stop as text. Parens are balanced, so a
/// wiki-style `https://e.org/Foo_(bar)` keeps its closing paren while a URL
/// written inside `(https://x.io)` does not swallow the wrapper's.
pub fn url_len(s: &str) -> Option<usize> {
    const SCHEMES: [&str; 4] = ["https://", "http://", "mailto:", "file://"];
    // Cheap first-byte reject: `url_len` is called at every byte offset of the
    // inline scanner, and almost none of them start a URL.
    if !matches!(s.as_bytes().first(), Some(b'h' | b'm' | b'f')) {
        return None;
    }
    let scheme = SCHEMES.iter().find(|p| s.starts_with(**p))?;
    let mut end = s.len();
    for (idx, c) in s.char_indices() {
        // Whitespace, markdown/quote delimiters, and controls end the URL.
        if c.is_whitespace()
            || c.is_control()
            || matches!(c, '<' | '>' | '"' | '`' | '\'' | '|' | '\\' | '*')
        {
            end = idx;
            break;
        }
    }
    // Nothing after the scheme → not a URL (`https://` alone).
    if end <= scheme.len() {
        return None;
    }
    let mut url = &s[..end];
    // Trailing punctuation is almost always the sentence, not the URL.
    loop {
        let trimmed = url.trim_end_matches(['.', ',', ';', ':', '!', '?']);
        let trimmed = match trimmed.strip_suffix([')', ']', '}']) {
            // Keep a closing bracket only when the URL opened it itself.
            Some(short) => {
                let (open_c, close_c) = match trimmed.as_bytes()[trimmed.len() - 1] {
                    b')' => ('(', ')'),
                    b']' => ('[', ']'),
                    _ => ('{', '}'),
                };
                if short.matches(open_c).count() >= short.matches(close_c).count() + 1 {
                    trimmed
                } else {
                    short
                }
            }
            None => trimmed,
        };
        if trimmed.len() == url.len() {
            break;
        }
        url = trimmed;
    }
    if url.len() <= scheme.len() {
        return None;
    }
    Some(url.len())
}

#[cfg(test)]
mod tests {
    use super::{close, open, sanitize, url_len, wrap};

    #[test]
    fn wrap_emits_osc8_around_unchanged_text() {
        assert_eq!(
            wrap("https://x.io", "docs"),
            "\x1b]8;;https://x.io\x1b\\docs\x1b]8;;\x1b\\"
        );
        // The visible text is byte-identical to the input — the invariant that
        // keeps table/column widths correct.
        let w = wrap("https://x.io", "docs");
        assert!(w.contains("docs"));
        assert!(w.starts_with(&open("https://x.io")));
        assert!(w.ends_with(close()));
    }

    #[test]
    fn sanitize_strips_escape_injection() {
        // A URL carrying ESC/BEL/newline could close the OSC string early and
        // inject arbitrary SGR or even another OSC — the whole reason sanitize
        // exists. All controls must be gone.
        let evil = "https://x.io/\x1b]8;;file:///etc/passwd\x1b\\\x07\nrest";
        let clean = sanitize(evil);
        assert!(!clean.contains('\x1b'));
        assert!(!clean.contains('\x07'));
        assert!(!clean.contains('\n'));
        assert!(clean.starts_with("https://x.io/"));
        assert!(!wrap(evil, "x").contains("\x1b]8;;file"));
    }

    #[test]
    fn url_len_matches_supported_schemes() {
        assert_eq!(url_len("https://x.io"), Some(12));
        assert_eq!(url_len("http://x.io/a?b=c#d"), Some(19));
        assert_eq!(url_len("mailto:a@b.io"), Some(13));
        assert_eq!(url_len("file:///tmp/x"), Some(13));
        // Not a URL / unsupported scheme / bare scheme.
        assert_eq!(url_len("ftp://x.io"), None);
        assert_eq!(url_len("hello world"), None);
        assert_eq!(url_len("https://"), None);
    }

    #[test]
    fn url_len_stops_at_delimiters_and_punctuation() {
        // Whitespace ends it.
        assert_eq!(url_len("https://x.io then more"), Some(12));
        // Sentence punctuation is not part of the URL.
        assert_eq!(
            &"https://x.io."[..url_len("https://x.io.").unwrap()],
            "https://x.io"
        );
        assert_eq!(
            &"https://x.io/a,"[..url_len("https://x.io/a,").unwrap()],
            "https://x.io/a"
        );
        // Markdown/quote delimiters end it so `*https://x.io*` doesn't eat the star.
        assert_eq!(
            &"https://x.io*"[..url_len("https://x.io*").unwrap()],
            "https://x.io"
        );
        assert_eq!(
            &"https://x.io\"x"[..url_len("https://x.io\"x").unwrap()],
            "https://x.io"
        );
    }

    #[test]
    fn url_len_balances_parens() {
        // Wrapper paren is NOT part of the URL …
        let s = "https://x.io/a)";
        assert_eq!(&s[..url_len(s).unwrap()], "https://x.io/a");
        // … but a paren the URL itself opened is kept (wiki-style links).
        let s = "https://e.org/Foo_(bar)";
        assert_eq!(&s[..url_len(s).unwrap()], "https://e.org/Foo_(bar)");
    }
}
