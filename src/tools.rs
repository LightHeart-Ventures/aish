use crate::backend::{ToolCall, ToolDef, ToolResult};
use crate::session::Session;
use anyhow::Result;
use serde_json::json;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};

const MAX_OUTPUT: usize = 50_000; // bytes of program output fed back to the model
const MAX_FILE_READ: usize = 100_000;
const DEFAULT_TIMEOUT_SECS: u64 = 120; // run_program kill deadline unless the call overrides it
const MAX_TIMEOUT_SECS: u64 = 3600;
// After the child exits, wait this long for its pipes to EOF — a daemonized
// grandchild can hold the write end open forever.
const PIPE_GRACE: Duration = Duration::from_secs(5);

/// The user's answer to a confirmation prompt.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Decision {
    /// Decline this call.
    Deny,
    /// Allow this one call.
    AllowOnce,
    /// Allow this call and persist the tool/command so it never prompts again.
    AlwaysAllow,
}

/// Frontend-provided confirmation hook. Engine stays TTY-free.
pub type Confirm<'a> = dyn FnMut(&str) -> Decision + 'a;

/// Consult the persistent always-allow list, prompting only when the key isn't
/// already allowed. Returns true when the action may proceed; an 'always'
/// answer is persisted under `key` so it skips the prompt next time.
fn gate(session: &Session, key: &str, prompt: &str, confirm: &mut Confirm<'_>) -> bool {
    if session.is_tool_allowed(key) {
        return true;
    }
    match confirm(prompt) {
        Decision::Deny => false,
        Decision::AllowOnce => true,
        Decision::AlwaysAllow => {
            session.allow_tool(key);
            true
        }
    }
}

/// Stable identity used by the always-allow list. Program execution keys on the
/// binary name (allowing `git` must not also allow `rm`); everything else keys
/// on the tool name.
fn allow_key(call: &ToolCall) -> String {
    match call.name.as_str() {
        "run_program" | "run_interactive" => call.args["program"]
            .as_str()
            .map(bin_name)
            .unwrap_or(call.name.as_str())
            .to_string(),
        _ => call.name.clone(),
    }
}

pub fn tool_defs() -> Vec<ToolDef> {
    vec![
        ToolDef {
            name: "run_program".into(),
            description: "Execute one program directly (fork/exec — NO shell). Call this for any \
system command: process info, disk usage, git, compilers, package managers, etc. `program` is the \
binary name or path, `args` is the argv array. There is no pipe/glob/redirection support; run \
programs one at a time and process their output yourself. Programs are killed after \
`timeout_secs` (default 120) and you get whatever they printed up to that point, so \
long-running or never-exiting commands (monitors, watchers, servers) are safe to run — they get \
cut off, they can't hang the session."
                .into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "program": {"type": "string", "description": "Binary name (resolved via PATH) or absolute path"},
                    "args": {"type": "array", "items": {"type": "string"}, "description": "Argument vector (no quoting/escaping — pass each arg as-is)"},
                    "timeout_secs": {"type": "integer", "description": "Kill the program after this many seconds (default 120, max 3600). Raise for slow builds; lower to sample a continuous monitor. Ignored for background jobs."},
                    "env": {"type": "object", "description": "Extra environment variables (string values). A value may reference \"${NAME}\" (session exports, then process env) or \"${profile:KEY}\" (resolved from ~/.atum/credentials section [profile] at spawn time) — use the reference for secrets so their values never enter the conversation."},
                    "background": {"type": "boolean", "description": "Run detached as a background job: returns a job id immediately, the program runs until it exits or :kill, its output streams live to the user's terminal, and you can read the accumulated output anytime with job_output. Use for watchers, listeners, tails, and servers."}
                },
                "required": ["program"]
            }),
        },
        ToolDef {
            name: "run_interactive".into(),
            description: "Run a program attached to the user's real terminal (full TTY hand-off). \
Use this for screen-oriented or interactive programs — top, htop, vim, nano, less, ssh, language \
REPLs — anything that needs a live keyboard or draws a UI. The user drives the program directly; \
you see nothing of the session, only the exit status when it ends. Use run_program instead \
whenever you need the output."
                .into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "program": {"type": "string", "description": "Binary name (resolved via PATH) or absolute path"},
                    "args": {"type": "array", "items": {"type": "string"}, "description": "Argument vector (no quoting/escaping — pass each arg as-is)"},
                    "env": {"type": "object", "description": "Extra environment variables (string values). Supports \"${NAME}\" and \"${profile:KEY}\" references like run_program's env."}
                },
                "required": ["program"]
            }),
        },
        ToolDef {
            name: "read_file".into(),
            description: "Read a file's contents. Call this instead of running cat/head/tail. \
Relative paths resolve against the shell's current directory.".into(),
            schema: json!({
                "type": "object",
                "properties": {"path": {"type": "string"}},
                "required": ["path"]
            }),
        },
        ToolDef {
            name: "write_file".into(),
            description: "Create or overwrite a file with the given content. Call this whenever the \
user wants a file created or changed — never try echo/tee tricks.".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "content": {"type": "string"}
                },
                "required": ["path", "content"]
            }),
        },
        ToolDef {
            name: "list_dir".into(),
            description: "List a directory's entries (name, type, size). Call this instead of ls, \
and to expand wildcards since globs don't exist here. Defaults to the current directory.".into(),
            schema: json!({
                "type": "object",
                "properties": {"path": {"type": "string", "description": "Optional; defaults to cwd"}}
            }),
        },
        ToolDef {
            name: "remember".into(),
            description: "Save one durable memory (user preference, project fact, lesson learned) \
to aish's persistent store — it survives across sessions. Keep each memory a single \
self-contained sentence.".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "content": {"type": "string"},
                    "tags": {"type": "string", "description": "Optional comma-separated tags, e.g. \"preference\" or \"project,aios\""}
                },
                "required": ["content"]
            }),
        },
        ToolDef {
            name: "recall".into(),
            description: "Search persistent memories by keyword (empty query → most recent). \
Check here when past context might matter: preferences, project facts, prior decisions.".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "query": {"type": "string"},
                    "limit": {"type": "integer", "description": "Max results (default 8)"}
                }
            }),
        },
        ToolDef {
            name: "change_dir".into(),
            description: "Change the shell's working directory for all subsequent operations. Call \
this whenever the user says cd / go to / work in some directory.".into(),
            schema: json!({
                "type": "object",
                "properties": {"path": {"type": "string"}},
                "required": ["path"]
            }),
        },
        ToolDef {
            name: "job_output".into(),
            description: "Read a background job's accumulated output and status (started via \
run_program with background:true). Use this to check on watchers/listeners you started earlier — \
their output does NOT reach you by push, only the user's terminal sees it live.".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "job": {"type": "integer", "description": "Job id, as returned when the job started (also shown by :jobs)"}
                },
                "required": ["job"]
            }),
        },
        ToolDef {
            name: "get_skill".into(),
            description: "Fetch an MCP-published skill playbook (the system prompt lists them \
under 'MCP skills'). Returns the expanded instructions — read them, then follow them.".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "server": {"type": "string", "description": "MCP server name, as listed"},
                    "name": {"type": "string", "description": "Skill name, e.g. atum/sprint-status"},
                    "args": {"type": "object", "description": "Skill arguments (string values), per the listed argument names"}
                },
                "required": ["server", "name"]
            }),
        },
    ]
}

