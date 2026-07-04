# Fly.io ↔ aish Plugin Integration — Design & Gap Analysis

Status: **Draft for review** · Owner: aish core · Scope: design only (no implementation)

This document captures the review of [fly.io](https://fly.io), the concrete
opportunities to integrate it with aish through a plugin, and the gaps in aish's
**current** plugin capability surface that block a full-fidelity Fly plugin. It
is grounded in the plugin code that ships today (`src/plugins.rs`,
`src/plugin_dispatcher.rs`, `src/plugin_state.rs`, `src/plugin_memory.rs`,
`src/plugin_auth.rs`) and the two extensibility tracks already designed but not
fully built: the **plugin webhook** track
([`plugin-webhook-events.md`](../plugin-webhook-events.md)) and the **lifecycle
hooks** track ([`aish-hooks-design.md`](../aish-hooks-design.md)).

It is written to sit alongside
[`blacksmith-plugin-integration.md`](./blacksmith-plugin-integration.md) and
deliberately mirrors its structure — but the **conclusion inverts**. Blacksmith
has *no public API* (its data plane is GitHub Actions), so the best integration
routes around Blacksmith through GitHub. Fly.io is the opposite: it ships a
first-class **public REST API** (the Machines API) plus a mature CLI, so the
highest-fidelity integration *is* "call the platform" — which makes a bundled
**MCP server** the ideal shape and puts the pressure squarely on the plugin
capability gaps rather than on the vendor.

---

## 1. What Fly.io is

Fly.io is a **public-cloud application runtime**: you ship a container (or a
Dockerfile/buildpack) and it runs as **Fly Machines** — fast-booting Firecracker
microVMs — across global edge regions, front-ended by an Anycast proxy. Unlike
Blacksmith (a CI accelerator), Fly is a *deploy-and-run* platform, so the unit of
work a plugin observes is a **deploy / a machine / an app**, not a CI run.

| Capability | What it does | Integration relevance |
|---|---|---|
| Fly Machines | Firecracker microVMs, boot in ~hundreds of ms, start/stop on demand | The unit of work aish would trigger/observe/scale |
| `flyctl` (`fly`) CLI | `fly deploy`, `fly status`, `fly logs`, `fly machine …`, `fly scale`, `fly secrets` | The typed surface a plugin would wrap |
| **Machines API** (`api.machines.dev/v1`) | Public REST: create/start/stop/destroy machines, apps, volumes | **Key**: a real programmatic control plane — the thing Blacksmith *lacks* |
| GraphQL API (`api.fly.io/graphql`) | Broader account/org/app/release management | Deeper queries (orgs, releases, billing) |
| Anycast proxy + regions | Global routing, autoscale-to-zero, `fly regions` | Deploy targets a plugin could expose |
| Prometheus + Grafana | Per-org metrics endpoint (`api.fly.io/prometheus/<org>`) | The analytics a plugin would surface |
| Token auth (Macaroons) | `fly auth token`, org/deploy tokens, `FLY_API_TOKEN` | Fine-grained creds → `[profile:fly]` |

**Critical finding (the inversion vs. Blacksmith):** Fly.io **does** expose a
public, documented REST API (the Machines API) and a GraphQL API for
programmatic control. Therefore the highest-fidelity aish integration is exactly
the shape Blacksmith could not support:

1. **Typed tools**: wrap `flyctl` / the Machines API into first-class agent
   tools (`fly deploy`, `fly status`, `fly machine start/stop`, `fly scale`) so
   the agent can *act on* the platform, not just watch it.
2. **MCP server**: a bundled `mcp__fly__*` server over the Machines API — the
   *ideal* shape, and the one Fly's real API actually makes worthwhile.
3. **Gate**: block a `fly deploy` (or a `git push` that triggers one) until a CI
   gate is green, and inject live app/machine status into agent context.

Every one of those maps onto a different aish extension surface — and, as with
Blacksmith, **only the login + outbound-webhook slice is buildable today.** The
difference is *where the ceiling comes from*: with Blacksmith the ceiling was the
vendor (no API); with Fly the vendor has no ceiling, so the ceiling is entirely
aish's own plugin capability surface.

### 1.1 One caveat Fly shares with Blacksmith: no rich inbound webhooks

Fly.io has **no first-class "notify me on deploy/health-change" webhook plane**
comparable to GitHub's `workflow_run`. Deploy and machine-state changes surface
through the **API/CLI (poll)** and the log stream (`fly logs`, NATS-backed), not
through an inbound HTTP callback aish can subscribe to. So the "react to a Fly
event" story (feature B) is **poll-based** — an aish `webhook_command` that runs
`fly status --json` on a lifecycle tick — rather than push-based. This is the one
spot where Fly is *weaker* than the GitHub-fronted Blacksmith path.

---

## 2. Integration opportunities (the target plugin)

A `fly` aish plugin would ideally contribute:

| # | Feature | aish surface required | Buildable today? |
|---|---|---|---|
| A | `aish login fly` → store `fly auth token` in `[profile:fly]` | `provides.login` + `plugin_auth` | **Yes** — surface exists |
| B | React to deploy/health change (notify, annotate, unblock) — **poll-based** | outbound webhook / lifecycle tick | **Partial** — outbound only today |
| C | Typed `fly deploy` / `fly status` / `fly machine` agent tools | plugin-contributed **tools** | **No** |
| D | `mcp__fly__*` tools via a bundled MCP server over the Machines API | plugin-contributed **MCP server** | **No** |
| E | Block `fly deploy` / deploy tool call until CI green (or health OK) | **blocking** lifecycle hook | **No** (design-only) |
| F | Inject "app <x>: N machines, region, last release <status>" into context | **mutating** lifecycle hook | **No** (design-only) |
| G | Cache/index deploy + machine history for "why did release N fail / cost?" | queryable plugin state | **No** (K/V only) |

Features A–B are the near-term wins. C–G are the capability gaps below — and
because Fly has a real API, **D (the bundled MCP server) is the single most
valuable target**, where for Blacksmith it was permanently out of reach.

---

## 3. Current plugin capability surface (what ships)

Verified against `src/plugins.rs` manifest structs (`PluginManifest`,
`Provides`) and sibling modules:

| Surface | Module | State |
|---|---|---|
| Manifest `id/name/version/description/enabled` | `plugins.rs` (`PluginManifest`) | ✅ |
| `webhook_url` / `webhook_command` (outbound) | `plugin_dispatcher.rs` | ✅ (fire-and-forget, non-blocking) |
| `config_schema` + `load_config` (Phase 1.4) | `plugins.rs` | ✅ |
| `provides.lifecycle_hooks` (`on_init`/`on_shell_ready`/`on_shutdown`/…) | `plugins.rs` (`Provides`) | ✅ (loader lifecycle only) |
| `provides.login` → `login.sh` → `[profile:*]` | `plugin_auth.rs` (~24 KB) | ✅ |
| Plugin K/V state store | `plugin_state.rs` (`plugins.db`) | ✅ (K/V, no query) |
| Plugin memory store | `plugin_memory.rs` (~30 KB) | ✅ |
| Plugin-contributed **skills** (`<plugin>/skills/<name>/SKILL.md`) | `plugins.rs` (`discover`) | ✅ |
| Plugin-contributed **JSON schemas** (`<plugin>/schemas/*.json`) | `plugins.rs` (`PluginSchema`) | ✅ (output validation) |
| Outbound event types: `workspace_open`, `skill_loaded`, `background_job_start/complete`, `tool_invoked` | `plugin_dispatcher.rs` / webhook-events doc | ⚠️ defined; only `workspace_open` wired |

Two things are true at once: the **auth**, **skill/schema**, and
**outbound-webhook** surfaces are real and usable now; everything that would let
a plugin *act back on the shell as the agent* (tools, MCP, blocking/mutating
hooks) is either unbuilt or design-only.

---

## 4. Gap analysis

Seven gaps separate today's surface from the target `fly` plugin. Each is tagged
with the track it belongs to, because two distinct tracks are involved and
conflating them is the main planning hazard:

- **Webhook track** — `plugin_dispatcher.rs` + broker. *External → plugin* event
  plumbing. Outbound half ships; inbound broker is SPR-059.
- **Hooks track** — `aish-hooks-design.md`, `~/.aish/hooks.json`, `:hooks`. The
  *blocking* and *mutating* gates live here (design-only), **not** in SPR-059.

| # | Gap | Impact on Fly plugin | Track / where it lives | Status |
|---|---|---|---|---|
| 1 | No inbound / mutation hooks — can't block a deploy or inject app status | Blocks features E & F (the highest-value gates) | Hooks-design **Phase 2** (blocking) / **Phase 3** (mutating) | Design-only |
| 2 | Outbound event sites incomplete (`tool_invoked`, `background_job_*` defined but not wired) | Weakens B: fewer moments to poll/react | Webhook track, "land incrementally Phase 1.6+" | Partial |
| 3 | No plugin-contributed **tools/commands** | Blocks C: no typed `fly deploy/status/machine` | Unscoped on any roadmap | Absent |
| 4 | No plugin-contributed **MCP servers** | Blocks D: **the ideal Fly shape** (a real API to wrap) | Unimplemented, unscoped | Absent |
| 5 | Fly's inbound event surface is poll-only (no rich webhooks) | Caps B: react = poll `fly status`, not push | External to aish | N/A (external) |
| 6 | Login handler is single-shot JSON→profile | Fine for A; no token refresh/rotation (Fly Macaroons are long-lived, so OK) | `plugin_auth.rs` exists | **Non-gap** (already ships) |
| 7 | Plugin state store is K/V only (no query/index) | Blocks G: "why did release N fail / cost trend?" analytics | `plugin_state.rs` | Absent |

**Note the inversion at gap #5.** For Blacksmith, gap #5 was "no public REST API
at all" — a hard ceiling on ambition. For Fly, the API is rich; the only external
limitation is the *inbound event* direction (no push webhooks). That shrinks gap
#5 from "caps the whole integration" to "the react-path polls instead of
subscribes" — a much softer constraint — and correspondingly raises the value of
closing gap #4 (the MCP server), since there is a real API worth wrapping.

### 4.1 Relationship to SPR-059

SPR-059 delivers the **inbound webhook broker** (plugin-Phase 4) + **handler
dispatch** (Phase 5) + a reference **GitHub plugin** (Phase 6). Mapped onto the
gaps:

- It makes gap #1's **plumbing half** concrete (inbound external→plugin events)
  — but **not** the blocking/mutating gate, which is the hooks track.
- For Fly specifically its value is *muted*: because Fly emits no rich inbound
  webhooks (gap #5), the broker has little Fly traffic to route. The Fly react
  path leans on **polling** (`fly status --json` on a lifecycle tick), not the
  inbound broker.
- It does **not** touch gaps #2, #3, #4, #7 — including gap #4, the MCP server,
  which is the Fly integration's centre of gravity.

Net: SPR-059 is high-leverage for the GitHub/Blacksmith shape and largely
orthogonal to Fly. Fly's value unlock is **gaps #3–#4 (tools + MCP)**, which no
current sprint scopes.

---

## 5. Proposed roadmap

Ordered by value-per-effort for the Fly use case:

| Stage | Work | Unlocks | Depends on |
|---|---|---|---|
| 0 | Ship `fly` plugin: `login` (`fly auth token` → `[profile:fly]`) + `webhook_command` running `fly status --json` on lifecycle ticks | A, minimal B | Today's surface |
| 1 | Wire remaining outbound event sites (`tool_invoked`, `background_job_*`) so more moments can poll Fly | Fuller B | Gap #2 |
| 2 | Plugin-contributed **MCP server** (`mcp__fly__*` over the Machines API) | **D (the ideal shape)** | Gap #4 |
| 3 | Plugin-contributed **tools** (typed `fly deploy/status/machine/scale`) | C | Gap #3 |
| 4 | **Blocking hook** (`PreToolUse` on `fly deploy`/`git push`) — hooks-design Phase 2 | E (green-CI / healthy-before-deploy) | Gap #1a |
| 5 | **Mutating hook** (`UserPromptSubmit`/context inject live app status) — hooks-design Phase 3 | F (app-status context injection) | Gap #1b |
| 6 | Queryable plugin state (indexed deploy/machine history) | G (analytics, cost/failure trends) | Gap #7 |

**The ordering differs from Blacksmith's on purpose.** For Blacksmith the
strategic stages were the *hooks* (block-the-push), because there was no API to
build tools/MCP against. For Fly the strategic stage is **stage 2 — the MCP
server** — because a real Machines API makes `mcp__fly__deploy` /
`mcp__fly__machine_start` genuinely first-class, and that is the highest-value
gap Fly can close that Blacksmith never could.

---

## 6. Recommendation

1. **Now:** build the stage-0 `fly` plugin against today's surface (`login` +
   `webhook_command`). Real value, zero core changes, and it validates the
   token→`[profile:fly]` + poll-status loop end to end (see §7).
2. **Next (Fly-specific unlock):** land **gap #4 — plugin-contributed MCP
   servers** — and ship a bundled `mcp__fly__*` server over the Machines API.
   This is the single highest-leverage gap for Fly and, unlike the Blacksmith
   case, it is *achievable* because the API exists. It also benefits every future
   API-backed plugin, not just Fly.
3. **Then:** typed tools (stage 3) for the ergonomic `fly deploy/status` agent
   experience, followed by the hooks track (stages 4–5) for the
   block-deploy-until-green and inject-app-status gates.
4. **Do not** design the react-path around inbound Fly webhooks (gap #5) — they
   don't exist in a rich form; poll the Machines API / `fly status` on a
   lifecycle tick instead, exactly as stage 0 does.

---

## 7. Concrete implementation: the stage-0 `fly` plugin (buildable today)

Everything here uses **only the surfaces that ship** (§3): `provides.login` +
`login.sh` (→ `[profile:fly]`) and `webhook_command` (poll `fly status`). No core
changes, no design-only track.

### 7.1 On-disk layout

Plugins live under `~/.aish/plugins/<id>/`:

```
~/.aish/plugins/fly/
├── plugin.json          # manifest (below)
├── login.sh             # provides.login handler → JSON on stdout
└── skills/
    └── fly-ops/
        └── SKILL.md      # optional: a flyctl playbook the agent can read
```

### 7.2 `plugin.json`

```json
{
  "id": "fly",
  "name": "Fly.io",
  "version": "0.1.0",
  "description": "Fly.io login + deploy/machine status polling for aish",
  "enabled": true,
  "webhook_command": "fly status --json > /dev/null 2>&1 || true",
  "provides": {
    "login": "fly",
    "lifecycle_hooks": ["on_shell_ready"]
  }
}
```

- `provides.login: "fly"` routes `aish login fly` to `login.sh` (`plugin_auth`),
  whose JSON is persisted to `~/.aish/credentials` under `[profile:fly]`.
- `webhook_command` fires on each wired lifecycle event (Phase 1.6 wires
  `workspace_open` today); its captured stdout/exit lands in the plugin state
  store under `fly:last_webhook_output` — a cheap, non-blocking status poll.

### 7.3 `login.sh` (handler contract)

`plugin_auth` runs the handler and persists its **stdout JSON** to the profile.
Fly Macaroon tokens are long-lived, so a single-shot capture is sufficient (gap
#6 is a non-gap here):

```sh
#!/bin/sh
# aish login fly  →  captures a Fly API token into [profile:fly]
set -eu

# Prefer an existing flyctl session; else mint an org-scoped token.
TOKEN="$(fly auth token 2>/dev/null || true)"

if [ -z "$TOKEN" ]; then
  echo '{"error":"run `fly auth login` first, then re-run `aish login fly`"}' >&2
  exit 1
fi

# JSON on stdout → persisted under [profile:fly]
printf '{"FLY_API_TOKEN":"%s"}\n' "$TOKEN"
```

Downstream, tools and any future `mcp__fly__*` server read the token via the
credential ref `${profile:fly}` / `${FLY_API_TOKEN}` — never inline.

| Item | Value |
|---|---|
| Login command | `aish login fly` → `provides.login` → `login.sh` |
| Credential sink | `~/.aish/credentials` `[profile:fly]` → `FLY_API_TOKEN` |
| React mechanism | `webhook_command` polls `fly status --json` (poll, not push) |
| State written | `fly:last_webhook_output` in `plugins.db` (K/V) |
| Core changes required | **none** — ships on today's surface |

**Relationship to roadmap:** this is the pragmatic interim for stages 2–3 (MCP +
typed tools). It proves the auth + poll loop and gives a scaffold to graduate
into `mcp__fly__*` once gap #4 closes.

---

## 8. Bonus angle: Fly Machines as an ephemeral coordinator-worker backend

Fly's *deploy-and-run* nature opens a door Blacksmith's CI-runner nature never
could. aish today spawns background coordinator workers as **local Docker
containers** (`src/container.rs`, `Dockerfile.worker`). Those builds are the
source of the well-known coordinator-worktree **OOM** risk (`aish_sre` SKILL.md
§3). Fly Machines — boot-in-hundreds-of-ms Firecracker microVMs with a real API —
are an alternative worker backend: `POST /v1/apps/<app>/machines` to spawn,
`DELETE` to reap, exactly mirroring the container lifecycle in `container.rs`.

### 8.1 The comparison (local Docker worker vs. Fly Machine worker)

| | **Local Docker worker** (today) | **Fly Machine worker** (proposed) |
|---|---|---|
| Spawn | `docker run` on the host | `POST /v1/apps/.../machines` (Machines API) |
| Boot latency | seconds (image already local) | ~hundreds of ms cold (Firecracker) |
| Resource pressure | **competes with the host** → OOM risk (`aish_sre` §3) | isolated microVM; host untouched |
| Cost | "free" (your CPU/RAM) but blocks other work | per-second machine billing, stop-to-zero when idle |
| Scale-out | bounded by one machine's RAM | N machines across regions, in parallel |
| Reap | container stop/rm | `machine stop`/`destroy` (or autosuspend) |
| Cleanup guarantee | reaper (`docs/spikes/S1.4-reaper-vs-waitpid.md`) | API destroy + Fly's own idle GC |

### 8.2 When each wins

- **Local Docker:** the default for single, light workers where host headroom is
  ample and there's no build pressure. Zero marginal cash cost.
- **Fly Machine:** the choice for **heavy or fan-out** coordinator runs — the
  exact case that OOM-kills local worktrees. Offloading a `cargo test`-class
  worker to a right-sized Fly Machine **eliminates the host OOM** (mirrors the
  Blacksmith-Testbox conclusion in `blacksmith-plugin-integration.md` §8, but for
  *compute workers* rather than *CI*) and lets multiple workers run truly in
  parallel across regions.

### 8.3 Rule of thumb

*Keep local Docker for light/single workers; route heavy or parallel fan-out
coordinator runs to Fly Machines* — pay per-second compute to erase the OOM class
and unlock real horizontal fan-out. This requires no new **plugin** surface (it's
a `container.rs` backend choice), but a `fly` plugin that owns the
`[profile:fly]` token and a "spawn/reap machine" tool (gap #3) is the natural
place to expose it to the agent.

---

## 9. Missing plugin registry (the distribution gap)

Every gap in §4 is a *capability* gap — what a plugin can **do** once it is on
disk. §9 is a different axis entirely: **distribution** — how a plugin **gets**
on disk in the first place. Even a perfect `fly` plugin (login + poll today,
`mcp__fly__*` after gap #4 closes) is worthless if there is no supported way to
install, version, or update it. Today there is not.

### 9.1 How plugins are actually loaded

Plugin loading is a **local directory walk, nothing more**. `plugins::discover`
(`src/plugins.rs`) does exactly one thing: `read_dir(~/.aish/plugins/)`, and for
each subdirectory parse `plugin.json`, load its `skills/`, `schemas/`, and
`.mcp.json`. There is no network step, no manifest catalog, no signature check,
no version resolution, no `aish plugin add`. A plugin exists on your machine iff
**you** placed its files under `~/.aish/plugins/<id>/` by hand (or a script did).

That means the stage-0 `fly` plugin from §7 ships to a user's machine by
precisely one mechanism today: *manually* create `~/.aish/plugins/fly/`, write
`plugin.json` and `login.sh`, `chmod +x` the handler, and drop in the optional
`skills/fly-ops/SKILL.md`. There is no `aish plugin add fly`, no
`aish plugin update fly`, no pinned version, and no discovery of "what fly
plugins exist."

### 9.2 The asymmetry that makes this glaring

The gap is stark because the **sibling skill subsystem already has the whole
registry stack** the plugin subsystem lacks:

| Concern | **Skills** (has a registry) | **Plugins** (has none) |
|---|---|---|
| Discovery | `aish --skill-search <query>` → registry catalog | directory listing of `~/.aish/plugins/` only |
| Catalog | `~/.aish/registry/index.json` (binary-embedded, offline) | — |
| Remote source | `AISH_SKILL_REGISTRY` → skill.fish / mcpmarket / mirror | — |
| Install | `:skill add <owner/name>` (fetch → import) | copy files by hand |
| Update / version | ref-pinned fetch, re-import | — (edit files in place) |
| Recommend-on-miss | `skill_match.rs` nudges an installable registry skill | — |
| Code | `src/skill_provider.rs`, `initialize_registry` | `src/plugins.rs::discover` (local `read_dir`) |

The irony is direct: **a plugin's job is to *contribute skills into* that skill
registry** (`plugin_skills` flattens each plugin's `skills/` into the catalog),
yet the plugin *carrying* those skills has no registry of its own. The transport
has richer distribution than the thing being transported.

### 9.3 Impact on the Fly plugin

| Stage (from §5) | Blocked by the registry gap? | Consequence |
|---|---|---|
| 0 — login + poll | Not *blocked*, but un-distributable | Every user hand-installs `~/.aish/plugins/fly/`; no shared, versioned artifact |
| 2 — `mcp__fly__*` MCP server | Amplified | Closing gap #4 produces a bundled MCP plugin with **no channel to ship it** — capability without distribution |
| 3 — typed tools | Amplified | Same: the higher-value the plugin, the more its lack of an install/update path hurts |

The registry gap is **orthogonal** to §4's capability gaps but **multiplies**
their payoff risk: the more valuable a plugin becomes (Fly's centre of gravity is
gap #4, the MCP server), the more its absence of a supported install/version/
update path bottlenecks adoption. You can build `mcp__fly__deploy`; you cannot
`aish plugin add fly` it onto a fleet.

### 9.4 What closing it looks like

The cheapest path is to **reuse the skill-registry machinery** rather than invent
a parallel one — the shapes already rhyme:

1. **Plugin catalog** — a `plugins`-flavoured index (mirror
   `~/.aish/registry/index.json`) listing `id / version / source / description`,
   with an `AISH_PLUGIN_REGISTRY` override paralleling `AISH_SKILL_REGISTRY`.
2. **`aish plugin add <ref>`** — fetch → verify → materialise under
   `~/.aish/plugins/<id>/`, the plugin analogue of `:skill add`. `discover`
   needs no change; it already loads whatever lands on disk.
3. **Versioning / update** — `plugin.json` already carries `version`; a registry
   makes it meaningful (pin, diff, `aish plugin update`).
4. **Recommend-on-miss (optional)** — when the agent wants a capability a plugin
   would provide, nudge an installable registry plugin, exactly as
   `skill_match.rs` nudges skills.

None of this touches the capability tracks (webhooks, hooks, tools, MCP). It is a
pure **distribution** layer — and until it exists, "ship the `fly` plugin" means
"paste files into `~/.aish/plugins/fly/`," which is fine for one developer and a
non-starter for a fleet.

> **Gap #8 (registry/distribution).** Add to §4's list: *No plugin registry —
> plugins are local-disk-only, discovered by a `read_dir` of `~/.aish/plugins/`;
> there is no catalog, install command, or version/update path (skills have all
> three).* Track: **distribution** (new, unscoped). Status: **Absent.** Impact:
> caps adoption of every plugin stage, most acutely the high-value MCP plugin
> (stage 2). Where it lives: `src/plugins.rs::discover`.

---

## Appendix: source references

| Claim | Evidence |
|---|---|
| Manifest surface (`webhook_url`, `provides.login`, `config_schema`, `provides`) | `src/plugins.rs` `PluginManifest` / `Provides` |
| Outbound-only, non-blocking dispatch; only `workspace_open` wired | `docs/plugin-webhook-events.md`; `src/plugin_dispatcher.rs` |
| Blocking/mutating hooks are design-only (Phases 2/3) | `docs/aish-hooks-design.md` §1.2, §2 |
| Login handler exists (single-shot JSON→`[profile:*]`) | `src/plugin_auth.rs` (~24 KB) |
| K/V-only state store | `src/plugin_state.rs`; `docs/plugin-state-schema.md` |
| Plugin skills + JSON-schema contribution | `src/plugins.rs` `discover` / `PluginSchema` |
| Local Docker worker backend + OOM risk | `src/container.rs`; `Dockerfile.worker`; `aish_sre` SKILL.md §3; `docs/spikes/S1.4-reaper-vs-waitpid.md` |
| Fly Machines API / `flyctl` / GraphQL / token auth | fly.io platform docs (`api.machines.dev/v1`, `api.fly.io/graphql`, `fly auth token`) |
| Plugins are local-disk-only (no registry): loaded by a `read_dir` of `~/.aish/plugins/` | `src/plugins.rs` `discover` / `default_plugins_dir` |
| Skills, by contrast, have a full registry (catalog, `AISH_SKILL_REGISTRY`, install, recommend-on-miss) | `src/skill_provider.rs`; `registry/index.json`; `src/skill_match.rs` |
