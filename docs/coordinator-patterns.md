# Coordinator patterns — the 5-phase pipeline, batching & call-budget discipline

Audience: **coordinator developers** and **agents writing feature-dev tasks**
for aish's background coordinator (`src/coordinator.rs`). This note codifies the
workflow and the tool-efficiency rules that a coordinator should follow on every
`review → design → develop → open PR` run, and the guardrails the runtime
enforces when it doesn't.

It is the connective tissue between three shipped bodies of work:

- **Phase-0 existence guard** — TASK-355 (PR #546)
- **Token-efficiency sprint** — cache repair (TASK-320), run-length cap
  (TASK-321), ranged reads (TASK-322), MCP schema scoping (TASK-323), batching
  enforcement (TASK-324), token telemetry (TASK-325)
- **Loop-exhaustion guards** — see [`coordinator-loop-guards.md`](./coordinator-loop-guards.md)
  and [`coordinator-stale-row-prevention.md`](./coordinator-stale-row-prevention.md)

If you only read one section, read **§0 (Phase-0 guard)** and **§3 (batching)** —
they prevent the two most expensive failure modes: rebuilding shipped work, and
burning a turn per file.

---

## §1 · The 5-phase pipeline

A feature-dev coordinator run moves through **Phase 0 (a pre-flight guard)** and
then **five ordered phases**. Each phase has an entry condition, a definition of
done, and a single dominant tool-batch. Do not enter a phase before its
predecessor's DoD is met — that's what produces detail-first thrash (§2).

| Phase | Name | Goal | Dominant tools | Done when |
|---|---|---|---|---|
| **0** | **Existence guard** | Prove the work isn't already shipped/in-flight | `atum_get_project_task`, `gh pr list`, `git branch`, `background_status` | Target resolves to a real, unshipped item with no live worker (§0) |
| **1** | **Review / context** | Load *all* context in one front-loaded batch | `read_file` (ranged), `grep_files`, `glob_expand`, `list_dir` | You can state the change in one paragraph without another read |
| **2** | **Design** | Decide the smallest correct change | *(reasoning)* + targeted confirming reads | You know every file to touch and the test plan |
| **3** | **Develop** | Make the change + tests | `write_file`, `edit_file`, `cargo test` | Change compiles and the local gate passes |
| **4** | **Verify** | Green the canonical gate | `cargo test --no-default-features --locked` (§4) | CI-equivalent passes locally, or is dispatched |
| **5** | **Ship** | Branch, commit, PR | `git`, `gh pr create` | PR open on a feature branch, never pushed to `main` |

**Example — a docs task (this file):**

```
Phase 0  atum_get_project_task TASK-359 → real, col_plan, no branch/PR/worker  ✔
Phase 1  list_dir docs/ + read sibling docs (ranged) in ONE batch              ✔
Phase 2  decide: new file docs/coordinator-patterns.md, 6 sections, no code    ✔
Phase 3  write_file docs/coordinator-patterns.md                               ✔
Phase 4  docs-only → markdown lints; no cargo gate needed                      ✔
Phase 5  git checkout -b → commit → gh pr create                               ✔
```

---

## §0 · Phase-0 existence guard (do this FIRST, always)

**Rule:** before any design or build work, prove the referenced item *exists*
and is *not already done or in-flight*. Rebuilding shipped work is the single
most expensive coordinator mistake — it burns a full multi-round run and can
open a duplicate PR. This guard shipped as TASK-355 (PR #546).

Check, in one front-loaded batch:

1. **The item exists** — `atum_get_project_task <KEY>` returns a card (a 404 or a
   key that doesn't appear on the board means *stop and report a dead
   reference* — do **not** invent scope).
2. **No PR already ships it** — `gh pr list --search "<KEY> in:title" --state all`.
3. **No branch already holds it** — `git branch -a --list "*<slug>*"`.
4. **No live worker owns it** — `background_status` (a sibling coordinator
   `coordinating` on the same task ⇒ defer, don't race).

```
# Phase-0 guard — copy/paste, fill in KEY + slug
atum_get_project_task     taskId=<KEY>            # → real card? else STOP
gh pr list  --repo <owner/repo> --state all --search "<KEY> in:title"
git branch  -a --list "*<slug>*"
background_status         scope=all               # any live worker on this task?
```

If **any** check shows the work is shipped or in-flight: **stop and report** with
the PR/branch/worker id. Only when all four are clear do you proceed to Phase 1.

> This is also enforced at the prompt/runtime level, but the coordinator must
> still *perform* the checks — the guard is a habit, not just a backstop.

---

## §2 · Anti-patterns to avoid

These are the tool-usage smells the token-efficiency sprint targeted. Each wastes
a turn (round-trip latency + re-sent context) for no added information.

| Anti-pattern | What it looks like | Fix |
|---|---|---|
| **Serial read-act loop** | read one file → think → read the next known file → think … one call per turn | Front-load: fire *all* independent reads/greps/status in **one** batch (§3). Enforced by TASK-324 (PR #541). |
| **Per-file inspection** | `read_file` a whole 20 KB file to find one symbol | `grep_files` to locate, then `read_file` with `line_start`/`line_end`. Enforced by TASK-322 (PR #548): bulk reads > 5 KiB without line bounds are refused. |
| **Detail-first reasoning** | diving into implementation before the change is scoped | Do Phase 1→2 (breadth then decision) before Phase 3. Read the *map* (`.repospec.json`, `list_dir`) before the *territory*. |
| **Re-reading the same file** | reading a large file end-to-end repeatedly across turns | Read the slice you need once; keep the line range. The loop-guard flags duplicate full reads. |
| **Huge listings** | `list_dir` / `glob` a giant tree and scroll | Scope the glob (`src/**/*.rs`), cap results, or `grep_files` with a `glob` filter. |
| **Close-a-turn-to-think** | ending a turn on output you already have | Decide-then-act: go straight to the next action batch; don't spend a round-trip narrating. |

**The one dependency exception:** grep-then-read of the *same* file **is**
serial — you need the line number before the ranged read. Everything else
independent goes in one batch.

---

## §3 · Batching rules — when parallel is safe, when serial is required

The engine runs every tool call in a turn concurrently. So the decision is
purely: *does call B need call A's output?*

**Safe to batch (fire together in one turn):**

- Reads of *different* already-known paths (`read_file` a, b, c).
- A `grep_files` for symbol X **and** a `read_file` of a *different*, already-known path.
- Independent status lookups (`background_status`, `gh pr list`, `git status`).
- N independent `edit_file`s to *different* files.

**Must be serial (B depends on A):**

- `grep_files` → `read_file` of the **same** file (need the line number first).
- `git checkout -b` → `git commit` → `gh pr create` (ordered state transitions).
- Any write whose content depends on a value you haven't read yet.
- `git add`/`commit` after the `write_file` that produced the change.

Rule of thumb: **one up-front breadth batch** (all the context you know you'll
need), then **one action batch**, serializing only the genuine dependencies.
Three files you know you need is *one* turn of three reads — not three turns.

TASK-324 (PR #541) adds a loop-guard nudge that fires when a coordinator emits a
run of single-call, independent-read turns that could have been one batch.

---

## §4 · Call-budget enforcement (soft/hard limits + monitoring)

A coordinator run is bounded so a stuck loop can't burn tokens forever. Limits
come in two tiers.

**Soft budget (nudge / compaction):** as a run grows, context is compacted early
and the coordinator is nudged to converge (TASK-321, PR #547). Older messages are
offloaded to long-term memory and replaced with a `[Context compacted: …]`
banner; retrieve them with `recall(query="context-offload")` if needed. The
pinned task block always survives compaction — re-read it, not the banner, when
unsure.

**Hard budget (circuit breaker + turn cap):** the runtime refuses to *start* a
run whose identical task text has already terminated `failed` ≥ N times, and caps
rounds per run. See [`coordinator-loop-guards.md`](./coordinator-loop-guards.md).

| Env var | Default | Effect |
|---|---|---|
| `AISH_COORDINATOR_MAX_FAILED_ATTEMPTS` | `3` | Prior `failed` runs of the *same task text* before a new dispatch is refused. `0` disables the breaker. |
| `AISH_COORDINATOR_FAILED_KEEP` | *(bounded)* | Most-recent `failed` rows retained for forensics before the reaper trims. |
| `AISH_COORDINATOR_FAILED_MAX_AGE_DAYS` | *(bounded)* | Age after which `failed` rows are reaped. |

**Monitoring:**

- `background_status` — live table of every run (status, turns, tokens, result).
- `:tokens` — per-run / per-session token spend, in:out ratio, top-N runs
  (TASK-325, PR #543).
- `:telemetry` / `:reasoning` — tool-call and escalate-vs-guess aggregates
  (see [`telemetry-efficiency.md`](./telemetry-efficiency.md)).
- Turn-audit journal — `.atum/run-<id>.jsonl` logs each turn's tool calls **and**
  end-of-round synthesis, so a run emitting the same synthesis round after round
  is visibly looping.

**Decision point (bake into the run):** after ~3 failed attempts at the same
sub-goal, *stop and declare the blocker* (`atum_agent_task_update` event=complete
outcome=blocked) rather than loop. A clean blocker beats a burned budget.

---

## §5 · Recovery points — yield strategy & state snapshots

Coordinator runs are durable and crash-resumable. Design each run so an
interruption (max-rounds, panic, parent death, operator Ctrl-C, `:update`
restart) loses at most one round of work.

**State snapshots.** Each run is a row in the `coordinator_runs` SQLite table,
keyed by `run_id`. Turn state is written transactionally per round (TASK-285), so
a panic mid-round can't leave a half-written row. On restart, `rehydrate`
reconstructs runs from the durable worktree (the source of truth) even if the DB
row was lost, and salvaged orphans get a synthetic task string so they never trip
a real task's circuit breaker. Terminal `done` rows are purged on restart;
`failed` rows are retained (bounded) for forensics.

**Yield strategy.** Prefer to reach a *committable* checkpoint before a likely
yield:

- **Commit early, commit often** on the feature branch — a pushed branch survives
  any local restart, an uncommitted worktree edit does not.
- At a max-rounds checkpoint, save operator-handoff state (TASK-291): commit
  in-flight work, push the branch, draft the PR, and report status — never drop
  work on the floor.
- On stand-down (`stop`) or Ctrl-C, take **one** graceful wrap-up turn: preserve
  work (commit/push/draft-PR), report a status, then terminate. Do not blindly
  resume the interrupted action — re-read the task and any newer operator
  messages first.
- Use `message_console` for an out-of-band heads-up on a long run; it is **not** a
  substitute for the final result.

**Resuming.** A resumed run receives its recent conversation as context plus the
pinned task block. Re-read the pinned task (not the compaction banner) to recover
intent, verify what already shipped (repeat the Phase-0 guard — the world may
have moved), and continue from the next uncommitted step.

---

## Related

- [`coordinator-loop-guards.md`](./coordinator-loop-guards.md) — circuit breaker, turn cap, synthesis logging, decision points
- [`coordinator-stale-row-prevention.md`](./coordinator-stale-row-prevention.md) — durable-registry / orphan-salvage semantics
- [`telemetry-efficiency.md`](./telemetry-efficiency.md) — `:telemetry` / `:reasoning` cost knobs
- `.repospec.json` — repo map: build/test commands, module layout, guardrails