/// Execute one tool call, routing mutating actions through the safety gate.
pub async fn execute(call: &ToolCall, session: &mut Session, confirm: &mut Confirm<'_>) -> ToolResult {
    // Paranoid mode confirms everything once, centrally; the per-tool gates
    // below then stand down (see exec_needs_confirm and friends).
    if session.mode == crate::session::Mode::Paranoid {
        let args = truncate_middle(serde_json::to_string(&call.args).unwrap_or_default(), 200);
        if !gate(session, &allow_key(call), &format!("{} {args}", call.name), confirm) {
            return ToolResult {
                id: call.id.clone(),
                content: "user declined this tool call".into(),
                is_error: false,
            };
        }
    }
    let result = match call.name.as_str() {
        "run_program" => run_program(call, session, confirm).await,
        "run_interactive" => run_interactive(call, session, confirm).await,
        "read_file" => read_file(call, session),
        "write_file" => write_file(call, session, confirm),
        "list_dir" => list_dir(call, session),
        "change_dir" => change_dir(call, session),
        "remember" => remember(call, session),
        "recall" => recall(call, session),
        "job_output" => job_output(call, session),
        "get_skill" => get_skill(call, session).await,
        other if other.starts_with("mcp__") => mcp_call(call, session, confirm).await,
        other => Err(anyhow::anyhow!("unknown tool: {other}")),
    };
    match result {
        Ok(content) => ToolResult { id: call.id.clone(), content, is_error: false },
        Err(e) => ToolResult { id: call.id.clone(), content: format!("error: {e:#}"), is_error: true },
    }
}

fn resolve(session: &Session, path: &str) -> PathBuf {
    let p = Path::new(path);
    if p.is_absolute() {
        p.to_path_buf()
    } else if let Some(rest) = path.strip_prefix("~/") {
        PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(rest)
    } else if path == "~" {
        PathBuf::from(std::env::var("HOME").unwrap_or_default())
    } else {
        session.cwd.join(p)
    }
}

// ---------------------------------------------------------------------------
// Safety gate
// ---------------------------------------------------------------------------

const READ_ONLY_PROGRAMS: &[&str] = &[
    "ls", "cat", "pwd", "grep", "rg", "find", "fd", "head", "tail", "wc", "which",
    "whereis", "ps", "df", "du", "stat", "file", "env", "printenv", "date", "whoami",
    "id", "uname", "free", "uptime", "hostname", "tree", "realpath", "readlink",
    "basename", "dirname", "echo", "sort", "uniq", "cut", "less", "more", "lsblk",
    "lscpu", "lsusb", "lspci", "uptime", "cal", "type", "md5sum", "sha256sum", "diff",
];

const GIT_READ_ONLY: &[&str] = &[
    "status", "log", "diff", "show", "branch", "remote", "ls-files", "blame",
    "shortlog", "describe", "rev-parse", "config", "stash",
];

/// Programs whose whole purpose is to mutate — always confirm.
const DESTRUCTIVE_PROGRAMS: &[&str] = &[
    "rm", "rmdir", "unlink", "mv", "cp", "dd", "shred", "truncate", "mkdir", "touch",
    "ln", "chmod", "chown", "chgrp", "tee", "kill", "pkill", "killall", "mkfs",
    "fdisk", "parted", "mount", "umount", "reboot", "shutdown", "poweroff", "halt",
    "useradd", "userdel", "usermod", "groupadd", "groupdel", "passwd",
];

