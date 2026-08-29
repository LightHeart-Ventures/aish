//! Lifecycle hook system — Phase 0 core + observe-only dispatch.
//!
//! A hook lets a user or operator run their own program at a well-defined
//! moment in aish's lifecycle (session start/end, every tool call, file writes,
//! …) without patching the binary. This module is the foundation specified by
//! `docs/aish-hooks-design.md`:
//!
//!   * the [`HookEvent`] catalog (every lifecycle boundary),
//!   * the [`HookSet`] registry, merged from `~/.aish/hooks.json` and the
//!     project-local `.aish/hooks.json`,
//!   * the [`Matcher`] (tool/program/path/mode/agent filters),
//!   * the [`HookPayload`] JSON envelope handed to each hook on stdin, and
//!   * the async, best-effort, timeout-bounded **observe** dispatcher.
//!
//! ## Zero overhead when unused (design §1.4)
//! Every call site is guarded by [`HookSet::has`], an `O(n)` scan of a usually
//! empty `Vec`. When no hook is registered for an event aish spawns no process,
//! serializes no JSON, and allocates no payload — the only cost is one cheap
//! membership test. The payload is built inside the `has`-guarded block, so the
//! closure-free fast path never touches it.
//!
//! ## Trust model (design §6)
//! Hooks are local programs that run with the user's own UID/GID, spawned
//! fork/exec (no shell — there is none under aish). The payload carries **no
//! credential values** (export keys only, never `${profile:…}` values). A
//! recursion guard (`AISH_IN_HOOK`) stops a hook from re-triggering the same
//! lifecycle and looping. Dispatch is best-effort with a hard per-hook timeout;
//! observe hooks can never change a turn's outcome (the blocking/mutating
//! variants are deferred to Phases 2–3).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde::Deserialize;
use serde_json::{Map, Value};

/// Default per-hook timeout when the config omits one (design §6.3).
const DEFAULT_TIMEOUT_MS: u64 = 5_000;

/// Env var set on a spawned hook process. Its presence anywhere up the process
/// tree means "we are already inside a hook" — every dispatch path checks it and
/// refuses to fire, so a `FileChanged` hook that itself writes a file cannot
/// re-trigger `FileChanged` and spin (design §6.3).
const RECURSION_GUARD: &str = "AISH_IN_HOOK";

/// Every lifecycle boundary a hook can fire at. The wire name (in `hooks.json`'s
/// `event` field and in the payload envelope) is the PascalCase variant name.
///
/// Phase 0/1 wires the **observe-only** subset; the sync/blocking
/// (`PreToolUse` veto, `PermissionRequest`, `PreCompact`, `ModeChanged`) and
/// mutating (`UserPromptSubmit` prepend, `CwdChanged` env) semantics arrive in
/// later phases. The catalog is complete here so config written against the
/// design validates today.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HookEvent {
    // Session & lifecycle
    SessionStart,
    SessionEnd,
    InstructionsLoaded,
    McpServersReady,
    // Interactive
    ModeChanged,
    BackendChanged,
    PromptRouteDecided,
    DirectCommandRun,
    CwdChanged,
    // Turn & tool lifecycle
    UserPromptSubmit,
    PreToolUse,
    PostToolUse,
    PostToolUseFailure,
    PermissionRequest,
    PermissionDenied,
    TurnEnd,
    TurnEndFailure,
    // Files & persistence
    FileChanged,
    MemoryStored,
    PreCompact,
    PostCompact,
    // Background jobs & coordination
    WorkerStart,
    WorkerStop,
    CoordinatorPhaseChanged,
    OperatorMessageReceived,
    LoopGuardTripped,
    EscalationRequested,
    BatchFanOut,
    BackgroundJobStart,
    BackgroundJobStop,
    // Goal loop (background stopping-oracle pursuit)
    GoalStart,
    GoalTurnEnd,
    GoalEnd,
    // System
    UpdateAvailable,
    UpdateApplied,
    SkillMatched,
}

impl HookEvent {
    /// The stable wire name (PascalCase) used in config and payloads.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SessionStart => "SessionStart",
            Self::SessionEnd => "SessionEnd",
            Self::InstructionsLoaded => "InstructionsLoaded",
            Self::McpServersReady => "McpServersReady",
            Self::ModeChanged => "ModeChanged",
            Self::BackendChanged => "BackendChanged",
            Self::PromptRouteDecided => "PromptRouteDecided",
            Self::DirectCommandRun => "DirectCommandRun",
            Self::CwdChanged => "CwdChanged",
            Self::UserPromptSubmit => "UserPromptSubmit",
            Self::PreToolUse => "PreToolUse",
            Self::PostToolUse => "PostToolUse",
            Self::PostToolUseFailure => "PostToolUseFailure",
            Self::PermissionRequest => "PermissionRequest",
            Self::PermissionDenied => "PermissionDenied",
            Self::TurnEnd => "TurnEnd",
            Self::TurnEndFailure => "TurnEndFailure",
            Self::FileChanged => "FileChanged",
            Self::MemoryStored => "MemoryStored",
            Self::PreCompact => "PreCompact",
            Self::PostCompact => "PostCompact",
            Self::WorkerStart => "WorkerStart",
            Self::WorkerStop => "WorkerStop",
            Self::CoordinatorPhaseChanged => "CoordinatorPhaseChanged",
            Self::OperatorMessageReceived => "OperatorMessageReceived",
            Self::LoopGuardTripped => "LoopGuardTripped",
            Self::EscalationRequested => "EscalationRequested",
            Self::BatchFanOut => "BatchFanOut",
            Self::BackgroundJobStart => "BackgroundJobStart",
            Self::BackgroundJobStop => "BackgroundJobStop",
            Self::GoalStart => "GoalStart",
            Self::GoalTurnEnd => "GoalTurnEnd",
            Self::GoalEnd => "GoalEnd",
            Self::UpdateAvailable => "UpdateAvailable",
            Self::UpdateApplied => "UpdateApplied",
            Self::SkillMatched => "SkillMatched",
        }
    }

    /// Parse a wire name back to an event. Case-sensitive (PascalCase), so a
    /// typo in config is rejected at load instead of silently never firing.
    pub fn parse(s: &str) -> Option<Self> {
        // Every variant round-trips through `as_str`; enumerate them once.
        const ALL: [HookEvent; 36] = [
            HookEvent::SessionStart,
            HookEvent::SessionEnd,
            HookEvent::InstructionsLoaded,
            HookEvent::McpServersReady,
            HookEvent::ModeChanged,
            HookEvent::BackendChanged,
            HookEvent::PromptRouteDecided,
            HookEvent::DirectCommandRun,
            HookEvent::CwdChanged,
            HookEvent::UserPromptSubmit,
            HookEvent::PreToolUse,
            HookEvent::PostToolUse,
            HookEvent::PostToolUseFailure,
            HookEvent::PermissionRequest,
            HookEvent::PermissionDenied,
            HookEvent::TurnEnd,
            HookEvent::TurnEndFailure,
            HookEvent::FileChanged,
            HookEvent::MemoryStored,
            HookEvent::PreCompact,
            HookEvent::PostCompact,
            HookEvent::WorkerStart,
            HookEvent::WorkerStop,
            HookEvent::CoordinatorPhaseChanged,
            HookEvent::OperatorMessageReceived,
            HookEvent::LoopGuardTripped,
            HookEvent::EscalationRequested,
            HookEvent::BatchFanOut,
            HookEvent::BackgroundJobStart,
            HookEvent::BackgroundJobStop,
            HookEvent::GoalStart,
            HookEvent::GoalTurnEnd,
            HookEvent::GoalEnd,
            HookEvent::UpdateAvailable,
            HookEvent::UpdateApplied,
            HookEvent::SkillMatched,
        ];
        ALL.into_iter().find(|e| e.as_str() == s)
    }

    /// True for the sync/blocking-class events (the `evaluate` gate can veto a
    /// turn on these). Used by the plugin merge (Phase 0.5.2) to enforce
    /// "one blocking winner per event": a plugin-contributed entry on a blocking
    /// event degrades to observe when a higher-precedence entry already owns the
    /// veto. Mirrors the sync subset called out in this module's header.
    pub fn is_blocking(self) -> bool {
        matches!(
            self,
            Self::PreToolUse | Self::PermissionRequest | Self::PreCompact | Self::ModeChanged
        )
    }
}

/// Provenance of a registered hook (Phase 0.5.2). `Local` covers the user
/// (`~/.aish/hooks.json`) and project (`.aish/hooks.json`) catalogs; `Plugin`
/// carries the contributing plugin's id for `:hooks list` provenance and for
/// the blocking-precedence rule (local outranks plugin).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum HookSource {
    #[default]
    Local,
    Plugin(String),
}

