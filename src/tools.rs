use crate::backend::{ToolCall, ToolDef, ToolResult};
use crate::session::Session;
use anyhow::Result;
use serde_json::json;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
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
fn gate(session: &mut Session, key: &str, prompt: &str, confirm: &mut Confirm<'_>) -> bool {
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

pub fn tool_defs(batch_mode: bool, escalate_available: bool) -> Vec<ToolDef> {
    let mut defs = vec![
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
    ];
    // Interactive batch mode: only offered when on, so the model never sees these
    // tools (or the system-prompt nudge) unless the user opted in with :batch on.
    if batch_mode {
        defs.push(ToolDef {
            name: "run_in_background".into(),
            description: "Run a self-contained, deferrable task in the BACKGROUND. Do NOT use it \
for work the user needs answered right now. The task runs as a full background coordinator: a \
headless aish in the SAME directory with your COMPLETE toolset and MCP servers (read/write files, \
run programs, atum/github, …) that can also fan heavy parallel sub-work out on its own. It runs \
asynchronously, survives a restart, and its result auto-delivers here when done. You don't choose \
a backend or worry about batches — just describe the task and offload it."
                .into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "task": {"type": "string", "description": "The self-contained task to run in the background. It has no access to THIS conversation — include everything it needs. It CAN read the project files and use tools/MCP in the current directory."},
                    "isolate": {"type": "boolean", "description": "Set TRUE for any task that WRITES or EDITS files or runs builds/tests — it then runs in its own dedicated git worktree (a fresh branch) so it can't clobber the working tree of other parallel background jobs or your live session. Set FALSE for read-only / analysis tasks (search, summarize, inspect) that change nothing. If omitted, it defaults to TRUE when the current directory is a git repo (isolation is free when no changes are made — the worktree is auto-removed), FALSE otherwise. When an isolated job makes changes, its branch is left intact and reported back for you to review/merge; nothing is auto-merged."},
                    "base": {"type": "string", "enum": ["main", "head"], "description": "Which baseline an ISOLATED job branches from (ignored when isolate is false). \"main\" (default) = a CLEAN trunk baseline (latest origin/main when there's a remote, else local main) — use it for independent/new work so the job doesn't inherit unrelated in-progress changes. \"head\" = branch from the CURRENT checkout — use it ONLY when the task must build on the work currently in this branch (\"continue/extend what I'm doing\")."}
                },
                "required": ["task"]
            }),
        });
        defs.push(ToolDef {
            name: "background_status".into(),
            description: "List ALL background jobs and their LIVE status — background coordinators \
(running in this session) and durable coordinator runs / Anthropic batch jobs (shared across \
sessions). Call this to answer \"what's running?\" / \"status\" instead of guessing or inventing \
your own tracking. Returns a table: id, kind, owner, status, task."
                .into(),
            schema: json!({ "type": "object", "properties": {} }),
        });
    }
    // Synchronous escalation: only offered to a weak frontend (a stronger model
    // is reachable). An Opus/default-Grok session never sees this tool — it would
    // just be the model consulting itself.
    if escalate_available {
        defs.push(ToolDef {
            name: "escalate".into(),
            description: "Hand a hard reasoning or analysis sub-problem to a STRONGER model and get \
its answer back THIS turn — a synchronous, blocking consult that returns in a few seconds. Reach for \
it the moment a step needs deeper reasoning than you can do reliably yourself: diagnosing a confusing \
error, planning a multi-step change, weighing an ambiguous or risky decision, careful code/logic \
analysis. The strong model has NO tools and NO access to this conversation, the files, or the machine \
— put EVERYTHING it needs into `task` (the question, the relevant output, the constraints). It returns \
reasoning/text only; you then act on its answer with your own tools. Use this for a step you must \
finish NOW but can't reason through alone; use run_in_background instead when the result can wait. \
Escalating beats guessing."
                .into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "task": {"type": "string", "description": "The self-contained problem to reason about: the question plus ALL the context, output, and constraints the strong model needs, since it sees nothing but this string."}
                },
                "required": ["task"]
            }),
        });
    }
    defs
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
        "run_in_background" => run_in_background(call, session),
        "batch_result" => batch_result(call, session),
        "background_status" => background_status(session),
        "escalate" => escalate(call, session).await,
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

/// The currently checked-out branch in `cwd`, or `None` if it can't be told
/// (not a repo, detached HEAD, git missing). Cheap `git rev-parse` probe.
fn current_git_branch(cwd: &Path) -> Option<String> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let b = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!b.is_empty() && b != "HEAD").then_some(b)
}

/// Guard entry point: returns a human reason when this `git` invocation would
/// mutate the default branch (push to or commit on main/master). Only `push`
/// and `commit` are inspected — and only those pay for the branch lookup.
fn git_default_branch_guard(args: &[String], cwd: &Path) -> Option<String> {
    match args.first().map(String::as_str) {
        Some("push") | Some("commit") => {
            protected_git_mutation(args, current_git_branch(cwd).as_deref())
        }
        _ => None,
    }
}

