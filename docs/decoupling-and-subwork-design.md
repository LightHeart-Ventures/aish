# Design: vendor-neutral aish + the sub-worker / sub-work split

**Status:** proposed
**Scope:** two coupled problems in one design, because they share one root cause —
behavior that should be *derived from capability* is instead *hardcoded by name*.

1. aish's code and agent prompts hardcode **Atum**. Atum is an *optional*
   integration we chose to make, not part of the shell.
2. A worker has no stated rule for when work becomes a **sub-worker** (another
   process) versus **sub-work** (a tracked item it does itself), so the choice is
   improvised per run.

---

## Part 1 — Atum decoupling

### 1.1 Current state (verified, file:line)

| # | Site | Class | What it does | Blast radius |
|---|---|---|---|---|
| 1 | `src/session.rs:1534` | **prompt, unconditional** | Base system prompt carries "Atum project context: … look up the linked Atum board via `atum_list_projects(repositoryName=…)`" | Every session, every turn, even with no MCP servers connected. Locked in by `system_prompt_carries_atum_project_context_directive` (`session.rs:2319`). |
| 2 | `src/session.rs:1624` (`BATCH_NUDGE`) | prompt | "…MCP servers (read/write files, run programs, atum/github, …)" | Every batch-mode session |
| 3 | `src/tools.rs:604` | prompt (tool description) | `run_in_background` description repeats "atum/github" | Every batch-mode session |
| 4 | `src/tools.rs:589` | prompt (cosmetic) | `get_skill` example is `atum/sprint-status` | Cosmetic |
| 5 | `src/skills.rs:244` | **code** | `INTERACTIVE_MCP_SKILLS = ["atum/should-i-hire-an-agent", "atum/pick-model"]` — the interactive shell's entire MCP-skill allowlist is two Atum skill names | Non-Atum tenants get an **empty** interactive skill surface |
| 6 | `src/turn_audit.rs:121` | **code, on-disk** | Turn journal writes to `<cwd>/.atum/run-<id>.jsonl`; `worker.rs` adds `.atum` to git excludes in three places | aish's own telemetry lives in a directory named after another product |
| 7 | `src/tools.rs:4816`, `:4852`, `src/mcp.rs:14` | **code, on-disk** | `${profile:KEY}` resolves from `~/.atum/credentials` | aish's credential store is in another tool's config dir. `worker.rs:2366` already calls this class of thing "a stray port artifact". |
| 8 | `src/secrets.rs:114` | code | `atum` in `KNOWN_SECRET_CONSUMERS` seed allowlist | Minor; a vendor binary in a core security default |
| 9 | `src/engine.rs:2071` | code (cosmetic) | Glyph special-case on the `atum_` tool prefix | Cosmetic |
| 10 | `src/redact.rs:425`, `src/sensitive.rs:47` | code | `.atum/credentials` in sensitive-path lists | **Keep.** Defense in depth for a path that exists on real machines. |

Two distinct failure modes, worth separating:

- **Prompt coupling (1–4)** makes every agent believe a specific SaaS exists.
  Item 1 is the worst: it is unconditional, so a user with zero MCP servers is
  still told to call `atum_list_projects`, a tool they do not have.
- **Namespace coupling (5–8)** puts aish's own state — skills, journals,
  credentials — under another product's name, which is a durability and
  portability bug independent of any opinion about Atum.

What is already clean and should stay that way: `Goal.linked_tasks` /
`TaskRef.key` (`goal.rs:1501`) is a **free-form string**. There is no
Atum-specific validation, no `TASK-\d+` regex gate, and no board API call in the
goal path. The work-item data model is already vendor-neutral; only the prompts
that populate it are not.

### 1.2 Desired state

1. The core shell knows about **capabilities**, never vendors. "A project
   tracker is connected" is a runtime fact derived from the MCP catalog, not a
   string literal in a prompt.
2. No vendor name appears in an **unconditional** prompt. Vendor-specific
   guidance is emitted only when that vendor's tools are actually present.
3. aish's own on-disk state lives under `~/.aish` and `<repo>/.aish`.
4. Removing Atum entirely degrades aish gracefully: fewer tools, no behavior
   change, no empty allowlists, no prompts referencing missing tools.

