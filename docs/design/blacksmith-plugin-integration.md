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

## Appendix: source references

| Claim | Evidence |
|---|---|
| Manifest surface (`webhook_url`, `provides.login`, `config_schema`) | `src/plugins.rs` `PluginManifest` / `Provides` |
| Outbound-only, non-blocking dispatch; only `workspace_open` wired | `docs/plugin-webhook-events.md`; `src/plugin_dispatcher.rs` |
| Blocking/mutating hooks are design-only (Phases 2/3) | `docs/aish-hooks-design.md` §1.2, §2 |
| Login handler exists | `src/plugin_auth.rs` (~24 KB) |
| K/V-only state store | `src/plugin_state.rs`; `docs/plugin-state-schema.md` |