/// Pure default-branch detection (split out for testing — no IO). `args` is the
/// git argv *after* the program; `current_branch` is the checked-out branch.
fn protected_git_mutation(args: &[String], current_branch: Option<&str>) -> Option<String> {
    let is_default = |b: &str| matches!(b, "main" | "master");
    let on_default = current_branch.map(is_default).unwrap_or(false);
    match args.first().map(String::as_str)? {
        "push" => {
            // An explicit main/master target — `git push origin main`,
            // `… HEAD:main`, `… :main` — is unambiguous.
            let names_default = args
                .iter()
                .any(|a| is_default(a) || a.rsplit(':').next().map(is_default).unwrap_or(false));
            if names_default {
                return Some("push to the default branch (main/master)".into());
            }
            // An implicit push (no explicit refspec beyond a remote) while ON the
            // default branch pushes it. An explicit other-branch refspec is fine.
            if on_default && !push_has_explicit_refspec(args) {
                return Some("push the current branch, which is the default (main/master)".into());
            }
            None
        }
        "commit" if on_default => {
            Some("commit directly on the default branch (main/master)".into())
        }
        _ => None,
    }
}

/// True when a `git push` argv carries an explicit refspec (a second non-flag
/// token after the remote), i.e. it targets a named branch rather than pushing
/// the current one. Flags (`-u`, `--force`, …) and the lone remote don't count.
fn push_has_explicit_refspec(args: &[String]) -> bool {
    args.iter()
        .skip(1) // drop the "push" subcommand
        .filter(|a| !a.starts_with('-'))
        .count()
        >= 2 // remote + at least one refspec
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

    // Default-branch guard: never let an agent commit on / push the default
    // branch directly (the footgun that pushed unreviewed work straight to a
    // shared main). Unattended (yolo or a background coordinator) → hard refuse;
    // interactive → require an explicit y/N (not the always-allow gate, so this
    // can't be permanently waved through). Branch + PR is the only sanctioned path.
    if bin_name(&program) == "git" {
        if let Some(reason) = git_default_branch_guard(&args, &session.cwd) {
            if session.mode == crate::session::Mode::Yolo || session.nested {
                anyhow::bail!(
                    "refused: this command would {reason}. aish does not let an agent touch the \
default branch directly — create a feature branch (git checkout -b …), commit there, push the \
branch, and open a pull request (gh pr create) instead."
                );
            }
            if confirm(&format!(
                "⚠ this git command would {reason}, bypassing review — proceed anyway?"
            )) == Decision::Deny
            {
                return Ok(
                    "declined — use a feature branch + pull request instead of touching the default branch".into(),
                );
            }
        }
    }

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

pub use crate::jobs::{Job, Jobs};

/// Print one line from a background source (job output, MCP notification)
/// over the top of whatever is on the current terminal line — rustyline
/// redraws its prompt on the next keypress.
pub fn announce(prefix: &str, line: &str) {
    eprint!("\r\x1b[2K\x1b[2m{prefix} {line}\x1b[0m\n");
}

/// Hang up every still-live managed job when the shell exits, so none is
/// orphaned (TASK-123 / S3.6). For each non-terminal job with a known process
/// group: if it is stopped, continue it first (SIGCONT) — a stopped process
/// can't act on a pending SIGHUP — then send SIGHUP to the whole group. Jobs
/// that have already finished, or that never recorded a pgid, are skipped.
pub fn hangup_jobs_on_exit(jobs: &Jobs) {
    let jobs = jobs.lock().unwrap();
    for job in jobs.iter() {
        if job.is_done() {
            continue;
        }
        let Some(pgid) = job.pgid() else { continue };
        // SAFETY: signalling a managed job's own process group (pgid == its
        // leader pid), which the shell put in its own group at spawn.
        unsafe {
            if job.is_stopped() {
                libc::kill(-pgid, libc::SIGCONT);
            }
            libc::kill(-pgid, libc::SIGHUP);
        }
    }
}

fn spawn_background(
    program: &str,
    args: &[String],
    env: &[(String, String)],
    session: &Session,
    display: String,
) -> Result<String> {
    let mut cmd = tokio::process::Command::new(program);
    cmd.args(args)
        .current_dir(&session.cwd)
        .envs(session.env.iter().map(|(k, v)| (k, v)))
        .envs(env.iter().map(|(k, v)| (k, v)))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // The Child moves into the waiter task below, so this only fires if
        // aish itself shuts down — background jobs die with the shell.
        .kill_on_drop(true);
    // Lead its own process group (pgid == pid) so the shell can hang the job up
    // by group on exit without signalling itself (TASK-123). setpgid(0, 0) in
    // the post-fork child; `pre_exec` is an inherent method on tokio's Command.
    unsafe {
        cmd.pre_exec(|| match libc::setpgid(0, 0) {
            0 => Ok(()),
            _ => Err(std::io::Error::last_os_error()),
        });
    }
    let mut child = cmd
        .spawn()
        .map_err(|e| anyhow::anyhow!("failed to exec {program}: {e}"))?;
    let pid = child.id().unwrap_or_default();
    // Mirror the setpgid from the parent to close the spawn race (EACCES once
    // the child has exec'd is expected — ignore it).
    unsafe { libc::setpgid(pid as libc::pid_t, pid as libc::pid_t) };

    let mut jobs = session.jobs.lock().unwrap();
    let id = jobs.iter().map(|j| j.id).max().unwrap_or(0) + 1;
    let (job, kill_rx) = Job::background(id, display);
    job.set_pgid(pid as libc::pid_t);

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
        waiter_job.finish(summary.clone());
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
            job.push_line(&line);
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
    let buf = job.output();
    Ok(format!(
        "[job {id}: {}] {}\n{}",
        job.status(),
        job.desc,
        if buf.is_empty() { "(no output yet)" } else { buf.as_str() }
    ))
}

// ---------------------------------------------------------------------------
// Interactive batch mode — offload deferrable work to background Anthropic
// Message Batches jobs. Only reachable when :batch is on (the tools aren't in
// the tool set otherwise).
// ---------------------------------------------------------------------------

/// System prompt for a synchronous `escalate` consult: a pure-reasoning helper
/// with no tools and no machine access, answering from the task text alone.
const ESCALATE_SYSTEM: &str = "You are a strong reasoning model consulted by aish, an AI shell agent \
running on a smaller, faster model. It has hit a step that needs deeper reasoning than it can do \
reliably and has handed you that sub-problem. You have NO tools and NO access to its conversation, \
files, or machine — reason only over what is in the message. Return a clear, concrete, \
directly-usable answer: the decision, the command(s) or plan, or the analysis asked for, with just \
enough reasoning to justify it. Be precise and concise; the shell agent will act on your answer.";

/// Synchronous escalation: a weak frontend hands one hard sub-problem to the
/// stronger model and gets its reasoning back WITHIN this turn. Unlike
/// `run_in_background` (async coordinator), this blocks on a single tool-less
/// completion against the `(provider, model)` the engine resolved for this turn
/// (`session.escalation`) and returns the strong model's text. No tools, no
/// machine access — the strong model answers from `task` alone.
async fn escalate(call: &ToolCall, session: &Session) -> Result<String> {
    let task = call.args["task"].as_str().map(str::trim).unwrap_or("");
    if task.is_empty() {
        anyhow::bail!(
            "`task` is required — state the sub-problem in full (question + context + constraints); \
the strong model sees nothing else"
        );
    }
    // The engine recomputes this each turn before building the tool set, so the
    // tool is only present when this is Some — but guard anyway.
    let (provider, model) = session.escalation.clone().ok_or_else(|| {
        anyhow::anyhow!("escalation isn't available on this backend/model (already the strongest)")
    })?;
    let backend = match provider.as_str() {
        "grok" => crate::backend::Backend::new_grok(model, &session.env),
        // claude (also the target a local frontend escalates to)
        _ => crate::backend::claude::Credential::resolve(&session.env)
            .and_then(|cred| crate::backend::Backend::new_claude(model, cred)),
    }
    .map_err(|e| anyhow::anyhow!("couldn't reach the strong model for escalation: {e:#}"))?;

    let turn = backend
        .complete(ESCALATE_SYSTEM, &[crate::backend::Msg::user(task)], &[])
        .await
        .map_err(|e| anyhow::anyhow!("escalation consult failed: {e:#}"))?;
    let answer = turn.text.trim();
    if answer.is_empty() {
        anyhow::bail!("the strong model returned no usable text");
    }
    Ok(answer.to_string())
}

fn run_in_background(call: &ToolCall, session: &Session) -> Result<String> {
    let task = call.args["task"].as_str().map(str::trim).unwrap_or("");
    if task.is_empty() {
        anyhow::bail!("`task` is required");
    }
    // A background coordinator (a re-exec'd headless aish) runs on the SAME
    // backend as this session (full parity), so it needs a credential for THAT
    // backend — both inherited by the child (env + ~/.aishrc exports, and for
    // Grok the ~/.grok/auth.json token file). Claude works with either
    // ANTHROPIC_API_KEY or a CLAUDE_CODE_OAUTH_TOKEN subscription; the nested
    // tool-less batch path below additionally needs a metered key (checked there).
    let cred_ok = match session.backend_kind.as_str() {
        "grok" => crate::backend::grok::credential_available(&session.env),
        _ => crate::backend::claude::Credential::resolve(&session.env).is_ok(),
    };
    if !cred_ok {
        anyhow::bail!(
            "no credential for the active backend — Claude needs CLAUDE_CODE_OAUTH_TOKEN or \
ANTHROPIC_API_KEY; Grok needs a Grok CLI login (~/.grok/auth.json) or XAI_API_KEY (env or ~/.aishrc)"
        );
    }

    // The DEFAULT (and only model-facing) background path is now the coordinator:
    // a full-tool, durable, resumable headless aish that can ALSO fan heavy
    // sub-work out to the Batches API on its own. The model no longer picks
    // batch-vs-worker — it just offloads. The tool-less batch is kept purely as
    // an INTERNAL optimization (used by a coordinator round / `:batch`), not a
    // user-facing mode.
    //
    // Nested guard: a coordinator must never spawn its own coordinator (no
    // infinite re-exec recursion). When we ARE a coordinator (`session.nested`),
    // a deferred offload degrades to the in-process tool-less batch — the one
    // internal use of the batch path on this model-facing route.
    if session.nested {
        // The tool-less Batches API needs a metered key; a subscription OAuth
        // token can't reach it. Look in ~/.aishrc exports first, then the process
        // env. If that's all we have, this nested fan-out can't run — say so
        // plainly rather than failing opaquely later.
        let api_key = session
            .env
            .iter()
            .rev()
            .find(|(k, _)| k == "ANTHROPIC_API_KEY")
            .map(|(_, v)| v.clone())
            .or_else(|| std::env::var("ANTHROPIC_API_KEY").ok())
            .filter(|v| !v.trim().is_empty())
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "nested background fan-out uses the Anthropic Batches API, which needs a metered \
ANTHROPIC_API_KEY — a Claude subscription token (CLAUDE_CODE_OAUTH_TOKEN) can't reach it"
                )
            })?;
        let _id = crate::batch::spawn(
            &session.batch_jobs,
            task.to_string(),
            session.batch_model.clone(),
            api_key,
            session.batch_store.clone(),
            session.session_id.clone(),
            session.name.clone(),
        );
        return Ok("Queued in the background. Now reply with one short, natural sentence that \
you're working on it and the answer will appear here when ready — no job id, no restating the task."
            .to_string());
    }

    let exe = std::env::current_exe()
        .map_err(|e| anyhow::anyhow!("can't locate the aish binary to re-exec: {e}"))?;
    // Worktree isolation (the headline fix for parallel coordinators clobbering
    // one tree). The model sets `isolate` explicitly (true for write/build work);
    // when omitted, default to isolated WHEN we're in a git repo — isolation is
    // free for a no-change job (the worktree auto-removes) so it's the safe default.
    let isolate = match call.args["isolate"].as_bool() {
        Some(b) => b,
        None => crate::worker::is_git_repo(&session.cwd),
    };
    // Base for the isolated worktree: default to a clean trunk baseline ("main"),
    // so a job never inherits a stale/unrelated local checkout. The model passes
    // base:"head" to continue the current branch's work instead.
    let base = match call.args["base"].as_str() {
        Some(b) if b.eq_ignore_ascii_case("head") => "head",
        _ => "main",
    };
    let spec = crate::worker::WorkerSpec {
        exe,
        cwd: session.cwd.clone(),
        backend: session.backend_kind.clone(),
        model: crate::worker::coordinator_model(&session.backend_kind, &session.batch_model),
        env: session.env.clone(),
        isolate,
        base: base.to_string(),
        launch_session_id: session.session_id.clone(),
        launch_session_name: session.name.clone(),
        show_output: session.show_worker_output.clone(),
    };
    let _id = crate::worker::spawn(&session.worker_jobs, task.to_string(), spec);
    Ok("Queued a background coordinator (full toolset + MCP in this directory; it can fan parallel \
sub-work out on its own). The result auto-delivers here when it's done — do NOT try to fetch it. \
Now reply to the user with one short, natural sentence that you're on it and the answer will appear \
when ready — no job id, no restating the task."
        .to_string())
}

