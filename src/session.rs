use crate::backend::{Msg, ToolResult};
use anyhow::Result;
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

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
    /// Optional session name, shown as a `[NAME] |` prefix on the prompt. Set
    /// with `:rename <name>`, cleared with bare `:rename`. Session-local.
    pub name: Option<String>,
    /// Stable per-process id (a uuid). Batch jobs are tagged with it so results
    /// auto-deliver only to the session that spawned them; other sessions can
    /// still query any job. The friendly `name` (if set) is the display label.
    pub session_id: String,
    /// Static host info baked into the system prompt once.
    pub host_info: String,
    /// `export` lines from ~/.aishrc, applied to every program aish spawns.
    pub env: Vec<(String, String)>,
    /// Pre-rendered system-prompt section listing ~/.aish/skills (may be empty).
    pub skills_prompt: String,
    /// The parsed local skill catalog (`~/.aish/skills`), kept in step with
    /// `skills_prompt`. Drives the per-turn skill-awareness nudge
    /// (`crate::skill_match`): each turn the user's input is scored against these
    /// and, when one clearly fits, a short note pointing at its SKILL.md is
    /// folded into the turn input.
    pub skills: Vec<crate::skills::Skill>,
    /// Registry skill refs already recommended-for-install this session (the
    /// offline `crate::skill_match::recommend_install` path). Deduped so the same
    /// "you could install `<ref>`" nudge fires at most once per session.
    pub skill_suggested: HashSet<String>,
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
    /// Directory-scoped permission grants for THIS session only — the in-memory
    /// fallback that keeps the 'd' answer working when the persistent store is
    /// unavailable. Each entry is `(perm, dir)` where perm is read|write|delete
    /// and the grant covers `dir` recursively. Populated on every 'd' answer and
    /// consulted before the DB.
    pub session_dir_allows: HashSet<(String, PathBuf)>,
    /// Running estimate of how many tokens the current `history` occupies in the
    /// model's context window — updated after each turn from the backend's
    /// reported usage (or a char-based estimate). Drives auto-compaction and the
    /// `:context` readout. Starts at 0 (no turn taken yet). Session-local.
    pub context_used: usize,
    /// Interactive background mode (on by default, persisted; toggle with `:batch`).
    /// When on, the agent gets the run_in_background/background_status tools and a
    /// system-prompt nudge to offload deferrable work to a full background
    /// coordinator (which may itself fan sub-work out to the Anthropic Batches
    /// API). A persisted `:batch off` survives restarts.
    pub batch_mode: bool,
    /// Model every background batch runs on (batches are Anthropic-only). Always
    /// Opus by default — deferred work gets the strongest model regardless of the
    /// interactive backend. Settable via `:batch model`.
    pub batch_model: String,
    /// Live background batch jobs (in memory for the session, mirrored to
    /// `batch_store` for durability).
    pub batch_jobs: crate::batch::BatchJobs,
    /// Durable batch-job store (own SQLite connection). None if it failed to
    /// open — batches then fall back to session-only, lost on exit.
    pub batch_store: Option<crate::db::BatchStore>,
    /// Live full-tool background workers — aish subprocesses run in
    /// `--coordinator` mode. In memory for the session, like `batch_jobs`.
    pub worker_jobs: crate::worker::WorkerJobs,
    /// Durable coordinator-run store (own SQLite connection). Records the phase
    /// of each background coordinator run so a crash/exit resumes, and so
    /// `background_status` / startup rehydrate can see runs across restarts.
    /// None if it failed to open — coordinator runs then aren't persisted.
    pub coordinator_store: Option<crate::db::CoordinatorStore>,
    /// True when THIS aish is itself a background coordinator (env
    /// `AISH_COORDINATOR=1`). The nested guard: a coordinator must never spawn
    /// its own workers (no infinite re-exec recursion), so `run_in_background`
    /// downgrades a tool-needing offload to a tool-less batch when this is set.
    pub nested: bool,
    /// The active background `:goal` loop, if any (one per session). Set by
    /// `:goal <condition>`, inspected by bare `:goal`, stopped by `:goal clear`.
    pub goal: Option<crate::goal::Handle>,
    /// Which provider the interactive backend runs on (`"claude"`/`"grok"`/
    /// `"local"`). Set right after the backend is built and updated by `:backend`.
    /// Background coordinators are spawned on this same backend (full parity), so
    /// `:dispatch`/`run_in_background`/`:goal` thread it through to `WorkerSpec`.
    pub backend_kind: String,
    /// Gates whether a background coordinator's live activity is forwarded to
    /// the terminal. When false (the DEFAULT) a background worker is QUIET: none
    /// of its stderr is echoed — not its `🔧` tool-activity, not its turn/batch
    /// narration — only the prompt's `⟳N` pulse and the completion notice show it's
    /// alive. When true the full live stream is forwarded: `🔧` tool lines plus the
    /// coordinator's turn output (tagged standard vs batch). Toggled by
    /// `:worker-output`; session-local, never persisted. Shared into the detached
    /// stderr-streaming task (via `WorkerSpec`) and read per line, so toggling
    /// mid-run takes effect on later lines.
    pub show_worker_output: Arc<AtomicBool>,
    /// `(provider, model)` of the stronger model a weak frontend should escalate
    /// hard, in-turn reasoning to — recomputed each turn by the engine from
    /// `Backend::escalation_target`. `None` when the frontend is already frontier
    /// (an Opus/default-Grok session, or an offline local run). When `Some`, the
    /// `escalate` tool is offered and the capability nudge is added; the tool
    /// reads this to reconstruct the strong-model backend at call time.
    pub escalation: Option<(String, String)>,
    /// True when aish was launched as a login shell (`-l`/`--login`, or an
    /// argv[0] beginning with `-`). Login shells source the profile files and
    /// become a session leader; non-login shells skip that. Set in `main`.
    pub login: bool,
    /// Tier‑1 turn‑audit journal for a background coordinator run (see
    /// `crate::turn_audit`). `Some` only for a headless `--coordinator` run, where
    /// it is attached by `coordinator::drive` so `engine::run_turn` can journal
    /// each tool call (and replay completed turns on a resume). Always `None` for
    /// an interactive session — no journaling there.
    pub turn_audit: Option<crate::turn_audit::TurnAudit>,
    /// S9.3 per-worker conversation-store WRITER (see `crate::worker_store`).
    /// `Some` only for a headless `--coordinator` run, where `coordinator::drive`
    /// attaches it so `engine::run_turn` persists each turn-event (user message,
    /// tool call/result, narration) and `drive` each round’s synthesis to the
    /// per-worker `transcript.jsonl` for `:attach` replay / resume. Always `None`
    /// for an interactive session — no per-worker store there (mirrors
    /// `turn_audit`).
    pub worker_transcript: Option<crate::worker_store::TranscriptWriter>,
    /// Shared "attached coordinator" handle (`:attach`/`:detach`). Holds the
    /// run-id of the background coordinator this interactive session is currently
    /// attached to, or `None`. Cloned into every `WorkerSpec` so the live stderr
    /// stream forwards exactly the attached worker's activity even with
    /// `:worker-output` off, and read at the prompt to steer typed lines to it.
    /// Session-local, never persisted.
    pub attached: Arc<Mutex<Option<String>>>,
    /// Review-mode bookkeeping for an attached coordinator that has reached a
    /// terminal state. Holds the run-id whose "finished — review mode" notice has
    /// already been shown, so the live->terminal transition is announced exactly
    /// once and `:attach`-ing an already-finished worker does not double-announce.
    /// Cleared when the attachment goes live again (e.g. on resume). Session-local.
    pub attach_review_announced: Arc<Mutex<Option<String>>>,
    /// When true, a headless `--coordinator` run prints its final result as a
    /// machine-readable JSON object (`{"ok":true,"output":"…"}`) instead of
    /// rendered markdown — set from `--output json` in `main`. Mirrors the
    /// one-shot `-c` JSON path so an agent driving a background coordinator can
    /// parse the result. Always false for an interactive session.
    pub output_json: bool,
    /// Lifecycle-hook registry (see `crate::hooks`), merged from
    /// `~/.aish/hooks.json` and the project-local `.aish/hooks.json`. Defaults to
    /// EMPTY — the zero-overhead state every call site checks first
    /// (`hooks.has(event)`), so an unconfigured session spawns no hook process
    /// and builds no payload. Loaded explicitly at startup via `load_hooks`.
    pub hooks: crate::hooks::HookSet,
}

