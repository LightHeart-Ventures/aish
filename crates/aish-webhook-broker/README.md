# aish Webhook Broker

Self-hosted webhook broker for the aish plugin system. Routes webhooks from external services (GitHub, Slack, etc.) to connected aish clients via WebSocket or long-poll.

## Features

- **WebSocket delivery** — Real-time webhook routing to connected clients
- **Long-poll fallback** — For clients behind restrictive firewalls
- **Message queuing** — Persists to SQLite when clients offline
- **Multi-tenant** — One broker serves entire organization
- **HMAC-SHA256 verification** — Prevents spoofed webhooks
- **Auto-reconnect** — Clients reconnect with exponential backoff

## Building

```bash
cargo build --release
```

Binary is at `target/release/aish-webhook-broker` (~10MB).

## Running

```bash
aish-webhook-broker --port 8080 --db /var/lib/aish-broker.db --log-level info
```

## Configuration

```
--listen <ADDR>              Listen address (default: 0.0.0.0:8080)
--db <PATH>                  SQLite database path (default: /var/lib/aish-broker.db)
--max-queue-size <N>         Max messages per tenant (default: 1000)
--ws-heartbeat-secs <N>      WebSocket heartbeat interval (default: 30)
--poll-timeout-secs <N>      Long-poll timeout (default: 60)
--msg-ttl-secs <N>           Message TTL in seconds (default: 604800 = 7 days)
--log-level <LEVEL>          Logging level (default: info)
```

## API

### Health Check

```bash
GET /health
```

Response:
```json
{
  "status": "ok",
  "uptime_secs": 12345,
  "connected_clients": 42,
  "queued_messages": 127,
  "db_health": "ok"
}
```

### Register Client

```bash
POST /clients/register
Content-Type: application/json

{
  "tenant_id": "t_abc123",
  "plugin_id": "github",
  "session_id": "s_def456",
  "transport": "websocket",
  "secret": "shared_secret_for_webhook_verification"
}
```

### Receive Webhook

```bash
POST /webhooks/{tenant_id}/{plugin_id}
X-Signature: sha256=abcd...
Content-Type: application/json

{
  "action": "opened",
  "pull_request": { ... }
}
```

### Poll for Messages

```bash
GET /webhooks/{tenant_id}/{plugin_id}/pending?session_id=s_def456&wait_secs=60
```

### ACK Webhook

```bash
DELETE /webhooks/{tenant_id}/{plugin_id}/messages/{webhook_id}
```

## Deployment

### Docker

```bash
docker build -f Dockerfile.broker -t aish-webhook-broker:latest .
docker run -p 8080:8080 -v broker_data:/var/lib/aish-broker aish-webhook-broker:latest
```

### Systemd

Copy `aish-webhook-broker.service` to `/etc/systemd/system/` and run:

```bash
sudo systemctl start aish-webhook-broker
sudo systemctl enable aish-webhook-broker
```

## Architecture

See `TASK-257` through `TASK-267` on the aish Atum board for full Phase 4 (webhook broker) design.

- Phase 4.1: Project scaffold (this crate)
- Phase 4.2: REST API endpoints
- Phase 4.3: WebSocket server
- Phase 4.4: Long-poll fallback
- Phase 4.5: Database schema
- Phase 4.6: Message queue
- Phase 4.7: Docker & systemd
- Phase 4.8–4.11: aish client integration & testing

## License

Apache-2.0
