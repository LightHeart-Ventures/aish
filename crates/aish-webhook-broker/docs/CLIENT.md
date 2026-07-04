# Client — aish-webhook-client

The consumer side of SPR-059. `aish-webhook-client` (PR #518) is the library aish
embeds to connect to a running [broker](../README.md), authenticate for a
`(tenant_id, plugin_id)` route, receive webhooks in real time over WebSocket,
acknowledge them, and fork/exec plugin **handlers** to process each event.

- **Crate:** `aish-webhook-client` (standalone workspace)
- **Ships in:** PR #518 / SPR-059 (Phase 5)
- **Pairs with:** the broker (#515), the deploy assets (#516), and the GitHub
  reference plugin (#517, see [PLUGINS.md](PLUGINS.md))

```text
   broker  ──WS──►  Client ──►  ConnectionManager ──►  WebhookDispatcher ──► handlers
  (wss://…/ws)      (auth,         (reconnect +           (match event →       (fork/exec,
                     ack)           backoff)               filters → run)       stdin+env)
```

## Configuration — `broker.json`

Loaded from `~/.aish/config/broker.json` via `BrokerConfig::load()`.

```json
{
  "broker_url": "wss://webhook-broker.example.com/ws",
  "tenant_id": "acme",
  "plugin": "github",
  "transport": "websocket",
  "enabled": true,
  "secret": "shared-secret-echoed-in-auth",
  "client_id": "workstation-1"
}
```

| Field | Req | Default | Meaning |
|-------|-----|---------|---------|
| `broker_url` | ✅ | — | Broker WebSocket URL (`ws://` or `wss://`). |
| `tenant_id` | ✅ | — | Tenant this client authenticates as. |
| `plugin` | | `null` | Optional plugin scope; broker may fan out per-plugin. |
| `transport` | | `websocket` | Transport hint; only `websocket` is implemented. |
| `enabled` | | `true` | Master switch — aish skips broker init when `false`. |
| `secret` | | `null` | Shared secret echoed in the auth frame. |
| `client_id` | | generated | Stable client id; auto-generated when absent. |

A missing file or `enabled: false` is treated as a soft "no broker" no-op — aish
starts normally without a broker connection.

## Connection lifecycle

The `ConnectionManager` owns a single logical session and keeps it alive:

1. **Dial** the `broker_url` (real socket only under the `net` feature; see below).
2. **Auth** — send a `ClientFrame::Auth { tenant_id, client_id, plugin?, secret? }`.
3. **Await** `AuthOk { session_token?, client_id? }`; adopt any broker-assigned id.
4. **Serve** — read frames, dispatch webhooks, ack them, answer heartbeats.
5. **Reconnect** on drop with exponential backoff, then resume from the queue.

Because the broker's SQLite queue is the durable source of truth and delivery is
at-least-once, a dropped/reconnected client loses nothing — undelivered messages
are redelivered after re-auth.

### Backoff

Reconnects use capped exponential backoff (`backoff.rs`): the delay doubles per
consecutive failure up to a ceiling, and resets to the floor after a successful
connect. This prevents a reconnect storm against a broker that is briefly down.

## WebSocket protocol

Text frames, JSON-encoded. The client tolerates both tagged control frames and
bare (untyped) webhook envelopes.

**Client → broker** (`ClientFrame`):

| `type` | Fields | When |
|--------|--------|------|
| `auth` | `tenant_id`, `client_id`, `plugin?`, `secret?` | On connect. |
| `ack` | `id` | After a webhook's handlers have been dispatched. |
| `pong` | — | In reply to a broker `ping`. |

**Broker → client** (`ServerFrame`, sniffed):

| Shape | Parsed as | Action |
|-------|-----------|--------|
| `{"type":"webhook"\|"event", …}` or untyped `{id, event_type, …}` | `Webhook` | dispatch → ack |
| `{"type":"auth_ok"\|"registered"\|"ack", session_token?, client_id?}` | `AuthOk` | adopt session |
| `{"type":"ping"}` | `Ping` | reply `pong` |
| anything else / `{"type":"pong"}` | `Other` | ignore |

### Webhook envelope

```json
{
  "id": "delivery-uuid",
  "tenant_id": "acme",
  "plugin_id": "github",
  "event_type": "pull_request",
  "payload": { "action": "opened", "...": "raw provider body" }
}
```

`id` is echoed back in the `ack`. `event_type` selects handlers; `payload` is the
raw provider JSON, passed to handlers unchanged.

## Handler dispatch contract

For each webhook, the `WebhookDispatcher` finds every handler subscribed to the
event across all loaded plugins, applies each handler's filters, and fork/exec's
the survivors **concurrently with full failure isolation** — one handler
panicking, failing, or timing out never blocks another.

Handlers are declared in a plugin's `plugin.json`:

```json
{
  "id": "github",
  "name": "GitHub",
  "version": "1.0.0",
  "webhooks": [
    {
      "event_type": "pull_request",
      "command": ["handlers/pull_request.sh"],
      "filters": { "action": "opened", "pull_request.base.ref": "main" },
      "timeout_secs": 20
    }
  ]
}
```

| Field | Req | Default | Meaning |
|-------|-----|---------|---------|
| `event_type` | ✅ | — | Event to subscribe to. `"*"` matches every event. |
| `command` | ✅ | — | argv to fork/exec. `command[0]` is the program; no shell is involved. |
| `filters` | | `{}` | AND-combined **equality** checks over dotted payload paths (e.g. `pull_request.base.ref`). All must match or the handler is skipped. |
| `timeout_secs` | | `30` | Per-handler wall-clock timeout; the child is killed on expiry. |

> `webhooks` also accepts the legacy alias `handlers`. The manifest loader scans
> `<root>/*/plugin.json`; a malformed manifest is skipped with a warning rather
> than sinking the whole registry.

### What a handler receives

- **stdin:** the raw webhook `payload` JSON (a single write, then EOF).
- **environment:**

  | Var | Value |
  |-----|-------|
  | `WEBHOOK_ID` | delivery id (same as the envelope `id`) |
  | `WEBHOOK_TENANT_ID` | tenant id |
  | `WEBHOOK_PLUGIN_ID` | plugin id that owns the handler |
  | `WEBHOOK_EVENT_TYPE` | the event type |

- **exit code:** `0` = success. Non-zero, a spawn failure, or a timeout is
  recorded (stdout/stderr captured, `duration_ms` measured) and logged, but does
  not affect sibling handlers or the ack.

Minimal handler:

```bash
#!/usr/bin/env bash
set -euo pipefail
payload="$(cat)"                       # raw JSON on stdin
echo "event=$WEBHOOK_EVENT_TYPE id=$WEBHOOK_ID"
action="$(jq -r '.action // empty' <<<"$payload")"
# … do work …
```

See [PLUGINS.md](PLUGINS.md) and `examples/plugins/github/` (#517) for a complete
reference plugin.

## Features / build

```bash
# From crates/aish-webhook-client/
cargo test                       # hermetic: MockTransport, no sockets
cargo build --release --features net   # real WebSocket (tokio-tungstenite)
```

The connection manager and message loop are written against a `Transport` trait,
so the full client is exercised end-to-end by an in-memory `MockTransport` with
no network. The real `wss://`/`ws://` transport (`TungsteniteTransport`) compiles
only under the `net` feature.
