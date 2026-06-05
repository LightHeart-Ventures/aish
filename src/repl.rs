use crate::backend::Backend;
use crate::engine;
use crate::pipeline;
use crate::rc;
use crate::session::Session;
use crate::tools;
use anyhow::Result;
use rustyline::completion::{Completer, Pair};
use rustyline::error::ReadlineError;
use rustyline::highlight::Highlighter;
use rustyline::hint::Hinter;
use rustyline::history::DefaultHistory;
use rustyline::validate::Validator;
use rustyline::{
    Cmd, ConditionalEventHandler, Context, Editor, Event, EventContext, EventHandler, Helper,
    KeyEvent, RepeatCount,
};
use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// Blocking y/N prompt on the controlling TTY.
pub fn confirm_tty(prompt: &str) -> bool {
    print!("\x1b[33mrun?\x1b[0m {prompt} \x1b[33m[y/N]\x1b[0m ");
    std::io::stdout().flush().ok();
    let mut line = String::new();
    if std::io::stdin().read_line(&mut line).is_err() {
        return false;
    }
    matches!(line.trim(), "y" | "Y" | "yes")
}

pub async fn run(mut backend: Backend, mut session: Session) -> Result<()> {
    // Install the process-wide SIGINT handler up front. A Ctrl-C during a
    // direct-dispatch child must interrupt the child (the terminal delivers
    // it to the shared foreground group) — never kill aish itself.
    tokio::spawn(async {
        loop {
            let _ = tokio::signal::ctrl_c().await;
        }
    });

    let rc = rc::load();
    session.env = rc.env;
    let aliases = rc.aliases;

    let mut rl: Editor<AishHelper, DefaultHistory> = Editor::new()?;
    rl.set_helper(Some(AishHelper { cwd: session.cwd.clone() }));
    let history_path = dirs_history_path();
    let _ = rl.load_history(&history_path);

    // Ctrl-O toggles raw tool output. The handler can't reach the Session, so
    // it just flags the request and bails out of the line editor (Interrupt);
    // the loop below performs the toggle, status line, and retroactive reveal.
    let raw_toggle = Arc::new(AtomicBool::new(false));
    rl.bind_sequence(
        KeyEvent::ctrl('O'),
        EventHandler::Conditional(Box::new(CtrlOToggle { pending: raw_toggle.clone() })),
    );

    // Start on a clean screen (interactive terminals only — keep piped output clean).
    // SAFETY: plain isatty query.
    if unsafe { libc::isatty(1) } == 1 {
        print!("\x1b[2J\x1b[H");
    }
    println!("\x1b[1maish\x1b[0m — AI-native shell · {} · :help for commands", backend.describe());

    let mut prev_dir: Option<PathBuf> = None;
    let mut needs_gap = false; // blank line between previous output and the prompt

    loop {
        if needs_gap {
            println!();
            needs_gap = false;
        }
        // Tab completion resolves against the session's cwd, which `cd` mutates.
        if let Some(h) = rl.helper_mut() {
            h.cwd.clone_from(&session.cwd);
        }
        let prompt = format!("\x1b[36m{}\x1b[0m ❯ ", short_cwd(&session));
        match rl.readline(&prompt) {
            Ok(line) => {
                let line = line.trim().to_string();
                if line.is_empty() {
                    continue;
                }
                let _ = rl.add_history_entry(&line);
                needs_gap = true;

                if let Some(cmd) = line.strip_prefix(':') {
                    if handle_colon(cmd, &mut backend, &mut session) {
                        break;
                    }
                    continue;
                }

                if let Some(db) = &session.db {
                    db.record("input", &session.cwd.to_string_lossy(), &line);
                }

                // Explicit routing escape hatches: `!cmd` forces direct
                // execution, `?text` forces the model.
                let (line, route) = split_route(line);
                if line.is_empty() {
                    continue;
                }

                // Shell-first: when the first word is a real executable (or a
                // builtin/alias), run it directly on the terminal like any
                // shell would. Everything else routes to the model.
                if route != Route::Model {
                    match dispatch(&line, route == Route::Direct, &mut session, &aliases, &mut prev_dir).await {
                        Dispatch::Handled => continue,
                        Dispatch::Quit => break,
                        Dispatch::NotACommand => {}
                    }
                }

                // Agentic turn. Ctrl-C aborts it — unless a TTY hand-off is in
                // progress, in which case the SIGINT belongs to the foreground
                // child (the terminal already delivered it there).
                let pre_len = session.history.len();
                let mut aborted = false;
                let mut reply: Option<String> = None;
                {
                    let handoff = session.tty_handoff.clone();
                    let mut confirm = confirm_tty;
                    let turn = engine::run_turn(&backend, &mut session, line, &mut confirm);
                    tokio::pin!(turn);
                    loop {
                        tokio::select! {
                            res = &mut turn => {
                                match res {
                                    Ok(text) => reply = Some(text),
                                    Err(e) => eprintln!("\x1b[31maish:\x1b[0m {e:#}"),
                                }
                                break;
                            }
                            _ = tokio::signal::ctrl_c() => {
                                if !handoff.load(Ordering::SeqCst) {
                                    aborted = true;
                                    break;
                                }
                            }
                        }
                    }
                }
                if aborted {
                    // A half-finished turn can leave a dangling tool_use with no
                    // tool_result — the next request would 400. Roll it back.
                    session.history.truncate(pre_len);
                    println!("\x1b[33m^C\x1b[0m turn aborted");
                } else if let Some(text) = reply {
                    if !text.trim().is_empty() {
                        println!("{}", crate::md::render_stdout(text.trim()));
                    }
                    if let Some(db) = &session.db {
                        db.record("output", &session.cwd.to_string_lossy(), &text);
                    }
                }
            }
            Err(ReadlineError::Interrupted) => {
                // Ctrl-O routes here too (handler returns Interrupt); distinguish
                // it from a plain Ctrl-C clear-line via the toggle flag.
                if raw_toggle.swap(false, Ordering::SeqCst) {
                    toggle_raw_output(&mut session);
                }
                continue;
            }
            Err(ReadlineError::Eof) => break,            // Ctrl-D: exit
            Err(e) => {
                eprintln!("aish: readline error: {e}");
                break;
            }
        }
    }

    let _ = rl.save_history(&history_path);
    println!("bye");
    Ok(())
}

