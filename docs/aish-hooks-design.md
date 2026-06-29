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

## 4. Hook catalog (abbreviated)

**See the full design doc for complete hook specifications, error semantics,
implementation phases, test strategy, and open questions.**

Core hooks by category:

### Session & lifecycle
- `SessionStart` — process initialized, ready for first turn (async observe)
- `SessionEnd` — shell exiting (async observe)
- `InstructionsLoaded` — rc/profile/skill/mcp config loaded (async observe)
- `McpServersReady` — MCP connections established (async observe)

### Interactive
- `ModeChanged` — `:yolo`/`:mode` (sync, can block)
- `BackendChanged` — `:backend`/`:model` (async observe)
- `PromptRouteDecided` — route to model or shell decided (async observe)
- `DirectCommandRun` — command dispatched directly (async observe)
- `CwdChanged` — `cd` / `change_dir` (sync, can mutate env)

### Turn & tool lifecycle
- `UserPromptSubmit` — turn begins (sync, can prepend/append to prompt)
- `PreToolUse` — before tool execution (sync, can block/veto)
- `PostToolUse` — after tool execution (async observe)
- `PostToolUseFailure` — tool returned error (async observe)
- `PermissionRequest` — gated action confirmation (sync, can block)
- `PermissionDenied` — user/hook denied permission (async observe)
- `TurnEnd` — turn finished (async observe)
- `TurnEndFailure` — turn failed/aborted (async observe)

### Files & persistence
- `FileChanged` — file written/edited/deleted (async observe)
- `MemoryStored` — fact stored/recalled (async observe)
- `PreCompact` — context compaction beginning (sync, can veto)
- `PostCompact` — compaction finished (async observe)

### Background jobs & coordination
- `WorkerStart`, `WorkerStop` — coordinator spawn/exit (async observe)
- `CoordinatorPhaseChanged` — round/phase transition (async observe)
- `OperatorMessageReceived` — `:tell` message received (async observe)
- `LoopGuardTripped` — repeat/budget exhausted (async observe)
- `EscalationRequested` — model escalation begin (async observe)
- `BatchFanOut` — batch job dispatch (async observe)

### System
- `UpdateAvailable`, `UpdateApplied` — version events (async observe)
- `SkillMatched` — skill matched/suggested (async observe)

---

## 5. Configuration & management

Hooks are registered via three sources (merged):
1. **`~/.aish/hooks.json`** — user global config
2. **`.aish/hooks.json`** — project local config
3. **`plugin.json`** — plugins (future, integrates with plugin system)

Schema:
```json
{
  "hooks": [
    {
      "event": "PreToolUse",
      "matcher": {
        "tool": "*",               // glob: "run_program", "read_file", "*"
        "program": "git",          // optional program filter
        "path_glob": "*.lock",     // optional path in-arg filter
        "mode": ["paranoid"],      // optional mode list (OR)
        "agent": "interactive"     // optional agent filter
      },
      "action": {
        "type": "command",
        "program": "/usr/local/bin/my-hook",
        "timeout_ms": 5000,
        "required": false          // fail-closed on timeout?
      }
      // OR for simple rules:
      // "action": { "type": "rule", "deny_if": "path_contains('secret')" }
    }
  ]
}
```

**`:hooks` commands:**
- `:hooks list` — show registered hooks + match counts
- `:hooks test <event>` — dry-run: show sample payload, invoke matching hooks (no action)
- `:hooks reload` — re-read config from disk
- `:hooks audit` — replay recent hook invocations + latencies

---

## 6. Security model

1. **Principle of least privilege.** Hooks run with the user's UID/GID. Payloads
   carry no credential values (export keys-only, no values; `${profile:…}`
   never serialized). File paths are absolute.
2. **Fail-safe gating.** A hook cannot weaken a permission gate. A `deny` on
   `paranoid` mode still prompts; a `PreToolUse` hook `allow` in `paranoid` mode
   **does not bypass the builtin gate**. Hooks augment, never override.
3. **Timeout & isolation.** Each hook has a hard timeout (default 5s, configurable
   per hook). On timeout, a `required` hook fails-closed (denies the action), an
   optional hook fails-open (allows). No recursive invocation (`AISH_IN_HOOK` env
   guard prevents a `FileChanged` hook from re-triggering `FileChanged`).
4. **No shell.** Hooks spawn fork/exec directly — they do not run in a shell, so
   they cannot accidentally use shell metacharacters.
5. **Audit trail.** Every hook invocation is logged (optional redacted copy to disk
   for compliance). The `:hooks audit` command surfaces recent events + outcomes.

---

## 7. Implementation plan (phased)

