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
            if let Some((name, value)) = split_assignment(rest) {
                // Unexpandable values ($VAR, `cmd`) are bash's business, not ours.
                if !value.contains('$') && !value.contains('`') {
                    rc.env.push((name, value));
                }
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

/// Split a command line into argv words, honoring single/double quotes.
/// Returns None when the line uses shell machinery aish doesn't implement —
/// pipes, redirection, expansion, globs, control operators — so the caller
/// can route it to the model instead.
pub fn tokenize(line: &str) -> Option<Vec<String>> {
    const META: &[char] = &[
        '|', '&', ';', '<', '>', '$', '`', '*', '?', '(', ')', '{', '}', '\\',
    ];
    let mut words: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut in_word = false;
    let mut chars = line.chars();
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
                        Some('$' | '`') => return None, // would need expansion
                        Some(ch) => cur.push(ch),
                        None => return None,
                    }
                }
            }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_aliases_and_exports() {
        let rc = parse(
            "# comment\n\
             alias ll='ls -alF'\n\
             alias grep='grep --color=auto'\n\
             alias bad='ls | wc -l'\n\
             export EDITOR=vim\n\
             export GREETING=\"hello world\"\n\
             export PATH=$PATH:/opt/bin\n\
             if [ -f /etc/bashrc ]; then\n\
             fi\n",
        );
        assert_eq!(rc.aliases["ll"], vec!["ls", "-alF"]);
        assert_eq!(rc.aliases["grep"], vec!["grep", "--color=auto"]);
        assert!(!rc.aliases.contains_key("bad"), "piped alias must be skipped");
        assert!(rc.env.contains(&("EDITOR".into(), "vim".into())));
        assert!(rc.env.contains(&("GREETING".into(), "hello world".into())));
        assert!(!rc.env.iter().any(|(k, _)| k == "PATH"), "$-value must be skipped");
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
        for line in ["ls | wc -l", "echo $HOME", "ls > out", "a && b", "rm *.rs", "what?"] {
            assert!(tokenize(line).is_none(), "should reject: {line}");
        }
        // ...but quoted metachars are just text
        assert_eq!(tokenize("grep 'a|b' x").unwrap(), vec!["grep", "a|b", "x"]);
        // unbalanced quote (apostrophe in English) routes to the model
        assert!(tokenize("what's eating my disk").is_none());
    }
}
