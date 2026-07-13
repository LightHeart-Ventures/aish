# `:workers` — show the full coordinator tree (transitive children), attachable

## Problem

`:workers` only lists the **direct** children of the current REPL session. When a
coordinator (`w_0mWEFj86`) itself spawns workers, those grandchildren are
**invisible** to `:workers`, even though `background_status()` (the global
registry) shows them:

| System | Shows |
| --- | --- |
| `background_status()` / API | All jobs (global registry) |
| `:workers` REPL command | Direct children of this REPL session only |

Users cannot see, `:attach`, or shift-tab to a coordinator's sub-workers.

## Root cause

Two independent facts combine:

1. **Display source.** `collect_worker_rows` (repl.rs) builds the `:workers`
   table **only** from `session.worker_jobs` — the in-memory pipe handles for
   workers *this process* spawned directly. Transitive descendants have no
   in-memory handle here, so they never appear.
2. **No parent linkage.** `worker_command` (worker.rs) stamps
   `AISH_LAUNCH_SESSION_ID` on every child (so the durable `coordinator_runs`
   row is attributed to the *originating* interactive session) but never stamps
   the child's **own** run id (`AISH_RUN_ID`) nor its **parent's** run id. So a
   coordinator can't know its own id, `requested_by_worker` in the spawn broker
   is always `None`, and nothing records who spawned whom.

Because `AISH_LAUNCH_SESSION_ID` *is* propagated through the nested/broker spawn
path, a transitive grandchild's `coordinator_runs.session_id` **already equals
the originating REPL session id**. The durable store therefore already has the
rows — `:workers` just isn't reading them, and has no parent column to nest them.

## Design

### 1. Parentage env (foundation)

In `worker_command`, stamp two ids on every spawned child:

- `AISH_RUN_ID = <child run_id>` — the child's own durable id, so a coordinator
  finally knows its identity (also fixes `requested_by_worker` in the spawn
  broker, read from `AISH_RUN_ID`).
- `AISH_PARENT_RUN_ID = <spawning process's own AISH_RUN_ID>` — omitted at the
  top-level REPL (which has no `AISH_RUN_ID`), so a direct child records an empty
  parent and renders as a root.

### 2. Durable `parent_run_id`

- Idempotent additive migration: `ALTER TABLE coordinator_runs ADD COLUMN
  parent_run_id TEXT` (duplicate-column error swallowed, matching the existing
  `session_name` / `stand_down` migrations).
- `CoordinatorRow` gains `parent_run_id: Option<String>`; `load_all` selects it.
- `insert()` takes a `parent_run_id` argument and stamps it; `coordinator::drive`
  passes `std::env::var("AISH_PARENT_RUN_ID").ok()` when it inserts its own row.

### 3. Hierarchical `:workers`

- `WorkerRow` gains display-only `parent_id: Option<String>` and `depth: usize`.
- A pure, unit-tested `build_worker_forest(rows) -> Vec<WorkerRow>` orders rows
  as a stable pre-order forest: roots (no known parent within the set) newest
  first, each followed by its children indented one level. Cycles / missing
  parents degrade gracefully to roots.
- `:workers` merges two sources, de-duplicated by run id:
  - in-memory `session.worker_jobs` (live, this session's direct children), and
  - durable `CoordinatorStore::load_all()` rows reachable from this session —
    `session_id == my session` **or** an ancestor chain that leads to such a row.
  The merged set is passed through `build_worker_forest`; the table/modal indent
  the `task` cell by `depth` and show the parent coordinator as the root row.

### 4. Attach + shift-tab to transitive workers

- The `:attach <id>` / shift-tab rotation id-set is widened from "in-memory
  direct children" to "every row in the merged forest".
- A **live** transitive descendant streams through its parent coordinator, which
  this session does not hold a pipe to. Two-phase plan:
  - **This PR:** attach to a transitive worker resolves to **review mode** —
    replay its durable per-worker transcript (`~/.aish/workers/<id>/`) and its
    `coordinator_runs` result; steering is routed through the existing `:tell`
    mailbox (durable, keyed by run id), which already works cross-process.
  - **Follow-up:** live stdout fan-out from a parent coordinator to the
    grandparent session (a broker-style stream relay) for true live attach.

## Testing

- Pure unit tests for `build_worker_forest`: roots ordering, nesting depth,
  multi-level chains, missing-parent and cycle degradation.
- `coordinator_store` round-trip: `insert` with a parent then `load_all` returns
  the `parent_run_id`; migration is idempotent on a pre-existing table.
- CI gate: `cargo test --no-default-features --locked`.

## Rollout / back-compat

Rows created before the migration read `parent_run_id = NULL` → rendered as
roots, identical to today. No behavior change for sessions that never nest.
