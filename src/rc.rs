//! ~/.aishrc — aish's rc file, seeded from ~/.bashrc on first run.
//!
//! There is no shell underneath aish, so we don't *execute* the file: we parse
//! the two line shapes that make sense without one — `alias name='value'` and
//! `export NAME=value` — and ignore everything else (functions, conditionals,
//! `$expansions`, bashisms). Aliases feed the REPL's direct-dispatch path;
//! exports are applied to every program aish spawns.

use std::collections::HashMap;
use std::path::PathBuf;

#[derive(Default)]
pub struct Rc {
    /// alias name → already-tokenized replacement argv prefix.
    pub aliases: HashMap<String, Vec<String>>,
    /// export NAME=value pairs, applied per-spawn (never process-global setenv).
    pub env: Vec<(String, String)>,
}

pub fn load() -> Rc {
    let home = std::env::var("HOME").unwrap_or_default();
    if home.is_empty() {
        return Rc::default();
    }
    let path = PathBuf::from(&home).join(".aishrc");
    if !path.exists() {
        let bashrc = PathBuf::from(&home).join(".bashrc");
        match std::fs::copy(&bashrc, &path) {
            Ok(_) => eprintln!(
                "\x1b[2mcreated ~/.aishrc from your ~/.bashrc — aish reads its alias/export lines\x1b[0m"
            ),
            Err(_) => {
                let _ = std::fs::write(
                    &path,
                    "# ~/.aishrc — read by aish at startup.\n\
                     # Only `alias name='value'` and `export NAME=value` lines are honored;\n\
                     # there is no shell underneath, so functions/conditionals are ignored.\n",
                );
                eprintln!("\x1b[2mcreated ~/.aishrc\x1b[0m");
            }
        }
    }
    parse(&std::fs::read_to_string(&path).unwrap_or_default())
}

fn parse(text: &str) -> Rc {
    let mut rc = Rc::default();
    for line in text.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("alias ") {
            if let Some((name, value)) = split_assignment(rest) {
                // The replacement has to survive our own tokenizer, or direct
                // dispatch couldn't run it anyway (pipes etc. need a shell).
                if let Some(words) = tokenize(&value) {
                    if !words.is_empty() {
                        rc.aliases.insert(name, words);
                    }
                }
            }
        } else if let Some(rest) = line.strip_prefix("export ") {
            match split_assignment(rest) {
                // Command substitution genuinely needs a shell we don't have.
                Some((_, value)) if value.contains('`') => eprintln!(
                    "\x1b[2maish: ~/.aishrc: skipped `{line}` — command substitution (`) needs a shell\x1b[0m"
                ),
                // Resolve $NAME / ${NAME} against the exports parsed so far, then
                // the process env — so `export PATH=\"$PATH:$HOME/.local/bin\"`
                // extends the live PATH at startup.
                Some((name, value)) => {
                    let expanded = expand(&value, &rc.env);
                    rc.env.push((name, expanded));
                }
                None => eprintln!(
                    "\x1b[2maish: ~/.aishrc: skipped `{line}` — not a plain NAME=value export\x1b[0m"
                ),
            }
        }
    }
    rc
}

/// Parse `NAME=value` where value may be 'single', "double", or bare-quoted.
/// Returns None for anything that isn't a single plain assignment.
fn split_assignment(s: &str) -> Option<(String, String)> {
    let (name, raw) = s.split_once('=')?;
    let name = name.trim();
    if name.is_empty()
        || !name.chars().all(|c| c.is_alphanumeric() || matches!(c, '_' | '-' | '.'))
    {
        return None;
    }
    let raw = raw.trim();
    let value = match raw.chars().next() {
        Some(q @ ('\'' | '"')) => {
            let inner = &raw[1..];
            let end = inner.find(q)?;
            // trailing garbage after the closing quote → not a plain assignment
            if !inner[end + 1..].trim().is_empty() {
                return None;
            }
            inner[..end].to_string()
        }
        _ => {
            // bare value: a space would start a second word (e.g. `export A=1 B=2`)
            if raw.contains(char::is_whitespace) {
                return None;
            }
            raw.to_string()
        }
    };
    Some((name.to_string(), value))
}