/// Mutating subcommand verbs for multi-tools (aws, kubectl, docker, npm, …).
/// Matched against each arg exactly or as a `verb-…`/`verb_…` prefix, so
/// `aws s3 rm`, `s3api delete-bucket`, and `ec2 terminate-instances` all hit.
const DESTRUCTIVE_VERBS: &[&str] = &[
    "rm", "rb", "delete", "del", "remove", "destroy", "drop", "purge", "prune",
    "terminate", "uninstall", "reset", "revert", "rollback", "kill",
    "create", "mb", "put", "push", "apply", "set", "update", "upgrade", "install",
    "add", "write", "import", "restore", "sync", "cp", "mv", "deploy", "publish", "new",
    "format", "chmod", "chown", "exec",
];

fn bin_name(program: &str) -> &str {
    Path::new(program).file_name().and_then(|s| s.to_str()).unwrap_or(program)
}

fn git_is_read_only(args: &[String]) -> bool {
    // `git config` / `git stash` are read-only only without mutating sub-args
    match args.first().map(String::as_str) {
        Some("config") => args.len() <= 2 || args.iter().any(|a| a == "--list" || a == "--get"),
        Some("stash") => args.get(1).map(String::as_str) == Some("list"),
        Some(sub) => GIT_READ_ONLY.contains(&sub),
        None => true,
    }
}

/// Provably read-only (careful mode's allowlist question).
fn is_read_only(program: &str, args: &[String]) -> bool {
    let bin = bin_name(program);
    READ_ONLY_PROGRAMS.contains(&bin) || (bin == "git" && git_is_read_only(args))
}

/// Does this look like it writes, creates, or deletes? (normal mode's
/// question — reads and unknown commands run free, mutations prompt.)
/// Heuristic by design: an unknown verb runs unprompted, a read named like
/// a write prompts once.
fn is_destructive(program: &str, args: &[String]) -> bool {
    let bin = bin_name(program);
    if READ_ONLY_PROGRAMS.contains(&bin) {
        return false;
    }
    if DESTRUCTIVE_PROGRAMS.contains(&bin) {
        return true;
    }
    if matches!(bin, "sudo" | "doas") {
        // judge the wrapped command, and treat a bare sudo as destructive
        return match args.split_first() {
            Some((cmd, rest)) => is_destructive(cmd, rest),
            None => true,
        };
    }
    if bin == "git" {
        return !git_is_read_only(args);
    }
    args.iter().any(|a| {
        let token = a.trim_start_matches('-').to_ascii_lowercase();
        DESTRUCTIVE_VERBS.iter().any(|v| {
            token == *v
                || (token.len() > v.len()
                    && token.starts_with(v)
                    && matches!(token.as_bytes()[v.len()], b'-' | b'_'))
        })
    })
}

/// Should this program run prompt, given the session's mode?
fn exec_needs_confirm(mode: crate::session::Mode, program: &str, args: &[String]) -> bool {
    use crate::session::Mode;
    match mode {
        Mode::Paranoid => false, // already confirmed centrally in execute()
        Mode::Careful => !is_read_only(program, args),
        Mode::Normal => is_destructive(program, args),
        Mode::Yolo => false,
    }
}

// ---------------------------------------------------------------------------
// Executors
// ---------------------------------------------------------------------------

/// Extract (program, args) from a tool call, enforcing the "no shell" invariant.
fn parse_argv(call: &ToolCall) -> Result<(String, Vec<String>)> {
    let program = call.args["program"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("missing program"))?
        .to_string();
    let args: Vec<String> = call.args["args"]
        .as_array()
        .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
        .unwrap_or_default();

    // The "no shell" invariant: refuse to be a backdoor into one.
    let bin = Path::new(&program).file_name().and_then(|s| s.to_str()).unwrap_or(&program);
    if matches!(bin, "sh" | "bash" | "zsh" | "dash" | "fish" | "ksh") {
        anyhow::bail!("aish does not invoke shells — call the underlying program directly");
    }
    Ok((program, args))
}

