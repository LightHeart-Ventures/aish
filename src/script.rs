//! Script mode (TASK-17): `aish <file>` runs the file's lines non-interactively,
//! then exits with the status of the last line — the shell-script entry point.
//!
//! Each non-blank, non-comment line is executed as if typed at the prompt: a
//! line that resolves to a program (or a pipeline of programs, or the `cd`
//! builtin) runs directly on the terminal; anything else is handed to the model,
//! exactly like the interactive REPL. `#` comment lines and blank lines are
//! skipped — and because a `#!` shebang line is just a `#` comment, a script
//! beginning with `#!/usr/bin/env aish` Just Works (that's the seam TASK-18
//! builds on). The `!`/`?` route prefixes still force direct/model.
//!
//! Unlike the interactive prompt this path deliberately SKIPS the
//! "looks-like-English" routing heuristics (`looks_like_prose`, the bare-`yes`
//! guard, …): a script is explicit, so a bare `who` line means the `who`
//! command, never a question. A line that simply doesn't resolve to a program
//! still falls through to the model, so prose-y automation lines keep working.

use crate::backend::Backend;
use crate::engine;
use crate::pipeline;
use crate::rc;
use crate::repl::{confirm_tty, resolve_program};
use crate::session::Session;
use crate::tools;
use anyhow::Result;
use std::path::{Path, PathBuf};

/// Outcome of trying to run one script line directly (without the model).
enum Step {
    /// Ran on the terminal (or errored locally) — move to the next line.
    Ran,
    /// `exit` / `logout` — stop the script.
    Quit,
    /// Not a runnable command — the caller routes the line to the model.
    NotACommand,
}

/// Execute an aish script file. Returns the exit status of the last executed
/// line (`$?`), matching `sh script`. A read error is surfaced as an `Err` so
/// `main` can report it and exit non-zero.
pub async fn run(backend: &Backend, session: &mut Session, path: &Path) -> Result<i32> {
    let src = std::fs::read_to_string(path)
        .map_err(|e| anyhow::anyhow!("aish: cannot read script {}: {e}", path.display()))?;
    let mut prev_dir: Option<PathBuf> = None;
    for raw in executable_lines(&src) {
        let (line, route) = split_route(raw);
        if line.is_empty() {
            continue;
        }
        if route != Route::Model {
            match run_directly(&line, route == Route::Direct, session, &mut prev_dir).await {
                Step::Ran => continue,
                Step::Quit => break,
                Step::NotACommand => {}
            }
        }
        // A non-command line (or a `?`-forced one) goes to the model, exactly as
        // at the interactive prompt. Errors don't abort the script — they set a
        // non-zero `$?` and execution continues, like `sh` without `set -e`.
        match engine::run_turn(backend, session, line, &mut confirm_tty).await {
            Ok(text) => {
                let text = text.trim();
                if !text.is_empty() {
                    println!("{}", crate::md::render_stdout(text));
                }
                if let Some(db) = &session.db {
                    db.record("output", &session.cwd.to_string_lossy(), text);
                }
            }
            Err(e) => {
                eprintln!("\x1b[31maish:\x1b[0m {e:#}");
                session.last_status = 1;
            }
        }
    }
    Ok(session.last_status)
}

/// The lines of a script that actually execute: trimmed, with blank lines and
/// `#` comments removed. A leading `#!` shebang is a `#` comment, so it's
/// dropped here too — the single seam that lets `#!/usr/bin/env aish` scripts
/// run (TASK-18). Order is preserved.
fn executable_lines(src: &str) -> impl Iterator<Item = &str> {
    src.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
}

/// Routing prefixes work in scripts too: `!cmd` forces direct execution, `?text`
/// forces the model. Mirrors the REPL's `split_route`.
#[derive(PartialEq, Clone, Copy)]
enum Route {
    Auto,
    Direct,
    Model,
}

fn split_route(line: &str) -> (String, Route) {
    if let Some(rest) = line.strip_prefix('!') {
        (rest.trim().to_string(), Route::Direct)
    } else if let Some(rest) = line.strip_prefix('?') {
        (rest.trim().to_string(), Route::Model)
    } else {
        (line.to_string(), Route::Auto)
    }
}

