//! aish's first-class diagnostic surface (S7.1 / TASK-139).
//!
//! Before this module, aish had no error type of its own: parse/routing
//! failures collapsed to `Option::None` and the line silently routed to the
//! model, malformed `~/.aishrc` lines were dropped with ad-hoc dim `eprintln!`s
//! (no location, no code), and exec misses were flat strings. A mistyped quote
//! was indistinguishable from genuine intent.
//!
//! [`AishDiagnostic`] is a single [`miette::Diagnostic`] enum giving aish a
//! proper diagnostic surface — a byte-span caret, a stable `aish::…` code, and a
//! did-you-mean `help:` line — across the three error families that benefit:
//! **parse** (the forced-shell tokenizer), **config** (`~/.aishrc`), and **exec**
//! (command-not-found). It deliberately does NOT change routing semantics: the
//! silent route-to-model fallback is untouched (`rc::tokenize` is still a
//! `.ok()` shim over the diagnosed tokenizer), so diagnostics render only at the
//! explicit forced-shell (`!`), rc-load, and exec sites.
//!
//! Rendering goes through [`render`]/[`eprint`], which pick miette's themed
//! graphical handler — unicode+color when [`crate::style::colors_enabled`] says
//! so, plain (no ANSI) otherwise — so `NO_COLOR`/`--no-color`/a piped stdout
//! still get the caret, code, and help, just without escape codes.

use miette::{Diagnostic, GraphicalReportHandler, GraphicalTheme, NamedSource, SourceSpan};
use thiserror::Error;

/// The header aish gives the source snippet for a `~/.aishrc` diagnostic — the
/// file name plus the 1-based line number (`~/.aishrc:42`), the way a compiler
/// names a location.
fn rc_header(line_no: usize) -> String {
    format!("~/.aishrc:{line_no}")
}

/// One aish diagnostic. Every variant carries a stable `aish::…` code; the
/// parse/config variants also carry the offending source string and a
/// [`SourceSpan`] caret at the byte offset of the offending character. The exec
/// variant has no span (the failure is a missing program, not a location) but
/// carries an optional did-you-mean `help:` hint.
#[derive(Debug, Error, Diagnostic)]
pub enum AishDiagnostic {
    /// A quote (`'` or `"`) is opened and never closed. The caret sits on the
    /// opening quote. Codes: `aish::parse::unbalanced_quote`.
    #[error("unbalanced quote")]
    #[diagnostic(
        code(aish::parse::unbalanced_quote),
        help("close the quote — or, if you meant prose (an apostrophe in English), drop the `!` so the line routes to the model")
    )]
    UnbalancedQuote {
        #[source_code]
        src: NamedSource<String>,
        #[label("this quote is never closed")]
        span: SourceSpan,
    },

    /// A shell metacharacter aish has no shell to interpret — a pipe,
    /// redirection, glob, command substitution, or grouping char.
    /// Codes: `aish::parse::unsupported_meta`.
    #[error("unsupported shell metacharacter `{ch}`")]
    #[diagnostic(
        code(aish::parse::unsupported_meta),
        help("there's no shell underneath aish — pipes, redirection, globbing, and command substitution aren't available in a directly-run command")
    )]
    UnsupportedMeta {
        ch: char,
        #[source_code]
        src: NamedSource<String>,
        #[label("not supported here")]
        span: SourceSpan,
    },

    /// A pipeline stage with no command (`a |`, `| b`, `a | | b`). Defined and
    /// unit-tested in v1; the spanned-pipeline producer is deferred (the
    /// pipeline path still routes such lines to the model). Codes:
    /// `aish::parse::empty_stage`.
    // Producer deferred to a future spanned pipeline tokenizer (see TASK-139
    // eng-spec non-goals); the code is defined + unit-tested now so it's stable.
    #[allow(dead_code)]
    #[error("empty pipeline stage")]
    #[diagnostic(
        code(aish::parse::empty_stage),
        help("every stage of a pipeline needs a command — remove the extra `|` or fill the gap")
    )]
    EmptyStage {
        #[source_code]
        src: NamedSource<String>,
        #[label("this stage has no command")]
        span: SourceSpan,
    },

    /// A malformed variable reference — an unterminated or invalid `${…}`.
    /// The caret sits on the `$`. Codes: `aish::parse::bad_var_ref`.
    #[error("malformed variable reference")]
    #[diagnostic(
        code(aish::parse::bad_var_ref),
        help("use $NAME or ${{NAME}} — the braces must close and contain only letters, digits, or underscores")
    )]
    BadVarRef {
        #[source_code]
        src: NamedSource<String>,
        #[label("this reference is malformed")]
        span: SourceSpan,
    },

    /// A `~/.aishrc` `export` line aish can't honor — a bare `NAME=value` with
    /// trailing words, or a command-substitution (`` ` ``) value that needs a
    /// shell aish doesn't have. rc parsing continues past it.
    /// Codes: `aish::config::bad_export`.
    #[error("malformed config line")]
    #[diagnostic(
        code(aish::config::bad_export),
        help("aish honors only `export NAME=value` and `alias name='value'`; command substitution and extra words need a shell aish doesn't have — the rest of the file still loads")
    )]
    BadConfigLine {
        #[source_code]
        src: NamedSource<String>,
        #[label("aish can't parse this")]
        span: SourceSpan,
    },

    /// A forced command (`!cmd`) whose program isn't on `$PATH`. Carries an
    /// optional did-you-mean hint computed from nearby PATH names.
    /// Codes: `aish::exec::not_found`.
    #[error("command not found: {cmd}")]
    #[diagnostic(code(aish::exec::not_found))]
    ExecFailed {
        cmd: String,
        #[help]
        hint: Option<String>,
    },
}

