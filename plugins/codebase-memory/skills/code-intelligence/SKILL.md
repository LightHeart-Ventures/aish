---
name: code-intelligence
description: Graph-first code discovery for this repo via the codebase-memory MCP server. Use INSTEAD OF grep/glob when finding definitions, callers, impact, data-flow, architecture, or hot paths — search_graph, trace_path, get_architecture, query_graph, get_code_snippet.
categories: [discovery, review, perf, design]
applies-to: [all]
unwanted-for: [ui]
---

# code-intelligence — query the code knowledge graph

The `codebase-memory` MCP server indexes this repo into a queryable graph
(functions, classes, routes, CALLS/DATA_FLOWS/IMPORTS edges, complexity
signals, cross-service links). Reach for these tools **before** grep/glob for
any *structural* question — they return resolved symbols and relationships, not
raw text hits.

## First move

If nothing is indexed yet (`list_projects` empty, or you just opened the repo):

```
index_repository { repo_path: "<repo root>", mode: "moderate" }
```

Modes: `full` (all files + similarity/semantic edges), `moderate` (filtered +
semantic), `fast` (no semantic), `cross-repo-intelligence` (match routes/channels
across projects — needs `target_projects`).

## Pick the right tool

| Question | Tool |
|----------|------|
| "Where is X defined?" / natural-language find | `search_graph { project, query: "..." }` (BM25; add `semantic_query: ["..."]` to bridge vocabulary) |
| "Who calls X?" / "what does X call?" | `trace_path { project, function_name, direction: inbound\|outbound, mode: calls }` |
| "How does this value flow?" | `trace_path { ..., mode: data_flow, parameter_name }` |
| "Cross-service call path?" | `trace_path { ..., mode: cross_service }` |
| "Big-picture architecture / modules" | `get_architecture { project }` (includes Leiden clusters) |
| "Read the source of a symbol" | `search_graph` to get `qualified_name`, then `get_code_snippet { project, qualified_name }` |
| "Complex multi-hop / aggregate" | `query_graph { project, query: "<Cypher>" }` |
| "What changed + blast radius" | `detect_changes { project, since\|base_branch, depth }` |
| "Grep, but ranked by structure" | `search_code { project, pattern, mode: compact }` |

## Hot-path / bottleneck hunt (Cypher)

Function & Method nodes carry complexity props: `complexity` (cyclomatic),
`cognitive`, `loop_depth`, `transitive_loop_depth`, `linear_scan_in_loop`,
`alloc_in_loop`, `recursion_in_loop`, `unguarded_recursion`, `recursive`.

```cypher
MATCH (f:Function)
WHERE f.transitive_loop_depth >= 3 OR f.linear_scan_in_loop >= 1
RETURN f.qualified_name, f.transitive_loop_depth, f.linear_scan_in_loop
ORDER BY f.transitive_loop_depth DESC
```

## Workflow tips

- **Resolve first, read second.** `get_code_snippet` wants an exact
  `qualified_name` — get it from `search_graph`, don't guess.
- **Paginate.** `search_graph` caps results; check `has_more`/`total` and page
  with `offset` for broad queries. Narrow via `label`, `file_pattern`,
  `min_degree` first.
- **Impact analysis before edits.** `trace_path {direction: inbound}` on the
  function you're about to change shows every caller you might break.
- **Stale graph?** Re-run `index_repository` (or `detect_changes` to see drift)
  after big edits. aish auto-indexes on repo-open when the gate is on.

The graph replaces the grep→read→grep ping-pong for structural discovery — one
`search_graph` or `trace_path` usually answers what three greps would only hint
at.
