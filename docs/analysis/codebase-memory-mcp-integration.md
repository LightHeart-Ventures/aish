# Analysis: DeusData/codebase-memory-mcp → cherry-pick for aish

**Date:** 2026-07-05
**Author:** aish coordinator (w_NmlNy2Mv)
**Board:** aish (`b_9e8fb2e16501`)

## TL;DR — MCP server, not native Rust

**Consume `codebase-memory-mcp` as an MCP server that aish connects to. Do NOT
reimplement it in Rust.** Build a thin *native enrollment/handoff layer* instead.

| Option | Verdict | Why |
|---|---|---|
| Reimplement in native Rust | ❌ Reject | The engine is ~170 MB of vendored C: 158 tree-sitter grammars, a Hybrid-LSP type resolver for 13 languages, bundled Nomic `nomic-embed-code` embeddings (40K tokens, 768d int8), a Cypher lexer/parser/planner/executor, Louvain community detection, LZ4/zstd graph store. Months of work, enormous maintenance surface, zero product differentiation for a shell. |
| Consume as MCP server (+ thin native glue) | ✅ Adopt | aish is **already an MCP client** (`src/mcp.rs` `McpHost::start`, stdio transport, `.mcp.json`). The tool ships as a **single static binary, MIT-licensed, zero deps, no API keys**, and already auto-configures 11 coding agents (Claude Code, Cursor, Zed, Aider, …). aish becomes agent #12 with an afternoon of glue, not a quarter of engine work. |

## What the target is

`DeusData/codebase-memory-mcp` (26.6k ⭐, MIT, pure C): a high-performance code
intelligence MCP server. Indexes a repo into a persistent SQLite **knowledge
graph** (functions, classes, call chains, HTTP routes, cross-service links) via
tree-sitter AST parsing across 158 languages. Linux kernel (28M LOC) in ~3 min;
structural queries in <1 ms. **14 MCP tools.** Benchmarked: 83% answer quality,
**10–120× fewer tokens, 2.1× fewer tool calls** vs file-by-file exploration
(arXiv:2603.27277).

Selected capabilities: `get_architecture`, `trace_path` (call graph),
`search_graph` (structural), `semantic_query` (bundled vector search, no API
key), BM25 FTS5 code search, `detect_changes` (git-diff → affected symbols +
risk), dead-code detection, Cypher queries, cross-service HTTP/gRPC/GraphQL
linking, `manage_adr` (Architecture Decision Records), a committable
team-shared graph artifact (`.codebase-memory/graph.db.zst`), background
git-watcher auto-reindex, and an optional 3D graph-visualization UI.

## Why it matters for aish specifically

aish coordinators today burn tokens on **grep → read → grep** loops to
understand a codebase (the exact failure mode the 5-phase pipeline and the
87-call runaway lessons target). A single structural graph query replaces
dozens of those cycles. The headline number — *~3,400 tokens vs ~412,000 for 5
structural queries* — maps directly onto aish's cost/latency goals and the
Phase-0/Phase-1 discovery batch. This is a **token-efficiency multiplier for
every coordinator**, not just a new toy tool.

## Cherry-picked features (mapped to aish work)

1. **Curated auto-install MCP entry** — an aish-native installer (mirrors the
   existing `:skill add` UX) that downloads the static binary and writes the
   `.mcp.json` server entry, so the 14 tools appear with one command. aish
   becomes "coding agent #12". *[native glue: `src/mcp.rs` + installer]*
2. **Repo-open auto-index handoff** — wire into the existing repospec habit:
   when aish enters a repo, register/trigger a codebase-memory index so
   structural queries are warm before the first coordinator turn. *[native:
   session/repospec hook]*
3. **Git-diff impact surfacing** — before a coordinator edits, call
   `detect_changes` → affected symbols + risk; fold into the Phase-0 guard and
   the draft-PR body. The token-efficiency headline. *[native glue calling MCP]*
4. **Advertise code-intelligence tools to the model** — prompt-surface
   `get_architecture` / `trace_path` / `search_graph` / `semantic_query` /
   dead-code as first-class discovery tools, so the model prefers a graph query
   over a grep loop. *[prompt + docs, free via MCP]*
5. **Team-shared graph artifact convention** — document
   `.codebase-memory/graph.db.zst` (commit once, teammates skip reindex)
   alongside the `.repospec.json` habit. *[docs]*
6. **ADR management (optional / stretch)** — surface `manage_adr` so
   architectural decisions persist across sessions, complementing `memory.rs`.

## Explicitly NOT cherry-picked

- The tree-sitter grammar engine, Hybrid-LSP resolver, Nomic embeddings,
  Cypher engine, 3D UI — all consumed via MCP, never vendored into aish.
- aish's own `src/memory.rs` (facts + sqlite-vec recall) stays as-is; it solves
  a *different* problem (durable agent/user facts, not code structure). The two
  are complementary, not overlapping.

## Sequencing

Phase 1 (must): enrollment installer + `.mcp.json` glue + tool advertisement +
docs. Phase 2 (should): repospec auto-index handoff + git-diff impact in the
Phase-0 guard. Phase 3 (could/stretch): team artifact convention polish + ADR
surface + watcher lifecycle management.
