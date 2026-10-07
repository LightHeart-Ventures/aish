//! OpenTelemetry metrics export (TASK-375) — SigNoz / any OTLP collector.
//!
//! When `OTEL_EXPORTER_OTLP_ENDPOINT` is set, [`init`] builds an OTLP
//! HTTP/protobuf exporter with a periodic reader and registers *observable*
//! instruments. When it is unset nothing is constructed (no thread, no client):
//! the default deployment pays zero cost.
//!
//! There is no second counting system here. Every callback reads the same
//! sources as `GET /stats` (TASK-314) at collection time:
//!
//! | metric | kind | source |
//! |---|---|---|
//! | `aish.webhook.broker.received` | counter | [`BrokerStats::counters`] `received` |
//! | `aish.webhook.broker.delivered` | counter (`transport`=ws\|poll) | `delivered_ws` / `delivered_poll` |
//! | `aish.webhook.broker.dropped` | counter | `dropped` (queue-cap overflow) |
//! | `aish.webhook.broker.expired` | counter | `expired` (TTL sweep of undelivered) |
//! | `aish.webhook.broker.queued` | gauge | [`db::count_by_key`] `queued` |
//! | `aish.webhook.broker.acked` | gauge | [`db::count_by_key`] `acked` |
//!
//! Every point carries `tenant_id` and `plugin_id` attributes — never payloads,
//! secrets, client ids or session tokens. The standard OTel env vars
//! (`OTEL_EXPORTER_OTLP_HEADERS`, `OTEL_EXPORTER_OTLP_TIMEOUT`,
//! `OTEL_METRIC_EXPORT_INTERVAL`, `OTEL_SERVICE_NAME`,
//! `OTEL_RESOURCE_ATTRIBUTES`) are honoured by the SDK.
//!
//! [`BrokerStats::counters`]: crate::stats::BrokerStats::counters

use opentelemetry::metrics::{Meter, MeterProvider as _};
use opentelemetry::KeyValue;
use opentelemetry_otlp::{MetricExporter, Protocol, WithExportConfig};
use opentelemetry_sdk::metrics::{PeriodicReader, SdkMeterProvider};
use opentelemetry_sdk::Resource;

use crate::config::BrokerConfig;
use crate::db;
use crate::stats::Counters;

/// Instrumentation scope / default `service.name`.
pub const METER_NAME: &str = "aish-webhook-broker";

pub const RECEIVED: &str = "aish.webhook.broker.received";
pub const DELIVERED: &str = "aish.webhook.broker.delivered";
pub const DROPPED: &str = "aish.webhook.broker.dropped";
pub const EXPIRED: &str = "aish.webhook.broker.expired";
pub const QUEUED: &str = "aish.webhook.broker.queued";
pub const ACKED: &str = "aish.webhook.broker.acked";

/// Every metric name the broker exports (dashboards/tests key off this).
pub const METRIC_NAMES: [&str; 6] = [RECEIVED, DELIVERED, DROPPED, EXPIRED, QUEUED, ACKED];

/// Env var that switches the exporter on.
pub const ENDPOINT_ENV: &str = "OTEL_EXPORTER_OTLP_ENDPOINT";

const UNIT: &str = "{webhook}";

/// Keeps the meter provider alive; [`MetricsGuard::shutdown`] flushes the
/// final collection on graceful exit.
pub struct MetricsGuard {
    provider: SdkMeterProvider,
}

impl MetricsGuard {
    /// Flush and stop the periodic reader. Blocking — call off the async
    /// executor (e.g. `spawn_blocking`).
    pub fn shutdown(self) {
        if let Err(e) = self.provider.shutdown() {
            tracing::warn!(error = %e, "OTel metrics shutdown failed");
        }
    }
}

/// The OTLP endpoint from the environment, if set to a non-blank value.
pub fn endpoint_from_env() -> Option<String> {
    normalize_endpoint(std::env::var(ENDPOINT_ENV).ok())
}