impl Session {
    pub fn new() -> Result<Self> {
        let cwd = std::env::current_dir()?;
        Ok(Self {
            cwd,
            history: Vec::new(),
            mode: Mode::default(),
            name: None,
            session_id: uuid::Uuid::new_v4().to_string(),
            host_info: host_info(),
            env: Vec::new(),
            skills_prompt: String::new(),
            skills: Vec::new(),
            skill_suggested: HashSet::new(),
            mcp: crate::mcp::McpHost::default(),
            db: None,
            raw_tool_output: false,
            last_turn_tools: Vec::new(),
            jobs: Default::default(),
            last_status: 0,
            session_allows: HashSet::new(),
            session_dir_allows: HashSet::new(),
            context_used: 0,
            batch_mode: true,
            batch_model: crate::batch::DEFAULT_BATCH_MODEL.to_string(),
            batch_jobs: Default::default(),
            batch_store: None,
            worker_jobs: Default::default(),
            coordinator_store: None,
            nested: std::env::var("AISH_COORDINATOR").is_ok(),
            goal: None,
            backend_kind: "claude".to_string(),
            show_worker_output: Arc::new(AtomicBool::new(false)),
            escalation: None,
            turn_audit: None,
            worker_transcript: None,
            login: false,
            attached: Arc::new(Mutex::new(None)),
            attach_review_announced: Arc::new(Mutex::new(None)),
            output_json: false,
            hooks: crate::hooks::HookSet::empty(),
        })
    }

