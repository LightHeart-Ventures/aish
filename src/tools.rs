use crate::backend::{ToolCall, ToolDef, ToolResult};
use crate::session::Session;
use anyhow::Result;
use regex::Regex;
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
    /// Allow this call and persist a recursive grant for the target's directory
    /// (the 'd' answer). On a read/write/delete prompt this allows every access
    /// of the SAME permission kind to anything under that directory, recursively.
    /// Where no directory can be derived (a non-path tool), the gate treats it
    /// as a one-time allow.
    AllowDir,
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
        // 'd' (AllowDir) is path-scoped; the generic gate has no path in scope,
        // so it degrades to a one-time allow here. The path-aware gates
        // (`gate_path`, `gate_delete`) handle the recursive directory grant.
        Decision::AllowOnce | Decision::AllowDir => true,
        Decision::AlwaysAllow => {
            session.allow_tool(key);
            true
        }
    }
}

/// A path-scoped permission kind for the directory ('d') grant.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Perm {
    Read,
    Write,
    Delete,
}

impl Perm {
    fn as_str(self) -> &'static str {
        match self {
            Perm::Read => "read",
            Perm::Write => "write",
            Perm::Delete => "delete",
        }
    }
}

/// File-deletion programs whose path arguments carry a delete permission — the
/// `run_program` delete gate offers the directory ('d') grant for these.
const DELETE_COMMANDS: &[&str] = &["rm", "rmdir", "unlink", "shred"];

/// Gate a path-scoped file operation (read/write), offering the directory ('d')
/// grant. Proceeds without prompting when the whole tool is always-allowed, or
/// when `path` already falls under a directory grant for `perm`. Otherwise the
/// user is prompted: 'a' persists the tool (every path), 'd' persists `path`'s
/// parent directory as a recursive grant for `perm`, 'y' allows just this call.
fn gate_path(
    session: &mut Session,
    perm: Perm,
    path: &Path,
    tool_key: &str,
    prompt: &str,
    confirm: &mut Confirm<'_>,
) -> bool {
    if session.is_tool_allowed(tool_key) || session.is_path_allowed(perm.as_str(), path) {
        return true;
    }
    match confirm(prompt) {
        Decision::Deny => false,
        Decision::AllowOnce => true,
        Decision::AlwaysAllow => {
            session.allow_tool(tool_key);
            true
        }
        Decision::AllowDir => {
            // Grant the file's parent directory recursively for this permission.
            // No derivable parent (a bare name or the filesystem root) → behaves
            // like a one-time allow.
            if let Some(dir) = path.parent() {
                if !dir.as_os_str().is_empty() {
                    session.allow_path_dir(perm.as_str(), dir);
                }
            }
            true
        }
    }
}

