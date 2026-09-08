# Slack ↔ aish Plugin Integration — Design & Gap Analysis

Status: **Draft for review** · Owner: aish core · Scope: design only (no implementation)

This document captures the design for a **Slack** aish plugin, the concrete
opportunities to integrate Slack with aish, and the gaps in aish's **current**
plugin capability surface that block a full-fidelity Slack plugin. It is
grounded in the plugin code that ships today (`src/plugins.rs`,
`src/plugin_dispatcher.rs`, `src/plugin_state.rs`, `src/plugin_memory.rs`,
`src/plugin_auth.rs`) and the two extensibility tracks already designed but not
fully built: the **plugin webhook** track
([`plugin-webhook-events.md`](../plugin-webhook-events.md)) and the
**lifecycle hooks** track ([`aish-hooks-design.md`](../aish-hooks-design.md)).

It is a deliberate companion to
[`blacksmith-plugin-integration.md`](./blacksmith-plugin-integration.md) and
uses the same structure. The two integrations sit at **opposite ends of one
axis**: Blacksmith has *no* public API and forces everything through GitHub as
the data plane; Slack has a *rich, mature* public API, so the constraint flips —
the outbound direction is strong **today**, and the gaps are all about letting
Slack *act back on the shell*.

---

## 1. What Slack is

Slack is a team-messaging platform with a first-class developer surface. Unlike
Blacksmith, its API is the point — a plugin can genuinely *talk to Slack*:

| Capability | What it does | Integration relevance |
|---|---|---|
| Incoming Webhooks | POST JSON to a channel URL, zero auth beyond the URL | The **simplest** outbound path — buildable today |
| Web API (`chat.postMessage`, …) | Full REST API, Bot/User OAuth tokens, Block Kit | Rich outbound: threads, attachments, buttons |
| Block Kit | Structured message layout + interactive components | How a notification becomes actionable |
| Events API | Slack → your endpoint push (messages, mentions, reactions) | **Inbound**: react to Slack activity |
| Slash commands / Interactivity | `/aish …` and button clicks POST to your endpoint | **Inbound**: drive aish *from* Slack |
| Socket Mode | Inbound events over an outbound WebSocket (no public URL) | Inbound without exposing an ingress |
| Official Slack MCP server | MCP tools for channels/messages/users | The **ideal** agent-native shape |

**Critical finding:** Slack's control-and-data plane is a genuine bidirectional
API. That inverts the Blacksmith conclusion:

1. **Outbound is easy and high-value today.** aish already ships a
   fire-and-forget `webhook_command`/`webhook_url` dispatcher — pointing it at a
   Slack Incoming Webhook is a stage-0 plugin with **zero core changes**.
2. **Inbound (Slack → aish) is where the value ceiling is** — a `/aish` slash
   command or "approve this deploy" button — and it lands on the exact same
   inbound-broker + tools/MCP gaps that block Blacksmith features C–F.
3. **The ideal shape is a bundled MCP server** (official Slack MCP or a thin
   wrapper), giving the agent typed `slack_post_message` / `slack_read_channel`
   tools — blocked only by the "plugin-contributed MCP server" gap.

So Slack is the mirror image of Blacksmith: the *first* rung is trivially
buildable, and the top of the ladder needs the same unbuilt surfaces.

---

## 2. Integration opportunities (the target plugin)

A `slack` aish plugin would ideally contribute:

| # | Feature | aish surface required | Buildable today? |
|---|---|---|---|
| A | `aish login slack` → store Bot token in `[profile:slack]` | `provides.login` + `plugin_auth` | **Yes** — surface exists |
| B | Notify a channel on shell events (job done, skill loaded, tool run) | outbound `webhook_command`/`webhook_url` | **Yes** — outbound ships |
| C | Rich Block Kit notifications (worker result, PR link, buttons) | outbound webhook + token from A | **Yes** (payload-only; no reply loop) |
| D | React to Slack events (`/aish run …`, mention, button) | inbound webhook broker | **No** — inbound is SPR-059 |
| E | Typed `slack_post` / `slack_read` agent tools | plugin-contributed **tools** | **No** |
| F | `mcp__slack__*` tools via a bundled MCP server | plugin-contributed **MCP server** | **No** |
| G | Block a `git push`/deploy until a Slack **approval** click | **blocking** lifecycle hook | **No** (design-only) |
| H | Inject "last Slack thread / on-call context" into a turn | **mutating** lifecycle hook | **No** (design-only) |