    /// Load the lifecycle-hook registry from the user-global
    /// (`~/.aish/hooks.json`) and project-local (`<cwd>/.aish/hooks.json`)
    /// config, replacing whatever was set. Called once at startup (after the cwd
    /// is established). A missing/empty config leaves the zero-overhead empty set
    /// in place. `:hooks reload` re-invokes this to pick up edits mid-session.
    pub fn load_hooks(&mut self) {
        let home = std::env::var_os("HOME").map(PathBuf::from);
        self.hooks = crate::hooks::HookSet::load(home.as_deref(), &self.cwd);
    }

    /// The autonomy descriptor stamped on every hook payload (design §3.1): a
    /// background coordinator (`nested`) is `coordinator`, everything else is
    /// `interactive`. Lets a hook scope itself to human vs. autonomous turns.
    pub fn agent_kind(&self) -> crate::hooks::Agent {
        if self.nested {
            crate::hooks::Agent::Coordinator
        } else {
            crate::hooks::Agent::Interactive
        }
    }

    /// Build a hook payload pre-filled with this session's common envelope
    /// (session id, agent kind, cwd, mode). Call sites add event-specific fields
    /// with `.with(...)`. Only constructed inside a `hooks.has(event)` guard, so
    /// the empty fast path never reaches here.
    pub fn hook_payload(&self, event: crate::hooks::HookEvent) -> crate::hooks::HookPayload {
        crate::hooks::HookPayload::new(
            event,
            &self.session_id,
            self.agent_kind(),
            &self.cwd,
            self.mode.name(),
        )
    }

    /// Set a session environment variable (last-wins), replacing any existing
    /// entries with the same key so every spawned child sees exactly one value.
    /// This is how the in-process env-mutating builtins (`set`, `unset`, and `cd`
    /// updating `$PWD`/`$OLDPWD`) keep the per-spawn env coherent.
    pub fn set_var(&mut self, key: &str, value: impl Into<String>) {
        let value = value.into();
        self.env.retain(|(k, _)| k != key);
        self.env.push((key.to_string(), value));
    }