### 1.3 Design

**D1. Capability-derived tracker directive (fixes 1, 2, 3, 4).**

Delete the hardcoded paragraph from the base prompt template. Add to
`session.rs` a conditional block built from the connected MCP catalog:

```rust
/// Emitted ONLY when a tracker-capable MCP server is connected. Names no vendor:
/// the tool names come from the live catalog.
fn tracker_context_block(cat: &McpCatalog) -> String // "" when none
```

Capability detection is by **declared capability, then shape, never by server
name**:

- preferred: the server declares `capabilities: ["project-tracker"]` in
  `.mcp.json` (new, optional, operator-authored key);
- fallback: a tool whose name matches `*_list_projects` *and* one matching
  `*_list_*tasks*` exists.

The emitted text is generic and interpolates the real tool names:

> Project context: when in a git repo and asked about project-level actions
> (sprints, tasks, board, issues), resolve the linked project with
> `<tool>(repositoryName=<owner/repo>)` before concluding none exists. The
> repository link is authoritative — never match by name alone.

`BATCH_NUDGE` and the `run_in_background` description drop "atum/github" for
"your connected MCP servers". The `get_skill` example uses a neutral
`<server>/<skill>` form. The `session.rs:2319` test is **inverted**: it must now
assert the base prompt contains *no* vendor literal, plus a new test that the
block appears when (and only when) a tracker capability is present.

**D2. Capability-based interactive skill filter (fixes 5).**

`INTERACTIVE_MCP_SKILLS` becomes a *default tag set*, not a name list. Select by
skill tag/category — `routing`, `decision` — with an operator override
(`AISH_INTERACTIVE_SKILLS`, or `interactive_skills` in config). Absent any
match, fall back to **all** MCP skills rather than none: a missing allowlist must
not silently blank the interactive surface.

**D3. Namespace reclamation (fixes 6, 7, 8).**

| Item | Change | Back-compat |
|---|---|---|
| Turn journal | `<cwd>/.atum/` → `<cwd>/.aish/` | On attach, if `.atum/run-<id>.jsonl` exists and `.aish/` does not, read the old path for replay (one release), write the new. Git excludes list both. |
| `${profile:KEY}` | `~/.aish/credentials` is primary | Read `~/.atum/credentials` as a fallback when the aish file is absent; log once at `debug`. Dropped after one release. |
| `KNOWN_SECRET_CONSUMERS` | Drop `atum` from the compiled seed | Already grantable with `:secrets allow-program atum`; document in the release note. |
| Glyph map | Replace the `atum_` special-case with a data-driven `&[(prefix, glyph)]` table | None needed. |

`redact.rs` / `sensitive.rs` keep `.atum/credentials` **and gain**
`.aish/credentials`, with a comment stating this is deliberate: the list
protects paths that exist on disk regardless of what aish itself uses.

### 1.4 Rollout

| Phase | Content | Gate |
|---|---|---|
| P1 | D1 + D2 (prompt + allowlist; no on-disk change) | New tests: base prompt vendor-free; tracker block present ⇔ capability present; skill filter non-empty without Atum |
| P2 | D3 journal + glyph rename, dual-read | Journal replay round-trips from both paths |
| P3 | D3 credentials dual-read, `KNOWN_SECRET_CONSUMERS` trim | `${profile:}` resolves from either path |
| P4 | Drop both fallbacks | Release note |

---

## Part 2 — sub-worker vs sub-work

### 2.1 Current state

Three mechanisms exist; only two are wired to each other.

**A. Sub-worker (another process).** `run_in_background` → `worker.rs`. Gets its
own git worktree and branch, its own context, its own full toolset. Guarded in
code by `spawn_budget_gate()` (`worker.rs:2048`): **depth** (`AISH_SPAWN_BUDGET`,
default 3), **width** (`AISH_SPAWN_WIDTH_MAX`), **total**
(`AISH_SPAWN_TOTAL_MAX`), plus an atomic `spawn_ledger` ticket at launch.