/// Gate a `run_program` file-deletion command (rm/rmdir/unlink/shred), offering
/// the directory ('d') grant. Proceeds without prompting when the binary is
/// always-allowed, or when every path argument is already covered by a delete
/// directory grant. On 'a' the whole binary is persisted; on 'd' each path
/// argument's parent directory is persisted as a recursive delete grant.
fn gate_delete(
    session: &mut Session,
    program: &str,
    args: &[String],
    prompt: &str,
    confirm: &mut Confirm<'_>,
) -> bool {
    let bin = bin_name(program);
    let paths: Vec<PathBuf> = args
        .iter()
        .filter(|a| !a.starts_with('-'))
        .map(|a| resolve(session, a))
        .collect();
    if session.is_tool_allowed(bin)
        || (!paths.is_empty()
            && paths.iter().all(|p| session.is_path_allowed(Perm::Delete.as_str(), p)))
    {
        return true;
    }
    match confirm(prompt) {
        Decision::Deny => false,
        Decision::AllowOnce => true,
        Decision::AlwaysAllow => {
            session.allow_tool(bin);
            true
        }
        Decision::AllowDir => {
            for p in &paths {
                if let Some(dir) = p.parent() {
                    if !dir.as_os_str().is_empty() {
                        session.allow_path_dir(Perm::Delete.as_str(), dir);
                    }
                }
            }
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
            name: "edit_file".into(),
            description: "Surgically edit a file in place by pattern — change specific lines without rewriting the whole file. Finds `pattern` (a literal substring, or a regular expression when `regex` is true) and either REPLACES each match with `replacement` (default), or INSERTS `replacement` as a new line before/after each matching line (`mode`). Scope the edit with `count` (change at most the first N matches; 0 = all) and/or `line_start`/`line_end` (1-based inclusive — only act on matches within that line range). With `regex`, `replacement` may reference capture groups (`$1`, `${name}`). Returns how many changes were made; reports \"no matches\" and leaves the file untouched when nothing matched. Prefer this over read_file+write_file for targeted edits to large files.".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "File to edit (relative paths resolve against the cwd)"},
                    "pattern": {"type": "string", "description": "Text to find — a literal substring, or a regular expression when `regex` is true"},
                    "replacement": {"type": "string", "description": "Replacement text (replace mode), or the line to insert (insert modes). Defaults to empty string, so replace mode with no replacement deletes the matches. With `regex`, may reference capture groups via $1 / ${name}."},
                    "regex": {"type": "boolean", "description": "Treat `pattern` as a regular expression (default false → literal match)"},
                    "mode": {"type": "string", "enum": ["replace", "insert_before", "insert_after"], "description": "replace (default) substitutes each match; insert_before / insert_after add `replacement` as a new line before/after each line that matches"},
                    "count": {"type": "integer", "description": "Change at most the first N matches (0 or omitted = all matches)"},
                    "line_start": {"type": "integer", "description": "Optional 1-based first line to act on — matches before this line are ignored"},
                    "line_end": {"type": "integer", "description": "Optional 1-based last line (inclusive) to act on — matches after this line are ignored"}
                },
                "required": ["path", "pattern"]
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
            name: "glob_expand".into(),
            description: "Expand a glob pattern to the list of matching paths (structured: type + \
size per entry). Supports `*`, `?`, `[...]` character classes, and `**` for recursive directory \
descent (e.g. `src/**/*.rs`). Call this instead of shelling out to find/ls with wildcards — globs \
don't exist in run_program."
                .into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "pattern": {"type": "string", "description": "Glob pattern, e.g. \"*.toml\" or \"src/**/*.rs\". A relative pattern resolves against `path` (or cwd); an absolute pattern (leading /) matches from the filesystem root."},
                    "path": {"type": "string", "description": "Base directory to expand a relative pattern against (default: cwd)."},
                    "type": {"type": "string", "enum": ["file", "dir", "any"], "description": "Filter results by entry type (default: any)."},
                    "max": {"type": "integer", "description": "Max results to return (default 1000)."}
                },
                "required": ["pattern"]
            }),
        },
        ToolDef {
            name: "grep_files".into(),
            description: "Search file contents for a substring and return structured matches \
(path:line: text). Recurses into directories (skipping .git), skips binary files, and can be \
scoped to files matching a glob. Call this instead of shelling out to grep/rg."
                .into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "pattern": {"type": "string", "description": "Substring to search for."},
                    "path": {"type": "string", "description": "File or directory to search (default: cwd). Directories are searched recursively."},
                    "glob": {"type": "string", "description": "Only search files whose name matches this glob (e.g. \"*.rs\")."},
                    "ignore_case": {"type": "boolean", "description": "Case-insensitive match (default false)."},
                    "context": {"type": "integer", "description": "Lines of context to show before and after each match (default 0)."},
                    "max": {"type": "integer", "description": "Max matches to return (default 500)."}
                },
                "required": ["pattern"]
            }),
        },
        ToolDef {
            name: "stat_file".into(),
            description: "Return structured metadata for a path: type, size, permissions (octal), \
uid/gid, link count, modified time, and symlink target. Call this instead of shelling out to stat."
                .into(),
            schema: json!({
                "type": "object",
                "properties": {"path": {"type": "string"}},
                "required": ["path"]
            }),
        },
        ToolDef {
            name: "diff_files".into(),
            description: "Compute a unified diff between two text files (or a file and inline \
content). Returns the diff with @@ hunk headers and +/- lines. Use it to verify an edit or show \
what changed before committing."
                .into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "a": {"type": "string", "description": "Path to the original (left) file."},
                    "b": {"type": "string", "description": "Path to the new (right) file. Omit when `b_content` is given."},
                    "b_content": {"type": "string", "description": "Inline text to diff against `a` instead of a second file."},
                    "context": {"type": "integer", "description": "Context lines around each hunk (default 3)."}
                },
                "required": ["a"]
            }),
        },
        ToolDef {
            name: "copy_file".into(),
            description: "Copy a file (or directory tree) to a new path. Refuses to overwrite an \
existing destination unless `overwrite` is true. If the destination is an existing directory, the \
source is copied into it under its own name. Clear collision errors — prefer this over run_program cp."
                .into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "src": {"type": "string"},
                    "dst": {"type": "string"},
                    "overwrite": {"type": "boolean", "description": "Allow replacing an existing destination (default false)."}
                },
                "required": ["src", "dst"]
            }),
        },
        ToolDef {
            name: "rename_file".into(),
            description: "Rename or move a file/directory. Refuses to overwrite an existing \
destination unless `overwrite` is true. Falls back to copy+remove across filesystems. Prefer this \
over run_program mv."
                .into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "src": {"type": "string"},
                    "dst": {"type": "string"},
                    "overwrite": {"type": "boolean", "description": "Allow replacing an existing destination (default false)."}
                },
                "required": ["src", "dst"]
            }),
        },
        ToolDef {
            name: "append_file".into(),
            description: "Append text to a file (creating it if missing) without reading it back — \
crash-safe for logs, transcripts, and audit trails. Prefer this over run_program tee/echo."
                .into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "content": {"type": "string"},
                    "newline": {"type": "boolean", "description": "Ensure the appended content ends with a trailing newline (default false)."}
                },
                "required": ["path", "content"]
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
your own tracking. Returns a table: id, kind, owner, status, task, result."
                .into(),
            schema: json!({ "type": "object", "properties": {} }),
        });
        // The agent-facing side of the `:tell` channel (the human types `:tell`;
        // the model calls this). Lets the interactive agent and one coordinator
        // STEER another in-flight coordinator without killing + relaunching it.
        defs.push(ToolDef {
            name: "tell".into(),
            description: "Steer an in-flight background coordinator WITHOUT restarting it: queue a message (a clarification, a course-correction, a narrower scope) that is folded into that coordinator's NEXT round. This is the `:tell` channel — how you and your background coordinators message each other mid-flight. Call background_status to find the target run id. Only a still-running coordinator can receive a message; a finished one has nothing to read it."
                .into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "id": {"type": "string", "description": "The coordinator's run id, or an unambiguous prefix (e.g. the short 8-char id shown by background_status)."},
                    "message": {"type": "string", "description": "The steering message — updated instructions or a clarification. It is treated as a supervisory interjection and takes precedence over the coordinator's earlier assumptions where they conflict."}
                },
                "required": ["id", "message"]
            }),
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
reasoning/text only; you then act on its answer with your tools. Use this for a step you must \
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
    // read_file/write_file and run_program/run_interactive run their OWN gates
    // below (the path-aware ones offer the 'd' directory grant), so they're
    // excluded here to avoid a double prompt.
    if session.mode == crate::session::Mode::Paranoid
        && !matches!(
            call.name.as_str(),
            "read_file" | "write_file" | "edit_file" | "run_program" | "run_interactive"
                | "copy_file" | "rename_file" | "append_file"
        )
    {
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
        "read_file" => read_file(call, session, confirm),
        "write_file" => write_file(call, session, confirm),
        "edit_file" => edit_file(call, session, confirm),
        "list_dir" => list_dir(call, session),
        "glob_expand" => glob_expand(call, session),
        "grep_files" => grep_files(call, session),
        "stat_file" => stat_file(call, session),
        "diff_files" => diff_files(call, session),
        "copy_file" => copy_file(call, session, confirm),
        "rename_file" => rename_file(call, session, confirm),
        "append_file" => append_file(call, session, confirm),
        "change_dir" => change_dir(call, session),
        "remember" => remember(call, session),
        "recall" => recall(call, session),
        "job_output" => job_output(call, session),
        "run_in_background" => run_in_background(call, session),
        "batch_result" => batch_result(call, session),
        "background_status" => background_status(session),
        "tell" => tell(call, session),
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
        // run_program/run_interactive self-gate (they're excluded from the
        // central paranoid gate in execute()), so confirm here too.
        Mode::Paranoid => true,
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

/// Drop a model-emitted argv that repeats the program as its own first
/// argument (`program="gh", args=["gh","pr","create"]` → the command
/// `gh gh pr create`). Some models reliably echo the binary name into argv[0];
/// left as-is that runs `gh gh …`, `git git …`, `brew brew …`, etc., which the
/// underlying tool rejects as an unknown subcommand — surfacing to the user as
/// doubled commands AND as a binary that looks "intercepted" because it never
/// does what was asked. The de-dup is conservative: it strips ONLY when the
/// first arg is byte-for-byte the program token (the exact string the model
/// passed, path included), so a deliberate `echo echo hi` text payload is the
/// one case it can touch — an acceptable trade for making every doubled
/// `git`/`gh`/`brew`/… invocation work. Pure + unit-tested.
pub(crate) fn dedup_program_argv(program: &str, mut args: Vec<String>) -> Vec<String> {
    if args.first().map(String::as_str) == Some(program) {
        args.remove(0);
    }
    args
}

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
    // Defend against the model echoing the binary into argv[0] (see
    // dedup_program_argv): `gh gh pr create` would otherwise fail as an unknown
    // subcommand and read as aish "intercepting" the binary.
    let args = dedup_program_argv(&program, args);

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
    if exec_needs_confirm(session.mode, &program, &args) {
        // File-deletion commands route through the path-aware delete gate, which
        // offers the directory ('d') grant; everything else uses the generic
        // binary-keyed gate.
        let allowed = if DELETE_COMMANDS.contains(&bin_name(&program)) {
            gate_delete(session, &program, &args, display.trim(), confirm)
        } else {
            gate(session, bin_name(&program), display.trim(), confirm)
        };
        if !allowed {
            return Ok("user declined to run this command".into());
        }
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

/// Print a PRE-FORMATTED line (which carries its own colour) over the current
/// terminal line, like [`announce`] but WITHOUT the dim wrapper or trailing-
/// space prefix join. The caller has already framed the line — e.g. the
/// contained `:output` pane rows (`worker::pane_row`), which supply their own
/// box-drawing border + label gutter. As with `announce`, the `\r\x1b[2K`
/// erases whatever was on the line (the prompt) and rustyline redraws it on
/// the next keypress.
pub fn announce_raw(line: &str) {
    eprint!("\r\x1b[2K{line}\n");
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

/// Resume a stopped job for `bg`/`fg` (TASK-122 / S3.5): send SIGCONT to the
/// job's process group so the suspended child actually runs again, then flip the
/// shared job state to running. A job with no recorded pgid (one that never
/// spawned a child) just has its state updated. Mirrors the SIGCONT half of
/// [`hangup_jobs_on_exit`].
pub fn resume_job(job: &Job) {
    if let Some(pgid) = job.pgid() {
        // SAFETY: signalling a managed job's own process group (pgid == leader pid),
        // which the shell put in its own group at spawn.
        unsafe {
            libc::kill(-pgid, libc::SIGCONT);
        }
    }
    job.resume();
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
        attached: session.attached.clone(),
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
/// `tell` tool — the agent-facing side of the `:tell` / SendMessage channel.
/// Queues a steering message for an in-flight background coordinator (resolved by
/// run id, exact or unambiguous prefix) so it is folded into that coordinator's
/// NEXT round (see `coordinator::drive`). This is how the interactive agent, and
/// one coordinator, message ANOTHER running coordinator mid-flight — to clarify,
/// correct course, or narrow scope — without killing and relaunching it. Mirrors
/// the REPL `:tell` command (`repl::tell_coordinator`): this session's in-memory
/// workers are matched first, then every durable run; a terminal run is refused
/// (nothing would read it) and an ambiguous prefix lists the matches.
fn tell(call: &ToolCall, session: &Session) -> Result<String> {
    let id = call.args["id"].as_str().unwrap_or_default().trim();
    let message = call.args["message"].as_str().unwrap_or_default().trim();
    if id.is_empty() || message.is_empty() {
        anyhow::bail!(
            "tell requires both `id` (a coordinator run id from background_status) and a non-empty `message`"
        );
    }
    let Some(store) = &session.coordinator_store else {
        anyhow::bail!("coordinator store unavailable — can't queue messages");
    };
    let hit = |rid: &str| rid == id || rid.starts_with(id);

    // (run_id, terminal?). This session's in-memory workers first — their id is
    // known before the child writes its store row, so a message sent right after
    // launch still lands — then durable runs from any session (deduped on run_id).
    let mut candidates: Vec<(String, bool)> = Vec::new();
    for w in session.worker_jobs.lock().unwrap().iter() {
        if hit(&w.id) {
            candidates.push((w.id.clone(), matches!(w.status().as_str(), "done" | "failed")));
        }
    }
    if let Ok(rows) = store.load_all() {
        for r in rows {
            if hit(&r.run_id) && !candidates.iter().any(|(rid, _)| rid == &r.run_id) {
                candidates.push((r.run_id.clone(), matches!(r.phase.as_str(), "done" | "failed")));
            }
        }
    }

    match candidates.as_slice() {
        [] => anyhow::bail!(
            "no in-flight coordinator matching '{id}' — call background_status to list run ids"
        ),
        [(run_id, terminal)] => {
            let short = crate::batch::short_id(run_id);
            if *terminal {
                anyhow::bail!(
                    "coordinator {short} has already finished — nothing would read the message"
                );
            }
            store.enqueue_message(run_id, message, Some(&session.session_id))?;
            let pending = store.pending_message_count(run_id).unwrap_or(0);
            Ok(format!(
                "✉ queued for coordinator {short} ({pending} pending) — folded in at the start of its next round"
            ))
        }
        many => {
            let ids = many
                .iter()
                .map(|(rid, _)| crate::batch::short_id(rid).to_string())
                .collect::<Vec<_>>()
                .join(", ");
            anyhow::bail!(
                "'{id}' matches {} coordinators ({ids}) — use a longer id prefix",
                many.len()
            )
        }
    }
}

fn background_status(session: &Session) -> Result<String> {
    let trunc = |t: &str| -> String {
        let t = t.replace('|', "\\|");
        if t.chars().count() > 56 {
            format!("{}…", t.chars().take(56).collect::<String>())
        } else {
            t
        }
    };
    
    let format_result = |result: Option<&String>, error: Option<&String>| -> String {
        match (result, error) {
            (Some(r), None) => {
                // Check if result contains a PR reference
                if let Some(pr_match) = r.split_whitespace().find(|s| s.starts_with("#")) {
                    format!("✓ {}", pr_match)
                } else {
                    "✓ success".to_string()
                }
            }
            (None, Some(e)) => {
                // Truncate error message to ~40 chars
                let truncated = if e.len() > 40 {
                    format!("{}…", &e[..40])
                } else {
                    e.clone()
                };
                format!("✗ {}", truncated)
            }
            _ => "—".to_string(),
        }
    };
    
    let mut out =
        String::from("| ID | Kind | Owner | Status | Since | Task | Result |\n|---|---|---|---|---|---|---|\n");
    let mut any = false;

    // This session's full-tool background coordinators (in memory; the live
    // subprocess handle is session-local even though its durable row is shared).
    for w in session.worker_jobs.lock().unwrap().iter() {
        any = true;
        out.push_str(&format!(
            "| `{}` | coordinator | you | {} | — | {} | — |\n",
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
                let result = format_result(r.result.as_ref(), r.error.as_ref());
                out.push_str(&format!(
                    "| `{}` | coordinator | {} | {} | {} | {} | {} |\n",
                    crate::batch::short_id(&r.run_id),
                    owner,
                    r.phase,
                    r.created_at.as_deref().unwrap_or("—"),
                    trunc(&r.task),
                    result
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
                let result = format_result(r.result.as_ref(), r.error.as_ref());
                out.push_str(&format!(
                    "| `{}` | batch | {} | {} | {} | {} | {} |\n",
                    crate::batch::short_id(&r.local_id),
                    owner,
                    r.status,
                    r.created_at.as_deref().unwrap_or("—"),
                    trunc(&r.task),
                    result
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
        // the child or it would be deaf to Ctrl-C/Ctrl-\\/Ctrl-Z. Async-signal-safe:
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

fn read_file(call: &ToolCall, session: &mut Session, confirm: &mut Confirm<'_>) -> Result<String> {
    let path = call.args["path"].as_str().ok_or_else(|| anyhow::anyhow!("missing path"))?;
    let full = resolve(session, path);
    // Reads run free except in paranoid mode, where they confirm — with the 'd'
    // option to allow every read under the file's directory recursively.
    if session.mode == crate::session::Mode::Paranoid {
        let prompt = format!("read {}", full.display());
        if !gate_path(session, Perm::Read, &full, "read_file", &prompt, confirm) {
            return Ok("user declined the read".into());
        }
    }
    let content = std::fs::read_to_string(&full)
        .map_err(|e| anyhow::anyhow!("{}: {e}", full.display()))?;
    Ok(truncate_middle(content, MAX_FILE_READ))
}

fn write_file(call: &ToolCall, session: &mut Session, confirm: &mut Confirm<'_>) -> Result<String> {
    let path = call.args["path"].as_str().ok_or_else(|| anyhow::anyhow!("missing path"))?;
    let content = call.args["content"].as_str().ok_or_else(|| anyhow::anyhow!("missing content"))?;
    let full = resolve(session, path);

    // A write is destructive in every mode short of yolo. The 'd' option allows
    // every write under the file's directory recursively.
    if !matches!(session.mode, crate::session::Mode::Yolo) {
        let preview: String = content.lines().take(5).collect::<Vec<_>>().join("\n  │ ");
        let more = content.lines().count().saturating_sub(5);
        let suffix = if more > 0 { format!("\n  │ … +{more} lines") } else { String::new() };
        let action = if full.exists() { "overwrite" } else { "write" };
        let prompt = format!("{action} {} ({} bytes)\n  │ {preview}{suffix}\n ", full.display(), content.len());
        if !gate_path(session, Perm::Write, &full, "write_file", &prompt, confirm) {
            return Ok("user declined the write".into());
        }
    }

    if let Some(parent) = full.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    std::fs::write(&full, content).map_err(|e| anyhow::anyhow!("{}: {e}", full.display()))?;
    Ok(format!("wrote {} bytes to {}", content.len(), full.display()))
}

// ---------------------------------------------------------------------------
// edit_file — surgical, pattern-based in-place edits so the model can change
// specific lines/ranges of a large file without round-tripping the whole thing
// through read_file + write_file. Pure logic lives in `apply_edit` (unit-tested);
// `edit_file` is the IO + safety-gate wrapper.
// ---------------------------------------------------------------------------

/// What an edit does at each match site.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum EditMode {
    /// Substitute the matched text with the replacement.
    Replace,
    /// Insert the replacement as a new line BEFORE each matching line.
    InsertBefore,
    /// Insert the replacement as a new line AFTER each matching line.
    InsertAfter,
}

/// A fully-parsed edit request (decoupled from the JSON call for testing).
struct EditSpec<'a> {
    pattern: &'a str,
    replacement: &'a str,
    is_regex: bool,
    mode: EditMode,
    /// Max matches to act on; 0 means unlimited.
    count: usize,
    /// 1-based inclusive line window the edit is restricted to (None = open).
    line_start: Option<usize>,
    line_end: Option<usize>,
}

/// 1-based line number of a byte offset into `content`.
fn line_of_offset(content: &str, off: usize) -> usize {
    content.as_bytes()[..off].iter().filter(|&&b| b == b'\n').count() + 1
}

/// Is `line` inside the (optional, 1-based inclusive) window?
fn within_window(line: usize, start: Option<usize>, end: Option<usize>) -> bool {
    start.map_or(true, |s| line >= s) && end.map_or(true, |e| line <= e)
}

/// Apply an edit to `content`, returning `(new_content, changes_made)`. Pure —
/// no IO — so the edit semantics (literal vs regex, replace vs insert, count and
/// line-range scoping, `$1` capture expansion) are unit-testable in isolation.
fn apply_edit(content: &str, spec: &EditSpec) -> Result<(String, usize)> {
    if spec.pattern.is_empty() {
        anyhow::bail!("pattern must not be empty");
    }
    let limit = if spec.count == 0 { usize::MAX } else { spec.count };

    match spec.mode {
        EditMode::Replace if spec.is_regex => {
            let re = Regex::new(spec.pattern)
                .map_err(|e| anyhow::anyhow!("invalid regex: {e}"))?;
            let mut out = String::with_capacity(content.len());
            let mut last = 0usize;
            let mut n = 0usize;
            for caps in re.captures_iter(content) {
                if n >= limit {
                    break;
                }
                let m = caps.get(0).expect("group 0 always present");
                if !within_window(line_of_offset(content, m.start()), spec.line_start, spec.line_end) {
                    continue;
                }
                out.push_str(&content[last..m.start()]);
                caps.expand(spec.replacement, &mut out);
                last = m.end();
                n += 1;
            }
            out.push_str(&content[last..]);
            Ok((out, n))
        }
        EditMode::Replace => {
            // Literal substring replacement.
            let pat = spec.pattern;
            let mut out = String::with_capacity(content.len());
            let mut last = 0usize;
            let mut search_from = 0usize;
            let mut n = 0usize;
            while n < limit {
                let Some(rel) = content[search_from..].find(pat) else { break };
                let start = search_from + rel;
                let end = start + pat.len();
                if within_window(line_of_offset(content, start), spec.line_start, spec.line_end) {
                    out.push_str(&content[last..start]);
                    out.push_str(spec.replacement);
                    last = end;
                    n += 1;
                }
                // Advance past this match (even when skipped) to find the next.
                // Guard against a zero-width literal (shouldn't happen — empty
                // pattern is rejected above).
                search_from = end.max(start + 1);
            }
            out.push_str(&content[last..]);
            Ok((out, n))
        }
        EditMode::InsertBefore | EditMode::InsertAfter => {
            // Line-oriented: insert `replacement` as a new line before/after each
            // line that matches. Splitting on '\n' and re-joining preserves the
            // file's trailing-newline shape (a trailing '\n' yields a final ""
            // element that round-trips).
            let re = if spec.is_regex {
                Some(Regex::new(spec.pattern).map_err(|e| anyhow::anyhow!("invalid regex: {e}"))?)
            } else {
                None
            };
            let line_matches = |line: &str| match &re {
                Some(re) => re.is_match(line),
                None => line.contains(spec.pattern),
            };
            let mut out: Vec<&str> = Vec::new();
            let mut n = 0usize;
            for (i, line) in content.split('\n').enumerate() {
                let hit = n < limit
                    && within_window(i + 1, spec.line_start, spec.line_end)
                    && line_matches(line);
                if hit && spec.mode == EditMode::InsertBefore {
                    out.push(spec.replacement);
                    n += 1;
                }
                out.push(line);
                if hit && spec.mode == EditMode::InsertAfter {
                    out.push(spec.replacement);
                    n += 1;
                }
            }
            Ok((out.join("\n"), n))
        }
    }
}

fn edit_file(call: &ToolCall, session: &mut Session, confirm: &mut Confirm<'_>) -> Result<String> {
    let path = call.args["path"].as_str().ok_or_else(|| anyhow::anyhow!("missing path"))?;
    let pattern = call.args["pattern"].as_str().ok_or_else(|| anyhow::anyhow!("missing pattern"))?;
    let replacement = call.args["replacement"].as_str().unwrap_or("");
    let is_regex = call.args["regex"].as_bool().unwrap_or(false);
    let mode = match call.args["mode"].as_str().unwrap_or("replace") {
        "replace" => EditMode::Replace,
        "insert_before" => EditMode::InsertBefore,
        "insert_after" => EditMode::InsertAfter,
        other => anyhow::bail!("unknown mode '{other}' — use replace, insert_before, or insert_after"),
    };
    let count = call.args["count"].as_u64().unwrap_or(0) as usize;
    let line_start = call.args["line_start"].as_u64().map(|n| n as usize);
    let line_end = call.args["line_end"].as_u64().map(|n| n as usize);
    let full = resolve(session, path);

    let content = std::fs::read_to_string(&full)
        .map_err(|e| anyhow::anyhow!("{}: {e}", full.display()))?;

    let spec = EditSpec {
        pattern,
        replacement,
        is_regex,
        mode,
        count,
        line_start,
        line_end,
    };
    // Compute the result first — a bad regex or a no-op match reports cleanly
    // without prompting or touching the file.
    let (new_content, changes) = apply_edit(&content, &spec)?;
    if changes == 0 {
        return Ok(format!(
            "no matches for {} {} in {} — file unchanged",
            if is_regex { "regex" } else { "pattern" },
            serde_json::to_string(pattern).unwrap_or_else(|_| format!("{pattern:?}")),
            full.display()
        ));
    }

    // An edit is a write: it's destructive in every mode short of yolo. The 'd'
    // option allows every write under the file's directory recursively.
    if !matches!(session.mode, crate::session::Mode::Yolo) {
        let verb = match mode {
            EditMode::Replace => "replace",
            EditMode::InsertBefore => "insert before",
            EditMode::InsertAfter => "insert after",
        };
        let prompt = format!(
            "edit {} — {verb} {changes} match(es), {} → {} bytes\n  │ pattern: {}\n ",
            full.display(),
            content.len(),
            new_content.len(),
            truncate_middle(pattern.to_string(), 120)
        );
        if !gate_path(session, Perm::Write, &full, "edit_file", &prompt, confirm) {
            return Ok("user declined the edit".into());
        }
    }

    std::fs::write(&full, &new_content).map_err(|e| anyhow::anyhow!("{}: {e}", full.display()))?;
    Ok(format!(
        "edited {}: {changes} change(s), {} bytes",
        full.display(),
        new_content.len()
    ))
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

// ---------------------------------------------------------------------------
// Native file-operation tools: glob_expand, grep_files, stat_file, diff_files,
// copy_file, rename_file, append_file. Structured returns, explicit collision /
// not-found semantics, and write-gating that mirrors write_file (free in yolo,
// otherwise the path-aware gate offering the 'd' recursive-directory grant).
// ---------------------------------------------------------------------------

/// Gate a write-class file op (copy/rename/append) on its destination path,
/// mirroring write_file: free in yolo, otherwise prompt with the path-aware
/// gate (offering the 'd' recursive-directory grant).
fn gate_write_op(
    session: &mut Session,
    dst: &Path,
    action: &str,
    tool_key: &str,
    confirm: &mut Confirm<'_>,
) -> bool {
    if matches!(session.mode, crate::session::Mode::Yolo) {
        return true;
    }
    let prompt = format!("{action} {}", dst.display());
    gate_path(session, Perm::Write, dst, tool_key, &prompt, confirm)
}

/// Match one path segment (containing no '/') against a glob segment supporting
/// `*` (zero+ chars), `?` (one char), and `[...]` character classes. Recursive;
/// fine for typical filename lengths.
fn glob_segment_match(pat: &[char], text: &[char]) -> bool {
    match pat.first() {
        None => text.is_empty(),
        Some('*') => {
            glob_segment_match(&pat[1..], text)
                || (!text.is_empty() && glob_segment_match(pat, &text[1..]))
        }
        Some('?') => !text.is_empty() && glob_segment_match(&pat[1..], &text[1..]),
        Some('[') => {
            if let Some(end) = glob_class_end(pat) {
                !text.is_empty()
                    && class_matches(&pat[1..end - 1], text[0])
                    && glob_segment_match(&pat[end..], &text[1..])
            } else {
                // Unterminated class → treat '[' literally.
                !text.is_empty() && text[0] == '[' && glob_segment_match(&pat[1..], &text[1..])
            }
        }
        Some(&c) => !text.is_empty() && text[0] == c && glob_segment_match(&pat[1..], &text[1..]),
    }
}

/// If `pat` opens a `[...]` character class, return the index one past its
/// closing ']' (handling a leading '!'/'^' negation and a ']' as first member).
/// None when unterminated.
fn glob_class_end(pat: &[char]) -> Option<usize> {
    let mut i = 1; // skip '['
    if i < pat.len() && (pat[i] == '!' || pat[i] == '^') {
        i += 1;
    }
    if i < pat.len() && pat[i] == ']' {
        i += 1; // a ']' immediately after '['/'^' is a literal member
    }
    while i < pat.len() && pat[i] != ']' {
        i += 1;
    }
    (i < pat.len() && pat[i] == ']').then_some(i + 1)
}

/// Does `c` satisfy a class body (the chars between the brackets)? A leading
/// '!' or '^' negates; `a-z` ranges are supported.
fn class_matches(body: &[char], c: char) -> bool {
    let (neg, body) = match body.first() {
        Some('!') | Some('^') => (true, &body[1..]),
        _ => (false, body),
    };
    let mut i = 0;
    let mut found = false;
    while i < body.len() {
        if i + 2 < body.len() && body[i + 1] == '-' {
            if body[i] <= c && c <= body[i + 2] {
                found = true;
            }
            i += 3;
        } else {
            if body[i] == c {
                found = true;
            }
            i += 1;
        }
    }
    found ^ neg
}

/// Match a slash-separated path against a slash-separated glob, where a `**`
/// segment matches zero or more whole path segments.
fn glob_path_match(pat_segs: &[&str], path_segs: &[&str]) -> bool {
    match pat_segs.split_first() {
        None => path_segs.is_empty(),
        Some((&"**", rest)) => {
            glob_path_match(rest, path_segs)
                || (!path_segs.is_empty() && glob_path_match(pat_segs, &path_segs[1..]))
        }
        Some((seg, rest)) => {
            if path_segs.is_empty() {
                return false;
            }
            let p: Vec<char> = seg.chars().collect();
            let t: Vec<char> = path_segs[0].chars().collect();
            glob_segment_match(&p, &t) && glob_path_match(rest, &path_segs[1..])
        }
    }
}

fn glob_expand(call: &ToolCall, session: &Session) -> Result<String> {
    let pattern = call.args["pattern"].as_str().ok_or_else(|| anyhow::anyhow!("missing pattern"))?;
    let type_filter = call.args["type"].as_str().unwrap_or("any");
    let max = call.args["max"].as_u64().unwrap_or(1000) as usize;

    // An absolute pattern matches from the filesystem root; otherwise expand
    // against `path` (or cwd).
    let (base, pat) = if let Some(rest) = pattern.strip_prefix('/') {
        (PathBuf::from("/"), rest.to_string())
    } else {
        let base = call
            .args["path"]
            .as_str()
            .map(|p| resolve(session, p))
            .unwrap_or_else(|| session.cwd.clone());
        (base, pattern.to_string())
    };
    let pat_segs: Vec<&str> = pat.split('/').filter(|s| !s.is_empty()).collect();
    if pat_segs.is_empty() {
        anyhow::bail!("empty pattern");
    }

    let mut out: Vec<String> = Vec::new();
    let mut visited = 0usize;
    let mut truncated = false;
    // Iterative DFS over the base tree, carrying each dir's path segments
    // relative to base.
    let mut stack: Vec<(PathBuf, Vec<String>)> = vec![(base.clone(), Vec::new())];
    'walk: while let Some((dir, segs)) = stack.pop() {
        let rd = match std::fs::read_dir(&dir) {
            Ok(rd) => rd,
            Err(_) => continue,
        };
        for entry in rd.flatten() {
            visited += 1;
            if visited > 200_000 {
                truncated = true;
                break 'walk;
            }
            let ft = match entry.file_type() {
                Ok(ft) => ft,
                Err(_) => continue,
            };
            let name = entry.file_name().to_string_lossy().into_owned();
            let mut rel = segs.clone();
            rel.push(name);
            let rel_refs: Vec<&str> = rel.iter().map(String::as_str).collect();
            if glob_path_match(&pat_segs, &rel_refs) {
                let type_ok = match type_filter {
                    "file" => ft.is_file(),
                    "dir" => ft.is_dir(),
                    _ => true,
                };
                if type_ok {
                    let kind = if ft.is_dir() {
                        "dir "
                    } else if ft.is_symlink() {
                        "link"
                    } else {
                        "file"
                    };
                    let size = if ft.is_file() {
                        entry.metadata().map(|m| format!(" {}", m.len())).unwrap_or_default()
                    } else {
                        String::new()
                    };
                    out.push(format!("{kind} {}{size}", rel.join("/")));
                    if out.len() >= max {
                        truncated = true;
                        break 'walk;
                    }
                }
            }
            // Descend into real subdirectories (never follow symlinked dirs — avoids cycles).
            if ft.is_dir() && !ft.is_symlink() {
                stack.push((entry.path(), rel));
            }
        }
    }
    out.sort();
    if out.is_empty() {
        return Ok(format!("[no matches for {pattern}]"));
    }
    let mut res = out.join("\n");
    if truncated {
        res.push_str("\n…[results truncated]");
    }
    Ok(truncate_middle(res, MAX_OUTPUT))
}

fn grep_files(call: &ToolCall, session: &Session) -> Result<String> {
    let pattern = call.args["pattern"].as_str().ok_or_else(|| anyhow::anyhow!("missing pattern"))?;
    if pattern.is_empty() {
        anyhow::bail!("empty pattern");
    }
    let ignore_case = call.args["ignore_case"].as_bool().unwrap_or(false);
    let context = call.args["context"].as_u64().unwrap_or(0) as usize;
    let max = call.args["max"].as_u64().unwrap_or(500) as usize;
    let glob_seg: Option<Vec<char>> =
        call.args["glob"].as_str().map(|g| g.chars().collect());
    let base = call
        .args["path"]
        .as_str()
        .map(|p| resolve(session, p))
        .unwrap_or_else(|| session.cwd.clone());

    let needle = if ignore_case { pattern.to_lowercase() } else { pattern.to_string() };

    // Gather candidate files (a single file, or a recursive directory walk).
    let mut files: Vec<PathBuf> = Vec::new();
    let scope_is_file = base.is_file();
    if scope_is_file {
        files.push(base.clone());
    } else if base.is_dir() {
        let mut stack = vec![base.clone()];
        let mut visited = 0usize;
        while let Some(dir) = stack.pop() {
            let rd = match std::fs::read_dir(&dir) {
                Ok(rd) => rd,
                Err(_) => continue,
            };
            for entry in rd.flatten() {
                visited += 1;
                if visited > 200_000 {
                    break;
                }
                let ft = match entry.file_type() {
                    Ok(ft) => ft,
                    Err(_) => continue,
                };
                if ft.is_dir() && !ft.is_symlink() {
                    if entry.file_name() == ".git" {
                        continue;
                    }
                    stack.push(entry.path());
                } else if ft.is_file() {
                    if let Some(ref g) = glob_seg {
                        let name: Vec<char> =
                            entry.file_name().to_string_lossy().chars().collect();
                        if !glob_segment_match(g, &name) {
                            continue;
                        }
                    }
                    files.push(entry.path());
                }
            }
        }
    } else {
        anyhow::bail!("{}: not found", base.display());
    }
    files.sort();

    let mut out: Vec<String> = Vec::new();
    let mut total = 0usize;
    'outer: for f in &files {
        let bytes = match std::fs::read(f) {
            Ok(b) => b,
            Err(_) => continue,
        };
        if bytes.contains(&0u8) {
            continue; // skip binary files
        }
        let text = String::from_utf8_lossy(&bytes);
        let lines: Vec<&str> = text.lines().collect();
        let display = if scope_is_file {
            f.display().to_string()
        } else {
            f.strip_prefix(&base).unwrap_or(f).display().to_string()
        };
        for (i, line) in lines.iter().enumerate() {
            let hay = if ignore_case { line.to_lowercase() } else { (*line).to_string() };
            if hay.contains(&needle) {
                if context > 0 {
                    let lo = i.saturating_sub(context);
                    let hi = (i + context + 1).min(lines.len());
                    for j in lo..hi {
                        let marker = if j == i { ":" } else { "-" };
                        out.push(format!("{display}:{}{marker} {}", j + 1, lines[j]));
                    }
                    out.push("--".into());
                } else {
                    out.push(format!("{display}:{}: {}", i + 1, line));
                }
                total += 1;
                if total >= max {
                    out.push("…[matches truncated]".into());
                    break 'outer;
                }
            }
        }
    }
    if out.is_empty() {
        return Ok(format!("[no matches for \"{pattern}\"]"));
    }
    Ok(truncate_middle(out.join("\n"), MAX_OUTPUT))
}

fn human_age(secs: i64) -> String {
    let s = secs.max(0);
    if s < 60 {
        format!("{s}s")
    } else if s < 3600 {
        format!("{}m", s / 60)
    } else if s < 86_400 {
        format!("{}h", s / 3600)
    } else {
        format!("{}d", s / 86_400)
    }
}

fn stat_file(call: &ToolCall, session: &Session) -> Result<String> {
    use std::os::unix::fs::MetadataExt;
    use std::os::unix::fs::PermissionsExt;
    let path = call.args["path"].as_str().ok_or_else(|| anyhow::anyhow!("missing path"))?;
    let full = resolve(session, path);
    // symlink_metadata so a symlink reports as a symlink rather than its target.
    let meta = std::fs::symlink_metadata(&full)
        .map_err(|e| anyhow::anyhow!("{}: {e}", full.display()))?;
    let ft = meta.file_type();
    let kind = if ft.is_dir() {
        "dir"
    } else if ft.is_symlink() {
        "symlink"
    } else if ft.is_file() {
        "file"
    } else {
        "other"
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let mut lines = vec![
        format!("path: {}", full.display()),
        format!("type: {kind}"),
        format!("size: {} bytes", meta.len()),
        format!("perms: {:o}", meta.permissions().mode() & 0o7777),
        format!("uid/gid: {}/{}", meta.uid(), meta.gid()),
        format!("nlink: {}", meta.nlink()),
        format!("modified: {} (epoch), {} ago", meta.mtime(), human_age(now - meta.mtime())),
    ];
    if ft.is_symlink() {
        if let Ok(target) = std::fs::read_link(&full) {
            lines.push(format!("symlink_target: {}", target.display()));
        }
    }
    Ok(lines.join("\n"))
}

fn diff_files(call: &ToolCall, session: &Session) -> Result<String> {
    let a_path = call.args["a"].as_str().ok_or_else(|| anyhow::anyhow!("missing a"))?;
    let full_a = resolve(session, a_path);
    let a_text = std::fs::read_to_string(&full_a)
        .map_err(|e| anyhow::anyhow!("{}: {e}", full_a.display()))?;
    let (b_text, b_label) = if let Some(bc) = call.args["b_content"].as_str() {
        (bc.to_string(), "<b_content>".to_string())
    } else if let Some(bp) = call.args["b"].as_str() {
        let full_b = resolve(session, bp);
        let t = std::fs::read_to_string(&full_b)
            .map_err(|e| anyhow::anyhow!("{}: {e}", full_b.display()))?;
        (t, full_b.display().to_string())
    } else {
        anyhow::bail!("provide either `b` (path) or `b_content` (inline text)");
    };
    let context = call.args["context"].as_u64().unwrap_or(3) as usize;
    let diff = unified_diff(&a_text, &b_text, &full_a.display().to_string(), &b_label, context);
    if diff.trim().is_empty() {
        return Ok("[files are identical]".into());
    }
    Ok(truncate_middle(diff, MAX_OUTPUT))
}

#[derive(PartialEq)]
enum DiffOp {
    Eq,
    Del,
    Add,
}

/// Produce a unified diff of two texts via an LCS line alignment. O(n*m) time
/// and memory — guarded so a huge pair falls back to a note rather than OOM.
fn unified_diff(a: &str, b: &str, a_name: &str, b_name: &str, context: usize) -> String {
    let al: Vec<&str> = a.lines().collect();
    let bl: Vec<&str> = b.lines().collect();
    let (n, m) = (al.len(), bl.len());
    if (n as u64 + 1) * (m as u64 + 1) > 8_000_000 {
        return format!(
            "--- {a_name}\n+++ {b_name}\n[files too large for inline diff: {n} vs {m} lines — use run_program diff]\n"
        );
    }
    // LCS length DP.
    let mut dp = vec![vec![0u32; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            dp[i][j] = if al[i] == bl[j] {
                dp[i + 1][j + 1] + 1
            } else {
                dp[i + 1][j].max(dp[i][j + 1])
            };
        }
    }
    // Backtrack into an edit script.
    let mut ops: Vec<(DiffOp, &str)> = Vec::new();
    let (mut i, mut j) = (0usize, 0usize);
    while i < n && j < m {
        if al[i] == bl[j] {
            ops.push((DiffOp::Eq, al[i]));
            i += 1;
            j += 1;
        } else if dp[i + 1][j] >= dp[i][j + 1] {
            ops.push((DiffOp::Del, al[i]));
            i += 1;
        } else {
            ops.push((DiffOp::Add, bl[j]));
            j += 1;
        }
    }
    while i < n {
        ops.push((DiffOp::Del, al[i]));
        i += 1;
    }
    while j < m {
        ops.push((DiffOp::Add, bl[j]));
        j += 1;
    }

    let changed: Vec<usize> = ops
        .iter()
        .enumerate()
        .filter(|(_, (o, _))| *o != DiffOp::Eq)
        .map(|(k, _)| k)
        .collect();
    if changed.is_empty() {
        return String::new();
    }

    // Coalesce changed positions into hunks padded by `context`.
    let mut hunks: Vec<(usize, usize)> = Vec::new();
    let mut start = changed[0].saturating_sub(context);
    let mut end = (changed[0] + context + 1).min(ops.len());
    for &c in &changed[1..] {
        let s = c.saturating_sub(context);
        if s <= end {
            end = (c + context + 1).min(ops.len());
        } else {
            hunks.push((start, end));
            start = s;
            end = (c + context + 1).min(ops.len());
        }
    }
    hunks.push((start, end));

    let mut out = format!("--- {a_name}\n+++ {b_name}\n");
    for (s, e) in hunks {
        // 1-based start line numbers at the hunk's first op.
        let (mut a_ln, mut b_ln) = (1usize, 1usize);
        for (o, _) in &ops[..s] {
            match o {
                DiffOp::Eq => {
                    a_ln += 1;
                    b_ln += 1;
                }
                DiffOp::Del => a_ln += 1,
                DiffOp::Add => b_ln += 1,
            }
        }
        let (mut a_cnt, mut b_cnt) = (0usize, 0usize);
        for (o, _) in &ops[s..e] {
            match o {
                DiffOp::Eq => {
                    a_cnt += 1;
                    b_cnt += 1;
                }
                DiffOp::Del => a_cnt += 1,
                DiffOp::Add => b_cnt += 1,
            }
        }
        out.push_str(&format!("@@ -{a_ln},{a_cnt} +{b_ln},{b_cnt} @@\n"));
        for (o, line) in &ops[s..e] {
            let prefix = match o {
                DiffOp::Eq => ' ',
                DiffOp::Del => '-',
                DiffOp::Add => '+',
            };
            out.push(prefix);
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

/// Recursively copy a directory tree, returning total bytes copied.
fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<u64> {
    std::fs::create_dir_all(dst)?;
    let mut total = 0u64;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let ft = entry.file_type()?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if ft.is_dir() {
            total += copy_dir_recursive(&from, &to)?;
        } else if ft.is_file() {
            total += std::fs::copy(&from, &to)?;
        }
    }
    Ok(total)
}

/// Resolve a destination that may be an existing directory into a concrete
/// target path (dir → dir/<src-filename>), the shared cp/mv "into a directory"
/// convenience.
fn resolve_dest(full_src: &Path, mut full_dst: PathBuf) -> PathBuf {
    if full_dst.is_dir() {
        if let Some(name) = full_src.file_name() {
            full_dst = full_dst.join(name);
        }
    }
    full_dst
}

fn copy_file(call: &ToolCall, session: &mut Session, confirm: &mut Confirm<'_>) -> Result<String> {
    let src = call.args["src"].as_str().ok_or_else(|| anyhow::anyhow!("missing src"))?;
    let dst = call.args["dst"].as_str().ok_or_else(|| anyhow::anyhow!("missing dst"))?;
    let overwrite = call.args["overwrite"].as_bool().unwrap_or(false);
    let full_src = resolve(session, src);
    if !full_src.exists() {
        anyhow::bail!("{}: source not found", full_src.display());
    }
    let full_dst = resolve_dest(&full_src, resolve(session, dst));
    if full_dst.exists() && !overwrite {
        anyhow::bail!(
            "{}: destination exists (pass overwrite:true to replace)",
            full_dst.display()
        );
    }
    if !gate_write_op(session, &full_dst, "copy to", "copy_file", confirm) {
        return Ok("user declined the copy".into());
    }
    if let Some(parent) = full_dst.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let bytes = if full_src.is_dir() {
        copy_dir_recursive(&full_src, &full_dst)?
    } else {
        std::fs::copy(&full_src, &full_dst).map_err(|e| anyhow::anyhow!("{}: {e}", full_dst.display()))?
    };
    Ok(format!("copied {} → {} ({bytes} bytes)", full_src.display(), full_dst.display()))
}

fn rename_file(call: &ToolCall, session: &mut Session, confirm: &mut Confirm<'_>) -> Result<String> {
    let src = call.args["src"].as_str().ok_or_else(|| anyhow::anyhow!("missing src"))?;
    let dst = call.args["dst"].as_str().ok_or_else(|| anyhow::anyhow!("missing dst"))?;
    let overwrite = call.args["overwrite"].as_bool().unwrap_or(false);
    let full_src = resolve(session, src);
    if !full_src.exists() {
        anyhow::bail!("{}: source not found", full_src.display());
    }
    let full_dst = resolve_dest(&full_src, resolve(session, dst));
    if full_dst.exists() && !overwrite {
        anyhow::bail!(
            "{}: destination exists (pass overwrite:true to replace)",
            full_dst.display()
        );
    }
    if !gate_write_op(session, &full_dst, "rename to", "rename_file", confirm) {
        return Ok("user declined the rename".into());
    }
    if let Some(parent) = full_dst.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    // rename(2) is atomic within a filesystem; on EXDEV fall back to copy+remove.
    if std::fs::rename(&full_src, &full_dst).is_err() {
        if full_src.is_dir() {
            copy_dir_recursive(&full_src, &full_dst)?;
            std::fs::remove_dir_all(&full_src)?;
        } else {
            std::fs::copy(&full_src, &full_dst)?;
            std::fs::remove_file(&full_src)?;
        }
    }
    Ok(format!("renamed {} → {}", full_src.display(), full_dst.display()))
}

fn append_file(call: &ToolCall, session: &mut Session, confirm: &mut Confirm<'_>) -> Result<String> {
    use std::io::Write;
    let path = call.args["path"].as_str().ok_or_else(|| anyhow::anyhow!("missing path"))?;
    let mut content = call
        .args["content"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("missing content"))?
        .to_string();
    if call.args["newline"].as_bool().unwrap_or(false) && !content.ends_with('\n') {
        content.push('\n');
    }
    let full = resolve(session, path);
    if !gate_write_op(session, &full, "append to", "append_file", confirm) {
        return Ok("user declined the append".into());
    }
    if let Some(parent) = full.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&full)
        .map_err(|e| anyhow::anyhow!("{}: {e}", full.display()))?;
    f.write_all(content.as_bytes()).map_err(|e| anyhow::anyhow!("{}: {e}", full.display()))?;
    f.flush().ok();
    Ok(format!("appended {} bytes to {}", content.len(), full.display()))
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

    fn spec<'a>(
        pattern: &'a str,
        replacement: &'a str,
        is_regex: bool,
        mode: EditMode,
        count: usize,
        line_start: Option<usize>,
        line_end: Option<usize>,
    ) -> EditSpec<'a> {
        EditSpec { pattern, replacement, is_regex, mode, count, line_start, line_end }
    }

    #[test]
    fn edit_literal_replace_all_and_count() {
        let c = "foo bar foo baz foo";
        // All occurrences.
        let (out, n) = apply_edit(c, &spec("foo", "X", false, EditMode::Replace, 0, None, None)).unwrap();
        assert_eq!(out, "X bar X baz X");
        assert_eq!(n, 3);
        // First N only.
        let (out, n) = apply_edit(c, &spec("foo", "X", false, EditMode::Replace, 2, None, None)).unwrap();
        assert_eq!(out, "X bar X baz foo");
        assert_eq!(n, 2);
    }

    #[test]
    fn edit_literal_replace_empty_deletes() {
        let (out, n) = apply_edit("a-b-c", &spec("-", "", false, EditMode::Replace, 0, None, None)).unwrap();
        assert_eq!(out, "abc");
        assert_eq!(n, 2);
    }

    #[test]
    fn edit_no_match_reports_zero() {
        let (out, n) = apply_edit("hello", &spec("xyz", "Q", false, EditMode::Replace, 0, None, None)).unwrap();
        assert_eq!(out, "hello");
        assert_eq!(n, 0);
    }

    #[test]
    fn edit_replace_in_line_range_only() {
        let c = "foo\nfoo\nfoo\nfoo";
        // Restrict to lines 2..=3 — only those two `foo`s change.
        let (out, n) = apply_edit(c, &spec("foo", "X", false, EditMode::Replace, 0, Some(2), Some(3))).unwrap();
        assert_eq!(out, "foo\nX\nX\nfoo");
        assert_eq!(n, 2);
    }

    #[test]
    fn edit_regex_replace_with_capture_groups() {
        let c = "name: alice\nname: bob";
        let (out, n) = apply_edit(
            c,
            &spec(r"name: (\w+)", "user=$1", true, EditMode::Replace, 0, None, None),
        )
        .unwrap();
        assert_eq!(out, "user=alice\nuser=bob");
        assert_eq!(n, 2);
    }

    #[test]
    fn edit_regex_replace_multiline_anchors() {
        // Default regex is single-line; `.` doesn't cross newlines unless asked.
        let c = "a1\nb2\nc3";
        let (out, n) = apply_edit(c, &spec(r"\d", "#", true, EditMode::Replace, 0, None, None)).unwrap();
        assert_eq!(out, "a#\nb#\nc#");
        assert_eq!(n, 3);
    }

    #[test]
    fn edit_insert_after_matching_line() {
        let c = "alpha\nbeta\ngamma";
        let (out, n) = apply_edit(c, &spec("beta", "INSERTED", false, EditMode::InsertAfter, 0, None, None)).unwrap();
        assert_eq!(out, "alpha\nbeta\nINSERTED\ngamma");
        assert_eq!(n, 1);
    }

    #[test]
    fn edit_insert_before_matching_line() {
        let c = "alpha\nbeta\ngamma";
        let (out, n) = apply_edit(c, &spec("gamma", "// note", false, EditMode::InsertBefore, 0, None, None)).unwrap();
        assert_eq!(out, "alpha\nbeta\n// note\ngamma");
        assert_eq!(n, 1);
    }

    #[test]
    fn edit_preserves_trailing_newline_on_insert() {
        let c = "x\ny\n";
        let (out, n) = apply_edit(c, &spec("x", "z", false, EditMode::InsertAfter, 0, None, None)).unwrap();
        assert_eq!(out, "x\nz\ny\n");
        assert_eq!(n, 1);
    }

    #[test]
    fn edit_empty_pattern_rejected() {
        assert!(apply_edit("abc", &spec("", "x", false, EditMode::Replace, 0, None, None)).is_err());
    }

    #[test]
    fn edit_bad_regex_rejected() {
        assert!(apply_edit("abc", &spec("(", "x", true, EditMode::Replace, 0, None, None)).is_err());
    }

    #[tokio::test]
    async fn edit_file_end_to_end_replace() {
        let dir = std::env::temp_dir().join(format!("aish_edit_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("sample.txt");
        std::fs::write(&file, "line1\nTARGET\nline3\n").unwrap();

        let mut session = Session::new().unwrap();
        session.mode = crate::session::Mode::Yolo; // skip the gate in the test
        let mut confirm = |_: &str| Decision::AllowOnce;

        let call = ToolCall {
            id: "t".into(),
            name: "edit_file".into(),
            args: json!({
                "path": file.to_string_lossy(),
                "pattern": "TARGET",
                "replacement": "REPLACED",
            }),
        };
        let r = execute(&call, &mut session, &mut confirm).await;
        assert!(!r.is_error, "got: {}", r.content);
        assert!(r.content.contains("1 change"), "got: {}", r.content);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "line1\nREPLACED\nline3\n");

        // A no-match edit leaves the file untouched and says so.
        let miss = ToolCall {
            id: "t2".into(),
            name: "edit_file".into(),
            args: json!({ "path": file.to_string_lossy(), "pattern": "NOPE", "replacement": "x" }),
        };
        let r = execute(&miss, &mut session, &mut confirm).await;
        assert!(!r.is_error);
        assert!(r.content.contains("no matches"), "got: {}", r.content);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "line1\nREPLACED\nline3\n");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn edit_file_tool_is_offered() {
        let defs = tool_defs(false, false);
        assert!(defs.iter().any(|d| d.name == "edit_file"), "edit_file must be in the tool set");
    }

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
    fn tell_tool_offered_only_in_batch_mode() {
        let has = |defs: &[ToolDef], n: &str| defs.iter().any(|d| d.name == n);
        // The `tell` channel rides with background mode — there are no
        // coordinators to steer when batch mode is off.
        assert!(has(&tool_defs(true, false), "tell"));
        assert!(has(&tool_defs(true, true), "tell"));
        assert!(!has(&tool_defs(false, false), "tell"));
        assert!(!has(&tool_defs(false, true), "tell"));
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

    #[test]
    fn dedup_program_argv_strips_echoed_binary() {
        let v = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        // The reported bug: the model repeats the binary as argv[0].
        assert_eq!(dedup_program_argv("gh", v(&["gh", "pr", "create"])), v(&["pr", "create"]));
        assert_eq!(dedup_program_argv("ls", v(&["ls", "-la"])), v(&["-la"]));
        assert_eq!(dedup_program_argv("grep", v(&["grep", "x", "f"])), v(&["x", "f"]));
        // Absolute-path program with the same absolute path echoed.
        assert_eq!(
            dedup_program_argv("/opt/homebrew/bin/gh", v(&["/opt/homebrew/bin/gh", "--version"])),
            v(&["--version"])
        );
        // Only ONE copy is stripped (a genuine repeated token survives).
        assert_eq!(dedup_program_argv("gh", v(&["gh", "gh"])), v(&["gh"]));
        // Untouched: normal argv, empty argv, and a first arg that only shares
        // the basename (path differs) — too risky to strip on a partial match.
        assert_eq!(dedup_program_argv("ls", v(&["-la"])), v(&["-la"]));
        assert_eq!(dedup_program_argv("ls", v(&[])), Vec::<String>::new());
        assert_eq!(
            dedup_program_argv("/usr/bin/gh", v(&["gh", "pr"])),
            v(&["gh", "pr"])
        );
    }

    #[tokio::test]
    async fn run_program_dedups_echoed_binary() {
        // End-to-end: program="echo", args=["echo","hi"] must run `echo hi`,
        // not `echo echo hi` — the execution-side proof of the de-dup.
        let out = run(&call("echo", &["echo", "hi"], None)).await;
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

    #[tokio::test]
    async fn allow_dir_grants_recursive_read_permission() {
        // Paranoid mode confirms reads. Answering 'd' (AllowDir) on one file
        // grants every read under that file's directory, recursively — so a
        // sibling read in the same dir proceeds without a second prompt, while a
        // read OUTSIDE the granted dir still prompts.
        use std::cell::Cell;
        let dir = std::env::temp_dir().join(format!("aish_allowdir_{}", std::process::id()));
        let sub = dir.join("src");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(dir.join("Cargo.toml"), b"[package]").unwrap();
        std::fs::write(sub.join("main.rs"), b"fn main() {}").unwrap();
        let outside = std::env::temp_dir().join(format!("aish_allowdir_out_{}.txt", std::process::id()));
        std::fs::write(&outside, b"x").unwrap();

        let mut session = Session::new().unwrap();
        let dbpath = std::env::temp_dir().join(format!("aish_allowdir_{}.db", std::process::id()));
        let _ = std::fs::remove_file(&dbpath);
        session.db = Some(crate::db::Db::open(&dbpath).unwrap());
        session.mode = crate::session::Mode::Paranoid;

        let rd = |p: &std::path::Path| ToolCall {
            id: "t".into(),
            name: "read_file".into(),
            args: json!({ "path": p.to_string_lossy() }),
        };

        let calls = Cell::new(0);
        // First read in the dir: prompt fires, user answers 'd'.
        {
            let mut confirm = |_: &str| {
                calls.set(calls.get() + 1);
                Decision::AllowDir
            };
            let r = execute(&rd(&dir.join("Cargo.toml")), &mut session, &mut confirm).await;
            assert!(!r.is_error && !r.content.contains("declined"), "got: {}", r.content);
        }
        assert_eq!(calls.get(), 1);
        assert!(session.is_path_allowed("read", &sub.join("main.rs")));

        // A sibling read under the SAME directory must not prompt (would Deny).
        {
            let mut confirm = |_: &str| {
                calls.set(calls.get() + 1);
                Decision::Deny
            };
            let r = execute(&rd(&sub.join("main.rs")), &mut session, &mut confirm).await;
            assert!(!r.is_error && !r.content.contains("declined"), "got: {}", r.content);
        }
        assert_eq!(calls.get(), 1, "sibling read under the granted dir must skip the prompt");

        // A read OUTSIDE the granted dir still prompts (here: denied).
        {
            let mut confirm = |_: &str| {
                calls.set(calls.get() + 1);
                Decision::Deny
            };
            let r = execute(&rd(&outside), &mut session, &mut confirm).await;
            assert!(r.content.contains("declined"), "outside read should prompt+deny: {}", r.content);
        }
        assert_eq!(calls.get(), 2, "read outside the granted dir must prompt");

        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_file(&outside);
        let _ = std::fs::remove_file(&dbpath);
    }

    // AC ac_ea23cef2b000 — `bg`/`fg` continue a genuinely stopped job: SIGCONT
    // reaches the suspended child's process group and the shared state flips back
    // to running. Spawns a real `sleep`, SIGSTOPs it, then asserts `resume_job`
    // both continues the process (WIFCONTINUED) and updates the job state.
    #[test]
    fn resume_job_continues_a_stopped_child() {
        use std::os::unix::process::CommandExt;

        // Spawn `sleep 30` as its own process-group leader (mirrors spawn_background).
        let child = unsafe {
            std::process::Command::new("sleep")
                .arg("30")
                .pre_exec(|| match libc::setpgid(0, 0) {
                    0 => Ok(()),
                    _ => Err(std::io::Error::last_os_error()),
                })
                .spawn()
                .expect("spawn sleep")
        };
        let pid = child.id() as libc::pid_t;
        std::mem::forget(child); // we reap via waitpid below
        unsafe { libc::setpgid(pid, pid) }; // close the spawn race (EACCES after exec is fine)

        // Suspend the whole group, then confirm the kernel reports it stopped.
        assert_eq!(unsafe { libc::kill(-pid, libc::SIGSTOP) }, 0, "SIGSTOP: {}", std::io::Error::last_os_error());
        let mut status: libc::c_int = 0;
        assert_eq!(unsafe { libc::waitpid(pid, &mut status, libc::WUNTRACED) }, pid);
        assert!(unsafe { libc::WIFSTOPPED(status) }, "child should be stopped");

        // Build the matching Job and resume it.
        let (job, _kill_rx) = Job::background(1, "sleep 30".into());
        job.set_pgid(pid);
        job.stop();
        resume_job(&job);
        assert_eq!(job.status(), "running", "state should flip back to running");

        // The SIGCONT must reach the child: the kernel reports it continued.
        assert_eq!(
            unsafe { libc::waitpid(pid, &mut status, libc::WCONTINUED | libc::WUNTRACED) },
            pid
        );
        assert!(unsafe { libc::WIFCONTINUED(status) }, "child should have been continued by SIGCONT");

        // Cleanup: kill the group and reap.
        unsafe {
            libc::kill(-pid, libc::SIGKILL);
            libc::waitpid(pid, &mut status, 0);
        }
    }
}

#[cfg(test)]
mod fileops_tests {
    use super::*;
    use crate::session::{Mode, Session};

    fn yolo_session(cwd: &std::path::Path) -> Session {
        let mut s = Session::new().unwrap();
        s.mode = Mode::Yolo;
        s.cwd = cwd.to_path_buf();
        s
    }

    async fn run(session: &mut Session, name: &str, args: serde_json::Value) -> ToolResult {
        let call = ToolCall { id: "t".into(), name: name.into(), args };
        let mut confirm = |_: &str| Decision::AllowOnce;
        execute(&call, session, &mut confirm).await
    }

    fn tmp(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("aish_{tag}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn glob_segment_and_class() {
        let m = |p: &str, t: &str| {
            glob_segment_match(&p.chars().collect::<Vec<_>>(), &t.chars().collect::<Vec<_>>())
        };
        assert!(m("*.rs", "main.rs"));
        assert!(!m("*.rs", "main.toml"));
        assert!(m("foo?", "foob"));
        assert!(!m("foo?", "foobb"));
        assert!(m("[a-c]at", "bat"));
        assert!(!m("[a-c]at", "zat"));
        assert!(m("[!0-9]x", "ax"));
        assert!(!m("[!0-9]x", "5x"));
    }

    #[test]
    fn glob_path_doublestar() {
        fn split(s: &str) -> Vec<&str> { s.split('/').filter(|x| !x.is_empty()).collect() }
        assert!(glob_path_match(&split("src/**/*.rs"), &split("src/a/b/main.rs")));
        assert!(glob_path_match(&split("src/**/*.rs"), &split("src/main.rs")));
        assert!(!glob_path_match(&split("src/**/*.rs"), &split("tests/main.rs")));
        assert!(glob_path_match(&split("*.toml"), &split("Cargo.toml")));
        assert!(!glob_path_match(&split("*.toml"), &split("src/Cargo.toml")));
    }

    #[tokio::test]
    async fn glob_expand_finds_files() {
        let dir = tmp("glob");
        std::fs::create_dir_all(dir.join("src/inner")).unwrap();
        std::fs::write(dir.join("src/a.rs"), b"x").unwrap();
        std::fs::write(dir.join("src/inner/b.rs"), b"yy").unwrap();
        std::fs::write(dir.join("src/c.txt"), b"z").unwrap();
        let mut s = yolo_session(&dir);
        let r = run(&mut s, "glob_expand", json!({"pattern": "src/**/*.rs"})).await;
        assert!(!r.is_error, "{}", r.content);
        assert!(r.content.contains("src/a.rs"), "{}", r.content);
        assert!(r.content.contains("src/inner/b.rs"), "{}", r.content);
        assert!(!r.content.contains("c.txt"), "{}", r.content);
        // type filter
        let d = run(&mut s, "glob_expand", json!({"pattern": "src/**", "type": "dir"})).await;
        assert!(d.content.contains("src/inner"), "{}", d.content);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn grep_files_matches_and_case() {
        let dir = tmp("grep");
        std::fs::write(dir.join("a.txt"), b"hello world\nfoo bar\nHELLO again\n").unwrap();
        let mut s = yolo_session(&dir);
        let r = run(&mut s, "grep_files", json!({"pattern": "hello"})).await;
        assert!(r.content.contains("a.txt:1:"), "{}", r.content);
        assert!(!r.content.contains("a.txt:3:"), "{}", r.content);
        let r2 = run(&mut s, "grep_files", json!({"pattern": "hello", "ignore_case": true})).await;
        assert!(r2.content.contains("a.txt:3:"), "{}", r2.content);
        // glob scoping excludes non-matching files
        std::fs::write(dir.join("b.rs"), b"hello rust\n").unwrap();
        let r3 = run(&mut s, "grep_files", json!({"pattern": "hello", "glob": "*.rs"})).await;
        assert!(r3.content.contains("b.rs:1:"), "{}", r3.content);
        assert!(!r3.content.contains("a.txt"), "{}", r3.content);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn stat_file_reports_metadata() {
        let dir = tmp("stat");
        std::fs::write(dir.join("f"), b"12345").unwrap();
        let mut s = yolo_session(&dir);
        let r = run(&mut s, "stat_file", json!({"path": "f"})).await;
        assert!(r.content.contains("type: file"), "{}", r.content);
        assert!(r.content.contains("size: 5 bytes"), "{}", r.content);
        assert!(r.content.contains("perms:"), "{}", r.content);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn copy_rename_append_roundtrip() {
        let dir = tmp("crp");
        std::fs::write(dir.join("a"), b"orig").unwrap();
        let mut s = yolo_session(&dir);

        let r = run(&mut s, "copy_file", json!({"src": "a", "dst": "b"})).await;
        assert!(!r.is_error, "{}", r.content);
        assert_eq!(std::fs::read_to_string(dir.join("b")).unwrap(), "orig");

        // refuse overwrite without the flag
        let r = run(&mut s, "copy_file", json!({"src": "a", "dst": "b"})).await;
        assert!(r.content.contains("destination exists"), "{}", r.content);

        let r = run(&mut s, "append_file", json!({"path": "b", "content": "+more"})).await;
        assert!(!r.is_error, "{}", r.content);
        assert_eq!(std::fs::read_to_string(dir.join("b")).unwrap(), "orig+more");

        let r = run(&mut s, "rename_file", json!({"src": "b", "dst": "c"})).await;
        assert!(!r.is_error, "{}", r.content);
        assert!(!dir.join("b").exists());
        assert_eq!(std::fs::read_to_string(dir.join("c")).unwrap(), "orig+more");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn append_creates_missing_and_newline() {
        let dir = tmp("append");
        let mut s = yolo_session(&dir);
        let r = run(&mut s, "append_file", json!({"path": "log", "content": "line", "newline": true})).await;
        assert!(!r.is_error, "{}", r.content);
        assert_eq!(std::fs::read_to_string(dir.join("log")).unwrap(), "line\n");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn diff_files_unified() {
        let dir = tmp("diff");
        std::fs::write(dir.join("a"), b"one\ntwo\nthree\n").unwrap();
        std::fs::write(dir.join("b"), b"one\n2\nthree\n").unwrap();
        let mut s = yolo_session(&dir);
        let r = run(&mut s, "diff_files", json!({"a": "a", "b": "b"})).await;
        assert!(r.content.contains("@@"), "{}", r.content);
        assert!(r.content.contains("-two"), "{}", r.content);
        assert!(r.content.contains("+2"), "{}", r.content);
        let same = run(&mut s, "diff_files", json!({"a": "a", "b": "a"})).await;
        assert!(same.content.contains("identical"), "{}", same.content);
        // inline b_content
        let ic = run(&mut s, "diff_files", json!({"a": "a", "b_content": "one\ntwo\nthree\n"})).await;
        assert!(ic.content.contains("identical"), "{}", ic.content);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn new_tools_are_registered() {
        let defs = tool_defs(false, false);
        for t in [
            "glob_expand",
            "grep_files",
            "stat_file",
            "diff_files",
            "copy_file",
            "rename_file",
            "append_file",
        ] {
            assert!(defs.iter().any(|d| d.name == t), "missing tool def: {t}");
        }
    }
}
