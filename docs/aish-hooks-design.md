# aish Hook System — Design

Status: **Draft for review** · Owner: aish core · Scope: design only (no implementation)

A hook system lets users and operators run their own logic — a command, a
script, or an inline rule — at well-defined moments in aish's lifecycle:
session boundaries, every prompt and turn, every tool call and permission
check, file changes, context compaction, background-worker lifecycle, and more.
This document maps every lifecycle boundary in the current codebase, picks the
ones where a hook can fire **without adding latency to the hot path**, and
specifies the hook catalog, payload format, dispatch mechanism, trust model,
and a phased implementation plan.

---

## 1. Goals

1. **Extensibility.** A user can observe or influence aish at any meaningful
   boundary without patching the binary — audit logging, custom permission
   policy, formatters/linters on file writes, notifications on long jobs,
   metrics, secret-scanning, etc.
2. **Predictability.** Each hook has a documented firing point, a stable
   payload schema, a defined ordering, and explicit semantics for what happens
   to its return value (observe-only vs. able to block/mutate).
3. **Security.** Hooks are local code that runs with the user's privileges.
   The trust model, the data exposed to a hook, and the blast radius of a
   misbehaving hook are all explicit and conservative by default.
4. **Zero overhead when unused.** If no hook is registered for an event, the
   event site costs at most one cheap check (an empty-map/`is_empty` test). No
   process spawns, no JSON serialization, no allocation on the hot path.
5. **Natural integration.** Hooks fire at boundaries that already exist as
   clean seams in `engine.rs`, `tools.rs`, `coordinator.rs`, `repl.rs`,
   `session.rs`, and `main.rs` — they do not require restructuring those paths.

### 1.1 Non-goals

- **Not a plugin/ABI system.** Hooks are out-of-process programs (or inline
  rules), not dynamically-loaded Rust. No `dlopen`, no stable internal ABI.
- **Not a scripting language.** aish is "no shell underneath"; hooks do not get
  a shell either. A hook is one program invoked fork/exec with a JSON payload,
  exactly like every other program aish spawns.
- **Not a replacement for the permission gate.** Hooks can *augment* the
  permission model (`PreToolUse`/`PermissionRequest` may veto), but the
  built-in mode gates (`paranoid`/`careful`/`normal`/`yolo`) remain the
  baseline policy. A hook can only make a decision *stricter*, never bypass a
  gate that would otherwise prompt or refuse (see §6).
- **Not synchronous on latency-critical UI paths.** Hooks never animate, never
  block the line editor's repaint, and never run inside the route-preview
  highlighter (which repaints per keystroke).
- **Not retried.** Hook dispatch is best-effort with a hard timeout. aish does
  not build a durable hook queue in v1 (deferred to a later increment).

### 1.2 Relationship to the plugin system

`docs/PLUGIN_SYSTEM_DESIGN.md` introduces a narrow, plugin-scoped notion of
"hooks" — four shell scripts (`on_init`, `on_shell_ready`,
`on_webhook_url_changed`, `on_shutdown`) that live in a plugin directory and run
at startup/shutdown. This document **generalizes and subsumes** that idea into a
first-class lifecycle-hook subsystem:

- The plugin doc's four scripts map onto a strict subset of this catalog:
  `on_init`/`on_shell_ready` → `SessionStart`/`McpServersReady`,
  `on_shutdown` → `SessionEnd`, `on_webhook_url_changed` → a plugin-specific
  observe hook. They become *registrations against this system* rather than a
  separate mechanism.
- A plugin's `plugin.json` `hooks` array is just **another source of hook
  registrations** (merged into the `HookSet` alongside `~/.aish/hooks.json` and
  the project file), so plugins and standalone users share one dispatcher, one
  payload schema, one trust model, and one `:hooks` management surface.
- This design adds the ~35 finer-grained, security-relevant boundaries
  (every tool call, permission check, file write, turn, compaction, coordinator
  phase) the plugin doc's startup/shutdown-only scripts never reach.

Net: build this lifecycle-hook core first; the plugin system registers through
it instead of carrying its own parallel hook runner.

---

## 2. Use cases (motivating the catalog)

| # | Want | Hook(s) used | Mode |
|---|------|--------------|------|
| 1 | Audit every command/tool an agent runs to a SIEM | `PreToolUse`, `PostToolUse`, `DirectCommandRun` | async, observe |
| 2 | Block writes outside the project tree regardless of mode | `PreToolUse`, `PermissionRequest` | **sync, blocking** |
| 3 | Run `rustfmt`/`prettier` on every file aish writes | `FileChanged` | async, observe |
| 4 | Desktop/Slack notification when a background coordinator finishes | `WorkerStop`, `TurnEnd` | async, observe |
| 5 | Inject project rules/standards into every model turn | `UserPromptSubmit` | **sync, mutating** |
| 6 | Secret-scan tool args before they leave the box | `PreToolUse` | **sync, blocking** |
| 7 | Track token/credit spend per session in an external ledger | `TurnEnd`, `PreCompact`, `BatchFanOut` | async, observe |
| 8 | Refuse a deploy unless a CI gate passed | `PreToolUse` (matched on `gh`/`kubectl`) | **sync, blocking** |
| 9 | Snapshot history before compaction to long-term storage | `PreCompact` | async, observe |
| 10 | Custom `cd` side effects (e.g. `direnv`-style env loading) | `CwdChanged` | sync, **mutating env** |
| 11 | Alert on a loop-guard trip / flagged-for-operator coordinator | `LoopGuardTripped`, `CoordinatorPhaseChanged` | async, observe |
| 12 | Forbid the agent from touching the default branch beyond the built-in guard | `PreToolUse` | sync, blocking |

The split is the design's backbone: most hooks are **observe-only and async**
(off the hot path), a small, explicitly-named set is **synchronous and able to
influence** the action (block or mutate), and those carry a tight timeout.

---

## 3. Lifecycle map — every major boundary in the current code

This is the result of reading `main.rs`, `repl.rs`, `engine.rs`, `tools.rs`,
`session.rs`, `coordinator.rs`, `worker.rs`, `rc.rs`, and `update.rs`. Each row
is a real seam where a hook could fire. The "hot path?" column drives the
sync/async decision in §5.

