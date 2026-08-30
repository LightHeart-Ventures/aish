# Blacksmith ↔ aish Plugin Integration — Design & Gap Analysis

Status: **Draft for review** · Owner: aish core · Scope: design only (no implementation)

This document captures the review of [blacksmith.sh](https://blacksmith.sh),
the concrete opportunities to integrate it with aish through a plugin, and the
gaps in aish's **current** plugin capability surface that block a full-fidelity
Blacksmith plugin. It is grounded in the plugin code that ships today
(`src/plugins.rs`, `src/plugin_dispatcher.rs`, `src/plugin_state.rs`,
`src/plugin_memory.rs`, `src/plugin_auth.rs`) and the two extensibility tracks
already designed but not fully built: the **plugin webhook** track
([`plugin-webhook-events.md`](../plugin-webhook-events.md)) and the
**lifecycle hooks** track ([`aish-hooks-design.md`](../aish-hooks-design.md)).

---

## 1. What Blacksmith is

Blacksmith is a drop-in **CI acceleration** platform for GitHub Actions. The
value proposition is speed and cost, not a new API surface:

| Capability | What it does | Integration relevance |
|---|---|---|
| Faster GHA runners | Gaming-CPU bare-metal runners; swap `runs-on: blacksmith` | The unit of work aish would trigger/observe |
| Docker layer caching | Sticky, colocated build cache | Explains *why* a run is fast/slow |
| Blacksmith cache | Drop-in `actions/cache` replacement | — |
| Dashboard + analytics | Per-run timing, spend, flaky-test insight | The data a plugin would surface |
| GitHub-native | Installs as a GitHub App; runs are GitHub Actions runs | **Key**: the event/data plane is GitHub, not Blacksmith |

**Critical finding (unchanged from prior review):** Blacksmith exposes **no
public REST API / OpenAPI surface** for programmatic run control. Its
control-and-data plane *is* GitHub Actions. Therefore the highest-fidelity aish
integration is **not** "call the Blacksmith API" — it is:

1. **Inbound**: receive GitHub `workflow_run` / `check_run` webhooks (runs that
   happen to execute on Blacksmith runners) and react.
2. **Outbound / typed**: wrap the GitHub CLI + Blacksmith dashboard into typed
   aish tools (`testbox run`, `testbox status`) so the agent can *trigger* and
   *poll* CI as first-class actions.
3. **Gate**: block a `git push` / deploy tool call until the relevant
   Blacksmith-accelerated CI run is green.

Each of those maps onto a different aish extension surface — and only one of
them is buildable today.

---

## 2. Integration opportunities (the target plugin)

A `blacksmith` aish plugin would ideally contribute:

| # | Feature | aish surface required | Buildable today? |
|---|---|---|---|
| A | `aish login blacksmith` → store token in `[profile:blacksmith]` | `provides.login` + `plugin_auth` | **Yes** — surface exists |
| B | React to CI completion (notify, annotate, unblock) | inbound webhook broker | **Partial** — outbound only today |
| C | Typed `testbox run` / `testbox status` agent tools | plugin-contributed **tools** | **No** |
| D | `mcp__blacksmith__*` tools via a bundled MCP server | plugin-contributed **MCP server** | **No** |
| E | Block `git push`/deploy until CI green | **blocking** lifecycle hook | **No** (design-only) |
| F | Inject "last CI run: <id> <status>" into agent context | **mutating** lifecycle hook | **No** (design-only) |
| G | Cache/index run history for "why was build N slow?" | queryable plugin state | **No** (K/V only) |

Features A–B are the near-term wins. C–G are the capability gaps below.

---

## 3. Current plugin capability surface (what ships)

Verified against `src/plugins.rs` manifest structs and sibling modules:

| Surface | Module | State |
|---|---|---|
| Manifest `id/name/version/description/enabled` | `plugins.rs` (`PluginManifest`) | ✅ |
| `webhook_url` / `webhook_command` (outbound) | `plugin_dispatcher.rs` | ✅ (fire-and-forget, non-blocking) |
| `config_schema` + `load_config` (Phase 1.4) | `plugins.rs` | ✅ |
| `provides.lifecycle_hooks` (`on_init`/`on_shell_ready`/`on_shutdown`/…) | `plugins.rs` | ✅ (loader lifecycle only) |
| `provides.login` → `login.sh` → `[profile:*]` | `plugin_auth.rs` (~24 KB) | ✅ |
| Plugin K/V state store | `plugin_state.rs` (`plugins.db`) | ✅ (K/V, no query) |
| Plugin memory store | `plugin_memory.rs` (~30 KB) | ✅ |
| Outbound event types: `workspace_open`, `skill_loaded`, `background_job_start/complete`, `tool_invoked` | `plugin_dispatcher.rs` / webhook-events doc | ⚠️ defined; only `workspace_open` wired |

Two things are true at once: the **auth** and **outbound-webhook** surfaces are
real and usable now; everything that would let a plugin *act back on the shell*
(tools, MCP, blocking/mutating hooks) is either unbuilt or design-only.

---

## 4. Gap analysis

Seven gaps separate today's surface from the target `blacksmith` plugin. Each is
tagged with the track it belongs to, because two distinct tracks are involved
and conflating them is the main planning hazard:

- **Webhook track** — `plugin_dispatcher.rs` + broker. *External → plugin* event
  plumbing. Outbound half ships; inbound broker is SPR-059.
- **Hooks track** — `aish-hooks-design.md`, `~/.aish/hooks.json`, `:hooks`. The
  *blocking* and *mutating* gates live here (design-only), **not** in SPR-059.

| # | Gap | Impact on Blacksmith plugin | Track / where it lives | Status |
|---|---|---|---|---|
| 1 | No inbound / mutation hooks — can't block a push or inject CI context | Blocks features E & F (the highest-value ones) | Hooks-design **Phase 2** (blocking) / **Phase 3** (mutating) | Design-only |
| 2 | Outbound event sites incomplete (`tool_invoked`, `background_job_*` defined but not wired) | Weakens B: fewer moments to react to | Webhook track, "land incrementally Phase 1.6+" | Partial |
| 3 | No plugin-contributed **tools/commands** | Blocks C: no typed `testbox run/status` | Unscoped on any roadmap | Absent |
| 4 | No plugin-contributed **MCP servers** | Blocks D: the *ideal* Blacksmith shape | Unimplemented, unscoped | Absent |
| 5 | Blacksmith has no public REST API | Caps ambition — must go through GitHub, not Blacksmith | External to aish | N/A (external) |
| 6 | Login handler is single-shot JSON→profile | Fine for A; no refresh/rotation | `plugin_auth.rs` exists | **Non-gap** (already ships) |
| 7 | Plugin state store is K/V only (no query/index) | Blocks G: "why was run N slow?" analytics | `plugin_state.rs` | Absent |

### 4.1 Relationship to SPR-059

SPR-059 delivers the **inbound webhook broker** (plugin-Phase 4) + **handler
dispatch** (Phase 5) + a reference **GitHub plugin** (Phase 6). Mapped onto the
gaps:

- It makes gap #1's **plumbing half** concrete (inbound external→plugin events)
  — but **not** the blocking/mutating gate, which is the hooks track.
- It does **not** touch gaps #2, #3, #4, #7.
- Its follow-on Phases 7–12 are *config, enable/disable, testing, docs, error
  handling* — hardening, not new capability surfaces.

Caveat: as of this writing SPR-059's tasks are not decomposed on the board —
only the sprint-goal card (`card_73605f4530c3`) exists; the ~18 tasks live in
the goal text, so scope can still shift before activation (target 2026-07-16).

Net: SPR-059 unblocks **feature B (inbound reaction)** and gives a reference
plugin to copy. It leaves **C, D, E, F, G** open.

---

## 5. Proposed roadmap

Ordered by value-per-effort for the Blacksmith use case:

| Stage | Work | Unlocks | Depends on |
|---|---|---|---|
| 0 | Ship `blacksmith` plugin: `login` + `webhook_command` reacting to GitHub `workflow_run` | A, minimal B | Today's surface + SPR-059 broker for richer B |
| 1 | Wire remaining outbound event sites (`tool_invoked`, `background_job_*`) | Fuller B | Gap #2 |
| 2 | **Blocking hook** (`PreToolUse` on `git push`/`gh`/deploy) — hooks-design Phase 2 | E (green-CI-before-push) | Gap #1a |
| 3 | **Mutating hook** (`UserPromptSubmit`/context inject last-run status) — hooks-design Phase 3 | F (CI context injection) | Gap #1b |
| 4 | Plugin-contributed **tools** (typed `testbox run/status`) | C | Gap #3 |
| 5 | Plugin-contributed **MCP server** (`mcp__blacksmith__*`) | D (ideal shape) | Gap #4 |
| 6 | Queryable plugin state (indexed run history) | G (analytics) | Gap #7 |

Stages 2–3 are the **strategic** ones: "block the push until the
Blacksmith-accelerated run is green" is the feature that makes the integration
worth more than a notification script. They require the hooks track — a
separate sprint from SPR-059.

---

## 6. Recommendation

1. **Now:** build the stage-0 `blacksmith` plugin against today's surface
   (`login` + `webhook_command`). It's real value with zero core changes and
   validates the GitHub-as-data-plane finding end to end.
2. **Next sprint (post-SPR-059):** land hooks-design **Phase 2 (blocking)** —
   it is the single highest-leverage gap (feature E) and benefits every plugin,
   not just Blacksmith.
3. **Then:** plugin-contributed tools + a bundled MCP server (stages 4–5) for
   the typed, agent-native `testbox run/status` experience.
4. **Do not** wait on a Blacksmith REST API (gap #5) — it doesn't exist; design
   permanently around GitHub Actions as the control/data plane.

---

## 7. Concrete implementation: Testbox CLI pre-flight validation (interim)

**Status:** Implemented in PR #471 as `ci-testbox.yml` workflow + Testbox action scaffolding.

While the full plugin integration (stages 0–6 above) unfolds, operators can **today** use Blacksmith Testbox for pre-PR CI validation without waiting for plugin infrastructure. This sidesteps the local coordinator-worktree OOM risk documented in `aish_sre` SKILL.md (§3) by running the exact CI gate on a 4vcpu remote VM.

| Item | Value |
|---|---|
| Workflow | `.github/workflows/ci-testbox.yml` (mirror of `ci.yml` with `workflow_dispatch` + Testbox actions) |
| Gate tested | `cargo test --no-default-features --locked` (same as `ci.yml`) |
| VM setup | Rust toolchain + warm cargo cache (captured mid-workflow on the persistent testbox VM) |
| Operator flow | `blacksmith testbox warmup ci-testbox.yml` → `blacksmith testbox run --id <ID> "cargo test --no-default-features --locked"` |
| Iteration speed | ~30–60s with warm cache (vs. cold PR CI cycle) |
| OOM safety | Remote 4vcpu box; no local worktree build pressure |

**Prerequisite:** PR #471 must merge to `main` before `workflow_dispatch` becomes callable. Once merged, the warmup/run pattern is ready for feature branches.

**Relationship to roadmap:** This is a **pragmatic interim** for stages 4–5 (plugin-contributed tools). It demonstrates the `testbox run/status` UX using GitHub Actions + Testbox CLI directly, providing a scaffold for future agent-native tooling once gaps #3–4 close.

---

## 8. Cost & time analysis: persistent vs. on-demand Testbox (from the last 10 PRs)

**Question:** For a real burst-work session, what does it cost — and save — to (A) spin up a
Blacksmith Testbox at the *start* of work and hold it, vs. (B) spin one up on-demand
("I need to build → start a tester") each time?

### 8.1 The measured baseline (PRs #462–#471, 2026-07-03)

| Metric | Value |
|---|---|
| Session span | 18:30:33 → 19:28:32 = **~58 min** |
| PRs opened | 10 (#462–#471) |
| Code PRs (need build/test) | **7** (#463–#469) |
| Version bumps (trivial) | 2 (#462, #470) |
| CI/doc only | 1 (#471) |
| Cadence | a new PR every **~5–6 min** |
| CI gate | `cargo test --no-default-features --locked` on Blacksmith **4vcpu** ($0.008/min) |
| CI wall-time | **~37s warm**, **~150–196s cold** (a `Cargo.lock` version bump busts the `hashFiles('**/Cargo.lock')` cache key → full recompile) |

Every PR **already** runs this gate on Blacksmith automatically. The open question is only about the
**pre-PR loop** — where coordinator worktrees OOM on `cargo test` (heavy `local` feature, `aish_sre` §3),
so today the choice is *push-blind-and-round-trip-through-CI* or *OOM locally*.

### 8.2 Cost model inputs

- Blacksmith 4vcpu = **$0.008/min**; free tier **3,000 min/mo**.
- Cold warmup (first compile, cache-restored target) ≈ **3–4 min** VM.
- Warm incremental `testbox run` (few changed files) ≈ **~20–30s** VM.
- Assume **~2 test iterations per code PR** → ~14 runs across the session.

### 8.3 Strategy A — warm up at start, hold the whole session

- 1 cold warmup (~4 min) **overlaps** with writing the first fix → ~0 felt wait.
- VM stays alive ~58 min (bounded by session wall-clock); 14 runs are seconds each on the warm target.
- **VM cost ≈ 58 min × $0.008 = ~$0.46/session** (worst case ~$0.70 if left to idle-timeout unstopped).
- **Felt dev wait ≈ ~0** — tests are instant all session; OOM risk **eliminated**.

### 8.4 Strategy B — on-demand ("start a tester when I need to build")

- Each build burst: fresh-ish VM warmup (~3 min: cache restore + incremental link) + run + stop → ~4–5 min alive.
- 7 bursts × ~4.5 min = **~32 VM-min ≈ ~$0.26/session** — cheaper on raw VM-minutes (no idle gaps billed).
- **BUT** the ~3 min warmup is **not overlapped** — you've already written the code and are *blocked* waiting.
  7 × ~3 min = **~15–21 min of felt, blocking wait/session**.
- Because warmup (~3 min) ≈ cold CI (~3 min), for this cadence **B barely beats just-push-to-CI on time.**

### 8.5 Head-to-head

| | **A — warm at start** | **B — on-demand** |
|---|---|---|
| VM cost / session | ~**$0.46** (idle-inclusive) | ~**$0.26** |
| Free-tier burn | ~58 min | ~32 min |
| **Felt dev wait** | **~0** (warmup hidden behind coding) | **~15–21 min** (warmup blocks each build) |
| OOM risk eliminated | ✅ | ✅ |
| Best for | **batched multi-PR sessions (like this one)** | sparse, occasional one-off builds |

### 8.6 Time saved vs. the no-Testbox baseline

- Baseline pain per PR: OOM-retry locally, or push-blind + CI round-trip (~40s–3min, sometimes multiple pushes).
- **Strategy A saves the most:** ~1.5–3 min/PR × 7 ≈ **~10–20 min/session**, and removes OOM entirely.
- **Strategy B saves little on *time*:** re-paying ~3 min warmup per build ≈ CI cold time, so net ~0–1 min/PR over just-push-to-CI. It wins on *cash* only when a persistent VM would otherwise sit idle for hours.

### 8.7 Recommendation

- **The decision is about developer time, not dollars.** Both strategies sit far under the 3,000-min/mo
  free tier: a heavy ~58-min session (~58 testbox-min + ~30–40 CI-min ≈ ~90–100 Blacksmith-min) → **~30 such
  sessions/mo are free**. Real cash cost of either strategy for this volume ≈ **$0**.
- **For burst sessions (one PR every ~5–6 min, like these 10): use Strategy A.** The idle VM cost (~$0.46,
  likely $0 under free tier) buys instant, OOM-free tests across all 7 code PRs and saves ~10–20 min.
- **Use Strategy B only for sparse/occasional builds** where holding a VM would waste idle hours — then the
  ~3-min warmup-per-build latency is an acceptable trade for not paying idle minutes.
- **Rule of thumb:** *warm up at start whenever you expect ≥3 build/test iterations within ~30 min; otherwise
  spin on-demand.* This 10-PR session clears that bar 5× over → **A.**

---

## Appendix: source references

| Claim | Evidence |
|---|---|
| Manifest surface (`webhook_url`, `provides.login`, `config_schema`) | `src/plugins.rs` `PluginManifest` / `Provides` |
| Outbound-only, non-blocking dispatch; only `workspace_open` wired | `docs/plugin-webhook-events.md`; `src/plugin_dispatcher.rs` |
| Blocking/mutating hooks are design-only (Phases 2/3) | `docs/aish-hooks-design.md` §1.2, §2 |
| Login handler exists | `src/plugin_auth.rs` (~24 KB) |
| K/V-only state store | `src/plugin_state.rs`; `docs/reference/plugins/state.md` |
| Testbox CLI + Blacksmith integration | `blacksmith-testbox` SKILL.md; `.github/workflows/ci-testbox.yml` (PR #471) |
