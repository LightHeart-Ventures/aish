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

/// Resolve a config/credential value for `key` the way aish resolves env: the
/// `~/.aishrc` `export` pairs in `extra` (last-wins) win over the process
/// environment, and a blank/whitespace value counts as unset (→ `None`). This is
/// the single lookup the Claude credential resolver and the Grok key resolution
/// both share, so the precedence can't drift between them. Pass `&[]` when no rc
/// context is available.
pub fn env_value(extra: &[(String, String)], key: &str) -> Option<String> {
    extra
        .iter()
        .rev()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.clone())
        .or_else(|| std::env::var(key).ok())
        .filter(|v| !v.trim().is_empty())
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
                     # Only `alias name='value'` and `export NAME=value` lines are honored\n\
                     # (the `export` keyword is required; a bare NAME=value is ignored).\n\
                     # There is no shell underneath, so functions/conditionals are ignored.\n\
                     #\n\
                     # Credentials work here too, e.g.:\n\
                     #   export CLAUDE_CODE_OAUTH_TOKEN=sk-ant-oat...   # a Claude Max/Pro subscription\n\
                     #   export ANTHROPIC_API_KEY=sk-ant-...           # or a metered API key\n",
                );
                eprintln!("\x1b[2mcreated ~/.aishrc\x1b[0m");
            }
        }
    }
    parse(&std::fs::read_to_string(&path).unwrap_or_default())
}

/// Source the login-shell profile files in POSIX order — `/etc/profile` then
/// `~/.profile` — returning their merged aliases/exports (S4.5 / TASK-128).
///
/// Only the two line shapes aish understands (`alias`/`export`) are honored,
/// exactly like [`load`]; everything a real profile leans on (conditionals,
/// `$(…)`, shell functions) is ignored, since there is no shell underneath to
/// run it. A missing or unreadable file is simply skipped, so this is safe to
/// call on any system. `$NAME` references in `export` values resolve against the
/// names gathered so far across BOTH files and then the process environment, so
/// a `PATH` extended in `/etc/profile` is visible to `~/.profile`.
///
/// Login shells call this BEFORE layering `~/.aishrc` on top (see `main`), so an
/// `export`/`alias` in `~/.aishrc` overrides the same name from a profile — the
/// "/etc/profile then ~/.profile then ~/.aishrc" precedence the card asks for.
/// Non-login shells never call it, matching the convention that profiles are a
/// login-only concern.
pub fn load_login_profiles() -> Rc {
    let mut rc = Rc::default();
    let mut files: Vec<PathBuf> = vec![PathBuf::from("/etc/profile")];
    if let Ok(home) = std::env::var("HOME") {
        if !home.is_empty() {
            files.push(PathBuf::from(&home).join(".profile"));
        }
    }
    for f in &files {
        if let Ok(text) = std::fs::read_to_string(f) {
            parse_into(&text, &mut rc);
        }
    }
    rc
}

pub(crate) fn parse(text: &str) -> Rc {
    let mut rc = Rc::default();
    parse_into(text, &mut rc);
    rc
}

/// Parse `text`'s `alias`/`export` lines INTO an existing [`Rc`], accumulating
/// onto whatever it already holds. Later assignments win (env is a last-wins
/// list read in reverse; an alias overwrites an earlier one of the same name),
/// and `$NAME` references in `export` values resolve against the names gathered
/// so far — INCLUDING those from earlier-parsed files — then the process
/// environment. That cross-file visibility is what lets a login shell source
/// several profile files in sequence (see [`load_login_profiles`]) and have a
/// `PATH` extended in `/etc/profile` flow into `~/.profile`.
fn parse_into(text: &str, rc: &mut Rc) {
    // The diagnosed seam does the parsing AND returns the located diagnostics for
    // every line aish couldn't honor; here we render each to stderr (caret +
    // `aish::config::bad_export` code + help). Parsing always continues past a
    // bad line, so a single malformed export never drops the rest of the file.
    for d in parse_into_diagnosed(text, rc) {
        crate::diag::eprint(&d);
    }
}

