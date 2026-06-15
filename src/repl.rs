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
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Blocking y/N/a prompt on the controlling TTY. `a` ("always") allows this
/// call and persists the tool/command so it never prompts again.
pub fn confirm_tty(prompt: &str) -> tools::Decision {
    use tools::Decision;
    print!("\x1b[33mrun?\x1b[0m {prompt} \x1b[33m[y/N/a]\x1b[0m ");
    std::io::stdout().flush().ok();
    let mut line = String::new();
    if std::io::stdin().read_line(&mut line).is_err() {
        return Decision::Deny;
    }
    match line.trim() {
        "y" | "Y" | "yes" => Decision::AllowOnce,
        "a" | "A" | "always" => Decision::AlwaysAllow,
        _ => Decision::Deny,
    }
}

pub async fn run(mut backend: Backend, mut session: Session) -> Result<()> {
    // Job-control signal disposition (TASK-115): aish ignores SIGINT/QUIT/TSTP/
    // TTOU/TTIN so a Ctrl-C/Ctrl-\/Ctrl-Z reaches the foreground child's process
    // group (run_on_tty hands it the terminal) instead of killing or suspending
    // the shell; foreground children restore the default disposition in pre_exec.
    // SIGINT is also still observed by the ctrl_c task below for model-turn aborts
    // — reconciling that overlap (and removing tty_handoff) is TASK-116.
    tools::ignore_job_control_signals();

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
    let aliases = Arc::new(rc.aliases);

    let mut rl: Editor<AishHelper, DefaultHistory> = Editor::new()?;
    rl.set_helper(Some(AishHelper {
        cwd: session.cwd.clone(),
        path: session_path(&session),
        aliases: aliases.clone(),
        cmd_cache: Arc::new(Mutex::new(None)),
    }));
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
                    if handle_colon(cmd, &mut backend, &mut session).await {
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

                // Model turn: set the activity/reply apart from the typed line
                // (direct commands above stay shell-immediate on purpose).
                println!();

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
    /// Session PATH (rc exports + process PATH) used for command-name completion.
    path: String,
    /// Aliases from ~/.aishrc — offered as command names alongside PATH + builtins.
    aliases: Arc<HashMap<String, Vec<String>>>,
    /// Lazily-populated, TTL-refreshed cache of executable names on PATH.
    cmd_cache: Arc<Mutex<Option<CmdCache>>>,
}

/// Builtins aish handles itself — always offered as command-name completions.
const BUILTINS: &[&str] = &["cd", "exit", "logout"];

/// How long a PATH scan stays cached before it's re-scanned (picks up newly
/// installed binaries without re-statting every PATH dir on each TAB).
const CMD_CACHE_TTL: Duration = Duration::from_secs(10);

/// A cached PATH scan, keyed on the PATH string it was taken from.
struct CmdCache {
    path: String,
    scanned_at: Instant,
    names: Vec<String>,
}

impl Completer for AishHelper {
    type Candidate = Pair;
    fn complete(&self, line: &str, pos: usize, _ctx: &Context<'_>) -> rustyline::Result<(usize, Vec<Pair>)> {
        let start = line[..pos].rfind(char::is_whitespace).map(|i| i + 1).unwrap_or(0);
        // Word one (the command position): everything before the cursor's word is
        // blank. Complete against PATH binaries + builtins + aliases. Later words
        // get flag/subcommand completion for known commands, falling back to
        // filenames.
        if line[..start].trim().is_empty() {
            return Ok(self.complete_command(line, pos, start));
        }
        Ok(complete_line(line, pos, &self.cwd))
    }
}

impl AishHelper {
    /// Command-name completion for the first word: union of cached PATH
    /// executables, builtins, and alias names matching the typed prefix. A
    /// prefix containing `/` is a path (`./script`, `/usr/bin/l`), so defer to
    /// filename completion.
    fn complete_command(&self, line: &str, pos: usize, start: usize) -> (usize, Vec<Pair>) {
        let prefix = &line[start..pos];
        if prefix.contains('/') {
            return complete_path(line, pos, &self.cwd);
        }
        let mut names = self.cached_path_commands();
        names.extend(BUILTINS.iter().map(|s| s.to_string()));
        names.extend(self.aliases.keys().cloned());
        names.sort();
        names.dedup();
        let pairs: Vec<Pair> = names
            .into_iter()
            .filter(|n| n.starts_with(prefix))
            .map(|n| Pair { display: n.clone(), replacement: n })
            .collect();
        (start, pairs)
    }

    /// PATH executable names, re-scanning only when the PATH changed or the
    /// cached scan aged past `CMD_CACHE_TTL`.
    fn cached_path_commands(&self) -> Vec<String> {
        let mut cache = self.cmd_cache.lock().unwrap();
        let fresh = cache
            .as_ref()
            .is_some_and(|c| c.path == self.path && c.scanned_at.elapsed() < CMD_CACHE_TTL);
        if !fresh {
            *cache = Some(CmdCache {
                path: self.path.clone(),
                scanned_at: Instant::now(),
                names: scan_path_commands(&self.path),
            });
        }
        cache.as_ref().unwrap().names.clone()
    }
}

/// Sorted, de-duplicated executable basenames found across every directory on
/// `path_var`. Symlinks are followed (matching `resolve_program`); the exec bit
/// must be set.
fn scan_path_commands(path_var: &str) -> Vec<String> {
    use std::os::unix::fs::PermissionsExt;
    let mut names: Vec<String> = Vec::new();
    for dir in path_var.split(':').filter(|d| !d.is_empty()) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        for e in entries.flatten() {
            let is_exec = std::fs::metadata(e.path())
                .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
                .unwrap_or(false);
            if is_exec {
                names.push(e.file_name().to_string_lossy().into_owned());
            }
        }
    }
    names.sort();
    names.dedup();
    names
}

