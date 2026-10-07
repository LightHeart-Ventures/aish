# SigNoz observability for `aish-webhook-broker`

TASK-375. The broker exports OpenTelemetry metrics over **OTLP HTTP/protobuf**
when `OTEL_EXPORTER_OTLP_ENDPOINT` is set. When it is unset, the broker builds no
exporter and starts no thread. The exporter is compiled in by the default `otel`
cargo feature. Build with `--no-default-features` to leave it out.

## Enable export

| Env var | Example | Notes |
|---|---|---|
| `OTEL_EXPORTER_OTLP_ENDPOINT` | `http://otel-collector:4318` | **Turns export on.** OTLP/HTTP base URL. The SDK appends `/v1/metrics`. |
| `OTEL_EXPORTER_OTLP_HEADERS` | `signoz-ingestion-key=<key>` | Needed for SigNoz Cloud (`https://ingest.<region>.signoz.cloud:443`). |
| `OTEL_SERVICE_NAME` | `aish-webhook-broker` | The default when unset. |
| `OTEL_RESOURCE_ATTRIBUTES` | `deployment.environment=prod` | Optional. |
| `OTEL_METRIC_EXPORT_INTERVAL` | `60000` | Milliseconds. The SDK default is 60 s. |
| `LOG_FORMAT` | `json` | One JSON object per log line, for collector log pipelines. The default is `text`. |

Self-hosted SigNoz: point the endpoint at the SigNoz OTel collector's HTTP
receiver, which listens on port 4318.

## Metrics

All points carry the `tenant_id` and `plugin_id` attributes. The broker reads
every value from the same sources as `GET /stats` (TASK-314).

| Metric | Type | Meaning |
|---|---|---|
| `aish.webhook.broker.received` | counter | Webhooks accepted (HTTP 202) |
| `aish.webhook.broker.delivered` | counter, `transport`=`ws`\|`poll` | Envelopes delivered to clients |
| `aish.webhook.broker.dropped` | counter | Undelivered webhooks evicted by the queue cap (`BROKER_MAX_QUEUE_SIZE`) |
| `aish.webhook.broker.expired` | counter | Undelivered webhooks removed by the TTL sweep |
| `aish.webhook.broker.queued` | gauge | Undelivered webhooks queued right now (durable) |
| `aish.webhook.broker.acked` | gauge | Acknowledged webhooks kept until their TTL |

The counters are cumulative from process start, so they reset when the broker
restarts. Use `rate`/`increase` to query them.

## Dashboard

Import `broker-dashboard.json` through SigNoz → Dashboards → New dashboard →
Import JSON. It uses the SigNoz v6 schema. It has these panels: queued now,
received/dropped/expired over the selected range, received rate by plugin,
delivered rate by transport, queue depth by plugin, loss rate, acked, and a
per-plugin health table. It also has filter variables for `service.name`,
`tenant_id` and `plugin_id`.

### Suggested alerts

- `aish.webhook.broker.dropped` increase > 0 over 5m: a consumer is offline and the queue cap is losing data.
- `aish.webhook.broker.expired` increase > 0: webhooks aged out without ever being delivered.
- `aish.webhook.broker.queued` > 80% of `BROKER_MAX_QUEUE_SIZE` for 10m: a consumer is falling behind. This is the delivery-latency proxy.

## Load testing

`../../scripts/loadgen.py` sends HMAC-signed POSTs at a fixed rate. It uses only
the Python standard library:

```sh
python3 crates/aish-webhook-broker/scripts/loadgen.py \
  --url http://localhost:8080 --tenant acme --plugin github \
  --secret s3cret --register --rate 50 --duration 30
```