| Boundary | Where (file · symbol) | Natural hook | Hot path? |
|---|---|---|---|
| Process start, args parsed | `main.rs::main` | `SessionStart` | no (once) |
| rc/profile/config loaded | `main.rs` (after `rc::load`, `load_login_profiles`) | `InstructionsLoaded` | no (once) |
| MCP servers connected | `repl.rs::install_mcp_if_ready`; `main.rs` one-shot connect | `McpServersReady` | no (async arrival) |
| Self-update discovered / applied | `repl.rs` (`update_rx`), `update.rs::perform` | `UpdateAvailable`, `UpdateApplied` | no |
| REPL prompt drawn / line read | `repl.rs::run` loop, `editor.read_line` | (none — too hot) | **yes (per keystroke repaint)** |
| Route decided (direct/model/auto) | `repl.rs::split_route` + `dispatch` predicates | `PromptRouteDecided` | warm (per line) |
| Direct shell command run | `repl.rs::dispatch` → `tools::run_on_tty` | `DirectCommandRun` | warm |
| `cd` / `change_dir` | `repl.rs::builtin_cd`; `tools.rs::change_dir` | `CwdChanged` | warm |
| Mode change | `repl.rs::handle_colon` (`:mode`,`:yolo`) | `ModeChanged` | no |
| Backend/model change | `repl.rs` (`:backend`,`:model`) | `BackendChanged` | no |
| **Model turn begins** | `engine.rs::run_turn` (after `maybe_compact`, skill hint, `history.push(user)`) | `UserPromptSubmit` | **yes (per turn)** |
| Skill matched / suggested | `engine.rs::run_turn` (`skill_match::hint`, `maybe_recommend_skill`) | `SkillMatched` | per turn |
| Per-round model completion | `engine.rs::run_turn` loop (`backend.complete`) | (folded into Turn*) | **yes (per round)** |
| **Pre tool execution** | `engine.rs::run_turn` loop → before `tools::execute(call, …)` | `PreToolUse` | **yes (per tool call)** |
| **Permission prompt** | `tools.rs::gate`/`gate_path`/`gate_delete` → `confirm()` | `PermissionRequest` | per gated call |
| Permission denied | same, `Decision::Deny` branch | `PermissionDenied` | per denied call |
| Protected-branch block | `tools.rs::run_program` git default-branch guard | `PreToolUse` (policy) / `GitProtectedBranchBlocked` | rare |
| **Post tool execution** | `engine.rs::run_turn` after `tools::execute` returns | `PostToolUse` | **yes (per tool call)** |
| Tool failed | same, `result.is_error == true` | `PostToolUseFailure` | per failing call |
| File mutated | `tools.rs::write_file`/`edit_file`/`append_file`/`copy_file`/`rename_file` | `FileChanged` | per write |
| Memory written / recalled | `tools.rs::remember`/`recall`; `db.rs` | `MemoryStored` | per call |
| Escalation consult | `tools.rs::escalate` | `EscalationRequested` | rare |
| Background job (run_program bg) start/finish | `tools.rs::spawn_background`, waiter task | `BackgroundJobStart/Stop` | per job |
| Loop guard trips | `engine.rs::run_turn` (`loopguard::RepeatAction`, `BudgetPhase`, `ExitReason`) | `LoopGuardTripped` | rare |
| **Turn ends (final answer)** | `engine.rs::run_turn` returns `Ok(text)` | `TurnEnd` | **yes (per turn)** |
| Turn errored / aborted | `engine.rs::run_turn` `Err`; `repl.rs` Ctrl-C `aborted` branch | `TurnEndFailure` | per turn |
| **Pre-compaction** | `engine.rs::maybe_compact` (before `apply_compaction`) | `PreCompact` | occasional |
| **Post-compaction** | `engine.rs::maybe_compact` (after offload + apply) | `PostCompact` | occasional |
| **Coordinator/worker spawn** | `worker.rs::spawn` → `run_worker` re-exec | `WorkerStart` | per worker |
| Coordinator phase transition | `coordinator.rs::drive` (`set_phase`/`set_done`/`set_failed`) | `CoordinatorPhaseChanged` | per round |
| Operator message folded (`:tell`) | `coordinator.rs::fold_operator_messages` | `OperatorMessageReceived` | per round |
| Batch fan-out / collect | `coordinator.rs::drive` (`await_batches_with_heartbeat`) | `BatchFanOut` | occasional |
| Worker-exit disposition | `coordinator.rs::drive` (`classify_disposition`) | `WorkerExitEvaluated` | per recovery |
| **Coordinator/worker finishes** | `worker.rs::run_worker` `set_done`/`set_failed`; `coordinator.rs::run_coordinator` outcome | `WorkerStop` | per worker |
| Rehydrate/reattach at startup | `coordinator.rs::rehydrate`, `batch::rehydrate` | (folded into `SessionStart`) | once |
| Session ends | `repl.rs::run` exit (`hangup_jobs_on_exit`, `save_history`) | `SessionEnd` | once |

### 3.1 Subtle boundaries worth calling out

These are easy to miss but matter for a *complete* hook system:

- **Agent-context switching (interactive ↔ nested coordinator).** A background
  coordinator is a re-exec'd `aish --coordinator` with `session.nested == true`
  (see `session.rs`, `worker_command` sets `AISH_COORDINATOR=1`). The *same*
  hook sites fire inside a coordinator. Every payload therefore carries an
  `agent` descriptor (`interactive` | `coordinator` | `goal` | `script` |
  `oneshot`) and the launching session id, so a consumer can tell a human turn
  from an autonomous one. Hooks that are noisy in autonomous mode can scope
  themselves to `agent == "interactive"`.
- **Mode transitions mid-session.** `:yolo`, `:mode`, and the `--yolo`/`--mode`
  flags change the confirmation posture. A `ModeChanged` hook lets a policy
  layer veto or log a downgrade to `yolo` (a real audit concern).
- **Error-handling boundaries.** Three distinct failure exits exist and must be
  separable: a tool error (`PostToolUseFailure`, the tool ran and returned
  `is_error`), a turn error (`TurnEndFailure`, the backend/loop failed), and a
  Ctrl-C abort (`repl.rs` rolls history back to `pre_len`). They are different
  events with different payloads, not one "error" hook.