impl AishDiagnostic {
    /// An unbalanced-quote diagnostic for `line`, caret on the opening quote at
    /// byte `offset`.
    pub fn unbalanced_quote(line: &str, offset: usize) -> Self {
        Self::UnbalancedQuote {
            src: NamedSource::new("command", line.to_string()),
            span: (offset, 1).into(),
        }
    }

    /// An unsupported-metacharacter diagnostic for `line`, caret on `ch` at byte
    /// `offset`.
    pub fn unsupported_meta(line: &str, offset: usize, ch: char) -> Self {
        Self::UnsupportedMeta {
            ch,
            src: NamedSource::new("command", line.to_string()),
            span: (offset, ch.len_utf8()).into(),
        }
    }

    /// A malformed-variable-reference diagnostic for `line`, caret on the `$` at
    /// byte `offset`.
    pub fn bad_var_ref(line: &str, offset: usize) -> Self {
        Self::BadVarRef {
            src: NamedSource::new("command", line.to_string()),
            span: (offset, 1).into(),
        }
    }

    /// An empty-pipeline-stage diagnostic for `line`, caret at byte `offset`.
    /// Unit-tested; not yet produced by the (line-oriented) pipeline tokenizer.
    #[allow(dead_code)]
    pub fn empty_stage(line: &str, offset: usize) -> Self {
        Self::EmptyStage {
            src: NamedSource::new("command", line.to_string()),
            span: (offset, 1).into(),
        }
    }

    /// A bad-config-line diagnostic. `line` is the offending `~/.aishrc` line,
    /// `line_no` its 1-based number (used as the snippet header), and `offset`
    /// the byte index of the offending token within `line`.
    pub fn bad_config_line(line: &str, line_no: usize, offset: usize) -> Self {
        // Clamp the caret so a computed offset can never index past the line.
        let off = offset.min(line.len().saturating_sub(1));
        Self::BadConfigLine {
            src: NamedSource::new(rc_header(line_no), line.to_string()),
            span: (off, 1).into(),
        }
    }

    /// A command-not-found diagnostic with an optional did-you-mean `hint`.
    pub fn exec_not_found(cmd: &str, hint: Option<String>) -> Self {
        Self::ExecFailed {
            cmd: cmd.to_string(),
            hint,
        }
    }
}

/// Render `d` to a string using the themed graphical handler. The caller-chosen
/// `color` flag picks miette's unicode+color theme vs the plain (no-ANSI) one —
/// the pure form, so the caret/code/help layout is testable both ways without a
/// TTY (mirrors `style::*_with`).
pub fn render_themed(d: &AishDiagnostic, color: bool) -> String {
    let theme = if color {
        GraphicalTheme::unicode()
    } else {
        GraphicalTheme::none()
    };
    let handler = GraphicalReportHandler::new_themed(theme);
    let mut out = String::new();
    // Infallible against a String sink; ignore the fmt::Result.
    let _ = handler.render_report(&mut out, d as &dyn Diagnostic);
    out
}

/// Render `d` for the current terminal — colored unicode when
/// [`crate::style::colors_enabled`] is true (interactive TTY, color allowed),
/// plain otherwise (`NO_COLOR`, `--no-color`, or a piped stdout). The result
/// always carries the caret, the `aish::…` code, and the `help:` line.
pub fn render(d: &AishDiagnostic) -> String {
    render_themed(d, crate::style::colors_enabled())
}

/// Print `d` to stderr in the current theme, with a trailing newline.
pub fn eprint(d: &AishDiagnostic) {
    eprintln!("{}", render(d).trim_end());
}