/// The PATH aish dispatches against: the rc-exported PATH if any, else the
/// process PATH.
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

impl Hinter for AishHelper {
    type Hint = String;
}
impl Highlighter for AishHelper {}
impl Validator for AishHelper {}
impl Helper for AishHelper {}

// ---------------------------------------------------------------------------
// Command-aware completion (word 2+) — static tables
// ---------------------------------------------------------------------------
//
// Scope decision (TASK-11): static tables, not a carapace-style spec source.
// A spec-driven source would mean vendoring + parsing an external corpus of
// hundreds of tool definitions and carrying a parser/dependency for it. The
// task only asks for four high-traffic tools (git, cargo, docker, kubectl),
// so a handful of `&'static [&str]` tables is the minimum that satisfies it,
// stays dependency-free, and is trivially unit-testable. Any command without a
// table — or any word past the subcommand slot — degrades to path completion.

/// Static completion table for one command: its subcommands (offered in the
/// first-argument slot) and its common flags (offered when the word starts
/// with `-`).
struct CmdSpec {
    subcommands: &'static [&'static str],
    flags: &'static [&'static str],
}

/// Look up the static spec for a command name. Returns `None` for any command
/// we don't have a table for, which is the signal to fall back to path
/// completion (graceful degradation).
fn command_spec(cmd: &str) -> Option<CmdSpec> {
    let spec = match cmd {
        "git" => CmdSpec {
            subcommands: &[
                "add", "branch", "checkout", "cherry", "cherry-pick", "clone",
                "commit", "config", "diff", "fetch", "init", "log", "merge",
                "mv", "pull", "push", "rebase", "remote", "reset", "restore",
                "revert", "rm", "show", "stash", "status", "switch", "tag",
            ],
            flags: &[
                "--help", "--version", "--bare", "--git-dir", "--work-tree",
                "--paginate", "--no-pager",
            ],
        },
        "cargo" => CmdSpec {
            subcommands: &[
                "add", "bench", "build", "check", "clean", "clippy", "doc",
                "fetch", "fix", "fmt", "init", "install", "new", "publish",
                "remove", "run", "search", "test", "tree", "update", "vendor",
            ],
            flags: &[
                "--help", "--version", "--release", "--verbose", "--quiet",
                "--offline", "--locked", "--frozen", "--all-features",
                "--no-default-features", "--features", "--target",
                "--manifest-path",
            ],
        },
        "docker" => CmdSpec {
            subcommands: &[
                "attach", "build", "commit", "container", "cp", "create",
                "exec", "image", "images", "info", "inspect", "kill", "logs",
                "network", "pause", "ps", "pull", "push", "rename", "restart",
                "rm", "rmi", "run", "start", "stop", "system", "tag", "top",
                "unpause", "version", "volume",
            ],
            flags: &["--help", "--version", "--config", "--log-level"],
        },
        "kubectl" => CmdSpec {
            subcommands: &[
                "apply", "attach", "config", "cordon", "cp", "create",
                "delete", "describe", "drain", "edit", "exec", "explain",
                "expose", "get", "logs", "patch", "port-forward", "proxy",
                "rollout", "run", "scale", "set", "top", "uncordon", "version",
                "wait",
            ],
            flags: &[
                "--help", "--namespace", "--context", "--kubeconfig",
                "--output", "--all-namespaces",
            ],
        },
        _ => return None,
    };
    Some(spec)
}

/// Build completion `Pair`s for the candidates that start with `prefix`.
fn matches(candidates: &[&str], prefix: &str) -> Vec<Pair> {
    candidates
        .iter()
        .filter(|c| c.starts_with(prefix))
        .map(|c| Pair { display: (*c).to_string(), replacement: (*c).to_string() })
        .collect()
}