    /// Remove every entry for `key` from the session env. Returns true when
    /// something was removed. Backs the `unset` builtin: a subprocess can’t
    /// change its parent, so unset must drop the var here so later spawns no
    /// longer carry it.
    pub fn unset_var(&mut self, key: &str) -> bool {
        let before = self.env.len();
        self.env.retain(|(k, _)| k != key);
        self.env.len() != before
    }

    /// Re-scan the on-disk skills directory and rebuild `skills_prompt` so a
    /// skill added or removed mid-session (via the `:skill` commands) is
    /// advertised to the model from the very next turn — no restart. The MCP
    /// half is read fresh from the live `McpHost` and narrowed to the interactive
    /// routing subset (`interactive_mcp_skills`), so reloading the local catalog
    /// neither drops the MCP routing skills nor re-exposes the heavy code-work
    /// skills the interactive agent must not see. Resolves the directory exactly
    /// like startup does (`~/.aish/skills`); the work is in `reload_skills_from`.
    pub fn reload_skills(&mut self) -> Result<()> {
        self.reload_skills_from(&default_skills_dir());
        Ok(())
    }

    /// `reload_skills` against an explicit directory — the testable core. Loads
    /// the local catalog from `skills_dir` and re-renders `skills_prompt`,
    /// keeping the interactive routing subset of the live MCP skills alongside it.
    pub fn reload_skills_from(&mut self, skills_dir: &std::path::Path) {
        let local = crate::skills::load(skills_dir);
        // `reload_skills` is invoked ONLY from the interactive `:skill add` /
        // `:skill remove` path — never by a background coordinator (which builds
        // its skills_prompt once, from the full catalog, in `main`). So a reload
        // must re-apply the SAME light-touch filter the initial interactive
        // render does (`interactive_mcp_skills`, see repl::run): advertise only
        // the routing skills and keep the heavy code-work / agent-dispatch skills
        // for coordinators. Without this, a mid-session `:skill` reload would
        // silently re-expose the full MCP catalog to the interactive agent.
        let mcp = crate::skills::interactive_mcp_skills(&self.mcp.skills());
        self.skills_prompt = crate::skills::render_prompt_section(&local, &mcp);
        // Keep the parsed catalog in step so the per-turn skill-awareness nudge
        // (crate::skill_match) sees a skill added/removed mid-session immediately.
        self.skills = local;
    }

