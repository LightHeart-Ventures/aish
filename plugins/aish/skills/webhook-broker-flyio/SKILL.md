---
name: webhook-broker-flyio
description: Deploy and configure a webhook broker on Fly.io — manage webhooks, event routing, retry logic, and observability for serverless event distribution.
categories: [infrastructure, deployment, webhooks, observability]
applies-to: [aish, all]
unwanted-for: [design, ui]
allowed-tools: Read, Write, Edit, Bash, Git, Deploy
license: Proprietary
version: 1.0.0
tags: webhook-broker, fly.io, event-driven, serverless, observability, deployment
---

# Webhook Broker on Fly.io

Deploy and operate a **webhook broker** — a serverless event distributor that accepts incoming webhooks, queues them, and forwards to multiple subscribers with automatic retries, exponential backoff, and full observability.

Use this skill when:
- **Setting up a new webhook broker** on Fly.io from scratch
- **Configuring webhook routing** and subscriber management
- **Implementing retry logic** and dead-letter queues
- **Monitoring broker health** and event throughput
- **Troubleshooting failed deliveries** or stuck queues
- **Scaling the broker** across regions or deployment tiers

---

## § 0 — Quick Start: Deploy in 5 Minutes

### Prerequisites
- **Fly.io account** with flyctl installed: https://fly.io
- **GitHub repo** with code (can clone the webhook-broker template)
- **Node.js 18+** or **Rust 1.75+** (pick your runtime)

### 1. Clone or create the broker scaffold

**Option A: Use an existing template (fastest)**

```bash
# If a template exists in the aish org, clone it:
git clone https://github.com/LightHeart-Ventures/webhook-broker-flyio.git
cd webhook-broker-flyio
```

**Option B: Scaffold from scratch**

Create a new directory and initialize:

```bash
mkdir webhook-broker-flyio
cd webhook-broker-flyio
git init
```

Then create `Dockerfile`, `fly.toml`, and app code (see § 1 — Architecture).

### 2. Create a Fly.io app

```bash
flyctl launch
# Follow prompts:
#   - App name: "webhook-broker" (or your-org-webhook-broker)
#   - Region: select primary region (iad for US-East recommended)
#   - Keep defaults for PostgreSQL? (yes if you want persistent storage)
```

This creates `fly.toml` and sets up secrets.

### 3. Set environment variables

```bash
flyctl secrets set \
  LOG_LEVEL=info \
  WEBHOOK_SIGNING_KEY="$(openssl rand -hex 32)"
```

### 4. Deploy

```bash
flyctl deploy
```

Check status:

```bash
flyctl status
flyctl logs
```

Your broker is live at `https://<app-name>.fly.dev`.

---

## § 1 — Architecture Overview

### Components

```
┌─────────────────────────────────────────────────────────────────┐
│ Webhook Broker (Fly.io App)                                     │
├──────────────┬──────────────────┬──────────────────────────────┤
│              │                  │                              │
│ HTTP API     │ Event Queue      │ Subscriber Manager          │
│ (Accept)     │ (In-Memory or    │ (Route events to targets)   │
│              │  PostgreSQL)     │                             │
│              │                  │                             │
└──────────────┴──────────────────┴──────────────────────────────┘
       ▲               │                    │
       │               │                    ▼
       │          [Process Queue]      ┌────────────────┐
       │          - Retry logic        │ Target         │
       │          - Backoff            │ Webhooks       │
       │          - DLQ                │ (POST /notify) │
       │                               └────────────────┘
       │
    ┌──────────────┐
    │ Event Source │
    │ (GitHub API, │
    │  SaaS, etc)  │
    └──────────────┘

```

### Flow

1. **Event Ingestion**: POST `/webhooks` → JSON payload + signature
2. **Validation**: Check HMAC signature, schema
3. **Queueing**: Store event in queue (in-memory for dev, PostgreSQL for prod)
4. **Routing**: Look up subscribers for this event type
5. **Delivery**: POST to each subscriber with retries
6. **Observability**: Log all events, deliveries, failures to stdout/observability platform

---

## § 2 — Configuration & Setup

### 2.1 Environment Variables

| Variable | Purpose | Default | Example |
|----------|---------|---------|---------|
| `PORT` | HTTP server port | `3000` | `3000` |
| `LOG_LEVEL` | Logging verbosity | `info` | `debug`, `info`, `warn`, `error` |
| `WEBHOOK_SIGNING_KEY` | HMAC key for signature verification | (required) | `hex-encoded-32-bytes` |
| `DATABASE_URL` | PostgreSQL connection (optional) | unset | `postgres://user:pass@host/db` |
| `QUEUE_TYPE` | In-memory or PostgreSQL | `memory` | `memory`, `postgres` |
| `RETRY_MAX_ATTEMPTS` | Max retries per delivery | `5` | `3`–`10` |
| `RETRY_BACKOFF_MS` | Initial backoff (exponential) | `1000` | `500`–`5000` |
| `DLQ_ENABLED` | Dead-letter queue for failed events | `true` | `true`, `false` |
| `OBSERVABILITY_ENABLED` | Send metrics/logs to external service | `false` | `true`, `false` |
| `OTEL_EXPORTER_OTLP_ENDPOINT` | OpenTelemetry collector endpoint | unset | `http://localhost:4317` |

