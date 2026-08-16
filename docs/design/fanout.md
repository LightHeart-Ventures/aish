# Design — `:fanout` (prompt-race across N coordinators)

> Status: **Design** · Card: **TASK-405** · FR: **FR-336 #1** · Source: `docs/archive/spikes/orca-analysis.md` §1
>
> This is the design gate for the implementation cards **TASK-412** (`:fanout <n>` + `status`) and **TASK-413** (`:fanout compare | pick`). No runtime code lands in this card.

## 1. Summary & north-star

Orca's headline pattern is **Parallel Worktrees**: fan ONE prompt across N agents, each
in its own isolated git worktree, then compare the results and merge the winner.

aish already isolates every background job in a worktree and can fan work out
(`run_in_background`), but there is **no first-class "run ONE prompt across N agents,
then compare and merge the winner."** Today a human hand-spawns N coordinators and
eyeballs the branches.

`:fanout` closes that gap, **terminal-native** — no GUI, no Chromium diff viewer. One
prompt in, N racing coordinators, one winning branch out.

```
:fanout 3 "make the retry backoff jittered and add a unit test"
   → spawns 3 coordinators, branches fanout/jittered-backoff-{1,2,3}
:fanout status        → live rollup of the 3 candidates
:fanout compare       → diff-stat matrix + per-candidate summary
:fanout pick 2        → merge candidate 2, discard 1 & 3, GC worktrees
```

## 2. Command surface

| Command | Behaviour |
|---|---|
| `:fanout <n> <prompt>` | Validate `n` (see §6), derive a slug from `<prompt>`, spawn N coordinators on the **same** task, each `base: head`, into branches `fanout/<slug>-{1..n}`. Persist a fanout group (§5). Print the group id + the N branch names. |
| `:fanout status` | Thin wrapper over `background_status`, filtered to the active fanout group's run ids. Table: candidate #, branch, coordinator status, last heartbeat. |
| `:fanout compare` | For each candidate branch: `git diff --stat <base>...<branch>` + the coordinator's final-report summary. Rendered as a comparison table (files changed, +/- lines, one-line summary). |
| `:fanout pick <k>` | Merge `fanout/<slug>-k` into the base branch (fast-forward or merge commit), delete the other N-1 branches, GC their worktrees. Refuse if candidate `k` is still running (require `--force`). |

Sub-command dispatch lives behind the single `:fanout` verb so the palette stays clean.

## 3. Worktree / branch model

- Base branch = current `HEAD` at spawn time (captured into the group so later `pick`
  merges into the right place even if the operator has moved on).
- Each candidate: branch `fanout/<slug>-<i>` in its own worktree under the aish worktree
  root (same isolation the coordinator already uses — one worktree per background job).
- `<slug>` = kebab-cased, truncated (≤ 32 char) derivation of the prompt; collisions get
  a short hash suffix.
- Cleanup on `pick`/`abort`: `git worktree remove` + `git branch -D` for the losers.

## 4. Dispatch reuse (`coordinator.rs`)

Each candidate is a normal `run_in_background` coordinator:

- Same `task` string (the operator's prompt) for all N.
- `base: head` so every candidate branches from the captured base.
- The returned coordinator run id is stored on the candidate row (§5).

No new dispatch path — `:fanout` is an **orchestration layer** over the existing
background-coordinator machinery + worktree isolation + `background_status`. This keeps
the blast radius small and reuses the spawn-budget/turn-budget guards already in place.

## 5. Result-collection model

New persistence (spec'd here, built in TASK-412):

```
table fanout_group
  group_id      TEXT PRIMARY KEY   -- fanout_<hex>
  slug          TEXT
  base_branch   TEXT
  base_sha      TEXT
  n             INTEGER
  created_at    TEXT
  status        TEXT               -- running | compared | picked | aborted

table fanout_candidate
  group_id      TEXT               -- FK fanout_group
  idx           INTEGER            -- 1..n
  branch        TEXT               -- fanout/<slug>-<idx>
  run_id        TEXT               -- coordinator run id
  status        TEXT               -- running | done | blocked | failed
  PRIMARY KEY (group_id, idx)
```

`status`/`compare`/`pick` all key off `group_id`. There is at most one *active* group per
repo at a time in v1 (multi-group is a later stretch); `:fanout` with no sub-command
resolves to the most-recent non-terminal group.

## 6. Concurrency cap & validation

- `n` in `2..=8`. `n < 2` is rejected (that's just a normal background job). `n > cap`
  clamps with a warning.
- Cap default **4**, override `AISH_FANOUT_MAX`. Reuses the existing background spawn
  budget so a fanout can't exhaust the coordinator pool.
- If the spawn budget can't seat all N, spawn what fits and mark the rest `queued`
  (spawned as slots free) rather than hard-failing.

## 7. Lifecycle

```
:fanout n prompt ── spawn N coordinators ──▶ [running]
        │                                        │
        ▼                                        ▼
   group persisted                        :fanout status  (loop)
                                                 │
                              all candidates terminal
                                                 ▼
                                        :fanout compare  ──▶ [compared]
                                                 │
                                        :fanout pick k
                                                 ▼
                          merge k, delete losers, GC worktrees ──▶ [picked]
```

`abort` (implicit on `pick`, explicit via a future `:fanout abort`) stands down any
still-running losers via the existing coordinator stand-down flag before GC.

## 8. Open questions & non-goals

- **Non-goal:** cross-candidate merge (cherry-picking hunks from multiple winners). v1 is
  winner-take-all.
- **Non-goal:** automatic scoring / "best" selection — the operator picks. Auto-rank is a
  later enhancement that could ride on the `compare` summary.
- **Open:** should `compare` run the test suite per candidate? Deferred — expensive; v1
  shows diff-stat + coordinator self-report only.
- **Open:** multi-group concurrency (more than one active fanout per repo). Deferred to a
  follow-up once the single-group flow is proven.

## Cross-references

- **FR-336 #1** (this), **TASK-412** (`:fanout <n>` + `status`), **TASK-413**
  (`:fanout compare | pick`).
- Reuses: `coordinator.rs` dispatch, worktree isolation, `background_status`.
