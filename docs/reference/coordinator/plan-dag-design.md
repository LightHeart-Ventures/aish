# Delta Artifact + Plan DAG — Design

> **Card:** TASK-801 (`card_2e4aef456e16`) · **Sprint:** SPR-113 · **Status:** settled design
>
> This document is the single written source the rest of SPR-113 implements against.
> Downstream cards (802–810) must not re-decide anything recorded here.

---

## 1. Problem

The coordinator's intended loop has five steps:

```
review ask -> review state -> identify delta -> plan with deps -> fan out
```

An audit of the live prompt constants found steps **3 (identify delta)** and
**4 (plan with deps)** missing. The loop currently jumps from "I read the state"
straight to "here are the files I'll touch".

### Code evidence

All references verified against `main` at the time of writing.

| Location | What it is | What it does — and does not do |
|---|---|---|
| `src/coordinator.rs:104` | `PHASE0_GUARD` | Answers exactly one boolean: *"is this already shipped?"* It greps for the key symbol, checks `git log` / `git branch` / `gh pr list`, and calls `background_status`. It never answers **"what is left?"** |
| `src/coordinator.rs:132-137` | `PHASE_PIPELINE` → Phase 1 DISCOVERY | Batch-reads state: "Front-load EVERY context read you know you'll need in ONE batch". It gathers state and then **stops** — it never asks the model to state the subtraction. |
| `src/coordinator.rs:142` | `PHASE_PIPELINE` → Phase 2 PLANNING | "Decide the smallest correct change and the **exact list of files to touch**." **This is the exact point where dependency information is discarded.** A file list has no edges. |
| `src/coordinator.rs:974` | `RE-EVALUATE THE PLAN AFTER TRIAGE — don't over-decompose` | A discretionary suppression: "Only fan out when sub-problems are truly independent." It asks the model to *judge* independence it was never given the data to compute. |
| `src/goal.rs:645-664` | `inflight_directive` → `claim_work_package` | The existing work-package lease. Opens `CoordinatorStore` on `crate::db_paths::main_db_path()` (`src/db_paths.rs:48`), reads the run forest, and claims one lease per live descendant run (`src/coordinator_store.rs:1127`). |

### Root cause

**Phase 2 emits a file list, so fan-out has nothing to key off.**

Fan-out needs to answer two structural questions — *which units are unblocked?*
and *which of those are file-disjoint?* — and a file list answers neither. With
no graph, the model must *guess* independence every turn. Unstructured fan-out
built on that guess once produced an **87-call runaway** (the incident cited in
both `PHASE0_GUARD` and `PHASE_PIPELINE`).

The `don't over-decompose` directive at `src/coordinator.rs:974` was added in
response to that runaway. It **suppressed the symptom, not the cause**: it tells
the coordinator to fan out less, rather than giving it the structure that makes
fan-out decidable. The fix is to restore the discarded information.

### One drift correction on the lease

The lease is often described as "keyed on run-id shape". Precisely, as
implemented today:

- the lease **key** is `work_package_key(task)` (`src/coordinator_store.rs:207`)
  — a normalized digest of the *task brief text*;
- the **owner** is the run id.

So cross-turn identity rests on the brief text matching. If a later turn
re-words the same work package, the digest changes and the lease no longer
recognises it. That is the heuristic this design replaces (see §5, *Lease
keying*).

---

## 2. Delta Artifact

The explicit `(ask − state)` record, produced at the **Phase 1 → Phase 2
boundary** and persisted.

```rust
pub struct DeltaArtifact {
    pub scope_key: String,
    pub ask_summary: String,      // <=60-char restatement + ask digest
    pub state_summary: String,    // what Phase 1 DISCOVERY found
    pub delta_items: Vec<String>, // what the ask needs that state lacks
    pub out_of_scope: Vec<String>,
    pub created_at: i64,          // unix seconds
}
```

### Field semantics