### 2.2 Setting Variables in Fly.io

**One-time setup** (creates/updates secrets):

```bash
flyctl secrets set \
  LOG_LEVEL=info \
  WEBHOOK_SIGNING_KEY="$(openssl rand -hex 32)" \
  RETRY_MAX_ATTEMPTS=5 \
  RETRY_BACKOFF_MS=1000 \
  DLQ_ENABLED=true
```

**Verify secrets are set:**

```bash
flyctl secrets list
```

**Update a single secret:**

```bash
flyctl secrets set LOG_LEVEL=debug
```

**Deploy after config change:**

```bash
flyctl deploy
```

---

## § 3 — API Reference

### 3.1 Register a Webhook

**POST** `/webhooks/register`

Register your app to receive events of a specific type.

**Request:**

```json
{
  "webhook_url": "https://your-app.example.com/notify",
  "event_type": "order.created",
  "retry_policy": {
    "max_attempts": 5,
    "backoff_ms": 1000
  },
  "active": true
}
```

**Response:**

```json
{
  "id": "sub_abc123",
  "webhook_url": "https://your-app.example.com/notify",
  "event_type": "order.created",
  "created_at": "2024-01-15T10:30:00Z",
  "status": "active"
}
```

### 3.2 Send a Webhook

**POST** `/webhooks/send`

Trigger a webhook delivery to all subscribers of an event type.

**Request:**

```json
{
  "event_type": "order.created",
  "event_id": "evt_abc123",
  "timestamp": "2024-01-15T10:30:00Z",
  "data": {
    "order_id": "12345",
    "customer": "Alice",
    "total": 99.99
  }
}
```

**Response:**

```json
{
  "event_id": "evt_abc123",
  "queued_at": "2024-01-15T10:30:00Z",
  "subscribers_count": 2,
  "status": "queued"
}
```

### 3.3 List Subscribers

**GET** `/webhooks/subscribers?event_type=order.created`

Retrieve all subscribers for a given event type.

**Response:**

```json
{
  "subscribers": [
    {
      "id": "sub_abc123",
      "webhook_url": "https://app1.example.com/notify",
      "event_type": "order.created",
      "active": true,
      "created_at": "2024-01-15T10:00:00Z"
    },
    {
      "id": "sub_def456",
      "webhook_url": "https://app2.example.com/notify",
      "event_type": "order.created",
      "active": true,
      "created_at": "2024-01-15T10:15:00Z"
    }
  ]
}
```

### 3.4 Get Delivery Status

**GET** `/webhooks/deliveries/:event_id`

Check delivery status for a specific event.

**Response:**

```json
{
  "event_id": "evt_abc123",
  "event_type": "order.created",
  "sent_at": "2024-01-15T10:30:00Z",
  "deliveries": [
    {
      "subscriber_id": "sub_abc123",
      "target_url": "https://app1.example.com/notify",
      "status": "success",
      "attempts": 1,
      "last_attempt": "2024-01-15T10:30:05Z",
      "response_code": 200
    },
    {
      "subscriber_id": "sub_def456",
      "target_url": "https://app2.example.com/notify",
      "status": "retrying",
      "attempts": 2,
      "last_attempt": "2024-01-15T10:30:30Z",
      "next_retry": "2024-01-15T10:31:30Z",
      "response_code": 500
    }
  ]
}
```

---

## § 4 — Deployment & Operations

### 4.1 Initial Deployment

```bash
# 1. Deploy the app
flyctl deploy

# 2. Check app is running
flyctl status

# 3. Check logs
flyctl logs

# 4. Test the app
curl https://<app-name>.fly.dev/health
# Expected: { "status": "ok" }
```

### 4.2 Scaling

Scale your broker to multiple instances for high throughput:

```bash
# Scale to 3 instances (auto-fallback if one fails)
flyctl scale count 3

# Check running instances
flyctl status

# View instance details
flyctl scale show
```

### 4.3 Database Setup (Persistent Queue)

If using PostgreSQL for the queue:

