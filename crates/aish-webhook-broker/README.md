# aish Webhook Broker

Self-hosted webhook broker for the aish plugin system. Receives webhooks from
external services (GitHub, Slack, GitLab, …), verifies them, persists them to
SQLite, and routes them to connected aish clients in real time over WebSocket —
with an HTTP long-poll fallback for clients behind restrictive firewalls.

One broker serves an entire organization: webhooks are keyed by
`(tenant_id, plugin_id)` and only delivered to clients registered for that pair.

- **Crate:** `aish-webhook-broker` (standalone workspace — no parent `aish` build graph)
- **Binary:** `aish-webhook-broker` (~10 MB, statically-bundled SQLite)
- **Ships in:** SPR-059 — the broker (PR #515), deploy assets (#516,
  [docs/DEPLOYMENT.md](docs/DEPLOYMENT.md)), the GitHub reference plugin (#517,
  [docs/PLUGINS.md](docs/PLUGINS.md)), and the `aish-webhook-client` consumer
  (#518, [docs/CLIENT.md](docs/CLIENT.md))

## Documentation

| Doc | Contents |
|-----|----------|
| [docs/API.md](docs/API.md) | Full endpoint spec, WebSocket protocol, signature verification, envelope shape |
| [docs/CONFIGURATION.md](docs/CONFIGURATION.md) | Every CLI flag + env var, defaults, tuning guidance |
| [docs/DEPLOYMENT.md](docs/DEPLOYMENT.md) | Docker, systemd, AWS (EC2/ECS/Lambda), TLS, hardening; shipped `deploy/` assets (#516) |
| [docs/CLIENT.md](docs/CLIENT.md) | `aish-webhook-client` (#518): env-var config, reconnect/backoff, WS protocol, handler dispatch |
| [docs/PLUGINS.md](docs/PLUGINS.md) | Authoring webhook handlers; the GitHub reference plugin (#517) |

## Architecture

```text
 external service                      aish-webhook-broker                     aish clients
 (GitHub/Slack/…)                                                             (plugins)
                                ┌───────────────────────────────────┐
   POST /webhooks/:t/:p ─────►  │  http::receive_webhook             │
     (HMAC-signed body)         │    1. tenant/plugin known? (404)   │
                                │    2. verify HMAC-SHA256   (401)   │
                                │    3. resolve event_type           │
                                │    4. persist ──────────────┐      │
                                │                             ▼      │
                                │                        ┌─────────┐ │
                                │                        │ SQLite  │ │  durable
                                │                        │  (WAL)  │ │  source of truth
                                │                        └─────────┘ │
                                │    5. dispatch (best-effort) │     │
                                │             ▼                │     │
                                │        ┌──────────┐          │     │
                                │        │   Hub    │──push────┼───► GET /ws (WebSocket)
                                │        │ (in-mem) │          │     │   real-time
                                │        └──────────┘          │     │
                                │             │ notify         │     │
                                │             ▼                └───► GET /webhooks/:t/:p/pending
                                │        long-pollers                │   long-poll fallback
                                │                                    │
                                │  hourly TTL sweep ── purge expired │
                                └───────────────────────────────────┘
```

SQLite is the **durable source of truth**; the in-memory `Hub` is a best-effort
fast path. If no client is connected, the webhook simply waits in the DB queue
for the next poll or WebSocket reconnect to drain it. Nothing is lost on a
client disconnect or a broker restart.

## Features

- **WebSocket delivery** — real-time push to connected clients (`GET /ws`)
- **Long-poll fallback** — `GET /webhooks/:t/:p/pending?wait_secs=N`
- **Durable queue** — SQLite (WAL) survives restarts; per-tenant FIFO cap
- **Multi-tenant** — routed by `(tenant_id, plugin_id)`
- **HMAC-SHA256 verification** — constant-time, GitHub-compatible (`sha256=` prefix)
- **Message TTL** — expired webhooks swept hourly (default 7 days)
- **At-least-once** — messages held until explicitly ACKed
- **Graceful shutdown** — drains on SIGINT/SIGTERM

## Quick start

```bash
# Build (standalone crate — run from this directory)
cargo build --release
# → target/release/aish-webhook-broker

# Run
./target/release/aish-webhook-broker \
  --listen 0.0.0.0:8080 \
  --db /var/lib/aish-broker.db \
  --log-level info
```

Smoke test:

```bash
# 1. Register a client (creates the tenant/plugin route + session token)
curl -s -X POST localhost:8080/clients/register \
  -H 'content-type: application/json' \
  -d '{"tenant_id":"acme","plugin_id":"github","session_id":"sess-1"}'

# 2. Send a webhook
curl -s -X POST localhost:8080/webhooks/acme/github \
  -H 'content-type: application/json' -H 'x-event-type: push' \
  -d '{"ref":"refs/heads/main"}'

# 3. Drain the queue
curl -s 'localhost:8080/webhooks/acme/github/pending'

# 4. Health
curl -s localhost:8080/health
```

## Endpoint reference

| Method | Path | Purpose | Success |
|--------|------|---------|---------|
| `GET` | `/health` | Liveness + queue depth + client count | `200` |
| `POST` | `/clients/register` | Register a client, get a session token | `201` |
| `POST` | `/webhooks/:tenant_id/:plugin_id` | Ingest a webhook (external services) | `202` |
| `GET` | `/webhooks/:tenant_id/:plugin_id/pending` | Long-poll for queued messages | `200` |
| `DELETE` | `/webhooks/:tenant_id/:plugin_id/messages/:webhook_id` | ACK a delivered message | `204` |
| `GET` | `/ws` | WebSocket upgrade (auth by first frame) | `101` |

See [docs/API.md](docs/API.md) for request/response bodies, headers, error
codes, and the WebSocket protocol.

## Configuration

All flags have an environment-variable equivalent. Full table in
[docs/CONFIGURATION.md](docs/CONFIGURATION.md).

```
-l, --listen <ADDR>          BROKER_LISTEN            (default 0.0.0.0:8080)
-d, --db <PATH>              BROKER_DB                (default /var/lib/aish-broker.db)
    --max-queue-size <N>     BROKER_MAX_QUEUE_SIZE    (default 1000)
    --ws-heartbeat-secs <N>  BROKER_WS_HEARTBEAT_SECS (default 30)
    --poll-timeout-secs <N>  BROKER_POLL_TIMEOUT_SECS (default 60)
    --msg-ttl-secs <N>       BROKER_MSG_TTL_SECS      (default 604800 = 7 days)
    --log-level <LEVEL>      BROKER_LOG_LEVEL         (default info)
```

## Testing

```bash
cargo test          # unit tests (signature, queue, hub) + HTTP integration tests
```

Integration tests (`tests/integration.rs`) drive the real axum router in-process
against a temp-file SQLite DB — no network sockets required.

## License

Apache-2.0