// ---------------------------------------------------------------------------
// Tab completion — filenames and directories, against the session's cwd
// ---------------------------------------------------------------------------

struct AishHelper {
    cwd: PathBuf,
}

impl Completer for AishHelper {
    type Candidate = Pair;
    fn complete(&self, line: &str, pos: usize, _ctx: &Context<'_>) -> rustyline::Result<(usize, Vec<Pair>)> {
        Ok(complete_path(line, pos, &self.cwd))
    }
}
impl Hinter for AishHelper {
    type Hint = String;
}
impl Highlighter for AishHelper {}
impl Validator for AishHelper {}
impl Helper for AishHelper {}

/// Complete the path under the cursor. Splits the current word into a
/// directory part (kept verbatim in the replacement, `~` included) and a name
/// prefix matched against that directory's entries. Directories complete with
/// a trailing `/`; names containing whitespace come back double-quoted so the
/// tokenizer can re-read them.
fn complete_path(line: &str, pos: usize, cwd: &Path) -> (usize, Vec<Pair>) {
    let start = line[..pos].rfind(char::is_whitespace).map(|i| i + 1).unwrap_or(0);
    let word = &line[start..pos];

    let (dir_part, prefix) = match word.rfind('/') {
        Some(i) => (&word[..=i], &word[i + 1..]),
        None => ("", word),
    };
    let home = std::env::var("HOME").unwrap_or_default();
    let base = if dir_part.starts_with('/') {
        PathBuf::from(dir_part)
    } else if let Some(rest) = dir_part.strip_prefix("~/") {
        Path::new(&home).join(rest)
    } else {
        cwd.join(dir_part)
    };

    let Ok(entries) = std::fs::read_dir(&base) else {
        return (start, Vec::new());
    };
    let mut pairs: Vec<Pair> = entries
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            // hidden entries only when explicitly asked for
            if !name.starts_with(prefix) || (name.starts_with('.') && !prefix.starts_with('.')) {
                return None;
            }
            let is_dir = e.file_type().map(|t| t.is_dir()).unwrap_or(false);
            let display = if is_dir { format!("{name}/") } else { name.clone() };
            let mut replacement = format!("{dir_part}{name}");
            if replacement.contains(char::is_whitespace) {
                replacement = format!("\"{replacement}\"");
            }
            if is_dir {
                replacement.push('/');
            }
            Some(Pair { display, replacement })
        })
        .collect();
    pairs.sort_by(|a, b| a.display.cmp(&b.display));
    (start, pairs)
}

