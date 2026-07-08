# Preventing stale "coordinating" coordinator rows

## Symptom

A background coordinator's durable row (`coordinator_runs` in `aish.db`, surfaced
by `:workers` / `background_status`) is stuck showing **`coordinating`** even
though the actual work finished. Concrete case: `w_zrdvGyJC` resolved the merge
conflict for PR **#444** and the PR **merged at 2026-07-03T16:41:26Z**, yet the
row never advanced to `done`.

## Root cause

The `coordinator_runs` row is written by the **child** coordinator process
(`coordinator::drive`, `src/coordinator.rs:498`):

1. On startup the child `INSERT`s its row as `coordinating`.
2. The child does the real work (conflict resolution, PR merge).
3. The child is **responsible for finalizing** its own row to `done`/`failed`.

Step 3 is the single point of failure. If the child exits **after** doing the
work but **before** writing the terminal phase — SIGKILL, panic, container
teardown, host reboot, a dropped DB write, or a hang that outlives the process —
the row is orphaned in `coordinating` **forever**. There was no writer other than
the (now dead) child, so nothing ever corrected it.

## Fix (implemented)

**Parent-side reconciliation as a safety net.** The parent
(`worker::run_worker`) is the process that spawned the child and it **outlives**
it: `child.wait()` returns only once the child is dead. The parent holds the
**same** `run_id` (the child adopts the worker's visible id as its coordinator
run id) and now also carries a clone of the launching session's host
`CoordinatorStore` (`WorkerSpec.coordinator_store`).

After `child.wait()` returns and the in-memory job has been finalized,
`run_worker` reads the durable row:

- If the phase is **terminal** (`done`/`failed`) → the child finalized it; the
  child's record is authoritative and is **left untouched**.
- If the phase is still **non-terminal** (`coordinating`, `awaiting_batch`, …) →
  the child died without finalizing. The parent patches it from ground truth:
  - child exited `0` → `set_done` with the child's `result.txt` (or captured
    stdout as fallback),
  - child exited non-zero → `set_failed` with the exit description,
  - child timed out / was killed → `set_failed` noting the timeout.

Because the child is provably dead when the parent reads the row, there is **no
write race** — the parent only ever touches a row no live process can advance.
For launch paths without a store (goal `run_once`, tests) and container runs
whose DB is not host-shared, `result_for_run` returns `None` and reconciliation
is a no-op (the periodic salvage sweep covers those).

Touch points:
- `src/worker.rs` — `WorkerSpec.coordinator_store` field, `phase_needs_reconcile`
  predicate, and the reconciliation block at the end of `run_worker`.
- `src/repl.rs` (×3) and `src/tools.rs` (×1) — populate the new field from
  `session.coordinator_store.clone()` at every `WorkerSpec` construction site.

## Why this is the right layer

- The parent is the **only** actor guaranteed to be alive at the exact moment the
  child's fate is known, and it already knows the `run_id`. No new process,
  timer, or heartbeat needed.
- It is a **pure safety net**: zero behavior change on the happy path (child
  finalizes → row already terminal → parent skips).
- It closes the reported local-dev case completely (parent and child share the
  same host `aish.db`).

## Complementary hardening (recommended follow-ups)

1. **Periodic salvage sweep** — a startup/interval reaper that finds
   `coordinating` rows whose `run_id` has no live child PID (and/or is older than
   `WORKER_TIMEOUT + margin`) and marks them `failed (orphaned)`. This covers the
   cases parent-side reconciliation cannot: parent itself crashed, or a
   container run whose DB is not host-shared.
2. **Liveness/heartbeat column** — stamp `updated_at` each turn; treat a
   `coordinating` row with a stale heartbeat as orphaned in the status readout so
   even an un-reconciled row never *reads* as live.
3. **`:workers` display guard** — cross-check a `coordinating` row against the
   in-memory job table / PID; render "coordinating (no live process — stale?)"
   when there is no live worker, so the UI degrades honestly.