fn normalize_endpoint(raw: Option<String>) -> Option<String> {
    raw.map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

/// Start OTLP metrics export for `config`, or do nothing when `endpoint` is
/// `None`/blank. The exporter itself reads the endpoint (and headers, timeout)
/// from the standard `OTEL_EXPORTER_OTLP_*` env vars; `endpoint` is the
/// on/off switch the caller resolved via [`endpoint_from_env`].
pub fn init(config: &BrokerConfig, endpoint: Option<&str>) -> anyhow::Result<Option<MetricsGuard>> {
    if endpoint.map(str::trim).unwrap_or("").is_empty() {
        return Ok(None);
    }

    install_ring_provider();
    let exporter = MetricExporter::builder()
        .with_http()
        .with_protocol(Protocol::HttpBinary)
        .build()?;
    let reader = PeriodicReader::builder(exporter).build();

    let mut resource = Resource::builder();
    if std::env::var_os("OTEL_SERVICE_NAME").is_none() {
        resource = resource.with_service_name(METER_NAME);
    }

    let provider = SdkMeterProvider::builder()
        .with_resource(resource.build())
        .with_reader(reader)
        .build();
    register_instruments(&provider.meter(METER_NAME), config.clone());
    Ok(Some(MetricsGuard { provider }))
}

/// reqwest is built with `rustls-no-provider` (no aws-lc-sys C build), so a
/// process-default rustls CryptoProvider must exist before the exporter builds
/// its HTTPS client. Install `ring`; a provider already installed is kept.
fn install_ring_provider() {
    if rustls::crypto::CryptoProvider::get_default().is_none() {
        let _ = rustls::crypto::ring::default_provider().install_default();
    }
}

fn key_attrs(tenant_id: &str, plugin_id: &str) -> [KeyValue; 2] {
    [
        KeyValue::new("tenant_id", tenant_id.to_string()),
        KeyValue::new("plugin_id", plugin_id.to_string()),
    ]
}

/// Register a counter that observes one field of the TASK-314 process counters.
fn counter_from_stats(
    meter: &Meter,
    name: &'static str,
    description: &'static str,
    config: BrokerConfig,
    field: fn(&Counters) -> u64,
) {
    meter
        .u64_observable_counter(name)
        .with_description(description)
        .with_unit(UNIT)
        .with_callback(move |obs| {
            for ((tenant, plugin), c) in config.hub.stats().counters() {
                obs.observe(field(&c), &key_attrs(&tenant, &plugin));
            }
        })
        .build();
}

/// Register a gauge that observes one field of the durable DB counts.
fn gauge_from_db(
    meter: &Meter,
    name: &'static str,
    description: &'static str,
    config: BrokerConfig,
    field: fn(&db::KeyCounts) -> u64,
) {
    meter
        .u64_observable_gauge(name)
        .with_description(description)
        .with_unit(UNIT)
        .with_callback(move |obs| match db::count_by_key(&config.db) {
            Ok(rows) => {
                for k in rows {
                    obs.observe(field(&k), &key_attrs(&k.tenant_id, &k.plugin_id));
                }
            }
            // Metrics must never take the broker down: skip this collection.
            Err(e) => tracing::debug!(error = %e, metric = name, "count_by_key failed"),
        })
        .build();
}

/// Register all broker instruments on `meter`. Callbacks run on the SDK's
/// collection thread and read [`crate::stats`] / [`db::count_by_key`].
pub fn register_instruments(meter: &Meter, config: BrokerConfig) {
    counter_from_stats(
        meter,
        RECEIVED,
        "Webhooks accepted (HTTP 202) by the ingress endpoint",
        config.clone(),
        |c| c.received,
    );
    counter_from_stats(
        meter,
        DROPPED,
        "Undelivered webhooks evicted by queue-cap overflow",
        config.clone(),
        |c| c.dropped,
    );
    counter_from_stats(
        meter,
        EXPIRED,
        "Undelivered webhooks removed by the TTL sweep",
        config.clone(),
        |c| c.expired,
    );

    let delivered_cfg = config.clone();
    meter
        .u64_observable_counter(DELIVERED)
        .with_description("Envelopes delivered to clients, by transport (ws | poll)")
        .with_unit(UNIT)
        .with_callback(move |obs| {
            for ((tenant, plugin), c) in delivered_cfg.hub.stats().counters() {
                let [t, p] = key_attrs(&tenant, &plugin);
                obs.observe(
                    c.delivered_ws,
                    &[t.clone(), p.clone(), KeyValue::new("transport", "ws")],
                );
                obs.observe(
                    c.delivered_poll,
                    &[t, p, KeyValue::new("transport", "poll")],
                );
            }
        })
        .build();

    gauge_from_db(
        meter,
        QUEUED,
        "Undelivered webhooks currently queued (durable)",
        config.clone(),
        |k| k.queued,
    );
    gauge_from_db(
        meter,
        ACKED,
        "Acknowledged webhooks still retained until TTL (durable)",
        config,
        |k| k.acked,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::sync::Arc;
    use std::time::Instant;

    use opentelemetry_sdk::metrics::data::{AggregatedMetrics, MetricData};
    use opentelemetry_sdk::metrics::InMemoryMetricExporter;

    use crate::dispatcher::Hub;
    use crate::queue::Webhook;

    fn test_config() -> (BrokerConfig, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let pool = db::init(dir.path().join("b.db").to_str().unwrap()).unwrap();
        let config = BrokerConfig {
            db: pool,
            hub: Arc::new(Hub::new()),
            start_time: Instant::now(),
            max_queue_size: 100,
            ws_heartbeat_secs: 30,
            poll_timeout_secs: 30,
            msg_ttl_secs: 3600,
        };
        (config, dir)
    }

    #[test]
    fn init_is_noop_without_endpoint() {
        let (config, _dir) = test_config();
        assert!(init(&config, None).unwrap().is_none());
        assert!(init(&config, Some("   ")).unwrap().is_none());
    }

    #[test]
    fn init_builds_exporter_with_ring_tls_provider() {
        // reqwest is compiled with `rustls-no-provider`: building the HTTPS
        // client must not panic for want of a CryptoProvider.
        let (config, _dir) = test_config();
        let guard = init(&config, Some("http://127.0.0.1:9"))
            .unwrap()
            .expect("exporter built when endpoint set");
        assert!(rustls::crypto::CryptoProvider::get_default().is_some());
        guard.shutdown();
    }

    #[test]
    fn endpoint_normalization() {
        assert_eq!(normalize_endpoint(None), None);
        assert_eq!(normalize_endpoint(Some(" ".into())), None);
        assert_eq!(
            normalize_endpoint(Some(" http://c:4318 ".into())).as_deref(),
            Some("http://c:4318")
        );
    }

    /// (metric name, sorted attrs) -> value
    type Points = BTreeMap<(String, Vec<(String, String)>), u64>;

    fn collect(exporter: &InMemoryMetricExporter) -> Points {
        let mut out = Points::new();
        let all = exporter.get_finished_metrics().unwrap();
        let rm = all.last().expect("one collection");
        for sm in rm.scope_metrics() {
            for m in sm.metrics() {
                let mut push = |attrs: Vec<(String, String)>, v: u64| {
                    let mut attrs = attrs;
                    attrs.sort();
                    out.insert((m.name().to_string(), attrs), v);
                };
                let to_vec = |it: &mut dyn Iterator<Item = &KeyValue>| {
                    it.map(|kv| (kv.key.to_string(), kv.value.to_string()))
                        .collect::<Vec<_>>()
                };
                match m.data() {
                    AggregatedMetrics::U64(MetricData::Sum(s)) => {
                        for dp in s.data_points() {
                            push(to_vec(&mut dp.attributes()), dp.value());
                        }
                    }
                    AggregatedMetrics::U64(MetricData::Gauge(g)) => {
                        for dp in g.data_points() {
                            push(to_vec(&mut dp.attributes()), dp.value());
                        }
                    }
                    other => panic!("unexpected aggregation for {}: {other:?}", m.name()),
                }
            }
        }
        out
    }

    fn key(name: &str, extra: &[(&str, &str)]) -> (String, Vec<(String, String)>) {
        let mut attrs: Vec<(String, String)> = vec![
            ("plugin_id".into(), "github".into()),
            ("tenant_id".into(), "acme".into()),
        ];
        attrs.extend(extra.iter().map(|(k, v)| (k.to_string(), v.to_string())));
        attrs.sort();
        (name.to_string(), attrs)
    }

    #[test]
    fn instruments_report_broker_stats() {
        let (config, _dir) = test_config();
        db::register_client(&config.db, "acme", "github", "s1", "poll", None).unwrap();
        for _ in 0..3 {
            let wh = Webhook::new("acme", "github", "push", serde_json::json!({}));
            db::insert_webhook(&config.db, &wh, 3600, 100).unwrap();
        }
        let stats = config.hub.stats();
        stats.record_received("acme", "github");
        stats.record_received("acme", "github");
        stats.record_delivered_ws("acme", "github", 2);
        stats.record_delivered_poll("acme", "github", 1);
        stats.record_dropped("acme", "github", 4);
        stats.record_expired("acme", "github", 5);

        let exporter = InMemoryMetricExporter::default();
        let provider = SdkMeterProvider::builder()
            .with_reader(PeriodicReader::builder(exporter.clone()).build())
            .build();
        register_instruments(&provider.meter(METER_NAME), config.clone());
        provider.force_flush().unwrap();

        let pts = collect(&exporter);
        assert_eq!(pts[&key(RECEIVED, &[])], 2);
        assert_eq!(pts[&key(DELIVERED, &[("transport", "ws")])], 2);
        assert_eq!(pts[&key(DELIVERED, &[("transport", "poll")])], 1);
        assert_eq!(pts[&key(DROPPED, &[])], 4);
        assert_eq!(pts[&key(EXPIRED, &[])], 5);
        assert_eq!(pts[&key(QUEUED, &[])], 3);
        assert_eq!(pts[&key(ACKED, &[])], 0);
        for name in METRIC_NAMES {
            assert!(pts.keys().any(|(n, _)| n == name), "missing {name}");
        }
        provider.shutdown().unwrap();
    }

    #[test]
    fn signoz_dashboard_references_every_metric() {
        let raw = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/deploy/signoz/broker-dashboard.json"
        ))
        .expect("dashboard file");
        let v: serde_json::Value = serde_json::from_str(&raw).expect("dashboard is valid JSON");
        // SigNoz v6 (Perses-style) dashboard: every layout item must point
        // at a defined panel.
        assert_eq!(v["schemaVersion"], "v6");
        let panels = v["spec"]["panels"].as_object().expect("spec.panels");
        let items = v["spec"]["layouts"][0]["spec"]["items"]
            .as_array()
            .expect("layout items");
        assert_eq!(items.len(), panels.len());
        for it in items {
            let r = it["content"]["$ref"].as_str().unwrap();
            let id = r.strip_prefix("#/spec/panels/").expect("panel ref");
            assert!(panels.contains_key(id), "dangling layout ref {r}");
        }
        for name in METRIC_NAMES {
            assert!(raw.contains(name), "dashboard does not reference {name}");
        }
    }
}
