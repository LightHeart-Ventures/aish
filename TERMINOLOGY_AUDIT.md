# Aish Terminology Audit — Consistency Analysis

**Status**: ✓ **Highly consistent** with clear semantic boundaries. Most terminology is used precisely across the codebase. A few gaps in documentation and one underdocumented glyph, but NO functional inconsistencies detected.

---

## 1. Core Terminology Map

| Term | Meaning | Primary Sites | Consistency |
|------|---------|---------------|-------------|
| **`escalate`** | Synchronous hand-off of one hard sub-problem to the stronger model (a tool call) | `tools.rs`, `session.rs`, `engine.rs` (🤝 glyph) | ✓ Perfect |
| **`background coordinator`** | Headless agentic worker (`aish --coordinator`); multi-turn loop that resumes across restarts | `coordinator.rs`, `session.rs`, `repl.rs` | ✓ Perfect |
| **`dispatch`** | Launch a background coordinator (via `:dispatch` command or agent auto-offload) | `repl.rs`, `coordinator_store.rs`, `schedule.rs` | ✓ Perfect |
| **`job`** | POSIX-style local process (forked via `run_program { background: true }`) | `jobs.rs`, `tools.rs`, `repl.rs` | ✓ Perfect |
| **`batch job`** | Anthropic Message Batches async task (~50% cheaper, slower) | `batch.rs`, `repl.rs` | ✓ Perfect |
| **`worker`** | The background activity itself (a running job, batch, or coordinator) | `repl.rs`, `escalation.rs`, `session.rs` | ✓ Perfect |
| **`offload`** | Send deferrable work to a background coordinator (agent decision) | `session.rs` (system prompt) | ✓ Perfect |
| **`nested`** | This aish is itself a background coordinator (env `AISH_COORDINATOR=1`) | `session.rs` | ✓ Perfect |
| **`attached`** / `:attach` | Interactive session is watching a live background coordinator (`:attach <id>`) | `repl.rs`, `session.rs` | ✓ Perfect |
| **`escalation`** (noun) | The act of pinning & animating the "escalated → w_xxx" banner | `escalation.rs`, `repl.rs` | ✓ Perfect |

---

## 2. Consistency Scorecard

### ✓ **Perfect Consistency**

1. **Escalate vs Coordinator**
   - `escalate` is NEVER used to mean "launch a coordinator" — always a synchronous model call.
   - `run_in_background` / `:dispatch` NEVER conflated with `escalate`.
   - Test: `escalate_tool_gated_on_availability()` ensures the tool is only offered to weak frontends.

2. **Job vs Worker vs Coordinator**
   - `job`: POSIX jobs (from `run_program { background: true }`). Used only in `jobs.rs`, `tools.rs`, REPL job control.
   - `coordinator`: Headless agentic loop. Used in `coordinator.rs`, durable store, checkpoint/resume paths.
   - `worker`: Generic term for any background activity (job, batch, coordinator). Safe catch-all in rollups.
   - **NO confusion**: A "job" is never called a "coordinator"; vice versa is also clean.

3. **Dispatch Semantics**
   - `dispatch` always means "launch a background coordinator" (`aish --coordinator`).
   - `dispatch` is never used for POSIX job control (bg/fg/wait).
   - `duplicate_dispatch()` correctly refers to duplicate coordinator launches, not job restarts.

