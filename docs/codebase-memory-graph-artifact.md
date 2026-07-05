# Team-shared codebase-memory graph artifact (`.codebase-memory/graph.db.zst`)

**Status:** convention (docs-only) · **Sprint:** SPR-071 · **Card:** TASK-409
**Related:** [`docs/analysis/codebase-memory-mcp-integration.md`](analysis/codebase-memory-mcp-integration.md) (feature #5), the `.repospec.json` habit

## Why

aish consumes [`DeusData/codebase-memory-mcp`](https://github.com/DeusData/codebase-memory-mcp)
as an MCP server (see the integration analysis). That server indexes a repo into
a persistent SQLite **knowledge graph** — functions, classes, call chains, HTTP
routes, cross-service links — parsed from the AST across 158 languages. The first
index of a large repo costs real wall-clock time (Linux kernel, 28M LOC, ~3 min;
a typical service repo, seconds to tens of seconds) and CPU.

Left alone, **every teammate and every fresh CI checkout pays that reindex cost**.
codebase-memory ships a committable, compressed snapshot of the graph —
`.codebase-memory/graph.db.zst` — precisely so the cost is paid **once**: commit
the artifact, and teammates (and CI) decompress it and skip the full reindex. On
first run the server decompresses the snapshot and **incrementally** diffs it
against the current tree (via the git-diff → affected-symbols path), reindexing
only what changed since the snapshot was cut.

Treat this exactly like the [`.repospec.json` habit](#relationship-to-repospec):
a small, durable, committed artifact that front-loads discovery so the first
coordinator turn starts warm instead of cold.

## The artifact

| | |
|---|---|
| Path | `.codebase-memory/graph.db.zst` |
| Contents | zstd-compressed SQLite knowledge graph (nodes = symbols, edges = call/route/link relations) |
| Producer | the codebase-memory MCP server (`export`/snapshot on demand or via the git-watcher) |
| Consumer | the same server on any checkout — decompress, then incremental diff-reindex |
| Typical size | ~1–15 MB compressed for a service repo; tens of MB for a monorepo |
| Regenerable? | Yes — it is a **cache**. Deleting it only forces a one-time reindex; it is never the source of truth. |

## Commit / refresh workflow

1. **Cut a snapshot** after a meaningful structural change has landed on the main
   branch (new modules, large refactor, route surface change). Ask the MCP server
   to export the graph, which writes `.codebase-memory/graph.db.zst`.
2. **Commit it** on the same branch/PR as (or immediately after) the structural
   change, so the snapshot tracks a known-good tree state.
3. **Teammates pull** and the next codebase-memory query decompresses the snapshot
   and incrementally reindexes only the delta — no full cold index.
4. **Refresh cadence:** the artifact does not need to be perfectly current — the
   incremental diff closes any gap on first use. Refresh it opportunistically
   (e.g. per release, or when the delta grows large enough that first-run
   reindex becomes noticeable), **not** on every commit. Do **not** auto-commit
   it from the background git-watcher; that would produce noisy, conflict-prone
   diffs on a binary blob.

## `.gitignore` / `.gitattributes` guidance

The **snapshot** (`graph.db.zst`) is meant to be committed. The **working
database** and any scratch/index state the server keeps alongside it are **not** —
they are per-checkout and churn constantly. Ignore everything under
`.codebase-memory/` except the snapshot:

```gitignore
# .gitignore — codebase-memory: ignore the live DB, keep the shared snapshot
.codebase-memory/*
!.codebase-memory/graph.db.zst
```

Mark the snapshot as binary so Git never tries to line-diff or EOL-normalize it,
and so tooling treats it as an opaque blob:

```gitattributes
# .gitattributes
.codebase-memory/graph.db.zst binary -diff -text
```

### Git LFS (optional, for large monorepos)

`graph.db.zst` is already zstd-compressed, so Git's own zlib packing gains
little. For a small service repo the committed blob is fine in-tree. For a large
monorepo where the snapshot runs to tens of MB and refreshes often, track it with
[Git LFS](https://git-lfs.com/) to keep the packfile lean:

```gitattributes
# .gitattributes (LFS variant — pick ONE of the two blocks, not both)
.codebase-memory/graph.db.zst filter=lfs diff=lfs merge=lfs -text
```

Trade-off: LFS keeps clones fast but requires the LFS client in CI and on every
teammate's machine. Prefer plain in-tree until size actually hurts.

## CI implications

- **Fast path:** when the snapshot is present, CI decompresses it and diff-reindexes
  the delta instead of cold-indexing — shaving the codebase-memory warm-up off the
  job. Cache `.codebase-memory/` between runs (keyed on the artifact hash) for an
  even faster warm start.
- **Never gate the build on the snapshot.** It is a cache: a stale or absent
  `graph.db.zst` must degrade to "index on demand", never fail the pipeline.
- **Keep it out of merge-conflict paths.** Because it is a binary blob, two PRs
  that both refresh it will conflict irreconcilably. Refresh it on `main`
  (or a dedicated maintenance PR), not in parallel feature branches, and let the
  incremental diff absorb the drift on feature branches.
- **Provenance, not correctness:** the snapshot need not match HEAD exactly. CI
  correctness never depends on it — only warm-up speed does.

## Relationship to repospec

`.repospec.json` and `.codebase-memory/graph.db.zst` are complementary committed
artifacts that both front-load discovery:

| | `.repospec.json` | `.codebase-memory/graph.db.zst` |
|---|---|---|
| Shape | small hand/agent-curated JSON map | compressed machine-generated graph DB |
| Scope | high-level architecture map (entrypoints, modules, key files, patterns) | fine-grained symbol/call/route graph |
| Read by | the coordinator directly (cheap, human-readable) | the codebase-memory MCP tools (`get_architecture`, `trace_path`, `search_graph`, `semantic_query`) |
| Source of truth? | authored intent — kept in sync by hand/agent | a **cache** — regenerable from the tree at any time |
| Refresh | when structure changes | opportunistically; incremental diff closes gaps |

Adopt both with the same reflex: on first entry to a repo, ensure `.repospec.json`
exists and is accurate, and ensure a `graph.db.zst` snapshot is present (or cut
one) so the codebase-memory tools answer structural queries warm from turn one —
replacing the grep → read → grep loops that the 5-phase pipeline targets.
