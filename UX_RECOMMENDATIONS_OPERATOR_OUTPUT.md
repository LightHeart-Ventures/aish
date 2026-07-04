# Operator UI Output — 24h Analysis & 3 UX Recommendations

_Generated 2026-07-04 by an autonomous aish coordinator. Scope: all UI output presented to
operators in the trailing 24h across the two live operator surfaces — the **aish TUI**
(SecondStatusLine / OutputField / `:workers` / fired-`:alert` badges) and the **Atum operator
console** (orchestration-run feed + event/activity log for tenant `LightHeart Ventures`,
`t_5d2e99ddf596`)._

## What operators actually saw (evidence)

### A. Atum orchestration-run feed — drowning in no-op heartbeats
`atum_list_orchestration_runs` (most-recent 40) returned:

| Trigger | Runs (of 40) | Terminal status | Synthesis shown |
|---|---|---|---|
| `schedule:sprint-manager-heartbeat` (a92) | 39 | `cancelled` (every one) | none — ~2s no-op |
| `TaskMovedTrigger` (a165, eng-manager) | 1 | `cancelled` | none |

- Cadence is every **10 minutes** → **~144 sprint-manager heartbeat runs/day**, plus a second
  `schedule:lifecycle-conductor-heartbeat` stream (seen in the event log). **Every heartbeat run in
  the window terminated `cancelled` with no synthesis** (create→complete ≈ 2s = a no-op check).
- System total: **2,861 runs**. The one operator-meaningful run (a real `TaskMovedTrigger`
  engineering-manager run) is **1 row in 40** — buried ~40:1 under system heartbeats.

### B. Atum event/activity log — 2–4 raw rows per real event
`atum_list_events` (100 most-recent) is dominated by **raw, duplicated webhook envelopes** with
`actor: null` / `action: null`:

- `GithubPush` **+** `GithubPushReceived` fire as a **pair for the same push** (~20 pairs).
- `GithubCheckRunCompleted` **+** `GithubWorkflowRunCompleted` fire as a **pair for the same CI
  run** (~30+ combined), one per check — several per PR.
- Interleaved `OrchestrationRunRequested` **+** `OrchestrationRunCancelled` pairs from the same
  heartbeats as (A).
- Net: an operator scanning "what happened" reads **2–4 machine rows per single logical event**,
  most with no human actor and no pass/fail outcome attached.

### C. aish TUI footer — correct "most-recent-wins", but ephemeral
Reading the render path (`src/repl.rs::recent_message_row` + `coordinator_status_message`,
`src/alert.rs`, `src/style.rs::alert_badge`):

- The SecondStatusLine recent-message slot is a **most-recent-wins ticker, never a stack** — good,
  deliberate design. A fired `:alert` badge or finished-coordinator notice **replaces** the live
  hint (`session.flash`, dropped on the next attach/detach transition).
- Consequence: a fired-`:alert` / finished-worker **flash is transient** — the next state
  transition overwrites it, and the detailed OutputField line (`alert.rs::Fired.detail`) is printed
  once into scrollback with no recoverable tray. In a long session, **a notification the operator
  glances away from is effectively lost.**
- Severity is under-encoded: `alert_badge` is bold-yellow for alerts, but a **failed** coordinator /
  failed CI surfaces through the same neutral worker-badge path — no red-vs-green at the glance
  layer for the footer flash.

---

## 3 Recommendations

### 1. Collapse no-op system heartbeats out of the operator run feed
**Problem:** ~144+ `cancelled` heartbeat runs/day bury the ~1 meaningful run at ~40:1 (Evidence A).
**Fix:** Default the runs view to **hide `triggeredBy: schedule:*-heartbeat` runs that terminated
`cancelled` with no synthesis**, behind a "show system heartbeats" toggle. Better: **collapse a
consecutive heartbeat streak into one summary row** — e.g. `🫀 sprint-manager · 143 no-op checks
(24h) · 0 actions taken · last 20:40`. aish already has the primitive for this instinct
(`src/style.rs::job_activity_emoji` collapses same-class workers to one glyph); apply the same
"one row per class, not per fire" rule to the Atum feed. **Impact:** the meaningful-run signal goes
from 1-in-40 to top-of-list.

### 2. Coalesce the event log by _logical event_, not raw webhook
**Problem:** 2–4 duplicated `actor:null` webhook rows per real event (Evidence B).
**Fix:** Render **one operator row per logical event**: merge `GithubPush`+`GithubPushReceived`;
roll `GithubCheckRunCompleted`/`GithubWorkflowRunCompleted` **up under their PR** as a single
`CI: 6/6 green` (or `2 failed`) row; fold `OrchestrationRunRequested`+`…Cancelled` into the Rec-1
summary. Show the **human actor and the outcome** (pass/fail, merged/closed) instead of `null`.
This is the web-side twin of aish's own `recent_message_row` "replace, don't stack" discipline —
collapse redundant envelopes to the single line that carries meaning. **Impact:** the activity feed
becomes a human-readable timeline instead of a webhook tail.

### 3. Give the aish TUI a recoverable notification tray + severity color
**Problem:** footer flashes (fired `:alert`, finished/**failed** coordinator) are most-recent-wins
and **ephemeral** — overwritten on the next transition, unrecoverable once scrolled off; failures
aren't color-tiered at the glance layer (Evidence C).
**Fix (keep the single-line footer — it's right):** (a) **persist the last N flashes** to a
`:activity` / `:notifications` tray (reuse the `session.flash` plumbing + the existing history DB in
`src/db.rs`) so an operator who looked away can recover the missed detail line; (b) **tier the badge
color by severity** — keep alert=yellow, add **red** for failed coordinators / failed CI, green for
clean completion — so the footer flash reads pass/fail at a glance. **Impact:** no dropped
notifications in long sessions; failures are unmissable without adding footer clutter.

---

### Method / caveats
- Windows: `atum_list_orchestration_runs` (40 most-recent) and `atum_list_events`
  (100 most-recent, `since=2026-07-03T20:40Z`); tenant `t_5d2e99ddf596`. TUI findings are read from
  source (`repl.rs`, `alert.rs`, `style.rs`) — the render logic operators see, not a capture of one
  operator's literal scrollback (not retained centrally).
- Heartbeat counts are extrapolated from the observed 10-min cadence × 24h; the 40-run sample was
  100% `cancelled` heartbeats bar one, so the ratio is a floor, not an estimate.
