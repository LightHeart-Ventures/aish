# Prompt Learnings from `asgeirtj/system_prompts_leaks`

Review of <https://github.com/asgeirtj/system_prompts_leaks> for concrete
improvements to **aish's interactive-agent prompt** and **worker / background-coordinator
prompt**. The repo is a large corpus of leaked/extracted system prompts (Anthropic,
OpenAI, Google, xAI, etc.). The highest-signal artifacts for us are the ones that
describe agent architectures shaped like ours:

| Source file | Why it maps to us |
|---|---|
| `Anthropic/claude-cowork-dispatch.md` | Anthropic's **orchestrator/dispatch** prompt — a near-exact analog of our background-coordinator + `run_in_background`/`tell` model. |
| `Anthropic/Claude Code/bundled-skills/loop.md` | An **autonomous "steward" loop** prompt — analog of our headless coordinators and the `/loop`-style babysit pattern. |
| `Anthropic/Claude Code/bundled-skills/code-review/*` | A **tiered-effort** review pipeline (low→max) with hard finding caps — a model for our review/analysis workers. |
| `Anthropic/Claude Code/claude-code-*.md`, `deferred-tools.md`, `grep-tool.md` | The mainline agentic-CLI prompt + **deferred/bulk tool-loading** discipline. |
| `bundled-skills/*/SKILL.md` frontmatter | The `description` / `when_to_use` convention for skill routing. |

Everything below is grounded in text actually present in those files.

---

## 1. Interactive agent

### 1.1 "Match the ask" — the failure mode is *mismatch*, not length
Dispatch prompt, verbatim intent:
> *"Match the ask. Short question → short answer; they'll follow up if they want more.
> The failure mode isn't length, it's mismatch — answering a bigger question than asked,
> or padding with adjacent info. Gut check: if they could reasonably follow up to get
> this, don't preempt it."*

Our prompt already says "terse, shell-like," but it frames terseness as a style rule.
The leak reframes it as a **correctness** rule: padding with adjacent info is a *defect*,
not just verbosity. Adopt the "they'll follow up" gut-check verbatim — it's a sharper
stopping criterion than "be terse."

### 1.2 Emit the ack **and** the tool call in the same turn — never ack-then-wait
> *"don't send 'on it' then the answer two seconds later. If you need a tool, emit the
> ack and the tool call in the SAME response as parallel calls, not ack-then-wait."*

This is a stronger, more concrete statement of our existing "batch tool calls /
decide-then-act" rule. Our directive tells workers to batch *independent* calls; the
leak adds the **anti-pattern name** ("ack-then-wait") and ties it to user-facing latency.
Worth importing the phrasing — named anti-patterns are stickier than positive rules.

### 1.3 "Look before you assert" — ground claims in a fresh observation
> *"If you're about to say an app doesn't support an action, that claim should be grounded
> in what you just saw on screen, not general knowledge … a fresh screenshot is cheaper
> than a wrong assertion."*