- **The loop guard** (`engine.rs` + `loopguard.rs`) has three escalating
  states — repeat-detected, forced-summarize, budget-exhausted — that today only
  print a dim banner. `LoopGuardTripped` surfaces them for alerting.
- **The prefill-continuation path** (`engine.rs`, `continuing` flag): a single
  logical turn can span several `backend.complete` rounds. `TurnEnd` fires once
  per *logical* turn (when `run_turn` returns), not per round — otherwise a
  consumer double-counts.
- **Compaction is silent today.** `maybe_compact` offloads the oldest history to
  the SQLite offloads table. `PreCompact`/`PostCompact` are the only chance for
  a consumer to snapshot or veto that lossy transition.
- **The route decision is the shell-vs-model fork** unique to aish. `dispatch`
  may run a real command directly (never touching the model). `PromptRouteDecided`
  + `DirectCommandRun` make that path observable — a normal agent-tool hook
  would miss every directly-dispatched command.
- **`CwdChanged` has two sources:** the `cd` builtin (`repl.rs::builtin_cd`) and
  the `change_dir` tool (`tools.rs`). Both must fire it, or a consumer's view of
  cwd drifts.

---

## 4. Hook catalog

Each hook has: **timing** (exactly when it fires), **payload** (the data
fields), **kind** (observe-only vs. blocking vs. mutating), and **error
semantics** (what aish does when the hook fails/times out). Common envelope
fields (§4.0) are omitted from each entry.

### 4.0 Common envelope

Every hook receives a JSON object on **stdin** with these envelope fields, plus
an event-specific `data` object:

```json
{
  "hook": "PreToolUse",
  "schema": 1,
  "event_id": "evt_9f3c…",        // unique per firing (idempotency key)
  "ts": "2025-06-01T12:00:00Z",
  "session_id": "uuid",            // stable per process; coordinators adopt the launcher's
  "session_name": "myproj",        // :rename label, or null
  "agent": "interactive",          // interactive | coordinator | goal | script | oneshot
  "nested": false,                 // true inside a background coordinator
  "cwd": "/home/me/proj",
  "mode": "normal",                // paranoid | careful | normal | yolo
  "backend": "claude",
  "model": "claude-haiku-4-5",
  "run_id": null,                  // coordinator run id when agent != interactive
  "data": { … }                    // event-specific (below)
}
```

The hook's **response** (for sync hooks) is a JSON object on **stdout**:

```json
{
  "decision": "allow",     // allow | deny | ask   (blocking hooks only)
  "reason": "…",           // shown to user / fed to model on deny
  "mutate": { … },         // event-specific patch (mutating hooks only)
  "stop": false            // request the run stop after this event (advisory)
}
```

An empty stdout, exit 0, is treated as `{"decision":"allow"}` — the common case
for an observe-only hook that just logged something. Exit code conventions are
in §5.3.

---

### Session & process lifecycle

#### `SessionStart`
- **Timing:** once, in `main.rs::main` after the backend, db, MCP (one-shot),
  and rehydrate steps complete — i.e. when the session is fully constructed but
  before the first prompt / first turn.
- **Payload `data`:** `{ login, interactive, argv, host_info, skills: [names], mcp_servers: [names], rehydrated: { coordinators, batches } }`.
- **Kind:** observe-only (async, but awaited before the first prompt so a setup
  hook can finish — see §5.2 "startup barrier").
- **Errors:** failure logged dim, never aborts startup.

#### `SessionEnd`
- **Timing:** `repl.rs::run` on loop exit, before `hangup_jobs_on_exit` /
  `save_history`. Also fires on `-c`/script completion.
- **Payload:** `{ reason: "eof" | "exit" | "signal", turns, duration_secs, last_status }`.
- **Kind:** observe-only, async with a short drain timeout (we are exiting).
- **Errors:** swallowed; exit is never blocked by a hook.

#### `InstructionsLoaded`
- **Timing:** `main.rs` after `rc::load` (+ `load_login_profiles` for login
  shells), before the backend is built. Re-fires on `:skill add/remove` reloads
  (`session.rs::reload_skills`) and on a live MCP connect (`McpServersReady`
  carries the delta).
- **Payload:** `{ source: "aishrc" | "profile" | "skill" | "mcp", aliases: N, exports: [keys only], skills: [names] }`. **Export values are redacted** (keys only) — `~/.aishrc` holds credentials.
- **Kind:** observe-only. (A future iteration could let this hook *contribute*
  system-prompt text; v1 keeps it observe-only to protect the prompt-cache
  prefix — see §8.)
- **Errors:** logged, non-fatal.

#### `McpServersReady`
- **Timing:** `repl.rs::install_mcp_if_ready` when the deferred connect lands.
- **Payload:** `{ servers: [{name, tools: N, skills: [names]}] }`.
- **Kind:** observe-only, async.

#### `ModeChanged`, `BackendChanged`
- **Timing:** the `:mode`/`:yolo` and `:backend`/`:model` colon handlers.
- **Payload:** `{ from, to }`.
- **Kind:** `ModeChanged` may be **blocking** (`deny` keeps the old mode) so an
  org policy can forbid `yolo`; `BackendChanged` is observe-only.
- **Errors:** on timeout/failure, the change proceeds (fail-open) unless the
  hook is marked `required` (see §6.3).

#### `UpdateAvailable`, `UpdateApplied`
- **Timing:** `repl.rs` update notice; `update.rs::perform` after a successful
  binary swap.
- **Payload:** `{ from_version, to_version }`.
- **Kind:** observe-only.

---

### Turn lifecycle

#### `UserPromptSubmit`
- **Timing:** `engine.rs::run_turn`, **after** `maybe_compact` and skill-hint
  injection, **after** the user message is pushed to history, **before**
  `backend.prepare()` / the first `backend.complete`. Fires once per logical
  turn (interactive turn, each coordinator round, each `escalate` is *not* a
  turn — it has its own hook).
- **Payload:** `{ prompt, seeded: bool, skill_hint: name|null, history_len, context_used, iteration_budget }`.
- **Kind:** **sync, mutating.** A hook may return `mutate.prepend` /
  `mutate.append` text folded into the turn input (the same mechanism the skill
  hint and operator interjection already use — they prepend to `input`). It may
  also `deny` to refuse the turn (the prompt is dropped; history rolled back to
  `pre_len`).
