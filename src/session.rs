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

/// Largest iteration count `:loop` will accept — a guard so a fat-fingered
/// `:loop 100000 …` can't pin the session in a runaway agentic loop.
pub const MAX_LOOP: usize = 100;

/// State of an active `:loop`. `:loop <count> <prompt>` re-runs <prompt> as a
/// foreground model turn `count` times in sequence — each iteration is a normal
/// agentic turn, so conversation context accumulates across them, mirroring an
/// autonomous "keep iterating on this" loop (à la Claude CLI). The REPL drains
/// one iteration per pass via [`Session::next_loop_tick`]; a Ctrl-C on any turn
/// clears it. Session-local, never persisted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoopState {
    /// The prompt re-submitted each iteration.
    pub prompt: String,
    /// Total iterations requested (drives the `i/N` header).
    pub total: usize,
    /// Iterations already dispatched.
    pub done: usize,
}

/// What the REPL should do for one pass of the loop, returned by
/// [`Session::next_loop_tick`]. Keeps all the counter bookkeeping inside
/// `Session` (unit-tested) while leaving the IO to the REPL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoopTick {
    /// Dispatch iteration `index` of `total`: run `body` as a model turn.
    Run {
        index: usize,
        total: usize,
        body: String,
    },
    /// The loop just finished its last iteration (announce once, then idle).
    Done { total: usize },
    /// No loop is active — read a line from the user as usual.
    Idle,
}

/// Env knob gating the auto-resume-on-child-completion wake hook
/// (`AISH_AUTO_RESUME_ON_CHILD_COMPLETE`). Default **ON**; disabled only when the
/// var is set to a falsey token (`0`, `false`, `no`, `off`, case-insensitive).
/// Read live (not cached) so it can be flipped per invocation. See
/// [`ResumeState`].
pub fn auto_resume_enabled() -> bool {
    resume_enabled_from(std::env::var("AISH_AUTO_RESUME_ON_CHILD_COMPLETE").ok().as_deref())
}

/// Pure core of [`auto_resume_enabled`] — testable without mutating process env.
fn resume_enabled_from(raw: Option<&str>) -> bool {
    match raw {
        None => true,
        Some(v) => !matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "0" | "false" | "no" | "off"
        ),
    }
}

/// Build the synthetic continuation turn the parent runs when its fanned-out
/// background coordinators finish (see [`ResumeState`]). Phrased as an
/// instruction to READ each completed worker's result and synthesize it for the
/// user — explicitly NOT to re-dispatch already-done work (the failure mode a
/// naive "continue" nudge invites). The REPL prefixes the returned string with
/// `?` so it routes straight to the model turn, never shell-dispatched or
/// re-offloaded. Pure, so the exact wording is unit-tested.
fn resume_prompt(ids: &[String]) -> String {
    let list = ids.join(", ");
    let n = ids.len();
    let plural = if n == 1 { "" } else { "s" };
    let first = ids.first().map(String::as_str).unwrap_or("");
    format!(
        "[Auto-resume] {n} background worker{plural} you dispatched finished while you were idle: \
{list}. Their results are ready. Retrieve each one now (call background_status, then job_output \
for each id — e.g. job_output {{\"job\":\"{first}\"}}), read the output, and synthesize the \
outcome for the user. Do NOT re-dispatch work that is already complete."
    )
}

/// Coalescing state for the auto-resume-on-child-completion wake hook. When a
/// top-level interactive session fans out background coordinators via
/// `run_in_background`, their results "auto-deliver" only as one-line notices —
/// nothing re-invokes the assistant to READ and synthesize them, so the human
/// had to type "continue". This tracks which fanned-out children have gone
/// terminal and, once the LAST outstanding child finishes, arms a single
/// coalesced continuation turn the REPL consumes on its next idle pass (see
/// [`Session::take_resume_tick`]) — the parent then reads the results itself.
///
/// Shared (`Arc<Mutex<_>>`) with the background-result presenter task, which
/// `observe`s completions off the main thread; the main loop drains the armed
/// resume with `take`. Session-local, never persisted. Deliberately coalescing:
/// if N children finish close together the parent wakes ONCE with all ready ids,
/// not N times.
#[derive(Default)]
pub struct ResumeState {
    /// Terminal child ids observed but not yet consumed by a resume turn.
    pending: Vec<String>,
    /// Child ids already folded into `pending` — dedupes repeated observations
    /// across the presenter's polling ticks so one completion arms exactly once,
    /// and so a later re-observation after a drain never re-fires the same id.
    seen: HashSet<String>,
    /// Set once the LAST outstanding fanned-out child has gone terminal with
    /// something pending; cleared when the REPL consumes the resume via `take`.
    armed: bool,
}

