# Coordinator loop-exhaustion guards — review & implementation

Context: a batch of platform issues (ISS-2569/2568 max-turns exhaustion,
ISS-2566 turn-audit API, ISS-2001 context/memory enrichment) traced agent runs
that **loop until they exhaust their turn budget** instead of finishing or
declaring a blocker. The review produced six recommendations. This note records
which apply to **aish** (the local autonomous coordinator in this repo) versus
the **Atum platform** (the ECS-orchestrated agent fleet, tracked separately),
and what shipped in this PR.

aish's analogue of a platform "agent run on a card" is a **coordinator run**
(`src/coordinator.rs`): a durable, multi-round agentic loop keyed by `run_id`,
persisted to the `coordinator_runs` SQLite table. The same loop pathologies
apply, so the principles port even though the nouns differ ("card" → "task",
"orchestration run" → "coordinator run").

## Recommendation triage

| # | Recommendation | Tier | Where | This PR |
|---|----------------|------|-------|---------|
| 1 | max_iterations gate before dispatching — fail fast if prior runs on this card exceeded N failed attempts | Immediate | **aish** (principle) + Atum (per-card tracking) | ✅ implemented |
| 2 | Log tool calls + synthesis each turn to surface loops (turn-audit API, ISS-2566) | Immediate | **aish** (synthesis logging) + Atum (`atum_list_orchestration_run_turns` API) | ✅ partial (synthesis logging) |
| 3 | Increase the iteration limit temporarily as a bandaid (not durable) | Immediate | **aish** + Atum | ✅ implemented |
| 4 | Enrich invoke payloads (role, cardTitle, description, ACs) — ISS-2001 | Medium | **Atum** | ⏸ deferred |
| 5 | Add explicit decision_points to the prompt ("after 3 failed attempts, say 'I'm blocked because X'") | Medium | **aish** | ✅ implemented |
| 6 | Memory scoping by (agentId, role) — ISS-2001 | Medium | **Atum** | ⏸ deferred |

## What shipped here (aish)

### 1. Pre-dispatch circuit breaker (rec #1)
`coordinator::drive` now refuses to start a run when the **same task text** has
already terminated in `failed` ≥ N times (default **3**, env
`AISH_COORDINATOR_MAX_FAILED_ATTEMPTS`, `0` disables). Backed by
`CoordinatorStore::failed_attempts(task)`. The current run's own row is never
counted; only prior `failed` rows are. This fails fast instead of burning a full
multi-round attempt on a known-bad request.

*Durability boundary:* `clear_finished` purges terminal rows on a clean restart,
so this counter is effectively per-store-lifetime. It stops **in-session
re-dispatch storms** (e.g. a goal loop relaunching the same task), not a
cross-restart history — the same boundary the rest of the store's terminal
bookkeeping lives within. Durable cross-restart attempt history would need a
separate non-purged table; deferred as not worth the complexity for the
in-session loop case this targets.

### 2. Synthesis logging (rec #2, aish half)
Tier-1 turn audit (`src/turn_audit.rs`) already journals every **tool call**
(input + output) to `.atum/run-<id>.jsonl`. Added `TurnAudit::synthesis(round,
text)`, called by the coordinator after each round, to also journal the model's
**end-of-round narrative answer**. A run that emits the same synthesis round
after round is now visibly looping in the journal — the bare tool log alone can
hide that. Synthesis records carry a distinct `status: "synthesis"` and are
ignored by the replay loader, so the crash-resume contract is unaffected.

The **API surface** half of rec #2 (`atum_list_orchestration_run_turns`,
ISS-2566) is a platform feature and is out of scope here.

### 3. Configurable round cap (rec #3, the bandaid)
`MAX_ROUNDS` (was a hardcoded `36`) is now `DEFAULT_MAX_ROUNDS = 48` plus a
runtime override `AISH_COORDINATOR_MAX_ROUNDS` (clamped to `[1, 1000]`,
unparseable/out-of-range → default). This is the explicitly **non-durable**
bandaid: lift the cap without a rebuild when a legitimate task is starved, while
the real fixes (#1 breaker, #5 prompt) reduce wasted rounds in the first place.

### 5. Decision-point prompt (rec #5)
The coordinator's leading prompt now includes a `DECISION POINTS — avoid loops`
block instructing the model to stop re-trying a failing approach and instead
declare a concrete blocker ("I'm blocked because <reason>", what it tried, best
partial result). A clearly-stated blocker is framed as a **successful** terminal
outcome; an endless retry loop is a failure.

## Deferred to the Atum platform

- **#4 Enrich invoke payloads (ISS-2001):** role / cardTitle / description /
  acceptance-criteria enrichment of the agent invoke payload lives in the
  platform's dispatch path (`atum_invoke_agent` + orchestrator), not in aish's
  local loop. aish already front-loads its single "payload" (cwd + capability
  assertion + decision points + task); there's no card/role/AC model here to
  enrich.
- **#6 Memory scoping by (agentId, role) (ISS-2001):** the scoped-memory model
  (`atum_memory_*` tenant/project/agent scopes) is platform-side. aish's memory
  is a single local SQLite `memories` table with no agent/role dimension.

## Runtime knobs added

| Env var | Default | Range | Effect |
|---------|---------|-------|--------|
| `AISH_COORDINATOR_MAX_ROUNDS` | 48 | 1–1000 | Per-run agentic round cap (bandaid). |
| `AISH_COORDINATOR_MAX_FAILED_ATTEMPTS` | 3 | 0–1000 | Pre-dispatch circuit-breaker threshold; `0` disables. |

## Tests

- `db::tests::failed_attempts_counts_only_matching_failed_runs`
- `turn_audit::tests::synthesis_is_journaled_and_ignored_on_replay`
- `coordinator::tests::clamp_usize_parses_clamps_and_falls_back`

All green under `cargo test --no-default-features` (the CI gate).