async fn run_program(call: &ToolCall, session: &mut Session, confirm: &mut Confirm<'_>) -> Result<String> {
    let (program, args) = parse_argv(call)?;
    let env = resolve_env(call, session);
    let background = call.args["background"].as_bool() == Some(true);

    let timeout_secs = call.args["timeout_secs"]
        .as_u64()
        .unwrap_or(DEFAULT_TIMEOUT_SECS)
        .clamp(1, MAX_TIMEOUT_SECS);

    let display = format!(
        "{} {}{}",
        program,
        args.join(" "),
        if background { " (background)" } else { "" }
    );
    if exec_needs_confirm(session.mode, &program, &args)
        && !gate(session, bin_name(&program), display.trim(), confirm)
    {
        return Ok("user declined to run this command".into());
    }

    if background {
        return spawn_background(&program, &args, &env, session, display);
    }

    let mut child = tokio::process::Command::new(&program)
        .args(&args)
        .current_dir(&session.cwd)
        .envs(session.env.iter().map(|(k, v)| (k, v)))
        .envs(env.iter().map(|(k, v)| (k, v)))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // Ctrl-C aborts the turn by dropping this future mid-await; reap the
        // child too, or a SIGINT-ignoring program lingers as an orphan.
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| anyhow::anyhow!("failed to exec {program}: {e}"))?;

    // Drain pipes concurrently with the wait so a chatty child never blocks on
    // a full pipe, and so a timed-out kill still returns what it printed.
    // Half the budget per stream: the combined result must fit under MAX_OUTPUT
    // or the final truncate_middle would eat the capture's own drop markers.
    let mut out_task = tokio::spawn(drain_capped(child.stdout.take().expect("piped"), MAX_OUTPUT / 2));
    let mut err_task = tokio::spawn(drain_capped(child.stderr.take().expect("piped"), MAX_OUTPUT / 2));

    let mut timed_out = false;
    let status = tokio::select! {
        status = child.wait() => status,
        _ = tokio::time::sleep(Duration::from_secs(timeout_secs)) => {
            timed_out = true;
            child.start_kill().ok();
            child.wait().await
        }
    }
    .map_err(|e| anyhow::anyhow!("failed to wait on {program}: {e}"))?;

    // A model-run command counts toward `$?` just like a directly-dispatched one.
    session.set_last_status(&status);

    let stdout = await_capture(&mut out_task).await;
    let stderr = await_capture(&mut err_task).await;

    let mut out = String::new();
    if !stdout.is_empty() {
        out.push_str(&stdout);
    }
    if !stderr.is_empty() {
        out.push_str("\n--- stderr ---\n");
        out.push_str(&stderr);
    }
    if timed_out {
        out.push_str(&format!(
            "\n[killed: still running after the {timeout_secs}s timeout — output above is everything it printed]"
        ));
    } else if let Some(code) = status.code() {
        if code != 0 {
            out.push_str(&format!("\n[exit code: {code}]"));
        }
    } else {
        // Terminated by a signal — name it so an external Ctrl-C/kill is
        // distinguishable from a program failure.
        use std::os::unix::process::ExitStatusExt;
        if let Some(sig) = status.signal() {
            out.push_str(&format!("\n[killed by signal {sig}]"));
        }
    }
    if out.is_empty() {
        out = "[no output, exit 0]".into();
    }
    Ok(truncate_middle(out, MAX_OUTPUT))
}

// ---------------------------------------------------------------------------
// Background jobs — detached children whose output streams to the user live
// and accumulates (capped) for the model to read with job_output.
// ---------------------------------------------------------------------------

const JOB_BUFFER_CAP: usize = 64_000; // bytes of retained output per job

pub struct Job {
    pub id: usize,
    pub desc: String,
    buffer: Arc<Mutex<String>>,
    /// Exit summary once finished; None while running.
    pub done: Arc<Mutex<Option<String>>>,
    kill: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
}

pub type Jobs = Arc<Mutex<Vec<Arc<Job>>>>;

impl Job {
    /// "running" or the exit summary.
    pub fn status(&self) -> String {
        self.done.lock().unwrap().clone().unwrap_or_else(|| "running".into())
    }

    /// Ask the waiter task to kill the child. False when already finished.
    pub fn kill(&self) -> bool {
        self.kill.lock().unwrap().take().is_some_and(|tx| tx.send(()).is_ok())
    }
}

/// Print one line from a background source (job output, MCP notification)
/// over the top of whatever is on the current terminal line — rustyline
/// redraws its prompt on the next keypress.
pub fn announce(prefix: &str, line: &str) {
    eprint!("\r\x1b[2K\x1b[2m{prefix} {line}\x1b[0m\n");
}

fn spawn_background(
    program: &str,
    args: &[String],
    env: &[(String, String)],
    session: &Session,
    display: String,
) -> Result<String> {
    let mut child = tokio::process::Command::new(program)
        .args(args)
        .current_dir(&session.cwd)
        .envs(session.env.iter().map(|(k, v)| (k, v)))
        .envs(env.iter().map(|(k, v)| (k, v)))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // The Child moves into the waiter task below, so this only fires if
        // aish itself shuts down — background jobs die with the shell.
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| anyhow::anyhow!("failed to exec {program}: {e}"))?;
    let pid = child.id().unwrap_or_default();

    let mut jobs = session.jobs.lock().unwrap();
    let id = jobs.iter().map(|j| j.id).max().unwrap_or(0) + 1;
    let (kill_tx, kill_rx) = tokio::sync::oneshot::channel();
    let job = Arc::new(Job {
        id,
        desc: display,
        buffer: Arc::new(Mutex::new(String::new())),
        done: Arc::new(Mutex::new(None)),
        kill: Mutex::new(Some(kill_tx)),
    });

    stream_job_pipe(child.stdout.take().expect("piped"), job.clone());
    stream_job_pipe(child.stderr.take().expect("piped"), job.clone());

    // Waiter owns the child for its whole life.
    let waiter_job = job.clone();
    tokio::spawn(async move {
        let status = tokio::select! {
            s = child.wait() => s,
            _ = kill_rx => {
                child.start_kill().ok();
                child.wait().await
            }
        };
        let summary = match status {
            Ok(s) => match s.code() {
                Some(code) => format!("exited {code}"),
                None => "killed".into(),
            },
            Err(e) => format!("wait failed: {e}"),
        };
        *waiter_job.done.lock().unwrap() = Some(summary.clone());
        announce(&format!("[job {}]", waiter_job.id), &format!("{summary} — {}", waiter_job.desc));
    });

    jobs.push(job);
    Ok(format!(
        "started background job {id} (pid {pid}). It runs until it exits or the user kills it \
(:kill {id}); its output streams live to the user's terminal. You do NOT receive that output — \
read it with job_output {{\"job\": {id}}} when you need it."
    ))
}

