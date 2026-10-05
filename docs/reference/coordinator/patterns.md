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
- **Loop-exhaustion guards** — see [`loop-guards.md`](./loop-guards.md)
  and [`stale-row-prevention.md`](./stale-row-prevention.md)

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
| **1** | **Review / context** | Load *all* context in one front-loaded batch | `read_file` (ranged), `grep_files`, `glob_expand`, `list_dir` | You can state the change in one paragraph without another read **and** you have written the **DELTA** (`ask − state`) — the exit artifact of this phase (§6.2) |
| **2** | **Design** | Decide the smallest correct change | *(reasoning)* + targeted confirming reads | You have a **plan graph** — nodes carrying `files` **and** `depends_on`, not a bare file list (§6.3) — plus the test plan |
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

> **Scope note.** The Phase-0 guard answers exactly one boolean: *"is this
> already shipped?"* It never answers *"what is left?"* — that is the delta
> artifact's job (§6.2). Clearing Phase 0 is not the same as knowing the delta.

---

## §2 · Anti-patterns to avoid

These are the tool-usage smells the token-efficiency sprint targeted. Each wastes
a turn (round-trip latency + re-sent context) for no added information.

| Anti-pattern | What it looks like | Fix |
|---|---|---|
| **Serial read-act loop** | read one file → think → read the next known file → think … one call per turn | Front-load: fire *all* independent reads/greps/status in **one** batch (§3). Enforced by TASK-324 (PR #541). |
| **Per-file inspection** | `read_file` a whole 20 KB file to find one symbol | `grep_files` to locate, then `read_file` with `line_start`/`line_end`. Enforced by TASK-322 (PR #548): bulk reads > 5 KiB without line bounds are refused. |
| **Detail-first reasoning** | diving into implementation before the change is scoped | Do Phase 1→2 (breadth then decision) before Phase 3. Read the *map* (`docs/ARCHITECTURE.md`, `list_dir`) before the *territory*. |
| **Re-reading the same file** | reading a large file end-to-end repeatedly across turns | Read the slice you need once; keep the line range. The loop-guard flags duplicate full reads. |
| **Huge listings** | `list_dir` / `glob` a giant tree and scroll | Scope the glob (`src/**/*.rs`), cap results, or `grep_files` with a `glob` filter. |
| **Close-a-turn-to-think** | ending a turn on output you already have | Decide-then-act: go straight to the next action batch; don't spend a round-trip narrating. |
| **Bare file list out of Phase 2** | Phase 2 ends at *"here are the files I'll touch"* — no units, no edges | A file list has no edges, so fan-out has nothing to key off and has to guess. Emit a **plan graph** instead: nodes with `files` **and** `depends_on` (§6.3). TASK-805 (PR #872). |
| **Guessed fan-out** | dispatching N parallel workers on a hunch that the sub-problems are "truly independent" | Don't judge independence — *compute* it. Fan out iff ≥ 2 ready nodes with pairwise-disjoint `files`; otherwise solo (§6.4). TASK-807 (PR #872). |

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
rounds per run. See [`loop-guards.md`](./loop-guards.md).

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
  (see [`telemetry-efficiency.md`](../../internals/telemetry-efficiency.md)).
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

## §6 · Plan-DAG, delta artifact & derived fan-out

§1 tells a run *when* to think. This section says *what it must have written
down* at each hand-off — because state that lives only in the transcript is
erased by compaction and then re-derived, **differently**, on the next turn.

Rationale, the full field-by-field schema, and the rejected alternatives live in
[`plan-dag-design.md`](./plan-dag-design.md) — TASK-801 (PR #870). This section
is the quick reference, not a copy of it.

> **Merge status at time of writing (SPR-113).** The design (TASK-801, PR #870),
> the `src/plan.rs` types (TASK-802, PR #871) and the prompt constants
> (TASK-805/806/807, PR #872) are **open PRs, not yet on `main`**. Persistence of
> the delta (TASK-803) and of the graph (TASK-804), and the node-keyed lease
> (TASK-808), are still in flight. Every claim below is attributed to its owning
> card so you can tell shipped behaviour from in-review behaviour.

### §6.1 · The five-step loop

| # | Step | Question it answers | Surface that implements it |
|---|---|---|---|
| 1 | **review ask** | *What was I asked for?* | the pinned task block (survives compaction, §4); `atum_get_project_task` for a card-backed ask |
| 2 | **review state** | *What exists today?* | Phase-0 guard (§0) + the Phase-1 discovery batch (§1) |
| 3 | **identify delta** | *What is left?* | the **delta artifact** (§6.2) — `PHASE_PIPELINE` Phase-1 exit directive, TASK-806 (PR #872); persisted by TASK-803 |
| 4 | **plan with deps** | *In what units, in what order, touching what?* | the **plan graph** (§6.3) — `PHASE_PIPELINE` Phase 2, TASK-805 (PR #872); types in `src/plan.rs`, TASK-802 (PR #871); persisted by TASK-804 |
| 5 | **fan out** | *Solo, or N parallel workers?* | the **derived rule** (§6.4) — `FAN_OUT_DERIVED`, TASK-807 (PR #872), twinned with `PlanGraph::fan_out_candidates`, TASK-802 (PR #871) |

Steps 3 and 4 are the ones SPR-113 added. Before it, the pipeline went from
*"I read the state"* straight to *"here are the files I'll touch"* —
`PHASE_PIPELINE` Phase 2 on `main` (`src/coordinator.rs:142-143`) — and a file
list has no edges.

### §6.2 · The delta artifact — the `ask − state` record

Written at the **Phase 1 → Phase 2 boundary**, before any planning. Four content
fields (plus `scope_key`, the primary key, and `created_at`):

| Field | Holds |
|---|---|
| `ask_summary` | a ≤ 60-char restatement of the ask plus an ask digest. Short on purpose — it is re-injected on every resume, so it must be cheap. |
| `state_summary` | what Phase-1 discovery actually found on disk / on the board / in CI. The *state* half of the subtraction. |
| `delta_items` | the concrete things the ask requires that the current state does not yet provide. The output of the subtraction, and the input Phase 2 plans against. |
| `out_of_scope` | explicitly excluded work — recorded so a later turn cannot quietly re-admit it. |

**Why it is persisted rather than merely reasoned.** The subtraction used to
happen inside a single model turn and was never written down. The moment the
conversation is compacted the only record of `ask − state` is gone, so the
coordinator **re-derives it — and can re-derive it differently**: two turns of
the same run then disagree about what is left and about what was ruled out.
`PHASE0_GUARD` (`src/coordinator.rs:104`) does not rescue this; it only ever
answered the boolean *"already shipped?"*. Persisting the artifact turns the
subtraction into a durable fact — the same reasoning that already justifies the
in-flight work-package ledger: a fresh process must not re-decide what an
earlier turn settled.

**Re-injected on resume** by the same mechanism as the in-flight work-package
directive: `inflight_directive` (`src/goal.rs:645-664`) reads the store and
prepends a rendered directive block to the next turn's guidance.

Storage: `delta_artifact(scope_key PK, json, created_at)` — TASK-803.
Prompt side (the Phase-1 exit directive that asks for exactly these four
fields): TASK-806 (PR #872). The two are deliberately twinned so the prompt and
the schema cannot drift apart.

### §6.3 · The plan graph — what Phase 2 emits instead of a file list

`PlanNode` / `PlanGraph` / `NodeId` live in `src/plan.rs` — TASK-802 (PR #871).

| `PlanNode` field | Holds |
|---|---|
| `id` | stable lowercase-kebab slug, unique within the graph, matching `^[a-z0-9][a-z0-9-]{0,47}$`. The durable handle for the node across turns, leases and dispatches. |
| `intent` | what the node is for, in prose — the core of a worker brief. |
| `files` | the paths this node touches. Doubles as the **disjointness key** for fan-out: two nodes sharing any path are not independent. |
| `depends_on` | node ids that must land before this node may be dispatched. These are the edges a file list discarded. |
| `acceptance` | the node's acceptance criteria. They live on the node so a dispatch is **self-contained** — `intent` + `files` + `acceptance` is the whole brief, with nothing to re-derive worker-side. |
| `est_calls` | rough tool-call budget. The sizing signal that lets a runaway be seen as *this node blew its estimate* rather than as one undifferentiated 87-call blob. |

`PlanGraph { scope_key, nodes, created_at }`. Node **declaration order is
load-bearing**: it is the deterministic tie-break for `ready_set` and
`topo_order`, so two turns reading the same graph with the same done-set compute
the same answer. Cycles are a validation error (`PlanGraph::validate`), not a
runtime condition.

```
ready_set(done) = nodes whose `id` is NOT in `done`
                  AND whose every `depends_on` entry IS in `done`,
                  returned in declaration order
```

**Node ids are caller-supplied slugs, not content hashes** — deliberately. An id
must survive compaction. A content hash changes the instant the model re-words
`intent`, which is exactly what a compaction causes, so a hash-keyed node
silently becomes a *new* node. A slug persisted with the graph and re-injected is
never re-derived, so it is stable by construction. (This is the same failure mode
the current text-digest lease key has — see §6.6.)

Storage: `plan_graph(scope_key PK, json, created_at)` plus
`plan_node_done(scope_key, node_id, done_at)` — TASK-804. The done-set is a
separate table rather than a field on the blob, so marking a node done is one row
insert and never rewrites the graph.

### §6.4 · Derived fan-out

Fan-out is a **computation over the ready set**, not a judgement call:

| `ready.len()` | `files` pairwise disjoint? | Decision |
|---|---|---|
| 0 | — | nothing dispatchable — the graph is done or fully blocked |
| 1 | — | **solo** |
| ≥ 2 | **yes** | **fan out** — one worker per ready node |
| ≥ 2 | no | **solo** — merge or chain the colliding nodes instead |

Formally: fan out iff `ready.len() >= 2` **and** the ready nodes' `files` sets are
pairwise disjoint; otherwise execute solo in this turn.

Two invariants hold unconditionally:

- **Never dispatch a node with an unmet dependency.**
- **Never dispatch two nodes touching the same file.**

And the collapse case falls out for free: when triage narrows the remaining work
to a single root cause the ready set collapses to one node, so the run goes solo
**by construction** — no directive required. If a now-redundant fan-out is
already in flight, use `tell` to narrow or cancel the pointless peers rather than
letting them run.

> **Twin rule — keep these two in lockstep.** The condition is stated in exactly
> two places: the prompt constant `FAN_OUT_DERIVED` in `src/coordinator.rs`
> (TASK-807, PR #872) and the code `PlanGraph::fan_out_candidates` in
> `src/plan.rs` (TASK-802, PR #871). They must state the *same* condition — there
> is no third variant. TASK-809 asserts it.

### §6.5 · What this retired

Fan-out guidance used to be a **discretionary** directive in the coordinator
prompt: *"RE-EVALUATE THE PLAN AFTER TRIAGE — don't over-decompose … Only fan out
when sub-problems are truly independent"* (`src/coordinator.rs:974-981` on
`main`).

**Why it existed.** Unstructured fan-out once produced an **87-call runaway** —
the `w_nMYxaem3` run issued 87 serial tool calls before rate-limiting. That
incident is on the record in both `PHASE0_GUARD` (`src/coordinator.rs:104-119`)
and `PHASE_PIPELINE` (`src/coordinator.rs:132-135`), and the anti-decompose
directive was the response to it.

**Why it was replaced.** It suppressed the *symptom*, not the cause. It asked the
model to **judge** an independence it was never given the data to compute,
because Phase 2 emitted a bare file list and a file list has no edges. The
dependency graph replaces that judgement with a **structural** guard (§6.4):
independence is now something the coordinator *reads off* the graph rather than
guesses, and the outcome the old directive wanted — collapse to solo when the
work collapses to one cause — happens by construction.

The incident stays on the record here so that a future contributor who deletes
the sentence does not also delete the reason it was written.

### §6.6 · Where it lives

| Thing | Where | Card |
|---|---|---|
| `PlanNode` / `PlanGraph` / `NodeId`; `validate`, `ready_set`, `topo_order`, `fan_out_candidates` | `src/plan.rs` | TASK-802 (PR #871) |
| Phase-1 DELTA exit directive + Phase-2 plan-graph wording in `PHASE_PIPELINE`; the `FAN_OUT_DERIVED` constant | `src/coordinator.rs` | TASK-805 / TASK-806 / TASK-807 (PR #872) |
| `delta_artifact(scope_key PK, json, created_at)` | the **existing** coordinator SQLite db (`crate::db_paths::main_db_path()`) | TASK-803 |
| `plan_graph(scope_key PK, json, created_at)` + `plan_node_done(scope_key, node_id, done_at)` | same db, same `CoordinatorStore` handle | TASK-804 |
| Work-package lease re-keyed from the brief-text digest to the plan `NodeId` | `inflight_directive` → `claim_work_package`, `src/goal.rs:645-664` | TASK-808 |
| Tests: ready-set / topo-sort / cycle detection + prompt-constant assertions | `src/plan.rs`, `src/coordinator.rs` | TASK-809 |

**No new database and no new dependency.** All three tables ride the existing
`CoordinatorStore` schema-setup path — the same handle and the same db file the
work-package ledger already uses.

One drift note on the lease as it stands on `main` today: the key is
`work_package_key(task)` (`src/coordinator_store.rs:207`), a normalized digest
of the *task brief text*, claimed at `src/coordinator_store.rs:1127`; the owner
is the run id. Cross-turn identity therefore rests on the brief text
matching — re-word the same work package and the digest changes, so the lease no
longer recognises it. Keying on the plan `NodeId` (TASK-808) makes that
resumption exact rather than heuristic, and gives two fanned-out siblings two
unambiguous leases.

---

## Related

- [`plan-dag-design.md`](./plan-dag-design.md) — delta-artifact + plan-DAG schema, the three settled design decisions, and the fan-out derivation rationale (TASK-801, PR #870 — the file lands with that PR)
- [`loop-guards.md`](./loop-guards.md) — circuit breaker, turn cap, synthesis logging, decision points
- [`stale-row-prevention.md`](./stale-row-prevention.md) — durable-registry / orphan-salvage semantics
- [`telemetry-efficiency.md`](../../internals/telemetry-efficiency.md) — `:telemetry` / `:reasoning` cost knobs
- `docs/ARCHITECTURE.md` — repo map: build/test commands, module layout, guardrails
  (`.repospec.json` was removed from the repo in commit 3271425 and is no longer
  a source of this map)