/// Live status of every background job the session can see — its own full-tool
/// workers (in memory) and all sessions' Anthropic batches (from the shared
/// store). This is what lets the model answer "what's running?" with facts
/// instead of fabricating its own tracking (see the Haiku failure transcript).
fn background_status(session: &Session) -> Result<String> {
    let trunc = |t: &str| -> String {
        let t = t.replace('|', "\\|");
        if t.chars().count() > 56 {
            format!("{}…", t.chars().take(56).collect::<String>())
        } else {
            t
        }
    };
    let mut out =
        String::from("| ID | Kind | Owner | Status | Since | Task |\n|---|---|---|---|---|---|\n");
    let mut any = false;

    // This session's full-tool background coordinators (in memory; the live
    // subprocess handle is session-local even though its durable row is shared).
    for w in session.worker_jobs.lock().unwrap().iter() {
        any = true;
        out.push_str(&format!(
            "| `{}` | coordinator | you | {} | — | {} |\n",
            crate::batch::short_id(&w.id),
            w.status(),
            trunc(&w.task)
        ));
    }
    // Durable coordinator runs from the shared store — every session's, so this
    // surfaces runs that outlive (or were started by) another aish process.
    if let Some(store) = &session.coordinator_store {
        if let Ok(rows) = store.load_all() {
            for r in rows {
                any = true;
                let owner = if r.session_id.as_deref() == Some(session.session_id.as_str()) {
                    "you".into()
                } else {
                    r.session_name
                        .clone()
                        .or_else(|| r.session_id.as_deref().map(|s| crate::batch::short_id(s).to_string()))
                        .unwrap_or_else(|| "—".into())
                };
                out.push_str(&format!(
                    "| `{}` | coordinator | {} | {} | {} | {} |\n",
                    crate::batch::short_id(&r.run_id),
                    owner,
                    r.phase,
                    r.created_at.as_deref().unwrap_or("—"),
                    trunc(&r.task)
                ));
            }
        }
    }
    // Anthropic batches from the shared store — every session's, so this answers
    // cross-session "what's running" too.
    if let Some(store) = &session.batch_store {
        if let Ok(rows) = store.load_all() {
            for r in rows {
                any = true;
                let owner = r
                    .session_name
                    .clone()
                    .or_else(|| r.session_id.as_deref().map(|s| crate::batch::short_id(s).to_string()))
                    .unwrap_or_else(|| "—".into());
                let owner = if r.session_id.as_deref() == Some(session.session_id.as_str()) {
                    format!("{owner} (you)")
                } else {
                    owner
                };
                out.push_str(&format!(
                    "| `{}` | batch | {} | {} | {} | {} |\n",
                    crate::batch::short_id(&r.local_id),
                    owner,
                    r.status,
                    r.created_at.as_deref().unwrap_or("—"),
                    trunc(&r.task)
                ));
            }
        }
    }

    if !any {
        return Ok("No background jobs running.".into());
    }
    Ok(out)
}