impl HookSource {
    /// The stable label shown in `:hooks list` / `:plugin info` — `"local"` or
    /// `"plugin:<id>"`.
    pub fn label(&self) -> String {
        match self {
            HookSource::Local => "local".to_string(),
            HookSource::Plugin(id) => format!("plugin:{id}"),
        }
    }

    /// True when contributed by a plugin.
    pub fn is_plugin(&self) -> bool {
        matches!(self, HookSource::Plugin(_))
    }
}

/// A plugin's event-hook fragment to merge into the catalog (Phase 0.5.2): the
/// contributing plugin id (for `source` tagging) and the path to its
/// `hooks.json`.
#[derive(Debug, Clone)]
pub struct PluginHookFragment {
    pub plugin_id: String,
    pub path: PathBuf,
}

/// The autonomy descriptor carried on every payload so a consumer can tell a
/// human turn from an autonomous one (design §3.1). Hooks that are noisy in
/// autonomous mode scope themselves with `matcher.agent == "interactive"`.
/// `Goal` is stamped on the background goal-loop's lifecycle events
/// (`GoalStart`/`GoalTurnEnd`/`GoalEnd`); `Script`/`Oneshot` remain reserved for
/// the finer agent-context split (design §3.1) later wiring phases stamp.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub enum Agent {
    Interactive,
    Coordinator,
    Goal,
    Script,
    Oneshot,
}

impl Agent {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Interactive => "interactive",
            Self::Coordinator => "coordinator",
            Self::Goal => "goal",
            Self::Script => "script",
            Self::Oneshot => "oneshot",
        }
    }
}

/// Optional filters narrowing when a hook fires (design §5 schema). Every field
/// is optional; an absent field matches anything. Present fields AND together.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Matcher {
    /// Glob on the tool name (`run_program`, `read_file`, `*`).
    #[serde(default)]
    pub tool: Option<String>,
    /// Glob on the spawned program (only meaningful for `run_program`).
    #[serde(default)]
    pub program: Option<String>,
    /// Glob on a path argument (e.g. `*.lock`, `**/secret*`).
    #[serde(default)]
    pub path_glob: Option<String>,
    /// Mode list (OR) — fire only in these confirmation modes.
    #[serde(default)]
    pub mode: Vec<String>,
    /// Agent descriptor filter (`interactive`/`coordinator`/…).
    #[serde(default)]
    pub agent: Option<String>,
}

impl Matcher {
    /// True when this matcher accepts the payload. A field is checked only when
    /// present; the relevant value is read from the payload envelope. A missing
    /// payload field with a present matcher field does NOT match (a `program`
    /// filter on a non-`run_program` event correctly excludes it).
    pub fn matches(&self, payload: &HookPayload) -> bool {
        if let Some(pat) = &self.tool {
            match payload.field("tool") {
                Some(v) if glob_match(pat, v) => {}
                _ => return false,
            }
        }
        if let Some(pat) = &self.program {
            match payload.field("program") {
                Some(v) if glob_match(pat, v) => {}
                _ => return false,
            }
        }
        if let Some(pat) = &self.path_glob {
            match payload.field("path") {
                Some(v) if glob_match(pat, v) => {}
                _ => return false,
            }
        }
        if !self.mode.is_empty() {
            match payload.field("mode") {
                Some(v) if self.mode.iter().any(|m| m == v) => {}
                _ => return false,
            }
        }
        if let Some(a) = &self.agent {
            match payload.field("agent") {
                Some(v) if v == a => {}
                _ => return false,
            }
        }
        true
    }
}

/// What a matched hook does. Phase 0/1 implements `command` (fork/exec a
/// program with the payload on stdin). The inline `rule` form is parsed but
/// reserved for Phase 2's blocking gate, so config written against it loads
/// without error today. `required` (fail-closed) and `deny_if` are likewise
/// parsed now and consumed by the Phase 2 blocking dispatch.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[allow(dead_code)] // `required` / `deny_if` are read by the Phase 2 blocking gate
pub enum Action {
    /// Run a program fork/exec with the JSON payload on stdin.
    Command {
        /// Absolute path or PATH-resolved binary name.
        program: String,
        /// Extra argv (the payload always arrives on stdin, never as an arg —
        /// it can be large and may carry odd bytes).
        #[serde(default)]
        args: Vec<String>,
        /// Hard timeout; the process is killed past it.
        #[serde(default)]
        timeout_ms: Option<u64>,
        /// Fail-closed on timeout/spawn error (only meaningful for the blocking
        /// phases; observe dispatch records it but never blocks).
        #[serde(default)]
        required: bool,
    },
    /// Inline policy rule — reserved for Phase 2's zero-spawn blocking gate.
    Rule {
        #[serde(default)]
        deny_if: Option<String>,
    },
}

impl Action {
    fn timeout(&self) -> Duration {
        match self {
            Action::Command { timeout_ms, .. } => {
                Duration::from_millis(timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS))
            }
            Action::Rule { .. } => Duration::from_millis(DEFAULT_TIMEOUT_MS),
        }
    }
}

/// One registered hook: the event it listens for, an optional matcher, and the
/// action to take. `matched` counts dispatches for `:hooks list`.
#[derive(Debug, Deserialize)]
pub struct Hook {
    #[serde(deserialize_with = "de_event")]
    pub event: HookEvent,
    #[serde(default)]
    pub matcher: Matcher,
    pub action: Action,
    /// Optional stable name (Phase 0.5.2). A higher-precedence catalog can
    /// **override** a lower one by re-declaring the same `name`, or **disable**
    /// it with a same-named tombstone (`"enabled": false`). Unnamed hooks can
    /// never be overridden or disabled (there's no handle to reference them).
    #[serde(default)]
    pub name: Option<String>,
    /// Tri-state enable flag (Phase 0.5.2). `None`/`Some(true)` register the
    /// hook; `Some(false)` makes it a **tombstone** — it registers nothing but
    /// suppresses any lower-precedence hook sharing its `name`.
    #[serde(default)]
    pub enabled: Option<bool>,
    /// Provenance (Phase 0.5.2). Set during the layered merge, never from JSON.
    #[serde(skip)]
    pub source: HookSource,
    /// Set during the merge when a plugin blocking entry loses the
    /// one-winner-per-event contest and is demoted to observe (Phase 0.5.2):
    /// the `evaluate` gate still runs it for side effects but ignores its veto.
    #[serde(skip)]
    pub observe_only: bool,
    /// Lifetime dispatch counter (observability; not serialized).
    #[serde(skip)]
    pub matched: AtomicU64,
}

fn de_event<'de, D>(d: D) -> Result<HookEvent, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let s = String::deserialize(d)?;
    HookEvent::parse(&s).ok_or_else(|| serde::de::Error::custom(format!("unknown hook event: {s}")))
}

/// The on-disk file shape: `{ "hooks": [ … ] }`.
#[derive(Debug, Deserialize, Default)]
struct HookFile {
    #[serde(default)]
    hooks: Vec<Hook>,
}

/// The merged, in-memory hook registry. Cheap to clone (an `Arc`), so it can be
/// handed to a detached dispatch task without copying the hook list.
#[derive(Clone, Default)]
pub struct HookSet {
    hooks: Arc<Vec<Hook>>,
}

impl HookSet {
    /// An empty registry — the universal zero-overhead default.
    pub fn empty() -> Self {
        Self {
            hooks: Arc::new(Vec::new()),
        }
    }

    /// True when no hook is registered. The fast-path guard at every call site.
    pub fn is_empty(&self) -> bool {
        self.hooks.is_empty()
    }

    /// Total registered hooks (across all events). Used by the `:hooks list`
    /// management surface (forthcoming wiring phase).
    #[allow(dead_code)]
    pub fn len(&self) -> usize {
        self.hooks.len()
    }

    /// True when at least one hook listens for `event`. The cheap membership
    /// test a call site runs BEFORE building any payload, so an unconfigured
    /// event allocates nothing.
    pub fn has(&self, event: HookEvent) -> bool {
        !self.is_empty() && self.hooks.iter().any(|h| h.event == event)
    }

    /// Read-only view of the registered hooks (for `:hooks list`).
    pub fn hooks(&self) -> &[Hook] {
        &self.hooks
    }