```bash
# Attach a PostgreSQL database to your Fly app
flyctl postgres attach webhook-broker-db

# Set DATABASE_URL secret (should be done automatically)
# Verify it:
flyctl secrets list | grep DATABASE

# Run migrations (if using a migration tool like Flyway or sqlc)
flyctl ssh console
# Inside console:
psql $DATABASE_URL -f migrations/001-init.sql
exit
```

### 4.4 Monitoring & Logs

**Stream live logs:**

```bash
flyctl logs --follow
```

**Search logs for errors:**

```bash
flyctl logs | grep ERROR
```

**Export metrics to observability platform:**

If using **SigNoz**, **Datadog**, or **New Relic**, configure OpenTelemetry:

```bash
flyctl secrets set \
  OBSERVABILITY_ENABLED=true \
  OTEL_EXPORTER_OTLP_ENDPOINT="https://your-collector.example.com"
```

---

## § 5 — Troubleshooting

### 5.1 Events Not Being Delivered

**Symptoms:** Events are queued but not reaching subscribers.

**Checklist:**

1. Check if subscriber is registered:
   ```bash
   curl https://<app-name>.fly.dev/webhooks/subscribers?event_type=order.created
   ```
   Expect a non-empty list.

2. Check if subscriber URL is reachable:
   ```bash
   curl -X POST https://your-subscriber.example.com/notify \
     -H "Content-Type: application/json" \
     -d '{"test": "payload"}'
   ```

3. Check logs for delivery errors:
   ```bash
   flyctl logs | grep "delivery failed"
   ```

4. Verify HMAC signature on subscriber side — broker signs with `WEBHOOK_SIGNING_KEY`:
   ```python
   import hmac
   import hashlib
   
   signature = request.headers.get('X-Webhook-Signature')
   body_bytes = request.data
   expected = 'sha256=' + hmac.new(
     WEBHOOK_SIGNING_KEY.encode(),
     body_bytes,
     hashlib.sha256
   ).hexdigest()
   assert signature == expected, "Invalid signature"
   ```

### 5.2 High Latency or Timeouts

**Symptoms:** Deliveries are slow or timing out.

**Solutions:**

1. Scale up to more instances:
   ```bash
   flyctl scale count 5
   ```

2. Increase timeout for slow subscribers:
   ```bash
   flyctl secrets set DELIVERY_TIMEOUT_MS=10000
   flyctl deploy
   ```

3. Check database performance (if using PostgreSQL):
   ```bash
   flyctl ssh console
   psql $DATABASE_URL
   EXPLAIN ANALYZE SELECT * FROM events WHERE status = 'pending' LIMIT 1000;
   \q
   exit
   ```

### 5.3 Memory or Disk Pressure

**Symptoms:** App crashes, "out of memory" or "disk full" errors.

**Solutions:**

1. Check resource usage:
   ```bash
   flyctl metrics
   ```

2. If using in-memory queue, switch to PostgreSQL:
   ```bash
   flyctl secrets set QUEUE_TYPE=postgres
   flyctl deploy
   ```

3. Increase instance RAM:
   ```bash
   flyctl scale memory <amount-in-MB>
   # e.g., flyctl scale memory 512
   ```

---

## § 6 — Security Best Practices

### 6.1 HMAC Signing

**Always verify webhook signatures** on the subscriber side:

1. **Generate a strong key** (32 random hex bytes):
   ```bash
   openssl rand -hex 32
   ```

2. **Set in Fly.io:**
   ```bash
   flyctl secrets set WEBHOOK_SIGNING_KEY="<your-key>"
   ```

3. **Share with subscribers** (via secure channel, never in URL):
   ```bash
   # Subscriber stores this securely (env var, secrets manager)
   export WEBHOOK_SIGNING_KEY="<same-key>"
   ```

4. **Verify on receipt:**
   ```python
   import hmac
   import hashlib
   
   def verify_signature(request):
       sig = request.headers.get('X-Webhook-Signature', '')
       body = request.data
       expected = 'sha256=' + hmac.new(
           WEBHOOK_SIGNING_KEY.encode(),
           body,
           hashlib.sha256
       ).hexdigest()
       return hmac.compare_digest(sig, expected)
   ```

### 6.2 Rate Limiting

Protect your subscribers from being overwhelmed:

```bash
flyctl secrets set \
  RATE_LIMIT_PER_SECOND=100 \
  RATE_LIMIT_WINDOW_SECONDS=60
```

### 6.3 Secrets Management

Never commit secrets to git:

```bash
# .gitignore
.env
.env.local
fly.toml  # (if it contains secrets; use fly.toml template instead)
```

Use `flyctl secrets` for all credentials:

```bash
flyctl secrets set VAR=value  # Creates on Fly.io
flyctl secrets unset VAR       # Removes from Fly.io
flyctl secrets list            # Lists all (values hidden)
```

---

## § 7 — Advanced Topics

### 7.1 Multi-Region Deployment