fn batch_result(call: &ToolCall, session: &Session) -> Result<String> {
    // Ids are now uuids; match the full id or any unambiguous prefix (e.g. the
    // short 8-char form shown by background_status).
    let raw = call.args["job"]
        .as_str()
        .map(str::to_string)
        .or_else(|| call.args["job"].as_u64().map(|n| n.to_string()))
        .ok_or_else(|| anyhow::anyhow!("missing job id"))?;
    let q = raw.trim();
    let hit = |id: &str| id == q || id.starts_with(q);

    // Full-tool workers auto-deliver, but the model sometimes reflexively fetches
    // one — so check worker_jobs too rather than reporting "no such job".
    if let Some(w) = session.worker_jobs.lock().unwrap().iter().find(|j| hit(&j.id)) {
        return Ok(w.fetch());
    }
    if let Some(j) = session.batch_jobs.lock().unwrap().iter().find(|j| hit(&j.id)) {
        return Ok(j.fetch());
    }
    anyhow::bail!(
        "no background job matching '{q}' in this session — call background_status to list \
running jobs (workers auto-deliver, so you usually don't need to fetch at all)"
    )
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
/// The child leads its own process group and owns the terminal (tcsetpgrp), so
/// the terminal delivers a Ctrl-C straight to it — aish needs no hand-off flag
/// to know the signal isn't its to handle (TASK-116). Terminal modes are
/// restored afterwards in case the program crashed without cleaning up its
/// raw-mode state.
///
/// Per the S1.4 spike (docs/spikes/S1.4-reaper-vs-waitpid.md) this single
/// foreground path is spawned with `std::process` rather than `tokio::process`
/// so its pid is disjoint from tokio's reaper set: the child leads its own
/// process group (`setpgid`), owns the terminal (`tcsetpgrp`, with SIGTTOU
/// ignored across the hand-off), and is reaped by a contained SIGCHLD task that
/// `waitpid`s *only this pid* with `WUNTRACED|WCONTINUED|WNOHANG`. The signal is
/// observed through `tokio::signal` (which multiplexes the handler) so tokio
/// keeps reaping its own captured/background/pipeline children and never
/// `waitpid(-1)`s — disjoint PID sets, no double-reap.
pub async fn run_on_tty(
    program: &str,
    args: &[String],
    extra_env: &[(String, String)],
    session: &Session,
) -> Result<std::process::ExitStatus> {
    use std::os::unix::process::CommandExt;

    let _guard = TtyGuard::engage();

    // Subscribe to SIGCHLD *before* spawning so an instant-exit child can't fire
    // before we are listening. tokio::signal multiplexes the handler, so tokio's
    // own child reaper keeps working (no raw `sigaction` clobber).
    let mut sigchld = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::child())
        .map_err(|e| anyhow::anyhow!("failed to watch SIGCHLD: {e}"))?;

    let mut cmd = std::process::Command::new(program);
    cmd.args(args)
        .current_dir(&session.cwd)
        .envs(session.env.iter().map(|(k, v)| (k, v)))
        .envs(extra_env.iter().map(|(k, v)| (k, v)));
    // The child leads its own process group so terminal signals (Ctrl-C/-Z) hit
    // it, not the shell. setpgid(0, 0): the new pgid is the child's pid.
    unsafe {
        cmd.pre_exec(|| match libc::setpgid(0, 0) {
            0 => Ok(()),
            _ => Err(std::io::Error::last_os_error()),
        });
        // aish ignores the job-control signals (see ignore_job_control_signals);
        // SIG_IGN is inherited across exec, so restore the default disposition in
        // the child or it would be deaf to Ctrl-C/Ctrl-\/Ctrl-Z. Async-signal-safe:
        // signal(2) only.
        cmd.pre_exec(|| {
            reset_job_control_signals();
            Ok(())
        });
    }
    let proc = cmd
        .spawn()
        .map_err(|e| anyhow::anyhow!("failed to exec {program}: {e}"))?;
    let pid = proc.id() as libc::pid_t;
    // std::process::Child::drop neither reaps nor kills, so the pid stays
    // waitpid-able — our SIGCHLD task owns the wait.
    drop(proc);
    let mut child = ForegroundChild { pid, reaped: false };

    // Mirror the child's setpgid from the parent too (closes the spawn race;
    // EACCES once the child has exec'd is expected — ignore it), then hand it
    // the terminal. tcsetpgrp from our now-background pgrp would raise SIGTTOU,
    // so ignore that signal across the call.
    let on_tty = unsafe { libc::isatty(0) == 1 };
    unsafe { libc::setpgid(pid, pid) };
    if on_tty {
        with_sigttou_ignored(|| unsafe { libc::tcsetpgrp(0, pid) });
    }
    // Reclaim the terminal for the shell on every exit path, including a Ctrl-C
    // that drops this future mid-await.
    let _reclaim = ForegroundReclaim { on_tty };

    // Reap loop: poll first (covers an exit between spawn and the first await),
    // then wait for the next SIGCHLD. waitpid is scoped to `pid` only.
    loop {
        if let Some(status) = reap_foreground(pid)? {
            child.reaped = true;
            return Ok(status);
        }
        sigchld.recv().await;
    }
}