Features A–C are near-term wins and are **strictly further along than
Blacksmith's** (Slack's API makes C real today, where Blacksmith could only
notify). D–H are the capability gaps below.

---

## 3. Current plugin capability surface (what ships)

Verified against `src/plugins.rs` manifest structs and sibling modules:

| Surface | Module | State |
|---|---|---|
| Manifest `id/name/version/description/enabled` | `plugins.rs` (`PluginManifest`) | ✅ |
| `webhook_url` / `webhook_command` (outbound) | `plugin_dispatcher.rs` | ✅ (fire-and-forget, non-blocking, 10s timeout) |
| `config_schema` + `load_config` (Phase 1.4) | `plugins.rs` | ✅ |
| `provides.lifecycle_hooks` (`on_init`/`on_shell_ready`/`on_shutdown`/…) | `plugins.rs` | ✅ (loader lifecycle only) |
| `provides.login` → `login.sh` → `[profile:*]` | `plugin_auth.rs` (~24 KB) | ✅ |
| Plugin K/V state store | `plugin_state.rs` (`plugins.db`) | ✅ (K/V, no query) |
| Plugin memory store | `plugin_memory.rs` (~30 KB) | ✅ |
| Outbound event types: `workspace_open`, `skill_loaded`, `background_job_start/complete`, `tool_invoked` | `plugin_dispatcher.rs` / webhook-events doc | ⚠️ defined; only `workspace_open` wired |

The load-bearing fact for Slack: the **auth** surface (store the Bot token) and
the **outbound-webhook** surface (POST a message on an event) both ship and are
usable now. Everything that would let Slack *act back on the shell* (inbound
events, tools, MCP, blocking/mutating hooks) is unbuilt or design-only.

Note the `webhook_command` result is captured to the plugin state store under
`<plugin_id>:last_webhook_output` (exit code + stdout/stderr) — enough to record
"did the last Slack post succeed?" without any new surface.

---

## 4. Gap analysis

Six gaps separate today's surface from the target `slack` plugin. Each is tagged
with the track it belongs to, because two distinct tracks are involved and
conflating them is the main planning hazard:

- **Webhook track** — `plugin_dispatcher.rs` + broker. *Shell ↔ plugin* event
  plumbing. Outbound half ships; the inbound broker is SPR-059.
- **Hooks track** — `aish-hooks-design.md`, `~/.aish/hooks.json`, `:hooks`. The
  *blocking* and *mutating* gates live here (design-only), **not** in SPR-059.

| # | Gap | Impact on Slack plugin | Track / where it lives | Status |
|---|---|---|---|---|
| 1 | No inbound webhook broker — Slack can't drive aish (`/aish`, buttons, Events API) | Blocks feature D (the two-way win) | Webhook track **Phase 4/5** (SPR-059) | Design/scoped |
| 2 | Outbound event sites incomplete (`tool_invoked`, `background_job_*` defined but not wired) | Weakens B: fewer moments to notify on | Webhook track, "land incrementally Phase 1.6+" | Partial |
| 3 | No inbound / mutation hooks — can't block a push on a Slack approval or inject Slack context | Blocks features G & H | Hooks-design **Phase 2** (blocking) / **Phase 3** (mutating) | Design-only |
| 4 | No plugin-contributed **tools/commands** | Blocks E: no typed `slack_post`/`slack_read` | Unscoped on any roadmap | Absent |
| 5 | No plugin-contributed **MCP servers** | Blocks F: the *ideal* Slack shape (`mcp__slack__*`) | Unimplemented, unscoped | Absent |
| 6 | Plugin state store is K/V only (no query/index) | Weak thread/message history for "what did on-call say?" | `plugin_state.rs` | Absent |

Unlike Blacksmith, there is **no external-API gap** — Slack's API is rich and
public, so every remaining limitation is on the aish side, which makes the
Slack integration a clean stress-test of the plugin surface itself.

### 4.1 Relationship to SPR-059

SPR-059 delivers the **inbound webhook broker** (plugin-Phase 4) + **handler
dispatch** (Phase 5) + a reference **GitHub plugin** (Phase 6). Mapped onto the
gaps:

