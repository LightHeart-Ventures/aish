# Plugin Webhook Events (Phase 1.6)

The webhook dispatcher (`src/plugin_dispatcher.rs`) routes aish lifecycle events
to plugins that opt in through their `plugin.json` manifest. It is the outbound
half of the plugin event system — events flow **from the shell to plugins**. No
plugin can yet mutate shell state through a hook; inbound/mutation hooks are
Phase 2+.

Delivery is **fire-and-forget and non-blocking**: routing an event does a cheap
manifest scan, spawns one detached `tokio` task per subscriber, and returns
immediately. A slow HTTP endpoint or a hung command can never stall the REPL.

---

## Opting in — manifest fields

Add either (or both) of these fields to a plugin's `plugin.json`:

| Field             | Type   | Effect                                                                 |
|-------------------|--------|-----------------------------------------------------------------------|
| `webhook_url`     | string | Each event is HTTP `POST`ed to this URL with the event as a JSON body. |
| `webhook_command` | string | Each event runs this command via `sh -c`, event JSON piped on stdin.   |

A plugin with neither field is never contacted. A plugin with `"enabled": false`
is skipped entirely. Example:

```json
{
  "id": "gh-notifier",
  "webhook_url": "https://hooks.example.com/aish",
  "webhook_command": "logger -t aish \"$AISH_EVENT_TYPE\""
}
```

Plugins live under `~/.aish/plugins/<id>/plugin.json`. Manifests that are missing
or unparseable are skipped silently — a broken plugin never blocks dispatch.

---

## Event types

The stable wire name (snake_case) is what appears in the payload's `event_type`
field, on the `plugin-events` log channel, and in the `AISH_EVENT_TYPE` env var.

| Variant                  | Wire name                  | Fired when                                   |
|--------------------------|----------------------------|----------------------------------------------|
| `WorkspaceOpen`          | `workspace_open`           | Shell finishes init, just before the REPL.   |
| `SkillLoaded`            | `skill_loaded`             | A skill is loaded into the catalog.          |
| `BackgroundJobStart`     | `background_job_start`     | A background job starts.                      |
| `BackgroundJobComplete`  | `background_job_complete`  | A background job finishes.                    |
| `ToolInvoked`            | `tool_invoked`             | A tool is invoked.                            |

> Phase 1.6 wires `workspace_open` at startup. The remaining variants are defined
> and deliverable; their hook sites land incrementally in later Phase 1.6+ work.

---

## Payload schema

The POST body / stdin JSON is the serialized `PluginEvent`:

```json
{
  "plugin_id": "gh-notifier",
  "event_type": "workspace_open",
  "timestamp": 1717000000,
  "payload_json": { "cwd": "/home/you/project" }
}
```

| Field          | Type    | Notes                                                        |
|----------------|---------|-------------------------------------------------------------|
| `plugin_id`    | string  | The receiving plugin's id.                                   |
| `event_type`   | string  | Stable snake_case wire name (see table above).               |
| `timestamp`    | integer | Unix epoch **seconds** when the event was routed.            |
| `payload_json` | object  | Event-specific data; may be `{}`.                            |

Per-event `payload_json`:

| Event            | `payload_json` fields         |
|------------------|-------------------------------|
| `workspace_open` | `cwd` — current working dir.  |
| *(others)*       | `{}` (extended as hooks land) |

### Command delivery specifics

For `webhook_command`, in addition to the JSON on stdin the child process gets:

- `AISH_EVENT_TYPE` — the event wire name.
- `AISH_PLUGIN_ID`  — the receiving plugin's id.

The command's result is captured to the **plugin state store** under the key
`<plugin_id>:last_webhook_output`:

```json
{
  "exit_code": 0,
  "stdout": "...",
  "stderr": "...",
  "event": "skill_loaded",
  "at": 1717000000
}
```

`exit_code` is `null` if the process was killed by a signal.

---

## Error handling

Failures are logged and swallowed — a bad plugin never crashes the shell or the
spawning task.

| Failure                     | Behavior                                                                    |
|-----------------------------|-----------------------------------------------------------------------------|
| HTTP endpoint down / errors | Logged as `... FAILED: <err>`; no retry in Phase 1.6.                        |
| HTTP slow                   | Bounded by a **10s** per-request timeout, then treated as a failure.        |
| Non-2xx HTTP response       | Logged with the status code; **not** treated as fatal (delivery completed). |
| Command spawn fails         | Logged; no state write.                                                     |
| Command non-zero exit       | Captured normally — `exit_code` is recorded, not treated as an error.       |
| Manifest missing/malformed  | Plugin skipped silently.                                                     |

**Deferred to Phase 2:** retry/backoff policy, per-plugin timeout override,
circuit-breaking a repeatedly-failing endpoint, signed payloads.

---

## Observability

All dispatch and delivery activity is emitted on a dedicated **`plugin-events`**
log channel. It is **quiet by default**; enable it by setting:

```
AISH_PLUGIN_EVENTS=1
```

Lines are prefixed `[plugin-events]` and cover: the dispatch fan-out count, each
HTTP delivery + status, each command delivery + exit code, and every failure.
The channel is a single sink (`log_channel`) so it is easy to redirect to a file
or structured logger later.

---

## Design note: `reqwest` vs `hyper`

Phase 1.6 uses **`reqwest`** (0.13, `json` feature) for HTTP dispatch.

`hyper` is lighter, but `reqwest` is already a transitive dependency of the
crate's HTTP stack and gives us connection pooling, TLS, timeouts, and JSON
bodies in a few lines — versus hand-rolling all of that on `hyper`. For a
fire-and-forget POST with a bounded timeout, the ergonomics win decisively and
the extra weight is already paid for. Revisit only if the dependency footprint
becomes a release-size concern.
