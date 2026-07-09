# codebase-memory plugin

Wraps the **DeusData/codebase-memory-mcp** server (MIT) as an aish plugin. It
gives every aish session — interactive and background coordinator — a queryable
**code knowledge graph**: functions, classes, routes, call edges, data-flow,
cross-service links, complexity/hot-path signals, and semantic search over the
whole repo. Use these tools **instead of** grep/glob when you need to *find
definitions, callers, impact, or architecture*.

- **Server name:** `codebase-memory`
- **Binary:** `codebase-memory-mcp` (stdio transport)
- **Upstream:** https://github.com/DeusData/codebase-memory-mcp
- **License:** MIT (server) — attribution preserved in `plugin.json`.

## What you get (14 tools)

| Tool | Use for |
|------|---------|
| `mcp__codebase-memory__index_repository` | Build/refresh the graph for a repo (modes: full / moderate / fast / cross-repo-intelligence). |
| `mcp__codebase-memory__get_architecture` | High-level packages, services, dependency clusters (Leiden communities). |
| `mcp__codebase-memory__search_graph` | BM25 / regex / semantic search for symbols. Replaces grep for definitions. |
| `mcp__codebase-memory__search_code` | Grep-then-enrich: text matches deduped into ranked functions. |
| `mcp__codebase-memory__query_graph` | Raw Cypher for multi-hop patterns, aggregations, complexity/bottleneck hunts. |
| `mcp__codebase-memory__trace_path` | Callers/callees, data-flow, cross-service tracing. Replaces grep for impact analysis. |
| `mcp__codebase-memory__get_code_snippet` | Read source for a resolved `qualified_name`. |
| `mcp__codebase-memory__get_graph_schema` | Node labels + edge types. |
| `mcp__codebase-memory__detect_changes` | Changed symbols + blast radius vs a base ref. |
| `mcp__codebase-memory__manage_adr` | Read/update Architecture Decision Records. |
| `mcp__codebase-memory__ingest_traces` | Fuse runtime traces into the graph. |
| `mcp__codebase-memory__list_projects` / `index_status` / `delete_project` | Manage indexed projects. |

## Setup

### 1. Install the binary

See [`PREREQUISITES.md`](./PREREQUISITES.md). Quickest paths:

- **Homebrew:** the binary lands on your `PATH` (e.g. `/opt/homebrew/bin/codebase-memory-mcp`).
- **aish-native:** `:codebase install` downloads the platform-matched release into `~/.aish/bin/`.
- **Source:** `cargo build --release` from the upstream repo.

Verify it resolves:

```
:!codebase-memory-mcp --version
```

### 2. Drop the plugin in place

This plugin lives at `~/.aish/plugins/codebase-memory/`. aish discovers it on
startup and **auto-merges its `.mcp.json`** into the MCP client set — you do
**not** need to hand-edit `~/.aish/.mcp.json`. Precedence is first-one-wins:
`project config > user config > plugin`. Confirm the server connected:

```
:mcp
```

You should see `codebase-memory` with its tool count.

### 3. (Optional) Generate the `~/.aish/.mcp.json` entry

The plugin merge is enough to make the tools available. But the **repo-open
auto-index** gate additionally probes `~/.aish/.mcp.json` for enrollment (see
[Auto-index gate](#auto-index-gate)). If you want auto-index, add the server
entry to your user config too — either run the aish-native enroller:

```
:codebase install      # writes mcpServers.codebase-memory into ~/.aish/.mcp.json
```

…or add it by hand to `~/.aish/.mcp.json` (merge into `mcpServers`, keep
siblings like `atum`):

```json
{
  "mcpServers": {
    "codebase-memory": { "type": "stdio", "command": "codebase-memory-mcp", "args": [] }
  },
  "codebaseMemory": { "autoIndex": true }
}
```

## Auto-index gate

On entering a repo where the server is enrolled + connected, aish fires **one**
bounded `index_repository` handoff so the graph is warm before your first query
(`src/codebase_memory.rs` → `should_auto_index`, `engine::maybe_auto_index_repo`).

Resolution order for the on/off gate:

1. **Env override** — `AISH_CODEBASE_AUTO_INDEX`. Any of `0`, `false`, `off`,
   `no` disables it; anything else enables. An empty value is treated as unset.
2. **Config key** — `codebaseMemory.autoIndex` (boolean) in `~/.aish/.mcp.json`.
3. **Default** — ON.

```
# disable for one session
:env AISH_CODEBASE_AUTO_INDEX=0
```

> **Enrollment caveat (verified against `engine.rs`):** the auto-index handoff
> requires `should_auto_index(enrolled, connected, gate_on, !already)` — and
> `enrolled` is `is_enrolled(~/.aish/.mcp.json, "codebase-memory")`. The plugin
> merge sets `connected` (tools work), but NOT `enrolled`. So after moving the
> server def into this plugin, **manual `index_repository` works, but repo-open
> auto-index stays dormant unless the `mcpServers.codebase-memory` entry is also
> present in `~/.aish/.mcp.json`** (step 3 above). Keep that entry, or run
> `:codebase install`, if you rely on auto-index.

## Usage

Prefer graph tools over grep for structural questions:

- *"Who calls `merge_server_entry`?"* → `trace_path {function_name, direction: inbound}`.
- *"Where's the auth middleware?"* → `search_graph {query: "auth middleware"}`.
- *"What's the architecture?"* → `get_architecture`.
- *"Find O(n²) hot paths"* → `query_graph` with the complexity properties (see the
  `code-intelligence` skill under `skills/`).

The bundled **`code-intelligence`** skill (`skills/code-intelligence/SKILL.md`)
teaches the graph-first discovery workflow; aish surfaces it automatically on
matching tasks.

## Files

| File | Purpose |
|------|---------|
| `plugin.json` | Plugin manifest + MCP/auto-index metadata. |
| `.mcp.json` | The `codebase-memory` stdio server entry aish auto-merges. |
| `PREREQUISITES.md` | How to install the `codebase-memory-mcp` binary. |
| `skills/code-intelligence/SKILL.md` | Graph-first code-discovery playbook. |

See also: [`docs/plugins/codebase-memory-plugin.md`](../../docs/plugins/codebase-memory-plugin.md).
