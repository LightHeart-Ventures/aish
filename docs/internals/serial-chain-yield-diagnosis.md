# Diagnosis: recurring `serial-chain-yield` (`count=9`)

**Symptom (verbatim):**

```
[aish-stop tag=serial-chain-yield iterations=0 count=9] yielded after 9
consecutive single-call rounds (a deep serial chain) to re-plan toward
batching independent calls
```

This note explains exactly what emits that line, why we see it continuously,
and a ranked set of ways to reduce it. It is analysis only — no guard behaviour
is changed here.

## What emits it

| Piece | Location |
|-------|----------|
| Constant `SERIAL_CHAIN_YIELD_DEPTH = 8` | `src/loopguard.rs:646` |
| `SerialChainGuard::record` (counts lone-call rounds) | `src/loopguard.rs:688` |
| `ExitReason::SerialChainYield { depth }` + banner/detail | `src/loopguard.rs:287,337,370` |
| Guard constructed per turn (with env override) | `src/engine.rs:343` |
| Recorded each round on `turn.tool_calls.len()` | `src/engine.rs:575` |
| Deferred yield after tool_results appended | `src/engine.rs:837` |
| Env override `AISH_SERIAL_CHAIN_YIELD_DEPTH` (clamp 1–1000) | `src/engine.rs:40` |
| Disposition → `Resume` (auto-recovers, not a failure) | `src/loopguard.rs:483` |

**Mechanics.** `SerialChainGuard.record(calls_this_round)` increments `depth`
when a round issues **exactly one** tool call and **resets** `depth` to 0 on any
batched (≥2) or no-call round. It returns `Some(depth)` the first time
`depth > threshold`. With the default `threshold = 8`, the **9th** consecutive
single-call round trips it — hence `count=9`. `iterations=0` is expected: the
iteration field is not carried for this reason (see `ExitReason::banner`), only
`count` (the chain depth) is meaningful.

**It is not a failure.** `classify_disposition` maps `SerialChainYield` →
`Disposition::Resume`. The turn yields with a resumable banner, the durable
coordinator loop checkpoints, and the next round resumes and re-plans. In the
goal loop (`src/goal.rs`), a worker that keeps yielding is folded back in via
`recovery_guidance`, which quotes the error and injects an explicit
"batch independent tool calls" instruction — bounded by `MAX_GOAL_RECOVERIES`.

## Why we see it continuously

1. **The guard is shape-blind — it counts ANY lone call**, not just batchable
   reads. Whereas `BatchGuard` (`is_batchable_read`, threshold 3) only nudges on
   drip-fed *reads*, the serial-chain guard counts `grep → read → edit → run →
   read-output → …` all the same. A modest amount of exploratory or dependent
   work reaches depth 9.

2. **Investigation/troubleshooting work is inherently serial.** The canonical
   discovery pipeline — grep to find a line, read *that exact* line, grep again
   for the next symbol, read it, edit, run the test, read the output — is a chain
   of **genuinely dependent** single calls that *cannot* be batched. It trips the
   guard even though the model is behaving correctly. This is the dominant source
   of "continuous" yields and is a **false positive** against the guard's stated
   intent ("re-plan toward batching independent calls" — but these calls are not
   independent).

3. **The soft nudge doesn't cover the trip.** `BatchGuard` fires a one-shot
   prompt nudge at 3 lone *reads*, but (a) it only covers read tools and (b) it
   never yields. If the model ignores it, or the streak is mutations/runs, `depth`
   marches to 9 and the hard yield fires with no earlier "you're about to yield"
   warning specific to the serial-chain shape.

4. **Threshold 8 is low for real work.** 9 sequential dependent steps is common
   in a single troubleshooting turn. The ceiling was set to the card's ">8
   sequential calls" policy, not tuned against observed genuine-vs-wasteful ratios.

5. **Cost of each trip.** Every yield spends a re-plan round-trip and, in the
   goal loop, advances the auto-recovery counter toward `MAX_GOAL_RECOVERIES`.
   Enough consecutive yields can escalate a perfectly-recoverable run to
   flag-for-operator.

## How to reduce it — ranked

### Tier 1 — immediate, zero code
- **Raise the ceiling for serial workloads.** Set
  `AISH_SERIAL_CHAIN_YIELD_DEPTH=12` (or up to ~16) for troubleshooting/
  investigation runs whose dependent calls legitimately can't be batched. Already
  supported and clamped to `[1,1000]`; the only trade-off is a deeper chain
  before the checkpoint.

### Tier 2 — behavioural (prompt / batching discipline)
- **Front-load discovery reads.** The 5-phase coordinator pipeline already tells
  the agent to batch every known read in one Phase-1 turn. Recurring yields mean
  compliance gaps on exploratory tasks — reinforce "fire all independent reads
  together" and treat grep-then-read-*same-file* as the only serial exception.
- **Add an earlier soft serial-chain advisory.** Emit a one-shot warning at, say,
  `depth == threshold - 3` ("you've chained N single calls — batch the rest or
  you'll yield"), giving the model a chance to converge before the hard yield,
  mirroring how `CallBudgetGuard` warns (soft) before it yields (hard).

### Tier 3 — code (cut false positives at the source)
- **Count only *batchable-read* lone calls toward the yield.** Align the
  serial-chain guard with `BatchGuard::is_batchable_read`: a lone `read_file`/
  `grep_files`/`list_dir` streak is genuine drip-feeding worth yielding on; a
  lone `edit → run → read-output` TDD cycle or a `checkout → commit → push → PR`
  sequence is a legitimate dependent pipeline that should **not** count (or should
  use a much higher ceiling). This directly kills the dominant false-positive
  class in #2 above.
- **Dependency-aware discounting.** Don't extend the chain across tool-*type*
  transitions that imply a real pipeline (read→edit→run). Reset or discount when
  the single call is plausibly dependent on the previous round's result.
- **Bump the default** from 8 to ~10–12 if telemetry (below) confirms most trips
  are legitimate serial work rather than drip-fed reads.

### Tier 4 — observability (decide model-fix vs guard-fix with data)
- **Log the tool-name sequence at yield time** alongside `count`, and count
  serial-chain yields per run. If the trip sequences are mostly
  `read/grep/list` → the fix is behavioural (Tier 2). If they're mostly mixed
  `edit/run/read` dependent chains → the guard is over-firing and the fix is
  Tier 3. This is the cheapest way to stop guessing which lever to pull.

## Bottom line

`serial-chain-yield count=9` is the guard firing on the **9th** consecutive
single-call round (`threshold 8`). It auto-resumes (it is not a failure), but we
see it continuously because the guard counts **any** lone call and most
troubleshooting turns are legitimately-serial dependent chains that can't be
batched — a false positive against its own "batch independent calls" intent.

**Fastest mitigation:** raise `AISH_SERIAL_CHAIN_YIELD_DEPTH` to ~12.
**Most correct fix:** scope the yield to drip-fed *batchable reads* (Tier 3) so
genuine dependent pipelines stop tripping it, and add an earlier soft advisory
(Tier 2). Instrument first (Tier 4) to confirm the genuine-vs-wasteful split
before retuning the default.