/// One non-blocking pass of the foreground reaper: `waitpid(pid, …)` with
/// `WNOHANG|WUNTRACED|WCONTINUED`, tracking job state. Returns `Some(status)`
/// once the child terminates, `None` while it is still running (or merely
/// stopped/continued). A Ctrl-Z stop is resumed in place — real
/// suspend-to-background (the fg/bg/jobs UX) is deferred to S2.
fn reap_foreground(pid: libc::pid_t) -> Result<Option<std::process::ExitStatus>> {
    use std::os::unix::process::ExitStatusExt;
    loop {
        let mut status: libc::c_int = 0;
        let r = unsafe {
            libc::waitpid(pid, &mut status, libc::WNOHANG | libc::WUNTRACED | libc::WCONTINUED)
        };
        if r == 0 {
            return Ok(None); // no state change for our child yet
        }
        if r < 0 {
            return Err(anyhow::anyhow!(
                "failed to wait on pid {pid}: {}",
                std::io::Error::last_os_error()
            ));
        }
        if libc::WIFEXITED(status) || libc::WIFSIGNALED(status) {
            return Ok(Some(std::process::ExitStatus::from_raw(status)));
        }
        if libc::WIFSTOPPED(status) {
            // Job state: stopped. S2 owns suspend-to-background; for now resume
            // so the foreground child keeps running and the terminal stays live.
            unsafe { libc::kill(-pid, libc::SIGCONT) };
        }
        // WIFCONTINUED (or the resume above): loop and poll again.
    }
}