    /// Render the `:hooks list` table — one row per registered hook with its
    /// event, optional name, dispatch class, provenance, and lifetime match
    /// count. Pure (no I/O), so the REPL command and the unit tests share the
    /// exact same code path. Rows are sorted by (event, source, name) for a
    /// deterministic listing. `Status` is `observe` when a plugin blocking hook
    /// lost the one-winner-per-event contest and was demoted (Phase 0.5.2),
    /// otherwise `enabled`.
    pub fn format_list(&self) -> String {
        if self.hooks.is_empty() {
            return "no hooks registered".to_string();
        }
        let headers = ["Event", "Name", "Status", "Source", "Matched"];
        let mut rows: Vec<[String; 5]> = self
            .hooks
            .iter()
            .map(|h| {
                [
                    h.event.as_str().to_string(),
                    h.name.clone().unwrap_or_else(|| "-".to_string()),
                    if h.observe_only { "observe" } else { "enabled" }.to_string(),
                    h.source.label(),
                    h.matched.load(Ordering::Relaxed).to_string(),
                ]
            })
            .collect();
        rows.sort_by(|a, b| {
            a[0].cmp(&b[0]).then(a[3].cmp(&b[3])).then(a[1].cmp(&b[1]))
        });
        let mut widths = headers.map(str::len);
        for row in &rows {
            for (i, cell) in row.iter().enumerate() {
                widths[i] = widths[i].max(cell.len());
            }
        }
        let render = |cells: &[String; 5]| -> String {
            cells
                .iter()
                .enumerate()
                .map(|(i, c)| format!("{c:<width$}", width = widths[i]))
                .collect::<Vec<_>>()
                .join("  ")
                .trim_end()
                .to_string()
        };
        let header_row = headers.map(str::to_string);
        let mut out = vec![render(&header_row)];
        for row in &rows {
            out.push(render(row));
        }
        out.join("\n")
    }

    /// Load + merge the user-global (`~/.aish/hooks.json`) and project-local
    /// (`<cwd>/.aish/hooks.json`) config. A missing file is fine (empty); a
    /// malformed file is reported to stderr and skipped, so one bad project file
    /// never wedges startup.
    #[allow(dead_code)] // superseded by load_with_plugins (0.5.6); retained for the tests + external callers
    pub fn load(home: Option<&Path>, cwd: &Path) -> Self {
        // Route through the layered merge (with no plugins) so name-based
        // override/disable and the one-blocking-winner rule apply to the
        // user>project stack too. Configs without `name`/`enabled` behave
        // exactly as the legacy append merge did.
        Self::load_with_plugins(home, cwd, &[])
    }

    /// Load + merge an explicit, ordered list of config files. Earlier files
    /// register first (so they dispatch first). The testable core of [`load`].
    #[allow(dead_code)]
    pub fn load_from(paths: &[PathBuf]) -> Self {
        let mut hooks: Vec<Hook> = Vec::new();
        for path in paths {
            match std::fs::read_to_string(path) {
                Ok(text) => match parse_hooks(&text) {
                    Ok(mut parsed) => hooks.append(&mut parsed),
                    Err(e) => eprintln!("aish: ignoring {} — {e}", path.display()),
                },
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => eprintln!("aish: cannot read {} — {e}", path.display()),
            }
        }
        Self {
            hooks: Arc::new(hooks),
        }
    }

    /// Load + merge the user/project catalogs **and** any plugin-contributed
    /// `event_hooks_file` fragments (Phase 0.5.2). Precedence is
    /// **user > project > plugin**; within plugins, fragment order (as passed)
    /// decides ties. Each plugin entry is tagged `source: plugin:<id>`.
    ///
    /// Two cross-layer rules are applied after the raw parse:
    ///   * **override / disable by name** — a higher-precedence hook that
    ///     re-declares a `name` replaces the lower one; a same-named tombstone
    ///     (`"enabled": false`) suppresses it entirely. Unnamed hooks are never
    ///     overridden (all fire — observe fan-out).
    ///   * **one blocking winner per event** — on a blocking-class event
    ///     ([`HookEvent::is_blocking`]) only the highest-precedence entry keeps
    ///     the veto; lower ones (including every plugin entry when a local hook
    ///     owns the event) are demoted to observe (`observe_only`).
    pub fn load_with_plugins(
        home: Option<&Path>,
        cwd: &Path,
        plugins: &[PluginHookFragment],
    ) -> Self {
        let mut local_paths: Vec<PathBuf> = Vec::new();
        if let Some(home) = home {
            local_paths.push(home.join(".aish").join("hooks.json"));
        }
        local_paths.push(cwd.join(".aish").join("hooks.json"));
        Self::load_layered(&local_paths, plugins)
    }

    /// The testable core of [`load_with_plugins`]: `local_paths` are read in
    /// precedence order (highest first — i.e. user, then project) and tagged
    /// [`HookSource::Local`]; each plugin fragment is tagged
    /// [`HookSource::Plugin`]. A missing file is skipped; a malformed one is
    /// reported and skipped (one bad fragment never wedges startup).
    pub fn load_layered(local_paths: &[PathBuf], plugins: &[PluginHookFragment]) -> Self {
        // (source, parsed hooks) in strict precedence order, highest first.
        let mut layers: Vec<(HookSource, Vec<Hook>)> = Vec::new();
        let read_layer = |path: &Path| -> Option<Vec<Hook>> {
            match std::fs::read_to_string(path) {
                Ok(text) => match parse_hooks(&text) {
                    Ok(parsed) => Some(parsed),
                    Err(e) => {
                        eprintln!("aish: ignoring {} — {e}", path.display());
                        None
                    }
                },
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
                Err(e) => {
                    eprintln!("aish: cannot read {} — {e}", path.display());
                    None
                }
            }
        };
        for path in local_paths {
            if let Some(hooks) = read_layer(path) {
                layers.push((HookSource::Local, hooks));
            }
        }
        for pf in plugins {
            if let Some(hooks) = read_layer(&pf.path) {
                layers.push((HookSource::Plugin(pf.plugin_id.clone()), hooks));
            }
        }
        Self::merge_layers(layers)
    }

    /// Apply the override/disable-by-name and one-blocking-winner rules to the
    /// precedence-ordered `layers` (highest first) and return the merged set.
    /// Factored out so tests can drive the merge from in-memory layers.
    fn merge_layers(layers: Vec<(HookSource, Vec<Hook>)>) -> Self {
        use std::collections::HashSet;
        // Pass 1: override / disable by name, walking highest precedence first.
        let mut seen_names: HashSet<String> = HashSet::new();
        let mut out: Vec<Hook> = Vec::new();
        for (source, hooks) in layers {
            for mut hook in hooks {
                hook.source = source.clone();
                let disabled = hook.enabled == Some(false);
                if let Some(name) = hook.name.clone() {
                    if seen_names.contains(&name) {
                        // A higher-precedence catalog already owns this name.
                        continue;
                    }
                    seen_names.insert(name);
                    if disabled {
                        // Tombstone: registers nothing, but now suppresses any
                        // lower-precedence hook sharing the name (recorded above).
                        continue;
                    }
                } else if disabled {
                    // Unnamed + disabled: nothing to reference it, just drop it.
                    continue;
                }
                out.push(hook);
            }
        }
        // Pass 2: one blocking winner per event — but only ever demote PLUGIN
        // entries. Local (user+project) hooks keep the legacy most-restrictive-
        // wins semantics of the `evaluate` gate. A plugin blocking entry keeps
        // its veto only when it is the FIRST entry seen on that event (no local
        // hook and no earlier plugin owns it); otherwise it degrades to observe.
        // Because `out` is precedence ordered (local first, then plugins in
        // fragment order), a single forward scan gives "highest precedence wins".
        let mut blocking_seen: HashSet<HookEvent> = HashSet::new();
        for hook in out.iter_mut() {
            if !hook.event.is_blocking() {
                continue;
            }
            if hook.source.is_plugin() && blocking_seen.contains(&hook.event) {
                hook.observe_only = true;
            } else {
                blocking_seen.insert(hook.event);
            }
        }
        Self {
            hooks: Arc::new(out),
        }
    }

    /// Fire an OBSERVE-only event: spawn every matching hook on a detached tokio
    /// task and return IMMEDIATELY. Best-effort by design — a slow or failing
    /// observe hook never blocks the turn (design §2 split: observe = async, off
    /// the hot path). No-op (no spawn, no task) when nothing matches or when we
    /// are already inside a hook (`AISH_IN_HOOK`).
    ///
    /// Requires a tokio runtime in scope (every call site is inside one).
    pub fn fire_observe(&self, event: HookEvent, payload: HookPayload) {
        if in_hook() {
            return;
        }
        let matched = self.matching(event, &payload);
        if matched.is_empty() {
            return;
        }
        let body = payload.to_json_string();
        for hook in matched {
            let body = body.clone();
            let action = hook_action(&hook);
            tokio::spawn(async move {
                let _ = dispatch(&action, &body).await;
            });
        }
    }

    /// Like [`fire_observe`] but AWAITS every matching hook (bounded by each
    /// hook's timeout). Used at `SessionEnd`, where the process is about to exit
    /// and a detached task would be killed before it runs. Returns the number of
    /// hooks invoked.
    pub async fn run_observe_blocking(&self, event: HookEvent, payload: HookPayload) -> usize {
        if in_hook() {
            return 0;
        }
        let matched = self.matching(event, &payload);
        if matched.is_empty() {
            return 0;
        }
        let body = payload.to_json_string();
        let mut n = 0;
        for hook in matched {
            let action = hook_action(&hook);
            let _ = dispatch(&action, &body).await;
            n += 1;
        }
        n
    }