// ---------------------------------------------------------------------------
// Direct dispatch — the "real shell" path
// ---------------------------------------------------------------------------

enum Dispatch {
    /// Not an executable/builtin — route the line to the model.
    NotACommand,
    /// Ran (or errored) right here; prompt again.
    Handled,
    /// `exit` / `logout`.
    Quit,
}

#[derive(PartialEq, Clone, Copy)]
enum Route {
    Auto,
    Direct, // `!` prefix
    Model,  // `?` prefix
}

fn split_route(line: String) -> (String, Route) {
    if let Some(rest) = line.strip_prefix('!') {
        (rest.trim().to_string(), Route::Direct)
    } else if let Some(rest) = line.strip_prefix('?') {
        (rest.trim().to_string(), Route::Model)
    } else {
        (line, Route::Auto)
    }
}

/// Real commands that double as English function words. For these, a line of
/// nothing but bare words is almost certainly a question ("who is …",
/// "find me big files"), not an invocation — flags, paths, digits, or quotes
/// flip it back to a command.
const AMBIGUOUS_COMMANDS: &[&str] = &[
    "who", "w", "find", "time", "test", "yes", "look", "last", "watch", "date",
    "which", "whatis", "finger", "write", "wall", "users", "top", "more",
    "head", "tail", "make", "cat", "kill",
];

fn looks_like_prose(line: &str, words: &[String]) -> bool {
    if words.len() < 3
        || !AMBIGUOUS_COMMANDS.contains(&words[0].as_str())
        || line.contains(['"', '\''])
    {
        return false;
    }
    // A comma reads as a sentence, not an argv — "watch for events from atum,
    // let me know…" must go to the model even though `watch` is real.
    if line.contains(", ") {
        return true;
    }
    // Otherwise every word must be plain alphabetic, with trailing sentence
    // punctuation forgiven; flags (-n), paths (a/b), and digits stay commands.
    words.iter().all(|w| {
        let w = w.trim_end_matches([',', '.', '!', '?', ';', ':']);
        !w.is_empty() && w.chars().all(|c| c.is_ascii_alphabetic())
    })
}

