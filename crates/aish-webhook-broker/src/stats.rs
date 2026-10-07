//! Broker delivery statistics (`GET /stats`).
//!
//! Two sources are combined into one [`StatsSnapshot`]:
//!
//! * **Durable gauges** read from SQLite on demand ([`crate::db::count_by_key`]):
//!   `queued` (undelivered rows) and `acked` (acknowledged rows still retained
//!   until their TTL). These survive restarts and cover acks from both the HTTP
//!   and WebSocket paths.
//! * **Process counters** kept in memory by [`BrokerStats`] (owned by the
//!   dispatch [`crate::dispatcher::Hub`]): `received`, `delivered_ws`,
//!   `delivered_poll`, `dropped` (queue-cap overflow) and `expired` (TTL sweep of
//!   undelivered rows). These are monotonic since [`BrokerStats::started_at`]
//!   and reset when the process restarts.
//!
//! The snapshot carries only counts and the `(tenant_id, plugin_id)` routing
//! keys — never payloads, secrets, client ids or session tokens. Metric
//! exporters (TASK-375) should read [`BrokerStats::counters`] and
//! [`crate::db::count_by_key`], or simply call [`snapshot`].

use std::collections::BTreeMap;
use std::sync::{Mutex, MutexGuard};

use chrono::{DateTime, Utc};
use serde::Serialize;

use crate::config::BrokerConfig;
use crate::db::{self, KeyCounts};
use crate::error::Result;

/// `(tenant_id, plugin_id)` routing key.
pub type Key = (String, String);

/// Per-key process counters (monotonic since process start).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Counters {
    /// Webhooks accepted (HTTP 202) by the ingress endpoint.
    pub received: u64,
    /// Envelopes pushed to live WebSocket clients by the dispatch fast path.
    pub delivered_ws: u64,
    /// Envelopes returned to long-poll clients by `GET .../pending`.
    pub delivered_poll: u64,
    /// Undelivered webhooks evicted by queue-cap overflow.
    pub dropped: u64,
    /// Undelivered webhooks removed by the TTL sweep.
    pub expired: u64,
}

/// Thread-safe, in-memory per-key counters. Cheap to update from handlers.
#[derive(Debug)]
pub struct BrokerStats {
    started_at: DateTime<Utc>,
    inner: Mutex<BTreeMap<Key, Counters>>,
}

impl Default for BrokerStats {
    fn default() -> Self {
        Self::new()
    }
}

impl BrokerStats {
    pub fn new() -> Self {
        Self {
            started_at: Utc::now(),
            inner: Mutex::new(BTreeMap::new()),
        }
    }

    /// When the process counters started accumulating.
    pub fn started_at(&self) -> DateTime<Utc> {
        self.started_at
    }

    pub fn record_received(&self, tenant_id: &str, plugin_id: &str) {
        self.add(tenant_id, plugin_id, 1, |c, n| c.received += n);
    }

    pub fn record_delivered_ws(&self, tenant_id: &str, plugin_id: &str, n: u64) {
        self.add(tenant_id, plugin_id, n, |c, n| c.delivered_ws += n);
    }

    pub fn record_delivered_poll(&self, tenant_id: &str, plugin_id: &str, n: u64) {
        self.add(tenant_id, plugin_id, n, |c, n| c.delivered_poll += n);
    }

    pub fn record_dropped(&self, tenant_id: &str, plugin_id: &str, n: u64) {
        self.add(tenant_id, plugin_id, n, |c, n| c.dropped += n);
    }

    pub fn record_expired(&self, tenant_id: &str, plugin_id: &str, n: u64) {
        self.add(tenant_id, plugin_id, n, |c, n| c.expired += n);
    }

    /// Point-in-time copy of all per-key counters, sorted by key.
    pub fn counters(&self) -> BTreeMap<Key, Counters> {
        self.lock().clone()
    }

    fn add(&self, tenant_id: &str, plugin_id: &str, n: u64, f: impl FnOnce(&mut Counters, u64)) {
        if n == 0 {
            return;
        }
        let mut map = self.lock();
        let entry = map
            .entry((tenant_id.to_string(), plugin_id.to_string()))
            .or_default();
        f(entry, n);
    }

    /// Stats must never take a handler down: recover from a poisoned lock.
    fn lock(&self) -> MutexGuard<'_, BTreeMap<Key, Counters>> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// Counts for one `(tenant_id, plugin_id)`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct PluginStats {
    pub tenant_id: String,
    pub plugin_id: String,
    pub queued: u64,
    pub acked: u64,
    pub delivered: u64,
    pub delivered_ws: u64,
    pub delivered_poll: u64,
    pub received: u64,
    pub dropped: u64,
    pub expired: u64,
}

