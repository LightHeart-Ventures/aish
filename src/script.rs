//! Script mode (TASK-17/18): `aish <file> [args…]` runs the file's lines
//! non-interactively, then exits with the status of the last line — the
//! shell-script entry point.
//!
//! Each non-blank, non-comment line is executed as if typed at the prompt: a
//! line that resolves to a program (or a pipeline of programs, or the `cd`
//! builtin) runs directly on the terminal; anything else is handed to the model,
//! exactly like the interactive REPL. `#` comment lines and blank lines are
//! skipped — and because a `#!` shebang line is just a `#` comment, a script
//! beginning with `#!/usr/bin/env aish` Just Works. That makes aish a valid
//! shebang interpreter: `chmod +x script.aish && ./script.aish foo bar` has the
//! kernel run `aish /path/script.aish foo bar`, and TASK-18 wires the trailing
//! `foo bar` through as the positional parameters `$1`/`$2`, with `$0` the
//! script path, `$#` the count, and `$@`/`$*` the whole list (see `var_lookup`).
//! The `!`/`?` route prefixes still force direct/model.
//!
//! No-TTY safety (TASK-18, cron/CI): a script run without a controlling
//! terminal must never block on a confirmation read. Direct dispatch already
//! degrades gracefully (`run_on_tty` skips the `tcsetpgrp` hand-off when stdin
//! isn't a tty); for the model path we pick the confirm hook up front — an
//! interactive tty prompts as usual, no tty auto-denies (a script can't answer
//! a y/N), so a model tool call can't hang an unattended run.
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

