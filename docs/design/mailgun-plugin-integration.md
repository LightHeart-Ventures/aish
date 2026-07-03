# Mailgun ↔ aish Plugin Integration — Design & Gap Analysis

Status: **Draft for review** · Owner: aish core · Scope: design only (no implementation)

This document captures the design for a **Mailgun** aish plugin, the concrete
opportunities to integrate Mailgun with aish, and the gaps in aish's **current**
plugin capability surface that block a full-fidelity Mailgun plugin. It is
grounded in the plugin code that ships today (`src/plugins.rs`,
`src/plugin_dispatcher.rs`, `src/plugin_state.rs`, `src/plugin_memory.rs`,
`src/plugin_auth.rs`) and the two extensibility tracks already designed but not
fully built: the **plugin webhook** track
([`plugin-webhook-events.md`](../plugin-webhook-events.md)) and the
**lifecycle hooks** track ([`aish-hooks-design.md`](../aish-hooks-design.md)).

It is a deliberate companion to
[`blacksmith-plugin-integration.md`](./blacksmith-plugin-integration.md) and
[`slack-plugin-integration.md`](./slack-plugin-integration.md), and uses the
same structure. Mailgun sits alongside Slack on the "rich public API" end of the
axis — but with a twist: Mailgun's outbound is a **single, simple, high-value
HTTP call** (send an email), while its inbound is *two* distinct planes (event
webhooks **and** inbound-route email parsing). That makes it the cleanest
demonstration of "outbound ships today, the two-way ceiling needs the broker."

---

## 1. What Mailgun is

Mailgun is a transactional + programmatic **email** platform (an API-first email
service provider). Like Slack and unlike Blacksmith, the API *is* the product —
a plugin can genuinely *send and receive mail*:

| Capability | What it does | Integration relevance |
|---|---|---|
| Messages API (`POST /v3/<domain>/messages`) | Send email over HTTP with an API key | The **simplest** outbound path — buildable today |
| Templates + variables | Server-stored templates, per-recipient substitution | Turns a notify into a branded, structured message |
| Tags + tracking | Per-message tags, open/click tracking | Correlate a mail back to the shell event that sent it |
| Event webhooks | Mailgun → your endpoint push: `delivered`, `opened`, `clicked`, `bounced`, `complained`, `unsubscribed`, `failed` | **Inbound**: react to deliverability outcomes |
| Inbound Routes | Match incoming mail by recipient/pattern → forward parsed MIME to a URL | **Inbound**: drive aish *from* an email reply |
| Email Validation API | Syntax/deliverability check for an address | Typed pre-send guard (`mailgun_validate`) |
| Suppressions API | Bounce/unsubscribe/complaint lists | Compliance-aware sending; queryable state |
| Events API (poll) | Query historical events (no webhook needed) | Poll-based alternative to inbound webhooks |

**Critical finding:** Mailgun's control-and-data plane is a genuine, mature,
bidirectional HTTP API. That puts it firmly in Slack's camp, not Blacksmith's:

1. **Outbound is trivial and high-value today.** aish already ships a
   fire-and-forget `webhook_command`/`webhook_url` dispatcher — pointing it at a
   two-line `curl` that hits the Messages API is a stage-0 plugin with **zero
   core changes**. "Email me when the background worker finishes" ships now.
2. **Inbound has *two* value ceilings** — reacting to **event webhooks**
   (a bounce/complaint should annotate or alert) and reacting to **inbound-route
   mail** (reply to `aish@your-domain` to steer a run). Both land on the exact
   same inbound-broker gap that blocks Blacksmith C–F and Slack D.
3. **The ideal shape is a bundled MCP server** (a thin wrapper over the Messages
   / Events / Validation APIs), giving the agent typed `mailgun_send` /
   `mailgun_events` / `mailgun_validate` tools — blocked only by the
   "plugin-contributed MCP server" gap.

Mailgun's distinctive contribution to the plugin-surface stress-test: it is the
first integration where the **inbound plane itself is bifurcated** (async
deliverability events *vs.* inbound parsed mail), which sharpens the argument
for a *typed, filterable* inbound broker rather than a single catch-all sink.

---

## 2. Integration opportunities (the target plugin)

A `mailgun` aish plugin would ideally contribute:

