# Spawn budget: depth, width, and total

A runaway dispatch needs one of three things: **depth** (nested coordinators),
**width** (one coordinator forking a huge wave), or **volume** (a loop trickling
workers out forever). aish bounds all three.

| Dimension | Guard | Env var | Default | What it bounds |
|-----------|-------|---------|---------|----------------|
| Depth | `worker::spawn_budget_gate` | `AISH_SPAWN_BUDGET` | 3 | Levels of nested coordinator in one chain |
| Width | `spawn_ledger` | `AISH_SPAWN_WIDTH_MAX` | 8 | Workers **concurrently in flight** from this run |
| Total | `spawn_ledger` | `AISH_SPAWN_TOTAL_MAX` | 32 | Workers dispatched over this run's **whole lifetime** |

Depth alone is not sufficient: at depth 1 — a coordinator that never nests —
width and volume are unbounded. Fifty siblings in one wave, or hundreds over a
long run, all pass the depth budget because the chain never got deeper than one
level. The width and total ceilings close that hole at *every* depth.

## Semantics

- **Width is a gauge.** It rises when a worker starts and falls when it
  finishes. The reservation is an RAII `SpawnTicket`, so the slot is released on
  every exit path — normal return, early return, error, panic — with no manual
  release to forget. A `Drop` at the floor saturates instead of wrapping, so a
  stray double-release can't wedge the guard shut for the rest of the run.
- **Total is a monotonic counter.** It only rises, and is *not* released when a
  worker finishes, so a run cannot evade it by waiting for workers to drain.
- **Scope is one aish run.** The ledger is process-global, which is exactly the
  "per-run" unit the caps describe. A child coordinator is a separate process
  with a fresh ledger of its own — its spawns are bounded by its own width/total
  caps *and* by the decremented depth budget it inherits.
- **`0` disables a cap.** This matches the ordinary reading of a `*_MAX` knob and
  gives operators an escape hatch. An unparseable or absent value falls back to
  the default, mirroring `AISH_SPAWN_BUDGET` — a typo can neither uncap nor
  zero-cap the guard.

## Enforcement

Two layers, deliberately:

1. **Pre-flight (`spawn_ledger::check`)** — a cheap non-mutating check in the
   dispatch gate, so an over-budget call is refused *before* a worktree is taken
   and a process is forked, with a message naming the knob and the live counts.
2. **Launch site (`SpawnTicket::acquire`)** — the authoritative guard. Width is
   reserved with a compare-and-swap loop, so two threads racing at the ceiling
   cannot both win: the check and the increment are one atomic step. The
   lifetime counter is bumped only *after* the width slot is secured, so a
   refused dispatch never consumes lifetime budget.

The pre-check can go stale between the check and the launch — that is precisely
why it is only a fast path and never the sole gate.

When both ceilings are full, the refusal reports **total**: it is the terminal
condition (waiting cannot clear it), so surfacing it is strictly more useful
than reporting a width stall the operator would then sit and retry.

## Refusals

Both denials are explicit errors on the dispatch call — not a silent drop, not a
queue:

```
per-run spawn WIDTH budget reached (8/8 workers already in flight;
AISH_SPAWN_WIDTH_MAX=8) — refusing to dispatch another concurrent worker. …
Wait for an in-flight worker to finish, narrow the wave, or raise
AISH_SPAWN_WIDTH_MAX if you understand the host-load risk.
```

```
per-run spawn TOTAL budget exhausted (32/32 workers dispatched by this run;
AISH_SPAWN_TOTAL_MAX=32) — refusing to dispatch another worker. …
Finish or re-scope the current work, or raise AISH_SPAWN_TOTAL_MAX if you
understand the runaway risk.
```

Each names the offending env var and the actual numbers, so the model (or the
operator reading the log) can act on it instead of just seeing a "no".

## Tuning

```sh
AISH_SPAWN_WIDTH_MAX=16 aish     # wider waves, more host load
AISH_SPAWN_TOTAL_MAX=0  aish     # disable the lifetime ceiling
```

Raise width when a genuinely-independent fan-out needs more parallelism than the
default eight and the host can carry it — each worker is a full aish process
with its own git worktree. Raise total for a long-running coordinator that
legitimately works through many cards in one run. Disabling either (`0`) removes
a backstop against the runaway case; prefer raising the number.