/// Execute an aish script file. `args` are the kernel-appended operands a
/// shebang invocation passes through (`aish <script> <args…>`), exposed to the
/// script as the positional parameters `$1`/`$2`/… with `$0` the script path
/// (TASK-18). Returns the exit status of the last executed line (`$?`), matching
/// `sh script`. A read error is surfaced as an `Err` so `main` can report it and
/// exit non-zero.
pub async fn run(
    backend: &Backend,
    session: &mut Session,
    path: &Path,
    args: &[String],
) -> Result<i32> {
    let src = std::fs::read_to_string(path)
        .map_err(|e| anyhow::anyhow!("aish: cannot read script {}: {e}", path.display()))?;
    // Positional parameters: params[0] = $0 (the script path), params[1..] = $1…
    // — the shell convention exposed by `var_lookup`/`positional`.
    let mut params: Vec<String> = Vec::with_capacity(args.len() + 1);
    params.push(path.to_string_lossy().into_owned());
    params.extend(args.iter().cloned());

    // No-TTY safety (cron/CI): pick the model-turn confirm hook ONCE, up front.
    // With a controlling terminal we prompt as usual; without one a script can't
    // answer y/N, so auto-deny rather than block on a stdin read that never
    // returns. Direct dispatch's own tty hand-off is already guarded downstream.
    // SAFETY: isatty is a plain query on fd 0.
    let on_tty = unsafe { libc::isatty(0) == 1 };
    let mut confirm = move |prompt: &str| -> tools::Decision {
        if on_tty {
            confirm_tty(prompt)
        } else {
            tools::Decision::Deny
        }
    };

    let mut prev_dir: Option<PathBuf> = None;
    for raw in executable_lines(&src) {
        let (line, route) = split_route(raw);
        if line.is_empty() {
            continue;
        }
        if route != Route::Model {
            match run_directly(
                &line,
                route == Route::Direct,
                session,
                &params,
                &mut prev_dir,
            )
            .await
            {
                Step::Ran => continue,
                Step::Quit => break,
                Step::NotACommand => {}
            }
        }
        // A non-command line (or a `?`-forced one) goes to the model, exactly as
        // at the interactive prompt. Errors don't abort the script — they set a
        // non-zero `$?` and execution continues, like `sh` without `set -e`.
        match engine::run_turn_with_recovery(backend, session, line, &mut confirm).await {
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
/// `params` are the script's positional parameters (`$0`/`$1`/…) used by
/// `$VAR` expansion.
async fn run_directly(
    line: &str,
    force: bool,
    session: &mut Session,
    params: &[String],
    prev_dir: &mut Option<PathBuf>,
) -> Step {
    let path_var = session_path(session);

    // A pipeline `a | b | c` runs directly only when every stage is a real
    // program; otherwise it routes to the model (matching the REPL).
    if let Some(stages) = pipeline::parse(line) {
        let all_resolve = stages.iter().all(|s| {
            s.argv
                .first()
                .is_some_and(|p| resolve_program(p, &session.cwd, &path_var).is_some())
        });
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

    // Single command. `$VAR`/`$?` and the positional `$0`/`$1`/`$@`/… expand
    // against the script params and the session's exports first, then the
    // process environment — what the spawned program would see.
    let mut words = match rc::tokenize_diagnosed(line, var_lookup(session, params)) {
        Ok(w) => w,
        Err(diag) => {
            // Forced (`!`): surface the coded diagnostic (caret + code + help).
            // Auto: stay silent and route the line to the model.
            if force {
                crate::diag::eprint(&diag);
                session.last_status = 1;
                return Step::Ran;
            }
            return Step::NotACommand;
        }
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

/// Resolve a script positional parameter (`$0`/`$1`/…, `$#`, `$@`/`$*`) from
/// `params` (where `params[0]` is `$0`). Returns `None` when `name` isn't a
/// positional reference, so `var_lookup` can fall through to session exports and
/// the environment. Pure + unit-tested.
fn positional(name: &str, params: &[String]) -> Option<String> {
    match name {
        // `$#` — the count of positional parameters, excluding `$0`.
        "#" => Some(params.len().saturating_sub(1).to_string()),
        // `$@` / `$*` — every positional from `$1` on, space-joined. aish has no
        // word-splitting/quoting distinction here, so the two behave the same.
        "@" | "*" => Some(params.iter().skip(1).cloned().collect::<Vec<_>>().join(" ")),
        // `$0`/`$1`/… — a positional by index; out-of-range is the empty string,
        // matching a POSIX shell. A non-numeric name is not a positional.
        n => n
            .parse::<usize>()
            .ok()
            .map(|i| params.get(i).cloned().unwrap_or_default()),
    }
}

/// `$VAR`/`$?`/`$$` and positional-parameter resolver for script lines.
/// Resolution order: `$?` (last exit status) and `$$` (shell pid) first, then
/// the script's positional parameters (`$0`/`$1`/`$#`/`$@`/…), then session
/// exports (a user override wins), then the process environment.
fn var_lookup<'a>(
    session: &'a Session,
    params: &'a [String],
) -> impl Fn(&str) -> Option<String> + 'a {
    move |name: &str| {
        if name == "?" {
            return Some(session.last_status.to_string());
        }
        if name == "$" {
            // `$$` — the shell's own pid (S4.6), resolved live like the REPL.
            return Some(std::process::id().to_string());
        }
        if let Some(v) = positional(name, params) {
            return Some(v);
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

    // ---- TASK-18: positional parameters ($0/$1/$#/$@/$*) -----------------

    #[test]
    fn positional_resolves_indices_count_and_all() {
        // params[0] = $0 (script path), then $1, $2, …
        let params = vec![
            "/tmp/build.aish".to_string(),
            "release".to_string(),
            "x86_64".to_string(),
        ];
        assert_eq!(positional("0", &params).as_deref(), Some("/tmp/build.aish"));
        assert_eq!(positional("1", &params).as_deref(), Some("release"));
        assert_eq!(positional("2", &params).as_deref(), Some("x86_64"));
        // Out-of-range positionals are the empty string (POSIX), not absent.
        assert_eq!(positional("3", &params).as_deref(), Some(""));
        // `$#` counts the operands, excluding $0.
        assert_eq!(positional("#", &params).as_deref(), Some("2"));
        // `$@` / `$*` join $1.. with spaces (identical here).
        assert_eq!(positional("@", &params).as_deref(), Some("release x86_64"));
        assert_eq!(positional("*", &params).as_deref(), Some("release x86_64"));
        // A non-numeric, non-special name is NOT a positional → None, so the
        // caller falls through to env lookup.
        assert_eq!(positional("HOME", &params), None);
        assert_eq!(positional("PATH", &params), None);
    }

    #[test]
    fn positional_with_no_args_has_zero_count_and_empty_all() {
        // Only $0 present (no operands): $# is 0, $@/$* are empty, $1 is "".
        let params = vec!["/tmp/noargs.aish".to_string()];
        assert_eq!(positional("#", &params).as_deref(), Some("0"));
        assert_eq!(positional("@", &params).as_deref(), Some(""));
        assert_eq!(positional("1", &params).as_deref(), Some(""));
        assert_eq!(
            positional("0", &params).as_deref(),
            Some("/tmp/noargs.aish")
        );
    }

    #[test]
    fn var_lookup_expands_positionals_specials_and_env() {
        let mut session = Session::new().unwrap();
        session.set_var("GREETING", "hello");
        session.last_status = 7;
        let params = vec!["script.aish".to_string(), "world".to_string()];
        let lookup = var_lookup(&session, &params);

        // Positional parameters resolve through var_lookup.
        assert_eq!(lookup("0").as_deref(), Some("script.aish"));
        assert_eq!(lookup("1").as_deref(), Some("world"));
        assert_eq!(lookup("#").as_deref(), Some("1"));
        assert_eq!(lookup("@").as_deref(), Some("world"));
        // Specials: `$?` is the last status, `$$` the live pid.
        assert_eq!(lookup("?").as_deref(), Some("7"));
        assert_eq!(
            lookup("$").as_deref(),
            Some(&*std::process::id().to_string())
        );
        // Session exports still resolve (and aren't shadowed by positionals).
        assert_eq!(lookup("GREETING").as_deref(), Some("hello"));
        // An unknown name is None (var_lookup falls through to the env, which
        // doesn't have this one either).
        assert_eq!(lookup("NO_SUCH_VAR_XYZ_18"), None);
    }

    #[test]
    fn tokenize_expands_positionals_in_a_script_line() {
        // End-to-end: a script line referencing $1/$2/$# tokenizes with the
        // params substituted, exactly as the spawned program would receive them.
        let session = Session::new().unwrap();
        let params = vec![
            "deploy.aish".to_string(),
            "staging".to_string(),
            "v2".to_string(),
        ];
        let words = rc::tokenize_with(
            "echo deploying $1 $2 count $#",
            var_lookup(&session, &params),
        )
        .expect("line tokenizes");
        assert_eq!(
            words,
            vec!["echo", "deploying", "staging", "v2", "count", "2"]
        );

        // `$@` expands to the args space-joined as a SINGLE word — aish never
        // re-splits an expansion (a variable can't smuggle extra argv words).
        let all = rc::tokenize_with("echo $@", var_lookup(&session, &params)).expect("tokenizes");
        assert_eq!(all, vec!["echo", "staging v2"]);
    }
}
