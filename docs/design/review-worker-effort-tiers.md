# Review / Analysis Worker Effort Tiers + Hard Finding Caps — Design

Status: **Draft for review** · Owner: aish core · Scope: high-level design (no implementation) · Tracks: **SPR-067 / TASK-354** (item 9 of the prompt-leak review)

This document designs **caller-selectable effort tiers** (`low → medium → high →
xhigh → max`) and a **hard finding-cap** mechanism for aish's **review /
analysis** workers — the background coordinators dispatched with a "review this
PR", "audit this module", "find the bugs" style task.

It is the separately-scoped follow-on to
[`prompt-learnings-from-leaks.md`](../prompt-learnings-from-leaks.md) **§3.4**
(and item **9** of that doc's §4 edit table), which extracted the pattern from
Anthropic's bundled `/code-review` skill:

> Two transferable ideas: (a) **explicit effort tiers** the caller selects,
> mapping cost to thoroughness — we could expose this on review/analysis workers
> instead of one fixed depth; (b) **hard output caps** ("≤N findings") that force
> prioritization. Our workers have no output-count ceiling and sometimes
> over-produce. A cap is a cheap precision lever.

§3.4 explicitly parks this as *"out of current sprint scope … a larger,
separately-scoped enhancement"* — so this is a **design note**, not an
implementation. Items 1–6 and 8 of that review (the one-to-three-line prompt
insertions) ship independently as their own SPR-067 tasks; **this deliverable
does not block them and they do not block it.**

---

## 1. The problem: one fixed depth, no ceiling

A review/analysis worker today is dispatched with a free-text task and inherits
whatever depth the coordinator model happens to choose that run. Two concrete
failure modes fall out of that:

1. **No cost/thoroughness dial.** A throwaway "does this diff look sane?" and a
   "audit this auth module for security bugs before we ship" get the *same*
   open-ended treatment. The caller cannot say "spend 30 seconds" vs "spend the
   whole budget" — so cheap asks over-spend and critical asks under-spend.
2. **No output ceiling.** A worker that finds 40 nits emits 40 nits. There is no
   forcing function to **prioritize**, so signal drowns in noise and the reader
   pays the triage cost the worker should have paid. The prompt-leak review names
   this directly: *"Our workers have no output-count ceiling and sometimes
   over-produce."*

The `/code-review` skill solves both with a single lever the **caller** picks: a
named tier that fixes *both* the pipeline breadth *and* a hard finding cap.

---

## 2. The effort-tier ladder

Adapted from `/code-review`'s five levels to aish's coordinator/fan-out
primitives. Each tier fixes three things: **fan-out breadth** (how many
independent review angles run in parallel via `run_in_background`), a
**verification pass** (whether findings are re-checked before reporting), and a
**hard finding cap**.

| Tier | Fan-out breadth | Verify pass | Hard cap | Reasoning-effort | When the caller picks it |
|---|---|---|---|---|---|
| `low` | 1 pass, no sub-workers | none | **≤4** | low | Quick sanity check, small diff, "does this look obviously wrong?" |
| `medium` | up to 4 angles, dedup | 1-vote, precision-tuned | **≤8** | medium | Routine PR review; bias toward *fewer, surer* findings |
| `high` *(default)* | up to 8 angles | 1-vote, recall-biased | **≤10** | high | Default review depth; bias toward *catching more* |
| `xhigh` | up to 10 angles + gap-sweep | verify + gap-sweep | **≤15** | high | Security-sensitive or pre-release audit |
| `max` | identical pipeline to `xhigh` | verify + gap-sweep | **≤15** | max | Same breadth as `xhigh`; only the per-call API reasoning-effort is raised |

Design invariants carried over from the source pipeline:

- **`max` == `xhigh` in structure.** The *only* difference is the API
  reasoning-effort knob. This keeps the ladder honest: `max` is not "even more
  sub-workers", it is "the same thorough pipeline, thinking harder per step". It
  prevents an unbounded top tier.
- **The cap grows sub-linearly with breadth.** `low`→`max` multiplies fan-out
  ~10× but the cap only rises 4→15 (<4×). Thoroughness buys *more angles
  searched*, not *more findings emitted* — the cap is the precision lever, held
  deliberately tight.
- **Default is `high`, not `max`.** Recall-biased but bounded. A caller must
  *opt in* to the expensive tiers, so the common path stays cheap.

### 2.1 Mapping to aish primitives

The tiers are not new machinery — they are a **preset** over things aish already
has:

| Tier knob | aish primitive it drives |
|---|---|
| Fan-out breadth | number of parallel `run_in_background` sub-workers (one per review angle) |
| Verify pass | one extra serial sub-worker that fact-checks the merged findings (the "guard against cascading errors / independent verifier" rule already in the coordinator charter) |
| Reasoning-effort | the per-call reasoning-effort already threaded through the coordinator (`fan-out` / `batch` tiers, `reasoning_note` telemetry) |
| Hard cap | a post-merge truncation + a prompt-level ceiling (see §3) |

So a review coordinator that adopts tiers is just choosing a **fixed fan-out fan
width + a verify toggle + a reasoning-effort + a cap constant** from the table,
instead of improvising each run.

---

## 3. The hard finding-cap mechanism

A cap has to be enforced at **two** points, because either alone leaks:

1. **Prompt-level ceiling (soft, at generation time).** The review worker's
   instructions state the cap explicitly and tell it to *self-prioritize*:
   > "Emit at most **N** findings, ordered highest-severity first. If you found
   > more than N, drop the lowest-severity ones — do **not** append an 'other
   > minor issues' overflow list. The cap forces a call: what are the N things
   > that most matter here?"
   This is the important half: the cap's *value* is that it forces the worker to
   **rank and choose**, not that it truncates after the fact. A worker that
   silently drops #11 without having *ranked* #1–#10 well is missing the point.

2. **Merge-level truncation (hard, at aggregation time).** When a fan-out tier
   merges findings from K parallel angle-workers, the *union* can exceed the cap
   even if each sub-worker stayed under it. The coordinator therefore:
   a. dedups findings across angles (same file+line+claim → one),
   b. sorts the survivors by severity (then by verify-confidence),
   c. **truncates to the tier's cap**, and
   d. reports the truncated count honestly: *"showing top 10 of 14 deduped
      findings (tier=high, cap=10)"* — never hides that a cut happened.

**Severity ordering** (for the sort in step 2b) reuses a fixed 4-rung scale so
the truncation is deterministic and explainable:
`critical > high > medium > low`. Ties break by verify-confidence, then by file
path (stable). A finding with no severity is treated as `low`.

**Anti-gaming clause.** A worker must not dodge the cap by bundling ("issues #7
through #15: various naming nits"). One finding = one discrete, independently
actionable claim. Bundled overflow counts against the cap as the number of
distinct claims it hides, and the merge step is allowed to reject a bundle.

---

## 4. Caller surface — how a tier is selected

The tier is chosen by the **caller** (the human, or the parent coordinator), not
the review worker. Three non-exclusive surfaces, cheapest first:

1. **Natural-language in the task text** (zero new syntax). The dispatch/skill
   prompt teaches the mapping: *"quick look" / "sanity check" → low; "review this
   PR" → high (default); "security audit" / "before we ship" → xhigh; "no
   expense spared" → max.* The coordinator classifies intent and picks the tier —
   the same way it already classifies "question vs new work".
2. **An explicit `effort:` hint** the caller can drop in the task
   (`effort: xhigh`) that overrides the natural-language guess. Deterministic and
   greppable; wins over (1) when present.
3. **A skill argument** if/when review is packaged as a first-class skill
   (`review-pr`, `code-review`): a `tier` / `effort` arg on the SKILL.md, so the
   value lives in the frontmatter's `args` schema and gets validated at call
   time, exactly like the existing `atum/review-pr` `depth` arg.

Precedence: **explicit `effort:` / skill-arg** > **natural-language inference** >
**default (`high`)**.

---

## 5. Why this is its own deliverable (scoping)

Per TASK-354's acceptance criteria and §3.4's "separately-scoped" note, this is
deliberately **carved off** from the sibling SPR-067 prompt-hardening tasks:

- **It does not block them.** Items 1–6 and 8 of the prompt-leak review are
  one-to-three-line insertions into the interactive/worker system prompts. They
  ship the moment they're written; none of them wait on tiers.
- **They do not block it.** This design stands on primitives that already exist
  (`run_in_background` fan-out, the verify-sub-worker charter rule,
  reasoning-effort tiers). No sibling task is a prerequisite.
- **It is `M`-effort, not `XS`.** Wiring the preset table, the two-point cap
  enforcement, and the caller surface is a structural change with its own tests
  (tier→pipeline mapping, cap-truncation determinism, dedup correctness) — which
  is exactly why §3.4 parked it out of the prompt-copy sprint.

### 5.1 Success criteria for the eventual implementation

When this design is built (a later card), "done" means:

1. A caller can select one of the five tiers via any of the §4 surfaces, and the
   selected tier deterministically fixes fan-out breadth, verify pass,
   reasoning-effort, and cap.
2. A review run **never emits more than the tier's cap**, and when it truncates
   it reports the honest `top N of M` line (§3.2d).
3. The cap enforcement is tested at *both* points: a single over-producing worker
   is capped, and a fan-out union exceeding the cap is deduped-then-truncated.
4. Default behavior (no tier specified) is `high` — unchanged recall bias, now
   with a `≤10` ceiling where before there was none.

---

## 6. Open questions (for review)

1. **Should `low` ever fan out?** The table says no (single pass). Alternative:
   allow `low` a 2-angle fan-out for cheap parallelism. Current call: keep `low`
   single-pass so its cost floor is genuinely low.
2. **Per-file vs per-run cap?** The design is a **per-run** cap. A very large
   multi-file review might warrant a per-file sub-cap that rolls up to the run
   cap. Deferred — start with per-run and revisit if large reviews starve
   individual files.
3. **Does the cap apply to analysis (non-review) workers too?** Yes in spirit —
   an "analyze this codebase" worker over-produces the same way — but the tier
   *names* are review-framed. A follow-up could generalize the ladder to any
   finding-emitting worker. Out of scope here.
