# aish System Architecture

## Overview

**aish** is an AI-native shell for Linux that reasons over natural language commands, chains tools, and orchestrates work via coordinators (background agents). It combines:

1. **Interactive CLI** — read-eval-print loop with agent assistance
2. **Background Coordinators** — durable, long-running autonomous agents
3. **MCP Integration** — extensible tool/skill ecosystem
4. **Durable Persistence** — SQLite for history, memory, state, batch jobs

---

## System Layers

```
┌─────────────────────────────────────────┐
│  User Terminal (TTY)                    │
│  ├─ Interactive Commands                │
│  ├─ Background Job Control              │
│  └─ Real-time Output Streaming          │
└──────────────┬──────────────────────────┘
               │
┌──────────────v──────────────────────────┐
│  aish Shell Loop (Main)                 │
│  ├─ Command Parsing & Intent Detection  │
│  ├─ Tool Call Orchestration             │
│  ├─ Response Assembly & Streaming       │
│  └─ Job Lifecycle (inline vs. bg)       │
└──────────────┬──────────────────────────┘
               │
        ┌──────┴──────┐
        │             │
┌───────v────┐  ┌─────v──────────────────┐
│  Inline    │  │  Background Mode       │
│  Execution │  │  (Coordinator)         │
│  • Faster  │  │  ├─ Durable State      │
│  • Sync    │  │  ├─ Auto-Restart       │
│  • Unbaked │  │  ├─ Fan-Out (Batch)    │
│  • Tools   │  │  └─ Long-Running Work  │
└────────────┘  └─────────────────────────┘
        │             │
        └──────┬──────┘
               │
┌──────────────v──────────────────────────┐
│  MCP Tool/Skill Layer                   │
│  ├─ File I/O (read, write, edit)        │
│  ├─ Process Execution (run_program)     │
│  ├─ Code Intelligence (codebase-memory) │
│  ├─ Cloud APIs (AWS, GitHub, Atum)      │
│  └─ Custom Skills (local + skillfish)   │
└──────────────┬──────────────────────────┘
               │
┌──────────────v──────────────────────────┐
│  Persistent Layer                       │
│  ├─ SQLite (~/.aish/aish.db)            │
│  │  ├─ History                          │
│  │  ├─ Memories (tenant + project)      │
│  │  ├─ Coordinator Runs                 │
│  │  └─ Batch Jobs                       │
│  ├─ Git Worktrees (bg coordinator work) │
│  └─ Plugin State (MCP servers)          │
└─────────────────────────────────────────┘
```

---

## Key Components

### 1. Interactive Shell

The main `aish` process handles:

- **Command Parsing:** Interprets user intent (freeform text → tool calls)
- **Tool Invocation:** Dispatches calls to MCP servers (read_file, run_program, etc.)
- **Response Streaming:** Real-time output to the TTY
- **Job Control:** Spawns inline work OR delegates to background coordinators
- **Error Recovery:** Single smart fix on failure; escalates hard problems to stronger model

### 2. Background Coordinators

Headless aish instances (full tool/MCP access) that:

- **Run Durable:** Persist state in DDB/SQLite; survive restarts
- **Execute Autonomously:** Multi-turn reasoning without user input
- **Fan-Out:** Dispatch parallel sub-tasks via Anthropic Batch API
- **Self-Heal:** Auto-restart on transient failure (up to 2 retries); escalate to operator

**When to use:**
- Long-running work (builds, bulk edits, data migrations)
- Parallelizable tasks (Batch API eligible)
- Non-urgent work (result auto-delivers later)

### 3. MCP (Model Context Protocol) Ecosystem

Tools exposed via MCP servers:

| Server | Tools | Typical Use |
|--------|-------|------------|
| **aish-core** | File I/O, process exec, git, terminal | Every command |
| **codebase-memory** | Code search, graph trace, architecture | Code intelligence |
| **atum** | Project mgmt, releases, agents, costs | Atum.AI backend |
| **github** | Repo, PR, issue, workflow control | GitHub integration |
| Custom MCP | Domain-specific tools | Plugins & integrations |

### 4. Persistence Layer

**SQLite Database** (`~/.aish/aish.db`)
- Session history (commands, outputs)
- Durable memories (tenant/project/agent scoped)
- Coordinator run state (for restart)
- Batch job tracking
- Settings & preferences

**Git Worktrees**
- Each background coordinator gets an isolated worktree
- No interference with the live session
- Easy rollback & cleanup

**Plugin State** (MCP-native)
- Per-plugin durable store
- Webhook subscription registry
- Integration credentials (OAuth, API keys)

---

## Execution Modes

### Mode: Inline (Default)

```
User Input
   ↓
Parse Intent
   ↓
Dispatch Tools (synchronous)
   ↓
Receive Outputs
   ↓
Stream Response → User
   ↓
[Turn ends]
```

**Characteristics:**
- Synchronous (user waits)
- Full tool access
- No persistence overhead
- Best for quick, interactive tasks
- ~30s-2min typical duration

### Mode: Background (Coordinator)

```
User Input: "offload this task"
   ↓
Create Coordinator Run
   ├─ Write to DDB/SQLite
   ├─ Spawn headless aish
   └─ Return job_id immediately
   ↓
[User can continue]
   ↓
[Coordinator runs autonomously]
├─ Multi-turn reasoning
├─ Tool calls + observe
├─ Parallel fan-out (Batch API)
└─ Self-heal on failure
   ↓
[Result auto-delivers]
```