/// Top-level completion dispatcher. Completes flags and subcommands for known
/// commands at word 2+, and falls back to path completion everywhere else
/// (word 1, unknown commands, and any known-command slot with no table match).
fn complete_line(line: &str, pos: usize, cwd: &Path) -> (usize, Vec<Pair>) {
    let start = line[..pos].rfind(char::is_whitespace).map(|i| i + 1).unwrap_or(0);
    let word = &line[start..pos];

    // First whitespace-delimited token on the line is the command name.
    let cmd = line.split_whitespace().next().unwrap_or("");

    // How many complete tokens precede the word under the cursor. 0 means we're
    // still typing the command itself → path completion (the Completer impl
    // routes that case to command-name completion before calling here).
    let words_before = line[..start].split_whitespace().count();
    if words_before == 0 {
        return complete_path(line, pos, cwd);
    }

    if let Some(spec) = command_spec(cmd) {
        if word.starts_with('-') {
            let pairs = matches(spec.flags, word);
            if !pairs.is_empty() {
                return (start, pairs);
            }
        } else if words_before == 1 {
            // First-argument slot → subcommand completion.
            let pairs = matches(spec.subcommands, word);
            if !pairs.is_empty() {
                return (start, pairs);
            }
        }
        // No table match → fall through to path completion (graceful).
    }

    complete_path(line, pos, cwd)
}

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
    "which", "what", "whatis", "finger", "write", "wall", "users", "top", "more",
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

/// A lone confirmation token typed at the prompt (`y`, `yes`, `n`, `no`) is
/// almost certainly a stray answer to a prompt that is no longer there — not a
/// request to run the `yes` flood. Route it to the model rather than dispatch
/// `yes`/`y` as a command. Case-insensitive; only a single bare word qualifies,
/// so `yes hello` and a forced `!yes` still run directly.
fn is_stray_confirmation(words: &[String]) -> bool {
    matches!(words, [w] if matches!(w.to_ascii_lowercase().as_str(), "y" | "yes" | "n" | "no"))
}

