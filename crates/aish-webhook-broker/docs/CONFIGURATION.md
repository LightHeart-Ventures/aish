# Configuration — aish Webhook Broker

Every setting is a CLI flag with a matching environment variable. CLI flags take
precedence over env vars, which take precedence over the built-in defaults.

```bash
aish-webhook-broker \
  --listen 0.0.0.0:8080 \
  --db /var/lib/aish-broker.db \
  --max-queue-size 1000 \
  --ws-heartbeat-secs 30 \
  --poll-timeout-secs 60 \
  --msg-ttl-secs 604800 \
  --log-level info
```

## Reference

| CLI flag | Env var | Type | Default | Description |
|----------|---------|------|---------|-------------|
| `-l`, `--listen` | `BROKER_LISTEN` | `host:port` | `0.0.0.0:8080` | TCP socket address to bind. Must parse as a `SocketAddr` — use an explicit IP (`0.0.0.0`, `127.0.0.1`), not a hostname. |
| `-d`, `--db` | `BROKER_DB` | path | `/var/lib/aish-broker.db` | SQLite database file. Created if absent; parent directory must exist and be writable. WAL sidecars (`-wal`, `-shm`) are written alongside. |
| `--max-queue-size` | `BROKER_MAX_QUEUE_SIZE` | int | `1000` | Max undelivered messages retained **per `(tenant_id, plugin_id)`**. On overflow the oldest undelivered message is dropped (favours fresh events). `0` disables queuing — every webhook is rejected with `503`. |
| `--ws-heartbeat-secs` | `BROKER_WS_HEARTBEAT_SECS` | int (s) | `30` | Interval between server→client WebSocket `Ping` frames. Floored to `1`. Lower it to detect dead peers faster; raise it to cut idle chatter. |
| `--poll-timeout-secs` | `BROKER_POLL_TIMEOUT_SECS` | int (s) | `60` | Upper bound on a long-poll's `wait_secs`. A client asking for longer is capped here. Set below your reverse-proxy/idle timeout. |
| `--msg-ttl-secs` | `BROKER_MSG_TTL_SECS` | int (s) | `604800` (7 d) | Age at which an undelivered webhook expires. A background sweep runs **hourly** and purges expired rows. |
| `--log-level` | `BROKER_LOG_LEVEL` | string | `info` | `tracing` `EnvFilter` directive. Accepts levels (`error`/`warn`/`info`/`debug`/`trace`) and per-target filters, e.g. `info,aish_webhook_broker=debug,tower_http=warn`. |

## Notes & tuning

- **Bind address** — `--listen` is parsed with `SocketAddr::parse`; a bare
  hostname will fail at startup. Bind `127.0.0.1` when a reverse proxy fronts
  the broker; `0.0.0.0` to accept external traffic directly.
- **Queue sizing** — the cap is per route, not global. With `max_queue_size=1000`
  and 50 active `(tenant, plugin)` pairs the worst-case backlog is ~50k rows.
  Size it against how long a client may stay offline × event rate.
- **TTL vs. queue cap** — two independent back-pressure mechanisms: the cap
  bounds depth, the TTL bounds age. A message leaves the queue when ACKed,
  evicted by the cap, or swept past its TTL.
- **Poll timeout** — keep `poll_timeout_secs` comfortably under any
  load-balancer/proxy idle timeout (e.g. ALB default 60 s) so long-polls return
  before the connection is reaped.
- **Heartbeat** — must be shorter than intermediary idle timeouts to keep
  WebSocket connections alive through proxies/NAT.
- **Logging** — logs go to stdout. For JSON/structured shipping, run behind a
  collector; `BROKER_LOG_LEVEL` controls verbosity and per-module filtering.

## Minimal env file

```bash
# /etc/aish-webhook-broker.env
BROKER_LISTEN=0.0.0.0:8080
BROKER_DB=/var/lib/aish-broker/broker.db
BROKER_MAX_QUEUE_SIZE=1000
BROKER_WS_HEARTBEAT_SECS=30
BROKER_POLL_TIMEOUT_SECS=60
BROKER_MSG_TTL_SECS=604800
BROKER_LOG_LEVEL=info
```