| # | Feature | aish surface required | Buildable today? |
|---|---|---|---|
| A | `aish login mailgun` → store API key + domain in `[profile:mailgun]` | `provides.login` + `plugin_auth` | **Yes** — surface exists |
| B | Email on shell events (job done, skill loaded, tool run) via Messages API | outbound `webhook_command`/`webhook_url` | **Yes** — outbound ships |
| C | Rich templated mail (worker result, PR link, run summary) via templates/tags | outbound webhook + key from A | **Yes** (payload-only; no reply loop) |
| D | React to **event webhooks** (`bounced`/`complained`/`delivered` → alert/annotate) | inbound webhook broker | **No** — inbound is SPR-059 |
| E | React to **inbound-route mail** (reply-to-`aish@domain` steers a run) | inbound webhook broker | **No** — inbound is SPR-059 |
| F | Typed `mailgun_send` / `mailgun_validate` / `mailgun_events` agent tools | plugin-contributed **tools** | **No** |
| G | `mcp__mailgun__*` tools via a bundled MCP server | plugin-contributed **MCP server** | **No** |
| H | Block a `git push`/deploy until an email **approval reply** arrives | **blocking** lifecycle hook | **No** (design-only) |
| I | Inject "last deliverability status / bounce context" into a turn | **mutating** lifecycle hook | **No** (design-only) |

Features A–C are near-term wins and are **on par with Slack's** — Mailgun's
Messages API makes C real today. D–I are the capability gaps below. Note Mailgun
adds a feature Slack lacks a clean analog for: **F's `mailgun_validate`** is a
pure, side-effect-free typed tool that would be valuable even before any webhook
plumbing lands.

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

The load-bearing fact for Mailgun: the **auth** surface (store the API key +
sending domain) and the **outbound-webhook** surface (POST to the Messages API
on an event) both ship and are usable now. Everything that would let Mailgun
*act back on the shell* (inbound events, inbound mail, tools, MCP,
blocking/mutating hooks) is unbuilt or design-only.

Note the `webhook_command` result is captured to the plugin state store under
`<plugin_id>:last_webhook_output` (exit code + stdout/stderr) — enough to record
"did the last send return a Mailgun message id / 200?" without any new surface.

---

## 4. Gap analysis

Six gaps separate today's surface from the target `mailgun` plugin. Each is
tagged with the track it belongs to, because two distinct tracks are involved
and conflating them is the main planning hazard:

- **Webhook track** — `plugin_dispatcher.rs` + broker. *Shell ↔ plugin* event
  plumbing. Outbound half ships; the inbound broker is SPR-059.
- **Hooks track** — `aish-hooks-design.md`, `~/.aish/hooks.json`, `:hooks`. The
  *blocking* and *mutating* gates live here (design-only), **not** in SPR-059.

| # | Gap | Impact on Mailgun plugin | Track / where it lives | Status |
|---|---|---|---|---|
| 1 | No inbound webhook broker — Mailgun can't drive aish (event webhooks *or* inbound-route mail) | Blocks features D & E (both two-way wins) | Webhook track **Phase 4/5** (SPR-059) | Design/scoped |
| 2 | Outbound event sites incomplete (`tool_invoked`, `background_job_*` defined but not wired) | Weakens B: fewer moments to email on | Webhook track, "land incrementally Phase 1.6+" | Partial |
| 3 | No inbound / mutation hooks — can't block a push on an email approval or inject bounce context | Blocks features H & I | Hooks-design **Phase 2** (blocking) / **Phase 3** (mutating) | Design-only |
| 4 | No plugin-contributed **tools/commands** | Blocks F: no typed `mailgun_send`/`mailgun_validate`/`mailgun_events` | Unscoped on any roadmap | Absent |
| 5 | No plugin-contributed **MCP servers** | Blocks G: the *ideal* Mailgun shape (`mcp__mailgun__*`) | Unimplemented, unscoped | Absent |
| 6 | Plugin state store is K/V only (no query/index) | Weak deliverability/suppression history for "who bounced this week?" | `plugin_state.rs` | Absent |

Like Slack, there is **no external-API gap** — Mailgun's API is rich and public,
so every remaining limitation is on the aish side. Mailgun sharpens gap #1: its
inbound plane is *two* event families (deliverability webhooks and parsed mail),
which is the strongest argument yet that the SPR-059 broker needs **typed,
filterable** inbound routing (by event type / source) rather than one sink.

### 4.1 Relationship to SPR-059

SPR-059 delivers the **inbound webhook broker** (plugin-Phase 4) + **handler
dispatch** (Phase 5) + a reference **GitHub plugin** (Phase 6). Mapped onto the
gaps:

- It makes gap #1 concrete (inbound external→plugin events) — the plumbing that
  lets a Mailgun `bounced` webhook *or* an inbound-route reply hit an aish
  handler. This is the single biggest unlock for the two-way Mailgun story
  (features D & E).
