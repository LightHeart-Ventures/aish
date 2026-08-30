# aish Documentation Index

Welcome to the aish codebase documentation. This index guides you to the right resource based on your need.

## Quick Start

- **New to aish?** Start with [ARCHITECTURE.md](./ARCHITECTURE.md) for a system overview.
- **Deploying or releasing?** See [RELEASE.md](./RELEASE.md).
- **Implementing a feature?** Check [reference/](#reference) for implementation details.
- **Debugging or deep-diving?** Browse [internals/](#internals) for design decisions and diagnostics.

---

## Core Documentation

| Document | Purpose |
|----------|---------|
| [ARCHITECTURE.md](./ARCHITECTURE.md) | High-level system design, layers, and dependencies |
| [RELEASE.md](./RELEASE.md) | Release workflows, channels, and deployment procedures |

---

## Reference

Implementation details, schemas, and how things work.

### Coordinator & Orchestration
- [reference/coordinator/patterns.md](./reference/coordinator/patterns.md) — Coordinator loop patterns and best practices
- [reference/coordinator/loop-guards.md](./reference/coordinator/loop-guards.md) — Detecting and preventing infinite loops
- [reference/coordinator/stale-row-prevention.md](./reference/coordinator/stale-row-prevention.md) — DDB row staleness detection

### Plugin System
- [reference/plugins/memory.md](./reference/plugins/memory.md) — Plugin memory schema and APIs
- [reference/plugins/state.md](./reference/plugins/state.md) — Plugin state management
- [reference/plugins/webhook-events.md](./reference/plugins/webhook-events.md) — Webhook event contracts

### Codebase Intelligence
- [reference/codebase-memory/graph-artifacts.md](./reference/codebase-memory/graph-artifacts.md) — Knowledge graph indexing and artifact format

### Infrastructure & Data
- [reference/database.md](./reference/database.md) — Database schema and key paths
- [webhooks.md](./webhooks.md) — Webhook handler dispatch and routing

---

## Internals

Deep dives, design decisions, and implementation notes.

- [internals/error-diagnostics.md](./internals/error-diagnostics.md) — Error classification and diagnostic tooling
- [internals/hooks-design.md](./internals/hooks-design.md) — aish hook system architecture
- [internals/host-brokered-sibling-spawn.md](./internals/host-brokered-sibling-spawn.md) — Spawning sibling processes via host broker
- [internals/serial-chain-yield-diagnosis.md](./internals/serial-chain-yield-diagnosis.md) — Diagnosing serial-chain execution bottlenecks
- [internals/telemetry-efficiency.md](./internals/telemetry-efficiency.md) — Observability at scale (cost & latency)
- [internals/skillfish-integration.md](./internals/skillfish-integration.md) — Skill catalog integration with skillfish
- [internals/sqlite-vec-integration.md](./internals/sqlite-vec-integration.md) — Vector search via sqlite-vec

---

## Output Formats

Specification for output serialization and rendering.

- [formats/skill-format.md](./formats/skill-format.md) — SKILL.md frontmatter and body structure
- [formats/emoji.md](./formats/emoji.md) — Emoji encoding for status and result representation
- [formats/colorized.md](./formats/colorized.md) — ANSI color codes and terminal styling

---

## Archive

Completed work, superseded design phases, and exploratory spikes. See [archive/MANIFEST.md](./archive/MANIFEST.md) for context.

- **Design Phases:** `archive/S6-*.md`, `archive/S7-*.md` — Completed feature phases
- **Exploration:** `archive/spikes/` — Prototypes and analysis
- **Implementation Notes:** `archive/session-scoped-jobs/`, `archive/TASK-268-*` — Completed tasks and proposals

---

## Directory Structure

```
docs/
├── INDEX.md                          # You are here
├── ARCHITECTURE.md                   # System design overview
├── RELEASE.md                        # Release workflows & deployment
├── webhooks.md                       # Webhook dispatch & routing
│
├── reference/                        # Implementation guides
│   ├── coordinator/                  # Coordinator patterns & safety
│   ├── plugins/                      # Plugin schemas & APIs
│   ├── codebase-memory/              # Knowledge graph
│   └── database.md                   # DB schema
│
├── internals/                        # Deep dives & design
│   ├── error-diagnostics.md
│   ├── hooks-design.md
│   ├── host-brokered-sibling-spawn.md
│   ├── serial-chain-yield-diagnosis.md
│   ├── telemetry-efficiency.md
│   ├── skillfish-integration.md
│   └── sqlite-vec-integration.md
│
├── formats/                          # Output format specs
│   ├── skill-format.md
│   ├── emoji.md
│   └── colorized.md
│
├── archive/                          # Completed work & history
│   ├── MANIFEST.md                   # What & why archived
│   ├── spikes/                       # Prototypes & analysis
│   └── session-scoped-jobs/          # Completed exploration
│
├── plugins/                          # (existing)
├── design/                           # (existing)
└── [REORGANIZATION_PLAN.md]          # This migration's documentation
```

---

## Contributing

When adding new documentation:

1. **Is it a reference/how-to?** → `reference/`
2. **Is it a deep-dive/design decision?** → `internals/`
3. **Is it a format/spec?** → `formats/`
4. **Is it completed/superseded work?** → `archive/` + update `archive/MANIFEST.md`
5. Otherwise, it likely belongs in the root or a topic subdirectory.

Update this INDEX.md when adding new top-level docs.
