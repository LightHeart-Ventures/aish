use crate::backend::{Msg, ToolResult};
use anyhow::Result;
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

/// How much the safety gate asks before acting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Mode {
    /// Confirm every tool call — reads, writes, executions, everything.
    Paranoid,
    /// Confirm anything not provably read-only (allowlist); reads run free.
    Careful,
    /// Confirm only destructive actions (write/create/delete); reads and
    /// unrecognized commands run free.
    #[default]
    Normal,
    /// Confirm nothing.
    Yolo,
}

impl Mode {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "paranoid" => Some(Self::Paranoid),
            "careful" => Some(Self::Careful),
            "normal" => Some(Self::Normal),
            "yolo" => Some(Self::Yolo),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Paranoid => "paranoid",
            Self::Careful => "careful",
            Self::Normal => "normal",
            Self::Yolo => "yolo",
        }
    }
}

/// Engine-side state, independent of any frontend (REPL today, login shell later).
pub struct Session {
    /// The shell's working directory. Lives HERE, never in the process global —
    /// applied per-exec via Command::current_dir(). `cd` is a tool that mutates this.
    pub cwd: PathBuf,
    /// Normalized conversation history (backend-agnostic).
    pub history: Vec<Msg>,
    /// How much the safety gate asks before acting (paranoid → yolo).
    pub mode: Mode,
    /// Static host info baked into the system prompt once.
    pub host_info: String,
    /// True while a child owns the terminal (run_interactive / direct dispatch).
    /// The REPL's Ctrl-C handling consults this: a SIGINT then belongs to that
    /// foreground child, not to aish's turn-abort.
    pub tty_handoff: Arc<AtomicBool>,
    /// `export` lines from ~/.aishrc, applied to every program aish spawns.
    pub env: Vec<(String, String)>,
    /// Pre-rendered system-prompt section listing ~/.aish/skills (may be empty).
    pub skills_prompt: String,
    /// Connected MCP servers; their tools join the model's tool set.
    pub mcp: crate::mcp::McpHost,
    /// Persistent store (history + agent memories). None if it failed to open.
    pub db: Option<crate::db::Db>,
    /// When true, each tool call's raw result is echoed dim under its 🔧 line.
    /// Toggled by Ctrl-O at the prompt; session-local, never persisted.
    pub raw_tool_output: bool,
    /// Tool calls + results of the most recent turn, kept for the retroactive
    /// reveal when raw output is switched on after a surprising answer.
    pub last_turn_tools: Vec<(String, ToolResult)>,
    /// Background jobs (run_program background:true). Output streams to the
    /// terminal live; the model reads it via job_output. Die with the shell.
    pub jobs: crate::tools::Jobs,
    /// Exit status of the most recently dispatched command (direct, pipeline, or
    /// model-run), expanded as `$?` on the next dispatch line. Starts at 0, as in
    /// any POSIX shell.
    pub last_status: i32,
    /// Tools always-allowed for THIS session only — the in-memory fallback that
    /// keeps `a` working even when the persistent store is unavailable (AC3).
    /// Populated on every 'a' answer and consulted before the DB.
    pub session_allows: HashSet<String>,
}

impl Session {
    pub fn new() -> Result<Self> {
        let cwd = std::env::current_dir()?;
        Ok(Self {
            cwd,
            history: Vec::new(),
            mode: Mode::default(),
            host_info: host_info(),
            tty_handoff: Arc::new(AtomicBool::new(false)),
            env: Vec::new(),
            skills_prompt: String::new(),
            mcp: crate::mcp::McpHost::default(),
            db: None,
            raw_tool_output: false,
            last_turn_tools: Vec::new(),
            jobs: Default::default(),
            last_status: 0,
            session_allows: HashSet::new(),
        })
    }