impl ResumeState {
    /// Fold newly-terminal `terminal_ids` into the pending set and, when the last
    /// outstanding fanned-out child has finished (`outstanding == 0`) with
    /// something pending, arm a coalesced resume. Returns true only on the call
    /// that FRESHLY arms it, so the caller can announce the wake exactly once.
    /// Idempotent: re-observing the same ids, or re-calling while already armed,
    /// does not re-arm. A no-op (returns false, records nothing) when auto-resume
    /// is disabled, so a disabled session never accrues state.
    pub fn observe(&mut self, terminal_ids: &[String], outstanding: usize) -> bool {
        if !auto_resume_enabled() {
            return false;
        }
        for id in terminal_ids {
            if self.seen.insert(id.clone()) {
                self.pending.push(id.clone());
            }
        }
        if outstanding == 0 && !self.pending.is_empty() && !self.armed {
            self.armed = true;
            return true;
        }
        false
    }

    /// Drain an armed resume: returns the ids to synthesize and clears both the
    /// arm and the pending list (the `seen` set is RETAINED so an already-consumed
    /// completion can never re-fire). `None` when nothing is armed.
    pub fn take(&mut self) -> Option<Vec<String>> {
        if !self.armed {
            return None;
        }
        self.armed = false;
        Some(std::mem::take(&mut self.pending))
    }