/// Resolve `$NAME` and `${NAME}` references in an export value against the
/// exports parsed so far (so chained `export PATH="$PATH:..."` lines stack),
/// then the process environment. Unknown names expand to empty, as bash does.
fn expand(value: &str, so_far: &[(String, String)]) -> String {
    let mut out = String::with_capacity(value.len());
    let mut chars = value.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '$' {
            out.push(c);
            continue;
        }
        let braced = chars.peek() == Some(&'{');
        if braced {
            chars.next();
        }
        let mut name = String::new();
        while let Some(&n) = chars.peek() {
            if n.is_ascii_alphanumeric() || n == '_' {
                name.push(n);
                chars.next();
            } else {
                break;
            }
        }
        if braced && chars.peek() == Some(&'}') {
            chars.next();
        }
        if name.is_empty() {
            out.push('$'); // a lone `$` is just a dollar sign
            continue;
        }
        let resolved = so_far
            .iter()
            .rev()
            .find(|(k, _)| *k == name)
            .map(|(_, v)| v.clone())
            .or_else(|| std::env::var(&name).ok())
            .unwrap_or_default();
        out.push_str(&resolved);
    }
    out
}

/// Split a command line into argv words, honoring single/double quotes and
/// expanding `$VAR` / `${VAR}` references against the process environment.
/// Returns None when the line uses shell machinery aish doesn't implement —
/// pipes, redirection, command substitution, globs, control operators — so the
/// caller can route it to the model instead.
pub fn tokenize(line: &str) -> Option<Vec<String>> {
    tokenize_with(line, |name| std::env::var(name).ok())
}

/// Like [`tokenize`], but resolves `$VAR` references through `lookup` so the
/// dispatch path can consult per-session `export`s before falling back to the
/// process environment. An unset variable expands to the empty string, matching
/// POSIX shells. Expanded values are inserted verbatim — never re-split on
/// whitespace nor re-scanned for metacharacters — so a variable can't smuggle
/// shell syntax (pipes, `;`, …) into the argv.
pub fn tokenize_with(line: &str, lookup: impl Fn(&str) -> Option<String>) -> Option<Vec<String>> {
    const META: &[char] = &[
        '|', '&', ';', '<', '>', '`', '*', '?', '(', ')', '{', '}', '\\',
    ];
    let mut words: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut in_word = false;
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\'' => {
                in_word = true;
                loop {
                    match chars.next() {
                        Some('\'') => break,
                        Some(ch) => cur.push(ch),
                        None => return None, // unbalanced quote
                    }
                }
            }
            '"' => {
                in_word = true;
                loop {
                    match chars.next() {
                        Some('"') => break,
                        Some('`') => return None, // command substitution
                        // Inside double quotes the word already exists, so even
                        // an empty expansion is part of it.
                        Some('$') => match expand_dollar(&mut chars, &lookup)? {
                            Dollar::Expanded(val) => cur.push_str(&val),
                            Dollar::Literal => cur.push('$'),
                        },
                        Some(ch) => cur.push(ch),
                        None => return None,
                    }
                }
            }
            '$' => match expand_dollar(&mut chars, &lookup)? {
                // Unquoted: an empty expansion that isn't adjacent to other text
                // produces no word at all (POSIX word-splitting), so only join
                // the word when there's something to add.
                Dollar::Expanded(val) => {
                    if !val.is_empty() {
                        cur.push_str(&val);
                        in_word = true;
                    }
                }
                Dollar::Literal => {
                    cur.push('$');
                    in_word = true;
                }
            },
            c if c.is_whitespace() => {
                if in_word {
                    words.push(std::mem::take(&mut cur));
                    in_word = false;
                }
            }
            c if META.contains(&c) => return None,
            c => {
                in_word = true;
                cur.push(c);
            }
        }
    }
    if in_word {
        words.push(cur);
    }
    Some(words)
}

/// Result of reading a `$…` reference after the `$` has been consumed.
enum Dollar {
    /// A `$NAME` / `${NAME}` reference; carries the looked-up value (empty when
    /// the variable is unset).
    Expanded(String),
    /// A bare `$` that doesn't start a name (e.g. `$`, `$5`, `$ `) — keep it as
    /// a literal dollar sign.
    Literal,
}