- **Errors:** timeout/failure → fail-open (turn proceeds unmutated), logged. A
  `required` hook that fails → turn refused with a clear message.
- **Why here:** this is the single clean seam (`let input = …; session.history.push(Msg::user(input))`) where prompt augmentation already happens, so injecting hook output is a one-line fold, prompt-cache-safe (the mutation goes into turn input, never the cached system prompt).

#### `TurnEnd`
- **Timing:** `engine.rs::run_turn` just before `return Ok(text)` — the
  **logical** turn end (after any prefill-continuation rounds merged). In the
  REPL this is also where `db.record("output", …)` happens.
- **Payload:** `{ reply, rounds, tool_calls: N, context_used, usage: {input,output,total}|null, exit_reason: "normal"|"forced-summarize"|"loop-detected"|"budget-exhausted" }`.
- **Kind:** observe-only, async (does not delay showing the reply). A `stop:true`
  response is advisory for coordinators (requests no further rounds).
- **Errors:** swallowed.

#### `TurnEndFailure`
- **Timing:** `run_turn` returns `Err` (backend/loop failure), OR the REPL
  Ctrl-C abort path (`aborted` branch, history truncated to `pre_len`).
- **Payload:** `{ error, aborted: bool, rounds, partial_reply }`.
- **Kind:** observe-only, async.

#### `SkillMatched`
- **Timing:** `engine.rs::run_turn` when `skill_match::hint` matches an
  installed skill or `maybe_recommend_skill` suggests one.
- **Payload:** `{ kind: "matched" | "suggested", skill, reference }`.
- **Kind:** observe-only.

#### `LoopGuardTripped`
- **Timing:** `engine.rs::run_turn` when `loopguard` fires a non-`Allow` repeat
  action, a `ForceSummarize` budget phase, or a tagged `ExitReason`.
- **Payload:** `{ kind: "repeat" | "forced-summarize" | "budget-exhausted", call: desc|null, count, iteration }`.
- **Kind:** observe-only, async (must not add latency to an already-degrading
  turn).

#### `EscalationRequested`
- **Timing:** `tools.rs::escalate` (weak frontend → strong model consult).
- **Payload:** `{ provider, model, task_preview }` (task truncated).
- **Kind:** observe-only.

---

### Tool & permission lifecycle (the hot, security-critical path)

#### `PreToolUse`
- **Timing:** `engine.rs::run_turn` inside the tool loop, **after** the
  loop/repeat guard and turn-audit replay check, **before** `tools::execute`.
  Fires for every tool call (local, MCP, escalate) the model emits.
- **Payload:** `{ tool, args, call_id, glyph, is_mcp, repeat_count }`. Args are
  passed verbatim (a secret-scanning hook needs them) — see §6.4 for the
  redaction toggle.
- **Kind:** **sync, blocking + mutating.** Response:
  - `deny` → the tool is **not executed**; the model receives a synthetic
    `ToolResult` carrying `reason` (mirrors the existing "user declined" path).
  - `ask` → force a permission prompt even if the mode/allowlist would not have
    (escalate the gate).
  - `mutate.args` → replace the call args before execution (e.g. add `--dry-run`,
    rewrite a path). Use sparingly; logged.
- **Ordering:** runs *before* the built-in permission gate, so a hook `deny`
  short-circuits without prompting; a hook `ask` *adds* a prompt the mode would
  have skipped. A hook can only make a call **more** restricted (§6.1).
- **Errors:** timeout/failure → fail-**closed** for a `required` hook (call
  denied), fail-**open** for a normal hook (call proceeds, prompt logged). This
  is the one place the default leans toward safety being configurable.

#### `PermissionRequest`
- **Timing:** inside `tools.rs::gate` / `gate_path` / `gate_delete`, right
  before `confirm(prompt)` is called (i.e. the mode decided a prompt is needed
  and the allowlist didn't already pass).
- **Payload:** `{ tool, args, perm: "read"|"write"|"delete"|null, path: …|null, prompt }`.
- **Kind:** **sync, blocking.** A hook may pre-answer: `allow` (auto-`AllowOnce`),
  `deny`, or `ask` (fall through to the human `confirm_tty`). This lets an
  external policy engine answer prompts in an otherwise-interactive session, and
  is the seam non-interactive runs (`script.rs`, coordinators) use to apply
  policy instead of blanket-allowing.
- **Errors:** fail-open to the normal `confirm` path (the human still decides);
  `required` hook failure → deny.

#### `PermissionDenied`
- **Timing:** any gate returns `Deny` (human said no, or a hook denied).
- **Payload:** `{ tool, args, perm, path, source: "user"|"hook"|"policy" }`.
- **Kind:** observe-only, async.

#### `PostToolUse`
- **Timing:** `engine.rs::run_turn` immediately after `tools::execute` returns,
  before the result is pushed to history / journaled.
- **Payload:** `{ tool, args, call_id, is_error, duration_ms, result_preview, structured: bool }`. Result text truncated to a cap (configurable; default 4 KB).
- **Kind:** observe-only, async. (A future increment may allow result mutation;
  v1 keeps it observe-only — mutating a tool result is a footgun.)
- **Errors:** swallowed.

#### `PostToolUseFailure`
- **Timing:** same site, only when `result.is_error == true`.
- **Payload:** `{ tool, args, call_id, error }`.
- **Kind:** observe-only, async.

#### `FileChanged`
- **Timing:** after a successful `write_file`, `edit_file`, `append_file`,
  `copy_file`, or `rename_file` in `tools.rs`. Distinct from `PostToolUse` so a
  formatter/linter consumer can subscribe to *just file mutations* without
  filtering every tool.
- **Payload:** `{ op: "write"|"edit"|"append"|"copy"|"rename", path, dst: …|null, bytes, created: bool }`.
- **Kind:** observe-only, async (a `rustfmt` hook runs after the write; it does
  not block the turn). A `mutate`/blocking variant is explicitly out of scope
  for v1 — re-writing a file under the agent invites races.
- **Note:** fires on the **resolved absolute path** (`tools.rs::resolve`), so a
  hook sees the real target regardless of cwd-relative input.

#### `DirectCommandRun`
- **Timing:** `repl.rs::dispatch` after a real program/pipeline runs via
  `run_on_tty` / `pipeline::run` (the shell-first path that never touches the
  model).
- **Payload:** `{ argv, pipeline: bool, exit_code, duration_ms }`.
- **Kind:** observe-only, async. (Pre-execution blocking of *directly typed*
  commands is intentionally **not** offered — aish trusts what the human types
  at the prompt; agent-driven commands go through `PreToolUse`.)

#### `MemoryStored`
- **Timing:** `tools.rs::remember` (and context offload in `maybe_compact`).
- **Payload:** `{ kind: "remember"|"offload", tags, bytes }`. Content redacted
  to length by default (memories can be sensitive).
- **Kind:** observe-only.

---

### Environment & navigation

#### `CwdChanged`
- **Timing:** `repl.rs::builtin_cd` and `tools.rs::change_dir`, after the cwd is
  successfully updated on the session.
- **Payload:** `{ from, to, source: "builtin"|"tool" }`.
- **Kind:** **sync, mutating env (narrow).** A hook may return
  `mutate.env: {KEY: VAL, …}` merged into the session env (the `direnv`/per-dir
  credential use case). It may **not** veto the `cd` (POSIX `cd` is the user's
  prerogative).
- **Errors:** fail-open (cd stands, env unchanged), logged.

---

### Background work lifecycle

#### `WorkerStart`
- **Timing:** `worker.rs::spawn` / start of `run_worker`, just before/after the
  child `aish --coordinator` is exec'd (and after worktree isolation is set up,
  so the branch is known).
