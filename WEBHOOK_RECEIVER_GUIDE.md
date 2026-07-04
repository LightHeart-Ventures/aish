# Aish Webhook Receiver — Deployment Guide

## What You Built

A production-ready webhook receiver service that:
- ✅ Accepts webhooks from any source (GitHub, Stripe, Slack, etc.)
- ✅ Verifies HMAC-SHA256 signatures
- ✅ Stores webhooks in SQLite with auto-schema
- ✅ REST API to query stored webhooks
- ✅ Deployable to Fly.io with one script

**Binary location:** `/home/grhohertz/projects/aish/target/release/webhook-receiver`

## Deploy to Fly.io (30 seconds)

### Prerequisites
```bash
# Install flyctl
curl -L https://fly.io/install.sh | sh

# Log in
fly auth login
```

### Deploy
```bash
cd /home/grhohertz/projects/aish/crates/webhook-receiver
bash deploy.sh
```

This script will:
1. ✅ Build the release binary
2. ✅ Create a Fly app (or use existing)
3. ✅ Create a persistent volume for SQLite
4. ✅ Set up webhook secret
5. ✅ Deploy the container
6. ✅ Output the public URL

### Manual Deployment
```bash
# If you prefer step-by-step:
cd crates/webhook-receiver

# Deploy with Fly.io config
flyctl deploy --config fly.toml -a aish-webhooks

# View logs
flyctl logs -a aish-webhooks

# Check status
flyctl status -a aish-webhooks
```

## Local Testing

```bash
# Build
cargo build -p webhook-receiver --release

# Run locally
export WEBHOOK_SECRET="your-secret-key"
export DATABASE_URL="sqlite:webhooks.db"
./target/release/webhook-receiver

# In another terminal
curl http://localhost:8080/health

# Send a test webhook
python crates/webhook-receiver/examples/send_webhook.py \
  --source github \
  --event push \
  --data '{"ref":"refs/heads/main"}' \
  --secret "your-secret-key"
```

## Integration Examples

### GitHub Webhooks

```bash
# Configure webhook in GitHub repo settings
# Payload URL: https://your-app.fly.dev/webhooks/github
# Content type: application/json
# Secret: (set in WEBHOOK_SECRET)
# Events: Select individual events...
```

### Stripe Webhooks

```bash
# In Stripe Dashboard → Webhooks
# Endpoint URL: https://your-app.fly.dev/webhooks/stripe
# Events: Select events to send...
```

### Slack Events

```bash
# In Slack App → Event Subscriptions
# Request URL: https://your-app.fly.dev/webhooks/slack
# Subscribe to events: app_mention, message, etc.
```

## Query Webhooks

```bash
# List all GitHub webhooks
curl https://your-app.fly.dev/webhooks/github

# Get a specific webhook
curl https://your-app.fly.dev/webhooks/github/550e8400-e29b-41d4-a716-446655440000

# Paginate
curl "https://your-app.fly.dev/webhooks/github?limit=20&offset=0"
```

## Next Steps

### Monitor
```bash
# Live logs
flyctl logs -a aish-webhooks -f

# SSH into the instance
flyctl ssh console -a aish-webhooks
```

### Scale
```bash
# Add more instances (for redundancy/load)
flyctl scale count 2 -a aish-webhooks

# Adjust memory
flyctl scale memory 1024 -a aish-webhooks
```

### Customize
- Edit `fly.toml` for region, resource sizing, env vars
- Add webhook filtering/routing in handlers
- Store derived events in aish's main database

## File Structure

```
crates/webhook-receiver/
├── src/
│   ├── main.rs             # 284 lines: handlers, DB, signing
│   └── tests.rs            # Unit tests
├── examples/
│   └── send_webhook.py     # Testing client (standalone)
├── Dockerfile              # Container build
├── fly.toml                # Fly.io config
├── deploy.sh               # One-script deployment
├── Cargo.toml              # Deps: axum, sqlx, tokio, serde, hmac
└── README.md               # Full reference
```

## Key Endpoints

| Method | Path | Purpose |
|--------|------|---------|
| POST | `/webhooks/{source}` | Receive a webhook |
| GET | `/webhooks/{source}` | List webhooks |
| GET | `/webhooks/{source}/{id}` | Get specific webhook |
| GET | `/health` | Health check |

## Environment Variables

| Variable | Default | Purpose |
|----------|---------|---------|
| `WEBHOOK_SECRET` | (required) | HMAC-SHA256 key |
| `DATABASE_URL` | `sqlite:webhooks.db` | SQLite path |
| `PORT` | `8080` | Listen port |
| `RUST_LOG` | `info` | Log level |

## Troubleshooting

**"Connection refused"** → Server not running or wrong port
```bash
curl http://localhost:8080/health
```

**"Database is locked"** → Normal under concurrent load; SQLite is single-writer
```bash
# Check log volume
curl http://localhost:8080/webhooks/github | jq '.count'
```

**"Signature mismatch"** → Sender secret ≠ `WEBHOOK_SECRET`
```bash
# Verify secret is set
flyctl secrets list -a aish-webhooks
```

## Cost on Fly.io

- **Free tier**: Up to 3 shared-cpu-1x instances (1GB RAM)
- **Disk**: 1GB volume @ ~$0.15/month
- **Bandwidth**: First 30GB free, then $0.02/GB
- **Estimated**: ~$3–5/month for production

## Security Checklist

- ✅ HMAC-SHA256 signature verification (constant-time)
- ✅ SQL parameter binding (no injection)
- ✅ No external API calls
- ✅ Secrets in environment, not code
- ⚠️ Add IP whitelisting via Fly.io edge rules (recommended)
- ⚠️ Add rate limiting via reverse proxy (recommended)

## Next Integration Ideas

1. **Forward to aish**: POST webhooks to aish's event API
2. **Filter by event**: Route different sources to different handlers
3. **Webhook delivery queue**: Implement retries for processing
4. **Metrics export**: Prometheus endpoint for monitoring
5. **Web UI**: Dashboard to browse received webhooks

---

**Ready to deploy?** Run `bash crates/webhook-receiver/deploy.sh` now!