**B. Sub-work (items inside one run).** `src/subwork.rs`. Max 5 items
(`MAX_SUBWORK_ITEMS`, enforced in `ingest`, not merely requested), **one shared
worktree** — the parent's own `session.cwd`, with no API that could give an item
its own worktree — a `SUBWORK PLAN/RUNNING/DONE/FAILED` marker grammar, a
changed-only progress block, and `verify()`/`finalize()` which require **every**
declared item to be `done` and append the verdict to the final answer.

**C. Work items.** `Goal.linked_tasks: Vec<TaskRef>` — a free-form key, a cached
title, a `done` flag, persisted as JSON in `db.rs`. Nothing creates a work item
when a sub-worker spawns (`create_project_task` has no match in `src/`); the
`:workers` view links the two by *matching the key against the worker's task
text*.

**Visibility (landed in #1105).** Sub-work is no longer invisible. The ledger
persists in a durable `coordinator_runs.subwork` JSON column, is surfaced on the
escalation tray as `Kind::SubWork`, and uses `sw:{parent}:{item}` row ids that
cannot collide with run ids. `sync_subwork` reaps retracted items and `sweep`
reaps orphans. The tray row shows the item and its state without opening the
worker.

**The gap.** Two prompt constants state criteria, and they do not agree on the
same axis:

- `FAN_OUT_DERIVED` (`coordinator.rs:176`) is the **sub-worker** rule and is
  genuinely structural: fan out only when ≥2 plan nodes are ready, every ready
  node declares a non-empty `files` set, and those sets are pairwise disjoint.
  Empty `files` means unknown, not disjoint, and collapses the wave to solo.
- `SUBWORK_DECOMPOSITION` (`subwork.rs:48`) is the **sub-work** rule and says
  only "split into ≤5 items, same worktree, don't dispatch a sub-worker for
  one."

So a worker is told *how* to fan out and *how* to decompose, but never **which
of the two a given piece of work is**. Nothing connects the sub-work ledger to
the plan graph, and `verify()` is not tied to the "open the PR" step — the
WRAPPING UP directive fires independently of whether the ledger verified.

### 2.2 Desired state

The operator's criterion, stated as the normative rule:

> A worker that needs to implement a feature touching 3 files builds a plan,
> spawns **sub-work** to update each file, and when the sub-work is all done,
> verifies, then submits the PR.

Generalized:

**Sub-work (same process, same worktree, ledger-tracked) is the DEFAULT.**
Choose a **sub-worker** only when sub-work cannot express the piece.

| Signal | Sub-work | Sub-worker |
|---|---|---|
| Shares the worktree / needs one PR | ✅ | ❌ |
| File scope known and ≤ ~5 cohesive units | ✅ | — |
| Needs its own branch or separate PR | ❌ | ✅ |
| Needs a different baseline (`base: main` vs this branch) | ❌ | ✅ |
| Context cost would blow this run's window | ❌ | ✅ |
| Genuinely parallel **and** file-disjoint **and** ≥2 ready nodes | ❌ | ✅ |
| Different repo or different cwd | ❌ | ✅ |
| Deferrable — operator does not need it in this run | ❌ | ✅ |

Tie-break: **if the pieces must land in one PR, they are sub-work.**

Rationale update: part of why workers reached for sub-workers was that sub-work
was invisible to the operator. Since #1105 it appears on the escalation tray, so
the reason to spawn a sub-worker just to get tracking is gone. The default
should be sub-work, and the sub-worker lane should need one of the listed
signals. That single
sentence resolves most real cases, including the operator's 3-file example.

### 2.3 Design

**D4. One decision block, two outcomes.** Replace the two disjoint prompt
constants with a single `WORK_DECOMPOSITION` directive in `coordinator.rs` that
presents the choice explicitly, defaults to sub-work, and keeps
`FAN_OUT_DERIVED`'s structural test as the *sub-worker* branch's precondition:

```
DECOMPOSE, THEN CHOOSE A LANE (sub-work is the default).
1. Plan first: list the units of work and, for each, the files it touches.
2. For each unit, pick a lane:
   - SUB-WORK (default) — same worktree, lands in ONE PR, you do it yourself,
     tracked on the ledger (max 5). Use this for every unit whose files are
     known and whose change belongs in this PR.
   - SUB-WORKER — only if the unit needs its own branch/PR, a different
     baseline, a different repo/cwd, or is deferrable; AND the fan-out test
     passes (>=2 ready nodes, each with a non-empty `files` set, pairwise
     disjoint).
3. If the units must land in ONE pull request, they are SUB-WORK. Period.
4. When every sub-work item is DONE, verify (build + test + read the diff),
   THEN open the draft PR. A failed or unfinished item blocks the PR and is
   reported.
```

**D5. Make step 4 a code gate, not a request.** Today `verify()` only annotates
the final answer. Change: when the ledger is non-empty, the coordinator's
wrap-up path checks `verify()` **before** the PR step. Not verified → skip
`gh pr create`, keep the branch, and report the failing/unfinished items with
`git status`. This is the same philosophy as `MAX_SUBWORK_ITEMS` being enforced
in `ingest`: a rule the model can talk past is not a rule.

**D6. Declare `files` on each sub-work item.** Add an optional `files` field to
`SubWorkItem` and to the `SubWorkRow` ledger view (#1105). Because the ledger is
JSON in `coordinator_runs.subwork` and `SubWorkItem` already derives serde, the
field is additive: existing rows deserialize with `files` empty, which means
unknown, not disjoint. Extend the marker grammar to
`SUBWORK PLAN: <id> | <intent> | files: a.rs, b.rs` (third field optional for
back-compat). Two wins: it feeds the same disjointness test the sub-worker lane
uses, so the lane choice is computed from one data shape; and it lets the ledger
flag two items declaring the same file, which is the in-run analogue of the
collision `FAN_OUT_DERIVED` prevents across workers.

**D7. Work items are a tracker concern, not a spawn concern.** Keep the current
behavior — spawning does **not** create a board card. Add instead: when a tracker
capability is connected (Part 1's D1), the coordinator may link an existing work
item to the run via `TaskRef`. The free-form `TaskRef.key` stays the only
contract, so a GitHub-issue key, a Jira key, or nothing at all are equally
valid. Also replace the `:workers` substring match between `linked_tasks` keys
and worker task text with an explicit `task_key` field on the dispatch; keep the
substring path as a fallback for one release.

### 2.4 The operator's example, traced

| Step | Lane | Mechanism |
|---|---|---|
| "Implement feature touching 3 files" | — | Worker plans; 3 units, files declared |
| Units must land in one PR | **sub-work** | D4 step 3 |
| Update file A / B / C | sub-work ×3 | `SUBWORK PLAN:` ×3, shared worktree, ≤5 cap |
| All done | — | `verify()` requires all 3 `done` |
| Verify | code gate | build + test, D5 |
| Submit PR | — | `gh pr create --draft`, allowed only post-verify |

No sub-worker is spawned anywhere in that flow — correct, because one PR is the
deliverable. Had the ask been "implement the feature **and** separately migrate
an unrelated repo", the second half is a different repo → sub-worker.

### 2.5 Testing

| Area | Test |
|---|---|
| D1 | Base prompt contains no vendor literal; tracker block present ⇔ capability present; block interpolates live tool names |
| D2 | Skill filter returns non-empty with a non-Atum catalog |
| D3 | Journal replays from legacy `.atum` and new `.aish`; `${profile:}` resolves from either credentials path |
| D4 | `WORK_DECOMPOSITION` states the one-PR tie-break and the fan-out precondition (const-level assertion, as `subwork.rs:707` already does) |
| D5 | Unverified ledger ⇒ PR step skipped and items reported; verified ⇒ PR step runs |
| D6 | `files:` parsed, optional, back-compatible; two items claiming one file are flagged |
| D7 | `task_key` on dispatch links the item; substring fallback still resolves |

### 2.6 Non-goals / risks

- Not proposing *removal* of the Atum MCP server. It stays a first-class
  optional integration — this is about the shell not assuming it.
- D5 can block a PR on a flaky test. Mitigation: the report names the failing
  item and the branch is kept, so the operator can finish manually; nothing is
  lost, only auto-PR is withheld.
- D6 risks models under-declaring `files`. Mitigation: reuse the existing
  semantics — empty means *unknown*, not *disjoint*.