- It makes gap #1 concrete (inbound external→plugin events) — the plumbing that
  lets a Slack slash command or button hit an aish handler. This is the single
  biggest unlock for the two-way Slack story (feature D).
- It does **not** deliver the blocking/mutating gate (gap #3) — that is the
  hooks track, a separate sprint. A Slack *approval that blocks a push* (G)
  needs the inbound broker **and** the blocking hook.
- It does **not** touch gaps #2, #4, #5, #6.
- Its follow-on Phases 7–12 are *config, enable/disable, testing, docs, error
  handling* — hardening, not new capability surfaces.

Caveat: as of this writing SPR-059's tasks are not decomposed on the board —
only the sprint-goal card (`card_73605f4530c3`) exists; the ~18 tasks live in
the goal text, so scope can still shift before activation (target 2026-07-16).

Net: SPR-059 unblocks **feature D (inbound Slack → aish)** and gives a reference
plugin to copy. It leaves **E, F, G, H** open (E/F on tools+MCP, G/H on hooks).

---

## 5. Proposed roadmap

Ordered by value-per-effort for the Slack use case:

| Stage | Work | Unlocks | Depends on |
|---|---|---|---|
| 0 | Ship `slack` plugin: `login` + `webhook_command` posting to an Incoming Webhook on `background_job_complete` | A, minimal B | Today's surface |
| 1 | Rich Block Kit payloads via Bot token (`chat.postMessage`) — worker result, PR link, thread reply | C | A (token) + gap #2 for more triggers |
| 2 | Wire remaining outbound event sites (`tool_invoked`, `background_job_*`) | Fuller B | Gap #2 |
| 3 | Inbound broker: `/aish …` slash command + button interactivity → handler | D (two-way) | Gap #1 (SPR-059) |
| 4 | **Blocking hook** (`PreToolUse` on `git push`/deploy → wait on Slack approval click) | G (approval-gated push) | Gap #1 + Gap #3a |
| 5 | **Mutating hook** (`UserPromptSubmit`/inject on-call + last-thread context) | H (Slack context inject) | Gap #3b |
| 6 | Plugin-contributed **tools** / bundled **MCP server** (`slack_post`, `mcp__slack__*`) | E, F (agent-native) | Gaps #4, #5 |

Stage 0 is *immediate*: a strictly-more-capable version of Blacksmith's stage-0
because Slack accepts a real message payload, not just a fire signal. Stages 3–4
are the **strategic** ones: "approve the deploy from Slack" and "drive aish from
a channel" are what make this more than a notification script — and they need
the inbound broker (SPR-059) plus, for the gate, the hooks track.

---

## 6. Recommendation

1. **Now:** build the stage-0 `slack` plugin against today's surface (`login` +
   `webhook_command` → Incoming Webhook). It is real value with zero core
   changes and is the cleanest possible demonstration of the outbound webhook
   path — Slack's zero-auth Incoming Webhook makes it a one-file plugin.
2. **Fast-follow:** stage 1 (Block Kit via Bot token) — turns the notification
   into a *useful* message (worker summary, PR link, run status) using only the
   already-shipping auth surface.
3. **Post-SPR-059:** land the inbound broker consumer (stage 3) — `/aish` slash
   command is the highest-leverage two-way feature and reuses the SPR-059
   reference plugin wholesale.
4. **Then:** hooks-design **Phase 2 (blocking)** for Slack-approval-gated pushes
   (stage 4) — it benefits every plugin, not just Slack — followed by
   tools + a bundled MCP server (stage 6) for the agent-native experience.
