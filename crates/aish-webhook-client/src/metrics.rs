//! TASK-375 — client-side handler counters (per-plugin health).
//!
//! [`HandlerCounters`] tallies every handler the [`crate::WebhookDispatcher`]
//! actually executed: dispatched / ok / failed / timeout plus total run time,
//! keyed by plugin id. Attach it with
//! [`crate::WebhookDispatcher::with_counters`]; aish surfaces it in
//! `:webhook status`. Filtered-out (never executed) handlers are not counted.

use std::collections::BTreeMap;
use std::sync::{Mutex, MutexGuard};

use serde::Serialize;

use crate::dispatcher::HandlerOutcome;

/// Counts for one plugin (or the totals across plugins).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct HandlerCounts {
    /// Handlers fork/exec'd (or attempted: spawn failures count here too).
    pub dispatched: u64,
    /// Exited 0.
    pub ok: u64,
    /// Non-zero exit, spawn/wait failure.
    pub failed: u64,
    /// Killed after exceeding the handler timeout.
    pub timeout: u64,
    /// Sum of handler run times (ms), for the average.
    pub total_ms: u64,
}

impl HandlerCounts {
    /// Mean handler run time in ms (0 when nothing ran).
    pub fn avg_ms(&self) -> u64 {
        self.total_ms.checked_div(self.dispatched).unwrap_or(0)
    }

    fn add(&mut self, o: &HandlerCounts) {
        self.dispatched += o.dispatched;
        self.ok += o.ok;
        self.failed += o.failed;
        self.timeout += o.timeout;
        self.total_ms = self.total_ms.saturating_add(o.total_ms);
    }
}

/// True when the outcome is a handler timeout (see `run_handler`).
pub fn is_timeout(o: &HandlerOutcome) -> bool {
    o.error
        .as_deref()
        .is_some_and(|e| e.starts_with("timed out"))
}

/// Thread-safe per-plugin handler counters.
#[derive(Debug, Default)]
pub struct HandlerCounters {
    inner: Mutex<BTreeMap<String, HandlerCounts>>,
}

impl HandlerCounters {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record one dispatch outcome. Non-executed (filtered) outcomes are ignored.
    pub fn record(&self, o: &HandlerOutcome) {
        let attempted = o.executed || o.error.is_some();
        if !attempted {
            return;
        }
        let mut map = self.lock();
        let c = map.entry(o.plugin_id.clone()).or_default();
        c.dispatched += 1;
        c.total_ms = c
            .total_ms
            .saturating_add(u64::try_from(o.duration_ms).unwrap_or(u64::MAX));
        if o.success {
            c.ok += 1;
        } else if is_timeout(o) {
            c.timeout += 1;
        } else {
            c.failed += 1;
        }
    }

    /// Snapshot of per-plugin counts, sorted by plugin id.
    pub fn per_plugin(&self) -> BTreeMap<String, HandlerCounts> {
        self.lock().clone()
    }

    /// Sum across all plugins.
    pub fn totals(&self) -> HandlerCounts {
        let mut t = HandlerCounts::default();
        for c in self.lock().values() {
            t.add(c);
        }
        t
    }

    /// Counters must never take dispatch down: recover from a poisoned lock.
    fn lock(&self) -> MutexGuard<'_, BTreeMap<String, HandlerCounts>> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn outcome(plugin: &str, executed: bool, success: bool, error: Option<&str>) -> HandlerOutcome {
        HandlerOutcome {
            plugin_id: plugin.into(),
            event_type: "push".into(),
            matched: true,
            executed,
            exit_code: if success { Some(0) } else { None },
            success,
            stdout: String::new(),
            stderr: String::new(),
            error: error.map(str::to_string),
            duration_ms: 10,
        }
    }

    #[test]
    fn records_ok_failed_timeout_and_skips_filtered() {
        let c = HandlerCounters::new();
        c.record(&outcome("a", true, true, None));
        c.record(&outcome("a", true, false, None));
        c.record(&outcome("a", true, false, Some("timed out after 1s")));
        c.record(&outcome("b", false, false, None)); // filtered: not counted
        c.record(&outcome("b", true, false, Some("spawn failed: nope")));

        let per = c.per_plugin();
        let a = per["a"];
        assert_eq!((a.dispatched, a.ok, a.failed, a.timeout), (3, 1, 1, 1));
        assert_eq!(a.avg_ms(), 10);
        let b = per["b"];
        assert_eq!((b.dispatched, b.ok, b.failed, b.timeout), (1, 0, 1, 0));

        let t = c.totals();
        assert_eq!((t.dispatched, t.ok, t.failed, t.timeout), (4, 1, 2, 1));
        assert_eq!(HandlerCounts::default().avg_ms(), 0);
    }

    #[tokio::test]
    async fn dispatcher_records_into_attached_counters() {
        use crate::dispatcher::{
            PluginManifest, PluginRegistry, WebhookDispatcher, WebhookHandler,
        };
        use std::sync::Arc;
        use std::time::Duration;

        let handler = |cmd: &[&str], filter: Option<(&str, &str)>| WebhookHandler {
            event_type: "push".into(),
            command: cmd.iter().map(|s| s.to_string()).collect(),
            filters: filter
                .map(|(k, v)| {
                    let mut m = serde_json::Map::new();
                    m.insert(k.into(), serde_json::json!(v));
                    m
                })
                .unwrap_or_default(),
            timeout_secs: None,
        };
        let plugin = |id: &str, webhooks| PluginManifest {
            id: id.into(),
            name: String::new(),
            version: String::new(),
            webhooks,
        };
        let reg = PluginRegistry::from_plugins(vec![
            plugin("ok", vec![handler(&["true"], None)]),
            plugin("bad", vec![handler(&["false"], None)]),
            plugin("slow", vec![handler(&["sleep", "5"], None)]),
            plugin("skip", vec![handler(&["true"], Some(("action", "x")))]),
        ]);
        let counters = Arc::new(HandlerCounters::new());
        let d = WebhookDispatcher::new(Arc::new(reg))
            .with_default_timeout(Duration::from_millis(150))
            .with_counters(counters.clone());
        let wh = crate::envelope::Webhook {
            id: "d1".into(),
            tenant_id: "t".into(),
            plugin_id: String::new(),
            event_type: "push".into(),
            payload: serde_json::json!({}),
        };
        d.dispatch(&wh).await;

        let per = counters.per_plugin();
        assert_eq!(per["ok"].ok, 1);
        assert_eq!(per["bad"].failed, 1);
        assert_eq!(per["slow"].timeout, 1);
        assert!(!per.contains_key("skip"), "filtered handler not counted");
        assert_eq!(counters.totals().dispatched, 3);
    }
}