/// Parse `text`'s `alias`/`export` lines INTO `rc` (identical accumulation to
/// [`parse_into`]) and RETURN the located diagnostics for the lines aish can't
/// honor — a testable emission seam (S7.1 / TASK-139). The mutation of `rc` is
/// byte-for-byte what `parse_into` always did; only the previously side-effecting
/// `eprintln!` skips are now returned as coded [`crate::diag::AishDiagnostic`]
/// values so a caller (or a test) decides whether/how to surface them. Good
/// lines still parse regardless of how many bad ones precede them (AC#5).
pub(crate) fn parse_into_diagnosed(
    text: &str,
    rc: &mut Rc,
) -> Vec<crate::diag::AishDiagnostic> {
    let mut diags = Vec::new();
    for (idx, raw) in text.lines().enumerate() {
        let line = raw.trim();
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
            // Byte offset where `rest` starts within the trimmed line, so the
            // caret lands on the right column of the ORIGINAL export line.
            let rest_off = line.len() - rest.len();
            match split_assignment(rest) {
                // Command substitution genuinely needs a shell we don't have.
                Some((_, value)) if value.contains('`') => {
                    let off = rest_off + export_fault_offset(rest);
                    diags.push(crate::diag::AishDiagnostic::bad_config_line(
                        line,
                        idx + 1,
                        off,
                    ));
                }
                // Resolve $NAME / ${NAME} against the exports parsed so far, then
                // the process env — so `export PATH=\"$PATH:$HOME/.local/bin\"`
                // extends the live PATH at startup.
                Some((name, value)) => {
                    let expanded = expand(&value, &rc.env);
                    rc.env.push((name, expanded));
                }
                None => {
                    let off = rest_off + export_fault_offset(rest);
                    diags.push(crate::diag::AishDiagnostic::bad_config_line(
                        line,
                        idx + 1,
                        off,
                    ));
                }
            }
        }
    }
    diags
}

/// Byte offset WITHIN `rest` (an `export` line's text after the `export `
/// keyword) of the token that makes it un-honorable — used to place the
/// diagnostic caret. A command-substitution backtick points at the backtick; an
/// extra word after a bare value (`A=1 B=2`) points at that second word; any
/// other shape falls back to the start of the value (or `rest`).
fn export_fault_offset(rest: &str) -> usize {
    if let Some(b) = rest.find('`') {
        return b;
    }
    let Some(eq) = rest.find('=') else {
        return 0;
    };
    let after = &rest[eq + 1..];
    let val_lead = after.len() - after.trim_start().len();
    let val = after[val_lead..].trim_end();
    let value_start = eq + 1 + val_lead;
    // A second whitespace-separated word in a bare value is the fault (the caret
    // points at the start of that extra word, e.g. `B` in `A=1 B=2`).
    if let Some(ws) = val.find(char::is_whitespace) {
        let tail = &val[ws..];
        let next = tail.len() - tail.trim_start().len();
        return value_start + ws + next;
    }
    value_start
}

/// Parse `NAME=value` where value may be 'single', "double", or bare-quoted.
/// Returns None for anything that isn't a single plain assignment.
fn split_assignment(s: &str) -> Option<(String, String)> {
    let (name, raw) = s.split_once('=')?;
    let name = name.trim();
    if name.is_empty()
        || !name
            .chars()
            .all(|c| c.is_alphanumeric() || matches!(c, '_' | '-' | '.'))
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
    // The span-aware [`tokenize_diagnosed`] is the single source of truth; the
    // silent route-to-model path just drops the diagnostic. `.ok()` here is what
    // guarantees ZERO behavioural change for every caller of `tokenize`/
    // `tokenize_with` (S7.1 / TASK-139, AC#2).
    tokenize_diagnosed(line, lookup).ok()
}