    /// Record the exit status of the command just dispatched, so the next line
    /// can expand `$?`. Signal termination maps to 128 + signal, as POSIX shells
    /// report it.
    pub fn set_last_status(&mut self, status: &std::process::ExitStatus) {
        use std::os::unix::process::ExitStatusExt;
        self.last_status = status
            .code()
            .unwrap_or_else(|| 128 + status.signal().unwrap_or(0));
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
            || self
                .db
                .as_ref()
                .is_some_and(|db| db.is_allowed(key).unwrap_or(false))
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

    /// True when `path` falls under a directory granted for `perm` (a prior 'd'
    /// answer). Checks the in-memory session grants first, then the persistent
    /// store. Best-effort: no store → only the session set applies.
    pub fn is_path_allowed(&self, perm: &str, path: &std::path::Path) -> bool {
        if self
            .session_dir_allows
            .iter()
            .any(|(p, dir)| p == perm && path.starts_with(dir))
        {
            return true;
        }
        self.db.as_ref().is_some_and(|db| {
            db.is_dir_allowed(perm, &path.to_string_lossy())
                .unwrap_or(false)
        })
    }

    /// Grant `perm` recursively for everything under `dir` — the 'd' answer.
    /// Always recorded in the session set so it holds for the rest of this
    /// session; also persisted when a store is available.
    pub fn allow_path_dir(&mut self, perm: &str, dir: &std::path::Path) {
        self.session_dir_allows
            .insert((perm.to_string(), dir.to_path_buf()));
        match &self.db {
            Some(db) => {
                let _ = db.allow_dir(perm, &dir.to_string_lossy());
            }
            None => eprintln!("note: directory allow won't persist — database unavailable"),
        }
    }

    pub fn system_prompt(&self, escalate_available: bool) -> String {
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
Repository Navigation:\n\
- When analyzing a repo, check for `.repospec.json` FIRST — it's the agent-optimized navigation \
spec. Read it before README, git log, or source files. It contains description, summary, module map, \
entrypoints, and patterns.\n\
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
- You can drive a fresh, non-interactive aish as a subprocess to hand off a self-contained \
agentic sub-task: run_program with program `aish` and args \
`[\"-c\", \"<prompt>\", \"--output\", \"json\"]`. The child runs the prompt to completion and prints a \
SINGLE machine-readable line on stdout — `{{\"ok\":true,\"output\":\"…\"}}` on success, \
`{{\"ok\":false,\"error\":\"…\"}}` on failure — so you parse the answer instead of scraping rendered \
markdown. Always pass `--output json` (not a bare `aish -c`) when you need to read the result back \
programmatically.\n\
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
- When asked to \"resolve CI failures\"/\"fix CI\" or \"resolve conflicts\"/\"fix merge \
conflicts\", do NOT hand-fix it yourself. First reach for the matching installed skill (a \
fix-ci skill for CI failures, a fix-conflicts skill for merge conflicts): read its SKILL.md \
and follow it. If no installed skill matches, recommend one via `:skill add <ref>` before \
proceeding. Then escalate the actual fix to an agent rather than resolving it by hand.\n\
- Git hygiene — NEVER commit or push directly to the default branch (main/master). To make a \
change: create a feature branch (git checkout -b …), commit THERE, push that branch, then open a \
pull request (gh pr create). Do NOT `git push` the default branch yourself, and do NOT merge into \
it locally. A PR is the only way work reaches the default branch — and once work is on the default \
branch it is DONE: never open a PR for commits that already exist there (no pushing main AND \
PR-ing the same commits). If you discover local commits sitting on main (or any unexpected state), \
STOP and report it to the user instead of pushing — surfacing it is the fix, not force-syncing.\n\
- You have persistent memory across sessions: `remember` stores a durable fact, `recall` \
searches by keyword. recall when prior preferences or decisions might matter; remember \
preferences, project facts, and lessons worth keeping.\n\
- When a reply lists more than one item (files, processes, packages, search hits, results), \
prefer a markdown table over prose: a header row plus one row per item, with the columns that \
matter — aish renders these as aligned terminal tables. Be verbose with columns rather than \
terse, and order the rows deliberately: chronological for events or history, by stage for \
pipelines or build/run phases, by category for mixed or grouped sets.\n\
- Final replies are terse and shell-like. One line when one line will do, but reach for a table \
the moment there are several items to compare. No markdown headers.{skills}{batch}{escalate}",
            host = self.host_info,
            cwd = self.cwd.display(),
            skills = self.skills_prompt,
            batch = if self.batch_mode { BATCH_NUDGE } else { "" },
            escalate = if escalate_available {
                ESCALATE_NUDGE
            } else {
                ""
            },
        )
    }
}

/// The default on-disk skills directory: `~/.aish/skills`. Mirrors main.rs's
/// `aish_dir().join("skills")` so the interactive `:skill` reload and the
/// startup catalog scan agree on where local skills live.
fn default_skills_dir() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_default())
        .join(".aish")
        .join("skills")
}