- **Payload:** `{ worker_id, task, isolate, base, branch: …|null, backend, model, launch_session_id }`.
- **Kind:** observe-only, async.

#### `WorkerStop`
- **Timing:** `worker.rs::run_worker` on `set_done`/`set_failed`; also the
  headless `coordinator.rs::run_coordinator` terminal outcome.
- **Payload:** `{ worker_id, status: "done"|"failed", rounds, branch: …|null, result_preview, error: …|null, duration_secs }`.
- **Kind:** observe-only, async. The headline "notify me when the long job
  finishes" use case.

#### `CoordinatorPhaseChanged`
- **Timing:** `coordinator.rs::drive` at each `set_phase`/`set_done`/`set_failed`.
- **Payload:** `{ run_id, from, to, round }` where phases are
  `coordinating|awaiting_batch|done|failed`.
- **Kind:** observe-only, async.

#### `OperatorMessageReceived`
- **Timing:** `coordinator.rs::fold_operator_messages` when a `:tell` message is
  folded into a round.
- **Payload:** `{ run_id, count, messages: [text] }`.
- **Kind:** observe-only.

#### `BatchFanOut`
- **Timing:** `coordinator.rs::drive` when a round fans sub-work to the Batches
  API (and on collect).
- **Payload:** `{ run_id, phase: "fanout"|"collected", batch_count }`.
- **Kind:** observe-only, async.

#### `WorkerExitEvaluated`
- **Timing:** `coordinator.rs::drive` after `classify_disposition` picks
  resume/nudge/flag-operator.
- **Payload:** `{ run_id, exit_reason, disposition, auto_recoveries }`.
- **Kind:** observe-only; a `stop:true` response advises the coordinator to flag
  the operator instead of auto-recovering.

#### `BackgroundJobStart`, `BackgroundJobStop`
- **Timing:** `tools.rs::spawn_background` and the waiter task that calls
  `job.finish`.
- **Payload:** `{ job_id, argv, status, summary }`.
- **Kind:** observe-only, async.

---

### Context lifecycle

#### `PreCompact`
- **Timing:** `engine.rs::maybe_compact`, after `plan_compaction` produced a
  plan, **before** `apply_compaction` mutates history / offloads to SQLite.
- **Payload:** `{ context_used, window, threshold_pct, dropped_msgs, keep_recent }`.
- **Kind:** **sync, blocking (veto-only).** A hook may `deny` to skip this
  compaction (e.g. a consumer that wants to snapshot first and isn't ready, or a
  policy that forbids lossy compaction in a forensic session). It may not mutate
  the plan in v1.
- **Errors:** fail-open (compaction proceeds), logged.

#### `PostCompact`
- **Timing:** `engine.rs::maybe_compact` after offload + `apply_compaction`.
- **Payload:** `{ dropped_msgs, offload_id, new_context_used }`.
- **Kind:** observe-only, async.

---

### 4.1 Recommended additional hooks (beyond the requested list)

The request listed: `SessionStart`, `SessionEnd`, `InstructionsLoaded`,
`UserPromptSubmit`, `PreToolUse`, `PostToolUse`, `PermissionRequest`,
`PermissionDenied`, `PostToolUseFailure`, `WorkerStart`, `WorkerStop`,
`TurnEnd`, `TurnEndFailure`, `PreCompact`, `PostCompact`, `CwdChanged`,
`FileChanged`. On top of those, this design **recommends adding** the following,
each justified by a real seam unique to aish's architecture:

| # | New hook | Why it's needed (and why a generic hook set would miss it) |
|---|----------|------------------------------------------------------------|
| 1 | `PromptRouteDecided` | aish's shell-first router (`dispatch`) decides *direct vs. model* per line. Without this, a consumer can't see (or correct) routing — the defining behavior of an AI shell. |
| 2 | `DirectCommandRun` | Commands dispatched directly (`run_on_tty`/pipelines) never become tool calls, so `PreToolUse`/`PostToolUse` would miss every real shell command the user runs. Needed for complete audit. |
| 3 | `ModeChanged` | `:yolo`/`:mode` silently change the safety posture mid-session. An org-policy/audit consumer must see (and optionally block) a downgrade to `yolo`. |
| 4 | `CoordinatorPhaseChanged` | The durable coordinator state machine (`coordinating→awaiting_batch→done/failed`) is the heartbeat of background work; phase transitions are where progress/stall is observable. |
| 5 | `LoopGuardTripped` | The repeat/forced-summarize/budget-exhausted guards today only print a dim line. Surfacing them lets ops alert on a wedged/looping agent before it burns its budget. |
| 6 | `OperatorMessageReceived` | The `:tell` mid-flight steering channel is invisible to any tool/turn hook; a consumer tracking human-in-the-loop interventions needs it. |
| 7 | `McpServersReady` | MCP connects *asynchronously after* `SessionStart` (deferred off the startup critical path). A consumer that depends on MCP tools must know when they actually arrive. |
| 8 | `EscalationRequested` | The weak→strong model consult is a cost/governance event distinct from a normal turn or tool call — worth its own signal for spend tracking. |
| 9 | `BackgroundJobStart`/`BackgroundJobStop` | `run_program background:true` jobs are a separate lifecycle from coordinators/turns; long watchers/servers deserve start/stop signals. |
| 10 | `UpdateAvailable`/`UpdateApplied` | Fleet operators want to know when a box self-upgrades the aish binary under it. |