/// Span-aware sibling of [`tokenize_with`]: identical word-splitting and `$VAR`
/// expansion, but every rejection returns a located [`crate::diag::AishDiagnostic`]
/// (caret + stable code + help) instead of a bare `None`. This is the one
/// tokenizer; `tokenize`/`tokenize_with`/`tokenize_pipeline` are `.ok()` shims
/// over it, so the route-to-model path is byte-for-byte unchanged while the
/// forced-shell (`!`) path can surface WHY a line wasn't a command (S7.1 /
/// TASK-139). Byte offsets come from `char_indices`, so a caret lands on the
/// exact offending byte even with multibyte input.
pub fn tokenize_diagnosed(
    line: &str,
    lookup: impl Fn(&str) -> Option<String>,
) -> Result<Vec<String>, crate::diag::AishDiagnostic> {
    use crate::diag::AishDiagnostic as D;
    const META: &[char] = &[
        '|', '&', ';', '<', '>', '`', '*', '?', '(', ')', '{', '}', '\\',
    ];
    let mut words: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut in_word = false;
    let mut chars = line.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        match c {
            '\'' => {
                in_word = true;
                loop {
                    match chars.next() {
                        Some((_, '\'')) => break,
                        Some((_, ch)) => cur.push(ch),
                        None => return Err(D::unbalanced_quote(line, i)), // opening quote
                    }
                }
            }
            '"' => {
                in_word = true;
                loop {
                    match chars.next() {
                        Some((_, '"')) => break,
                        // command substitution — no shell underneath aish
                        Some((bi, '`')) => return Err(D::unsupported_meta(line, bi, '`')),
                        // Inside double quotes the word already exists, so even
                        // an empty expansion is part of it.
                        Some((di, '$')) => match expand_dollar(&mut chars, &lookup) {
                            Some(Dollar::Expanded(val)) => cur.push_str(&val),
                            Some(Dollar::Literal) => cur.push('$'),
                            None => return Err(D::bad_var_ref(line, di)),
                        },
                        Some((_, ch)) => cur.push(ch),
                        None => return Err(D::unbalanced_quote(line, i)), // opening quote
                    }
                }
            }
            '$' => match expand_dollar(&mut chars, &lookup) {
                // Unquoted: an empty expansion that isn't adjacent to other text
                // produces no word at all (POSIX word-splitting), so only join
                // the word when there's something to add.
                Some(Dollar::Expanded(val)) => {
                    if !val.is_empty() {
                        cur.push_str(&val);
                        in_word = true;
                    }
                }
                Some(Dollar::Literal) => {
                    cur.push('$');
                    in_word = true;
                }
                None => return Err(D::bad_var_ref(line, i)),
            },
            c if c.is_whitespace() => {
                if in_word {
                    words.push(std::mem::take(&mut cur));
                    in_word = false;
                }
            }
            c if META.contains(&c) => return Err(D::unsupported_meta(line, i, c)),
            c => {
                in_word = true;
                cur.push(c);
            }
        }
    }
    if in_word {
        words.push(cur);
    }
    Ok(words)
}

/// Result of reading a `$…` reference after the `$` has been consumed.
enum Dollar {
    /// A `$NAME` / `${NAME}` reference, a special parameter (`$?`/`$$`), or a
    /// positional parameter (`$1`, `$@`, `$*`, `$#`); carries the looked-up value
    /// (empty when unset).
    Expanded(String),
    /// A bare `$` that doesn't start a name (e.g. `$`, `$.`, `$ `, end-of-input)
    /// — keep it as a literal dollar sign.
    Literal,
}