/// Stream one job pipe line by line: echo to the terminal, append to the
/// job's capped buffer.
fn stream_job_pipe<R>(pipe: R, job: Arc<Job>)
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut lines = BufReader::new(pipe).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            {
                let mut buf = job.buffer.lock().unwrap();
                buf.push_str(&line);
                buf.push('\n');
                if buf.len() > JOB_BUFFER_CAP {
                    let mut cut = buf.len() - JOB_BUFFER_CAP;
                    while !buf.is_char_boundary(cut) {
                        cut += 1;
                    }
                    buf.drain(..cut);
                }
            }
            announce(&format!("[job {}]", job.id), &line);
        }
    });
}

fn job_output(call: &ToolCall, session: &Session) -> Result<String> {
    let id = call.args["job"].as_u64().ok_or_else(|| anyhow::anyhow!("missing job id"))? as usize;
    let jobs = session.jobs.lock().unwrap();
    let job = jobs
        .iter()
        .find(|j| j.id == id)
        .ok_or_else(|| anyhow::anyhow!("no such job: {id} (see :jobs)"))?;
    let buf = job.buffer.lock().unwrap();
    Ok(format!(
        "[job {id}: {}] {}\n{}",
        job.status(),
        job.desc,
        if buf.is_empty() { "(no output yet)" } else { buf.as_str() }
    ))
}

// ---------------------------------------------------------------------------
// Spawn environment — the call's `env` object, with secret-safe references
// ---------------------------------------------------------------------------

/// Extra env for a spawn. Values may reference `${NAME}` (session exports,
/// then process env) or `${profile:KEY}` (~/.atum/credentials) — resolved
/// here at spawn time so secret values never enter the conversation.
fn resolve_env(call: &ToolCall, session: &Session) -> Vec<(String, String)> {
    let Some(map) = call.args["env"].as_object() else {
        return Vec::new();
    };
    let creds = format!("{}/.atum/credentials", std::env::var("HOME").unwrap_or_default());
    map.iter()
        .filter_map(|(k, v)| {
            v.as_str().map(|v| (k.clone(), resolve_env_value(v, &session.env, &creds)))
        })
        .collect()
}

fn resolve_env_value(raw: &str, session_env: &[(String, String)], creds_file: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut rest = raw;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let Some(end) = after.find('}') else {
            out.push_str(&rest[start..]);
            return out;
        };
        let name = &after[..end];
        let resolved = if let Some((profile, key)) = name.split_once(':') {
            crate::mcp::load_profile(creds_file, profile).get(key).cloned()
        } else {
            session_env
                .iter()
                .rev()
                .find(|(k, _)| k == name)
                .map(|(_, v)| v.clone())
                .or_else(|| std::env::var(name).ok())
        };
        match resolved {
            Some(v) => out.push_str(&v),
            // Unresolvable: keep the reference verbatim so the failure
            // surfaces in the program, not silently as an empty string.
            None => {
                out.push_str("${");
                out.push_str(name);
                out.push('}');
            }
        }
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    out
}

/// Read a pipe to EOF, keeping at most `cap` bytes: a head prefix plus a tail
/// ring, mirroring truncate_middle's head+tail split. Excess is drained and
/// discarded so the child never stalls on a full pipe.
async fn drain_capped<R: tokio::io::AsyncRead + Unpin>(
    mut r: R,
    cap: usize,
) -> (Vec<u8>, Vec<u8>, u64) {
    let head_cap = cap * 3 / 4;
    let tail_cap = cap / 4;
    let mut head: Vec<u8> = Vec::new();
    let mut tail: std::collections::VecDeque<u8> = std::collections::VecDeque::new();
    let mut dropped: u64 = 0;
    let mut buf = [0u8; 8192];
    loop {
        match r.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                let to_head = (head_cap - head.len()).min(n);
                head.extend_from_slice(&buf[..to_head]);
                tail.extend(buf[to_head..n].iter().copied());
                if tail.len() > tail_cap {
                    dropped += (tail.len() - tail_cap) as u64;
                    tail.drain(..tail.len() - tail_cap);
                }
            }
        }
    }
    (head, tail.into_iter().collect(), dropped)
}

/// Collect a drain task's result. Bounded by PIPE_GRACE: after the child is
/// gone the pipe should EOF immediately, unless a daemonized grandchild
/// inherited the write end — don't let that hang the session.
async fn await_capture(task: &mut tokio::task::JoinHandle<(Vec<u8>, Vec<u8>, u64)>) -> String {
    match tokio::time::timeout(PIPE_GRACE, &mut *task).await {
        Ok(Ok((head, tail, dropped))) => {
            let mut s = String::from_utf8_lossy(&head).into_owned();
            if dropped > 0 {
                s.push_str(&format!("\n…[dropped {dropped} bytes]…\n"));
            }
            s.push_str(&String::from_utf8_lossy(&tail));
            s
        }
        Ok(Err(_)) => "[output lost: capture task failed]".into(),
        Err(_) => {
            task.abort();
            "[output lost: a surviving descendant still holds the pipe open]".into()
        }
    }
}

