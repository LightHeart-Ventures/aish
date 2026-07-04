# API Reference — aish Webhook Broker

Base URL: `http://<host>:8080` (default). All request/response bodies are JSON
unless noted. Errors are always `{"error": "<message>"}` with the status codes
listed under [Errors](#errors).

## Endpoint summary

| Method | Path | Auth | Success |
|--------|------|------|---------|
| `GET` | `/health` | none | `200` |
| `POST` | `/clients/register` | none | `201` |
| `POST` | `/webhooks/:tenant_id/:plugin_id` | HMAC (if secret set) | `202` |
| `GET` | `/webhooks/:tenant_id/:plugin_id/pending` | none¹ | `200` |
| `DELETE` | `/webhooks/:tenant_id/:plugin_id/messages/:webhook_id` | none¹ | `204` |
| `GET` | `/ws` | session token (first frame) | `101` |

¹ The poll/ack endpoints are unauthenticated in this release — put the broker
behind a network boundary or a reverse proxy that enforces auth. See
[DEPLOYMENT.md](DEPLOYMENT.md#hardening).

---

## `GET /health`

Liveness + at-a-glance state. Never fails (reports `db_health: "error"` if the
DB read throws).

```json
{
  "status": "ok",
  "uptime_secs": 3921,
  "connected_clients": 4,
  "queued_messages": 12,
  "db_health": "ok"
}
```

---

## `POST /clients/register`

Register an aish client for a `(tenant_id, plugin_id)` route and mint a session
token. **Registering also creates the route** — a webhook `POST` to a
tenant/plugin pair that has never been registered returns `404`.

Idempotent on `(tenant_id, plugin_id, session_id)`: re-registering the same
triple rotates and returns a fresh `session_token`.

Request:

```json
{
  "tenant_id": "acme",          // required
  "plugin_id": "github",        // required
  "session_id": "sess-1",       // required — stable per client session
  "transport": "websocket",     // optional: "websocket" (default) | "poll"
  "secret": "whsec_..."         // optional: HMAC secret for inbound verification
}
```

Response `201`:

```json
{
  "client_id": "cl_9f3c...",
  "session_token": "st_1a2b...",
  "ws_path": "/ws",
  "poll_path": "/webhooks/acme/github/pending",
  "transport": "websocket",
  "registered_at": "2026-07-04T12:00:00+00:00"
}
```

- `session_token` — use it to authenticate the WebSocket (`/ws`) connection.
- `secret` — once set for a `(tenant, plugin)`, **all** inbound webhooks to that
  pair must carry a valid signature. The most recently registered non-null
  secret wins. Omit to disable verification for that route.

Errors: `400` if any of `tenant_id` / `plugin_id` / `session_id` is empty.

---

## `POST /webhooks/:tenant_id/:plugin_id`

Ingest a webhook from an external producer (GitHub, Slack, GitLab, …). The raw
request body is stored verbatim as the `payload`.

Processing order:

1. **Route check** — unknown `(tenant, plugin)` → `404`.
2. **Signature** — if a secret is registered, the body's HMAC-SHA256 must match
   (see [Signature verification](#signature-verification)); missing/invalid → `401`.
3. **Event type** — resolved from the first present header
   `X-Event-Type`, `X-GitHub-Event`, `X-GitLab-Event`; else from the JSON body's
   `action`, then `event`; else `"unknown"`.
4. **Persist** — written to SQLite (durable), enforcing the per-route queue cap.
5. **Dispatch** — best-effort push to connected WebSocket clients + wake
   long-pollers.

Headers:

| Header | Purpose |
|--------|---------|
| `Content-Type: application/json` | Body is parsed as JSON (empty body → `{}`) |
| `X-Signature` or `X-Hub-Signature-256` | HMAC signature (hex, optional `sha256=` prefix) |
| `X-Event-Type` / `X-GitHub-Event` / `X-GitLab-Event` | Event type override |

Response `202`:

```json
{ "id": "wh_5e6f...", "status": "queued" }
```

Errors: `404` unknown route · `401` bad/missing signature · `400` malformed JSON
· `503` queue full (only when `max_queue_size` is 0).

---

## `GET /webhooks/:tenant_id/:plugin_id/pending`

Long-poll fallback for clients that can't hold a WebSocket. Returns queued
(undelivered) messages **without** removing them — you must ACK each one.

Query parameters:

| Param | Type | Default | Notes |
|-------|------|---------|-------|
| `limit` | int | `100` | Clamped to `1..=500` |
| `wait_secs` | int | `0` | If the queue is empty, park up to this long for a new message. Effective wait is capped by `poll_timeout_secs` |
| `session_id` | string | — | Accepted, currently unused |

With `wait_secs=0` the call returns immediately (returning `[]` if empty). With
`wait_secs>0` it blocks until a message arrives or the timeout elapses, then
re-reads.

Response `200`:

```json
{
  "messages": [
    {
      "type": "webhook",
      "id": "wh_5e6f...",
      "tenant_id": "acme",
      "plugin_id": "github",
      "event_type": "push",
      "payload": { "ref": "refs/heads/main" },
      "received_at": "2026-07-04T12:00:01+00:00"
    }
  ],
  "remaining_queue_size": 0
}
```

Poll loop: `GET …/pending?wait_secs=30` → process each message → `DELETE …` to
ACK → repeat.

---

## `DELETE /webhooks/:tenant_id/:plugin_id/messages/:webhook_id`

Acknowledge a message: marks it delivered so it stops appearing in polls and
won't be re-pushed on WebSocket reconnect. Delivery is **at-least-once** —
always ACK after you've durably handled the event.

- Response `204` — acknowledged.
- Response `404` — no matching undelivered message (already ACKed, expired, or
  wrong id). Safe to treat as success.

---

## `GET /ws` — WebSocket

Real-time push. Upgrade to WebSocket, then authenticate with the first frame.
All frames are JSON **text** frames.

Protocol:

| Direction | Frame |
|-----------|-------|
| client → server (first frame) | `{"type":"auth","session_token":"st_..."}` |
| server → client (on success) | `{"type":"auth_ok","client_id":"cl_..."}` |
| server → client (on failure) | `{"type":"auth_error","error":"authentication failed"}` then close |
| server → client (per webhook) | `{"type":"webhook","id":"wh_...","tenant_id":...,"plugin_id":...,"event_type":...,"payload":{...},"received_at":...}` |
| client → server (ack) | `{"type":"ack","webhook_id":"wh_..."}` |
| both | WebSocket control `ping`/`pong` |

Behavior:

- **Auth window** — the server waits up to 10 s for the opening `auth` frame; a
  non-auth first frame or an invalid token closes the connection.
- **Backlog drain** — on connect, up to 500 already-queued webhooks for the
  route are pushed immediately (so a reconnecting client catches up).
- **Heartbeat** — the server sends a WebSocket `Ping` every
  `ws_heartbeat_secs` (default 30 s); reply with `Pong` (browsers/most libs do
  this automatically). An app-level `{"type":"ping"}` is accepted and ignored.
- **ACK** — send `{"type":"ack","webhook_id":"..."}` after handling each
  webhook. Un-ACKed messages are re-pushed on the next reconnect.

Minimal client (pseudocode):

```
ws = connect("ws://host:8080/ws")
ws.send({"type":"auth","session_token": SESSION_TOKEN})
assert ws.recv().type == "auth_ok"
for msg in ws:
    if msg.type == "webhook":
        handle(msg.payload)
        ws.send({"type":"ack","webhook_id": msg.id})
```

---

## Signature verification

When a route has a registered `secret`, inbound webhooks must be signed:

- Algorithm: **HMAC-SHA256** over the **raw request body**.
- Encoding: lowercase hex digest.
- Header: `X-Signature` or `X-Hub-Signature-256`.
- The `sha256=` prefix is optional (GitHub sends it; both forms accepted).
- Comparison is constant-time.

Compute a signature (equivalent to the broker's `signature::sign`):

```bash
# body is the exact bytes you POST
BODY='{"hello":"world"}'
SIG=$(printf '%s' "$BODY" \
  | openssl dgst -sha256 -hmac "$SECRET" -r | cut -d' ' -f1)

curl -X POST http://host:8080/webhooks/acme/secure \
  -H 'content-type: application/json' \
  -H "x-signature: sha256=$SIG" \
  --data "$BODY"
```

```python
import hmac, hashlib
sig = hmac.new(secret.encode(), body_bytes, hashlib.sha256).hexdigest()
headers = {"X-Signature": f"sha256={sig}"}
```

GitHub-native producers work out of the box: point the webhook at
`/webhooks/:tenant/:plugin`, set the same secret on both sides, and GitHub's
`X-Hub-Signature-256` header verifies as-is.

---

## Message envelope

Every delivered message (WebSocket push **and** long-poll `messages[]`) shares
one shape:

| Field | Type | Description |
|-------|------|-------------|
| `type` | string | Always `"webhook"` |
| `id` | string | Broker id, `wh_<uuid>` — use for ACK |
| `tenant_id` | string | Route tenant |
| `plugin_id` | string | Route plugin |
| `event_type` | string | Resolved event (`push`, `opened`, …, or `unknown`) |
| `payload` | object | The original webhook JSON body, verbatim |
| `received_at` | string | RFC 3339 UTC timestamp |

---

## Errors

| Status | Condition | Body |
|--------|-----------|------|
| `400` | Malformed JSON / missing required register fields | `{"error":"..."}` |
| `401` | Missing or invalid signature; failed WS auth | `{"error":"invalid signature"}` |
| `404` | Unknown tenant/plugin route; ACK of unknown message | `{"error":"..."}` |
| `503` | Queue full (`max_queue_size` = 0) | `{"error":"queue full"}` |
| `500` | Database / internal error | `{"error":"..."}` |
| `502` | WebSocket transport error | `{"error":"..."}` |