/// Read a variable reference from `chars` (the `$` is already consumed) and
/// resolve it via `lookup`. Supports:
///   * `$NAME` / `${NAME}` where NAME is `[A-Za-z_][A-Za-z0-9_]*` (`${12}` too);
///   * the special parameters `$?` (last exit status) and `$$` (shell pid);
///   * the positional parameters `$1`..`$9` (one digit unbraced, POSIX — `$12`
///     is `$1` then `2`), and the positional-list specials `$@`/`$*` (the script
///     args, space-joined) and `$#` (their count). These feed script mode's
///     positional parameters (TASK-18) and resolve empty/zero elsewhere.
/// Returns None for a malformed `${…}` (unterminated or containing an invalid
/// character) so the caller rejects the line and routes it to the model.
fn expand_dollar(
    chars: &mut std::iter::Peekable<std::str::CharIndices>,
    lookup: &impl Fn(&str) -> Option<String>,
) -> Option<Dollar> {
    match chars.peek().map(|&(_, c)| c) {
        Some('{') => {
            chars.next(); // consume '{'
            // A positional-list special as the sole braced content: ${@} ${*} ${#}.
            if let Some(&(_, c)) = chars.peek() {
                if matches!(c, '@' | '*' | '#') {
                    chars.next();
                    return match chars.next() {
                        Some((_, '}')) => {
                            Some(Dollar::Expanded(lookup(&c.to_string()).unwrap_or_default()))
                        }
                        _ => None, // ${@x} / unterminated → route to the model
                    };
                }
            }
            let mut name = String::new();
            loop {
                match chars.next() {
                    Some((_, '}')) => break,
                    Some((_, c)) if c.is_ascii_alphanumeric() || c == '_' => name.push(c),
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
        // `$$` — this shell's process id. A double-dollar special parameter
        // resolved through the same lookup as `$?`, so the dispatch path feeds
        // it the live pid (see repl::var_lookup).
        Some('$') => {
            chars.next();
            Some(Dollar::Expanded(lookup("$").unwrap_or_default()))
        }
        // `$1`..`$9` — a positional parameter (single digit, unbraced; POSIX
        // treats `$12` as `$1` then a literal `2`). Feeds script mode's
        // positional params (TASK-18); resolves empty in the REPL/pipeline paths.
        Some(c) if c.is_ascii_digit() => {
            chars.next();
            Some(Dollar::Expanded(lookup(&c.to_string()).unwrap_or_default()))
        }
        // `$@` / `$*` / `$#` — the positional-list special parameters: the script
        // args space-joined (`$@`/`$*`) and their count (`$#`).
        Some(c) if matches!(c, '@' | '*' | '#') => {
            chars.next();
            Some(Dollar::Expanded(lookup(&c.to_string()).unwrap_or_default()))
        }
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {
            let mut name = String::new();
            while let Some(&(_, c)) = chars.peek() {
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
        assert!(
            !rc.aliases.contains_key("bad"),
            "piped alias must be skipped"
        );
        assert!(rc.env.contains(&("EDITOR".into(), "vim".into())));
        assert!(rc.env.contains(&("GREETING".into(), "hello world".into())));
        // $-references now expand at load time instead of being skipped.
        assert!(
            rc.env
                .contains(&("PATH".into(), "/usr/bin:/opt/bin".into()))
        );
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
        assert!(
            rc.env
                .contains(&("B".into(), "/home/dev/.local/bin".into()))
        );
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
        let last = rc
            .env
            .iter()
            .rev()
            .find(|(k, _)| k == "PATH")
            .map(|(_, v)| v.clone());
        assert_eq!(last, Some("/base:/a:/b".to_string()));
    }

    #[test]
    fn command_substitution_export_is_skipped() {
        let rc = parse("export NOW=`date`\n");
        assert!(!rc.env.iter().any(|(k, _)| k == "NOW"));
    }

    #[test]
    fn parse_into_accumulates_across_files() {
        // S4.5 / TASK-128: profile sourcing parses several files INTO one Rc.
        // A name exported by the first "file" must be visible to a `$`-reference
        // in the second, and a later alias/export of the same name wins.
        let mut rc = Rc::default();
        // Stand-in for /etc/profile.
        parse_into(
            "export AISH_PROFILE_BASE=/opt/tools\n\
             export PATH=$AISH_PROFILE_BASE/bin\n\
             alias ll='ls -l'\n",
            &mut rc,
        );
        // Stand-in for ~/.profile: extends PATH using the var from the first file
        // (cross-file visibility) and redefines the `ll` alias (last wins).
        parse_into(
            "export PATH=$PATH:$AISH_PROFILE_BASE/sbin\n\
             alias ll='ls -alF'\n",
            &mut rc,
        );
        let path = rc
            .env
            .iter()
            .rev()
            .find(|(k, _)| k == "PATH")
            .map(|(_, v)| v.clone());
        assert_eq!(path, Some("/opt/tools/bin:/opt/tools/sbin".to_string()));
        assert_eq!(rc.aliases["ll"], vec!["ls", "-alF"]);
    }

    #[test]
    fn login_profiles_layer_under_aishrc() {
        // Models main's login layering: profiles are the base, ~/.aishrc on top,
        // so a name set in both resolves to the ~/.aishrc value (last-wins).
        let mut profiles = Rc::default();
        parse_into("export EDITOR=nano\nexport PAGER=less\n", &mut profiles);
        let aishrc = parse("export EDITOR=vim\n");

        // The merge main performs: profile env first, rc env appended.
        let mut env = profiles.env.clone();
        env.extend(aishrc.env.clone());
        let lookup = |k: &str| {
            env.iter()
                .rev()
                .find(|(n, _)| n == k)
                .map(|(_, v)| v.clone())
        };
        assert_eq!(lookup("EDITOR"), Some("vim".to_string())); // ~/.aishrc wins
        assert_eq!(lookup("PAGER"), Some("less".to_string())); // profile-only survives
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
        assert_eq!(
            tok("cat pre${HOME}post").unwrap(),
            vec!["cat", "pre/home/adapost"]
        );
        // PATH-extension scenario
        assert_eq!(
            tok("env PATH=$PATH:/opt/bin").unwrap(),
            vec!["env", "PATH=/usr/bin:/bin:/opt/bin"]
        );
        // a value with spaces is NOT re-split — it stays one argv word
        assert_eq!(tok("echo $GREETING").unwrap(), vec!["echo", "hi there"]);
        // an expanded value is not re-scanned for metacharacters
        let pipey = |_: &str| Some("a|b".to_string());
        assert_eq!(
            tokenize_with("echo $X", pipey).unwrap(),
            vec!["echo", "a|b"]
        );

        // unset → empty; a standalone unquoted empty expansion drops the word
        assert_eq!(tok("grep $MISSING file").unwrap(), vec!["grep", "file"]);
        // ...but adjacent literal text is preserved
        assert_eq!(tok("echo a$MISSING").unwrap(), vec!["echo", "a"]);
        // a quoted empty expansion IS a real (empty) argument
        assert_eq!(tok("echo \"$MISSING\"").unwrap(), vec!["echo", ""]);

        // expansion happens inside double quotes; single quotes stay literal
        assert_eq!(
            tok("echo \"$HOME/x\"").unwrap(),
            vec!["echo", "/home/ada/x"]
        );
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
    fn tokenizer_expands_pid() {
        // `$$` resolves through the same lookup the dispatch path uses to feed in
        // the shell's own process id (S4.6).
        let pid = |name: &str| (name == "$").then(|| "4242".to_string());
        let tok = |line: &str| tokenize_with(line, pid);
        assert_eq!(tok("echo $$").unwrap(), vec!["echo", "4242"]);
        // adjacent to literal text
        assert_eq!(tok("echo pid=$$").unwrap(), vec!["echo", "pid=4242"]);
        // an unset pid expands to empty and drops the standalone word
        let none = |_: &str| -> Option<String> { None };
        assert_eq!(tokenize_with("echo $$", none).unwrap(), vec!["echo"]);
    }

    #[test]
    fn tokenizer_expands_positional_parameters() {
        // TASK-18: `$1`..`$9`, `$@`/`$*`, `$#` resolve through `lookup` so script
        // mode can feed in the kernel-appended argv. A lookup mimicking a script
        // invoked as `deploy.aish staging v2` ($0=deploy.aish, $1=staging, …).
        let params = |name: &str| match name {
            "0" => Some("deploy.aish".to_string()),
            "1" => Some("staging".to_string()),
            "2" => Some("v2".to_string()),
            "#" => Some("2".to_string()),
            "@" | "*" => Some("staging v2".to_string()),
            _ => None,
        };
        let tok = |line: &str| tokenize_with(line, params);

        // bare single-digit positionals
        assert_eq!(tok("echo $1 $2").unwrap(), vec!["echo", "staging", "v2"]);
        // $0 is the script name
        assert_eq!(tok("echo $0").unwrap(), vec!["echo", "deploy.aish"]);
        // braced form, including the two-digit ${12} (one reference, not $1 then 2)
        let twelve = |name: &str| (name == "12").then(|| "twelfth".to_string());
        assert_eq!(
            tokenize_with("echo ${12}", twelve).unwrap(),
            vec!["echo", "twelfth"]
        );
        // `$@`/`$*` expand to the space-joined args as ONE word (aish never
        // re-splits an expansion), and `$#` to the count.
        assert_eq!(tok("echo $@").unwrap(), vec!["echo", "staging v2"]);
        assert_eq!(tok("echo $*").unwrap(), vec!["echo", "staging v2"]);
        assert_eq!(tok("echo count=$#").unwrap(), vec!["echo", "count=2"]);
        // braced specials resolve too
        assert_eq!(tok("echo ${#}").unwrap(), vec!["echo", "2"]);
        // POSIX: unbraced `$12` is `$1` followed by a literal `2`
        assert_eq!(tok("echo $12").unwrap(), vec!["echo", "staging2"]);
        // an unset positional drops the standalone word (empty expansion)
        let none = |_: &str| -> Option<String> { None };
        assert_eq!(tokenize_with("echo $9", none).unwrap(), vec!["echo"]);
    }

    #[test]
    fn pipeline_splits_stages() {
        assert_eq!(
            tokenize_pipeline("ls -la | grep rs | wc -l").unwrap(),
            vec![vec!["ls", "-la"], vec!["grep", "rs"], vec!["wc", "-l"],],
        );
        // single command is a one-stage pipeline
        assert_eq!(
            tokenize_pipeline("ls -la").unwrap(),
            vec![vec!["ls", "-la"]]
        );
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

    // ---- S7.1 / TASK-139: diagnosed tokenizer ----------------------------

    use crate::diag::AishDiagnostic as Diag;

    /// The byte offset of a parse diagnostic's caret, for span assertions.
    fn diag_offset(d: &Diag) -> usize {
        match d {
            Diag::UnbalancedQuote { span, .. }
            | Diag::UnsupportedMeta { span, .. }
            | Diag::EmptyStage { span, .. }
            | Diag::BadVarRef { span, .. }
            | Diag::BadConfigLine { span, .. } => span.offset(),
            Diag::ExecFailed { .. } => panic!("exec diagnostic has no span"),
        }
    }

    #[test]
    fn tokenize_diagnosed_ok_matches_tokenize_over_corpus() {
        // AC#2: `tokenize(x) == tokenize_diagnosed(x).ok()` over a corpus — the
        // diagnosed tokenizer is the single source of truth, so the silent
        // route-to-model path is byte-for-byte unchanged.
        let env = |name: &str| std::env::var(name).ok();
        let corpus = [
            "ls -la",
            "git commit -m \"fix: a thing\"",
            "echo 'a  b'",
            "  ",
            "ls | wc -l",
            "ls > out",
            "a && b",
            "rm *.rs",
            "what?",
            "grep 'a|b' x",
            "what's eating my disk",
            "echo $HOME/bin",
            "echo ${UNCLOSED",
            "echo ${}",
            "echo \"oops",
            "echo \"`date`\"",
            "café résumé",
        ];
        for line in corpus {
            let shim = tokenize(line);
            let diagnosed = tokenize_diagnosed(line, env).ok();
            assert_eq!(shim, diagnosed, "divergence on `{line}`");
        }
    }

    #[test]
    fn tokenize_diagnosed_span_offsets_equal_offending_char() {
        // AC#4: the caret lands on the offending byte.
        let env = |name: &str| std::env::var(name).ok();
        let tok = |l: &str| tokenize_diagnosed(l, env).unwrap_err();

        // `|` in `a | | b` — the FIRST `|` (byte 2) is the offending metachar.
        let d = tok("a | | b");
        assert!(matches!(d, Diag::UnsupportedMeta { ch: '|', .. }));
        assert_eq!(diag_offset(&d), 2);

        // `'` in an unbalanced single quote — the opening quote at byte 5.
        let d = tok("echo 'x");
        assert!(matches!(d, Diag::UnbalancedQuote { .. }));
        assert_eq!(diag_offset(&d), 5);

        // unbalanced double quote — opening `"` at byte 5.
        let d = tok("echo \"x");
        assert!(matches!(d, Diag::UnbalancedQuote { .. }));
        assert_eq!(diag_offset(&d), 5);

        // malformed `${…}` — the `$` at byte 5.
        let d = tok("echo ${");
        assert!(matches!(d, Diag::BadVarRef { .. }));
        assert_eq!(diag_offset(&d), 5);

        // command substitution backtick inside double quotes → unsupported_meta
        // on the backtick (byte 6).
        let d = tok("echo \"`date`\"");
        assert!(matches!(d, Diag::UnsupportedMeta { ch: '`', .. }));
        assert_eq!(diag_offset(&d), 6);

        // multibyte: `café |` — `café` is 5 bytes (é is 2), space at 5, `|` at 6.
        let d = tok("café |");
        assert!(matches!(d, Diag::UnsupportedMeta { ch: '|', .. }));
        assert_eq!(diag_offset(&d), 6);
    }

    #[test]
    fn bad_config_line_caret_lands_on_offending_token() {
        // AC#4: `B` in `export A=1 B=2` (byte 11 of the line).
        let mut rc = Rc::default();
        let diags = parse_into_diagnosed("export A=1 B=2\n", &mut rc);
        assert_eq!(diags.len(), 1);
        assert!(matches!(diags[0], Diag::BadConfigLine { .. }));
        assert_eq!(diag_offset(&diags[0]), 11);
        // The bad export was NOT applied.
        assert!(!rc.env.iter().any(|(k, _)| k == "A"));

        // Command substitution → caret on the backtick (byte 11).
        let mut rc = Rc::default();
        let diags = parse_into_diagnosed("export NOW=`date`\n", &mut rc);
        assert_eq!(diags.len(), 1);
        assert_eq!(diag_offset(&diags[0]), 11);
    }

    #[test]
    fn bad_config_line_emits_diagnostic_and_keeps_good_lines() {
        // AC#5: a malformed `~/.aishrc` line yields a coded/located diagnostic
        // AND rc parsing continues — the good export still lands.
        let mut rc = Rc::default();
        let diags = parse_into_diagnosed(
            "export A=1 B=2\n\
             export GOOD=ok\n\
             export NOW=`date`\n\
             export ALSO=fine\n",
            &mut rc,
        );
        // Two bad lines → two diagnostics.
        assert_eq!(diags.len(), 2);
        // …and both good exports survived.
        assert!(rc.env.contains(&("GOOD".into(), "ok".into())));
        assert!(rc.env.contains(&("ALSO".into(), "fine".into())));

        // The diagnostic renders with the config code and the `~/.aishrc:N`
        // header (line 1 for the first bad export).
        let rendered = crate::diag::render_themed(&diags[0], false);
        assert!(rendered.contains("aish::config::bad_export"), "{rendered}");
        assert!(rendered.contains("~/.aishrc:1"), "{rendered}");
    }
}
