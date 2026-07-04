# Deployment — aish Webhook Broker

The broker is a single self-contained binary with an embedded SQLite engine. Its
only runtime dependency is a writable path for the database file. This guide
covers Docker, systemd, and AWS (EC2 / ECS / Lambda), plus TLS and hardening.

> The Dockerfile, systemd unit, and task definitions below are **templates** —
> they aren't shipped in the crate. Copy them into your deploy repo.

## Build

```bash
# Standalone crate — build from crates/aish-webhook-broker/
cargo build --release
# → target/release/aish-webhook-broker  (statically-bundled SQLite)
```

The binary is portable across Linux hosts of the same libc family. For a fully
static musl build (no glibc dependency), use the cross target:

```bash
rustup target add x86_64-unknown-linux-musl
cargo build --release --target x86_64-unknown-linux-musl
```

## State & persistence

- The DB file (`--db`) is the durable queue. **Persist it** across restarts —
  a lost DB means a lost backlog.
- SQLite runs in WAL mode: expect `broker.db`, `broker.db-wal`, `broker.db-shm`
  in the same directory. Back up all three (or checkpoint first).
- A single broker process owns the file. Do **not** point two brokers at one DB
  (or a networked filesystem). Scale vertically, or shard by tenant with
  separate DBs/processes.
- Graceful shutdown: the broker drains and exits cleanly on `SIGINT`/`SIGTERM`.

---

## Docker

`Dockerfile`:

```dockerfile
# ---- build ----
FROM rust:1-bookworm AS build
WORKDIR /src
COPY . .
RUN cargo build --release --locked
# binary lands at target/release/aish-webhook-broker

# ---- runtime ----
FROM debian:bookworm-slim
RUN useradd -r -u 10001 broker && mkdir -p /var/lib/aish-broker && chown broker /var/lib/aish-broker
COPY --from=build /src/target/release/aish-webhook-broker /usr/local/bin/
USER broker
VOLUME ["/var/lib/aish-broker"]
EXPOSE 8080
ENV BROKER_LISTEN=0.0.0.0:8080 \
    BROKER_DB=/var/lib/aish-broker/broker.db \
    BROKER_LOG_LEVEL=info
ENTRYPOINT ["aish-webhook-broker"]
```

Run:

```bash
docker build -t aish-webhook-broker .
docker run -d --name broker \
  -p 8080:8080 \
  -v aish_broker_data:/var/lib/aish-broker \
  aish-webhook-broker
```

`docker-compose.yml`:

```yaml
services:
  broker:
    build: .
    ports: ["8080:8080"]
    volumes: ["aish_broker_data:/var/lib/aish-broker"]
    environment:
      BROKER_MAX_QUEUE_SIZE: "1000"
      BROKER_MSG_TTL_SECS: "604800"
      BROKER_LOG_LEVEL: "info"
    restart: unless-stopped
volumes:
  aish_broker_data:
```

Health check: `GET /health` → `200`. Add to compose with
`healthcheck: { test: ["CMD","curl","-fsS","http://localhost:8080/health"], interval: 30s }`.

---

## systemd (self-hosted / bare EC2)

`/etc/systemd/system/aish-webhook-broker.service`:

```ini
[Unit]
Description=aish Webhook Broker
After=network-online.target
Wants=network-online.target

[Service]
Type=exec
User=broker
Group=broker
EnvironmentFile=/etc/aish-webhook-broker.env
ExecStart=/usr/local/bin/aish-webhook-broker
Restart=on-failure
RestartSec=2
# hardening
StateDirectory=aish-broker
ReadWritePaths=/var/lib/aish-broker
ProtectSystem=strict
ProtectHome=true
PrivateTmp=true
NoNewPrivileges=true

[Install]
WantedBy=multi-user.target
```

```bash
sudo useradd -r -s /usr/sbin/nologin broker
sudo install -m0755 target/release/aish-webhook-broker /usr/local/bin/
# create /etc/aish-webhook-broker.env (see CONFIGURATION.md)
sudo systemctl daemon-reload
sudo systemctl enable --now aish-webhook-broker
journalctl -u aish-webhook-broker -f
```