/// Sum of every [`PluginStats`] entry.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct StatsTotals {
    pub queued: u64,
    pub acked: u64,
    pub delivered: u64,
    pub delivered_ws: u64,
    pub delivered_poll: u64,
    pub received: u64,
    pub dropped: u64,
    pub expired: u64,
}

impl StatsTotals {
    fn add(&mut self, p: &PluginStats) {
        self.queued = self.queued.saturating_add(p.queued);
        self.acked = self.acked.saturating_add(p.acked);
        self.delivered = self.delivered.saturating_add(p.delivered);
        self.delivered_ws = self.delivered_ws.saturating_add(p.delivered_ws);
        self.delivered_poll = self.delivered_poll.saturating_add(p.delivered_poll);
        self.received = self.received.saturating_add(p.received);
        self.dropped = self.dropped.saturating_add(p.dropped);
        self.expired = self.expired.saturating_add(p.expired);
    }
}

/// The `GET /stats` response body.
#[derive(Clone, Debug, Serialize)]
pub struct StatsSnapshot {
    pub generated_at: String,
    pub uptime_secs: u64,
    /// Start of the process-counter window (RFC 3339).
    pub counters_since: String,
    pub totals: StatsTotals,
    pub plugins: Vec<PluginStats>,
}

/// Merge durable DB gauges with process counters into sorted per-key entries
/// plus totals. Keys present in either source are included.
pub fn merge(
    db_counts: Vec<KeyCounts>,
    counters: BTreeMap<Key, Counters>,
) -> (Vec<PluginStats>, StatsTotals) {
    let mut by_key: BTreeMap<Key, PluginStats> = BTreeMap::new();
    for k in db_counts {
        let e = by_key
            .entry((k.tenant_id.clone(), k.plugin_id.clone()))
            .or_default();
        e.queued = k.queued;
        e.acked = k.acked;
    }
    for (key, c) in counters {
        let e = by_key.entry(key).or_default();
        e.received = c.received;
        e.delivered_ws = c.delivered_ws;
        e.delivered_poll = c.delivered_poll;
        e.dropped = c.dropped;
        e.expired = c.expired;
    }

    let mut totals = StatsTotals::default();
    let plugins = by_key
        .into_iter()
        .map(|((tenant_id, plugin_id), mut p)| {
            p.tenant_id = tenant_id;
            p.plugin_id = plugin_id;
            p.delivered = p.delivered_ws.saturating_add(p.delivered_poll);
            totals.add(&p);
            p
        })
        .collect();
    (plugins, totals)
}

/// Build a full snapshot for the broker described by `config`.
pub fn snapshot(config: &BrokerConfig) -> Result<StatsSnapshot> {
    let stats = config.hub.stats();
    let db_counts = db::count_by_key(&config.db)?;
    let (plugins, totals) = merge(db_counts, stats.counters());
    Ok(StatsSnapshot {
        generated_at: Utc::now().to_rfc3339(),
        uptime_secs: config.start_time.elapsed().as_secs(),
        counters_since: stats.started_at().to_rfc3339(),
        totals,
        plugins,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kc(t: &str, p: &str, queued: u64, acked: u64) -> KeyCounts {
        KeyCounts {
            tenant_id: t.into(),
            plugin_id: p.into(),
            queued,
            acked,
        }
    }

    #[test]
    fn zero_increments_do_not_create_keys() {
        let s = BrokerStats::new();
        s.record_delivered_ws("t", "p", 0);
        s.record_dropped("t", "p", 0);
        assert!(s.counters().is_empty());
        s.record_received("t", "p");
        s.record_dropped("t", "p", 2);
        let c = s.counters()[&("t".to_string(), "p".to_string())];
        assert_eq!(c.received, 1);
        assert_eq!(c.dropped, 2);
    }

    #[test]
    fn merge_unions_sorts_and_totals() {
        let s = BrokerStats::new();
        s.record_received("b", "slack");
        s.record_delivered_ws("b", "slack", 1);
        s.record_delivered_poll("b", "slack", 2);
        s.record_expired("z", "only-counters", 4);
        let (plugins, totals) = merge(
            vec![kc("b", "slack", 1, 2), kc("a", "github", 3, 0)],
            s.counters(),
        );
        let keys: Vec<_> = plugins
            .iter()
            .map(|p| (p.tenant_id.as_str(), p.plugin_id.as_str()))
            .collect();
        assert_eq!(
            keys,
            vec![("a", "github"), ("b", "slack"), ("z", "only-counters")]
        );
        assert_eq!(plugins[1].delivered, 3);
        assert_eq!(plugins[1].queued, 1);
        assert_eq!(plugins[1].acked, 2);
        assert_eq!(plugins[2].expired, 4);
        assert_eq!(totals.queued, 4);
        assert_eq!(totals.acked, 2);
        assert_eq!(totals.delivered, 3);
        assert_eq!(totals.received, 1);
        assert_eq!(totals.expired, 4);
    }
}