(`SkillMatched`, `BatchFanOut`, `WorkerExitEvaluated`, `MemoryStored`,
`GitProtectedBranchBlocked` are catalogued above as lower-priority "nice to
have" observe-only signals; the ten above are the recommended core additions.)

---

## 5. Dispatch mechanism

### 5.1 What a hook *is*

A hook registration is `(event, matcher, command, options)`:

- **event** — one of the catalog names (or `*` for all).
- **matcher** — optional filter so a hook only fires on relevant events:
  - `tool: "run_program"` / `tool: "mcp__*"` (glob) for tool events,
  - `program: "git"` / `program_in: [rm, kubectl]` for `run_program`/direct,
  - `path_glob: "src/**/*.rs"` for `FileChanged`,
  - `mode: "yolo"`, `agent: "coordinator"`, etc.
  Matching is done **in-process** against the already-built payload, so a
  non-matching event costs no spawn.
- **command** — `program` + `args` (argv array, "no shell underneath" — exactly
  like every aish tool). The JSON payload arrives on **stdin**; the response (for
  sync hooks) is read from **stdout**.
- **options** — `{ kind: "observe"|"sync", timeout_ms, required: bool, redact: bool }`.

There is also an **inline rule** form for the common allow/deny case that needs
no external process (e.g. "deny `write_file` outside cwd") — evaluated entirely
in-process, zero spawn. This keeps the cheapest, most common policies free.

### 5.2 Sync vs. async (the latency contract)

The single most important rule: **only events explicitly marked "sync" in §4
ever block aish, and only those carry a timeout the call waits on.** Everything
else is fire-and-forget.

- **Observe-only (async).** aish serializes the payload, spawns the hook
  detached onto the tokio runtime, and moves on. Output is ignored except for a
  dim failure log. These never appear on a latency budget. `PostToolUse`,
  `TurnEnd`, `FileChanged`, all the worker/coordinator/job hooks, etc.
- **Sync (blocking, bounded).** `PreToolUse`, `PermissionRequest`,
  `UserPromptSubmit`, `ModeChanged`, `CwdChanged`, `PreCompact`. aish spawns the
  hook, writes the payload, and **awaits its stdout up to `timeout_ms`** (default
  **2000 ms**, max 30 s). On timeout the child is killed and the fail-open /
  fail-closed policy (§6.3) applies. Sync hooks run **sequentially** in
  registration order so a later hook sees an earlier hook's decision (the
  decision is threaded: once any hook says `deny`, later hooks for that event are
  skipped).
- **Startup barrier.** `SessionStart` and `InstructionsLoaded` are awaited (with
  a generous timeout) before the first prompt so a setup hook completes, but they
  cannot *block* startup beyond the timeout.

### 5.3 Invocation order & determinism

1. For one event, hooks fire in **registration order** (config order; user-scope
   then project-scope, matching the `.mcp.json` precedence convention).
2. **Sync** hooks for an event are sequential; **async** hooks are spawned
   concurrently (order among them is not significant — they can't influence the
   action).
3. A sync hook's decision composes by **most-restrictive-wins**: `deny` beats
   `ask` beats `allow`. The first `deny` short-circuits the remaining sync hooks
   for that event.
4. `mutate` patches from multiple sync hooks apply in order (last write wins per
   key); the merged result is what aish uses.