async fn dispatch(
    line: &str,
    force: bool,
    session: &mut Session,
    aliases: &HashMap<String, Vec<String>>,
    prev_dir: &mut Option<PathBuf>,
) -> Dispatch {
    // A pipeline (a | b | c) is the one bit of shell syntax aish runs itself:
    // connect each stage's stdout to the next stage's stdin. Run it directly
    // only when every stage is a real program; otherwise route to the model.
    if let Some(stages) = pipeline::parse(line) {
        // Same session-aware PATH the single-command path uses below.
        let path_var = session
            .env
            .iter()
            .rev()
            .find(|(k, _)| k == "PATH")
            .map(|(_, v)| v.clone())
            .or_else(|| std::env::var("PATH").ok())
            .unwrap_or_default();
        if stages.iter().all(|s| {
            s.first().is_some_and(|p| resolve_program(p, &session.cwd, &path_var).is_some())
        }) {
            match pipeline::run(&stages, session).await {
                Ok(status) => {
                    if let Some(code) = status.code() {
                        if code != 0 {
                            eprintln!("\x1b[2m[exit {code}]\x1b[0m");
                        }
                    }
                }
                Err(e) => eprintln!("\x1b[31maish:\x1b[0m {e:#}"),
            }
            return Dispatch::Handled;
        }
        if force {
            eprintln!("aish: a pipeline stage isn't an executable — can't run it directly");
            return Dispatch::Handled;
        }
        return Dispatch::NotACommand;
    }

    // Lines with shell machinery (globs, redirection, …) or that don't
    // tokenize (apostrophes in English) go to the model. `$VAR` references are
    // expanded here against the session's exports first, then the process
    // environment — matching what the spawned program would see.
    let lookup = |name: &str| {
        session
            .env
            .iter()
            .rev()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.clone())
            .or_else(|| std::env::var(name).ok())
    };
    let Some(mut words) = rc::tokenize_with(line, lookup) else {
        if force {
            eprintln!("aish: can't run that directly — it uses shell syntax aish doesn't implement");
            return Dispatch::Handled;
        }
        return Dispatch::NotACommand;
    };
    let Some(first) = words.first() else {
        return Dispatch::NotACommand;
    };

    // English that happens to start with a command word ("who is …") goes to
    // the model — unless the user forced it with `!` or aliased the word.
    if !force && !aliases.contains_key(first) && looks_like_prose(line, &words) {
        return Dispatch::NotACommand;
    }

    if let Some(expansion) = aliases.get(first) {
        let mut expanded = expansion.clone();
        expanded.extend(words.into_iter().skip(1));
        words = expanded;
    }

    // Tilde expansion for arguments — programs receive literal argv.
    let home = std::env::var("HOME").unwrap_or_default();
    for w in words.iter_mut().skip(1) {
        if *w == "~" {
            w.clone_from(&home);
        } else if let Some(rest) = w.strip_prefix("~/") {
            *w = format!("{home}/{rest}");
        }
    }

    match words[0].as_str() {
        "exit" | "logout" => Dispatch::Quit,
        "cd" => {
            builtin_cd(words.get(1).map(String::as_str), session, prev_dir);
            Dispatch::Handled
        }
        cmd => {
            // Resolve against the session's PATH — which includes any
            // `export PATH="$PATH:…"` from ~/.aishrc — falling back to the
            // process PATH when the rc file sets none.
            let path_var = session
                .env
                .iter()
                .rev()
                .find(|(k, _)| k == "PATH")
                .map(|(_, v)| v.clone())
                .or_else(|| std::env::var("PATH").ok())
                .unwrap_or_default();
            let Some(path) = resolve_program(cmd, &session.cwd, &path_var) else {
                if force {
                    eprintln!("aish: {cmd}: command not found");
                    return Dispatch::Handled;
                }
                return Dispatch::NotACommand;
            };
            match tools::run_on_tty(&path.to_string_lossy(), &words[1..], session).await {
                Ok(status) => {
                    if let Some(code) = status.code() {
                        if code != 0 {
                            eprintln!("\x1b[2m[exit {code}]\x1b[0m");
                        }
                    }
                }
                Err(e) => eprintln!("\x1b[31maish:\x1b[0m {e:#}"),
            }
            Dispatch::Handled
        }
    }
}

fn builtin_cd(arg: Option<&str>, session: &mut Session, prev: &mut Option<PathBuf>) {
    let home = std::env::var("HOME").unwrap_or_default();
    let target = match arg {
        None | Some("~") => PathBuf::from(&home),
        Some("-") => match prev.clone() {
            Some(p) => p,
            None => {
                eprintln!("cd: no previous directory");
                return;
            }
        },
        Some(p) => session.cwd.join(p),
    };
    match target.canonicalize() {
        Ok(c) if c.is_dir() => {
            *prev = Some(session.cwd.clone());
            session.cwd = c;
        }
        Ok(c) => eprintln!("cd: {}: not a directory", c.display()),
        Err(e) => eprintln!("cd: {}: {e}", target.display()),
    }
}

/// PATH lookup with the executable bit checked — `which`, basically.
fn resolve_program(cmd: &str, cwd: &Path, path_var: &str) -> Option<PathBuf> {
    fn is_exec(p: &Path) -> bool {
        use std::os::unix::fs::PermissionsExt;
        p.is_file() && p.metadata().map(|m| m.permissions().mode() & 0o111 != 0).unwrap_or(false)
    }
    if cmd.contains('/') {
        let p = if Path::new(cmd).is_absolute() { PathBuf::from(cmd) } else { cwd.join(cmd) };
        return is_exec(&p).then_some(p);
    }
    for dir in path_var.split(':').filter(|d| !d.is_empty()) {
        let p = Path::new(dir).join(cmd);
        if is_exec(&p) {
            return Some(p);
        }
    }
    None
}

