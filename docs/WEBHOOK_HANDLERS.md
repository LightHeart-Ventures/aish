# Webhook Handler Guide

How to react to external events (GitHub pushes, CI results, your own services)
from an aish plugin. Architecture and operator setup:
[webhooks.md](./webhooks.md). Plugin basics: [PLUGIN_DEVELOPER.md](./PLUGIN_DEVELOPER.md).

> **Status (SPR-104).** Covers `main` plus the webhook work shipping in SPR-104:
> the client↔broker protocol (TASK-449), `:webhook logs|test|replay` and the
> JSONL delivery log (TASK-273), handler error logging (TASK-274), broker
> `/stats` (TASK-314) and metrics (TASK-375).

- [1. When to use a webhook handler](#1-when-to-use-a-webhook-handler)
- [2. Declaring handlers](#2-declaring-handlers)
- [3. Matching and filters](#3-matching-and-filters)
- [4. The handler contract](#4-the-handler-contract)
- [5. Timeouts and error isolation](#5-timeouts-and-error-isolation)
- [6. The envelope and the client↔broker protocol](#6-the-envelope-and-the-clientbroker-protocol)
- [7. Signing and secrets](#7-signing-and-secrets)
- [8. Testing, logs and replay](#8-testing-logs-and-replay)
- [9. Examples](#9-examples)
- [10. Troubleshooting](#10-troubleshooting)

---

## 1. When to use a webhook handler

| You want to… | Use |
|---|---|
| React when something happens **outside** aish (PR opened, CI failed, deploy finished) | **Webhook handler** (`webhooks[]`) — this guide |
| Do something periodically (poll an API, refresh a cache) | `provides.timers` |
| Show a live value on the statusline | `provides.statusline` |
| React to aish's own events (session start, tool use, turn end) | Event hooks (`hooks.json`) or `webhook_command` |

Handlers are push-driven and cost nothing while idle. They only run while an
aish session is connected to a broker; events that arrive while you're offline
are queued (up to 7 days / 1000 per route) and delivered on reconnect.

## 2. Declaring handlers

In `plugin.json`:

```json
{
  "id": "github",
  "webhooks": [
    { "event_type": "pull_request", "command": ["handlers/pr-review.sh"],
      "filters": { "action": "opened" }, "timeout_secs": 30 },
    { "event_type": "push", "command": ["handlers/push.sh"], "timeout_secs": 20 }
  ]
}
```

| Key | Type | Default | Meaning |
|---|---|---|---|
| `event_type` | string | **required** | Event to match, or `"*"` for every event. |
| `command` | string[] | **required** | argv, no shell. `command[0]` relative **and** containing `/` resolves against the plugin dir; a bare name uses `PATH`; absolute paths are used as-is. |
| `filters` | object | `{}` | Payload conditions — see §3. |
| `timeout_secs` | integer | `30` | Wall-clock limit; the handler is killed on overrun. |

The plugin-level `enabled` key and `:plugin disable <id>` both remove a plugin's
handlers from the registry. The legacy top-level `handlers` key is **rejected**
(the plugin's handlers are skipped with a warning to rename it to `webhooks`).
Changes to `webhooks[]` apply on `:plugin reload` or `:webhook reload`.

## 3. Matching and filters

- A handler runs when its `event_type` equals the event's type or is `"*"`.
- Every enabled plugin's handlers are considered; one event can run several
  handlers (concurrently).
- `filters` maps **dotted payload paths** to values; **all** must match (AND),
  using exact JSON equality. A missing path fails the filter.

  ```json
  "filters": { "action": "completed", "workflow_run.conclusion": "failure" }
  ```

- There is no OR / `in` operator: declare one entry per alternative (the GitHub
  plugin has one `issues` entry per action).
- A filtered-out handler is recorded as `skipped` in the delivery log.

## 4. The handler contract

| Channel | Content |
|---|---|
| **argv** | `command` exactly as declared (no shell expansion). |
| **stdin** | The event **payload** JSON (not the envelope), then EOF. |
| **env** | Inherited aish env plus `WEBHOOK_ID`, `WEBHOOK_TENANT_ID`, `WEBHOOK_PLUGIN_ID`, `WEBHOOK_EVENT_TYPE`. |
| **cwd** | aish's working directory — use paths relative to your script (`$(dirname "$0")`), not the cwd. |
| **stdout** | The first non-blank line becomes the SecondStatusLine **flash** (trimmed, ≤60 chars with `…`). Most-recent-wins. Printed even if you exit non-zero. |
| **stderr** | Captured into the delivery log record / `:webhook test --run` output. |
| **exit code** | `0` = ok. Non-zero = `error` in the delivery log and a `handler_failed` entry in `:plugin errors`. |

Minimal handler:

```sh
#!/bin/sh
payload=$(cat)
msg=$(printf '%s' "$payload" | python3 -c 'import json,sys; print(json.load(sys.stdin).get("message","hi"))')
echo "👋 $msg"
```

Rules of thumb: read stdin before doing anything else (a `python3 - <<EOF`
heredoc **replaces** stdin — capture the payload first); never `eval` payload
fields; keep it under a few seconds and hand longer work to a detached process.

## 5. Timeouts and error isolation

- Each handler has its own `timeout_secs` (default 30 s); on overrun it is
  killed, recorded as `error` (`timed out after …`) and as `handler_timeout` in
  `~/.aish/plugins/<id>/errors.jsonl`.
- Handlers are isolated: one failing or hanging handler doesn't affect others or
  the shell. The event is still acked — a failed handler is **not** retried
  automatically; use `:webhook replay` (§8).
- A plugin whose config is invalid has its handlers **skipped** until fixed
  (`:plugin errors <id>`, then `:plugin reload`).
- Lost broker connection: aish reconnects with exponential backoff (0.5 s
  doubling, jittered, capped at 300 s); an `auth_error` triggers a fresh
  registration.

## 6. The envelope and the client↔broker protocol

Envelope (broker → aish, WebSocket text frame):

```json
{ "type": "webhook", "id": "wh_…", "tenant_id": "default", "plugin_id": "github",
  "event_type": "pull_request", "payload": { … }, "received_at": "…" }
```

Connection sequence (TASK-449):

1. `POST <broker>/clients/register` with
   `{"tenant_id","plugin_id","session_id","transport":"websocket","secret"?}` →
   `201 {"client_id","session_token":"st_…","ws_path":"/ws",…}`. The register URL
   is derived from `WEBHOOK_BROKER_URL` (`wss://h/ws` → `https://h/clients/register`;
   a path prefix is kept).
2. Open the WebSocket at `WEBHOOK_BROKER_URL`; first frame
   `{"type":"auth","session_token":"st_…"}` (10 s deadline) →
   `{"type":"auth_ok","client_id"}` or `{"type":"auth_error","error":…}`.
3. Receive `webhook` frames; reply `{"type":"ack","webhook_id":"<id>"}` after
   dispatch. Answer `{"type":"ping"}` with `pong`.

`WEBHOOK_PLUGIN_ID` is **required**: the broker routes by (tenant, plugin), and
without it aish logs a warning and does not start the client. Wire details:
[CLIENT.md](../crates/aish-webhook-broker/docs/CLIENT.md); contract test:
`crates/aish-webhook-broker/tests/client_contract.rs`.

## 7. Signing and secrets

| Secret | Who holds it | Purpose |
|---|---|---|
| `WEBHOOK_BROKER_SECRET` | aish session env | Sent once at registration; from then on the broker requires a valid signature on inbound webhooks for that (tenant, plugin). Never sent over the WebSocket. |
| Sender's webhook secret (e.g. GitHub "Secret") | sender config | Must equal `WEBHOOK_BROKER_SECRET`. The sender signs the raw body: `X-Hub-Signature-256: sha256=<hex>` (GitHub) or `X-Signature: sha256=<hex>`. |
| `session_token` | aish memory | Authenticates the WebSocket; reissued on re-registration. |
| Handler credentials (`GITHUB_TOKEN`, …) | aish env / `gh auth` / plugin `auth` memory | Used by your handler, never by the broker. See [PLUGIN_DEVELOPER.md §9](./PLUGIN_DEVELOPER.md#9-secrets-and-credentials). |

Signature verification happens **on the broker**; handlers receive only
already-verified payloads. Sign a test request yourself:

```sh
body='{"message":"hi"}'
sig=$(printf '%s' "$body" | openssl dgst -sha256 -hmac "$WEBHOOK_BROKER_SECRET" | sed 's/^.* //')
curl -sS -X POST "https://<broker>/webhooks/default/hello-world" \
  -H 'Content-Type: application/json' -H 'X-Event-Type: ping' \
  -H "X-Signature: sha256=$sig" -d "$body"
```

## 8. Testing, logs and replay

**Offline** — a handler is a plain program:

```sh
WEBHOOK_EVENT_TYPE=push ./handlers/push.sh < tests/fixtures/push.json
```

**Through aish's dispatcher** (no broker needed):

```text
:webhook test <plugin> <event> [--payload file.json] [--run]
```

Default is a dry run listing matching handlers and whether filters pass;
`--run` executes them and prints exit code, duration, stdout/stderr. Without
`--payload`, built-in samples exist for `ping`, `push`, `pull_request`, `issues`
(marked `"aish_test": true`). Test runs are not logged.

**Delivery log** — every real delivery appends one record per plugin to
`~/.aish/state/webhooks/<plugin>.jsonl`:

```json
{"id":"wh_…","webhook_id":"wh_…","tenant_id":"default","plugin_id":"github",
 "event_type":"push","received_at_ms":1760000000000,"received_at":"2026-10-09T12:00:00Z",
 "payload":{…redacted…},
 "handlers":[{"name":"push.sh","status":"ok","exit_code":0,"duration_ms":41,"error":null}]}
```

Payload keys containing `secret`, `token`, `password`, `authorization`,
`api_key`, `signature`, `cookie`, `credential`, `private_key` (any case) are
written as `"***"`. `AISH_WEBHOOK_AUDIT_MAX` (default `1000`) caps records per
plugin; `0` disables the log.

```text
:webhook logs [N] [plugin] [--plugin id] [--event type]   # newest N (default 20)
:webhook replay [plugin] <id|last> [--run]                 # re-dispatch a logged delivery
:webhook status                                            # connection, queue, handler counters
:webhook reload                                            # reconnect + reload handlers
```

`replay` uses the redacted payload and is a dry run unless `--run`.

**End-to-end** — the hello-world ping: start aish with
`WEBHOOK_BROKER_URL=wss://<broker>/ws WEBHOOK_PLUGIN_ID=hello-world`, then run
`./test-webhook-statusline.sh "hi"` from the repo root (env `BROKER_HTTP`,
`TENANT`, `PLUGIN`, optional `WEBHOOK_BROKER_SECRET`). `202` = queued; the
statusline flashes `👋 hi`.

**Automated** — see `crates/aish-webhook-client/tests/github_plugin_fixtures.rs`
for loading a plugin dir with `PluginRegistry::load_dir` and dispatching fixture
payloads through `WebhookDispatcher`.

**Observability** (operators):

- `:webhook status` — connection, broker queue depth, and per-plugin handler
  counters (`handlers run: N ok failed timeout avg`, TASK-375).
- Broker `GET /stats` — per-(tenant, plugin) `received`, `delivered_ws`,
  `delivered_poll`, `dropped`, `expired`, `queued`, `acked`
  ([API.md](../crates/aish-webhook-broker/docs/API.md)).
- OpenTelemetry metrics `aish.webhook.broker.{received,delivered,dropped,expired,queued,acked}`
  over OTLP/HTTP when `OTEL_EXPORTER_OTLP_ENDPOINT` is set on the broker
  (`OTEL_EXPORTER_OTLP_HEADERS`, `OTEL_SERVICE_NAME`,
  `OTEL_METRIC_EXPORT_INTERVAL`); `LOG_FORMAT=json` for structured logs; SigNoz
  dashboard `crates/aish-webhook-broker/deploy/signoz/broker-dashboard.json`.
  Details: [webhooks.md → Monitoring & Observability](./webhooks.md#monitoring--observability).

## 9. Examples

**Issue triage flash** — only newly opened issues:

```json
{ "event_type": "issues", "command": ["handlers/issues.sh"], "filters": { "action": "opened" } }
```

**Auto-review a PR** — the GitHub plugin's `pr-review.sh` prints a summary
line, then (opt-in `GITHUB_PR_AUTOREVIEW=1`) launches a detached
`aish --coordinator` run once per head SHA. Pattern: print fast, fork the slow
work, guard with a marker file for idempotency. See
[plugins/github/README.md](../plugins/github/README.md).

**PR auto-merge on green CI** — match `workflow_run` with
`{"action":"completed","workflow_run.conclusion":"success"}` and call
`gh pr merge --auto` from the handler (your token, your policy).

**Catch-all audit** — `{"event_type":"*","command":["handlers/log.sh"]}`.

## 10. Troubleshooting

| Symptom | Likely cause / fix |
|---|---|
| `:webhook status` says `not configured` | `WEBHOOK_BROKER_URL` unset in the environment aish started with. |
| Client never starts; log says `WEBHOOK_PLUGIN_ID is not` set | Export `WEBHOOK_PLUGIN_ID` (the plugin route), restart. |
| Sender gets `404` | No aish session has registered that `<tenant>/<plugin>` — start aish first; check `WEBHOOK_TENANT_ID`. |
| Sender gets `401` | Signature missing/wrong: sender secret ≠ `WEBHOOK_BROKER_SECRET`, or the body was re-serialized before signing. |
| Delivered (`:webhook logs`) but no handler ran | `event_type` mismatch (check the sender's event header; non-GitHub senders should set `X-Event-Type`) or a filter failed — `:webhook test <plugin> <event> --payload f.json` shows which. |
| Event type shows as `opened` / `unknown` | No event header was sent; the broker fell back to `payload.action` / `"unknown"`. |
| Handler `error`, exit 127 | `command[0]` not found: relative paths need a `/` (`handlers/x.sh`), file must be executable. |
| Handler times out | Raise `timeout_secs` or background the slow part. |
| Payload is empty in the handler | Your script consumed stdin twice or used a heredoc for the interpreter — read stdin once, first. |
| Plugin's handlers ignored | Plugin disabled, config invalid (`:plugin list` marker, `:plugin errors <id>`), or manifest still uses `handlers`. |
| No flash | Handler printed nothing to stdout, or a later flash replaced it. |
| Events "missing" after downtime | Queue cap (`BROKER_MAX_QUEUE_SIZE`) or TTL hit — check `dropped`/`expired` in `GET /stats`. |