---

## AWS

### EC2 (simplest)

1. Launch a small instance (t4g.small is plenty; ARM works).
2. Install the binary + the systemd unit above.
3. Attach an EBS volume for `/var/lib/aish-broker` and keep the DB on it.
4. Front with an **Application Load Balancer**:
   - Target group → instance:8080, health check path `/health`.
   - ALB supports WebSocket upgrades natively.
   - Raise the target group / ALB **idle timeout** above `ws_heartbeat_secs`
     and `poll_timeout_secs` (bump ALB idle timeout to e.g. 120 s).
   - Terminate TLS at the ALB (ACM cert).

### ECS / Fargate

Recommended for a managed container. Because the DB is single-writer, run
**exactly one task** (`desired_count = 1`) and persist state on **EFS**.

Task definition sketch:

```json
{
  "family": "aish-webhook-broker",
  "cpu": "256", "memory": "512",
  "networkMode": "awsvpc",
  "requiresCompatibilities": ["FARGATE"],
  "containerDefinitions": [{
    "name": "broker",
    "image": "<acct>.dkr.ecr.<region>.amazonaws.com/aish-webhook-broker:latest",
    "portMappings": [{ "containerPort": 8080, "protocol": "tcp" }],
    "environment": [
      { "name": "BROKER_DB", "value": "/data/broker.db" },
      { "name": "BROKER_LOG_LEVEL", "value": "info" }
    ],
    "mountPoints": [{ "sourceVolume": "state", "containerPath": "/data" }],
    "healthCheck": {
      "command": ["CMD-SHELL", "curl -fsS http://localhost:8080/health || exit 1"],
      "interval": 30, "timeout": 5, "retries": 3
    }
  }],
  "volumes": [{
    "name": "state",
    "efsVolumeConfiguration": { "fileSystemId": "fs-xxxx", "transitEncryption": "ENABLED" }
  }]
}
```

- Put the service behind an ALB (WebSocket-capable) with health check `/health`.
- **Do not** scale beyond one task on a shared DB. To scale, shard tenants
  across independent services, each with its own EFS/DB.
- Set the ALB deregistration delay low so deploys drain quickly.

### Lambda — not recommended

The broker is a long-lived stateful server (persistent WebSocket connections,
in-memory hub, a single-writer SQLite file, an hourly background sweep). None of
that maps onto Lambda's stateless, time-boxed, ephemeral-filesystem model:

- WebSocket delivery would require API Gateway WebSocket APIs + a rewrite.
- SQLite on `/tmp` is per-invocation and non-durable; you'd need RDS/Dynamo.
- The TTL sweep needs an always-on runtime.

Use **ECS/Fargate or EC2**. If you need serverless webhook ingestion, front the
broker with API Gateway/Lambda that forwards to a running broker instance — keep
the broker itself on a persistent compute surface.

---

## Hardening

The poll and ACK endpoints are **unauthenticated** in this release, and there is
no built-in TLS or rate limiting. Deploy defensively:

- **Terminate TLS** at an ALB / nginx / Caddy in front of the broker; bind the
  broker to `127.0.0.1` or a private subnet.
- **Restrict ingress** — only the load balancer's security group should reach
  port 8080. Only your webhook producers' IP ranges should reach the public
  `/webhooks/...` path (GitHub publishes its hook egress ranges).
- **Always set a per-route `secret`** so inbound webhooks are HMAC-verified
  (`401` otherwise). See [API.md](API.md#signature-verification).
- **Rate limit / WAF** the public ingress at the proxy layer.
- **Back up** the DB directory (all three WAL files) on a schedule.
- Run as a non-root user (the systemd/Docker templates above already do).

## Reverse proxy (nginx) — WebSocket-aware

```nginx
location / {
    proxy_pass         http://127.0.0.1:8080;
    proxy_http_version 1.1;
    proxy_set_header   Upgrade $http_upgrade;
    proxy_set_header   Connection "upgrade";
    proxy_set_header   Host $host;
    proxy_read_timeout 120s;   # > ws_heartbeat_secs and > poll_timeout_secs
}
```