/// Run `f` with SIGTTOU ignored. `tcsetpgrp()` from a process that isn't the
/// terminal's foreground group raises SIGTTOU (default action: stop us); ignore
/// it for the duration so the hand-off itself never suspends the shell.
fn with_sigttou_ignored<T>(f: impl FnOnce() -> T) -> T {
    // SAFETY: swapping the SIGTTOU disposition to SIG_IGN and back to its prior
    // handler around a single synchronous call.
    let prev = unsafe { libc::signal(libc::SIGTTOU, libc::SIG_IGN) };
    let out = f();
    unsafe { libc::signal(libc::SIGTTOU, prev) };
    out
}

/// The job-control signals an interactive shell ignores so that a Ctrl-C,
/// Ctrl-\\ or Ctrl-Z reaches the foreground child's process group (which owns
/// the terminal — see `run_on_tty`) rather than killing or stopping aish itself.
const JOB_CONTROL_SIGNALS: [libc::c_int; 5] = [
    libc::SIGINT,
    libc::SIGQUIT,
    libc::SIGTSTP,
    libc::SIGTTOU,
    libc::SIGTTIN,
];

/// Set aish's disposition for the job-control signals to SIG_IGN. Called once at
/// interactive REPL startup so the shell survives the terminal-generated signals
/// it would otherwise be killed or suspended by; the signal is delivered to the
/// foreground child's process group instead.
pub fn ignore_job_control_signals() {
    for sig in JOB_CONTROL_SIGNALS {
        // SAFETY: installing SIG_IGN for a fixed set of signals at startup.
        unsafe { libc::signal(sig, libc::SIG_IGN) };
    }
}

/// Restore the job-control signals to their default disposition. Runs in a
/// foreground child's `pre_exec` because SIG_IGN is inherited across exec —
/// without this reset the child would inherit the shell's ignore and never see
/// Ctrl-C/Ctrl-\\/Ctrl-Z. Async-signal-safe: `signal(2)` only.
fn reset_job_control_signals() {
    for sig in JOB_CONTROL_SIGNALS {
        // SAFETY: restoring SIG_DFL for a fixed set of signals in the child.
        unsafe { libc::signal(sig, libc::SIG_DFL) };
    }
}

/// Owns the foreground child's pid for the lifetime of `run_on_tty`. On the
/// normal exit path the SIGCHLD task has already reaped it (`reaped = true`); if
/// the future is dropped first (Ctrl-C aborts the turn), SIGKILL the child's
/// group and reap it so no orphan lingers — replacing the `kill_on_drop` we gave
/// up by spawning raw.
struct ForegroundChild {
    pid: libc::pid_t,
    reaped: bool,
}

impl Drop for ForegroundChild {
    fn drop(&mut self) {
        if !self.reaped {
            // SAFETY: signalling/reaping our own child's process group (pgid == pid).
            unsafe {
                libc::kill(-self.pid, libc::SIGKILL);
                let mut s: libc::c_int = 0;
                libc::waitpid(self.pid, &mut s, 0);
            }
        }
    }
}

/// Restore the shell's process group as the terminal's foreground group on the
/// way out, even if the future is dropped. No-op when stdin isn't a tty.
struct ForegroundReclaim {
    on_tty: bool,
}

impl Drop for ForegroundReclaim {
    fn drop(&mut self) {
        if self.on_tty {
            // SAFETY: tcsetpgrp on fd 0 back to our own process group.
            with_sigttou_ignored(|| unsafe { libc::tcsetpgrp(0, libc::getpgrp()) });
        }
    }
}