async fn run_interactive(call: &ToolCall, session: &mut Session, confirm: &mut Confirm<'_>) -> Result<String> {
    let (program, args) = parse_argv(call)?;
    let env = resolve_env(call, session);

    let display = format!("{} {}", program, args.join(" "));
    if exec_needs_confirm(session.mode, &program, &args)
        && !gate(session, bin_name(&program), display.trim(), confirm)
    {
        return Ok("user declined to run this command".into());
    }

    let status = run_on_tty(&program, &args, &env, session).await?;
    session.set_last_status(&status);
    Ok(match status.code() {
        Some(code) => format!("[interactive session ended: exit code {code}]"),
        None => {
            use std::os::unix::process::ExitStatusExt;
            match status.signal() {
                Some(sig) => format!("[interactive session ended: killed by signal {sig}]"),
                None => "[interactive session ended]".into(),
            }
        }
    })
}

/// Hand the terminal to a child program: inherited stdin/stdout/stderr, wait
/// until it exits. Used by run_interactive and the REPL's direct dispatch.
/// While the child runs, `session.tty_handoff` is set so the REPL knows a
/// Ctrl-C belongs to the child; terminal modes are restored afterwards in
/// case the program crashed without cleaning up its raw-mode state.
pub async fn run_on_tty(
    program: &str,
    args: &[String],
    extra_env: &[(String, String)],
    session: &Session,
) -> Result<std::process::ExitStatus> {
    let _guard = TtyGuard::engage(session.tty_handoff.clone());
    tokio::process::Command::new(program)
        .args(args)
        .current_dir(&session.cwd)
        .envs(session.env.iter().map(|(k, v)| (k, v)))
        .envs(extra_env.iter().map(|(k, v)| (k, v)))
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| anyhow::anyhow!("failed to exec {program}: {e}"))?
        .wait()
        .await
        .map_err(|e| anyhow::anyhow!("failed to wait on {program}: {e}"))
}

/// RAII for a TTY hand-off: flags the hand-off for the REPL's signal handling
/// and snapshots/restores terminal attributes around the child's lifetime.
struct TtyGuard {
    flag: std::sync::Arc<std::sync::atomic::AtomicBool>,
    saved: Option<libc::termios>,
}

impl TtyGuard {
    fn engage(flag: std::sync::Arc<std::sync::atomic::AtomicBool>) -> Self {
        flag.store(true, std::sync::atomic::Ordering::SeqCst);
        // SAFETY: plain libc terminal queries on fd 0; termios is POD.
        let saved = unsafe {
            if libc::isatty(0) == 1 {
                let mut t: libc::termios = std::mem::zeroed();
                (libc::tcgetattr(0, &mut t) == 0).then_some(t)
            } else {
                None
            }
        };
        Self { flag, saved }
    }
}

impl Drop for TtyGuard {
    fn drop(&mut self) {
        if let Some(t) = self.saved {
            // SAFETY: restoring the exact attributes captured in engage().
            unsafe { libc::tcsetattr(0, libc::TCSADRAIN, &t) };
        }
        self.flag.store(false, std::sync::atomic::Ordering::SeqCst);
    }
}

/// MCP tools come from outside the safety gate's knowledge — confirm them in
/// careful/normal mode unless the server declared the tool read-only
/// (the MCP `readOnlyHint` annotation).
async fn mcp_call(call: &ToolCall, session: &mut Session, confirm: &mut Confirm<'_>) -> Result<String> {
    let gated = matches!(session.mode, crate::session::Mode::Careful | crate::session::Mode::Normal)
        && !session.mcp.is_read_only(&call.name);
    if gated {
        let args = serde_json::to_string(&call.args).unwrap_or_default();
        let args = truncate_middle(args, 200);
        if !gate(session, &call.name, &format!("{} {args}", call.name), confirm) {
            return Ok("user declined this tool call".into());
        }
    }
    Ok(truncate_middle(session.mcp.call(&call.name, &call.args).await?, MAX_OUTPUT))
}

fn read_file(call: &ToolCall, session: &Session) -> Result<String> {
    let path = call.args["path"].as_str().ok_or_else(|| anyhow::anyhow!("missing path"))?;
    let full = resolve(session, path);
    let content = std::fs::read_to_string(&full)
        .map_err(|e| anyhow::anyhow!("{}: {e}", full.display()))?;
    Ok(truncate_middle(content, MAX_FILE_READ))
}

fn write_file(call: &ToolCall, session: &Session, confirm: &mut Confirm<'_>) -> Result<String> {
    let path = call.args["path"].as_str().ok_or_else(|| anyhow::anyhow!("missing path"))?;
    let content = call.args["content"].as_str().ok_or_else(|| anyhow::anyhow!("missing content"))?;
    let full = resolve(session, path);

    // A write is destructive in every mode short of yolo (paranoid already asked).
    if matches!(session.mode, crate::session::Mode::Careful | crate::session::Mode::Normal) {
        let preview: String = content.lines().take(5).collect::<Vec<_>>().join("\n  │ ");
        let more = content.lines().count().saturating_sub(5);
        let suffix = if more > 0 { format!("\n  │ … +{more} lines") } else { String::new() };
        let action = if full.exists() { "overwrite" } else { "write" };
        let prompt = format!("{action} {} ({} bytes)\n  │ {preview}{suffix}\n ", full.display(), content.len());
        if !gate(session, "write_file", &prompt, confirm) {
            return Ok("user declined the write".into());
        }
    }

    if let Some(parent) = full.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    std::fs::write(&full, content).map_err(|e| anyhow::anyhow!("{}: {e}", full.display()))?;
    Ok(format!("wrote {} bytes to {}", content.len(), full.display()))
}