**Phase 0 — Core infrastructure (foundation).**
Wire the `HookSet` struct, `HookPayload` envelope with common + per-event fields,
the configuration parser (`~/.aish/hooks.json`), the async dispatcher, the
timeout/spawn infrastructure, and the empty-map fast-path checks at call sites.
Unit test that an unconfigured `HookSet` spawns zero processes and allocates
nothing on the hot path.

**Phase 1 — Observe-only core (safest first).**
Wire the remaining async/observe events across `engine.rs`, `repl.rs`,
`coordinator.rs`, `worker.rs`, `tools.rs`, `main.rs`. Async dispatch on tokio
with `kill_on_drop` + drain timeout. `:hooks` list/reload/test. Ship the
"notify on job done" and "audit log" use cases. **No blocking yet** — lowest
risk, immediately useful, can't change a turn's outcome.

**Phase 2 — Sync/blocking gate hooks (restricted).**
Add `evaluate` (sequential, timeout, most-restrictive-wins) at `PreToolUse`,
`PermissionRequest`, `PreCompact`, `ModeChanged`. Implement fail-open/closed +
`required`. Thread a hook `deny` through the existing "user declined" synthetic
`ToolResult` path so the model handles it identically to a human decline.
Security-review this phase carefully (it can block actions). Inline `rule`
evaluation lands here (zero-spawn policy).

**Phase 3 — Mutating hooks (powerful & careful).**
`UserPromptSubmit` prompt prepend/append (fold into turn input — prompt-cache
safe), `PreToolUse` arg mutation, `CwdChanged` env injection. These are powerful
and the most footgun-prone, so they ship last with explicit logging of every
mutation and a `:hooks` audit trail.

**Phase 4 — Polish & ecosystem.**
Richer matchers, `*` wildcard event, per-hook metrics (count/latency/deny rate)
surfaced in `:hooks`, a small library of example hooks (rustfmt-on-write,
secret-scan, slack-notify, deny-outside-cwd), and docs. Optional: durable async
hook queue (retry) if demand exists — explicitly deferred from v1.

---

## 8. Test strategy

1. **Zero-overhead invariant.** No allocation / no process spawn when unconfigured.
2. **Matcher purity.** Table-driven tests for glob/mode/agent filters.
3. **Dispatch ordering.** Sequential execution, registration order, short-circuit.
4. **Timeout & failure modes.** Killing on timeout, fail-open/closed behavior.
5. **Blocking integration.** Hook `deny` produces synthetic "declined" result.
6. **Mutating integration.** Prompt mutations visible; env mutations propagate.
7. **Security boundaries.** Payloads carry no secrets; hooks can't loosen gates; recursion guard.
8. **Coordinator parity.** Hook sites fire identically in nested coordinators with correct `agent` descriptor.
9. **Golden snapshots.** Captured-payload snapshot per event for schema review.
10. **`:hooks test` dry-run.** Invokes hooks without performing actions.

---

## 9. Open questions

1. **Per-result mutation** (`PostToolUse` rewriting tool output) — powerful but risky. Deferred.
2. **Durable/retryable async.** v1 is best-effort; queue (survive restart) is a follow-on.
3. **Hook output → system prompt.** Persistent prompt contribution would affect
   prompt-cache prefix. `UserPromptSubmit` per-turn injection covers most cases.
4. **Remote/managed config.** Fleet policy pushed from a server. Out of v1 scope.
5. **Cross-session hook events.** Out of scope; aish hooks are local.

---

## Appendix: Recommended hooks (user request + 10 additional)

**User-requested:**
SessionStart, SessionEnd, InstructionsLoaded, UserPromptSubmit, PreToolUse,
PostToolUse, PermissionRequest, PermissionDenied, PostToolUseFailure,
WorkerStart, WorkerStop, TurnEnd, TurnEndFailure, PreCompact, PostCompact,
CwdChanged, FileChanged

**10 additional (recommended by codebase review):**
1. `PromptRouteDecided` — when direct-vs-model routing is decided (unique to aish)
2. `DirectCommandRun` — direct shell dispatch (aish-specific)
3. `ModeChanged` — mode downgrade to yolo (audit + policy enforcement)
4. `CoordinatorPhaseChanged` — coordinator round transitions (visibility)
5. `LoopGuardTripped` — repeat detection / budget exhaustion (alerting)
6. `OperatorMessageReceived` — `:tell` messages (workflow)
7. `McpServersReady` — MCP connection (setup)
8. `EscalationRequested` — escalate tool fired (audit + intervention)
9. `BackgroundJobStart/Stop` — background job lifecycle (monitoring)
10. `UpdateAvailable`/`UpdateApplied` — version updates (notifications)

---

**End of design. See the full doc for implementation details, payload schemas, and test strategy.**