    /// The hooks matching `event` + `payload`, bumping each one's dispatch
    /// counter. Returns owned `Arc<HookMatch>` so the detached task outlives the
    /// borrow on `self`.
    fn matching(&self, event: HookEvent, payload: &HookPayload) -> Vec<Arc<HookMatch>> {
        let mut out = Vec::new();
        for hook in self.hooks.iter() {
            if hook.event == event && hook.matcher.matches(payload) {
                hook.matched.fetch_add(1, Ordering::Relaxed);
                out.push(Arc::new(HookMatch {
                    action: hook.action.clone(),
                    observe_only: hook.observe_only,
                }));
            }
        }
        out
    }
}

/// A snapshot of a matched hook's action, detached from the `HookSet` borrow.
struct HookMatch {
    action: Action,
    /// When true this entry lost the blocking one-winner contest (Phase 0.5.2):
    /// the gate runs it for side effects but its veto is ignored.
    observe_only: bool,
}

fn hook_action(m: &Arc<HookMatch>) -> Action {
    m.action.clone()
}

/// True when we are already running inside a hook (recursion guard).
fn in_hook() -> bool {
    std::env::var_os(RECURSION_GUARD).is_some()
}

/// Spawn one hook action with the JSON payload on stdin, bounded by its timeout.
/// `Command` is fork/exec (no shell); `Rule` is a Phase-2 no-op here. Returns
/// `Ok(true)` when the hook ran and exited 0, `Ok(false)` on non-zero exit, and
/// `Err` on spawn failure or timeout.
async fn dispatch(action: &Action, payload: &str) -> Result<bool, DispatchError> {
    let (program, args) = match action {
        Action::Command { program, args, .. } => (program.clone(), args.clone()),
        // Inline rules don't spawn — reserved for the Phase-2 blocking gate.
        Action::Rule { .. } => return Ok(true),
    };
    use std::process::Stdio;
    use tokio::io::AsyncWriteExt;
    let mut child = tokio::process::Command::new(&program)
        .args(&args)
        .env(RECURSION_GUARD, "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| DispatchError::Spawn(e.to_string()))?;
    if let Some(mut stdin) = child.stdin.take() {
        // Best-effort: a hook that ignores stdin (closes it early) makes the
        // write fail with EPIPE; that is not our problem to surface.
        let _ = stdin.write_all(payload.as_bytes()).await;
        let _ = stdin.shutdown().await;
    }
    match tokio::time::timeout(action.timeout(), child.wait()).await {
        Ok(Ok(status)) => Ok(status.success()),
        Ok(Err(e)) => Err(DispatchError::Wait(e.to_string())),
        Err(_) => {
            // Timed out — kill it (kill_on_drop also covers the drop path).
            let _ = child.start_kill();
            Err(DispatchError::Timeout)
        }
    }
}

#[derive(Debug)]
enum DispatchError {
    Spawn(String),
    Wait(String),
    Timeout,
}

impl std::fmt::Display for DispatchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Spawn(e) => write!(f, "spawn failed: {e}"),
            Self::Wait(e) => write!(f, "wait failed: {e}"),
            Self::Timeout => write!(f, "timed out"),
        }
    }
}

/// The outcome of the Phase 2 blocking gate for one event. `Allow` lets the
/// action proceed; `Deny` carries a human-readable reason that is threaded into
/// the synthetic "declined" tool result the model sees — identical to a human
/// decline — so the model handles a hook veto exactly like a user one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Allow,
    Deny(String),
}

impl Decision {
    /// True when this is a veto.
    pub fn is_deny(&self) -> bool {
        matches!(self, Decision::Deny(_))
    }
}

impl HookSet {
    /// Evaluate a BLOCKING event (the Phase 2 gate): run every matching hook
    /// SEQUENTIALLY and combine most-restrictive-wins — the FIRST `Deny`
    /// short-circuits and is returned. A hook can only make a decision stricter,
    /// never loosen it (design §6.2), so there is no "allow override": an
    /// explicit `Allow` from one hook does not cancel a later hook's `Deny`
    /// (we stop at the first deny). Zero-cost and `Allow` when nothing matches
    /// or when we are already inside a hook (`AISH_IN_HOOK`).
    ///
    /// Per action kind:
    ///   * `command` — spawned fork/exec with the payload on stdin; **exit 0 =
    ///     allow, non-zero = deny** (its first non-empty stdout line becomes the
    ///     reason). On spawn failure or timeout a `required` hook fails **closed**
    ///     (deny), an optional hook fails **open** (allow).
    ///   * `rule` — the inline `deny_if` predicate is evaluated with NO process
    ///     spawned at all (see [`eval_rule`]); a match denies.
    ///
    /// Requires a tokio runtime in scope (the call site is inside one).
    pub async fn evaluate(&self, event: HookEvent, payload: HookPayload) -> Decision {
        if in_hook() {
            return Decision::Allow;
        }
        let matched = self.matching(event, &payload);
        if matched.is_empty() {
            return Decision::Allow;
        }
        let body = payload.to_json_string();
        for hook in matched {
            let decision = match &hook.action {
                Action::Rule { deny_if } => eval_rule(deny_if.as_deref(), &payload),
                Action::Command { required, .. } => match dispatch_capture(&hook.action, &body).await
                {
                    Ok((true, _)) => Decision::Allow,
                    Ok((false, reason)) => Decision::Deny(
                        reason.unwrap_or_else(|| "hook command exited non-zero".to_string()),
                    ),
                    Err(e) => {
                        if *required {
                            Decision::Deny(format!("required hook failed ({e})"))
                        } else {
                            // Optional hook: fail OPEN so a flaky logger can't wedge work.
                            eprintln!("\x1b[2maish: hook dispatch error (allowed) — {e}\x1b[0m");
                            Decision::Allow
                        }
                    }
                },
            };
            // A demoted plugin entry (Phase 0.5.2 one-winner rule) still ran for
            // its side effects above, but cannot veto — its decision is ignored.
            if hook.observe_only {
                continue;
            }
            if decision.is_deny() {
                return decision;
            }
        }
        Decision::Allow
    }
}

/// Like [`dispatch`] but CAPTURES the hook's stdout so a non-zero exit can carry
/// a reason (its first non-empty stdout line) into the blocking gate's `Deny`.
/// Returns `(exit_ok, reason)`; `Err` on spawn failure or timeout (the gate maps
/// that to fail-open/closed per the hook's `required` flag). On timeout the
/// dropped child is reaped by `kill_on_drop`.
async fn dispatch_capture(
    action: &Action,
    payload: &str,
) -> Result<(bool, Option<String>), DispatchError> {
    let (program, args) = match action {
        Action::Command { program, args, .. } => (program.clone(), args.clone()),
        Action::Rule { .. } => return Ok((true, None)),
    };
    use std::process::Stdio;
    use tokio::io::AsyncWriteExt;
    let mut child = tokio::process::Command::new(&program)
        .args(&args)
        .env(RECURSION_GUARD, "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| DispatchError::Spawn(e.to_string()))?;
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(payload.as_bytes()).await;
        let _ = stdin.shutdown().await;
    }
    match tokio::time::timeout(action.timeout(), child.wait_with_output()).await {
        Ok(Ok(output)) => {
            let reason = String::from_utf8_lossy(&output.stdout)
                .lines()
                .map(str::trim)
                .find(|l| !l.is_empty())
                .map(|s| s.to_string());
            Ok((output.status.success(), reason))
        }
        Ok(Err(e)) => Err(DispatchError::Wait(e.to_string())),
        Err(_) => Err(DispatchError::Timeout),
    }
}