/// Ctrl-O key handler: signal a raw-output toggle and leave the line editor so
/// the run loop can act on it. Returns `Cmd::Interrupt` to discard the typed
/// line without submitting it.
struct CtrlOToggle {
    pending: Arc<AtomicBool>,
}

impl ConditionalEventHandler for CtrlOToggle {
    fn handle(&self, _: &Event, _: RepeatCount, _: bool, _: &EventContext) -> Option<Cmd> {
        self.pending.store(true, Ordering::SeqCst);
        Some(Cmd::Interrupt)
    }
}

/// Flip raw tool output, print one-line status, and — when switching on —
/// reveal the most recent turn's raw results.
fn toggle_raw_output(session: &mut Session) {
    session.raw_tool_output = !session.raw_tool_output;
    if session.raw_tool_output {
        println!("\x1b[2mraw tool output on\x1b[0m");
        engine::reveal_last_turn(session);
    } else {
        println!("\x1b[2mraw tool output off\x1b[0m");
    }
}

/// Returns true when the REPL should exit.
fn handle_colon(cmd: &str, backend: &mut Backend, session: &mut Session) -> bool {
    let mut parts = cmd.split_whitespace();
    match parts.next() {
        Some("q" | "quit" | "exit") => return true,
        Some("help") => {
            println!(
                "type a command (first word in PATH) to run it directly — anything else goes to the model\n\
                 prefix ! to force direct execution, ? to force the model\n\
                 :mode <paranoid|careful|normal|yolo> confirmation level (paranoid asks for everything,\n\
                                                     normal only for write/create/delete, yolo never)\n\
                 :model <opus|sonnet|haiku|full-id>  switch model\n\
                 :backend <claude|local>             switch backend\n\
                 :yolo                               toggle yolo mode\n\
                 :new                                clear conversation history\n\
                 Ctrl-O                              toggle raw tool output (show/squelch tool results)\n\
                 :quit                               exit (also Ctrl-D or `exit`)"
            );
        }
        Some("model") => match parts.next() {
            Some(m) => {
                let id = match m {
                    "opus" => "claude-opus-4-8",
                    "sonnet" => "claude-sonnet-4-6",
                    "haiku" => "claude-haiku-4-5",
                    other => other,
                };
                backend.set_model(id.to_string());
                println!("model → {}", backend.describe());
            }
            None => println!("{}", backend.describe()),
        },
        Some("backend") => match parts.next() {
            Some("claude") => {
                if matches!(backend, Backend::Claude(_)) {
                    println!("already on {}", backend.describe());
                } else {
                    match Backend::new_claude("claude-opus-4-8".into()) {
                        Ok(b) => {
                            *backend = b;
                            println!("backend → {}", backend.describe());
                        }
                        Err(e) => println!("can't switch: {e:#}"),
                    }
                }
            }
            #[cfg(feature = "local")]
            Some("local") => {
                if matches!(backend, Backend::Local(_)) {
                    println!("already on {}", backend.describe());
                } else {
                    *backend = Backend::new_local();
                    println!(
                        "backend → {} (loads on first use; MCP tools off — they don't fit a small context window)",
                        backend.describe()
                    );
                }
            }
            #[cfg(not(feature = "local"))]
            Some("local") => println!("built without the local feature (cargo build --features local)"),
            _ => println!("usage: :backend <claude|local>"),
        },
        Some("mode") => match parts.next().and_then(crate::session::Mode::parse) {
            Some(m) => {
                session.mode = m;
                println!("mode → {}", describe_mode(m));
            }
            None => println!(
                "mode: {}\nusage: :mode <paranoid|careful|normal|yolo>",
                describe_mode(session.mode)
            ),
        },
        Some("yolo") => {
            // legacy toggle: yolo ⇄ normal
            session.mode = if session.mode == crate::session::Mode::Yolo {
                crate::session::Mode::Normal
            } else {
                crate::session::Mode::Yolo
            };
            println!("mode → {}", describe_mode(session.mode));
        }
        Some("new") => {
            session.history.clear();
            println!("history cleared");
        }
        Some(other) => println!("unknown command :{other} — try :help"),
        None => {}
    }
    false
}

