# Documentation Organization Plan

## Current State
- **34 markdown files** at root (many stale or task-specific)
- **6 subdirectories** (analysis, archive, backend-auth, design, plugins, spikes)
- **No clear navigation** — users must browse file listings

## Proposed Structure

```
docs/
├── INDEX.md                          # Navigation hub (NEW)
├── RELEASE.md                        # Consolidate: RELEASE-CHANNELS.md + RELEASING.md
├── ARCHITECTURE.md                   # High-level design (NEW — curated from existing)
│
├── reference/                        # Implementation details
│   ├── coordinator/
│   │   ├── patterns.md              # Consolidate: coordinator-patterns.md
│   │   ├── loop-guards.md           # Consolidate: coordinator-loop-guards.md
│   │   └── stale-row-prevention.md  # Consolidate: coordinator-stale-row-prevention.md
│   ├── plugins/
│   │   ├── memory.md                # Consolidate: plugin-memory-schema.md
│   │   ├── state.md                 # Consolidate: plugin-state-schema.md
│   │   └── webhook-events.md
│   ├── codebase-memory/
│   │   └── graph-artifacts.md       # Consolidate: codebase-memory-graph-artifact.md
│   ├── database.md                  # Consolidate: DATABASE_PATHS.md
│   └── git-cache.md                 # Consolidate: git-repo-cache.md
│
├── internals/                        # Deep-dives & proposals
│   ├── error-diagnostics.md
│   ├── hooks-design.md              # Consolidate: aish-hooks-design.md
│   ├── host-brokered-sibling-spawn.md
│   ├── serial-chain-yield-diagnosis.md
│   ├── telemetry-efficiency.md
│   ├── skillfish-integration.md
│   └── sqlite-vec-integration.md
│
├── archive/                          # Completed, superseded, or exploratory work
│   ├── MANIFEST.md                  # Manifest of archived docs (what & why archived)
│   ├── spikes/                       # Old exploration & prototypes
│   │   ├── analysis/
│   │   ├── backend-auth/
│   │   └── orca-analysis.md
│   ├── S6-rewrite-preview.md        # Preview of rewrite (completed)
│   ├── S7-miette-diagnostics/       # Old S7 track (superseded by error-diagnostics.md)
│   │   ├── eng-spec.md
│   │   └── prd.md
│   ├── S7.2-structured-toolresult-prd.md
│   ├── S7.4-tests-docs-scope.md
│   ├── session-scoped-jobs/         # Session jobs exploration (completed)
│   │   ├── implementation.md
│   │   └── proposal.md
│   ├── TASK-268-webhook-handler-dispatch.md
│   └── BUILD_ORDER_TASK285-298.md
│
├── formats/                          # Output format specs
│   ├── emoji.md                     # Consolidate: emoji-output-format.md
│   ├── colorized.md                 # Consolidate: colorized-output.md
│   └── skill-format.md              # Consolidate: SKILL-FORMAT.md
│
└── [existing subdirs kept]
    ├── design/
    ├── plugins/
    ├── analysis/
    └── backend-auth/
```

## Key Changes

| File(s) | Action | Rationale |
|---------|--------|-----------|
| `RELEASE-CHANNELS.md` + `RELEASING.md` | **Merge → `RELEASE.md`** | Single source of truth for release workflows |
| `coordinator-*.md` (3 files) | **Move → `reference/coordinator/`** | Group coordinator logic |
| `plugin-*.md` (3 files) | **Move → `reference/plugins/`** | Group plugin schemas |
| `S7.* + S6.4` | **Archive → `archive/`** | Completed design phases; preserve history |
| `TASK-268-*` + `BUILD_ORDER_*` | **Archive → `archive/`** | Task-specific; no ongoing relevance |
| `session-scoped-jobs*` | **Archive → `archive/session-scoped-jobs/`** | Completed exploration |
| `emoji-*.md` + `colorized-*.md` + `SKILL-FORMAT.md` | **Move → `formats/`** | Specs for output serialization |
| **INDEX.md** (new) | **Create** | Navigation hub linking all sections |
| **ARCHITECTURE.md** (new) | **Create** | System design overview (curated from existing) |
| **`archive/MANIFEST.md`** (new) | **Create** | Document why each archived file is archived |

## Migration Path

1. Create new subdirectories (`reference/`, `internals/`, `formats/`)
2. Move files per the plan above
3. Update internal cross-references (links in remaining files)
4. Create INDEX.md (navigation hub)
5. Create ARCHITECTURE.md (high-level system design)
6. Create archive/MANIFEST.md (context for archived work)
7. Verify no broken links
8. Commit: "docs: reorganize into tiered structure with archive"

## Outcome

- **Clear navigation:** INDEX.md as the entry point
- **Organized by use case:** reference (implementation), internals (deep-dives), formats (specs)
- **Preserved history:** archive/ with MANIFEST.md explaining each entry
- **Reduced clutter:** root level has only 4 core files + subdirs
