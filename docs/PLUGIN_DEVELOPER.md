# aish Plugin Developer Guide

Everything you need to write, configure, test, and ship an aish plugin. This is
the entry point: deep-dive references are linked from each section.

> **Status (SPR-104).** This guide describes `main` **plus the plugin work
> shipping in SPR-104**: `:plugin enable|disable|reload` (TASK-272),
> `:plugin config` (TASK-271), error handling / `:plugin errors` / health
> markers (TASK-274), the webhook debug tools (TASK-273) and the scaffold
> generator `:plugin create` (TASK-275). Webhook handlers have their own guide:
> [WEBHOOK_HANDLERS.md](./WEBHOOK_HANDLERS.md).

- [1. Quick start](#1-quick-start)
- [2. Architecture](#2-architecture)
- [3. Directory layout](#3-directory-layout)
- [4. `plugin.json` reference](#4-pluginjson-reference)
- [5. Lifecycle hooks and event hooks](#5-lifecycle-hooks-and-event-hooks)
- [6. Skills, skill sources, MCP servers, schemas](#6-skills-skill-sources-mcp-servers-schemas)
- [7. Timers and statusline segments](#7-timers-and-statusline-segments)
- [8. Configuration: `config_schema` and `:plugin config`](#8-configuration-config_schema-and-plugin-config)
- [9. Secrets and credentials](#9-secrets-and-credentials)
- [10. Memory and state](#10-memory-and-state)
- [11. Managing plugins: the `:plugin` command](#11-managing-plugins-the-plugin-command)
- [12. Errors and health](#12-errors-and-health)
- [13. Testing your plugin](#13-testing-your-plugin)
- [14. Best practices](#14-best-practices)

---

## 1. Quick start

```text
:plugin create my-plugin        # scaffold ~/.aish/plugins/my-plugin/
:plugin info my-plugin          # confirm the manifest parsed
:restart                        # load its skill + webhook handler
```

`:plugin create <id>` writes the same layout as the canonical
[`plugins/hello-world`](../plugins/hello-world) plugin:

```text
~/.aish/plugins/my-plugin/
├── plugin.json             minimal valid manifest (config_schema + one ping webhook)
├── README.md               next steps
├── skills/my-plugin/SKILL.md
└── handlers/ping.sh        webhook handler (executable)
```

Rules: the id must match `[a-z0-9][a-z0-9._-]{0,63}`; a `scope/` prefix is
dropped (`:plugin create acme/slack` creates `slack/`); an existing directory is
never overwritten. Implementation: `src/plugin_scaffold.rs`.

Where plugins come from:

| Source | How |
|---|---|
| Your own | `:plugin create <id>`, or copy a directory into `~/.aish/plugins/` |
| Registry | `:plugin add <id>` (list: bare `:plugin add`) — see [plugins/INSTALL.md](../plugins/INSTALL.md) |
| This repo | `plugins/` is the canonical tree (`github`, `hello-world`, `signoz-observability`, …). `examples/plugins/hello-world` is a fuller "every surface" reference example. |

---

## 2. Architecture

```text
                     ~/.aish/plugins/<id>/plugin.json  (+ config.json, hooks/, skills/, …)
                                    │  discovery at startup (src/plugins.rs::discover)
                                    │  + ~/.aish/plugins.state.json (enable/disable overrides)
        ┌───────────────┬───────────┼──────────────┬───────────────┬────────────────┐
        ▼               ▼           ▼              ▼               ▼                ▼
   skills/ →       hooks/on_init.sh hooks.json →  .mcp.json →   provides.timers  webhooks[] →
   skill registry  → session env    event hooks   MCP client    provides.        aish-webhook-client
   (+skill_source)  (KEY=VALUE)     (src/hooks.rs) (first wins) statusline        (broker events)
                                                                → 2nd statusline       │
                                                                                       ▼
                         webhook_url / webhook_command ◄── shell lifecycle events   handler argv,
                         (Phase-1.6 dispatcher, src/plugin_dispatcher.rs)           stdin JSON →
                                                                                    statusline flash
```

Design rules that hold everywhere:

- **Forgiving discovery.** A directory without a readable, parseable
  `plugin.json` is skipped; unknown manifest keys are ignored; a broken plugin
  never blocks startup.
- **No shell.** Every program a plugin declares (hooks, timers, statusline,
  `webhook_command`, webhook handlers, `login.sh`) is fork/exec'd as argv —
  `$VAR`, `|`, `;`, backticks are literal bytes. Write a script if you need a
  shell.
- **Plugin dir is CWD** for hooks, timers, and statusline commands; relative
  handler paths resolve against the plugin dir.
- **Bounded.** Hooks, timers, statusline commands and webhook handlers have a
  timeout and are killed on overrun (the legacy `webhook_command` is the one
  exception — keep it fast).

---

## 3. Directory layout

Every file is optional except `plugin.json`.

| Path | Purpose | Section |
|---|---|---|
| `plugin.json` | Manifest | [§4](#4-pluginjson-reference) |
| `config.json` | User-editable config values (managed by `:plugin config`) | [§8](#8-configuration-config_schema-and-plugin-config) |
| `skills/<name>/SKILL.md` | Skills merged into the agent's catalog | [§6](#6-skills-skill-sources-mcp-servers-schemas) |
| `hooks/on_init.sh` | Lifecycle hook run at startup; `KEY=VALUE` stdout → session env | [§5](#5-lifecycle-hooks-and-event-hooks) |
| `hooks.json` | Event-hook fragment (same schema as `~/.aish/hooks.json`) | [§5](#5-lifecycle-hooks-and-event-hooks) |
| `.mcp.json` | MCP servers (same schema as `~/.aish/.mcp.json`) | [§6](#6-skills-skill-sources-mcp-servers-schemas) |
| `schemas/<name>.json` | JSON Schemas for validating structured tool output | [§6](#6-skills-skill-sources-mcp-servers-schemas) |
| `handlers/*.sh` | Webhook handlers referenced from `webhooks[]` | [WEBHOOK_HANDLERS.md](./WEBHOOK_HANDLERS.md) |
| `login.sh` | `aish login <name>` auth handler | [§9](#9-secrets-and-credentials) |
| `memory/<ns>.json` | Plugin memory (written by aish, not shipped) | [§10](#10-memory-and-state) |
| `errors.jsonl` | Error log (written by aish, not shipped) | [§12](#12-errors-and-health) |

---

## 4. `plugin.json` reference

The shell loader parses `PluginManifest` in `src/plugins.rs`; the webhook client
parses its own view (`PluginManifest`/`WebhookHandler` in
`crates/aish-webhook-client/src/dispatcher.rs`). Every key below is enforced by
`tests/plugin_docs_completeness.rs`: adding a manifest field without
documenting it here fails the build.

### Top level

| Key | Type | Default | Meaning |
|---|---|---|---|
| `id` | string | **required** | Stable plugin id. Should equal the directory name. |
| `name` | string | `id` | Display name (`:plugin list`). |
| `version` | string | `""` | Free-form version shown as `v<version>`. |
| `description` | string | `""` | One-liner shown in `:plugin info` / `:plugin add`. |
| `enabled` | bool | `true` | Ship disabled with `false`. `:plugin enable/disable` overrides it ([§11](#11-managing-plugins-the-plugin-command)). |
| `config_schema` | object | none | JSON-Schema-shaped config description ([§8](#8-configuration-config_schema-and-plugin-config)). |
| `provides` | object | none | Capabilities contributed to the shell (below). |
| `webhooks` | array | `[]` | Broker webhook handlers — see [WEBHOOK_HANDLERS.md](./WEBHOOK_HANDLERS.md). The legacy key `handlers` is rejected with a migration warning (TASK-447). |
| `webhook_url` | string | none | Phase-1.6 opt-in: shell lifecycle events are POSTed here as JSON (10 s HTTP timeout). |
| `webhook_command` | string | none | Phase-1.6 opt-in: command run (argv, no shell) per shell lifecycle event, event JSON on stdin, `AISH_EVENT_TYPE` / `AISH_PLUGIN_ID` in env; first stdout line (≤60 chars) flashes on the statusline. Today only `workspace_open` is emitted. |

### `webhooks[]` entries

| Key | Type | Default | Meaning |
|---|---|---|---|
| `event_type` | string | **required** | Broker event type (e.g. `ping`, `pull_request`) or `"*"` for all. |
| `command` | string[] | **required** | argv. A relative path containing `/` resolves against the plugin dir; a bare name uses `PATH`. |
| `filters` | object | `{}` | AND-combined equality on dotted payload paths, e.g. `{"action": "opened"}`. |
| `timeout_secs` | integer | `30` | Per-handler wall-clock timeout; the handler is killed on overrun. |

### `provides`

| Key | Type | Meaning |
|---|---|---|
| `lifecycle_hooks` | string[] | Lifecycle hooks the plugin declares (`on_init`, …), shown by `:plugin info`. The hook *runs* if `hooks/<name>.sh` exists ([§5](#5-lifecycle-hooks-and-event-hooks)). |
| `hooks` | string[] | **Deprecated** alias of `lifecycle_hooks` (one-time warning at discovery). |
| `login` | string | Login command this plugin owns: `aish login <name>` runs `login.sh` ([§9](#9-secrets-and-credentials)). |
| `skill_source` | object | Federated `:skill search` / `:skill add` provider (below). |
| `timers` | array | Background timers ([§7](#7-timers-and-statusline-segments)). |
| `statusline` | object | First-class SecondStatusLine segment ([§7](#7-timers-and-statusline-segments)). |

### `provides.skill_source`

| Key | Type | Default | Meaning |
|---|---|---|---|
| `id` | string | plugin id | Label in the SOURCE column / `:skill sources`. |
| `priority` | integer | `0` | Higher wins dedup ties and is tried first for `add`. |
| `search` | string | none | Script (relative to plugin dir) answering `:skill search`. Absent → add-only. |
| `add` | string | none | Script resolving `:skill add <ref>`. Absent → search-only. |
| `handles` | string[] | `[]` | Ref patterns routed to `add` (`"github:*"`, `"acme/*"`, `"*"`). |

Handler protocol: [plugins/skill-source-authoring.md](./plugins/skill-source-authoring.md).

### `provides.timers[]`

| Key | Type | Default | Meaning |
|---|---|---|---|
| `command` | string | **required** | Program (`argv[0]`): a file in the plugin dir, else `PATH`. |
| `args` | string[] | `[]` | Extra arguments. |
| `every` | string | **required** | Interval: `"30s"`, `"10m"`, `"1h"`, `"1d"`, bare integer = seconds. Zero/unparseable disarms the timer. |
| `cache` | string | none | File the stdout is written to (relative → under `~/.aish/`). |
| `timeout_ms` | integer | `60000` | Per-run timeout; overrun is killed and skipped. |

### `provides.statusline`

| Key | Type | Default | Meaning |
|---|---|---|---|
| `command` | string | **required** | Program; first non-empty stdout line becomes the segment (you own any ANSI color). |
| `args` | string[] | `[]` | Extra arguments. |
| `every` | string | `"10m"` (600 s) | Refresh cadence, same grammar as timers. |
| `timeout_ms` | integer | `60000` | Per-run timeout. |

### Complete example

```json
{
  "id": "acme",
  "name": "Acme Tools",
  "version": "1.2.0",
  "description": "Acme deploy status + PR notifications",
  "enabled": true,
  "config_schema": {
    "type": "object",
    "properties": {
      "region":    { "type": "string", "default": "us-east-1", "enum": ["us-east-1", "eu-west-1"] },
      "api_token": { "type": "string", "default": "${env:ACME_TOKEN}" }
    },
    "required": ["api_token"]
  },
  "provides": {
    "lifecycle_hooks": ["on_init"],
    "login": "acme",
    "timers": [{ "command": "bin/refresh.sh", "every": "5m", "cache": "state/acme.json" }],
    "statusline": { "command": "bin/segment.sh", "every": "60s", "timeout_ms": 2000 }
  },
  "webhooks": [
    { "event_type": "deploy", "command": ["handlers/deploy.sh"], "filters": { "status": "failed" }, "timeout_secs": 15 }
  ]
}
```

---

## 5. Lifecycle hooks and event hooks

aish has two hook surfaces; don't confuse them.

**Lifecycle hooks** — `hooks/<hook>.sh` in the plugin dir.

- `on_init` runs once at startup, before the prompt, and again for one plugin on
  `:plugin enable <id>` / `:plugin reload <id>`. It is the only lifecycle hook
  fired today; `on_shell_ready`, `on_shutdown`, `on_webhook_url_changed` are
  reserved names you may declare but aish does not yet invoke.
- fork/exec, CWD = plugin dir, stdin `/dev/null`, `AISH_IN_HOOK=1`, the session
  env, plus your own credentials as `AISH_PROFILE_<ID>_<FIELD>` ([§9](#9-secrets-and-credentials)).
- **Timeout 10 s** (fixed). An overrun is killed and warned:
  ``plugin `<id>` hook `on_init` timed out after 10s — killed; shell continues``.
  A non-zero exit drops the hook's exports and logs a `hook_failed` entry.
- stdout lines of the form `NAME=VALUE` (`NAME` = `[A-Za-z_][A-Za-z0-9_]*`) are
  exported into the session env. Other lines are ignored. Credential-like names
  (containing `secret`, `password`, `token`, `key`, …) are **rejected**. Existing
  env wins over plugin exports; between plugins the alphabetically-first wins.
  Disable globally with `AISH_ENV_INJECTION_DISABLED=1`.
- Example: [examples/plugins/hello-world/hooks/on_init.sh](../examples/plugins/hello-world/hooks/on_init.sh).

**Event hooks** — a `hooks.json` in the plugin dir, merged into the shell's
33-event catalog (`SessionStart`, `PreToolUse`, `TurnEnd`, …) alongside
`~/.aish/hooks.json`:

```json
{ "hooks": [ { "name": "acme-banner", "event": "SessionStart",
  "action": { "type": "command", "program": "/bin/echo",
              "args": ["acme: session started"], "timeout_ms": 1000 } } ] }
```

Inspect with `:hooks list`. Event catalog, actions, and blocking semantics:
[internals/hooks-design.md](./internals/hooks-design.md).

---

## 6. Skills, skill sources, MCP servers, schemas

- **Skills** — `skills/<name>/SKILL.md` (front matter `name`, `description`) are
  merged into the same catalog as `~/.aish/skills`. Skills load even when the
  plugin's config is invalid. Skill changes apply on `:restart`.
- **Skill sources** — `provides.skill_source` (table above) and
  [plugins/skill-source-authoring.md](./plugins/skill-source-authoring.md).
- **MCP servers** — `.mcp.json` with `{ "mcpServers": { "<name>": { … } } }`.
  Servers are appended after the user's own config; on a name collision the
  first definition wins (`:plugin info <id> --mcp` shows winners/losers). MCP
  changes apply on `:restart`.
- **Schemas** — `schemas/<name>.json` JSON-Schema documents. Tool results that
  declare a schema are validated against them; `:plugin info <id> --schema`
  shows the shapes.

---

## 7. Timers and statusline segments

Use a **statusline** segment when you want text on the SecondStatusLine: core
owns cadence, cache and render — you only print one line. Use a **timer** for
any other periodic background work (refresh a cache file, poll an API). Both
start ~2 s after startup, run with CWD = plugin dir, and are re-armed on
`:plugin reload|enable|disable` (the old loops exit, so nothing runs twice).

---

## 8. Configuration: `config_schema` and `:plugin config`

Resolution pipeline (`src/plugins.rs::load_config`):

1. Read `~/.aish/plugins/<id>/config.json` (absent → `{}`; malformed → error).
2. Fill missing keys from `config_schema.properties.<key>.default`.
3. Expand every `${env:VAR}` (recursively). An **unset** variable is an error —
   fail-closed.
4. Validate `required` keys and each property's `type`.

Supported schema: `type: object`, `properties.<k>.{type, default}`, `required`;
`:plugin config --set` additionally validates `enum`, bounds, `pattern`, and
`additionalProperties`.

```text
:plugin config <id>                       # effective config, with provenance
:plugin config <id> --set <key> <value>   # value parsed as JSON, else string
:plugin config <id> --reset [key]         # drop one key, or all of config.json
```

- The view lists every key as `key = value  [file|default(, env)]`; values under
  credential-like keys (`token`, `secret`, `password`, `auth`, `key`, …) print as
  `<redacted>`; `${env:VAR}` values show `"${env:VAR}" → <resolved>`.
- `--set` validates the **whole resulting config** first and refuses invalid
  values (`✗ invalid config for `<id>`: … (config.json unchanged)`). Writes are
  atomic (`config.json.tmp` + rename).
- `--reset` always succeeds (warning if the result doesn't validate); resetting
  the last key deletes `config.json`.
- After a change, hooks, timers and webhooks pick it up immediately; MCP servers
  and skills on `:restart`.

If a plugin's config is invalid at load time the plugin is **warned and
skipped**, not disabled — see [§12](#12-errors-and-health).

---

## 9. Secrets and credentials

Never put a secret in `plugin.json`, `config.json`, `.mcp.json`, or a skill.
Use one of:

| Mechanism | Use for | How |
|---|---|---|
| `${env:VAR}` in `config.json` / `config_schema` defaults | Operator-provided tokens | Export `VAR` before starting aish; unset → config error (fail-closed). `:plugin config` redacts credential-like keys. |
| `aish login <name>` | Interactive OAuth / device-code | `provides.login` + `login.sh` prints a JSON credential on stdout; aish stores it in `~/.aish/credentials` (mode 0600) as `[profile:<name>]`. Your hooks get `AISH_PROFILE_<ID>_<FIELD>`; `.mcp.json` can reference the profile via `credentials`. |
| Plugin memory `auth` namespace | Tokens your plugin obtains at runtime | `memory/auth.json`, mode `0600`, always redacted in `:plugin memory` ([§10](#10-memory-and-state)). |
| Webhook secrets (operators) | Broker auth + payload signing | `WEBHOOK_BROKER_SECRET` and per-route HMAC secrets live on the broker/client env, never in a plugin — see [WEBHOOK_HANDLERS.md §7](./WEBHOOK_HANDLERS.md#7-signing-and-secrets). |

Lifecycle-hook stdout cannot leak a secret into the session env: credential-like
`KEY=VALUE` exports are rejected.

---

## 10. Memory and state

Two complementary stores:

| | Plugin memory | Plugin state |
|---|---|---|
| Where | `~/.aish/plugins/<id>/memory/<ns>.json` | `~/.aish/database/plugins.db` (SQLite) |
| Shape | closed namespaces, dot-notation keys | flat `(plugin_id, key) → JSON` |
| Use for | secrets, cache, webhook state, prefs | cheap scalars; `<id>:last_webhook_output` |
| Reference | [reference/plugins/memory.md](./reference/plugins/memory.md) | [reference/plugins/state.md](./reference/plugins/state.md) |

Memory namespaces:

| Namespace | File | Mode | Notes |
|---|---|---|---|
| `auth` | `auth.json` | `0600` | credentials; always redacted (`***`); perms auto-corrected |
| `cache` | `cache.json` | `0644` | rate limits, timestamps (TTL designed, not enforced) |
| `webhooks` | `webhooks.json` | `0644` | delivery/subscription state |
| `prefs` | `prefs.json` | `0644` | user-editable preferences |

```text
:plugin memory list
:plugin memory <id> <namespace>
:plugin memory <id> get|set|delete <namespace> <key> [value]
:plugin memory <id> clear <namespace> yes
```

---

## 11. Managing plugins: the `:plugin` command

| Command | What it does |
|---|---|
| `:plugin list` | All plugins (enabled and disabled) with a health marker ([§12](#12-errors-and-health)). Bare `:plugin` is an alias. |
| `:plugin info <id> [--schema\|--mcp]` | Full provenance: metadata, enabled source, login, hooks, MCP servers, schemas, skills. |
| `:plugin create <id>` | Scaffold a new plugin ([§1](#1-quick-start)). Alias `:plugin new`. |
| `:plugin add <id>` | Install from the registry; `:plugin reload <id>` then activates hooks/timers/webhooks. |
| `:plugin remove <id>` | Uninstall (alias `rm`); also forgets its enable/disable override. |
| `:plugin enable <id>` / `:plugin disable <id>` | Toggle; persisted to `~/.aish/plugins.state.json`; re-runs `on_init` on enable. |
| `:plugin reload [id]` | Re-read manifests and re-arm hooks, timers, statusline and webhook handlers; with `<id>` also re-run its `on_init`. |
| `:plugin config <id> …` | View/edit config ([§8](#8-configuration-config_schema-and-plugin-config)). |
| `:plugin errors <id> [N]` | Last N (default 20) error-log entries ([§12](#12-errors-and-health)). |
| `:plugin memory …` | Inspect/edit plugin memory ([§10](#10-memory-and-state)). |

**Enable/disable state.** `~/.aish/plugins.state.json` (a sibling of the
plugins dir, so reinstalling a plugin doesn't clobber it):

```json
{ "version": 1, "plugins": { "github": { "enabled": false } } }
```

Precedence: state file → manifest `enabled` → `true`. Disabled plugins are left
out of hooks, lifecycle env, timers, statusline, the Phase-1.6 dispatcher, and
the webhook handler registry immediately; their **MCP servers and skills change
on `:restart`**.

---

## 12. Errors and health

- **Invalid config → warn + skip.** If `load_config` fails, aish prints
  ``aish: plugin `<id>`: config invalid — <err>; skills stay loaded, hooks + webhook handlers skipped until fixed (:plugin errors <id>)``.
  Skills stay loaded; `on_init`, `hooks.json`, `webhook_url`/`webhook_command`
  and `webhooks[]` handlers are skipped. Fix the config and `:plugin reload`.
- **Hook timeouts** — 10 s, killed, warned (see [§5](#5-lifecycle-hooks-and-event-hooks)).
- **Error log** — `~/.aish/plugins/<id>/errors.jsonl`, newest 200 kept, one
  JSON object per line: `{"ts", "kind", "source", "message", "recovery"}`.

  | `kind` | Recorded when |
  |---|---|
  | `config_invalid` | config failed to resolve (deduplicated) |
  | `hook_timeout` / `hook_failed` | lifecycle hook overran / exited non-zero |
  | `webhook_failed` | `webhook_url` non-2xx/transport error, or `webhook_command` failure |
  | `handler_failed` / `handler_timeout` | a broker webhook handler failed / overran |

- `:plugin errors <id> [N]` prints `YYYY-MM-DD HH:MM:SSZ kind source: message → recovery`.
- **Health markers** in `:plugin list`, first match wins: `(disabled)`,
  `(config-invalid)`, `(N recent errors)` (last 24 h), else `(ok)`.
- **Broker reconnect backoff** — exponential from 0.5 s, doubling, jittered,
  capped at **300 s**.

---

## 13. Testing your plugin

1. **Manifest parses** — `:plugin info <id>`; `:plugin list` shows `(ok)`.
2. **Config** — `:plugin config <id>` shows no `✗ config does not validate`.
3. **Handlers offline** — handlers are plain programs:
   `WEBHOOK_EVENT_TYPE=ping ./handlers/ping.sh < fixture.json`.
4. **Handlers through aish** — `:webhook test` / `:webhook replay` and the
   delivery log ([WEBHOOK_HANDLERS.md §8](./WEBHOOK_HANDLERS.md#8-testing-logs-and-replay)).
5. **Rust tests** — mirror `crates/aish-webhook-client/tests/github_plugin_fixtures.rs`:
   load your plugin dir with `PluginRegistry::load_dir`, dispatch fixture payloads
   with `WebhookDispatcher`, assert on stdout/exit code.
6. **Errors** — `:plugin errors <id>` after exercising it.

---

## 14. Best practices

- Keep handlers and hooks **fast and idempotent**; offload long work to a
  detached process (see the GitHub plugin's PR-review agent).
- Read payloads from **stdin only**; never `eval` payload data. aish runs
  without a shell so injection via the manifest is inert — keep it that way in
  your scripts (quote variables, prefer `python3`/`jq` for JSON).
- Print **one short line** on stdout (it becomes the flash, ≤60 chars for
  Phase-1.6 commands); diagnostics go to **stderr**.
- Exit non-zero only for real failures — it is recorded in `errors.jsonl`.
- Declare every config key in `config_schema` with a `default` so
  `:plugin config` can show and validate it.
- Secrets via `${env:VAR}`, `aish login`, or the `auth` memory namespace — never
  committed files.