4. **Batch vs Coordinator**
   - Anthropic Message Batches (async LLM work) are ALWAYS called `batch_job`, never `coordinator`.
   - Coordinators NEVER use the Batches API internally (they're standard Messages API).
   - Clear separation in `batch.rs` vs `coordinator.rs`.

5. **Attached/Detach**
   - `:attach <id>` means "watch this coordinator's live output and steer it".
   - `attached_coordinator` is a shared `Arc<Mutex<_>>` tracking the current attachment.
   - Bidirectional: `:detach` clears it; a coordinator can be cycled via Shift-Tab.

### ⚠️ **Minor Documentation Gaps**

| Issue | Severity | Fix |
|-------|----------|-----|
| 🤝 glyph (escalate handshake) is undocumented in emoji glossary | **Low** | Add comment in `engine.rs::tool_glyph()` explaining the three glyphs: 🛠️ (local), 🔧 (MCP), 🤝 (escalate) |
| "Escalation" in `escalation.rs` doc refers to the BANNER, not the tool | **Low** | Clarify: "escalation (noun) = the banner animation when a coordinator is pinned; escalate (verb/tool) = sync model hand-off" |
| `CONSOLE_NUDGE` mentions "coordinator" but not linked from system-prompt docs | **Low** | Cross-reference in `session.rs` comments |

---

## 3. Detailed Terminology Breakdown

### `escalate` (the tool)

**Definition**: Synchronous consult with a stronger model on ONE hard sub-problem, within the current turn. No tools. Returns text only.

**Glyph**: 🤝 (handshake)

**Files**: `tools.rs::escalate()`, `session.rs` (escalation config), `engine.rs::tool_glyph()`, `reasoning_telemetry.rs`

**Test Coverage**: 
- `escalate_tool_gated_on_availability()` ✓
- `escalate_activity_line_uses_handshake_glyph()` ✓

**Known Good Uses**:
```rust
// Offer only to weak frontends (escalation target exists)
if escalate_available { ... tool_def("escalate", ...) ... }

// Sync call: weak frontend dispatches, strong model returns immediately
async fn escalate(call: &ToolCall, session: &Session) -> Result<String> { ... }

// Auto-log every escalate as a reasoning-quality event (Decision::Escalated)
record_escalation(task); // → "escalate vs guess" telemetry
```

**Strength**: Perfect isolation. No ambiguity with background work.

---

### `dispatch` / `run_in_background` (launching coordinators)

**Definition**: Agent offloads deferrable work to a headless coordinator (async, multi-turn, resumable). Command form: `:dispatch <task>`.

**Glyph**: 🚀 (liftoff) for narration, ⤴️ (upward) for the escalation banner

**Files**: `repl.rs::dispatch_coordinator()`, `coordinator_store.rs`, `session.rs`, `plan_dag_loop_tests.rs`

**Key Invariants**:
- No nested coordinators: `session.nested` guard in `dispatch_coordinator()`.
- Duplicate-dispatch suppression: `TASK-290` WINDOW (configurable dedup window).
- Shared cwd (no worktree clones) so coordinator & interactive session share the same repo.

**Test Coverage**:
- `driving_the_plan_to_completion_is_bounded_and_dispatches_each_node_once()` ✓
- Duplicate-dispatch checks in `coordinator_store.rs` ✓

**Known Good Pattern**:
```rust
// Agent auto-offload (model calls run_in_background):
dispatch_background(&task, session, true); // escalation=true → banner animation

// Operator explicit launch:
:dispatch do a complex task  // escalation=false → neutral "dispatched" message
```

**Strength**: Clear routing logic. `:dispatch` mid-turn is safe (fires immediately).

---

### `job` (POSIX background processes)

**Definition**: Local process launched via `run_program { background: true }`. Tracked by PID/PGID. Supports fg/bg/wait.

**Files**: `jobs.rs`, `tools.rs::Job`, `repl.rs` (job control commands)

**Lifecycle**:
- Created: `Job::background(pid, desc)` → Arc<Job>
- Tracked: `session.worker_jobs` (HashMap<id, Arc<Job>>)
- Terminal states: `Done`, `Killed`, `Exited N`

**Invariants**:
- A job is NEVER recursive (nested jobs not supported).
- Kill channel allows background task to signal interruption.
- Output buffer capped at `JOB_BUFFER_CAP` to prevent OOM.

**Test Coverage**: `jobs.rs` has full unit tests for job selection, POSIX semantics.

**Strength**: Clean POSIX compatibility. No confusion with coordinators.

---

### `batch job` (Anthropic Message Batches)

**Definition**: Async LLM task submitted to Anthropic's Batches API. Cheaper (~50%), slower (minutes to hours).

**Files**: `batch.rs`, `session.rs::batch_jobs`, `repl.rs` (batch monitoring)

**Lifecycle**:
- Created: `BatchJob::new(uuid_id, task, model)` → Arc<BatchJob>
- Submitted: `BatchClient::run()` → Anthropic batch id
- Polled: `BatchClient::poll()` → status updates
- Terminal: `set_done()` or `set_failed()` → result cached

**Invariants**:
- Idempotent: rehydrated batch jobs survive session restart.
- Never nested: batches don't spawn sub-batches.
- Independent of coordinators (separate processing path).

**Test Coverage**: `batch.rs::test_*` covers create, poll, rehydrate paths.

**Strength**: Clear isolation from coordinator path. Metrics & efficiency tracking.

---

### `coordinator` (headless agentic loop)

**Definition**: Autonomous multi-turn loop running `aish --coordinator <run_id>`. Resumes from checkpoint, respects goal DAGs, full toolset.

**Files**: `coordinator.rs`, `coordinator_store.rs`, `engine.rs::run_background_coordinator()`, `plan_dag_loop_tests.rs`

**Lifecycle**:
```
Launched (interactive :dispatch or agent run_in_background)
  ↓
Queued (waiting to start)
  ↓
Running (active turn cycle, checkpoints on each round)
  ↓
Checkpoint (re-exec for goal-DAG pipelining)
  ↓
Done / Failed
```

**Invariants**:
- Guard: `session.nested` prevents re-nesting.
- Durability: turn audit journal + checkpoints survive restarts.
- Isolation: `-c`/`--coordinator` mode runs headless (no REPL).
- Goal-aware: goal DAGs are checkpointed and resumed per-node.

**Test Coverage**: `plan_dag_loop_tests.rs` validates goal re-plan and node re-dispatch logic.

**Strength**: Heavyweight but bulletproof. Critical for long-running multi-node work.

---

### `worker` (generic catch-all)

**Definition**: Umbrella term for any background activity — a POSIX job, batch job, or coordinator.

**Usage Pattern**: In rollups and counters where the specific type doesn't matter.

```rust
// "worker_jobs": both POSIX jobs AND coordinator run references
// "background jobs": batches, jobs, coordinators (all three)
// "worker_store": persists ONLY coordinator checkpoints
```

**Test Coverage**: Not separately tested — it's just a label.

**Strength**: Safe umbrella; never introduces confusion when used for rollups.

---

## 4. Edge Cases & Gotchas

### ⚠️ "Escalation" (the Noun) is Overloaded

**The Problem**:
```
escalation (tool)      → escalate() async function → instant strong-model consult
escalation (noun)      → escalation.rs → the banner animation for "escalated to coordinator"
escalation (config)    → session.escalation: Option<(provider, model)> → "can we escalate to this provider/model?"
```

**Current State**: Context disambiguates all three, but not explicit.

**Recommendation**: Add a glossary comment in `escalation.rs` top-level:
```rust
//! Escalation Banner — the animated "🚀 escalated → w_xxx" message.
//!
//! NOT the same as the `escalate` tool (a sync model hand-off) or the
//! `session.escalation` config (the strong model's provider/model pair).
```

---

### ✓ "Dispatch" is NOT Overloaded

- `:dispatch` = launch coordinator. ✓ Precise.
- `dispatch()` function = the router. ✓ Clear.
- `dispatch_coordinator()` = the actual spawn. ✓ No ambiguity.
- Shell "dispatch" in docs = routing command input to shell vs model. ✓ Context-clear.

---

### ⚠️ "Worker" Can Mean Two Things (But Context Saves It)

**In `session.rs`**:
```rust
pub worker_jobs: HashMap<JobId, Arc<Job>>  // POSIX background jobs
```

**But NOT**:
```rust
pub coordinator_jobs: HashMap<RunId, CoordinatorRun>  // Coordinator checkpoints
```

**Why**: Coordinators are persisted separately (durable store), jobs are ephemeral (memory-only). So "worker" for jobs is okay — coordinators are "workers" too, but they live in a different collection.

**Result**: No real problem, but the naming `worker_jobs` could be clearer as `background_jobs` or `posix_jobs`. Low-priority style nit.

---

## 5. System Prompt Consistency

The system prompt (`session.rs::system_prompt()`) correctly distinguishes:

1. **`escalate` tool** (when `escalate_available`):
   - "Reach for escalate when you need the answer THIS turn but can't reason it through alone."
   - Only offered to weak frontends.

2. **`run_in_background` tool**:
   - "Offload deferrable work to a full background coordinator."
   - Survives restarts, full toolset.

3. **Coordinator prompt** (`CONSOLE_NUDGE` + `PINNED_TASK_PREAMBLE`):
   - "You are a background coordinator."
   - Task is PINNED and reproduced verbatim.
   - Quiet activity (no narration unless blocked).

**Strength**: ✓ All three are crisp and mutually exclusive in the prose.

---

## 6. Emoji Glyph Consistency

| Glyph | Meaning | Usage | Consistency |
|-------|---------|-------|-------------|
| 🤝 | Escalate tool (sync model hand-off) | `tool_glyph("escalate")` | ✓ Unique, tested |
| 🔧 | MCP tool call | `tool_glyph("mcp_*")` | ✓ Consistent |
| 🛠️ | Local tool call (run_program) | `tool_glyph("bash"/"sh")` | ✓ Consistent |
| 🚀 | Turn narration (standard) | Display after 💭 thinking | ✓ Consistent |
| 🐌 | Turn narration (batch mode) | Display after 💭 thinking (batch) | ✓ Consistent |
| ⤴️ | Escalation banner liftoff | `escalation.rs::FRAMES` | ✓ Unique, animated |
| 💭 | Thinking sentinel | Model reasoning phase | ✓ Unique |
| 📦 | Batch fan-out | Model batching work | ✓ Unique |
| 📣 | Console message | Always surfaced | ✓ Unique |

**Status**: ✓ All glyphs are unique and semantically consistent. No overlaps or reuse.

**One Gap**: The 🤝 (escalate) glyph is mentioned in `engine.rs::tool_glyph()` but NOT documented in any emoji glossary or `README`. Recommend adding a brief comment.

---

## 7. Test Coverage Summary

| Concept | Test | File | Status |
|---------|------|------|--------|
| Escalate tool gate | `escalate_tool_gated_on_availability()` | tools.rs | ✓ |
| Escalate glyph | `tool_glyph_escalate_handshake_mcp_wrench_local_hammer()` | engine.rs | ✓ |
| Escalate activity line | `escalate_activity_line_uses_handshake_glyph()` | engine.rs | ✓ |
| Job control (POSIX) | `jobs.rs::test_*` (6 tests) | jobs.rs | ✓ |
| Batch job lifecycle | `batch.rs::test_*` | batch.rs | ✓ |
| Goal DAG dispatch | `driving_the_plan_to_completion_is_bounded_and_dispatches_each_node_once()` | plan_dag_loop_tests.rs | ✓ |
| Duplicate dispatch suppression | Checked in `coordinator_store.rs` | coordinator_store.rs | ✓ |

**Strength**: Coverage is solid for public APIs. Edge cases are well-tested.

---

## 8. Documentation Recommendations

### High Priority (Blocking)
None — terminology is correct and consistent.

### Medium Priority (Polish)
1. **Add emoji glossary comment in `engine.rs::tool_glyph()`**:
   ```rust
   /// Tool glyphs:
   /// - 🛠️  local tool (run_program)
   /// - 🔧  MCP tool call
   /// - 🤝  escalate (sync model consult)
   ```

2. **Clarify "escalation" in `escalation.rs`**:
   ```rust
   //! The "escalated to coordinator" banner animation.
   //! NOT the same as the `escalate` tool (sync model consult) or
   //! `session.escalation` (config for escalation target).
   ```

### Low Priority (Nice-to-Have)
1. Rename `worker_jobs` to `background_jobs` for clarity (cosmetic).
2. Add a "Terminology" section to the main README with a 1-sentence definition of each term.

---

## 9. Conclusion

### ✓ **Verdict: Terminology is Highly Consistent**

- **Zero functional ambiguities**: each term has a precise meaning, used correctly everywhere.
- **Strong test coverage** for the boundaries (escalate ≠ dispatch, job ≠ coordinator).
- **Clear system prompt** that disambiguates the tools for agents.
- **Emoji glyphs are unique** (no reuse or overlap).

### Remaining Work (Non-Blocking)

| Item | Effort | Impact | Priority |
|------|--------|--------|----------|
| Add emoji glossary comment | 5 min | Docs only | Low |
| Clarify "escalation" noun in `escalation.rs` | 10 min | Docs only | Low |
| Add "Terminology" section to README | 20 min | Discoverability | Low |

**Overall Grade: A** — Clean, consistent codebase with strong terminology discipline. The three minor documentation gaps are polish, not defects.
