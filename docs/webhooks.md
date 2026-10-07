# Webhook Architecture

aish receives events from external sources (GitHub, CI systems, Atum, anything
that can POST JSON) through a small relay — the **webhook broker** — and routes
each event to plugin **handlers** running inside your aish session. aish always
**dials out** to the broker, so it works unchanged behind NAT, on a laptop, or in
a container: no inbound port, tunnel, or public IP on the aish side.

> Writing a handler? See [WEBHOOK_HANDLERS.md](./WEBHOOK_HANDLERS.md).
> Writing a plugin? See [PLUGIN_DEVELOPER.md](./PLUGIN_DEVELOPER.md).
> Running the broker? See the broker crate docs linked under
> [Deployment](#deployment).
>
> The client↔broker protocol described here is the one shipping in SPR-104
> (TASK-449: register → `session_token` auth → `webhook_id` acks).

## High-Level Architecture

```text
 GitHub / CI / any sender
        │  POST /webhooks/<tenant_id>/<plugin_id>     (optional X-Signature HMAC)
        ▼
 ┌──────────────────────────────┐
 │ aish-webhook-broker          │  axum + SQLite (crates/aish-webhook-broker)
 │  • verifies HMAC per route   │  durable queue per (tenant, plugin), TTL 7 d
 │  • queues, delivers, acks    │  GET /health, GET /stats, OTLP metrics
 └──────────────┬───────────────┘
                │  WebSocket (or HTTP long-poll) — opened BY aish
                ▼
 ┌──────────────────────────────┐
 │ aish session                 │  src/webhook.rs + crates/aish-webhook-client
 │  • registers, authenticates  │
 │  • matches plugin webhooks[] │  ~/.aish/plugins/*/plugin.json
 │  • fork/execs handlers       │  payload JSON on stdin, WEBHOOK_* env
 │  • logs deliveries (JSONL)   │  ~/.aish/state/webhooks/<plugin>.jsonl
 └──────────────┬───────────────┘
                ▼
     first stdout line → SecondStatusLine flash
```

Delivery is **at-least-once**: the broker keeps each message in SQLite until the
client acks it (or it ages out after `BROKER_MSG_TTL_SECS`, default 7 days), and
replays the backlog when a client reconnects. Handlers should be idempotent;
the envelope `id` is stable across redeliveries.

## Event Flow

1. A sender POSTs JSON to `https://<broker>/webhooks/<tenant_id>/<plugin_id>`.
   The broker answers `404` if no client has registered that route, `401` if the
   route has a secret and the signature is missing/wrong, else
   `202 {"id": "...", "status": "queued"}`.
2. The event type is taken from the first header present — `X-Event-Type`,
   `X-GitHub-Event`, `X-GitLab-Event` — falling back to `payload.action`, then
   `payload.event`, then `"unknown"`. Set `X-Event-Type` explicitly for
   non-GitHub senders.
3. The broker pushes `{"type":"webhook","id","tenant_id","plugin_id","event_type","payload","received_at"}`
   to the connected client (or holds it for the next poll/reconnect).
4. aish dispatches the event to every handler whose `event_type` and `filters`
   match, records the outcome in the delivery log, and acks with
   `{"type":"ack","webhook_id":"..."}`.

Handler contract, matching rules, and troubleshooting:
[WEBHOOK_HANDLERS.md](./WEBHOOK_HANDLERS.md).

## Security

- **Outbound only.** aish opens the connection; nothing listens on the aish host.
- **Registration + session token.** aish registers at
  `POST /clients/register` and authenticates the WebSocket with the returned
  `session_token`; a bad token gets `auth_error`.
- **Per-route HMAC.** If aish registers with `WEBHOOK_BROKER_SECRET`, the broker
  requires `X-Signature` or `X-Hub-Signature-256` (`sha256=<hex HMAC-SHA256 of
  the raw body>`, constant-time compare) on every inbound webhook for that
  (tenant, plugin). Use the same value as the GitHub webhook secret.
- **No shell.** Handlers are fork/exec'd as argv; payload data only arrives on
  stdin.
- **Redacted logs.** The delivery log masks secret-looking keys (`token`,
  `secret`, `authorization`, `signature`, …) before writing.
- Terminate TLS in front of the broker (Fly, nginx, ALB) — see the broker's
  hardening notes.

## Configuration

### aish (client)

| Variable | Required | Default | Meaning |
|---|---|---|---|
| `WEBHOOK_BROKER_URL` | yes | — | `wss://host/ws` (or `ws://`). Unset → webhooks off, zero cost. |
| `WEBHOOK_PLUGIN_ID` | yes | — | Plugin route this session registers for. Missing → client not started (warning). |
| `WEBHOOK_TENANT_ID` | no | `default` | Tenant route segment. |
| `WEBHOOK_BROKER_SECRET` | no | — | Per-route HMAC secret, sent once at registration. |
| `WEBHOOK_CLIENT_ID` | no | generated | Stable client/session id. |
| `AISH_PLUGINS_DIR` | no | `~/.aish/plugins` | Where handler manifests are loaded from. |
| `AISH_WEBHOOK_AUDIT_MAX` | no | `1000` | Delivery-log records kept per plugin; `0` disables the log. |

### Broker

| Variable | Default | Meaning |
|---|---|---|
| `BROKER_LISTEN` | `0.0.0.0:8080` | Bind address |
| `BROKER_DB` | `/var/lib/aish-broker.db` | SQLite path (Fly: `/data/aish-broker.db` on a volume) |
| `BROKER_MAX_QUEUE_SIZE` | `1000` | Per-(tenant, plugin) queue cap; oldest evicted (counted as `dropped`) |
| `BROKER_WS_HEARTBEAT_SECS` | `30` | WebSocket ping interval |
| `BROKER_POLL_TIMEOUT_SECS` | `60` | Long-poll wait |
| `BROKER_MSG_TTL_SECS` | `604800` | Message TTL (7 days) |
| `BROKER_LOG_LEVEL` | `info` | Log filter |

Full reference: [CONFIGURATION.md](../crates/aish-webhook-broker/docs/CONFIGURATION.md).

## Deployment

The broker is a single static binary with an embedded SQLite file; run one
instance per database.

- Docker, systemd, AWS, nginx, hardening, and Fly.io (persistent volume at
  `/data`): [DEPLOYMENT.md](../crates/aish-webhook-broker/docs/DEPLOYMENT.md)
- HTTP/WebSocket API, `/health`, `/stats`: [API.md](../crates/aish-webhook-broker/docs/API.md)
- Client protocol: [CLIENT.md](../crates/aish-webhook-broker/docs/CLIENT.md)
- Load bench (`tests/load_bench.rs`, ignored by default; `BENCH_N`,
  `BENCH_CONC`, `BENCH_RATE`) — TASK-371

- Metrics: `GET /stats`, plus OTLP metrics and a SigNoz dashboard when
  `OTEL_EXPORTER_OTLP_ENDPOINT` is set — see
  [Monitoring & Observability](#monitoring--observability) (TASK-375)

Hosted `aish.sh` broker: not yet available (TASK-270, deferred).

### Operator checklist

- [ ] Broker reachable over TLS; `GET /health` returns `"status"`.
- [ ] `BROKER_DB` on persistent storage.
- [ ] Each aish session has `WEBHOOK_BROKER_URL` + `WEBHOOK_PLUGIN_ID`
      (+ `WEBHOOK_TENANT_ID`, `WEBHOOK_BROKER_SECRET` if signing).
- [ ] Sender (e.g. GitHub) points at `https://<broker>/webhooks/<tenant>/<plugin>`
      with the same secret.
- [ ] `:webhook status` shows `connected`; `:webhook logs` shows deliveries.

## Monitoring & Observability

The webhook broker (`crates/aish-webhook-broker`) uses **SigNoz / OpenTelemetry**
for observability. It does not export Prometheus metrics. Everything below is
implemented today (TASK-314, TASK-375).

### Broker metrics (OTLP)

Set `OTEL_EXPORTER_OTLP_ENDPOINT` (for example `http://otel-collector:4318`) and
the broker pushes OpenTelemetry metrics over OTLP HTTP/protobuf. When it is
unset, the broker builds no exporter, so the default costs nothing. SigNoz Cloud
also needs `OTEL_EXPORTER_OTLP_HEADERS=signoz-ingestion-key=<key>`. The broker
also honours `OTEL_SERVICE_NAME` (default `aish-webhook-broker`) and
`OTEL_METRIC_EXPORT_INTERVAL` (in ms, default 60000).

| Metric | Type | Attributes | Meaning |
|---|---|---|---|
| `aish.webhook.broker.received` | counter | tenant_id, plugin_id | Webhooks accepted (HTTP 202) |
| `aish.webhook.broker.delivered` | counter | + `transport` (ws\|poll) | Envelopes delivered to clients |
| `aish.webhook.broker.dropped` | counter | tenant_id, plugin_id | Undelivered webhooks evicted by the queue cap |
| `aish.webhook.broker.expired` | counter | tenant_id, plugin_id | Undelivered webhooks removed by the TTL sweep |
| `aish.webhook.broker.queued` | gauge | tenant_id, plugin_id | Undelivered webhooks queued now (durable) |
| `aish.webhook.broker.acked` | gauge | tenant_id, plugin_id | Acked webhooks kept until their TTL |

`GET /stats` returns the same numbers as JSON. The metrics never include
payloads, secrets, client ids or tokens. The broker does not record
signature-failure, duplicate-event or delivery-latency histogram metrics yet.
Queue depth works as the latency proxy.

### Dashboard

Import `crates/aish-webhook-broker/deploy/signoz/broker-dashboard.json` into
SigNoz (Dashboards → Import JSON). It shows ingress and delivery rates, losses
(dropped/expired), queue depth and a per-plugin health table. You can filter it
by `tenant_id` and `plugin_id`. See
[`deploy/signoz/README.md`](../crates/aish-webhook-broker/deploy/signoz/README.md).

### Logs

`LOG_FORMAT=json` switches broker logs to one JSON object per line, with
`timestamp`, `level`, `target` and `fields.*`. The SigNoz/OTel collector log
pipelines parse this format directly. `BROKER_LOG_LEVEL` sets the filter. The
default `text` format is meant for humans.

### Client-side handler health

In aish, `:webhook status` shows the handler counters for the connected client:

```
handlers run: 12  ok: 10  failed: 1  timeout: 1  avg: 34ms
plugins: github 7/8 ✗1, slack 3/4 ⏱1
```

The counters are kept per plugin. "dispatched" counts handlers that were
actually executed, so handlers removed by filters are not counted. A timeout is
a handler killed after its `timeout_secs`. `:webhook logs` shows the individual
audit records, each with `duration_ms`.

### Load testing

`crates/aish-webhook-broker/scripts/loadgen.py` sends HMAC-SHA256-signed POSTs
at a fixed rate. It uses only the Python standard library. The run shows up in
`/stats` and on the dashboard.

```sh
python3 crates/aish-webhook-broker/scripts/loadgen.py --url http://localhost:8080 \
  --tenant acme --plugin github --secret s3cret --register --rate 50 --duration 30
```

### Alerting (SigNoz alerts on the metrics above)

| Alert | Condition | Action |
|-------|-----------|--------|
| Data loss (queue cap) | `aish.webhook.broker.dropped` increase > 0 / 5m | Consumer offline: check client connectivity, raise `BROKER_MAX_QUEUE_SIZE` |
| Data loss (TTL) | `aish.webhook.broker.expired` increase > 0 | Webhooks aged out undelivered |
| Backlog | `aish.webhook.broker.queued` > 80% of `BROKER_MAX_QUEUE_SIZE` for 10m | Consumer slow or disconnected |
| Ingress stopped | `aish.webhook.broker.received` rate == 0 for 1h (when traffic is expected) | Check source webhook config / tunnel |

## Related Docs

- [WEBHOOK_HANDLERS.md](./WEBHOOK_HANDLERS.md) — handler contract, testing, troubleshooting
- [PLUGIN_DEVELOPER.md](./PLUGIN_DEVELOPER.md) — plugin manifest and lifecycle
- [reference/plugins/webhook-events.md](./reference/plugins/webhook-events.md) — shell lifecycle events (`webhook_url` / `webhook_command`)
- [design/webhook-plugin-routing.md](./design/webhook-plugin-routing.md) — routing design
- [ARCHITECTURE.md](./ARCHITECTURE.md) — aish architecture