5. **Do not** hand-roll a Slack API client in the core — keep Slack knowledge in
   the plugin (`login.sh` + `webhook_command` script), and reach for the
   **official Slack MCP server** the moment the MCP gap (#5) closes.

---

## 7. Concrete implementation: stage-0 notifier (buildable today)

**Status:** Design-only sketch — no core changes required; runs on the shipping
`webhook_command` dispatcher.

A minimal `slack` plugin needs only a manifest plus a two-line post script. The
dispatcher pipes the event JSON on stdin and sets `AISH_EVENT_TYPE` /
`AISH_PLUGIN_ID` in the child env (see `plugin-webhook-events.md`).

`~/.aish/plugins/slack/plugin.json`:

```json
{
  "id": "slack",
  "name": "Slack Notifier",
  "version": "0.1.0",
  "description": "Post aish shell events to a Slack channel",
  "enabled": true,
  "webhook_command": "~/.aish/plugins/slack/notify.sh",
  "provides": { "login": "login.sh" }
}
```

`~/.aish/plugins/slack/notify.sh` (reads event JSON on stdin, posts to Slack):

```sh
#!/bin/sh
# $AISH_EVENT_TYPE is the wire name; token/URL come from [profile:slack].
event="$(cat)"
text="aish: ${AISH_EVENT_TYPE} — $(printf '%s' "$event" | jq -r '.payload_json.cwd // empty')"
curl -s -X POST -H 'Content-type: application/json' \
  --data "$(jq -n --arg t "$text" '{text:$t}')" \
  "$SLACK_WEBHOOK_URL"
```

| Item | Value |
|---|---|
| Manifest | `~/.aish/plugins/slack/plugin.json` (`webhook_command` + `provides.login`) |
| Trigger today | `workspace_open` (only wired event); `background_job_complete` once gap #2 lands |
| Auth | `aish login slack` → `login.sh` writes `[profile:slack]` (webhook URL and/or Bot token) |
| Delivery | Fire-and-forget, non-blocking, **10s** per-event timeout (dispatcher default) |
| Result capture | `<plugin_id>:last_webhook_output` in `plugins.db` (exit code + stdout/stderr) |
| Failure mode | Logged on the `plugin-events` channel (`AISH_PLUGIN_EVENTS=1`); never blocks the REPL |

**Relationship to roadmap:** this is stage 0. It validates the auth +
outbound-webhook path end to end and is the scaffold every richer stage
(Block Kit, inbound, tools) builds on.

---

## 8. Slack vs. Blacksmith — why the same surface, opposite conclusions

The two plugin designs share one capability surface but reach mirrored verdicts,
which is the clearest way to reason about *what the plugin system is missing*:

| Dimension | Blacksmith | Slack |
|---|---|---|
| Public API | **None** — control plane *is* GitHub Actions | **Rich** — Web API, Events API, Block Kit, MCP |
| Best outbound today | Notify on GitHub `workflow_run` webhook | **Post a real message** (Incoming Webhook / `chat.postMessage`) |
| Stage-0 value | Real, but only a fire signal | Real **and** carries a useful payload |
| Two-way ceiling | Gate a push on green CI (hooks track) | `/aish` slash command + approval buttons (broker + hooks) |
| Dominant gap | External (#5, no API) + inbound broker | **All internal** — inbound broker, tools, MCP, hooks |
| Ideal shape | Bundled MCP wrapping `gh` + dashboard | Bundled **official Slack MCP** server |

Takeaway: Slack removes the "no external API" excuse entirely, so it is the
better forcing function for prioritizing the aish-side gaps — the inbound broker
(#1/SPR-059), plugin tools (#4), plugin MCP (#5), and the blocking/mutating hooks
(#3). Whatever lands for Slack's two-way story is directly reusable by every
other API-backed integration.

---

## Appendix: source references

| Claim | Evidence |
|---|---|
| Manifest surface (`webhook_url`, `webhook_command`, `provides.login`, `config_schema`) | `src/plugins.rs` `PluginManifest` / `Provides` |
| Outbound-only, non-blocking dispatch, 10s timeout; only `workspace_open` wired | `docs/plugin-webhook-events.md`; `src/plugin_dispatcher.rs` |
| `webhook_command` env (`AISH_EVENT_TYPE`, `AISH_PLUGIN_ID`) + result captured to `plugins.db` | `docs/plugin-webhook-events.md` §Command delivery |
| Blocking/mutating hooks are design-only (Phases 2/3); Slack notify is a listed use case | `docs/aish-hooks-design.md` §1.2, §2 (use case #4) |
| Login handler exists (`aish login <plugin>` → `[profile:*]`) | `src/plugin_auth.rs` (~24 KB) |
| K/V-only state store | `src/plugin_state.rs`; `docs/reference/plugins/state.md` |
| Inbound broker + reference plugin scoped in SPR-059 | `blacksmith-plugin-integration.md` §4.1 (`card_73605f4530c3`) |