/// Appended to the system prompt when batch mode is on (ported from atum's
/// BATCH_MODE_NUDGE): biases the agent toward offloading deferrable work.
const BATCH_NUDGE: &str = "\n\nBackground mode is ON. You have run_in_background(task) — it offloads a \
self-contained, deferrable task to a full background COORDINATOR: a headless aish running in the same \
directory with your COMPLETE toolset and MCP servers (read/write files, run programs, atum/github, …), \
which can itself fan heavy parallel sub-work out to the Anthropic Batches API. There is no separate \
\"batch\" mode to choose and no batch_result() to call — you just describe the task and offload it; the \
result auto-delivers here when it's done. It returns a job id immediately and survives restarts. PREFER \
run_in_background for deferrable, parallelizable, or non-urgent work — it keeps the conversation moving. \
Only answer inline when the user needs the result right now — and ALWAYS answer a QUESTION inline \
rather than offloading it. Offloading is for NEW work the user is asking you to DO; a question — \
including \"didn't we already dispatch a worker for this?\", \"what is it doing?\", or any ask about \
the status or history of existing work — is something you ANSWER, not a task to spawn a coordinator \
for. Read the request first: if it is a question, reach for background_status (or the relevant \
lookup) and reply; dispatching a fresh coordinator to answer whether a coordinator was already \
dispatched is exactly the wrong move. To answer \"what's running?\" call \
background_status (never invent your own tracking). Steer a coordinator that is ALREADY running without restarting it: call the `tell` tool with its run id (from background_status) and a message — a clarification, a course-correction, a narrower scope — and it is folded into that coordinator's next round; this is how you and your background coordinators message each other mid-flight. When you offload, call run_in_background with NO \
preamble, then reply with ONE short, natural sentence — tailored to what they asked — saying you're \
handling it in the background and the answer will appear here when it's ready (e.g. \"On it — I'll work \
that out in the background and post the answer here.\"). Do NOT predict or mention the job id, restate \
the task, or explain cost/timing; the result auto-delivers.";

/// Appended when the frontend is a smaller/faster model than the strongest one
/// available (haiku/sonnet, or a local model with a Claude credential). It tells
/// that weak frontend to lean on the two workers — `escalate` for hard reasoning
/// it must finish THIS turn, `run_in_background` for deferrable work — instead of
/// guessing past a step it can't reason through. Omitted entirely when the
/// frontend is already frontier (Opus), so a strong model is never told to
/// second-guess itself.
const ESCALATE_NUDGE: &str = "\n\nYou are running on a smaller, faster model, and a STRONGER model is \
one call away. Play to that: do the routing, dispatch, file edits, and straightforward steps yourself \
— you are good at those and fast — but the moment a step needs deeper reasoning than you can do \
reliably (a confusing error to diagnose, a multi-step plan, an ambiguous or risky judgment, careful \
analysis), do NOT guess. If you need the answer to keep going THIS turn, call escalate(task) — a \
synchronous consult that returns the stronger model's reasoning in a few seconds; put everything it \
needs in `task` (it sees nothing else), then act on its answer with your tools. If the result can wait, \
use run_in_background instead. Escalating a hard step is the correct, expected move here, not a \
failure — a wrong guess you act on is far more costly than a few seconds of consulting.";

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

/// True when aish was invoked as a login shell: either the explicit `-l` /
/// `--login` flag, or the classic convention of an argv[0] beginning with `-`
/// (e.g. `-aish`, as `login`(1)/`getty` exec a login shell). Pure for testing.
pub fn is_login_invocation(login_flag: bool, argv0: &str) -> bool {
    login_flag || argv0.starts_with('-')
}