- It does **not** deliver the blocking/mutating gate (gap #3) — that is the
  hooks track, a separate sprint. An email *approval that blocks a push* (H)
  needs the inbound broker **and** the blocking hook.
- It does **not** touch gaps #2, #4, #5, #6.
- Its follow-on Phases 7–12 are *config, enable/disable, testing, docs, error
  handling* — hardening, not new capability surfaces.

Caveat: as of this writing SPR-059's tasks are not decomposed on the board —
only the sprint-goal card (`card_73605f4530c3`) exists; the ~18 tasks live in
the goal text, so scope can still shift before activation (target 2026-07-16).

Net: SPR-059 unblocks **features D & E (inbound Mailgun → aish)** and gives a
reference plugin to copy. It leaves **F, G, H, I** open (F/G on tools+MCP, H/I on
hooks).

---

## 5. Proposed roadmap

Ordered by value-per-effort for the Mailgun use case:

| Stage | Work | Unlocks | Depends on |
|---|---|---|---|
| 0 | Ship `mailgun` plugin: `login` + `webhook_command` sending via Messages API on `background_job_complete` | A, minimal B | Today's surface |
| 1 | Templated mail via API key (server templates, tags, tracking) — worker result, PR link, run summary | C | A (key) + gap #2 for more triggers |
| 2 | Wire remaining outbound event sites (`tool_invoked`, `background_job_*`) | Fuller B | Gap #2 |
| 3 | Inbound broker: Mailgun **event webhooks** (`bounced`/`complained`/`delivered`) → handler | D (react to deliverability) | Gap #1 (SPR-059) |
| 4 | Inbound broker: **inbound-route mail** (reply-to-`aish@domain`) → handler | E (drive aish by email) | Gap #1 (SPR-059) |
| 5 | **Blocking hook** (`PreToolUse` on `git push`/deploy → wait on an email approval reply) | H (approval-gated push) | Gap #1 + Gap #3a |
| 6 | **Mutating hook** (`UserPromptSubmit`/inject last-run deliverability + bounce context) | I (Mailgun context inject) | Gap #3b |
| 7 | Plugin-contributed **tools** / bundled **MCP server** (`mailgun_send`, `mailgun_validate`, `mcp__mailgun__*`) | F, G (agent-native) | Gaps #4, #5 |

Stage 0 is *immediate* and on par with Slack's — Mailgun accepts a real,
recipient-addressed message payload. Stages 3–5 are the **strategic** ones:
"react when a customer email bounces" and "reply to an email to approve the
deploy" are what make this more than a notification script — and they need the
inbound broker (SPR-059) plus, for the gate, the hooks track. Stage 7's
`mailgun_validate` is worth pulling forward independently: as a side-effect-free
typed tool it delivers value without any inbound plumbing.

---

## 6. Recommendation

1. **Now:** build the stage-0 `mailgun` plugin against today's surface
   (`login` + `webhook_command` → Messages API). It is real value with zero core
   changes and is a clean demonstration of the outbound webhook path — one
   `curl` to `POST /v3/<domain>/messages` turns any shell event into an email.
2. **Fast-follow:** stage 1 (server templates + tags) — turns the notification
   into a *branded, correlatable* message (worker summary, PR link, run status)
   using only the already-shipping auth surface.
3. **Post-SPR-059:** land the inbound broker consumer for **event webhooks**
   (stage 3) first — reacting to `bounced`/`complained` is the highest-leverage,
   lowest-ambiguity inbound feature and reuses the SPR-059 reference plugin
   wholesale. Add inbound-route mail (stage 4) once the broker's typed routing
   proves out.
4. **Then:** hooks-design **Phase 2 (blocking)** for email-approval-gated pushes
   (stage 5) — it benefits every plugin, not just Mailgun — followed by
   tools + a bundled MCP server (stage 7) for the agent-native experience.
5. **Do not** hand-roll a Mailgun API client in the core — keep Mailgun
   knowledge in the plugin (`login.sh` + `webhook_command` script), and reach
   for a bundled MCP wrapper the moment the MCP gap (#5) closes.

---

## 7. Concrete implementation: stage-0 notifier (buildable today)

**Status:** Design-only sketch — no core changes required; runs on the shipping
`webhook_command` dispatcher.

A minimal `mailgun` plugin needs only a manifest plus a two-line send script. The
dispatcher pipes the event JSON on stdin and sets `AISH_EVENT_TYPE` /
`AISH_PLUGIN_ID` in the child env (see `plugin-webhook-events.md`).

`~/.aish/plugins/mailgun/plugin.json`:

```json
{
  "id": "mailgun",
  "name": "Mailgun Notifier",
  "version": "0.1.0",
  "description": "Email aish shell events via the Mailgun Messages API",
  "enabled": true,
  "webhook_command": "~/.aish/plugins/mailgun/notify.sh",
  "provides": { "login": "login.sh" }
}
```

`~/.aish/plugins/mailgun/notify.sh` (reads event JSON on stdin, sends via Mailgun):

```sh
#!/bin/sh
# $AISH_EVENT_TYPE is the wire name; API key + domain + recipient come from
# [profile:mailgun] (exported as MAILGUN_API_KEY / MAILGUN_DOMAIN / MAILGUN_TO).
event="$(cat)"
cwd="$(printf '%s' "$event" | jq -r '.payload_json.cwd // empty')"
subject="aish: ${AISH_EVENT_TYPE}"
body="Event ${AISH_EVENT_TYPE} in ${cwd:-unknown}"
curl -s --user "api:${MAILGUN_API_KEY}" \
  "https://api.mailgun.net/v3/${MAILGUN_DOMAIN}/messages" \
  -F from="aish <aish@${MAILGUN_DOMAIN}>" \
  -F to="${MAILGUN_TO}" \
  -F subject="$subject" \
  -F text="$body"
```

| Item | Value |
|---|---|
| Manifest | `~/.aish/plugins/mailgun/plugin.json` (`webhook_command` + `provides.login`) |
| Trigger today | `workspace_open` (only wired event); `background_job_complete` once gap #2 lands |
| Auth | `aish login mailgun` → `login.sh` writes `[profile:mailgun]` (API key, sending domain, default recipient) |
| Delivery | Fire-and-forget, non-blocking, **10s** per-event timeout (dispatcher default) |
| Result capture | `<plugin_id>:last_webhook_output` in `plugins.db` (exit code + stdout/stderr — includes the Mailgun message id on success) |
| Failure mode | Logged on the `plugin-events` channel (`AISH_PLUGIN_EVENTS=1`); never blocks the REPL |

**Relationship to roadmap:** this is stage 0. It validates the auth +
outbound-webhook path end to end and is the scaffold every richer stage
(templates, inbound events, inbound mail, tools) builds on.

---

## 8. Mailgun vs. Slack vs. Blacksmith — one surface, three shapes

The three plugin designs share one capability surface but exercise it
differently, which is the clearest way to reason about *what the plugin system
is missing*:

| Dimension | Blacksmith | Slack | Mailgun |
|---|---|---|---|
| Public API | **None** — control plane *is* GitHub Actions | **Rich** — Web API, Events API, Block Kit | **Rich** — Messages, Events, Validation, Routes |
| Best outbound today | Notify on GitHub `workflow_run` webhook | Post a real message (Incoming Webhook) | **Send a real email** (Messages API `curl`) |
| Stage-0 value | Real, but only a fire signal | Real **and** carries a useful payload | Real **and** delivers an addressed email |
| Inbound plane | One (GitHub `workflow_run`/`check_run`) | One (Events API / slash / buttons) | **Two** (deliverability webhooks + inbound-route mail) |
| Two-way ceiling | Gate a push on green CI (hooks track) | `/aish` slash + approval buttons | React to bounce/complaint; reply-to-approve a deploy |
| Dominant gap | External (#5, no API) + inbound broker | **All internal** — broker, tools, MCP, hooks | **All internal** — broker, tools, MCP, hooks |
| Ideal shape | Bundled MCP wrapping `gh` + dashboard | Bundled **official Slack MCP** server | Bundled MCP wrapping Messages/Events/Validation |

Takeaway: Mailgun, like Slack, removes the "no external API" excuse — but it goes
one step further and **bifurcates the inbound plane**. That makes it the best
forcing function for the specific SPR-059 design decision of *typed, filterable*
inbound routing: a `mailgun` plugin wants to subscribe to `bounced` events
*separately* from inbound-route mail, not receive an undifferentiated firehose.
Whatever lands for Mailgun's two-way story — especially typed inbound filtering —
is directly reusable by every other API-backed integration.

---

## Appendix: source references

| Claim | Evidence |
|---|---|
| Manifest surface (`webhook_url`, `webhook_command`, `provides.login`, `config_schema`) | `src/plugins.rs` `PluginManifest` / `Provides` |
| Outbound-only, non-blocking dispatch, 10s timeout; only `workspace_open` wired | `docs/plugin-webhook-events.md`; `src/plugin_dispatcher.rs` |
| `webhook_command` env (`AISH_EVENT_TYPE`, `AISH_PLUGIN_ID`) + result captured to `plugins.db` | `docs/plugin-webhook-events.md` §Command delivery |
| Blocking/mutating hooks are design-only (Phases 2/3) | `docs/aish-hooks-design.md` §1.2, §2 |
| Login handler exists (`aish login <plugin>` → `[profile:*]`) | `src/plugin_auth.rs` (~24 KB) |
| K/V-only state store | `src/plugin_state.rs`; `docs/plugin-state-schema.md` |
| Inbound broker + reference plugin scoped in SPR-059 | `blacksmith-plugin-integration.md` §4.1 (`card_73605f4530c3`) |