/// Evaluate an inline `deny_if` predicate against the payload — a tiny,
/// zero-spawn policy language for the common blocking cases. Supported forms
/// (case-sensitive keyword, single-quoted string arg):
///   * `always` / `true`             → always deny
///   * `never`  / `false`            → never deny
///   * `tool_is('run_program')`      → deny when the `tool` field == arg
///   * `program_is('git')`           → deny when `program` == arg
///   * `program_contains('rm')`      → deny when `program` contains arg
///   * `path_contains('secret')`     → deny when `path` contains arg
///   * `path_glob('*.lock')`         → deny when `path` glob-matches arg
///   * `mode_is('yolo')`             → deny when the confirmation `mode` == arg
///
/// A referenced field that is absent from the payload does NOT match (the
/// predicate can't be satisfied → allow). An UNRECOGNIZED or malformed
/// expression denies **fail-closed**, so a typo can't silently disable a policy.
/// A rule with no `deny_if` at all is a no-op (allow).
fn eval_rule(expr: Option<&str>, payload: &HookPayload) -> Decision {
    let Some(raw) = expr else {
        return Decision::Allow;
    };
    let e = raw.trim();
    let blocked = |b: bool| {
        if b {
            Decision::Deny(format!("blocked by rule: {e}"))
        } else {
            Decision::Allow
        }
    };
    match e {
        "always" | "true" => return Decision::Deny(format!("blocked by rule: {e}")),
        "never" | "false" => return Decision::Allow,
        _ => {}
    }
    let Some((name, arg)) = parse_predicate(e) else {
        return Decision::Deny(format!("malformed hook rule (fail-closed): {e}"));
    };
    let field = |k: &str| payload.field(k);
    match name {
        "tool_is" => blocked(field("tool") == Some(arg.as_str())),
        "program_is" => blocked(field("program") == Some(arg.as_str())),
        "program_contains" => blocked(field("program").is_some_and(|v| v.contains(arg.as_str()))),
        "path_contains" => blocked(field("path").is_some_and(|v| v.contains(arg.as_str()))),
        "path_glob" => blocked(field("path").is_some_and(|v| glob_match(&arg, v))),
        "mode_is" => blocked(field("mode") == Some(arg.as_str())),
        other => Decision::Deny(format!("unknown hook rule predicate (fail-closed): {other}")),
    }
}

/// Parse a `name('single-quoted arg')` predicate into `(name, arg)`. Returns
/// `None` when the shape doesn't match (the caller fails that closed). Only
/// single quotes; the arg may contain any char except a single quote.
fn parse_predicate(e: &str) -> Option<(&str, String)> {
    // Tolerate surrounding whitespace even on a direct call (eval_rule already
    // trims, but the predicate parser is robust on its own).
    let e = e.trim();
    let open = e.find('(')?;
    let name = e[..open].trim();
    if name.is_empty() || !e.ends_with(')') {
        return None;
    }
    let inner = e[open + 1..e.len() - 1].trim();
    let stripped = inner.strip_prefix('\'')?.strip_suffix('\'')?;
    if stripped.contains('\'') {
        return None;
    }
    Some((name, stripped.to_string()))
}

/// Parse a `hooks.json` body into a `Vec<Hook>`. Surfaces a clear error on
/// malformed JSON or an unknown event name.
fn parse_hooks(text: &str) -> Result<Vec<Hook>, String> {
    let file: HookFile = serde_json::from_str(text).map_err(|e| e.to_string())?;
    Ok(file.hooks)
}

/// The JSON envelope handed to a hook on stdin. Carries the common fields every
/// hook gets (event, session, agent, cwd, mode, timestamp) plus event-specific
/// extras. Built lazily inside a `has`-guarded block so the empty fast path
/// never constructs one.
#[derive(Debug, Clone)]
pub struct HookPayload {
    fields: Map<String, Value>,
}

impl HookPayload {
    /// Start an envelope with the common fields. `ts_ms` is the wall-clock epoch
    /// (ms) — taken once here so it isn't read per-field.
    pub fn new(event: HookEvent, session_id: &str, agent: Agent, cwd: &Path, mode: &str) -> Self {
        let mut fields = Map::new();
        fields.insert("event".into(), Value::from(event.as_str()));
        fields.insert("session_id".into(), Value::from(session_id));
        fields.insert("agent".into(), Value::from(agent.as_str()));
        fields.insert(
            "cwd".into(),
            Value::from(cwd.to_string_lossy().into_owned()),
        );
        fields.insert("mode".into(), Value::from(mode));
        fields.insert("timestamp_ms".into(), Value::from(epoch_ms()));
        Self { fields }
    }

    /// Attach (or overwrite) one event-specific field. Chains fluently.
    pub fn with(mut self, key: &str, value: impl Into<Value>) -> Self {
        self.fields.insert(key.to_string(), value.into());
        self
    }

    /// Read a string-valued field (used by the matcher). Non-string values
    /// (numbers, bools) read as `None` for matching purposes.
    pub fn field(&self, key: &str) -> Option<&str> {
        self.fields.get(key).and_then(Value::as_str)
    }

    /// Serialize to the compact JSON line written to the hook's stdin.
    pub fn to_json_string(&self) -> String {
        Value::Object(self.fields.clone()).to_string()
    }
}