fn host_info() -> String {
    let os = std::fs::read_to_string("/etc/os-release")
        .ok()
        .and_then(|s| {
            s.lines().find(|l| l.starts_with("PRETTY_NAME=")).map(|l| {
                l.trim_start_matches("PRETTY_NAME=")
                    .trim_matches('"')
                    .to_string()
            })
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
        assert!(
            out.ends_with("…[truncated]"),
            "missing marker: {}",
            &out[out.len() - 20..]
        );
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
    fn path_allow_dir_covers_subtree_per_perm() {
        let mut session = Session::new().unwrap();
        // No store → only the session set applies.
        let toml = std::path::Path::new("/tmp/proj/Cargo.toml");
        assert!(!session.is_path_allowed("read", toml));
        session.allow_path_dir("read", std::path::Path::new("/tmp/proj"));
        // The granted dir and everything under it is allowed for that perm.
        assert!(session.is_path_allowed("read", toml));
        assert!(session.is_path_allowed("read", std::path::Path::new("/tmp/proj/src/main.rs")));
        // Different perm, or a sibling dir, is not.
        assert!(!session.is_path_allowed("write", toml));
        assert!(!session.is_path_allowed("read", std::path::Path::new("/tmp/proj2/x")));
    }

    #[test]
    fn login_invocation_detection() {
        // Explicit flag wins regardless of argv0.
        assert!(is_login_invocation(true, "aish"));
        assert!(is_login_invocation(true, "/usr/local/bin/aish"));
        // Classic dash-argv0 convention (login(1)/getty).
        assert!(is_login_invocation(false, "-aish"));
        assert!(is_login_invocation(false, "-bash"));
        // Ordinary interactive/non-login invocations.
        assert!(!is_login_invocation(false, "aish"));
        assert!(!is_login_invocation(false, "/usr/local/bin/aish"));
        assert!(!is_login_invocation(false, ""));
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

    #[test]
    fn system_prompt_carries_ci_conflict_escalation_rule() {
        // The "resolve CI failures / resolve conflicts → skill + escalate"
        // behaviour is a baked-in prompt rule (not a per-session memory), so it
        // must always be present regardless of escalate availability.
        let session = Session::new().unwrap();
        for escalate in [false, true] {
            let p = session.system_prompt(escalate);
            assert!(p.contains("resolve CI failures"), "missing CI/conflict rule");
            assert!(p.contains("fix-conflicts skill"), "missing fix-conflicts ref");
            assert!(
                p.contains("escalate the actual fix to an agent"),
                "missing escalate-to-agent directive"
            );
        }
    }

    // ---- Phase 2: reload_skills -----------------------------------------

    /// Write a minimal valid SKILL.md under `dir/<name>/SKILL.md`.
    fn write_skill(dir: &std::path::Path, name: &str, desc: &str) {
        let d = dir.join(name);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(
            d.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: {desc}\n---\nbody\n"),
        )
        .unwrap();
    }

    #[test]
    fn reload_picks_up_new_skill() {
        let mut session = Session::new().unwrap();
        let tmp = std::env::temp_dir().join(format!("aish-reload-add-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        // Empty dir → empty section.
        session.reload_skills_from(&tmp);
        assert_eq!(session.skills_prompt, "");
        // Add a skill on disk → reload advertises it.
        write_skill(&tmp, "demo", "Demo skill.");
        session.reload_skills_from(&tmp);
        assert!(
            session.skills_prompt.contains("demo"),
            "{}",
            session.skills_prompt
        );
        assert!(session.skills_prompt.contains("Demo skill."));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn reload_drops_removed_skill() {
        let mut session = Session::new().unwrap();
        let tmp = std::env::temp_dir().join(format!("aish-reload-drop-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        write_skill(&tmp, "gone", "To be removed.");
        session.reload_skills_from(&tmp);
        assert!(session.skills_prompt.contains("gone"));
        // Remove the skill dir → reload no longer advertises it.
        std::fs::remove_dir_all(tmp.join("gone")).unwrap();
        session.reload_skills_from(&tmp);
        assert!(
            !session.skills_prompt.contains("gone"),
            "{}",
            session.skills_prompt
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn reload_matches_fresh_init() {
        // A reload produces exactly the prompt section a fresh load+render would
        // (with no MCP servers connected, the default McpHost contributes none).
        let tmp = std::env::temp_dir().join(format!("aish-reload-fresh-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        write_skill(&tmp, "alpha", "First.");
        write_skill(&tmp, "beta", "Second.");
        let fresh = crate::skills::render_prompt_section(&crate::skills::load(&tmp), &[]);
        let mut session = Session::new().unwrap();
        session.reload_skills_from(&tmp);
        assert_eq!(session.skills_prompt, fresh);
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