fn list_dir(call: &ToolCall, session: &Session) -> Result<String> {
    let path = call.args["path"].as_str().unwrap_or(".");
    let full = resolve(session, path);
    let mut entries: Vec<String> = Vec::new();
    for entry in std::fs::read_dir(&full).map_err(|e| anyhow::anyhow!("{}: {e}", full.display()))? {
        let entry = entry?;
        let meta = entry.metadata()?;
        let kind = if meta.is_dir() { "dir " } else if meta.is_symlink() { "link" } else { "file" };
        let size = if meta.is_file() { format!(" {}", meta.len()) } else { String::new() };
        entries.push(format!("{kind} {}{size}", entry.file_name().to_string_lossy()));
    }
    entries.sort();
    if entries.is_empty() {
        return Ok("[empty directory]".into());
    }
    Ok(truncate_middle(entries.join("\n"), MAX_OUTPUT))
}

fn remember(call: &ToolCall, session: &Session) -> Result<String> {
    let db = session.db.as_ref().ok_or_else(|| anyhow::anyhow!("memory store unavailable"))?;
    let content = call.args["content"].as_str().ok_or_else(|| anyhow::anyhow!("missing content"))?;
    let id = db.remember(content, call.args["tags"].as_str())?;
    Ok(format!("remembered (#{id})"))
}

fn recall(call: &ToolCall, session: &Session) -> Result<String> {
    let db = session.db.as_ref().ok_or_else(|| anyhow::anyhow!("memory store unavailable"))?;
    let query = call.args["query"].as_str().unwrap_or("");
    let limit = call.args["limit"].as_u64().unwrap_or(8) as usize;
    let hits = db.recall(query, limit)?;
    Ok(if hits.is_empty() { "no memories match".into() } else { hits.join("\n") })
}

async fn get_skill(call: &ToolCall, session: &mut Session) -> Result<String> {
    let server = call.args["server"].as_str().ok_or_else(|| anyhow::anyhow!("missing server"))?;
    let name = call.args["name"].as_str().ok_or_else(|| anyhow::anyhow!("missing name"))?;
    let args = if call.args["args"].is_object() { call.args["args"].clone() } else { json!({}) };
    session.mcp.get_skill(server, name, &args).await
}

fn change_dir(call: &ToolCall, session: &mut Session) -> Result<String> {
    let path = call.args["path"].as_str().ok_or_else(|| anyhow::anyhow!("missing path"))?;
    let full = resolve(session, path);
    let canonical = full
        .canonicalize()
        .map_err(|e| anyhow::anyhow!("{}: {e}", full.display()))?;
    if !canonical.is_dir() {
        anyhow::bail!("{} is not a directory", canonical.display());
    }
    session.cwd = canonical;
    Ok(format!("working directory is now {}", session.cwd.display()))
}

/// Keep head + tail when output exceeds the cap; the middle is least useful.
fn truncate_middle(s: String, max: usize) -> String {
    if s.len() <= max {
        return s;
    }
    let head_end = char_floor(&s, max * 3 / 4);
    let tail_start = char_ceil(&s, s.len() - max / 5);
    format!(
        "{}\n…[truncated {} bytes]…\n{}",
        &s[..head_end],
        tail_start - head_end,
        &s[tail_start..]
    )
}