/// Read a variable reference from `chars` (the `$` is already consumed) and
/// resolve it via `lookup`. Supports `$NAME` and `${NAME}` where NAME is
/// `[A-Za-z_][A-Za-z0-9_]*`, plus the special parameter `$?` (last exit status).
/// Returns None for a malformed `${…}` (unterminated
/// or containing an invalid character) so the caller rejects the line and routes
/// it to the model.
fn expand_dollar(
    chars: &mut std::iter::Peekable<std::str::Chars>,
    lookup: &impl Fn(&str) -> Option<String>,
) -> Option<Dollar> {
    match chars.peek() {
        Some('{') => {
            chars.next(); // consume '{'
            let mut name = String::new();
            loop {
                match chars.next() {
                    Some('}') => break,
                    Some(c) if c.is_ascii_alphanumeric() || c == '_' => name.push(c),
                    _ => return None, // invalid char or unterminated ${…}
                }
            }
            if name.is_empty() {
                return None; // ${} is not a valid reference
            }
            Some(Dollar::Expanded(lookup(&name).unwrap_or_default()))
        }
        // `$?` — the last command's exit status. A single-char special parameter
        // resolved through the same lookup, so the dispatch path can feed it the
        // session's tracked status.
        Some('?') => {
            chars.next();
            Some(Dollar::Expanded(lookup("?").unwrap_or_default()))
        }
        Some(&c) if c.is_ascii_alphabetic() || c == '_' => {
            let mut name = String::new();
            while let Some(&c) = chars.peek() {
                if c.is_ascii_alphanumeric() || c == '_' {
                    name.push(c);
                    chars.next();
                } else {
                    break;
                }
            }
            Some(Dollar::Expanded(lookup(&name).unwrap_or_default()))
        }
        _ => Some(Dollar::Literal),
    }
}

/// Split a command line into pipeline stages on unquoted `|`, tokenizing each
/// stage with [`tokenize`]. `a | b | c` becomes three argv vectors.
///
/// Returns `None` — so the caller routes the line to the model — when the line
/// uses shell machinery aish doesn't implement (any stage that [`tokenize`]
/// rejects), a quote is unbalanced, or a pipeline stage is empty (`a |`,
/// `| b`, `a || b`). A line with no pipe yields a single-stage pipeline.
// Consumed by the pipeline executor (TASK-9); allow until that caller lands.
#[allow(dead_code)]
pub fn tokenize_pipeline(line: &str) -> Option<Vec<Vec<String>>> {
    let segments = split_pipeline(line)?;
    let piped = segments.len() > 1;
    let mut stages = Vec::with_capacity(segments.len());
    for seg in segments {
        let words = tokenize(seg)?;
        // An empty stage only makes sense around a pipe: `a |`, `|`, `a || b`.
        if piped && words.is_empty() {
            return None;
        }
        stages.push(words);
    }
    Some(stages)
}