/// Variable resolver for direct-dispatch `$VAR` expansion. Resolution order:
/// session `export`s first (so a user override wins), then the TASK-13
/// last-output bindings `$LAST` / `$_` (the most recent recorded output,
/// truncated per the last-output policy), then the process environment. This
/// lets the next line reference the previous output without re-running it —
/// e.g. `echo $LAST`.
fn var_lookup(session: &Session) -> impl Fn(&str) -> Option<String> + '_ {
    move |name: &str| {
        if name == "?" {
            return Some(session.last_status.to_string());
        }
        if let Some(v) = session.env.iter().rev().find(|(k, _)| k == name).map(|(_, v)| v.clone()) {
            return Some(v);
        }
        if matches!(name, "LAST" | "_") {
            if let Some(out) = session.last_output() {
                return Some(out);
            }
        }
        std::env::var(name).ok()
    }
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
                    session.set_last_status(&status);
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
    let Some(mut words) = rc::tokenize_with(line, var_lookup(session)) else {
        if force {
            eprintln!("aish: can't run that directly — it uses shell syntax aish doesn't implement");
            return Dispatch::Handled;
        }
        return Dispatch::NotACommand;
    };
    let Some(first) = words.first() else {
        return Dispatch::NotACommand;
    };

    // English that happens to start with a command word ("who is …"), or a bare
    // stray confirmation ("yes"), goes to the model — unless the user forced it
    // with `!` or aliased the word.
    if !force
        && !aliases.contains_key(first)
        && (looks_like_prose(line, &words) || is_stray_confirmation(&words))
    {
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
            match tools::run_on_tty(&path.to_string_lossy(), &words[1..], &[], session).await {
                Ok(status) => {
                    session.set_last_status(&status);
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
pub(crate) fn resolve_program(cmd: &str, cwd: &Path, path_var: &str) -> Option<PathBuf> {
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
async fn handle_colon(cmd: &str, backend: &mut Backend, session: &mut Session) -> bool {
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
                 :mcp [list|status]                  list connected MCP servers\n\
                 :mcp reconnect [name|all]           restart MCP server(s)\n\
                 :mcp add <name> <command|url> [args] connect + save an MCP server (~/.aish/.mcp.json)\n\
                 :mcp remove <name>                  disconnect + unsave an MCP server\n\
                 :mcp tools [name]                   list MCP tools\n\
                 :yolo                               toggle yolo mode\n\
                 :new                                clear conversation history\n\
                 :batch <on|off|status|clear>        interactive batch mode: agent offloads deferrable\n\
                                                     work to background Anthropic batches (Opus, ~50%\n\
                                                     cheaper); jobs persist + reattach across restarts\n\
                 :batch model <opus|sonnet|haiku|id> model background batches run on (default opus)\n\
                 :jobs                               list background jobs\n\
                 :kill <id>                          kill a background job\n\
                 :allow                              list always-allowed tools/commands\n\
                 :allow remove <tool>                revoke an always-allowed tool/command\n\
                 a at a prompt                       always-allow this tool (see :allow)\n\
                 Ctrl-O                              toggle raw tool output (show/squelch tool results)\n\
                 :quit                               exit (also Ctrl-D or `exit`)"
            );
        }
        Some("jobs") => {
            let jobs = session.jobs.lock().unwrap();
            if jobs.is_empty() {
                println!("no background jobs");
            }
            for j in jobs.iter() {
                println!("[{}] {} — {}", j.id, j.status(), j.desc);
            }
        }
        Some("kill") => match parts.next().and_then(|s| s.parse::<usize>().ok()) {
            Some(id) => {
                let jobs = session.jobs.lock().unwrap();
                match jobs.iter().find(|j| j.id == id) {
                    Some(j) if j.kill() => println!("job {id} killed"),
                    Some(j) => println!("job {id} already finished ({})", j.status()),
                    None => println!("no such job: {id}"),
                }
            }
            None => println!("usage: :kill <job-id>"),
        },
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
        Some("allow") => handle_allow(parts.next(), parts.next(), session),
        Some("batch") => handle_batch(parts.next(), parts.next(), session),
        Some("mcp") => handle_mcp(parts.collect(), session).await,
        Some(other) => println!("unknown command :{other} — try :help"),
        None => {}
    }
    false
}

/// `:mcp` manages MCP servers (like Claude Code's `/mcp`): list, reconnect, add,
/// remove, and inspect tools. Adds/removes persist to ~/.aish/.mcp.json.
async fn handle_mcp(args: Vec<&str>, session: &mut Session) {
    match args.split_first() {
        None | Some((&"list", _)) | Some((&"status", _)) => {
            let servers = session.mcp.status();
            if servers.is_empty() {
                println!("no MCP servers connected");
                return;
            }
            for s in servers {
                println!(
                    "  {} [{}] {} — {} tool{}, {} skill{}",
                    s.name,
                    s.kind,
                    s.detail,
                    s.tools,
                    if s.tools == 1 { "" } else { "s" },
                    s.prompts,
                    if s.prompts == 1 { "" } else { "s" },
                );
            }
        }
        Some((&"reconnect", rest)) => {
            if rest.is_empty() || (rest.len() == 1 && rest[0] == "all") {
                let results = session.mcp.reconnect_all().await;
                if results.is_empty() {
                    println!("no MCP servers to reconnect");
                }
                for (name, res) in results {
                    match res {
                        Ok(()) => println!("reconnected {name}"),
                        Err(e) => println!("reconnect {name}: {e:#}"),
                    }
                }
            } else {
                for name in rest {
                    match session.mcp.reconnect(name).await {
                        Ok(()) => println!("reconnected {name}"),
                        Err(e) => println!("reconnect {name}: {e:#}"),
                    }
                }
            }
        }
        Some((&"add", rest)) => {
            let Some((&name, tail)) = rest.split_first() else {
                println!("usage: :mcp add <name> <command|url> [args…]");
                return;
            };
            let Some((&first, extra)) = tail.split_first() else {
                println!("usage: :mcp add <name> <command|url> [args…]");
                return;
            };
            let spec = if first.starts_with("http://") || first.starts_with("https://") {
                serde_json::json!({"type": "http", "url": first})
            } else {
                serde_json::json!({"command": first, "args": extra})
            };
            match session.mcp.add(name, spec, true).await {
                Ok(()) => println!("connected and saved {name}"),
                Err(e) => println!("mcp add {name}: {e:#}"),
            }
        }
        Some((&"remove" | &"rm", rest)) => {
            let Some(&name) = rest.first() else {
                println!("usage: :mcp remove <name>");
                return;
            };
            match session.mcp.remove(name) {
                Ok((false, _)) => println!("no connected server {name}"),
                Ok((true, true)) => println!("disconnected and unsaved {name}"),
                Ok((true, false)) => {
                    println!("disconnected {name} (defined in project .mcp.json — it returns on restart)")
                }
                Err(e) => println!("mcp remove {name}: {e:#}"),
            }
        }
        Some((&"tools", rest)) => {
            let names: Vec<String> = if rest.is_empty() {
                session.mcp.server_names()
            } else {
                rest.iter().map(|s| s.to_string()).collect()
            };
            if names.is_empty() {
                println!("no MCP servers connected");
            }
            for n in names {
                match session.mcp.tools_of(&n) {
                    Some(tools) if tools.is_empty() => println!("{n}: (no tools)"),
                    Some(tools) => {
                        println!("{n}:");
                        for (t, ro) in tools {
                            println!("  mcp__{n}__{t}{}", if ro { "  (read-only)" } else { "" });
                        }
                    }
                    None => println!("no such server {n}"),
                }
            }
        }
        Some((other, _)) => {
            println!("unknown :mcp subcommand '{other}' — usage: :mcp [list|reconnect|add|remove|tools]")
        }
    }
}

/// `:allow` lists the always-allowed tools; `:allow remove <tool>` revokes one.
fn handle_allow(sub: Option<&str>, arg: Option<&str>, session: &Session) {
    let Some(db) = session.db.as_ref() else {
        println!("allow: persistent store unavailable");
        return;
    };
    match sub {
        None => match db.allowed_tools() {
            Ok(tools) if tools.is_empty() => {
                println!("no always-allowed tools — press 'a' at a confirmation prompt to add one")
            }
            Ok(tools) => {
                println!("always-allowed tools:");
                for (t, ts) in tools {
                    println!("  {t}  ({ts})");
                }
            }
            Err(e) => println!("allow: {e:#}"),
        },
        Some("remove") => match arg {
            Some(tool) => match db.revoke(tool) {
                Ok(true) => println!("removed {tool} from the always-allow list"),
                Ok(false) => println!("{tool} wasn't on the always-allow list"),
                Err(e) => println!("allow: {e:#}"),
            },
            None => println!("usage: :allow remove <tool>"),
        },
        Some(other) => println!("unknown :allow subcommand '{other}' — usage: :allow [remove <tool>]"),
    }
}

/// `:batch` toggles/inspects interactive batch mode. `:batch` or `:batch status`
/// reports the mode and lists this session's batch jobs; `:batch on|off` flips
/// the (persisted) flag; `:batch model <id>` sets the model batches run on.
fn handle_batch(sub: Option<&str>, arg: Option<&str>, session: &mut Session) {
    let persist = |session: &Session| {
        if let Some(db) = session.db.as_ref() {
            let _ = db.set_setting("batch_mode", if session.batch_mode { "true" } else { "false" });
        }
    };
    match sub {
        Some("on") => {
            session.batch_mode = true;
            persist(session);
            println!(
                "batch mode on — the agent can offload deferrable work to background batches on {} (takes effect next turn)",
                session.batch_model
            );
        }
        Some("off") => {
            session.batch_mode = false;
            persist(session);
            println!("batch mode off");
        }
        Some("model") => match arg {
            Some(m) => {
                let id = match m {
                    "opus" => "claude-opus-4-8",
                    "sonnet" => "claude-sonnet-4-6",
                    "haiku" => "claude-haiku-4-5",
                    other => other,
                };
                session.batch_model = id.to_string();
                println!("batch model → {}", session.batch_model);
            }
            None => println!("batch model: {}\nusage: :batch model <opus|sonnet|haiku|full-id>", session.batch_model),
        },
        Some("clear") => {
            // Drop finished (done/failed) jobs from both the store and memory.
            if let Some(store) = session.batch_store.as_ref() {
                match store.clear_finished() {
                    Ok(n) => println!("cleared {n} finished batch job(s)"),
                    Err(e) => println!("batch clear: {e:#}"),
                }
            } else {
                println!("cleared (session-only — no persistent store)");
            }
            session
                .batch_jobs
                .lock()
                .unwrap()
                .retain(|j| !matches!(j.status().as_str(), "done" | "failed"));
        }
        None | Some("status") => {
            println!(
                "batch mode: {} · model: {}",
                if session.batch_mode { "on" } else { "off" },
                session.batch_model
            );
            let jobs = session.batch_jobs.lock().unwrap();
            if jobs.is_empty() {
                println!("no batch jobs");
            } else {
                for j in jobs.iter() {
                    println!("  {}", j.summary_line());
                }
            }
        }
        Some(other) => {
            println!("unknown :batch subcommand '{other}' — usage: :batch [on|off|status|clear|model <id>]")
        }
    }
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

    fn make_exec(path: &Path) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::write(path, "#!/bin/sh\n").unwrap();
        let mut perm = std::fs::metadata(path).unwrap().permissions();
        perm.set_mode(0o755);
        std::fs::set_permissions(path, perm).unwrap();
    }

    fn helper_for(dir: &Path, aliases: HashMap<String, Vec<String>>) -> AishHelper {
        AishHelper {
            cwd: dir.to_path_buf(),
            path: dir.to_string_lossy().into_owned(),
            aliases: Arc::new(aliases),
            cmd_cache: Arc::new(Mutex::new(None)),
        }
    }

    #[test]
    fn command_completion_word_one() {
        let dir = std::env::temp_dir().join(format!("aish_cmd_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        make_exec(&dir.join("cargo"));
        make_exec(&dir.join("cat"));
        std::fs::write(dir.join("README"), "").unwrap(); // non-exec: excluded

        let mut aliases = HashMap::new();
        aliases.insert("gs".to_string(), vec!["git".to_string(), "status".to_string()]);
        let helper = helper_for(&dir, aliases);

        // AC1: car<TAB> at the command position offers cargo
        let (start, pairs) = helper.complete_command("car", 3, 0);
        assert_eq!(start, 0);
        let reps: Vec<&str> = pairs.iter().map(|p| p.replacement.as_str()).collect();
        assert_eq!(reps, vec!["cargo"], "got: {reps:?}");

        // builtins are offered as command names
        let (_, pairs) = helper.complete_command("ex", 2, 0);
        assert!(pairs.iter().any(|p| p.replacement == "exit"), "exit missing");

        // aliases are offered as command names
        let (_, pairs) = helper.complete_command("g", 1, 0);
        assert!(pairs.iter().any(|p| p.replacement == "gs"), "alias gs missing");

        // non-executable files are not offered. (rustyline's Pair has no Debug;
        // report the replacement strings — `{pairs:?}` broke main's test build.)
        let (_, pairs) = helper.complete_command("READ", 4, 0);
        let names: Vec<&str> = pairs.iter().map(|p| p.replacement.as_str()).collect();
        assert!(names.is_empty(), "non-exec README offered: {names:?}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn command_completion_defers_to_path_for_later_words_and_slashes() {
        let dir = std::env::temp_dir().join(format!("aish_cmd2_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        make_exec(&dir.join("cargo"));
        std::fs::write(dir.join("sub.txt"), "").unwrap();
        let helper = helper_for(&dir, HashMap::new());

        // word two routes to filename completion, not command names
        let history = DefaultHistory::new();
        let ctx = Context::new(&history);
        let (start, pairs) = helper.complete("cat su", 6, &ctx).unwrap();
        assert_eq!(start, 4);
        assert!(pairs.iter().any(|p| p.replacement == "sub.txt"));
        assert!(!pairs.iter().any(|p| p.replacement == "cargo"));

        // a slash in word one means it's a path (e.g. ./script) → filename
        // completion (the dir part is kept verbatim in the replacement).
        let (_, pairs) = helper.complete_command("./su", 4, 0);
        assert!(pairs.iter().any(|p| p.display == "sub.txt"));
        assert!(pairs.iter().any(|p| p.replacement == "./sub.txt"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn command_scan_is_cached() {
        let dir = std::env::temp_dir().join(format!("aish_cmd3_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        make_exec(&dir.join("foo"));
        let helper = helper_for(&dir, HashMap::new());

        let first = helper.cached_path_commands();
        assert!(first.contains(&"foo".to_string()));

        // A binary added after the first scan must NOT appear within the TTL —
        // proving the scan is served from cache (AC2: no re-scan on every TAB).
        make_exec(&dir.join("bar"));
        let second = helper.cached_path_commands();
        assert!(!second.contains(&"bar".to_string()), "scan was not cached");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn command_subcommand_and_flag_completion() {
        let cwd = std::env::temp_dir();
        let reps = |line: &str| -> Vec<String> {
            let (_, pairs) = complete_line(line, line.len(), &cwd);
            pairs.into_iter().map(|p| p.replacement).collect()
        };

        // AC1: `git che<TAB>` offers checkout and cherry-pick.
        let r = reps("git che");
        assert!(r.contains(&"checkout".to_string()), "got: {r:?}");
        assert!(r.contains(&"cherry-pick".to_string()), "got: {r:?}");

        // start index points at the word under the cursor, not the command.
        let (start, _) = complete_line("git che", 7, &cwd);
        assert_eq!(start, 4);

        // Flags complete when the word begins with `-`.
        let r = reps("cargo --no-d");
        assert_eq!(r, vec!["--no-default-features".to_string()]);
        let r = reps("docker --ver");
        assert_eq!(r, vec!["--version".to_string()]);

        // Subcommands for the other supported tools.
        assert!(reps("kubectl get").contains(&"get".to_string()));
        assert!(reps("cargo bu").contains(&"build".to_string()));
    }

    #[test]
    fn unknown_command_degrades_to_path() {
        // AC2: unknown commands must not error and must fall back to path
        // completion (here: no matching paths → empty, but never a panic).
        let dir = std::env::temp_dir().join(format!("aish_unknown_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("report.txt"), "").unwrap();

        // A made-up command at the argument slot completes filenames, not a crash.
        let (_, pairs) = complete_line("frobnicate rep", 14, &dir);
        let reps: Vec<&str> = pairs.iter().map(|p| p.replacement.as_str()).collect();
        assert_eq!(reps, vec!["report.txt"], "got: {reps:?}");

        // A known command past the subcommand slot also falls to path completion.
        let (_, pairs) = complete_line("git add rep", 11, &dir);
        let reps: Vec<&str> = pairs.iter().map(|p| p.replacement.as_str()).collect();
        assert_eq!(reps, vec!["report.txt"], "got: {reps:?}");

        // An unrecognised git subcommand prefix degrades rather than erroring.
        let (_, pairs) = complete_line("git zzz", 7, &dir);
        let reps: Vec<&str> = pairs.iter().map(|p| p.replacement.as_str()).collect();
        assert!(reps.is_empty(), "got: {reps:?}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn prose_detection() {
        let prose = |l: &str| looks_like_prose(l, &rc::tokenize(l).unwrap());
        // English questions that start with a command word → model
        assert!(prose("who is zachary hohertz"));
        assert!(prose("find me big files"));
        assert!(prose("make this file executable"));
        assert!(prose("what is the capital of texas")); // `what` is a real binary on macOS
        // real invocations stay direct
        assert!(!prose("who"));
        assert!(!prose("who -a"));
        assert!(!prose("which ls"));
        assert!(!prose("what /usr/bin/ls")); // real `what` use takes a path
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
    fn last_output_binding_expands_in_dispatch() {
        let mut session = Session::new().unwrap();
        let path = std::env::temp_dir().join(format!("aish_repl_last_{}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let db = crate::db::Db::open(&path).unwrap();
        db.record("output", "/tmp", "hello from last");
        session.db = Some(db);

        // Both $LAST and $_ resolve to the most recent recorded output.
        assert_eq!(rc::tokenize_with("echo $LAST", var_lookup(&session)).unwrap(), vec!["echo", "hello from last"]);
        assert_eq!(rc::tokenize_with("echo $_", var_lookup(&session)).unwrap(), vec!["echo", "hello from last"]);
        // An explicit session export shadows the binding.
        session.env.push(("LAST".into(), "override".into()));
        assert_eq!(rc::tokenize_with("echo $LAST", var_lookup(&session)).unwrap(), vec!["echo", "override"]);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn route_prefixes() {
        assert!(matches!(split_route("!who is x".into()), (l, Route::Direct) if l == "who is x"));
        assert!(matches!(split_route("?ls".into()), (l, Route::Model) if l == "ls"));
        assert!(matches!(split_route("ls".into()), (l, Route::Auto) if l == "ls"));
    }

    // Snapshot of the three pure routing heuristics — `!`/`?` force routing,
    // looks_like_prose, and the tokenizer gate — so a tweak to any of them
    // surfaces as a golden diff instead of silently reshaping UX. The
    // executable-resolution step in `dispatch` is intentionally excluded to
    // keep the snapshot hermetic (no PATH/filesystem dependence); "direct"
    // here means "the heuristics did not divert the line to the model".
    fn heuristic_route(input: &str) -> &'static str {
        let (rest, route) = split_route(input.to_string());
        match route {
            Route::Direct => "direct  [! force]",
            Route::Model => "model   [? force]",
            Route::Auto => match rc::tokenize(&rest) {
                None => "model   [shell-syntax / non-tokenizable]",
                Some(words) if words.is_empty() => "model   [empty]",
                Some(words) => {
                    if is_stray_confirmation(&words) {
                        "model   [bare-yes guard]"
                    } else if looks_like_prose(&rest, &words) {
                        "model   [looks_like_prose]"
                    } else {
                        "direct  [auto]"
                    }
                }
            },
        }
    }

    #[test]
    fn routing_decision_snapshot() {
        // Golden corpus grouped by the heuristic each case exercises. Keep the
        // inputs and their order stable: a heuristic change should show up as a
        // value diff, not a reshuffle.
        let groups: &[(&str, &[&str])] = &[
            (
                "looks_like_prose — English starting with a real command word routes to the model, real invocations stay direct",
                &[
                    "who is zachary hohertz",
                    "find me big files",
                    "make this file executable",
                    "watch for events from atum, let me know about activity",
                    "find big files, oldest first",
                    "who",
                    "who -a",
                    "which ls",
                    "find . -name foo",
                    "tail logfile",
                    "echo hello there world",
                    "cat \"my file\" backup",
                    "watch -n 5 free",
                    "tail logs/app.log",
                ],
            ),
            (
                "bare-yes guard — a lone confirmation token (y/yes/n/no) routes to the model; multi-word invocations stay direct",
                &[
                    "yes",
                    "yes please",
                    "yes please thanks",
                    "test -f file",
                    "test the connection right now",
                ],
            ),
            (
                "!/? force routing — explicit escape hatches override every other heuristic",
                &[
                    "!who is zachary hohertz",
                    "!ls -la",
                    "?ls",
                    "?make this file executable",
                    "?who",
                ],
            ),
            (
                "tokenizer gate — shell syntax and prose punctuation the tokenizer rejects route to the model",
                &[
                    "who is on this machine right now?",
                    "echo *.rs",
                    "cat a > b",
                ],
            ),
        ];

        let mut snap = String::new();
        snap.push_str("# routing decision snapshot (TASK-110)\n");
        snap.push_str("# direct = run on the terminal · model = hand the line to the model\n");
        snap.push_str("# regenerate after an intended heuristic change: UPDATE_GOLDEN=1 cargo test routing_decision_snapshot\n");
        for (heading, cases) in groups {
            snap.push('\n');
            snap.push_str(&format!("## {heading}\n"));
            for &input in *cases {
                snap.push_str(&format!("{:<52} -> {}\n", input, heuristic_route(input)));
            }
        }

        let golden_path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/golden/routing_decisions.snap");
        if std::env::var("UPDATE_GOLDEN").is_ok() {
            std::fs::write(golden_path, &snap).expect("write golden snapshot");
            return;
        }

        let golden = std::fs::read_to_string(golden_path)
            .expect("missing tests/golden/routing_decisions.snap — run UPDATE_GOLDEN=1 cargo test to create it");
        if golden != snap {
            let mut diff = String::new();
            for (i, (g, a)) in golden.lines().zip(snap.lines()).enumerate() {
                if g != a {
                    diff.push_str(&format!("  L{}\n    golden: {g}\n    actual: {a}\n", i + 1));
                }
            }
            let (gn, an) = (golden.lines().count(), snap.lines().count());
            if gn != an {
                diff.push_str(&format!("  line count differs: golden={gn} actual={an}\n"));
            }
            panic!(
                "routing decision snapshot drift — a routing heuristic changed.\n\
                 Review the diff below; if the change is intended, regenerate the \n\
                 golden file with `UPDATE_GOLDEN=1 cargo test routing_decision_snapshot`.\n\n{diff}"
            );
        }
    }

    // ---- TASK-109: oracle harness — direct-dispatch path ------------------
    //
    // Sibling to pipeline.rs's pipeline-path oracle. A line with no `|` takes
    // the *direct-dispatch* path: `dispatch` resolves one program via
    // `resolve_program` and runs it through `tools::run_on_tty`. bash is the
    // ground truth aish's single-command path must reproduce on stdout bytes
    // and exit status.
    //
    // `run_on_tty` inherits the terminal's stdout, so — exactly as the pipeline
    // oracle appends a `dd` sink rather than read the terminal — the stdout
    // cases mirror `run_on_tty`'s spawn configuration (same cwd, same session
    // env, kill_on_drop) but pipe stdout into a capture buffer. The exit-status
    // cases call the genuine `run_on_tty`, so the production path is exercised
    // directly. The corpus is integer/ASCII coreutils with no stdin reads, so a
    // mismatch is an aish regression, not a locale or echo-builtin quirk.

    /// Resolve `cmd`'s program the way `dispatch` does (real `resolve_program`
    /// against the process PATH), returning the resolved path and argv tail.
    fn resolve_for_oracle(cmd: &str, session: &Session) -> (PathBuf, Vec<String>) {
        let words = rc::tokenize(cmd).expect("oracle command must tokenize");
        let path_var = std::env::var("PATH").unwrap_or_default();
        let program = resolve_program(&words[0], &session.cwd, &path_var)
            .expect("oracle command's program must resolve on PATH");
        (program, words[1..].to_vec())
    }

    /// Run `cmd` through aish's direct-dispatch resolution and a faithful mirror
    /// of `run_on_tty`'s spawn (stdout piped for capture). Returns stdout bytes.
    async fn aish_direct_stdout(cmd: &str, session: &Session) -> Vec<u8> {
        let (program, args) = resolve_for_oracle(cmd, session);
        tokio::process::Command::new(&program)
            .args(&args)
            .current_dir(&session.cwd)
            .envs(session.env.iter().map(|(k, v)| (k, v)))
            .kill_on_drop(true)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .output()
            .await
            .expect("spawn direct-dispatch program")
            .stdout
    }

    /// Exit code of `cmd` through the genuine `tools::run_on_tty`.
    async fn aish_direct_code(cmd: &str, session: &Session) -> Option<i32> {
        let (program, args) = resolve_for_oracle(cmd, session);
        tools::run_on_tty(&program.to_string_lossy(), &args, &[], session)
            .await
            .expect("run_on_tty")
            .code()
    }

    /// Run `cmd` through the oracle (`bash -c`) in `cwd`, returning its stdout.
    fn bash_stdout(cmd: &str, cwd: &Path) -> Vec<u8> {
        std::process::Command::new("bash")
            .arg("-c")
            .arg(cmd)
            .current_dir(cwd)
            .stdin(std::process::Stdio::null())
            .output()
            .expect("spawn bash oracle (is bash on PATH?)")
            .stdout
    }

    /// Exit code of `cmd` under the oracle (`bash -c`) in `cwd`.
    fn bash_code(cmd: &str, cwd: &Path) -> Option<i32> {
        std::process::Command::new("bash")
            .arg("-c")
            .arg(cmd)
            .current_dir(cwd)
            .status()
            .expect("spawn bash oracle")
            .code()
    }

    #[tokio::test]
    async fn oracle_direct_stdout_matches_bash() {
        let session = Session::new().unwrap();
        // Each entry is a single command aish runs natively (no pipe), reading
        // no stdin; bash is the ground truth.
        let corpus = [
            "echo hello world",
            "seq 1 20",
            "seq 5 1",        // descending with the default step → empty output
            "seq 1 2 9",      // strided: 1 3 5 7 9
            "printf %s+%s 3 4",
            "expr 6 + 7",
            "basename /usr/local/bin/aish",
            "dirname /usr/local/bin/aish",
            "head -c 8 /dev/zero",
            "wc -c /dev/null",
        ];
        for cmd in corpus {
            let got = aish_direct_stdout(cmd, &session).await;
            let want = bash_stdout(cmd, &session.cwd);
            assert_eq!(
                got,
                want,
                "aish direct-dispatch stdout diverged from bash for `{cmd}`\n  \
                 aish: {:?}\n  bash: {:?}",
                String::from_utf8_lossy(&got),
                String::from_utf8_lossy(&want),
            );
        }
    }

    #[tokio::test]
    async fn oracle_direct_exit_status_matches_bash() {
        let session = Session::new().unwrap();
        // A single command's status is the program's status in both shells.
        let corpus = [
            "true",
            "false",
            "test -d /",
            "test -f /no/such/path/aish-oracle",
            "grep -q anything /dev/null", // empty file → no match → exit 1
            "expr 1 = 1",                 // result 1 (true) → exit 0
            "expr 1 = 2",                 // result 0 (false) → exit 1
        ];
        for cmd in corpus {
            let got = aish_direct_code(cmd, &session).await;
            let want = bash_code(cmd, &session.cwd);
            assert_eq!(got, want, "aish direct-dispatch exit status diverged from bash for `{cmd}`");
        }
    }

    #[tokio::test]
    async fn oracle_direct_detects_deliberate_divergence() {
        // Teeth: feed aish and the oracle *different* commands and confirm the
        // comparator sees them as unequal. If this ever matches, the direct
        // oracle is blind and the agreement tests above are worthless.
        let session = Session::new().unwrap();
        let aish = aish_direct_stdout("echo same", &session).await;
        let bash = bash_stdout("echo different", &session.cwd);
        assert_ne!(
            aish, bash,
            "direct oracle failed to detect an intentional divergence — the harness is blind"
        );
    }
}