/// The Levenshtein edit distance between `a` and `b` (classic two-row DP).
/// Bounded use only — called against PATH basenames for the exec did-you-mean.
fn levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    if a.is_empty() {
        return b.len();
    }
    if b.is_empty() {
        return a.len();
    }
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0usize; b.len() + 1];
    for (i, &ca) in a.iter().enumerate() {
        cur[0] = i + 1;
        for (j, &cb) in b.iter().enumerate() {
            let cost = if ca == cb { 0 } else { 1 };
            cur[j + 1] = (prev[j + 1] + 1).min(cur[j] + 1).min(prev[j] + cost);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

/// The closest command name to `target` among `candidates`, within an edit
/// distance of 2 (a cheap, bounded did-you-mean). Ties break on the smallest
/// distance, then lexically for determinism. `None` when nothing is close.
pub fn nearest_command<'a, I>(target: &str, candidates: I) -> Option<String>
where
    I: IntoIterator<Item = &'a str>,
{
    candidates
        .into_iter()
        .filter_map(|c| {
            let d = levenshtein(target, c);
            (d <= 2 && d > 0).then(|| (d, c.to_string()))
        })
        .min_by(|(da, ca), (db, cb)| da.cmp(db).then_with(|| ca.cmp(cb)))
        .map(|(_, c)| c)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every code string, asserted present so a code rename is a test failure
    /// (PRD §5 AC#3 — six stable codes).
    const ALL_CODES: &[&str] = &[
        "aish::parse::unbalanced_quote",
        "aish::parse::unsupported_meta",
        "aish::parse::empty_stage",
        "aish::parse::bad_var_ref",
        "aish::config::bad_export",
        "aish::exec::not_found",
    ];

    fn sample(code: &str) -> AishDiagnostic {
        match code {
            "aish::parse::unbalanced_quote" => AishDiagnostic::unbalanced_quote("echo 'x", 5),
            "aish::parse::unsupported_meta" => AishDiagnostic::unsupported_meta("a | b", 2, '|'),
            "aish::parse::empty_stage" => AishDiagnostic::empty_stage("a | | b", 4),
            "aish::parse::bad_var_ref" => AishDiagnostic::bad_var_ref("echo ${", 5),
            "aish::config::bad_export" => {
                AishDiagnostic::bad_config_line("export A=1 B=2", 42, 11)
            }
            "aish::exec::not_found" => {
                AishDiagnostic::exec_not_found("gti", Some("did you mean `git`?".into()))
            }
            other => panic!("unknown code {other}"),
        }
    }

    #[test]
    fn all_six_codes_render_in_plain_theme() {
        // AC#3: each of the six codes is stable and appears in the rendered
        // output; AC#1: a parse failure renders caret + code + help in the
        // plain (TTY-independent) theme.
        for code in ALL_CODES {
            let rendered = render_themed(&sample(code), false);
            assert!(
                rendered.contains(code),
                "code {code} missing from render:\n{rendered}"
            );
        }
    }

    #[test]
    fn parse_diag_has_caret_code_and_help() {
        // AC#1: the forced-shell parse failure renders a caret (the snippet
        // pointer), its `aish::parse::` code, and a `help:` line.
        let rendered = render_themed(&AishDiagnostic::unbalanced_quote("echo 'x", 5), false);
        assert!(rendered.contains("aish::parse::unbalanced_quote"), "{rendered}");
        assert!(rendered.contains("help:"), "{rendered}");
        // The graphical handler renders the offending source snippet with a
        // pointer line carrying the span's label — that's the "caret".
        assert!(rendered.contains("echo 'x"), "snippet missing:\n{rendered}");
        assert!(
            rendered.contains("this quote is never closed"),
            "span label (caret) missing:\n{rendered}"
        );
    }

    #[test]
    fn no_color_theme_emits_no_ansi_but_keeps_caret_code_help() {
        // AC#6: NO_COLOR / plain theme → no ANSI escape, but the caret, code,
        // and help survive.
        let rendered = render_themed(&AishDiagnostic::unbalanced_quote("echo 'x", 5), false);
        assert!(!rendered.contains('\x1b'), "plain theme must not emit ANSI:\n{rendered}");
        assert!(rendered.contains("aish::parse::unbalanced_quote"));
        assert!(rendered.contains("help:"));
    }

    #[test]
    fn color_theme_emits_ansi() {
        // AC#6: color on → graphical (colored) theme, which emits ANSI escapes.
        let rendered = render_themed(&AishDiagnostic::unbalanced_quote("echo 'x", 5), true);
        assert!(rendered.contains('\x1b'), "color theme must emit ANSI:\n{rendered}");
    }

    #[test]
    fn exec_not_found_carries_hint() {
        let rendered = render_themed(
            &AishDiagnostic::exec_not_found("gti", Some("did you mean `git`?".into())),
            false,
        );
        assert!(rendered.contains("aish::exec::not_found"), "{rendered}");
        assert!(rendered.contains("git"), "hint missing:\n{rendered}");
    }

    #[test]
    fn nearest_command_finds_close_match() {
        let path = ["git", "grep", "cargo", "ls"];
        assert_eq!(
            nearest_command("gti", path.iter().copied()).as_deref(),
            Some("git")
        );
        assert_eq!(
            nearest_command("crago", path.iter().copied()).as_deref(),
            Some("cargo")
        );
        // Nothing within edit distance 2 → no suggestion.
        assert_eq!(nearest_command("xyzzy", path.iter().copied()), None);
        // An exact match is not a "did you mean" (distance 0 excluded).
        assert_eq!(nearest_command("git", path.iter().copied()), None);
    }

    #[test]
    fn levenshtein_basics() {
        assert_eq!(levenshtein("", ""), 0);
        assert_eq!(levenshtein("a", ""), 1);
        assert_eq!(levenshtein("kitten", "sitting"), 3);
        assert_eq!(levenshtein("git", "gti"), 2);
    }
}