/// RAII guard around a foreground child's terminal hand-off: snapshots the
/// terminal attributes on entry and restores them when the child exits, in case
/// the program crashed without cleaning up its raw-mode state. (The REPL no
/// longer needs a hand-off flag — with real process-group ownership the terminal
/// delivers job-control signals to the child's own pgrp directly; TASK-116.)
struct TtyGuard {
    saved: Option<libc::termios>,
}

impl TtyGuard {
    fn engage() -> Self {
        // SAFETY: plain libc terminal queries on fd 0; termios is POD.
        let saved = unsafe {
            if libc::isatty(0) == 1 {
                let mut t: libc::termios = std::mem::zeroed();
                (libc::tcgetattr(0, &mut t) == 0).then_some(t)
            } else {
                None
            }
        };
        Self { saved }
    }
}

impl Drop for TtyGuard {
    fn drop(&mut self) {
        if let Some(t) = self.saved {
            // SAFETY: restoring the exact attributes captured in engage().
            unsafe { libc::tcsetattr(0, libc::TCSADRAIN, &t) };
        }
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

fn write_file(call: &ToolCall, session: &mut Session, confirm: &mut Confirm<'_>) -> Result<String> {
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
    fn escalate_tool_gated_on_availability() {
        let has = |defs: &[ToolDef], n: &str| defs.iter().any(|d| d.name == n);
        // Offered only to a weak frontend (escalate_available = true).
        let weak = tool_defs(true, true);
        assert!(has(&weak, "escalate"));
        // A frontier frontend never sees it — no self-escalation.
        let strong = tool_defs(true, false);
        assert!(!has(&strong, "escalate"));
        // Independent of batch mode: escalate tracks its own flag.
        assert!(has(&tool_defs(false, true), "escalate"));
        assert!(!has(&tool_defs(false, false), "escalate"));
    }

    #[test]
    fn default_branch_guard_detects_protected_mutations() {
        let a = |s: &str| s.split_whitespace().map(String::from).collect::<Vec<_>>();
        // Explicit push to main/master — blocked regardless of current branch.
        assert!(protected_git_mutation(&a("push origin main"), Some("feature")).is_some());
        assert!(protected_git_mutation(&a("push -u origin master"), None).is_some());
        assert!(protected_git_mutation(&a("push origin HEAD:main"), Some("feature")).is_some());
        // Implicit push while ON the default branch — blocked.
        assert!(protected_git_mutation(&a("push"), Some("main")).is_some());
        assert!(protected_git_mutation(&a("push origin"), Some("main")).is_some());
        // Commit on the default branch — blocked.
        assert!(protected_git_mutation(&a("commit -m wip"), Some("main")).is_some());

        // Pushing/committing a feature branch — allowed.
        assert!(protected_git_mutation(&a("push origin feature"), Some("feature")).is_none());
        assert!(protected_git_mutation(&a("push -u origin feat/x"), Some("feat/x")).is_none());
        assert!(protected_git_mutation(&a("push"), Some("feature")).is_none());
        assert!(protected_git_mutation(&a("commit -m wip"), Some("feature")).is_none());
        // Non-mutating git verbs — never flagged.
        assert!(protected_git_mutation(&a("status"), Some("main")).is_none());
        assert!(protected_git_mutation(&a("log -5"), Some("main")).is_none());
    }

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
    async fn run_on_tty_reaps_foreground_child() {
        // AC1: foreground exec works through the new std::process + contained
        // SIGCHLD reaper path. Runs in CI without a controlling tty (on_tty is
        // false), exercising setpgid + the reaper without the tcsetpgrp dance.
        let session = Session::new().unwrap();
        let ok = run_on_tty("true", &[], &[], &session).await.unwrap();
        assert!(ok.success());
        let bad = run_on_tty("false", &[], &[], &session).await.unwrap();
        assert_eq!(bad.code(), Some(1));
    }

    // A `sh` snippet that exits 0 iff the running shell leads its own process
    // group. Field 1 of /proc/self/stat is the pid and field 5 is the pgrp; a
    // foreground child that `run_on_tty` setpgid'd into its own group has
    // pgrp == pid. `$$` stays the shell's pid inside the command substitution
    // (POSIX), and `comm` (field 2) is `(sh)`/`(dash)` — no embedded spaces — so
    // positional splitting keeps the fields aligned.
    const OWN_PGRP_PROBE: &str = r#"set -- $(cat /proc/$$/stat); [ "$5" = "$1" ]"#;

    #[tokio::test]
    async fn foreground_child_leads_its_own_process_group() {
        // AC1: child runs in its own pgrp (verified via /proc). Exercises the
        // real run_on_tty spawn path; the probe shell reports its own pgrp.
        let session = Session::new().unwrap();
        let status = run_on_tty(
            "sh",
            &["-c".to_string(), OWN_PGRP_PROBE.to_string()],
            &[],
            &session,
        )
        .await
        .unwrap();
        assert!(
            status.success(),
            "foreground child was not its own process-group leader"
        );
    }

    #[tokio::test]
    async fn repeated_foreground_launches_have_no_setpgid_race() {
        // AC2: no setpgid race under repeated launches. The parent+child double
        // setpgid must leave every back-to-back child as its own group leader; a
        // race would intermittently flip the probe's exit code.
        let session = Session::new().unwrap();
        for i in 0..20 {
            let status = run_on_tty(
                "sh",
                &["-c".to_string(), OWN_PGRP_PROBE.to_string()],
                &[],
                &session,
            )
            .await
            .unwrap();
            assert!(
                status.success(),
                "setpgid race on launch {i}: child not its own group leader"
            );
        }
    }

    #[test]
    fn job_control_signals_ignored_then_reset() {
        // TASK-115: the shell ignores the job-control signals; the child path
        // resets them to default so the foreground program stays interruptible.
        use std::mem::MaybeUninit;

        // Current disposition of `sig` per sigaction(2).
        fn disposition(sig: libc::c_int) -> libc::sighandler_t {
            // SAFETY: reading the installed sigaction into zeroed POD storage.
            unsafe {
                let mut old = MaybeUninit::<libc::sigaction>::zeroed();
                libc::sigaction(sig, std::ptr::null(), old.as_mut_ptr());
                old.assume_init().sa_sigaction
            }
        }

        // Snapshot so the test leaves the process's signal state untouched.
        let saved: Vec<_> = JOB_CONTROL_SIGNALS
            .iter()
            .map(|&s| (s, disposition(s)))
            .collect();

        ignore_job_control_signals();
        for &sig in &JOB_CONTROL_SIGNALS {
            assert_eq!(disposition(sig), libc::SIG_IGN, "signal {sig} not ignored");
        }

        reset_job_control_signals();
        for &sig in &JOB_CONTROL_SIGNALS {
            assert_eq!(disposition(sig), libc::SIG_DFL, "signal {sig} not reset to default");
        }

        // SAFETY: restoring each signal's captured prior disposition.
        for (sig, h) in saved {
            unsafe { libc::signal(sig, h) };
        }
    }

    // AC ac_3411acecc301 — exiting aish must not orphan stopped jobs. A stopped
    // child can't act on a pending SIGHUP, so hangup_jobs_on_exit must SIGCONT it
    // first and then SIGHUP its group; the child must terminate (by SIGHUP), not
    // linger as an orphan.
    #[test]
    fn hangup_on_exit_continues_and_kills_stopped_job() {
        use std::os::unix::process::CommandExt;
        use std::process::{Command, Stdio};

        // Leads its own process group (so we can signal it by group without
        // touching the test runner), stops itself, then would sleep — surviving
        // the shell unless it is continued and hung up.
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "kill -STOP $$; sleep 30"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        // SAFETY: setpgid(0, 0) in the post-fork child — new pgid == child pid.
        unsafe {
            cmd.pre_exec(|| match libc::setpgid(0, 0) {
                0 => Ok(()),
                _ => Err(std::io::Error::last_os_error()),
            });
        }
        let proc = cmd.spawn().expect("spawn stopped-job probe");
        let pid = proc.id() as libc::pid_t;
        // std::process::Child::drop neither reaps nor kills — we waitpid the pid.
        drop(proc);

        // Wait until the child has actually stopped itself before hanging it up.
        let mut st: libc::c_int = 0;
        let r = unsafe { libc::waitpid(pid, &mut st, libc::WUNTRACED) };
        assert!(r == pid && libc::WIFSTOPPED(st), "probe did not stop");

        // Register it as a stopped background job carrying its process group.
        let jobs: Jobs = Default::default();
        let (job, _kill_rx) = Job::background(1, "stopped probe".into());
        job.set_pgid(pid);
        job.stop();
        jobs.lock().unwrap().push(job);

        // Shell exit.
        hangup_jobs_on_exit(&jobs);

        // The child must be continued and terminated by SIGHUP — not orphaned.
        let mut st2: libc::c_int = 0;
        let r2 = unsafe { libc::waitpid(pid, &mut st2, 0) };
        assert_eq!(r2, pid, "failed to reap probe");
        assert!(libc::WIFSIGNALED(st2), "probe was not terminated by a signal");
        assert_eq!(
            libc::WTERMSIG(st2),
            libc::SIGHUP,
            "probe was not hung up (SIGHUP) on shell exit"
        );
    }

    #[tokio::test]
    async fn tokio_reaper_never_steals_foreground_pid() {
        // The disjoint-PID-set invariant: tokio reaps only its own children and
        // never waitpid(-1)s, so a foreground-style child spawned with
        // std::process stays ours to reap.
        use std::os::unix::process::ExitStatusExt;

        // A foreground child spawned the same way run_on_tty does — std::process,
        // deliberately not awaited here.
        let fg = std::process::Command::new("sleep").arg("0.3").spawn().unwrap();
        let fg_pid = fg.id() as libc::pid_t;
        std::mem::forget(fg); // we own the wait; don't let std reap it

        // Drive tokio's child reaper by spawning + awaiting a tokio child. If it
        // waitpid(-1)'d it would also harvest fg_pid out from under us.
        let status = tokio::process::Command::new("true").status().await.unwrap();
        assert!(status.success());

        // Let the foreground child exit, then prove WE can still reap it —
        // tokio stealing it via waitpid(-1) would have left ECHILD here.
        tokio::time::sleep(Duration::from_millis(600)).await;
        let mut wstatus: libc::c_int = 0;
        let r = unsafe { libc::waitpid(fg_pid, &mut wstatus, 0) };
        assert_eq!(r, fg_pid, "tokio's reaper stole the foreground pid via waitpid(-1)");
        assert!(std::process::ExitStatus::from_raw(wstatus).success());
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