/// Wall-clock epoch milliseconds, saturating to 0 before 1970 (never panics).
fn epoch_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Minimal glob: `*` matches any run (including empty), `?` matches exactly one
/// char, everything else is literal. No character classes or path-segment
/// semantics — deliberately small (the design's matcher is a convenience filter,
/// not a path language). Anchored: the whole text must match.
pub fn glob_match(pattern: &str, text: &str) -> bool {
    // Classic two-pointer wildcard match with backtracking on the last `*`.
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    let (mut pi, mut ti) = (0usize, 0usize);
    let (mut star, mut mark) = (None::<usize>, 0usize);
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            mark = ti;
            pi += 1;
        } else if let Some(s) = star {
            pi = s + 1;
            mark += 1;
            ti = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serializes tests that touch the PROCESS-GLOBAL recursion-guard env var or
    /// depend on it being unset. `AISH_IN_HOOK` is process-wide, so a test that
    /// sets it (the recursion-guard tests) races every parallel test that spawns
    /// a hook or calls `evaluate`/`run_observe_blocking` (each early-returns when
    /// the guard is seen). Every such test grabs this lock first, forcing them to
    /// run one-at-a-time. Poison is ignored — a panic in one guarded test must
    /// not wedge the rest.
    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
        LOCK.get_or_init(|| std::sync::Mutex::new(()))
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    fn payload(event: HookEvent) -> HookPayload {
        HookPayload::new(
            event,
            "sess-1",
            Agent::Interactive,
            Path::new("/tmp/proj"),
            "normal",
        )
    }

    // ---- HookEvent round-trip ------------------------------------------

    #[test]
    fn event_names_round_trip() {
        for e in [
            HookEvent::SessionStart,
            HookEvent::PreToolUse,
            HookEvent::PostToolUse,
            HookEvent::FileChanged,
            HookEvent::SessionEnd,
            HookEvent::SkillMatched,
            HookEvent::CoordinatorPhaseChanged,
        ] {
            assert_eq!(HookEvent::parse(e.as_str()), Some(e), "round-trip {e:?}");
        }
        assert_eq!(HookEvent::parse("NotAnEvent"), None);
        // Case-sensitive — a lowercase typo is rejected.
        assert_eq!(HookEvent::parse("pretooluse"), None);
    }

    // ---- glob matcher --------------------------------------------------

    #[test]
    fn glob_basics() {
        assert!(glob_match("*", "anything"));
        assert!(glob_match("*", ""));
        assert!(glob_match("git", "git"));
        assert!(!glob_match("git", "gitk"));
        assert!(glob_match("git*", "git push"));
        assert!(glob_match("*.lock", "Cargo.lock"));
        assert!(!glob_match("*.lock", "Cargo.toml"));
        assert!(glob_match("?at", "cat"));
        assert!(!glob_match("?at", "at"));
        assert!(glob_match("a*b*c", "axxbyyc"));
        assert!(!glob_match("a*b*c", "axxbyy"));
        assert!(glob_match("**/secret*", "src/x/secret.key"));
    }

    // ---- Matcher semantics --------------------------------------------

    #[test]
    fn matcher_empty_matches_everything() {
        let m = Matcher::default();
        assert!(m.matches(&payload(HookEvent::PreToolUse)));
    }

    #[test]
    fn matcher_fields_and_together() {
        let m = Matcher {
            tool: Some("run_program".into()),
            program: Some("git".into()),
            ..Default::default()
        };
        let p = payload(HookEvent::PreToolUse)
            .with("tool", "run_program")
            .with("program", "git");
        assert!(m.matches(&p));
        // Wrong program → no match (AND).
        let p2 = payload(HookEvent::PreToolUse)
            .with("tool", "run_program")
            .with("program", "ls");
        assert!(!m.matches(&p2));
    }

    #[test]
    fn matcher_program_filter_excludes_when_field_absent() {
        // A `program` filter on an event that carries no program (e.g. read_file)
        // must NOT match — a present matcher field with no payload field fails.
        let m = Matcher {
            program: Some("git".into()),
            ..Default::default()
        };
        let p = payload(HookEvent::PreToolUse).with("tool", "read_file");
        assert!(!m.matches(&p));
    }

    #[test]
    fn matcher_mode_is_or_list() {
        let m = Matcher {
            mode: vec!["paranoid".into(), "careful".into()],
            ..Default::default()
        };
        // payload() builds mode=normal, which is not in the OR list → no match.
        assert!(!m.matches(&payload(HookEvent::PreToolUse)));
        // Rebuild with a mode that IS in the list → match.
        let p = HookPayload::new(
            HookEvent::PreToolUse,
            "s",
            Agent::Interactive,
            Path::new("/x"),
            "paranoid",
        );
        assert!(m.matches(&p));
        let p2 = HookPayload::new(
            HookEvent::PreToolUse,
            "s",
            Agent::Interactive,
            Path::new("/x"),
            "yolo",
        );
        assert!(!m.matches(&p2));
    }

    #[test]
    fn matcher_agent_filter() {
        let m = Matcher {
            agent: Some("interactive".into()),
            ..Default::default()
        };
        assert!(m.matches(&payload(HookEvent::PreToolUse)));
        let coord = HookPayload::new(
            HookEvent::PreToolUse,
            "s",
            Agent::Coordinator,
            Path::new("/x"),
            "normal",
        );
        assert!(!m.matches(&coord));
    }

    #[test]
    fn matcher_path_glob() {
        // `*` spans everything including path separators (no path-segment
        // semantics), so a suffix pattern matches an absolute path too.
        let m = Matcher {
            path_glob: Some("*.lock".into()),
            ..Default::default()
        };
        assert!(m.matches(&payload(HookEvent::FileChanged).with("path", "/proj/Cargo.lock")));
        assert!(m.matches(&payload(HookEvent::FileChanged).with("path", "Cargo.lock")));
        // A non-matching suffix is excluded.
        assert!(!m.matches(&payload(HookEvent::FileChanged).with("path", "/proj/Cargo.toml")));
        // A more specific stem still matches.
        let m3 = Matcher {
            path_glob: Some("*Cargo.lock".into()),
            ..Default::default()
        };
        assert!(m3.matches(&payload(HookEvent::FileChanged).with("path", "/proj/Cargo.lock")));
    }

    // ---- mode-list edge: payload() default is normal -------------------
    #[test]
    fn matcher_mode_absent_when_not_in_list() {
        let m = Matcher {
            mode: vec!["paranoid".into()],
            ..Default::default()
        };
        // Default payload is mode=normal → excluded.
        assert!(!m.matches(&payload(HookEvent::PreToolUse)));
    }

    // ---- Config parsing & merge ---------------------------------------

    #[test]
    fn parse_valid_config() {
        let json = r#"{
          "hooks": [
            {
              "event": "PreToolUse",
              "matcher": { "tool": "run_program", "program": "git" },
              "action": { "type": "command", "program": "/bin/echo", "timeout_ms": 1000 }
            },
            {
              "event": "FileChanged",
              "action": { "type": "command", "program": "rustfmt" }
            }
          ]
        }"#;
        let hooks = parse_hooks(json).unwrap();
        assert_eq!(hooks.len(), 2);
        assert_eq!(hooks[0].event, HookEvent::PreToolUse);
        assert_eq!(hooks[0].matcher.program.as_deref(), Some("git"));
        match &hooks[1].action {
            Action::Command {
                program,
                timeout_ms,
                required,
                ..
            } => {
                assert_eq!(program, "rustfmt");
                assert_eq!(*timeout_ms, None);
                assert!(!*required);
            }
            _ => panic!("expected command action"),
        }
    }

    #[test]
    fn parse_rejects_unknown_event() {
        let json = r#"{ "hooks": [ { "event": "Nope", "action": { "type": "command", "program": "x" } } ] }"#;
        let err = parse_hooks(json).unwrap_err();
        assert!(err.contains("unknown hook event"), "{err}");
    }

    #[test]
    fn parse_rule_action_is_accepted() {
        let json = r#"{ "hooks": [ { "event": "PreToolUse", "action": { "type": "rule", "deny_if": "path_contains('secret')" } } ] }"#;
        let hooks = parse_hooks(json).unwrap();
        match &hooks[0].action {
            Action::Rule { deny_if } => {
                assert_eq!(deny_if.as_deref(), Some("path_contains('secret')"))
            }
            _ => panic!("expected rule action"),
        }
    }

    #[test]
    fn load_from_merges_in_order_and_skips_missing() {
        let dir = std::env::temp_dir().join(format!("aish-hooks-merge-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let a = dir.join("a.json");
        let b = dir.join("b.json");
        std::fs::write(
            &a,
            r#"{ "hooks": [ { "event": "SessionStart", "action": { "type": "command", "program": "a" } } ] }"#,
        )
        .unwrap();
        std::fs::write(
            &b,
            r#"{ "hooks": [ { "event": "SessionEnd", "action": { "type": "command", "program": "b" } } ] }"#,
        )
        .unwrap();
        let missing = dir.join("missing.json");
        let set = HookSet::load_from(&[a, missing, b]);
        assert_eq!(set.len(), 2);
        assert!(set.has(HookEvent::SessionStart));
        assert!(set.has(HookEvent::SessionEnd));
        assert!(!set.has(HookEvent::PreToolUse));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn malformed_file_is_skipped_not_fatal() {
        let dir = std::env::temp_dir().join(format!("aish-hooks-bad-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let bad = dir.join("bad.json");
        std::fs::write(&bad, "{ not json").unwrap();
        let set = HookSet::load_from(&[bad]);
        assert!(set.is_empty(), "a malformed file must not register hooks");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---- Fast path ------------------------------------------------------

    #[test]
    fn empty_set_has_nothing() {
        let set = HookSet::empty();
        assert!(set.is_empty());
        assert!(!set.has(HookEvent::PreToolUse));
        assert_eq!(set.len(), 0);
    }

    // ---- Payload --------------------------------------------------------

    #[test]
    fn payload_carries_common_envelope() {
        let p = payload(HookEvent::PreToolUse).with("tool", "run_program");
        let v: Value = serde_json::from_str(&p.to_json_string()).unwrap();
        assert_eq!(v["event"], "PreToolUse");
        assert_eq!(v["session_id"], "sess-1");
        assert_eq!(v["agent"], "interactive");
        assert_eq!(v["cwd"], "/tmp/proj");
        assert_eq!(v["mode"], "normal");
        assert_eq!(v["tool"], "run_program");
        assert!(v["timestamp_ms"].as_u64().is_some());
    }

    #[test]
    fn payload_carries_no_unset_fields() {
        // Sanity: a freshly built payload has exactly the 6 envelope keys.
        let p = payload(HookEvent::SessionStart);
        let v: Value = serde_json::from_str(&p.to_json_string()).unwrap();
        let obj = v.as_object().unwrap();
        assert_eq!(
            obj.len(),
            6,
            "envelope keys: {:?}",
            obj.keys().collect::<Vec<_>>()
        );
    }

    // ---- Dispatch (real fork/exec) -------------------------------------

    #[tokio::test]
    async fn dispatch_runs_a_real_program() {
        // A hook that creates a sentinel file proves the payload-on-stdin
        // fork/exec path works end-to-end without a shell.
        let dir = std::env::temp_dir().join(format!("aish-hook-fire-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let sentinel = dir.join("fired");
        let action = Action::Command {
            program: "touch".into(),
            args: vec![sentinel.to_string_lossy().into_owned()],
            timeout_ms: Some(2000),
            required: false,
        };
        let ok = dispatch(&action, "{}").await.unwrap();
        assert!(ok, "touch should exit 0");
        assert!(sentinel.exists(), "hook program did not run");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn dispatch_times_out_a_slow_hook() {
        // `sleep 5` with a 100ms timeout must be killed and reported as timeout,
        // returning quickly rather than blocking for 5s.
        let action = Action::Command {
            program: "sleep".into(),
            args: vec!["5".into()],
            timeout_ms: Some(100),
            required: false,
        };
        let start = std::time::Instant::now();
        let res = dispatch(&action, "{}").await;
        assert!(matches!(res, Err(DispatchError::Timeout)), "{res:?}");
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "timeout did not cut the hook short"
        );
    }

    #[tokio::test]
    async fn dispatch_reports_spawn_failure() {
        let action = Action::Command {
            program: "/nonexistent/aish-hook-binary".into(),
            args: vec![],
            timeout_ms: Some(500),
            required: false,
        };
        assert!(matches!(
            dispatch(&action, "{}").await,
            Err(DispatchError::Spawn(_))
        ));
    }

    #[tokio::test]
    async fn fire_observe_is_recursion_guarded() {
        let _env = env_lock();
        // With AISH_IN_HOOK set, fire_observe must early-return without spawning.
        // SAFETY: single-threaded test; we set and unset the guard around the call.
        unsafe { std::env::set_var(RECURSION_GUARD, "1") };
        let json = format!(
            r#"{{ "hooks": [ {{ "event": "SessionStart", "action": {{ "type": "command", "program": "touch", "args": ["{}"] }} }} ] }}"#,
            std::env::temp_dir().join("aish-should-not-exist").display()
        );
        let dir = std::env::temp_dir().join(format!("aish-hook-guard-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let cfg = dir.join("h.json");
        std::fs::write(&cfg, json).unwrap();
        let set = HookSet::load_from(&[cfg]);
        let n = set
            .run_observe_blocking(HookEvent::SessionStart, payload(HookEvent::SessionStart))
            .await;
        assert_eq!(n, 0, "recursion guard must suppress dispatch");
        unsafe { std::env::remove_var(RECURSION_GUARD) };
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn run_observe_blocking_invokes_matching_hooks() {
        let _env = env_lock();
        let dir = std::env::temp_dir().join(format!("aish-hook-block-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let sentinel = dir.join("ran");
        let json = format!(
            r#"{{ "hooks": [ {{ "event": "SessionEnd", "action": {{ "type": "command", "program": "touch", "args": ["{}"], "timeout_ms": 2000 }} }} ] }}"#,
            sentinel.display()
        );
        let cfg = dir.join("h.json");
        std::fs::write(&cfg, json).unwrap();
        let set = HookSet::load_from(&[cfg]);
        let n = set
            .run_observe_blocking(HookEvent::SessionEnd, payload(HookEvent::SessionEnd))
            .await;
        assert_eq!(n, 1);
        assert!(sentinel.exists());
        // A non-matching event invokes nothing.
        let n2 = set
            .run_observe_blocking(HookEvent::SessionStart, payload(HookEvent::SessionStart))
            .await;
        assert_eq!(n2, 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn matching_bumps_counter() {
        let _env = env_lock();
        let json = r#"{ "hooks": [ { "event": "PostToolUse", "action": { "type": "command", "program": "true" } } ] }"#;
        let dir = std::env::temp_dir().join(format!("aish-hook-count-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let cfg = dir.join("h.json");
        std::fs::write(&cfg, json).unwrap();
        let set = HookSet::load_from(&[cfg]);
        let _ = set
            .run_observe_blocking(HookEvent::PostToolUse, payload(HookEvent::PostToolUse))
            .await;
        assert_eq!(set.hooks()[0].matched.load(Ordering::Relaxed), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---- Phase 2: blocking gate (evaluate / eval_rule / parse_predicate) ----

    #[test]
    fn parse_predicate_shapes() {
        assert_eq!(
            parse_predicate("path_contains('secret')"),
            Some(("path_contains", "secret".to_string()))
        );
        // Whitespace around the name / arg is tolerated.
        assert_eq!(
            parse_predicate("  program_is( 'git' ) "),
            Some(("program_is", "git".to_string()))
        );
        // Empty arg is a valid string.
        assert_eq!(parse_predicate("tool_is('')"), Some(("tool_is", String::new())));
        // Malformed shapes → None (caller fails these closed).
        assert_eq!(parse_predicate("no_parens"), None);
        assert_eq!(parse_predicate("missing_close('x'"), None);
        assert_eq!(parse_predicate("unquoted(x)"), None);
        assert_eq!(parse_predicate("('orphan')"), None);
    }

    #[test]
    fn eval_rule_keywords_and_predicates() {
        let p = payload(HookEvent::PreToolUse)
            .with("tool", "run_program")
            .with("program", "git")
            .with("path", "/proj/.env.secret");
        // Keyword forms.
        assert!(eval_rule(Some("always"), &p).is_deny());
        assert!(eval_rule(Some("true"), &p).is_deny());
        assert_eq!(eval_rule(Some("never"), &p), Decision::Allow);
        assert_eq!(eval_rule(Some("false"), &p), Decision::Allow);
        // No deny_if at all → allow.
        assert_eq!(eval_rule(None, &p), Decision::Allow);
        // Field predicates that match → deny.
        assert!(eval_rule(Some("tool_is('run_program')"), &p).is_deny());
        assert!(eval_rule(Some("program_is('git')"), &p).is_deny());
        assert!(eval_rule(Some("program_contains('gi')"), &p).is_deny());
        assert!(eval_rule(Some("path_contains('secret')"), &p).is_deny());
        assert!(eval_rule(Some("path_glob('*.secret')"), &p).is_deny());
        // Predicates that do not match → allow.
        assert_eq!(eval_rule(Some("program_is('rm')"), &p), Decision::Allow);
        assert_eq!(eval_rule(Some("path_contains('nope')"), &p), Decision::Allow);
        // Absent field → cannot be satisfied → allow.
        assert_eq!(eval_rule(Some("mode_is('yolo')"), &p), Decision::Allow);
    }

    #[test]
    fn eval_rule_fails_closed_on_garbage() {
        let p = payload(HookEvent::PreToolUse);
        // Unknown predicate name and malformed expressions both DENY (fail-closed)
        // so a typo can't silently disable a policy.
        assert!(eval_rule(Some("banana('x')"), &p).is_deny());
        assert!(eval_rule(Some("path_contains(secret)"), &p).is_deny());
        assert!(eval_rule(Some("garbage"), &p).is_deny());
    }

    /// Build a `HookSet` from an inline JSON body (test helper).
    fn set_from_json(json: &str) -> HookSet {
        let dir = std::env::temp_dir().join(format!(
            "aish-hook-eval-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let cfg = dir.join("h.json");
        std::fs::write(&cfg, json).unwrap();
        HookSet::load_from(&[cfg])
    }

    #[tokio::test]
    async fn evaluate_rule_denies_matching_call() {
        let _env = env_lock();
        let set = set_from_json(
            r#"{ "hooks": [ { "event": "PreToolUse", "matcher": { "tool": "run_program" },
                 "action": { "type": "rule", "deny_if": "program_contains('rm')" } } ] }"#,
        );
        // A matching call is vetoed…
        let deny = set
            .evaluate(
                HookEvent::PreToolUse,
                payload(HookEvent::PreToolUse)
                    .with("tool", "run_program")
                    .with("program", "rm"),
            )
            .await;
        assert!(deny.is_deny(), "{deny:?}");
        // …a non-matching program is allowed.
        let allow = set
            .evaluate(
                HookEvent::PreToolUse,
                payload(HookEvent::PreToolUse)
                    .with("tool", "run_program")
                    .with("program", "ls"),
            )
            .await;
        assert_eq!(allow, Decision::Allow);
    }

    #[tokio::test]
    async fn evaluate_allows_when_nothing_matches() {
        let _env = env_lock();
        let set = set_from_json(
            r#"{ "hooks": [ { "event": "PreToolUse", "action": { "type": "rule", "deny_if": "always" } } ] }"#,
        );
        // A different event has no matching hook → allow, no spawn.
        let d = set
            .evaluate(HookEvent::PostToolUse, payload(HookEvent::PostToolUse))
            .await;
        assert_eq!(d, Decision::Allow);
    }

    #[tokio::test]
    async fn evaluate_command_exit_code_gates() {
        let _env = env_lock();
        // `false` exits non-zero → deny; `true` exits zero → allow.
        let deny_set = set_from_json(
            r#"{ "hooks": [ { "event": "PreToolUse", "action": { "type": "command", "program": "false", "timeout_ms": 2000 } } ] }"#,
        );
        assert!(deny_set
            .evaluate(HookEvent::PreToolUse, payload(HookEvent::PreToolUse))
            .await
            .is_deny());
        let allow_set = set_from_json(
            r#"{ "hooks": [ { "event": "PreToolUse", "action": { "type": "command", "program": "true", "timeout_ms": 2000 } } ] }"#,
        );
        assert_eq!(
            allow_set
                .evaluate(HookEvent::PreToolUse, payload(HookEvent::PreToolUse))
                .await,
            Decision::Allow
        );
    }

    #[tokio::test]
    async fn evaluate_command_reason_from_stdout() {
        let _env = env_lock();
        // A denying command's first stdout line becomes the human-readable reason.
        let set = set_from_json(
            r#"{ "hooks": [ { "event": "PreToolUse",
                 "action": { "type": "command", "program": "bash", "args": ["-c", "echo policy: no secrets; exit 3"], "timeout_ms": 2000 } } ] }"#,
        );
        match set
            .evaluate(HookEvent::PreToolUse, payload(HookEvent::PreToolUse))
            .await
        {
            Decision::Deny(reason) => assert_eq!(reason, "policy: no secrets"),
            other => panic!("expected deny with reason, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn evaluate_required_command_fails_closed_on_spawn_error() {
        let _env = env_lock();
        // A required hook whose program can't spawn DENIES (fail-closed)…
        let required = set_from_json(
            r#"{ "hooks": [ { "event": "PreToolUse", "action": { "type": "command", "program": "/nonexistent/aish-hook-x", "required": true } } ] }"#,
        );
        assert!(required
            .evaluate(HookEvent::PreToolUse, payload(HookEvent::PreToolUse))
            .await
            .is_deny());
        // …an optional one fails OPEN (allow).
        let optional = set_from_json(
            r#"{ "hooks": [ { "event": "PreToolUse", "action": { "type": "command", "program": "/nonexistent/aish-hook-x", "required": false } } ] }"#,
        );
        assert_eq!(
            optional
                .evaluate(HookEvent::PreToolUse, payload(HookEvent::PreToolUse))
                .await,
            Decision::Allow
        );
    }

    #[tokio::test]
    async fn evaluate_most_restrictive_wins() {
        let _env = env_lock();
        // Two matching rules: the first allows, the second denies → deny overall.
        let set = set_from_json(
            r#"{ "hooks": [
                 { "event": "PreToolUse", "action": { "type": "rule", "deny_if": "never" } },
                 { "event": "PreToolUse", "action": { "type": "rule", "deny_if": "always" } }
               ] }"#,
        );
        assert!(set
            .evaluate(HookEvent::PreToolUse, payload(HookEvent::PreToolUse))
            .await
            .is_deny());
    }

    #[tokio::test]
    async fn evaluate_is_recursion_guarded() {
        let _env = env_lock();
        // SAFETY: single-threaded test; guard is set/unset around the call.
        unsafe { std::env::set_var(RECURSION_GUARD, "1") };
        let set = set_from_json(
            r#"{ "hooks": [ { "event": "PreToolUse", "action": { "type": "rule", "deny_if": "always" } } ] }"#,
        );
        let d = set
            .evaluate(HookEvent::PreToolUse, payload(HookEvent::PreToolUse))
            .await;
        unsafe { std::env::remove_var(RECURSION_GUARD) };
        assert_eq!(d, Decision::Allow, "inside a hook, the gate must not veto");
    }

    // ---- Phase 0.5.2: plugin catalog merge, precedence, provenance --------

    /// Parse a `hooks.json` body into a `Vec<Hook>` (merge-layer test helper).
    fn hooks_from(json: &str) -> Vec<Hook> {
        parse_hooks(json).unwrap()
    }

    #[test]
    fn plugin_merge_tags_source_and_all_observe_fire() {
        // Observe fan-out: a local and a plugin hook on the same event BOTH
        // register (all fire), and each carries its provenance tag.
        let local = hooks_from(
            r#"{ "hooks": [ { "event": "SessionStart", "action": { "type": "command", "program": "u" } } ] }"#,
        );
        let plugin = hooks_from(
            r#"{ "hooks": [ { "event": "SessionStart", "action": { "type": "command", "program": "p" } } ] }"#,
        );
        let set = HookSet::merge_layers(vec![
            (HookSource::Local, local),
            (HookSource::Plugin("demo".into()), plugin),
        ]);
        assert_eq!(set.len(), 2, "observe hooks fan out: both registered");
        let sources: Vec<String> = set.hooks().iter().map(|h| h.source.label()).collect();
        assert!(sources.contains(&"local".to_string()));
        assert!(sources.contains(&"plugin:demo".to_string()));
    }

    #[test]
    fn plugin_override_by_name_prefers_higher_precedence() {
        // A user hook and a plugin hook share a name → user wins, plugin dropped.
        let user = hooks_from(
            r#"{ "hooks": [ { "name": "guard", "event": "PostToolUse", "action": { "type": "command", "program": "user-guard" } } ] }"#,
        );
        let plugin = hooks_from(
            r#"{ "hooks": [ { "name": "guard", "event": "PostToolUse", "action": { "type": "command", "program": "plugin-guard" } } ] }"#,
        );
        let set = HookSet::merge_layers(vec![
            (HookSource::Local, user),
            (HookSource::Plugin("demo".into()), plugin),
        ]);
        assert_eq!(set.len(), 1);
        assert_eq!(set.hooks()[0].source, HookSource::Local);
    }

    #[test]
    fn plugin_disable_via_user_tombstone() {
        // A user tombstone (enabled:false) named like the plugin hook removes it.
        let user = hooks_from(
            r#"{ "hooks": [ { "name": "noisy", "enabled": false, "event": "PostToolUse", "action": { "type": "command", "program": "x" } } ] }"#,
        );
        let plugin = hooks_from(
            r#"{ "hooks": [ { "name": "noisy", "event": "PostToolUse", "action": { "type": "command", "program": "plugin-noisy" } } ] }"#,
        );
        let set = HookSet::merge_layers(vec![
            (HookSource::Local, user),
            (HookSource::Plugin("demo".into()), plugin),
        ]);
        assert!(
            set.is_empty(),
            "tombstone suppresses the plugin hook and registers nothing"
        );
    }

    #[tokio::test]
    async fn plugin_blocking_veto_is_honored_when_alone() {
        let _env = env_lock();
        // A lone plugin blocking entry keeps its veto (no local hook contends).
        let plugin = hooks_from(
            r#"{ "hooks": [ { "event": "PreToolUse", "action": { "type": "rule", "deny_if": "always" } } ] }"#,
        );
        let set = HookSet::merge_layers(vec![(HookSource::Plugin("sec".into()), plugin)]);
        assert!(!set.hooks()[0].observe_only);
        let d = set
            .evaluate(HookEvent::PreToolUse, payload(HookEvent::PreToolUse))
            .await;
        assert!(d.is_deny(), "a lone plugin blocking entry vetoes: {d:?}");
    }

    #[tokio::test]
    async fn plugin_blocking_degrades_when_local_owns_event() {
        let _env = env_lock();
        // Local ALLOWS (never); plugin would DENY (always) but is demoted to
        // observe because a local hook owns the blocking event → allowed.
        let local = hooks_from(
            r#"{ "hooks": [ { "event": "PreToolUse", "action": { "type": "rule", "deny_if": "never" } } ] }"#,
        );
        let plugin = hooks_from(
            r#"{ "hooks": [ { "event": "PreToolUse", "action": { "type": "rule", "deny_if": "always" } } ] }"#,
        );
        let set = HookSet::merge_layers(vec![
            (HookSource::Local, local),
            (HookSource::Plugin("sec".into()), plugin),
        ]);
        let demoted = set.hooks().iter().find(|h| h.source.is_plugin()).unwrap();
        assert!(demoted.observe_only, "plugin entry must be demoted to observe");
        let d = set
            .evaluate(HookEvent::PreToolUse, payload(HookEvent::PreToolUse))
            .await;
        assert_eq!(
            d,
            Decision::Allow,
            "local owns the veto; demoted plugin cannot deny"
        );
    }

    #[test]
    fn plugin_blocking_single_winner_across_two_plugins() {
        // Two plugins on the same blocking event: the first keeps the veto, the
        // second is demoted to observe (one winner per event).
        let a = hooks_from(
            r#"{ "hooks": [ { "event": "PreToolUse", "action": { "type": "rule", "deny_if": "always" } } ] }"#,
        );
        let b = hooks_from(
            r#"{ "hooks": [ { "event": "PreToolUse", "action": { "type": "rule", "deny_if": "always" } } ] }"#,
        );
        let set = HookSet::merge_layers(vec![
            (HookSource::Plugin("a".into()), a),
            (HookSource::Plugin("b".into()), b),
        ]);
        let demoted: Vec<bool> = set.hooks().iter().map(|h| h.observe_only).collect();
        assert_eq!(
            demoted,
            vec![false, true],
            "first plugin keeps veto, second demoted"
        );
    }

    #[test]
    fn load_layered_reads_plugin_fragment_files() {
        let dir = std::env::temp_dir().join(format!("aish-plugmerge-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let frag = dir.join("plugin-hooks.json");
        std::fs::write(
            &frag,
            r#"{ "hooks": [ { "event": "SessionStart", "action": { "type": "command", "program": "p" } } ] }"#,
        )
        .unwrap();
        let set = HookSet::load_layered(
            &[],
            &[PluginHookFragment {
                plugin_id: "demo".into(),
                path: frag,
            }],
        );
        assert_eq!(set.len(), 1);
        assert_eq!(set.hooks()[0].source, HookSource::Plugin("demo".to_string()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn merge_precedence_local_over_plugin_ordering() {
        // Precedence walk is highest-first: user, then project, then plugins.
        // The kept, precedence-ordered set is exactly what `evaluate` iterates.
        let user = hooks_from(
            r#"{ "hooks": [ { "name": "n", "event": "PostToolUse", "action": { "type": "command", "program": "user" } } ] }"#,
        );
        let project = hooks_from(
            r#"{ "hooks": [ { "name": "n", "event": "PostToolUse", "action": { "type": "command", "program": "project" } } ] }"#,
        );
        let plugin = hooks_from(
            r#"{ "hooks": [ { "name": "n", "event": "PostToolUse", "action": { "type": "command", "program": "plugin" } } ] }"#,
        );
        let set = HookSet::merge_layers(vec![
            (HookSource::Local, user),
            (HookSource::Local, project),
            (HookSource::Plugin("p".into()), plugin),
        ]);
        // All three share name "n" → only the highest-precedence (user) survives.
        assert_eq!(set.len(), 1);
        match &set.hooks()[0].action {
            Action::Command { program, .. } => assert_eq!(program, "user"),
            _ => panic!("expected command"),
        }
    }
}