| Field | Meaning |
|---|---|
| `scope_key` | The coordinator/goal scope this delta belongs to — the same scope string the work-package ledger is keyed under. Primary key. |
| `ask_summary` | A ≤60-character restatement of the ask plus an ask digest. Short on purpose: it is re-injected on every resume, so it must be cheap. |
| `state_summary` | What Phase 1 DISCOVERY actually found on disk / in the board / in CI. The "state" half of the subtraction. |
| `delta_items` | The concrete things the ask requires that the current state does not yet provide. This is the *output* of the subtraction and the input Phase 2 plans against. |
| `out_of_scope` | Explicitly excluded work. Recorded so a later turn cannot quietly re-admit it. |
| `created_at` | Unix seconds. Lets a resume decide whether the delta is still current. |

### Lifecycle

1. **Written at Phase 1 exit**, before any planning happens.
2. **Re-injected into context on resume** by the same mechanism as the in-flight
   work-package directive (`src/goal.rs:645-664` — read the store, render a
   directive block, prepend it to the next turn's guidance).

### Why persistence matters

Today the subtraction happens *inside a single model turn* and is never written
down. The moment the conversation is compacted, the only record of
`ask − state` is gone — so the coordinator **re-derives it**, and it can
re-derive it *differently*. Two turns of the same run can disagree about what is
left to do, and about what was ruled out of scope. `PHASE0_GUARD` does not
rescue this: it only ever answered the boolean "already shipped?".

Persisting the artifact makes the subtraction a durable fact instead of a
per-turn re-derivation. This is the same reasoning that already justifies the
in-flight work-package ledger — a fresh process must not re-decide what an
earlier turn already settled.

---

## 3. Plan Graph

What Phase 2 must emit **instead of** a bare file list.

```rust
pub type NodeId = String;

pub struct PlanNode {
    pub id: NodeId,              // stable slug, ^[a-z0-9][a-z0-9-]{0,47}$
    pub intent: String,
    pub files: Vec<String>,
    pub depends_on: Vec<NodeId>,
    pub acceptance: Vec<String>,
    pub est_calls: u32,
}

pub struct PlanGraph {
    pub scope_key: String,
    pub nodes: Vec<PlanNode>,
    pub created_at: i64,
}
```

### `PlanNode` field semantics

| Field | Meaning |
|---|---|
| `id` | Stable lowercase-kebab slug, unique within the graph, matching `^[a-z0-9][a-z0-9-]{0,47}$`. The durable handle for the node across turns, leases, and dispatches. |
| `intent` | What this node is for, in prose. The core of a worker brief. |
| `files` | The paths this node touches. Doubles as the **disjointness key** for fan-out: two nodes sharing any path are not independent. |
| `depends_on` | Node ids that must land before this node may be dispatched. These are the edges that a file list discarded. |
| `acceptance` | The node's acceptance criteria. Lives here so a dispatch is self-contained (see §4). |
| `est_calls` | Rough tool-call budget for the node. Sizing signal — it is what lets a runaway be noticed as a node that blew its estimate rather than as an undifferentiated 87-call blob. |

### `PlanGraph` field semantics

| Field | Meaning |
|---|---|
| `scope_key` | Same scope string as the `DeltaArtifact` and the work-package ledger. Primary key — one graph per scope. |
| `nodes` | The nodes, in **declaration order**. Order is load-bearing: it is the deterministic tie-break for `ready_set` and `topo_order`. |
| `created_at` | Unix seconds. |

The graph is a DAG: cycles are a validation error, not a runtime condition.

---

## 4. Decisions

The three questions the card asked to settle.

| Question | Decision | Rationale |
|---|---|---|
| **Serialization** | A `serde_json` blob in the **existing** coordinator SQLite db (`crate::db_paths::main_db_path()`), via three new tables:<br>`plan_graph(scope_key PK, json, created_at)`<br>`plan_node_done(scope_key, node_id, done_at, PRIMARY KEY(scope_key, node_id))`<br>`delta_artifact(scope_key PK, json, created_at)` | Same db file and the same `CoordinatorStore` handle as the existing work-package ledger. **No new database, no new dependency**, and the migrations ride the existing schema-setup path. `plan_node_done` is a separate table rather than a field on the blob so marking a node done is a single row insert and never rewrites the graph. |
| **Node id scheme** | **Caller-supplied stable slug** — lowercase kebab, unique within the graph, regex-validated against `^[a-z0-9][a-z0-9-]{0,47}$`. **Not** a content hash. | The id must **survive compaction**. A content hash changes the instant the model re-words `intent` — which is exactly what happens after a compaction — so a hash-keyed node silently becomes a *new* node. A slug persisted with the graph and re-injected is never re-derived, so it is stable by construction. This is the same failure the text-digest lease key has today (§1). |
| **Where acceptance criteria live** | On `PlanNode.acceptance` (`Vec<String>`). | Makes a worker brief **self-contained**: dispatching a node hands over `intent` + `files` + `acceptance` as one object. No second lookup, nothing to re-derive on the worker side, and the node carries its own definition of done. |

---

## 5. Derived rules

### `ready_set(done)`

```
ready_set(done) = nodes whose `id` is NOT in `done`
                  AND whose every `depends_on` entry IS in `done`,
                  returned in declaration order
```

Declaration order makes the result deterministic, so two turns reading the same
graph with the same `done` set compute the same ready set.

### Fan-out derivation

Fan-out stops being a judgement call and becomes a computation over the ready
set:

| `ready.len()` | `files` pairwise disjoint? | Decision |
|---|---|---|
| 0 | — | Nothing dispatchable — the graph is done or fully blocked |
| 1 | — | **Solo** |
| ≥ 2 | **yes** | **Fan out** — one worker per ready node |
| ≥ 2 | no | **Solo** (merge or chain the colliding nodes instead) |

Formally: fan out when `ready.len() >= 2` **AND** the ready nodes' `files` sets
are pairwise disjoint; otherwise execute solo.

Two invariants hold unconditionally:

- **Never dispatch a node with an unmet dependency.**
- **Never dispatch two nodes touching the same file.**

This replaces the discretionary directive at `src/coordinator.rs:974`. The old
rule asked the model to *decide* whether sub-problems were "truly independent";
the graph makes independence something the coordinator *reads off*. And when
triage collapses the remaining work to one root cause, the ready set collapses
to one node and the coordinator runs solo **by construction** — the outcome the
anti-decompose directive was trying to achieve by instruction.

### Lease keying

The work-package lease key becomes the plan **`NodeId`** instead of the current
text-digest-of-the-brief shape (`work_package_key(task)`,
`src/coordinator_store.rs:207`, claimed at `src/coordinator_store.rs:1127` via
`src/goal.rs:645-664`).

Two consequences:

1. **Cross-turn resumption becomes exact rather than heuristic.** A re-worded
   brief currently produces a different digest and therefore a different lease;
   a slug persisted with the graph does not change, so a resuming turn
   recognises the *same* work package.
2. **Two parallel siblings get two unambiguous leases.** Fanning out two ready
   nodes produces two distinct `NodeId` keys, so each worker holds its own lease
   and neither can be mistaken for the other.

---

## 6. Downstream card map

| Card | Scope |
|---|---|
| **TASK-801** (this card) | This design doc — `docs/reference/coordinator/plan-dag-design.md` |
| **TASK-802** | `src/plan.rs`: `PlanNode` / `PlanGraph` types, `validate`, `ready_set`, `topo_order`, `fan_out_candidates` |
| **TASK-803** | Persist the delta artifact at the Phase 1 → Phase 2 boundary |
| **TASK-804** | `save_plan_graph` / `load_plan_graph` / `mark_node_done` in the coordinator store |
| **TASK-805** | Rewrite `PHASE_PIPELINE` Phase 2 to emit a plan object, not a file list |
| **TASK-806** | Add the DELTA directive to the Phase 1 exit |
| **TASK-807** | Derive fan-out from the ready set; retire the discretionary anti-decompose rule |
| **TASK-808** | Key the work-package lease on plan node id |
| **TASK-809** | Tests: ready-set / topo-sort / cycle detection, plus prompt-constant assertions |
| **TASK-810** | Document the loop in `docs/reference/coordinator/patterns.md` |

Two pairs must stay in lockstep:

- **TASK-802 ↔ TASK-807** — `PlanGraph::fan_out_candidates` and the prompt's
  fan-out rule must state the *same* condition (`>= 2 ready AND pairwise-disjoint
  files`). Code twin and prompt twin.
- **TASK-806 ↔ TASK-803** — the Phase 1 exit directive must ask for exactly the
  fields `DeltaArtifact` persists (`ask_summary`, `state_summary`, `delta_items`,
  `out_of_scope`), so the prompt and the schema cannot drift apart.