    /// Test-only peek at the pending count.
    #[cfg(test)]
    fn pending_len(&self) -> usize {
        self.pending.len()
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
    /// Cumulative prompt (input) tokens the model has been billed this session,
    /// summed from each turn's reported [`crate::context::Usage`]. Drives the
    /// interactive activity-stream status line (`tokens in: …`). Session-local.
    pub tokens_in: usize,
    /// Cumulative completion (output) tokens the model has produced this session
    /// (companion to [`tokens_in`]). Feeds the status line's `tokens out: …`.
    pub tokens_out: usize,
    /// Count of tool calls executed this session — every `escalate`, dispatch
    /// (`run_in_background`), and ordinary tool run tallied once when it finishes.
    /// Feeds the status line's `tool calls: …`.
    pub tool_calls_total: usize,
    /// Count of logical agentic turns taken this session (one per `run_turn`).
    /// Feeds the status line's `turns: …`.
    pub turns_total: usize,
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
    /// When on, force deferrable work to run as Anthropic Batches even when a
    /// background coordinator might otherwise execute it inline. Toggled with
    /// `:batch force-batches`; resets per session (not persisted).
    pub batch_force_batches: bool,
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
    /// Suppress the next turn's last-output context seed (TASK-13). `:new` clears
    /// `history` to start a fresh conversation, but an empty history is exactly
    /// the condition `seed_context` uses to re-inject the previous recorded
    /// output (`$LAST`) as `[Previous command output, for reference: …]`. Without
    /// this flag the old conversation's final reply would bleed straight back
    /// into the supposedly-clean turn. `:new` sets it; the engine consumes it on
    /// the next turn (one-shot), so a later command's output can still seed a
    /// genuinely fresh prompt. Session-local, never persisted.
    pub suppress_context_seed: bool,
    /// Set by `:restart` (and the post-`:update` auto-restart) to ask the REPL
    /// loop to re-exec the aish binary with the SAME argv it was launched with,
    /// instead of just exiting. Consumed by `repl::run` after the normal exit
    /// cleanup: when true it replaces the process image (Unix `exec`) rather than
    /// printing "bye" and returning. Session-local, never persisted.
    pub restart_requested: bool,
    /// The verbatim assignment a background coordinator was launched with, PINNED
    /// into the system prompt so it survives every history compaction. A
    /// long-running worker compacts its oldest turns to free context (see
    /// `crate::context`), and the original task message is the FIRST thing
    /// dropped — after which the worker only sees a "[Context compacted: …]"
    /// banner and can lose track of what it was doing (the banner even recurses
    /// into itself across repeated compactions). The system prompt is rebuilt
    /// every turn and is NEVER compacted, so anchoring the task here keeps it in
    /// front of the model for the whole run regardless of how much history is
    /// offloaded. Set by `coordinator::drive` at startup; always `None` for an
    /// interactive session (whose conversation has no single fixed task).
    pub task_anchor: Option<String>,
    /// The active `:loop`, if any (one per session). Set by `:loop <count>
    /// <prompt>`, drained one iteration per REPL pass by `next_loop_tick`,
    /// stopped by `:loop stop` or a Ctrl-C abort. Always `None` for a background
    /// coordinator — loops are an interactive affordance.
    pub loop_state: Option<LoopState>,
    /// Auto-resume-on-child-completion state (the parent-wake hook). When a
    /// top-level interactive session fans out background coordinators via
    /// `run_in_background`, this coalesces their completions so that once the
    /// LAST outstanding fanned-out child goes terminal the REPL synthesizes a
    /// single continuation turn — the parent reads + synthesizes the results
    /// itself, instead of the human having to type "continue". Shared
    /// (`Arc<Mutex<_>>`) with the background-result presenter task, which
    /// observes completions off the main thread; the main loop drains the armed
    /// resume on its next idle pass (`take_resume_tick`). Session-local, never
    /// persisted.
    pub resume: Arc<Mutex<ResumeState>>,
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
            tokens_in: 0,
            tokens_out: 0,
            tool_calls_total: 0,
            turns_total: 0,
            batch_mode: true,
            batch_model: crate::batch::DEFAULT_BATCH_MODEL.to_string(),
            batch_force_batches: false,
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
            suppress_context_seed: false,
            restart_requested: false,
            task_anchor: None,
            loop_state: None,
            resume: Arc::new(Mutex::new(ResumeState::default())),
        })
    }

    /// Begin a `:loop`: re-run `prompt` as a foreground model turn `count`
    /// times. Replaces any prior loop. `count` is clamped to `1..=MAX_LOOP`.
    pub fn start_loop(&mut self, count: usize, prompt: impl Into<String>) {
        let total = count.clamp(1, MAX_LOOP);
        self.loop_state = Some(LoopState {
            prompt: prompt.into(),
            total,
            done: 0,
        });
    }

    /// Advance the active loop by one pass. Returns [`LoopTick::Run`] for each
    /// iteration to dispatch, [`LoopTick::Done`] exactly once when the last
    /// iteration has been dispatched (clearing the loop), and [`LoopTick::Idle`]
    /// when no loop is active. The REPL prints the header / completion notice and
    /// feeds the `body` to the model — `Session` owns only the counter.
    pub fn next_loop_tick(&mut self) -> LoopTick {
        let Some(st) = self.loop_state.as_mut() else {
            return LoopTick::Idle;
        };
        if st.done >= st.total {
            let total = st.total;
            self.loop_state = None;
            return LoopTick::Done { total };
        }
        st.done += 1;
        LoopTick::Run {
            index: st.done,
            total: st.total,
            body: st.prompt.clone(),
        }
    }

    /// Clear any active loop (Ctrl-C abort, or `:loop stop`). Returns true when
    /// one was active.
    pub fn clear_loop(&mut self) -> bool {
        self.loop_state.take().is_some()
    }

    /// One-line status of the active loop (bare `:loop` / `:loop status`).
    pub fn loop_status(&self) -> String {
        match &self.loop_state {
            Some(st) => format!(
                "loop active — {}/{} iterations dispatched · prompt: {}",
                st.done, st.total, st.prompt
            ),
            None => {
                "no loop running — `:loop <count> <prompt>` to start one".to_string()
            }
        }
    }

    /// Consume an armed auto-resume (see [`ResumeState`]). Returns the synthetic
    /// continuation prompt to run as a model turn when the last outstanding
    /// fanned-out child of this session has completed, or `None`. Two gates keep
    /// it from firing surprisingly: it is skipped while `:attach`ed to a
    /// coordinator (typed lines steer that worker — an auto-synthesis would fight
    /// the attach), and when auto-resume is disabled via the env knob. The REPL
    /// runs the returned prompt through its `?`-forced model route, exactly like
    /// a `:loop` tick, so the turn is driven from the loop rather than blocked on
    /// a poll — consistent with "the assistant's turn ends at reply".
    pub fn take_resume_tick(&mut self) -> Option<String> {
        if !auto_resume_enabled() {
            return None;
        }
        // Attached to a coordinator → typed input is steered to it; don't also
        // auto-synthesize here.
        if self
            .attached
            .lock()
            .ok()
            .and_then(|g| g.clone())
            .is_some()
        {
            return None;
        }
        let ids = self.resume.lock().ok()?.take()?;
        if ids.is_empty() {
            return None;
        }
        Some(resume_prompt(&ids))
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
        let local = crate::skills::load_catalog(skills_dir);
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
            "You are aish, the AI-native shell built by LightHeart Ventures. You ARE the user's \
terminal on this Linux machine. No bash, no sh, no POSIX cruft underneath — you reason, tool-call, \
fork/exec real binaries, observe, iterate, and deliver. Stay in flow. Act like Gregory: ruthless \
efficiency, builder mindset, zero lectures, maximum signal.\n\
\n\
{host}\n\
Starting cwd: {cwd}\n\
\n\
Core Rules (NEVER break these):\n\
- Intent in, results out. Parse natural language and execute via tools. Chain steps aggressively.\n\
- NO shell syntax: no pipes, globs, redirection, &&/||, command substitution. Use list_dir + \
run_program chains. Filter/aggregate yourself.\n\
- change_dir updates session state for everything after.\n\
- Interactive stuff (vim, top, ssh): ALWAYS use run_interactive for TTY handoff. run_program for \
the output you need. Never run_interactive for watchers/monitors — it freezes the prompt.\n\
- Background/long-running (watchers, tails, servers): run_program with background:true → job id; \
output streams to the user and you read it later via job_output. User handles :jobs/:kill.\n\
- Your turn ENDS when you reply — nothing of yours runs between turns except background jobs, and \
you never receive pushed events; never claim to be 'listening' or 'waiting' after replying.\n\
- Secrets: ONLY credential refs like ${{profile:KEY}} or ${{ENV}}. Never read_file creds.\n\
- Failures: read the error, try ONE smart fix, then report. Don't loop forever.\n\
- Git: feature branches only. No direct pushes/merges to main/master. PRs or die. Find local \
commits already on main? STOP and report it — don't force-sync.\n\
- CI/Conflicts: prioritize the installed SKILL.md (fix-ci, fix-conflicts). Recommend `:skill add` \
if missing. Escalate fix to stronger agent — don't hand-fix.\n\
- NEVER FABRICATE, ALWAYS VERIFY: report ONLY what actually happened. If you narrate an action \
('watching…', 'running…') you MUST attach the actual tool call in that SAME turn — a bare narration \
runs nothing. Confirm every reported outcome with a real read (gh run view, a status query, a file \
read); if you couldn't verify, say so plainly instead of inventing a result.\n\
- Memory: remember() durable facts (projects, preferences, lessons); recall() proactively on context.\n\
- Output: terse, shell-like. Use markdown tables for ANY list >1 item (columns that matter, sorted \
deliberately). Flag costs/optimizations.\n\
\n\
Advanced Directives:\n\
- Repo mode: `.repospec.json` FIRST, then code.\n\
- Background mode: aggressively offload deferrable work via run_in_background. Inline only for \
urgent questions.\n\
- Weaker model? Escalate hard reasoning immediately.\n\
- Skills/MCP: use them ruthlessly — they're first-class.\n\
- Loops: detect repeats, summarize partials, force converge. Never spin.\n\
- You: East-Texas-optimized builder agent. Bias toward observability, AWS/Terraform, cost wins, \
longevity hacks, micro-SaaS velocity.\n\
\n\
Final reply style: one line when possible. Table when useful. End turn cleanly. No \"I'm thinking\" \
fluff.{skills}{batch}{escalate}{console}{task}",
            host = self.host_info,
            cwd = self.cwd.display(),
            skills = self.skills_prompt,
            batch = if self.batch_mode { BATCH_NUDGE } else { "" },
            escalate = if escalate_available {
                ESCALATE_NUDGE
            } else {
                ""
            },
            console = if self.nested { CONSOLE_NUDGE } else { "" },
            task = self
                .task_anchor
                .as_deref()
                .map(task_anchor_block)
                .unwrap_or_default(),
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

/// Render the PINNED-task block appended to a background coordinator's system
/// prompt (see [`Session::task_anchor`]). Lives in the system prompt — never
/// compacted — so the worker's original assignment stays in front of the model
/// for the entire run, even after its earliest turns (including the task message
/// itself) have been offloaded by [`crate::context`]. The wording tells the
/// model this is the authoritative copy to fall back on whenever a compaction
/// banner has displaced the conversational history.
fn task_anchor_block(task: &str) -> String {
    format!(
        "\n\nYOUR ASSIGNED TASK (verbatim, pinned — this is your single source of truth). You are a \
background coordinator working to complete exactly this; it is reproduced here because your \
conversation history is periodically compacted to free context, and when that happens the original \
task message is dropped and replaced by a \"[Context compacted: …]\" banner. Whenever you are unsure \
what you are doing, re-read THIS block — not the banner — and keep going until it is done:\n\n{task}",
        task = task.trim()
    )
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

/// Appended to a background coordinator's system prompt (`nested`). Advertises
/// the one-way `message_console` channel: a coordinator can post a short note
/// straight to the human's interactive console at any point — shown immediately,
/// bypassing the quiet `:worker-output` gate — without waiting to deliver its
/// final result. Interactive sessions never see this (there is no parent console
/// to message).
const CONSOLE_NUDGE: &str = "\n\nYou are a background coordinator, and your activity is QUIET by \
default — the human who launched you does not see your tool calls or narration unless they turn on \
`:worker-output`. When something genuinely warrants the operator's attention BEFORE you finish — a \
surfaced finding, a heads-up, a non-blocking question, meaningful progress on a long job, or a \
reason you may take a while — call message_console(message) to post a short note straight to their \
interactive console. It is ALWAYS shown the moment you send it, framed as coming from you. It is \
one-way (you cannot read a reply) and is NOT a substitute for your final result — it's an \
out-of-band note, so use it sparingly and keep it to a line or two.";

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
            assert!(p.contains("CI/Conflicts"), "missing CI/conflict rule");
            assert!(
                p.contains("fix-ci, fix-conflicts"),
                "missing fix-ci/fix-conflicts skill refs"
            );
            assert!(
                p.contains("Escalate fix to stronger agent"),
                "missing escalate-to-agent directive"
            );
        }
    }

    #[test]
    fn system_prompt_carries_never_fabricate_rule() {
        // The "never fabricate, always verify" behaviour is a baked-in prompt
        // rule — present regardless of escalate availability.
        let session = Session::new().unwrap();
        for escalate in [false, true] {
            let p = session.system_prompt(escalate);
            assert!(
                p.contains("NEVER FABRICATE, ALWAYS VERIFY"),
                "missing anti-fabrication rule"
            );
            assert!(
                p.contains("attach the actual tool call"),
                "missing attach-tool-call directive"
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
    fn resume_enabled_defaults_on_and_parses_falsey_marker() {
        assert!(resume_enabled_from(None));
        for v in ["0", "false", "no", "off", "  OFF ", "False"] {
            assert!(!resume_enabled_from(Some(v)), "{v:?} should disable");
        }
        for v in ["1", "true", "yes", "on", ""] {
            assert!(resume_enabled_from(Some(v)), "{v:?} should enable");
        }
    }

    #[test]
    fn resume_prompt_names_ids_and_forbids_redispatch() {
        let p = resume_prompt(&["w_a".into(), "w_b".into()]);
        assert!(p.contains("w_a") && p.contains("w_b"), "{p}");
        assert!(p.contains("2 background workers"), "{p}");
        assert!(p.contains("job_output"), "{p}");
        assert!(p.contains("Do NOT re-dispatch"), "{p}");
        let one = resume_prompt(&["w_a".into()]);
        assert!(one.contains("1 background worker "), "{one}");
    }

    #[test]
    fn resume_state_arms_only_when_last_child_done() {
        if !auto_resume_enabled() {
            return;
        }
        let mut r = ResumeState::default();
        assert!(!r.observe(&["w_a".into()], 1));
        assert_eq!(r.pending_len(), 1);
        assert!(r.take().is_none());
        assert!(r.observe(&["w_b".into()], 0));
        assert_eq!(r.pending_len(), 2);
        assert!(!r.observe(&["w_b".into()], 0));
        let ids = r.take().unwrap();
        assert_eq!(ids, vec!["w_a".to_string(), "w_b".to_string()]);
        assert!(r.take().is_none());
    }

    #[test]
    fn resume_state_dedupes_and_never_refires_consumed_ids() {
        if !auto_resume_enabled() {
            return;
        }
        let mut r = ResumeState::default();
        assert!(r.observe(&["w_a".into(), "w_a".into()], 0));
        assert_eq!(r.pending_len(), 1);
        assert_eq!(r.take().unwrap(), vec!["w_a".to_string()]);
        assert!(!r.observe(&["w_a".into()], 0));
        assert!(r.take().is_none());
        assert!(r.observe(&["w_c".into()], 0));
        assert_eq!(r.take().unwrap(), vec!["w_c".to_string()]);
    }

    #[test]
    fn resume_state_no_arm_without_pending() {
        if !auto_resume_enabled() {
            return;
        }
        let mut r = ResumeState::default();
        assert!(!r.observe(&[], 0));
        assert!(r.take().is_none());
    }

    #[test]
    fn take_resume_tick_gated_by_attach() {
        if !auto_resume_enabled() {
            return;
        }
        let mut session = Session::new().unwrap();
        assert!(session.resume.lock().unwrap().observe(&["w_a".into()], 0));
        *session.attached.lock().unwrap() = Some("w_x".to_string());
        assert!(session.take_resume_tick().is_none());
        *session.attached.lock().unwrap() = None;
        let body = session.take_resume_tick().expect("armed resume");
        assert!(body.contains("w_a"), "{body}");
        assert!(session.take_resume_tick().is_none());
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
