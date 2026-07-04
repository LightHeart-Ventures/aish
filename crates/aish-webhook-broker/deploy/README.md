# Deploying `aish-webhook-broker`

TASK-263 / SPR-059 Phase 4.7 — container image + systemd service for the
self-hosted webhook broker. This directory holds everything needed to run the
broker in production; the crate itself lands in [#515](https://github.com/LightHeart-Ventures/aish/pull/515).

The broker is a single static-ish Rust binary that:
- listens on `:8080` (`GET /health`, `POST /webhooks/:tenant/:plugin`, `/ws`, long-poll),
- persists queued webhooks in a SQLite database (`BROKER_DB`),
- verifies HMAC-SHA256 signatures when a tenant registers a secret.

## Option A — Docker

```sh
# Build (context is the crate dir; standalone workspace → fast, isolated build)
docker build -t aish-webhook-broker:latest crates/aish-webhook-broker

# Run with a persistent volume for the SQLite db
docker run -d --name aish-webhook-broker \
  -p 8080:8080 \
  -v aish_broker_data:/var/lib \
  aish-webhook-broker:latest

curl -fsS http://localhost:8080/health
```

Image details:
- multi-stage build (`rust:1.83-slim` builder → `debian:bookworm-slim` runtime),
- runs as the unprivileged `broker` user,
- built-in `HEALTHCHECK` hitting `/health`,
- config entirely via `BROKER_*` env vars (see `src/main.rs`).

## Option B — docker compose

```sh
docker compose -f crates/aish-webhook-broker/deploy/docker-compose.yml up -d
docker compose -f crates/aish-webhook-broker/deploy/docker-compose.yml logs -f
```

Named volume `broker_data` persists `/var/lib/aish-broker.db` across restarts.

## Option C — systemd (bare metal)

```sh
# 1. Install the binary
cargo build --release --manifest-path crates/aish-webhook-broker/Cargo.toml
sudo install -m0755 crates/aish-webhook-broker/target/release/aish-webhook-broker \
  /usr/local/bin/aish-webhook-broker

# 2. Create the service account + state dir
sudo useradd --system --home-dir /var/lib/aish-broker --create-home aish-broker

# 3. Config
sudo mkdir -p /etc/aish-broker
sudo cp crates/aish-webhook-broker/deploy/broker.env.example /etc/aish-broker/broker.env

# 4. Install + start the unit
sudo cp crates/aish-webhook-broker/deploy/aish-webhook-broker.service \
  /etc/systemd/system/aish-webhook-broker.service
sudo systemctl daemon-reload
sudo systemctl enable --now aish-webhook-broker
systemctl status aish-webhook-broker
```

The unit is hardened (`NoNewPrivileges`, `ProtectSystem=strict`, `ProtectHome`,
`PrivateTmp`, a dedicated `StateDirectory`) and restarts on failure.

## Configuration reference

| Env var | Default | Meaning |
|---|---|---|
| `BROKER_LISTEN` | `0.0.0.0:8080` | Listen address |
| `BROKER_DB` | `/var/lib/aish-broker.db` | SQLite database path |
| `BROKER_MAX_QUEUE_SIZE` | `1000` | Max queued webhooks per (tenant, plugin) |
| `BROKER_WS_HEARTBEAT_SECS` | `30` | WebSocket heartbeat interval |
| `BROKER_POLL_TIMEOUT_SECS` | `60` | Long-poll timeout |
| `BROKER_MSG_TTL_SECS` | `604800` | Webhook TTL (7 days) |
| `BROKER_LOG_LEVEL` | `info` | `tracing` env-filter level |

## Reverse proxy / TLS

Terminate TLS at a reverse proxy (nginx, Caddy, an ALB) in front of the broker
and forward to `127.0.0.1:8080`. The broker speaks plain HTTP/WS; the proxy is
responsible for `wss://` upgrades and the public certificate.