    /// Record the exit status of the command just dispatched, so the next line
    /// can expand `$?`. Signal termination maps to 128 + signal, as POSIX shells
    /// report it.
    pub fn set_last_status(&mut self, status: &std::process::ExitStatus) {
        use std::os::unix::process::ExitStatusExt;
        self.last_status = status.code().unwrap_or_else(|| 128 + status.signal().unwrap_or(0));
    }

    /// The most recent recorded output (a model/agent reply), truncated per the
    /// last-output policy. Backs TASK-13 last-output addressing: the `$LAST`/`$_`
    /// dispatch binding and the automatic model-prompt context. `None` when no
    /// output has been recorded yet or the persistent store is unavailable.
    pub fn last_output(&self) -> Option<String> {
        let raw = self.db.as_ref()?.last_output().ok()??;
        Some(truncate_last(raw))
    }

    /// True when `key` is always-allowed (a prior 'a' answer). Checks the
    /// in-memory session set first, then the persistent store. Best-effort: no
    /// store → only the session set applies.
    pub fn is_tool_allowed(&self, key: &str) -> bool {
        self.session_allows.contains(key)
            || self.db.as_ref().is_some_and(|db| db.is_allowed(key).unwrap_or(false))
    }

    /// Always-allow `key`. Always recorded in the session set so it holds for
    /// the rest of this session; also persisted when a store is available. When
    /// none is, the allow degrades to session-only and the user is warned.
    pub fn allow_tool(&mut self, key: &str) {
        self.session_allows.insert(key.to_string());
        match &self.db {
            Some(db) => {
                let _ = db.allow(key);
            }
            None => eprintln!("note: allow-list won't persist — database unavailable"),
        }
    }

    pub fn system_prompt(&self) -> String {
        // NOTE: deliberately static after session start (starting dir, not live cwd)
        // so the prompt-cache prefix never changes. The model learns cwd changes
        // from change_dir tool results.
        format!(
            "You are aish, an AI-native shell. You ARE the user's shell on this Linux machine — \
there is no bash or sh underneath; you act directly through tools.\n\
\n\
{host}\n\
Starting directory: {cwd}\n\
\n\
Rules:\n\
- Act, don't lecture. Use tools to do what the user asks, then answer in as few words as the task allows.\n\
- There is NO shell: run_program executes one binary with an argv array. Pipes, globs, redirection, \
`&&`, and quoting do not exist. Expand wildcards with list_dir, chain steps with multiple tool calls, \
and filter or aggregate output yourself.\n\
- Use change_dir to move around; it changes the shell's working directory for all later calls.\n\
- For screen-oriented or interactive programs (top, htop, vim, less, ssh, REPLs) use \
run_interactive: it attaches the program to the user's terminal and the user drives it — you \
only learn the exit status. Use run_program whenever you need the output yourself. NEVER use \
run_interactive for watchers or monitors — that freezes the user's prompt.\n\
- Your turn ENDS when you reply. Nothing of yours keeps running between turns except background \
jobs, and you never receive pushed events, MCP notifications, or job output — aish prints those \
on the user's terminal as they arrive, and you read them on a LATER turn via job_output. Never \
claim to be 'listening' or 'waiting' for anything after your reply.\n\
- Long-running programs (watchers, event listeners, tails, servers): run_program with \
background:true. It returns a job id immediately; output streams live to the user and \
accumulates for job_output {{job}}. The user manages jobs with :jobs and :kill. Foreground \
run_program is killed at timeout_secs.\n\
- run_program and run_interactive accept env (extra environment variables). For secrets, pass a \
reference — \"${{profile:KEY}}\" resolves from ~/.atum/credentials [profile] at spawn time, \
\"${{NAME}}\" from session exports/environment — so the value never enters the conversation. \
NEVER read credential files with read_file; reference them.\n\
- Prefer read_file/write_file/list_dir over cat/echo tricks.\n\
- When a command fails, read the error and try one sensible fix before reporting back.\n\
- You have persistent memory across sessions: `remember` stores a durable fact, `recall` \
searches by keyword. recall when prior preferences or decisions might matter; remember \
preferences, project facts, and lessons worth keeping.\n\
- When a reply lists more than one item (files, processes, packages, search hits, results), \
prefer a markdown table over prose: a header row plus one row per item, with the columns that \
matter — aish renders these as aligned terminal tables. Be verbose with columns rather than \
terse, and order the rows deliberately: chronological for events or history, by stage for \
pipelines or build/run phases, by category for mixed or grouped sets.\n\
- Final replies are terse and shell-like. One line when one line will do, but reach for a table \
the moment there are several items to compare. No markdown headers.{skills}",
            host = self.host_info,
            cwd = self.cwd.display(),
            skills = self.skills_prompt,
        )
    }
}