/// Try to run `line` directly (pipeline, program, or the `cd`/`exit` builtins),
/// without the model. Returns [`Step::NotACommand`] when the line doesn't
/// resolve to something runnable so the caller can route it to the model;
/// `force` (a `!` prefix) turns that miss into a reported error instead.
async fn run_directly(
    line: &str,
    force: bool,
    session: &mut Session,
    prev_dir: &mut Option<PathBuf>,
) -> Step {
    let path_var = session_path(session);

    // A pipeline `a | b | c` runs directly only when every stage is a real
    // program; otherwise it routes to the model (matching the REPL).
    if let Some(stages) = pipeline::parse(line) {
        let all_resolve = stages
            .iter()
            .all(|s| s.first().is_some_and(|p| resolve_program(p, &session.cwd, &path_var).is_some()));
        if all_resolve {
            match pipeline::run(&stages, session).await {
                Ok(status) => {
                    session.set_last_status(&status);
                    report_nonzero(&status);
                }
                Err(e) => {
                    eprintln!("\x1b[31maish:\x1b[0m {e:#}");
                    session.last_status = 1;
                }
            }
            return Step::Ran;
        }
        if force {
            eprintln!("aish: a pipeline stage isn't an executable — can't run it directly");
            session.last_status = 1;
            return Step::Ran;
        }
        return Step::NotACommand;
    }

    // Single command. `$VAR`/`$?` expand against the session's exports first,
    // then the process environment — what the spawned program would see.
    let Some(mut words) = rc::tokenize_with(line, var_lookup(session)) else {
        if force {
            eprintln!("aish: can't run that directly — it uses shell syntax aish doesn't implement");
            session.last_status = 1;
            return Step::Ran;
        }
        return Step::NotACommand;
    };
    let Some(first) = words.first().cloned() else {
        return Step::NotACommand;
    };

    // Tilde expansion for arguments (the program receives literal argv).
    let home = std::env::var("HOME").unwrap_or_default();
    for w in words.iter_mut().skip(1) {
        if *w == "~" {
            w.clone_from(&home);
        } else if let Some(rest) = w.strip_prefix("~/") {
            *w = format!("{home}/{rest}");
        }
    }

    match first.as_str() {
        "exit" | "logout" => {
            // An explicit numeric operand sets the script's exit status.
            if let Some(code) = words.get(1).and_then(|n| n.parse::<i32>().ok()) {
                session.last_status = code;
            }
            Step::Quit
        }
        "cd" => {
            builtin_cd(words.get(1).map(String::as_str), session, prev_dir);
            Step::Ran
        }
        cmd => {
            let Some(program) = resolve_program(cmd, &session.cwd, &path_var) else {
                if force {
                    eprintln!("aish: {cmd}: command not found");
                    session.last_status = 127;
                    return Step::Ran;
                }
                return Step::NotACommand;
            };
            match tools::run_on_tty(&program.to_string_lossy(), &words[1..], &[], session).await {
                Ok(status) => {
                    session.set_last_status(&status);
                    report_nonzero(&status);
                }
                Err(e) => {
                    eprintln!("\x1b[31maish:\x1b[0m {e:#}");
                    session.last_status = 1;
                }
            }
            Step::Ran
        }
    }
}

/// Print a dim `[exit N]` note for a non-zero status, matching the REPL.
fn report_nonzero(status: &std::process::ExitStatus) {
    if let Some(code) = status.code() {
        if code != 0 {
            eprintln!("\x1b[2m[exit {code}]\x1b[0m");
        }
    }
}

/// `cd` for scripts: keep `$PWD`/`$OLDPWD` and the `cd -` previous dir in step
/// with the move (S4.3 semantics), so spawned children see the real cwd.
fn builtin_cd(arg: Option<&str>, session: &mut Session, prev: &mut Option<PathBuf>) {
    let home = std::env::var("HOME").unwrap_or_default();
    let target = match arg {
        None | Some("~") => PathBuf::from(&home),
        Some("-") => match prev.clone() {
            Some(p) => p,
            None => {
                eprintln!("cd: no previous directory");
                session.last_status = 1;
                return;
            }
        },
        Some(p) => session.cwd.join(p),
    };
    match target.canonicalize() {
        Ok(c) if c.is_dir() => {
            let old = std::mem::replace(&mut session.cwd, c.clone());
            session.set_var("OLDPWD", old.to_string_lossy().into_owned());
            session.set_var("PWD", c.to_string_lossy().into_owned());
            *prev = Some(old);
            session.last_status = 0;
        }
        Ok(c) => {
            eprintln!("cd: {}: not a directory", c.display());
            session.last_status = 1;
        }
        Err(e) => {
            eprintln!("cd: {}: {e}", target.display());
            session.last_status = 1;
        }
    }
}

/// The PATH script dispatch resolves against: the session's exported PATH (rc +
/// profiles) if any, else the process PATH.
fn session_path(session: &Session) -> String {
    session
        .env
        .iter()
        .rev()
        .find(|(k, _)| k == "PATH")
        .map(|(_, v)| v.clone())
        .or_else(|| std::env::var("PATH").ok())
        .unwrap_or_default()
}

/// `$VAR`/`$?` resolver for script lines: session exports first (a user override
/// wins), then `$?` (last exit status), then the process environment.
fn var_lookup(session: &Session) -> impl Fn(&str) -> Option<String> + '_ {
    move |name: &str| {
        if name == "?" {
            return Some(session.last_status.to_string());
        }
        session
            .env
            .iter()
            .rev()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.clone())
            .or_else(|| std::env::var(name).ok())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn executable_lines_skips_blanks_comments_and_shebang() {
        let src = "#!/usr/bin/env aish\n\
                   # a comment\n\
                   \n\
                   ls -la\n\
                   \t  \n\
                   echo hi   \n\
                   #trailing comment\n\
                   cd /tmp";
        let lines: Vec<&str> = executable_lines(src).collect();
        // Shebang, comments, and blank/whitespace-only lines are gone; the real
        // commands survive in order and are trimmed.
        assert_eq!(lines, vec!["ls -la", "echo hi", "cd /tmp"]);
    }

    #[test]
    fn executable_lines_is_empty_for_a_comment_only_script() {
        let src = "#!/usr/bin/env aish\n# nothing to do\n\n";
        assert_eq!(executable_lines(src).count(), 0);
    }

    #[test]
    fn split_route_honors_prefixes() {
        assert!(matches!(split_route("!ls -la"), (l, Route::Direct) if l == "ls -la"));
        assert!(matches!(split_route("?what is up"), (l, Route::Model) if l == "what is up"));
        assert!(matches!(split_route("echo hi"), (l, Route::Auto) if l == "echo hi"));
    }
}