/// Split on top-level (unquoted) `|`, returning the raw segments untouched.
/// Pipes inside single/double quotes stay literal text. Returns `None` on an
/// unbalanced quote so the pipeline routes to the model like other bad input.
fn split_pipeline(line: &str) -> Option<Vec<&str>> {
    let mut segments = Vec::new();
    let mut start = 0;
    let mut chars = line.char_indices();
    while let Some((i, c)) = chars.next() {
        match c {
            // Skip over a quoted span so an embedded `|` isn't a separator.
            q @ ('\'' | '"') => loop {
                match chars.next() {
                    Some((_, ch)) if ch == q => break,
                    Some(_) => {}
                    None => return None, // unbalanced quote
                }
            },
            '|' => {
                segments.push(&line[start..i]);
                start = i + 1; // '|' is one ASCII byte
            }
            _ => {}
        }
    }
    segments.push(&line[start..]);
    Some(segments)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_aliases_and_exports() {
        // Reference an export defined earlier in the same file, so the test
        // doesn't have to mutate the process env to exercise expansion.
        let rc = parse(
            "# comment\n\
             alias ll='ls -alF'\n\
             alias grep='grep --color=auto'\n\
             alias bad='ls | wc -l'\n\
             export EDITOR=vim\n\
             export GREETING=\"hello world\"\n\
             export AISH_BASE=/usr/bin\n\
             export PATH=\"$AISH_BASE:/opt/bin\"\n\
             if [ -f /etc/bashrc ]; then\n\
             fi\n",
        );
        assert_eq!(rc.aliases["ll"], vec!["ls", "-alF"]);
        assert_eq!(rc.aliases["grep"], vec!["grep", "--color=auto"]);
        assert!(!rc.aliases.contains_key("bad"), "piped alias must be skipped");
        assert!(rc.env.contains(&("EDITOR".into(), "vim".into())));
        assert!(rc.env.contains(&("GREETING".into(), "hello world".into())));
        // $-references now expand at load time instead of being skipped.
        assert!(rc.env.contains(&("PATH".into(), "/usr/bin:/opt/bin".into())));
    }

    #[test]
    fn expands_dollar_references_in_exports() {
        let rc = parse(
            "export H=/home/dev\n\
             export A=$H/bin\n\
             export B=\"${H}/.local/bin\"\n\
             export C=$AISH_NO_SUCH_VAR_42:/tail\n\
             export D=plain\n",
        );
        assert!(rc.env.contains(&("A".into(), "/home/dev/bin".into())));
        assert!(rc.env.contains(&("B".into(), "/home/dev/.local/bin".into())));
        // An undefined name expands to empty, as bash does.
        assert!(rc.env.contains(&("C".into(), ":/tail".into())));
        assert!(rc.env.contains(&("D".into(), "plain".into())));
    }

    #[test]
    fn chained_path_exports_stack() {
        let rc = parse(
            "export PATH=/base\n\
             export PATH=\"$PATH:/a\"\n\
             export PATH=\"$PATH:/b\"\n",
        );
        // Each export sees the previous one, so all three dirs survive and the
        // last entry — the one `.envs()` keeps — holds the full PATH.
        let last = rc.env.iter().rev().find(|(k, _)| k == "PATH").map(|(_, v)| v.clone());
        assert_eq!(last, Some("/base:/a:/b".to_string()));
    }

    #[test]
    fn command_substitution_export_is_skipped() {
        let rc = parse("export NOW=`date`\n");
        assert!(!rc.env.iter().any(|(k, _)| k == "NOW"));
    }

    #[test]
    fn tokenizer_basics() {
        assert_eq!(tokenize("ls -la").unwrap(), vec!["ls", "-la"]);
        assert_eq!(
            tokenize("git commit -m \"fix: a thing\"").unwrap(),
            vec!["git", "commit", "-m", "fix: a thing"]
        );
        assert_eq!(tokenize("echo 'a  b'").unwrap(), vec!["echo", "a  b"]);
        assert_eq!(tokenize("  ").unwrap(), Vec::<String>::new());
    }

    #[test]
    fn tokenizer_rejects_shell_machinery() {
        for line in ["ls | wc -l", "ls > out", "a && b", "rm *.rs", "what?"] {
            assert!(tokenize(line).is_none(), "should reject: {line}");
        }
        // ...but quoted metachars are just text
        assert_eq!(tokenize("grep 'a|b' x").unwrap(), vec!["grep", "a|b", "x"]);
        // unbalanced quote (apostrophe in English) routes to the model
        assert!(tokenize("what's eating my disk").is_none());
    }

    #[test]
    fn tokenizer_expands_variables() {
        // Deterministic lookup so the test doesn't depend on the real environment.
        let env = |name: &str| match name {
            "HOME" => Some("/home/ada".to_string()),
            "PATH" => Some("/usr/bin:/bin".to_string()),
            "GREETING" => Some("hi there".to_string()),
            _ => None,
        };
        let tok = |line: &str| tokenize_with(line, env);

        // bare and braced forms
        assert_eq!(tok("echo $HOME").unwrap(), vec!["echo", "/home/ada"]);
        assert_eq!(tok("echo ${HOME}").unwrap(), vec!["echo", "/home/ada"]);
        // adjacent to text, on either side
        assert_eq!(tok("ls $HOME/bin").unwrap(), vec!["ls", "/home/ada/bin"]);
        assert_eq!(tok("cat pre${HOME}post").unwrap(), vec!["cat", "pre/home/adapost"]);
        // PATH-extension scenario
        assert_eq!(
            tok("env PATH=$PATH:/opt/bin").unwrap(),
            vec!["env", "PATH=/usr/bin:/bin:/opt/bin"]
        );
        // a value with spaces is NOT re-split — it stays one argv word
        assert_eq!(tok("echo $GREETING").unwrap(), vec!["echo", "hi there"]);
        // an expanded value is not re-scanned for metacharacters
        let pipey = |_: &str| Some("a|b".to_string());
        assert_eq!(tokenize_with("echo $X", pipey).unwrap(), vec!["echo", "a|b"]);

        // unset → empty; a standalone unquoted empty expansion drops the word
        assert_eq!(tok("grep $MISSING file").unwrap(), vec!["grep", "file"]);
        // ...but adjacent literal text is preserved
        assert_eq!(tok("echo a$MISSING").unwrap(), vec!["echo", "a"]);
        // a quoted empty expansion IS a real (empty) argument
        assert_eq!(tok("echo \"$MISSING\"").unwrap(), vec!["echo", ""]);

        // expansion happens inside double quotes; single quotes stay literal
        assert_eq!(tok("echo \"$HOME/x\"").unwrap(), vec!["echo", "/home/ada/x"]);
        assert_eq!(tok("echo '$HOME'").unwrap(), vec!["echo", "$HOME"]);

        // a lone `$`, or one before a non-name char, is a literal dollar sign
        assert_eq!(tok("echo $").unwrap(), vec!["echo", "$"]);
        assert_eq!(tok("echo 5$").unwrap(), vec!["echo", "5$"]);

        // malformed ${…} routes to the model
        assert!(tok("echo ${UNCLOSED").is_none());
        assert!(tok("echo ${}").is_none());
    }

    #[test]
    fn tokenizer_expands_last_status() {
        // `$?` resolves through the same lookup the dispatch path uses to feed in
        // the session's tracked exit status.
        let status = |name: &str| (name == "?").then(|| "1".to_string());
        let tok = |line: &str| tokenize_with(line, status);

        assert_eq!(tok("echo $?").unwrap(), vec!["echo", "1"]);
        // adjacent to literal text, bare and double-quoted
        assert_eq!(tok("echo code=$?").unwrap(), vec!["echo", "code=1"]);
        assert_eq!(tok("echo \"exit $?\"").unwrap(), vec!["echo", "exit 1"]);
        // an unset status expands to empty and drops the standalone word, like
        // any other unknown variable
        let none = |_: &str| -> Option<String> { None };
        assert_eq!(tokenize_with("echo $?", none).unwrap(), vec!["echo"]);
    }

    #[test]
    fn pipeline_splits_stages() {
        assert_eq!(
            tokenize_pipeline("ls -la | grep rs | wc -l").unwrap(),
            vec![
                vec!["ls", "-la"],
                vec!["grep", "rs"],
                vec!["wc", "-l"],
            ],
        );
        // single command is a one-stage pipeline
        assert_eq!(tokenize_pipeline("ls -la").unwrap(), vec![vec!["ls", "-la"]]);
        // pipes adjacent to arguments (no surrounding spaces)
        assert_eq!(
            tokenize_pipeline("a|b|c").unwrap(),
            vec![vec!["a"], vec!["b"], vec!["c"]],
        );
    }

    #[test]
    fn pipeline_keeps_quoted_pipes_literal() {
        // a quoted pipe is text, not a separator
        assert_eq!(
            tokenize_pipeline("grep 'a|b' x").unwrap(),
            vec![vec!["grep", "a|b", "x"]],
        );
        // mix: real separator plus a quoted pipe inside a stage
        assert_eq!(
            tokenize_pipeline("echo 'x|y' | cat").unwrap(),
            vec![vec!["echo", "x|y"], vec!["cat"]],
        );
    }

    #[test]
    fn pipeline_rejects_empty_stages() {
        for line in ["a |", "| b", "a || b", "|", "a | | b"] {
            assert!(tokenize_pipeline(line).is_none(), "should reject: {line}");
        }
    }

    #[test]
    fn pipeline_rejects_shell_machinery_in_a_stage() {
        // a stage that itself uses unsupported syntax fails the whole pipeline
        // (note: `$VAR` in a stage is no longer machinery — it expands now)
        assert!(tokenize_pipeline("cat *.rs | wc").is_none());
        // unbalanced quote routes to the model
        assert!(tokenize_pipeline("echo 'oops | cat").is_none());
    }

    #[test]
    fn pipeline_stages_expand_variables() {
        // stages run through the same tokenizer, so `$VAR` expands per stage
        // (process-env lookup; PATH is always set)
        let stages = tokenize_pipeline("echo $PATH | cat").unwrap();
        assert_eq!(stages.len(), 2);
        assert_eq!(stages[0][0], "echo");
        assert!(!stages[0][1].contains('$'), "PATH must be expanded");
        assert_eq!(stages[1], vec!["cat"]);
    }
}
