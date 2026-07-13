# Archive Manifest

This directory contains completed work, superseded design phases, and exploratory spikes. Each entry below explains what it is and why it's archived.

---

## Design Phases (S6, S7)

These represent completed multi-release feature cycles. Preserved for historical reference and to understand past architectural decisions.

| File | Status | Why Archived |
|------|--------|-------------|
| `S6-rewrite-preview.md` | ✓ Completed | Preview of the S6 rewrite (completed Q2 2026) |
| `S7-miette-diagnostics-prd.md` | ✓ Superseded | Diagnostics PRD; features rolled into `internals/error-diagnostics.md` |
| `S7-miette-diagnostics-eng-spec.md` | ✓ Superseded | Engineering spec for diagnostics; implementation complete |
| `S7.2-structured-toolresult-prd.md` | ✓ In Progress | PRD for structured tool result format (S7 phase 2) |
| `S7.4-tests-docs-scope.md` | ✓ In Progress | Scope doc for tests + docs phase (S7 phase 4) |

**How to use:** If you need historical context on why a design decision was made, check the relevant S-phase document. For current implementation details, refer to the `reference/` and `internals/` sections in the main docs.

---

## Task-Specific Work

Completed tasks and proposals with limited ongoing relevance.

| File | Status | Context |
|------|--------|---------|
| `TASK-268-webhook-handler-dispatch.md` | ✓ Completed | Design for webhook handler dispatch (task #268); implementation in `src/webhooks.rs` |
| `BUILD_ORDER_TASK285-298.md` | ✓ Completed | Build order for tasks #285–298 (Q2 2026 sprint planning) |

**How to use:** Reference only if tracing the history of a specific implementation. For current webhook docs, see `reference/../webhooks.md` in the main docs.

---

## Exploration & Prototypes

`spikes/` contains prototypes, analysis, and design exploration that informed later decisions but didn't ship directly.

| Directory / File | Status | Outcome |
|------------------|--------|---------|
| `spikes/analysis/` | ✓ Research | Earlier analysis work (see contents) |
| `spikes/backend-auth/` | ✓ Research | Backend authentication exploration |
| `spikes/orca-analysis.md` | ✓ Research | Orca cost analysis prototype (did not ship) |

**How to use:** These are educational — they show the design process and alternatives considered. For current implementation, refer to `reference/` sections.

---

## Completed Features

Features that were explored and either shipped or deprioritized.

| Directory | Status | Why Archived |
|-----------|--------|-------------|
| `session-scoped-jobs/` | ✓ Completed | Session-scoped job execution (completed; rolled into main coordinator design) |

**Contents:**
- `proposal.md` — Original proposal for session-scoped execution
- `implementation.md` — Implementation details (features shipped into main executor)

**How to use:** Historical reference for job execution design; current implementation is in `reference/coordinator/`.

---

## Migration Notes

This archive was created as part of a documentation reorganization (see `REORGANIZATION_PLAN.md` for details). Key changes:

- **Consolidated:** Multi-file topics (e.g. `S7-miette-*.md`) are grouped logically in the archive
- **Moved, not deleted:** All original content is preserved for full-text search and historical traceability
- **Redirected:** Active content moved to `reference/`, `internals/`, or `formats/` in the main docs
- **Updated links:** Cross-references in the main docs point to the new locations

---

## Accessing Archived Content

All archived files are still searchable and readable. You can:

1. **Browse by topic:** Look through this manifest for what you need
2. **Search full-text:** `grep -r "keyword" docs/archive/` or use your IDE's search
3. **Check links:** The main `INDEX.md` and `ARCHITECTURE.md` link to active content; archived content is referenced here

---

## Adding to the Archive

When a document becomes superseded or a task completes:

1. Move the file to `archive/` (or a subdirectory like `archive/spikes/`)
2. Add an entry to this MANIFEST.md with:
   - File/directory name
   - Status (e.g. ✓ Completed, ✓ Superseded, ✓ Research)
   - Brief explanation of why it's archived
   - Where related current content lives (if applicable)
3. Update any cross-references in the main docs to point to the new location

---

## Questions?

If you're unsure whether archived content is relevant to your current task:

- Check `INDEX.md` for the canonical location
- Read the "Why Archived" note above
- Search `reference/` and `internals/` for the current version
- Ask in #engineering or check recent commits for context

---

**Last updated:** 2026-07-08 (initial archive creation)