**Characteristics:**
- Asynchronous (user doesn't wait)
- Durable state (survive restarts)
- Full tool access
- Auto-restart on transient failure
- Best for long-running, parallelizable work
- Can span minutes to hours

### Mode: Batch (Coordinator Sub-Mode)

When a coordinator calls `run_in_background(..., tier: "batch")`:

```
Coordinator
   ↓
[Collect batch of tasks]
   ↓
Submit to Anthropic Batches API
   ↓
[Poll for completion ~minutes]
   ↓
Receive results
   ↓
Continue with next turn
```

- Cheaper than real-time (token-batched pricing)
- ~5-10 min latency
- Ideal for bulk, non-urgent work

---

## Coordinator Safety & Durability

### Stale-Row Prevention

Coordinators detect when their DDB/SQLite row is stale (owner heartbeat gone) and **gracefully unwind**:

1. Commit pending work (git push, Atum API calls)
2. Draft PR if applicable
3. Report final status to operator
4. Terminate cleanly

Prevents orphaned work lingering indefinitely.

### Loop Guards

Multi-level guards prevent infinite loops:

1. **Turn Limit:** Max 100 turns per run
2. **Timeout:** 3h hard ceiling per run
3. **Cycle Detection:** Detect repeating tool patterns
4. **Escalation:** Hard-stuck runs flag for operator review

See [reference/coordinator/loop-guards.md](./reference/coordinator/loop-guards.md).

### Atomic State Management

- **Idempotent APIs:** All tool calls include optional `clientRequestId` for 10-min cache
- **Worktree Isolation:** Each coordinator gets its own git worktree (no cross-contamination)
- **Transaction Rollback:** Failed tool calls don't corrupt state; retry or escalate

---

## Memory & Context

### Local Memory (aish)

- **Persistent Memory:** `remember()` stores durable facts across sessions
- **Recall:** `recall()` retrieves facts by keyword/tag
- **Scoped Memories:** tenant, project, agent scopes
- **Auto-Organize:** `atum_memory_organize()` deduplicates & prunes

### Coordinator Context

- **System Prompt:** 6-layer stack (platform → tenant → role → lifecycle → project → agent)
- **Memory Preload:** On start, load all visible memories (tenant + project + agent)
- **Refresh:** Periodic refresh of context on multi-hour runs

---

## Typical Workflows

### 1. Interactive Feature Development

```
$ aish
> "create a new React component for the dashboard"
  ├─ Parse intent → code-gen task
  ├─ Search codebase for similar patterns
  ├─ Draft new component (inline)
  ├─ Run tests (inline)
  └─ Stream: "Created component at src/DashboardCard.tsx; tests pass ✓"

> "make it responsive"
  ├─ Read component source
  ├─ Add media queries
  ├─ Update tests
  └─ Stream: "Updated with mobile breakpoints; tests pass ✓"
```

**Mode:** Inline (30s-2min per turn)

### 2. Long-Running CI Fix

```
$ aish
> "fix the broken test in ci-pipeline-debug style"
  ├─ Recognize: long-running task
  ├─ Offload → background coordinator
  └─ "On it — I'll work that out in the background and post the answer here."

[User continues working]

[~10 min later]
[Result auto-delivers]
✓ Fixed flaky test in utils/sort.test.ts
✓ Opened PR #1234 with the fix
```

**Mode:** Background Coordinator (10-30 min)

### 3. Bulk Data Migration

```
$ aish
> "migrate 10k records from old schema to new schema"
  ├─ Recognize: parallelizable, non-urgent
  ├─ Offload → background coordinator + Batch API
  └─ "On it — submitting batch work…"

[~5-10 min later via Batch API]
✓ Processed 10,000 records in 12 seconds (batched)
✓ Validated all; 0 failures
✓ Schema migration complete
```

**Mode:** Batch (via Coordinator)

---

## Cost & Performance

### Token Efficiency

- **Inline:** Full-context tool calls; ~500-5k tokens per turn
- **Coordinator:** Reduced turn overhead (batched reasoning); ~1-10k tokens per turn
- **Batch API:** Token-batched pricing; ~30% cheaper at scale

### Latency

| Mode | Latency | Use Case |
|------|---------|----------|
| Inline | 30s–2min | Interactive, immediate feedback |
| Coordinator | 1-30min | Long tasks, parallelizable |
| Batch | 5-10min | Non-urgent, bulk work |

### Resource Usage

- **Worktrees:** ~50MB per coordinator (git metadata)
- **SQLite:** ~100MB typical; auto-purges old history (30d TTL)
- **Memory:** Single aish process ~150-300MB; coordinators ~200-500MB each

---

## Extensibility

### Skills

User-defined or imported playbooks (SKILL.md format) that agents can discover and execute. Stored in:
- **Built-in:** `/Users/grhohertz/.aish/skills/*/SKILL.md`
- **Imported:** Via `atum_import_skill` (third-party GitHub repos, agentskills.io)

### MCP Servers

Custom tools via the Model Context Protocol. Loaded from:
- **Local plugins:** `~/.aish/plugins/*/mcp.json`
- **Remote:** Configured via project or user-scope `.mcp.json` (see `src/mcp.rs`)

### Coordinators as Programmable Agents

Background coordinators can:
- Invoke other agents (recursive, bounded depth)
- Subscribe to events and react
- Maintain durable state (memory, DB rows)
- Emit events for downstream coordination

---

## Next Steps / Reading

- **Getting Started?** See the main README
- **Deploying aish?** See [RELEASE.md](./RELEASE.md)
- **Implementing a feature?** Check [reference/](./reference/)
- **Debugging an issue?** See [internals/error-diagnostics.md](./internals/error-diagnostics.md)
- **Performance tuning?** See [internals/telemetry-efficiency.md](./internals/telemetry-efficiency.md)