/// Hard cap (bytes) on last-output text exposed via `$LAST`/`$_` and the
/// automatic model-prompt context (TASK-13 AC3). Outputs longer than this are
/// truncated head-first with an ellipsis marker — the head is what a follow-up
/// line most often references, and a bounded value keeps argv and prompt sizes
/// sane.
const LAST_OUTPUT_LIMIT: usize = 4000;

/// Truncation policy for last-output addressing: keep the leading
/// `LAST_OUTPUT_LIMIT` bytes (snapped to a char boundary) and append an ellipsis
/// marker when anything was dropped. Short outputs pass through unchanged.
fn truncate_last(mut s: String) -> String {
    if s.len() <= LAST_OUTPUT_LIMIT {
        return s;
    }
    let mut end = LAST_OUTPUT_LIMIT;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s.truncate(end);
    s.push_str("\n…[truncated]");
    s
}

fn host_info() -> String {
    let os = std::fs::read_to_string("/etc/os-release")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("PRETTY_NAME="))
                .map(|l| l.trim_start_matches("PRETTY_NAME=").trim_matches('"').to_string())
        })
        .unwrap_or_else(|| "Linux".into());
    let user = std::env::var("USER").unwrap_or_else(|_| "unknown".into());
    let home = std::env::var("HOME").unwrap_or_default();
    format!("Host: {os}\nUser: {user} (home: {home})")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_last_passes_short_output_through() {
        assert_eq!(truncate_last("short".into()), "short");
        let exact = "x".repeat(LAST_OUTPUT_LIMIT);
        assert_eq!(truncate_last(exact.clone()), exact);
    }

    #[test]
    fn truncate_last_caps_large_output_with_marker() {
        let big = "y".repeat(LAST_OUTPUT_LIMIT * 2);
        let out = truncate_last(big);
        assert!(out.ends_with("…[truncated]"), "missing marker: {}", &out[out.len() - 20..]);
        // head kept up to the limit; marker is the only addition
        assert!(out.starts_with(&"y".repeat(LAST_OUTPUT_LIMIT)));
        assert_eq!(out.len(), LAST_OUTPUT_LIMIT + "\n…[truncated]".len());
    }

    #[test]
    fn truncate_last_snaps_to_char_boundary() {
        // A multibyte char straddling the limit must not panic or split a code point.
        let mut s = "a".repeat(LAST_OUTPUT_LIMIT - 1);
        s.push('é'); // 2 bytes, crosses the LAST_OUTPUT_LIMIT boundary
        s.push_str(&"b".repeat(100));
        let out = truncate_last(s);
        assert!(out.ends_with("…[truncated]"));
        assert!(out.is_char_boundary(out.len() - "\n…[truncated]".len()));
    }

    #[test]
    fn session_last_output_reads_and_truncates() {
        let mut session = Session::new().unwrap();
        assert_eq!(session.last_output(), None); // no store
        let path = std::env::temp_dir().join(format!("aish_sess_last_{}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let db = crate::db::Db::open(&path).unwrap();
        db.record("output", "/tmp", &"z".repeat(LAST_OUTPUT_LIMIT * 2));
        session.db = Some(db);
        let out = session.last_output().unwrap();
        assert!(out.ends_with("…[truncated]"));
        let _ = std::fs::remove_file(&path);
    }
}