Deploy the broker to multiple regions for high availability:

```bash
# Add a region
flyctl regions add iad sjc

# Check regions
flyctl regions list

# Scale across regions
flyctl scale count 2 --max-per-region 1
```

### 7.2 Custom Event Types

Define and manage event types:

```bash
# Register a new event type (app-specific)
POST /webhooks/event-types
{
  "name": "order.created",
  "schema": {
    "type": "object",
    "properties": {
      "order_id": { "type": "string" },
      "customer": { "type": "string" },
      "total": { "type": "number" }
    },
    "required": ["order_id"]
  }
}
```

### 7.3 Dead-Letter Queue (DLQ)

Events that fail all retries go to the DLQ:

```bash
# Enable DLQ
flyctl secrets set DLQ_ENABLED=true

# Query DLQ
GET /webhooks/dlq?limit=100

# Response includes failed events with error details for debugging
```

### 7.4 Webhook Replay

Replay failed events to subscribers:

```bash
POST /webhooks/replay
{
  "event_id": "evt_abc123",
  "subscriber_id": "sub_def456"
}
```

---

## § 8 — Integration Examples

### 8.1 GitHub Webhooks → Your App

Receive GitHub events (push, PR, etc.) and forward to your app:

```bash
# 1. Register GitHub as subscriber
curl -X POST https://<broker>.fly.dev/webhooks/register \
  -H "Content-Type: application/json" \
  -d '{
    "webhook_url": "https://your-app.example.com/github-events",
    "event_type": "github.push",
    "active": true
  }'

# 2. On GitHub repo settings, add webhook:
# Payload URL: https://<broker>.fly.dev/webhooks/send
# Event type: Push events
# Secret: <use WEBHOOK_SIGNING_KEY>

# 3. Broker receives from GitHub, forwards to your-app.example.com
```

### 8.2 SaaS Webhook Distribution

Use broker to replay webhooks to multiple internal services:

```bash
# Register multiple subscribers
for service in order-processor billing-service notification-service; do
  curl -X POST https://<broker>.fly.dev/webhooks/register \
    -d "{
      \"webhook_url\": \"https://$service.internal/webhooks\",
      \"event_type\": \"order.created\",
      \"active\": true
    }"
done

# Send event once
curl -X POST https://<broker>.fly.dev/webhooks/send \
  -d '{
    "event_type": "order.created",
    "event_id": "evt_1",
    "data": {"order_id": "123"}
  }'

# Broker delivers to all three services with retries
```

---

## § 9 — Checklist: Running Production

- [ ] **Secrets set** — `WEBHOOK_SIGNING_KEY`, `LOG_LEVEL`, `DATABASE_URL` (if using DB)
- [ ] **Scaled to ≥2 instances** — `flyctl scale count 2`
- [ ] **Database attached** (if persistent queue) — `flyctl postgres attach`
- [ ] **Monitoring enabled** — Logs streaming, metrics exported
- [ ] **HMAC verification** implemented on subscribers
- [ ] **Rate limiting configured** (optional but recommended)
- [ ] **DLQ enabled** for observability of failed events
- [ ] **Health endpoint** accessible — `curl https://<app>.fly.dev/health`
- [ ] **Tested with real events** — Run end-to-end test before declaring "live"
- [ ] **Alerts set up** — Notify on delivery failures, high error rate, queue backlog

---

## § 10 — Quick Reference: Common Commands

| Task | Command |
|------|---------|
| Deploy | `flyctl deploy` |
| Check status | `flyctl status` |
| Stream logs | `flyctl logs --follow` |
| Set secret | `flyctl secrets set KEY=value` |
| List secrets | `flyctl secrets list` |
| Scale instances | `flyctl scale count 3` |
| SSH into app | `flyctl ssh console` |
| Restart app | `flyctl restart` |
| View metrics | `flyctl metrics` |
| Attach database | `flyctl postgres attach db-name` |
| Redeploy after config | `flyctl deploy` |

---

## Summary

A **webhook broker on Fly.io** provides:

✅ **Reliability** — Automatic retries, exponential backoff, DLQ  
✅ **Scalability** — Multi-instance, multi-region deployment  
✅ **Security** — HMAC signing, secrets management, rate limiting  
✅ **Observability** — Logs, metrics, delivery tracking  
✅ **Simplicity** — One-line registration, event-driven architecture  

Start with § 0 for quick deployment. Refer to § 2–9 for configuration, troubleshooting, and advanced patterns.

**Read the official Fly.io docs** for deeper details on deployments, scaling, and PostgreSQL:  
https://fly.io/docs

---

**Version:** 1.0.0  
**Last Updated:** 2024-01-15  
**Maintainer:** LightHeart Ventures Webhook Team