5. **Exit codes** (for hooks that don't emit JSON): `0` = allow; `2` = deny
   (stderr becomes `reason`); any other non-zero = error (treated per
   fail-open/closed). This mirrors common hook conventions so a one-line shell
   guard works without emitting JSON.

### 5.4 Where dispatch lives in the code

A new `hooks` module owns: config load (§7), the in-process `HookSet`
(event → matchers → registrations), payload structs, and two entry points:

```rust
// async, fire-and-forget; returns immediately. No-op when no hook matches.
hooks.emit(event, &payload, session);

// sync, awaited; returns the composed Decision/mutation. No-op (Allow) when
// no hook matches — one is_empty() check on the hot path.
let outcome = hooks.evaluate(event, &payload, session, timeout).await;
```

The `HookSet` is built once at startup (and rebuilt on a `:hooks reload`),
stored on `Session` next to `mcp`/`db`. The hot-path guarantee is that both
entry points start with `if self.is_empty_for(event) { return Allow }` — a hash
lookup, no allocation, no spawn. Payload construction itself is **lazy**: aish
only builds the JSON when at least one hook matches the event (a closure/builder
is passed, not a pre-serialized blob), so an unused event never pays
serialization cost either.

### 5.5 Integration points (one line each, no path restructuring)

- `engine.rs::run_turn`: `evaluate(UserPromptSubmit)` after input is assembled;
  `evaluate(PreToolUse)` before `tools::execute`; `emit(PostToolUse[Failure])`
  after; `emit(TurnEnd[Failure])` at the returns; `emit(LoopGuardTripped)` in
  the guard branches.
- `engine.rs::maybe_compact`: `evaluate(PreCompact)` before apply, `emit(PostCompact)` after.
- `tools.rs::gate*`: `evaluate(PermissionRequest)` before `confirm`,
  `emit(PermissionDenied)` on deny; `emit(FileChanged)` in the file ops;
  `emit(CwdChanged)` (also in `repl.rs::builtin_cd`); `emit(EscalationRequested)`,
  `emit(MemoryStored)`, `emit(BackgroundJobStart/Stop)`.
- `repl.rs`: `emit(PromptRouteDecided)`/`emit(DirectCommandRun)` in/after
  `dispatch`; `emit(ModeChanged)`/`emit(BackendChanged)` in colon handlers;
  `emit(McpServersReady)` in `install_mcp_if_ready`; `emit(SessionEnd)` at exit;
  `emit(TurnEndFailure)` in the Ctrl-C abort branch.
- `coordinator.rs::drive`: `emit(CoordinatorPhaseChanged)` at each set_phase;
  `emit(OperatorMessageReceived)`, `emit(BatchFanOut)`, `emit(WorkerExitEvaluated)`.
- `worker.rs`: `emit(WorkerStart)` in `spawn`/`run_worker`, `emit(WorkerStop)` at terminal.
- `main.rs`: `emit(SessionStart)`, `emit(InstructionsLoaded)`, `emit(UpdateApplied)`.

Each is a single call guarded by the empty-set fast path; none changes existing
control flow when no hook is registered.

---

## 6. Security & trust model

Hooks are **local code that runs as the user**, with the same privileges aish
itself has. The model is therefore "trusted author, untrusted inputs": the
person who writes a hook config is trusted (it's their machine), but the *data
flowing through* a hook (model-chosen tool args, file contents) is not, and a
*misbehaving or hostile-input-influenced* hook must have a bounded blast radius.

### 6.1 Hooks can only tighten, never loosen

A `PreToolUse`/`PermissionRequest`/`ModeChanged` hook may **deny** or **escalate
to a prompt** an action that would otherwise proceed, but it can **never
auto-allow an action the built-in gate would have blocked or prompted on** —
except in the *non-interactive policy* role (`script.rs`/coordinators), where a
hook answering `allow` substitutes for the human that isn't there, and only up
to what the mode already permits. The default-branch git guard and the mode
gates remain the floor. This is the single most important invariant: a hook
extends safety, it does not become a bypass.

### 6.2 Data exposure & redaction

- Payloads **never include credential values.** `InstructionsLoaded` sends
  export *keys* only; `resolve_env`/`${profile:…}` secrets are never serialized.
- Tool-arg payloads (`PreToolUse`/`PostToolUse`) include args verbatim by
  default because the security use cases (secret-scan, policy) need them — but a
  registration may set `redact: true` to receive length-only placeholders, and
  result/content previews are length-capped (default 4 KB).
- A hook receives the payload on **stdin**, not argv — so secrets/large content
  never land in the process table or a hook's own argv logs.

### 6.3 Failure posture: fail-open by default, fail-closed by opt-in

- A normal hook that errors or times out is **fail-open**: aish logs a dim
  warning and proceeds as if the hook allowed. Rationale: a broken audit hook
  must not brick the shell.
- A registration marked `required: true` is **fail-closed**: its failure on a
  blocking event (`PreToolUse`, `PermissionRequest`) **denies** the action.
  This is for genuine security gates where "we couldn't check" must mean "no".
- The choice is explicit per hook, so the operator owns the safety/availability
  tradeoff.

### 6.4 Blast radius containment

- Every hook runs with a **timeout** (default 2 s sync / drain-timeout async)
  and is killed (`kill_on_drop`) past it — a hung hook can't wedge a turn.
- Hooks are spawned **fork/exec, no shell** (same invariant as every aish tool):
  no `sh -c`, so payload data can't smuggle shell metacharacters into a command
  line.
- Hook **stdout is parsed as JSON only**; it is never echoed to the terminal
  unless the hook is observe-only and aish is in a verbose/debug mode. A hostile
  payload-influenced hook can't paint arbitrary ANSI over the prompt.
- A recursion guard: hooks do **not** themselves fire hooks for the tools they
  run (a `FileChanged` hook that writes a file does not re-trigger `FileChanged`).
  Implemented via an env marker (`AISH_IN_HOOK=1`) the child carries, checked at
  the emit sites — mirrors the existing `AISH_COORDINATOR` nested-guard.
- **Coordinators inherit hook config** (same machine, same user) but a hostile
  *task* cannot register a new hook mid-run: hook config is read at startup /
  explicit `:hooks reload`, never from model output or tool results.

### 6.5 Trust boundaries summary

| Actor | Trusted to… | NOT trusted to… |
|---|---|---|
| Hook config author (human) | define hooks, mark `required`, set timeouts | (it's their box — full trust) |
| The model / agent | choose tool args that *flow through* hooks | register/modify hooks, loosen a gate |
| A `:tell` operator | steer a coordinator | inject hook config |
| Hook process | observe payloads, return decisions | exceed timeout, see secrets, paint UI, recurse |

---

## 7. Configuration

Hooks live in a dedicated config file (kept out of `~/.aishrc`, which is only
alias/export lines), discovered the same way `.mcp.json` is — **project scope
overrides user scope**:

- user scope: `~/.aish/hooks.json`
- project scope: `./.aish/hooks.json` (cwd)

```json
{
  "hooks": [
    {
      "event": "PreToolUse",
      "match": { "tool": "run_program", "program_in": ["rm", "kubectl", "gh"] },
      "kind": "sync",
      "required": true,
      "timeout_ms": 1500,
      "command": { "program": "/usr/local/bin/aish-policy", "args": ["--check"] }
    },
    {
      "event": "FileChanged",
      "match": { "path_glob": "**/*.rs" },
      "kind": "observe",
      "command": { "program": "rustfmt", "args": ["--emit", "files"] }
    },
    {
      "event": "ModeChanged",
      "match": { "to": "yolo" },
      "kind": "sync",
      "rule": { "decision": "deny", "reason": "yolo disabled by org policy" }
    },
    {
      "event": "WorkerStop",
      "kind": "observe",
      "command": { "program": "/usr/local/bin/notify-send", "args": ["aish job done"] }
    }
  ]
}
```

- `rule` is the **inline** form (no process spawn): a static decision/mutation
  for the common allow/deny/env case.
- `command` is the **process** form (stdin payload → stdout response).
- A `:hooks` colon command (list / reload / enable / disable / test) manages
  them live, mirroring `:mcp` and `:skill`. `:hooks test <event>` renders a
  sample payload and runs the matching hooks dry — essential for authoring.
- Hook config changes are **never** sourced from model output, tool results, or
  fetched remotely — only from these on-disk files at startup / explicit reload
  (§6.4).

---

## 8. Implementation approach & phasing

Designed so each phase ships independently and the hot-path/zero-overhead
guarantee holds from phase 1.

**Phase 0 — scaffolding (no behavior change).**
`hooks` module: config types, loader (`~/.aish/hooks.json` + project), the
in-process `HookSet` with `is_empty_for(event)`, payload envelope structs, and
`emit`/`evaluate` entry points that are **no-ops when no hook matches**. Add the
empty-set fast-path call sites for the observe-only events that are cheapest and
safest: `SessionStart`, `SessionEnd`, `TurnEnd`, `PostToolUse`, `FileChanged`,
`WorkerStart`/`WorkerStop`. Unit tests assert zero spawns when unconfigured.

**Phase 1 — observe-only core.**
Wire the remaining async/observe events across `engine.rs`, `repl.rs`,
`coordinator.rs`, `worker.rs`, `tools.rs`, `main.rs`. Async dispatch on tokio
with `kill_on_drop` + drain timeout. `:hooks` list/reload/test. Ship the
"notify on job done" and "audit log" use cases. **No blocking yet** — lowest
risk, immediately useful, can't change a turn's outcome.

**Phase 2 — sync/blocking gate hooks.**
Add `evaluate` (sequential, timeout, most-restrictive-wins) at `PreToolUse`,
`PermissionRequest`, `PreCompact`, `ModeChanged`. Implement fail-open/closed +
`required`. Thread a hook `deny` through the existing "user declined" synthetic
`ToolResult` path so the model handles it identically to a human decline.
Security-review this phase carefully (it can block actions). Inline `rule`
evaluation lands here (zero-spawn policy).

**Phase 3 — mutating hooks.**
`UserPromptSubmit` prompt prepend/append (fold into turn input — prompt-cache
safe), `PreToolUse` arg mutation, `CwdChanged` env injection. These are powerful
and the most footgun-prone, so they ship last with explicit logging of every
mutation and a `:hooks` audit trail.

**Phase 4 — polish & ecosystem.**
Richer matchers, `*` wildcard event, per-hook metrics (count/latency/deny rate)
surfaced in `:hooks`, a small library of example hooks (rustfmt-on-write,
secret-scan, slack-notify, deny-outside-cwd), and docs. Optional: durable async
hook queue (retry) if demand exists — explicitly deferred from v1.

### 8.1 Key data structures (sketch)

```rust
pub struct HookSet { by_event: HashMap<HookEvent, Vec<Registration>> }
pub enum HookKind { Observe, Sync }
pub struct Registration { matcher: Matcher, kind: HookKind, required: bool,
                          timeout: Duration, action: Action /* Rule | Command */ }
pub struct HookPayload { /* envelope */ data: serde_json::Value }
pub enum Outcome { Allow, Deny { reason: String }, Ask, Mutate(Patch) }

impl HookSet {
    fn is_empty_for(&self, e: HookEvent) -> bool { … }            // hot-path gate
    pub fn emit(&self, e: HookEvent, build: impl FnOnce()->HookPayload, s:&Session);
    pub async fn evaluate(&self, e: HookEvent,
        build: impl FnOnce()->HookPayload, s:&Session) -> Outcome;
}
```

Note `build` is a closure: the payload is materialized **only** when a hook
matches, so the unconfigured path never serializes.

---

## 9. Test strategy

1. **Zero-overhead invariant (the headline guarantee).** With no hooks
   configured, `emit`/`evaluate` perform no allocation and spawn no process.
   Assert via a spawn-counter test double and `is_empty_for` returning early;
   benchmark `run_turn` tool-loop iteration with/without the empty `HookSet` to
   prove no measurable delta.
2. **Pure matcher tests.** `Matcher` (tool glob, `program_in`, `path_glob`,
   mode/agent filters) is pure — table-driven unit tests, like the existing
   routing-decision snapshot in `repl.rs`.
3. **Dispatch ordering & composition.** Sequential sync execution, registration
   order, most-restrictive-wins, short-circuit on first `deny`, mutation
   merge — all testable with in-process fake hooks (closures), no real spawn.
4. **Timeout / fail-open / fail-closed.** A hook that sleeps past `timeout_ms` is
   killed; a normal hook → allow (fail-open), a `required` hook → deny
   (fail-closed). Exit-code mapping (`0`/`2`/other) covered.
5. **Blocking integration.** A `PreToolUse` deny produces the synthetic
   "declined" `ToolResult` and the tool does **not** run (assert no side effect),
   mirroring the existing always-allow/decline tests in `tools.rs`.
6. **Mutating integration.** `UserPromptSubmit` prepend lands in the turn input
   (and **not** in the cached system prompt — prompt-cache prefix byte-stable
   assertion). `CwdChanged` env merge visible to the next spawn.
7. **Security tests.** Payloads carry no credential values
   (`InstructionsLoaded` keys-only; `${profile:…}` never serialized); a hook
   cannot loosen a gate (a hook `allow` on a paranoid-mode write still prompts);
   recursion guard (`AISH_IN_HOOK`) prevents a `FileChanged` hook that writes
   from re-firing `FileChanged`; hooks spawn fork/exec with no shell.
8. **Coordinator/nested parity.** Hook sites fire inside `aish --coordinator`
   with `agent:"coordinator"` and the launcher's `session_id`; an observe hook
   sees worker turns. Config is **not** re-read from task/model output mid-run.
9. **Golden payload snapshots.** One captured-payload snapshot per event
   (`tests/golden/hooks/<event>.json`) so a schema change is a reviewable diff,
   following the `routing_decisions.snap` precedent.
10. **`:hooks test`.** The dry-run command renders the right sample payload and
    invokes matching hooks without performing the underlying action.

---

## 10. Open questions

1. **Per-result mutation** (`PostToolUse` rewriting a tool result before the
   model sees it) — powerful for redaction, but a footgun. Deferred; revisit if
   a redaction use case demands it.
2. **Durable/retryable async hooks.** v1 is best-effort fire-and-forget. A
   durable queue (survive restart, retry) is a natural follow-on if audit
   consumers need delivery guarantees.
3. **Hook output → system prompt.** Letting `InstructionsLoaded`/`SessionStart`
   contribute *persistent* system-prompt text would change the prompt-cache
   prefix. Deferred — `UserPromptSubmit` per-turn injection covers most of the
   need without touching the cached prefix.
4. **Remote/managed hook config** (fleet policy pushed from a server). Out of
   scope for v1 (trust + security review needed); `required` local hooks cover
   the immediate enterprise need.
5. **Cross-session hook events over the wire** (e.g. an MCP-published hook bus).
   Intentionally not in scope; aish hooks are local.
```