This is our **NEVER FABRICATE / ALWAYS VERIFY** rule, but with a cost-framed justification
that makes it self-enforcing: *the verification is cheaper than the mistake.* Our prompt
asserts the rule; the leak *motivates* it. Add the cost framing ("a `gh run view` is
cheaper than a wrong claim about the run") so the model chooses to verify unprompted.

### 1.4 "Answer a question, don't dispatch it"
The dispatch prompt draws a bright line: greetings / small-talk / clarifying questions get
answered **directly** (`SendUserMessage`), only *new logical work* gets routed to a task
session. We already tell coordinators "answer a QUESTION inline rather than offloading it,"
but the dispatch prompt's **routing heuristics table** is cleaner than our prose:

> - New logical task (distinct goal) → `start_task`
> - Follow-up / clarification / correction for a running task → `send_message` to its session
> - Check a task's progress/outcome → `read_transcript`
> - Multiple distinct requests in one message → start multiple tasks

That maps 1:1 onto `run_in_background` / `tell` / `background_status`. Encoding it as a
compact decision table (not a paragraph) would reduce the "spawned a coordinator to answer
a question about coordinators" class of error.

---

## 2. Workers / background coordinators

### 2.1 The "steward, not initiator" frame (`loop.md`)
The autonomous-loop prompt is the single most transferable artifact. Its thesis:
> *"You're a steward, not an initiator … the value you provide comes from reliably
> advancing things they've already set in motion, not from finding new things to do."*

And the trust-erosion tell:
> *"If you find yourself reaching for justifications about why a push is probably fine,
> that's a signal to wait."*

Our coordinator prompt has loop-guards and "don't over-decompose," but nothing that frames
**scope discipline** this crisply. A coordinator that invents adjacent work is our exact
failure mode. Import the steward framing and the "reaching for justifications = wait" tell.

### 2.2 Reversibility gate for autonomous actions
> *"for reversible actions (local edits, running tests), make your best call and proceed;
> for irreversible ones (pushing, deleting, sending), keep waiting — the cost of acting
> wrongly on something irreversible is much higher than the cost of waiting one more cycle."*

We have "feature branches only / PRs or die / no direct pushes to main," but that's a git
rule. The **reversibility gate** is a general decision principle that would improve every
autonomous coordinator: cheap-and-reversible → act; expensive-and-irreversible → escalate
or wait. This dovetails with our `reasoning_note` escalate-vs-guess telemetry.

### 2.3 "Do the work, don't describe it" (again, and stronger)
> *"actually do the work, don't describe what could be done. Run the tests, don't say
> 'you could run the tests.'"*

Same spirit as our "a bare narration runs nothing," but note the leak pairs it with a
**stop condition**: after *three consecutive* "nothing to do" cycles, do one quick check
and stop — *don't* narrate what you checked. Our loop-guards prevent runaway spinning but
we don't currently tell coordinators to **go quiet** rather than emit "nothing to do"
filler. Add: repeated no-op results should shrink the footprint, not produce status noise.

### 2.4 Communication register for a remote/async reader
The dispatch prompt assumes the human is on a phone checking in — so it bans headers, bold,
and bullet lists in relayed results and says "break at thought boundaries" (send another
message rather than packing paragraphs). For **our** `message_console` / final-report
channel the opposite is often true (operator wants a scannable table), but the underlying
rule generalizes: **match output shape to the reading surface.** Our workers sometimes emit
long reports to a console that wants one line. Worth a one-liner in the worker prompt:
"tailor report density to the surface — console banner = 1–2 lines; final report = full."

---

## 3. Skills & tool routing

### 3.1 Tiered tool selection ("pick the right tool for the app")
The dispatch prompt ranks tool tiers explicitly: **dedicated MCP > generic browser MCP >
computer-use fallback**, and adds: *"if a dedicated MCP tool errors, debug or report it
rather than silently retrying via a slower tier."* This is exactly our skills/MCP-first
posture, but the **"don't silently fall through to a worse tier on error"** clause is one
we don't state. Add it: a failing skill/MCP call should surface, not degrade to hand-rolling.

### 3.2 Bulk / deferred tool loading
`deferred-tools.md` + the dispatch prompt describe **ToolSearch**: tool schemas aren't all
loaded; you load them in **bulk** with one query (`{ query: "computer-use", max_results: 30 }`)
rather than one-by-one, because per-tool selection is "one round-trip per tool." This is the
same economics as our "batch independent calls" rule, applied to *capability discovery*.
If aish ever grows a large/deferred tool surface, adopt bulk-load-by-substring, not
select-one-at-a-time.

### 3.3 SKILL.md `when_to_use` is a first-class routing field
Every bundled skill's frontmatter has a `description` **and** a `when_to_use` that is
written as an *explicit trigger list* ("Use this skill whenever the user … Do NOT use for …").
This directly validates the direction of the already-created **TASK-331** (extend SKILL.md
schema with `categories` / `applies-to` / `unwanted-for`). The leak's convention pairs a
positive trigger with an explicit **negative** trigger ("Do NOT use for PDFs …") — our
schema work should include the *unwanted-for* / negative-match field, not just positive
categories, because that's what prevents mis-routing.

### 3.4 Tiered-effort review pipeline (`code-review`)
The `/code-review` skill ships five effort levels — `low`/`medium`/`high`/`xhigh`/`max` —
each with an escalating pipeline and a **hard finding cap**:

| Level | Pipeline | Cap |
|---|---|---|
| low | 1 diff pass, no subagents, no verify | ≤4 findings |
| medium | 8 finder angles × 6 candidates, 1-vote verify (precision-tuned) | ≤8 |
| high (default) | 8 angles × 6, 1-vote verify (recall-biased) | ≤10 |
| xhigh | 10 angles × 8, verify, gap-sweep | ≤15 |
| max | identical to xhigh; only API reasoning-effort differs | ≤15 |

Two transferable ideas: (a) **explicit effort tiers** the caller selects, mapping cost to
thoroughness — we could expose this on review/analysis workers instead of one fixed depth;
(b) **hard output caps** ("≤N findings") that force prioritization. Our workers have no
output-count ceiling and sometimes over-produce. A cap is a cheap precision lever.

---

## 4. Concrete prompt edits (prioritized)

| # | Target | Change | Effort |
|---|---|---|---|
| 1 | interactive | Reframe terseness as **"match the ask; the failure is mismatch, not length"** + the "they'll follow up" gut-check | XS |
| 2 | interactive + worker | Add cost-framed verify rule: **"a fresh read is cheaper than a wrong assertion"** next to NEVER FABRICATE | XS |
| 3 | interactive | Import the **routing decision table** (answer inline vs `run_in_background` vs `tell` vs `background_status`) as a compact table | S |
| 4 | worker | Add the **"steward, not initiator"** frame + "reaching for justifications = wait" tell | S |
| 5 | worker | Add the **reversibility gate**: reversible→act, irreversible→wait/escalate | S |
| 6 | worker | Add **quiet-down rule**: repeated no-op cycles shrink footprint, don't emit "nothing to do" filler | XS |
| 7 | skills | Ensure SKILL.md schema work (TASK-331) includes an **`unwanted-for` / negative-trigger** field, per the `when_to_use` convention | — (folds into TASK-331) |
| 8 | skills/tools | Add **"don't silently fall through to a worse tier on skill/MCP error — surface it"** | XS |
| 9 | review workers | Consider **effort tiers + hard finding caps** for review/analysis workers (out of current sprint scope) | M |

Items 1–2, 6, 8 are one-to-three-line prompt insertions with high leverage. Items 3–5 are
the structural wins. Item 7 sharpens the already-scoped TASK-331. Item 9 is a larger,
separately-scoped enhancement.

### Relationship to existing SPR-063 work
The already-created TASK-331…335 (semantic skill-matching schema, ranking, ranged-I/O,
prompt hardening, docs) remain valid — this review **reinforces** TASK-331's schema
direction (add the negative/`unwanted-for` field) and adds a distinct, prompt-copy layer
(items 1–6, 8) that is orthogonal to the token-efficiency tasks. These are wording/behavioral
changes to the system prompts themselves, not code — cheap to land and independently testable.