fn char_floor(s: &str, mut i: usize) -> usize {
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

fn char_ceil(s: &str, mut i: usize) -> usize {
    while i < s.len() && !s.is_char_boundary(i) {
        i += 1;
    }
    i
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_value_resolution() {
        let creds = std::env::temp_dir().join(format!("aish_env_creds_{}", std::process::id()));
        std::fs::write(&creds, "[aish]\nATUM_API_KEY = sk_secret\n[other]\nATUM_API_KEY = nope\n").unwrap();
        let creds = creds.to_str().unwrap().to_string();
        let session_env = vec![("FOO".to_string(), "from_session".to_string())];

        // session exports win; ${profile:KEY} reads the right INI section
        assert_eq!(resolve_env_value("${FOO}", &session_env, &creds), "from_session");
        assert_eq!(resolve_env_value("${aish:ATUM_API_KEY}", &session_env, &creds), "sk_secret");
        // composition with literal text
        assert_eq!(resolve_env_value("Bearer ${aish:ATUM_API_KEY}!", &session_env, &creds), "Bearer sk_secret!");
        // process env fallback (PATH is always set), unresolved kept verbatim
        assert_ne!(resolve_env_value("${PATH}", &[], &creds), "${PATH}");
        assert_eq!(resolve_env_value("${NO_SUCH_VAR_XYZ}", &[], &creds), "${NO_SUCH_VAR_XYZ}");
        assert_eq!(resolve_env_value("${missing:KEY}", &[], &creds), "${missing:KEY}");
        // no references: passthrough
        assert_eq!(resolve_env_value("plain", &[], &creds), "plain");
        let _ = std::fs::remove_file(&creds);
    }

    fn call(program: &str, args: &[&str], timeout_secs: Option<u64>) -> ToolCall {
        let mut a = json!({"program": program, "args": args});
        if let Some(t) = timeout_secs {
            a["timeout_secs"] = json!(t);
        }
        ToolCall { id: "t1".into(), name: "run_program".into(), args: a }
    }

    async fn run(c: &ToolCall) -> String {
        let mut session = Session::new().unwrap();
        session.mode = crate::session::Mode::Yolo; // no confirm prompts in tests
        let mut confirm = |_: &str| Decision::AllowOnce;
        let r = execute(c, &mut session, &mut confirm).await;
        assert!(!r.is_error, "unexpected error: {}", r.content);
        r.content
    }

    #[tokio::test]
    async fn normal_command_unaffected() {
        let out = run(&call("echo", &["hi"], None)).await;
        assert_eq!(out.trim(), "hi");
    }

    #[tokio::test]
    async fn never_exiting_command_is_killed_at_timeout() {
        let start = std::time::Instant::now();
        let out = run(&call("sleep", &["300"], Some(1))).await;
        assert!(start.elapsed() < Duration::from_secs(10), "took {:?}", start.elapsed());
        assert!(out.contains("killed: still running after the 1s timeout"), "got: {out}");
    }

    #[tokio::test]
    async fn timed_out_command_returns_partial_output() {
        // `yes` floods stdout forever: exercises the cap, the drop marker, and the kill.
        let out = run(&call("yes", &[], Some(1))).await;
        assert!(out.starts_with("y\ny\n"), "got: {}", &out[..out.len().min(40)]);
        assert!(out.contains("dropped"), "expected drop marker, got tail: {}", &out[out.len() - 200..]);
        assert!(out.contains("killed: still running after the 1s timeout"));
    }

    #[tokio::test]
    async fn nonzero_exit_reported() {
        let out = run(&call("false", &[], None)).await;
        assert!(out.contains("[exit code: 1]"), "got: {out}");
    }

    #[tokio::test]
    async fn model_run_command_tracks_dollar_question() {
        // A model-run command updates `$?` just like a directly-dispatched one.
        let mut session = Session::new().unwrap();
        session.mode = crate::session::Mode::Yolo;
        let mut confirm = |_: &str| Decision::AllowOnce;
        let _ = execute(&call("false", &[], None), &mut session, &mut confirm).await;
        assert_eq!(session.last_status, 1);
        let _ = execute(&call("true", &[], None), &mut session, &mut confirm).await;
        assert_eq!(session.last_status, 0);
    }

    #[test]
    fn destructive_classifier() {
        let d = |p: &str, a: &[&str]| {
            is_destructive(p, &a.iter().map(|s| s.to_string()).collect::<Vec<_>>())
        };
        // reads run free
        assert!(!d("aws", &["s3", "ls"]));
        assert!(!d("kubectl", &["get", "pods"]));
        assert!(!d("cargo", &["build"]));
        assert!(!d("git", &["status"]));
        assert!(!d("ls", &["-la"]));
        // mutations prompt
        assert!(d("rm", &["-rf", "x"]));
        assert!(d("aws", &["s3", "rm", "s3://bucket/key"]));
        assert!(d("aws", &["s3api", "delete-bucket", "--bucket", "b"]));
        assert!(d("aws", &["ec2", "terminate-instances"]));
        assert!(d("kubectl", &["delete", "pod", "x"]));
        assert!(d("rsync", &["--delete", "a", "b"]));
        assert!(d("git", &["push"]));
        assert!(d("npm", &["install", "left-pad"]));
        assert!(d("sudo", &["rm", "x"]));
        assert!(!d("sudo", &["ls"]));
        // careful-mode allowlist still intact
        assert!(is_read_only("git", &["log".into()]));
        assert!(!is_read_only("git", &["push".into()]));
    }

    #[test]
    fn allow_key_uses_binary_name() {
        let c = |name: &str, args: serde_json::Value| ToolCall {
            id: "t".into(),
            name: name.into(),
            args,
        };
        assert_eq!(
            allow_key(&c("run_program", json!({"program": "/usr/bin/git", "args": ["push"]}))),
            "git"
        );
        assert_eq!(allow_key(&c("run_interactive", json!({"program": "vim"}))), "vim");
        assert_eq!(allow_key(&c("write_file", json!({"path": "/x"}))), "write_file");
        assert_eq!(allow_key(&c("mcp__srv__do", json!({}))), "mcp__srv__do");
    }

    #[tokio::test]
    async fn always_allow_persists_and_skips_confirm() {
        use std::cell::Cell;
        let mut session = Session::new().unwrap();
        let path = std::env::temp_dir().join(format!("aish_allow_gate_{}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        session.db = Some(crate::db::Db::open(&path).unwrap());
        session.mode = crate::session::Mode::Normal;

        let calls = Cell::new(0);
        // First destructive call: prompt fires, user answers 'always'.
        {
            let mut confirm = |_: &str| {
                calls.set(calls.get() + 1);
                Decision::AlwaysAllow
            };
            let r = execute(&call("rm", &["aish_no_such_file_a"], None), &mut session, &mut confirm).await;
            assert!(!r.content.contains("declined"), "gate should have passed: {}", r.content);
        }
        assert_eq!(calls.get(), 1);
        assert!(session.is_tool_allowed("rm"), "rm should be persisted on the allow-list");

        // Second destructive call: confirm must NOT be consulted (would Deny).
        {
            let mut confirm = |_: &str| {
                calls.set(calls.get() + 1);
                Decision::Deny
            };
            let r = execute(&call("rm", &["aish_no_such_file_b"], None), &mut session, &mut confirm).await;
            assert!(!r.content.contains("declined to run"), "should be auto-allowed: {}", r.content);
        }
        assert_eq!(calls.get(), 1, "second rm should have skipped the prompt");
        let _ = std::fs::remove_file(&path);
    }
}