fn describe_mode(m: crate::session::Mode) -> String {
    use crate::session::Mode;
    match m {
        Mode::Paranoid => "paranoid — confirm every tool call".into(),
        Mode::Careful => "careful — confirm anything not provably read-only".into(),
        Mode::Normal => "normal — confirm write/create/delete".into(),
        Mode::Yolo => "\x1b[31myolo — nothing is confirmed\x1b[0m".into(),
    }
}

fn short_cwd(session: &Session) -> String {
    let cwd = session.cwd.display().to_string();
    match std::env::var("HOME") {
        Ok(home) if cwd.starts_with(&home) => cwd.replacen(&home, "~", 1),
        _ => cwd,
    }
}

fn dirs_history_path() -> std::path::PathBuf {
    std::path::PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".aish_history")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_completion() {
        let dir = std::env::temp_dir().join(format!("aish_complete_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("subdir")).unwrap();
        std::fs::write(dir.join("sub.txt"), "").unwrap();
        std::fs::write(dir.join("other.txt"), "").unwrap();
        std::fs::write(dir.join(".hidden"), "").unwrap();
        std::fs::write(dir.join("with space.txt"), "").unwrap();

        // mid-line word, relative to cwd
        let (start, pairs) = complete_path("cat su", 6, &dir);
        assert_eq!(start, 4);
        let reps: Vec<&str> = pairs.iter().map(|p| p.replacement.as_str()).collect();
        assert_eq!(reps, vec!["sub.txt", "subdir/"], "got: {reps:?}");

        // hidden entries only with an explicit dot prefix
        let (_, pairs) = complete_path("ls .h", 5, &dir);
        assert_eq!(pairs[0].replacement, ".hidden");
        let (_, pairs) = complete_path("ls ", 3, &dir);
        assert!(pairs.iter().all(|p| !p.replacement.starts_with('.')));

        // whitespace in a name → double-quoted replacement
        let (_, pairs) = complete_path("cat wi", 6, &dir);
        assert_eq!(pairs[0].replacement, "\"with space.txt\"");

        // explicit directory part is kept verbatim in the replacement
        let line = "ls subdir/";
        let (start, _) = complete_path(line, line.len(), &dir);
        assert_eq!(start, 3);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn prose_detection() {
        let prose = |l: &str| looks_like_prose(l, &rc::tokenize(l).unwrap());
        // English questions that start with a command word → model
        assert!(prose("who is zachary hohertz"));
        assert!(prose("find me big files"));
        assert!(prose("make this file executable"));
        // real invocations stay direct
        assert!(!prose("who"));
        assert!(!prose("who -a"));
        assert!(!prose("which ls"));
        assert!(!prose("find . -name foo"));
        assert!(!prose("tail logfile"));
        assert!(!prose("echo hello there world")); // echo isn't ambiguous
        assert!(!prose("cat \"my file\" backup")); // quotes signal shell intent

        // sentence punctuation is prose evidence, not command evidence
        assert!(prose("watch for events from atum, let me know about activity in this sprint"));
        assert!(prose("find big files, oldest first"));
        // `?` is a glob metachar: tokenize already rejects it → model
        assert!(rc::tokenize("who is on this machine right now?").is_none());
        // …but flags/paths still win even with a trailing comma elsewhere
        assert!(!prose("watch -n 5 free"));
        assert!(!prose("tail logs/app.log"));
    }

    #[test]
    fn route_prefixes() {
        assert!(matches!(split_route("!who is x".into()), (l, Route::Direct) if l == "who is x"));
        assert!(matches!(split_route("?ls".into()), (l, Route::Model) if l == "ls"));
        assert!(matches!(split_route("ls".into()), (l, Route::Auto) if l == "ls"));
    }
}
