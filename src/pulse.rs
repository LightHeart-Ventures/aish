//! Structured Pulse event broadcast for external observers (TASK-293).
//!
//! [`worker::classify_event`] already distills each coordinator-stderr line into
//! a [`Pulse`] (`ToolOk` / `ToolErr` / `Turn`) to colour-pulse the prompt badge.
//! Historically that signal died at the badge. This module re-publishes every
//! `Pulse` on a process-wide [`tokio::sync::broadcast`] channel so *external
//! observers* — monitors, dashboards, log aggregators, metrics collectors, the
//! built-in `:metrics` command — can subscribe and react to coordinator activity
//! without re-parsing stderr. It is the cheap enabler for observability the card
//! calls for.
//!
//! Design guarantees:
//! * **Never back-pressures the hot path.** A `broadcast` channel is lossy by
//!   design: a lagging receiver drops the oldest messages rather than blocking
//!   the sender. `Pulse` is a fire-and-forget liveness signal, so the stderr
//!   drain loop that publishes it must never stall on a slow subscriber.
//! * **Cheap when unobserved.** [`publish`] ignores the "no receivers" error, so
//!   the common case (no observer attached) is one atomic load + a send that
//!   early-returns.
//! * **Testable in isolation.** The channel and counters live on a [`PulseBus`]
//!   struct; production uses one lazily-initialised global instance, while unit
//!   tests spin up their own bus so shared global state never makes them flaky.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::OnceLock;
use tokio::sync::broadcast;

use crate::worker::Pulse;

/// Capacity of the broadcast ring. `Pulse` events are tiny and bursty; 1024 is a
/// comfortable buffer so a briefly-descheduled subscriber (e.g. the metrics
/// collector between polls) does not lag under a tool-call storm.
const CHANNEL_CAP: usize = 1024;

/// A snapshot of the Pulse counters at one instant. Cheap to copy.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PulseCounts {
    /// Successful tool calls observed (`Pulse::ToolOk`).
    pub tool_ok: u64,
    /// Failed tool calls observed (`Pulse::ToolErr`).
    pub tool_err: u64,
    /// Turn/narration lines observed (`Pulse::Turn`).
    pub turn: u64,
    /// Events the collector missed because it lagged the broadcast ring.
    pub lagged: u64,
}

impl PulseCounts {
    /// Total classified Pulse events observed (excludes `lagged` drops).
    pub fn total(&self) -> u64 {
        self.tool_ok + self.tool_err + self.turn
    }
}

/// The broadcast channel plus the metrics counters a built-in subscriber folds
/// events into. Production runs a single global instance ([`global`]); tests
/// construct their own so they never race on shared statics.
pub struct PulseBus {
    tx: broadcast::Sender<Pulse>,
    tool_ok: AtomicU64,
    tool_err: AtomicU64,
    turn: AtomicU64,
    lagged: AtomicU64,
    collector_started: AtomicBool,
}

impl PulseBus {
    fn new() -> Self {
        let (tx, _rx) = broadcast::channel(CHANNEL_CAP);
        PulseBus {
            tx,
            tool_ok: AtomicU64::new(0),
            tool_err: AtomicU64::new(0),
            turn: AtomicU64::new(0),
            lagged: AtomicU64::new(0),
            collector_started: AtomicBool::new(false),
        }
    }

    /// Publish one Pulse to every subscriber. Infallible from the caller's view:
    /// when no observer is attached `send` returns `Err(SendError)`, which we
    /// deliberately ignore — a badge-driving Pulse with no subscriber is normal,
    /// not an error.
    pub fn publish(&self, pulse: Pulse) {
        let _ = self.tx.send(pulse);
    }

    /// Subscribe to the Pulse broadcast. Each receiver sees every Pulse published
    /// after it subscribed (lossy under lag — see module docs).
    pub fn subscribe(&self) -> broadcast::Receiver<Pulse> {
        self.tx.subscribe()
    }

    /// Fold one Pulse into this bus's counters. This is the body of the built-in
    /// metrics subscriber, factored out so tests can drive it synchronously.
    fn record(&self, pulse: Pulse) {
        match pulse {
            Pulse::ToolOk => &self.tool_ok,
            Pulse::ToolErr => &self.tool_err,
            Pulse::Turn => &self.turn,
        }
        .fetch_add(1, Ordering::Relaxed);
    }

    /// Read the current counters as a snapshot.
    pub fn counts(&self) -> PulseCounts {
        PulseCounts {
            tool_ok: self.tool_ok.load(Ordering::Relaxed),
            tool_err: self.tool_err.load(Ordering::Relaxed),
            turn: self.turn.load(Ordering::Relaxed),
            lagged: self.lagged.load(Ordering::Relaxed),
        }
    }

    /// Spawn the built-in metrics collector: a detached task that subscribes to
    /// the broadcast and folds every event into this bus's counters. Idempotent —
    /// only the first call spawns a task. Requires `&'static self` because the
    /// task outlives the call; the production global satisfies this.
    pub fn spawn_collector(&'static self) {
        if self.collector_started.swap(true, Ordering::SeqCst) {
            return; // already running
        }
        let mut rx = self.subscribe();
        tokio::spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(pulse) => self.record(pulse),
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        self.lagged.fetch_add(n, Ordering::Relaxed);
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        });
    }
}

/// The process-wide bus. Lazily initialised on first use.
fn global() -> &'static PulseBus {
    static BUS: OnceLock<PulseBus> = OnceLock::new();
    BUS.get_or_init(PulseBus::new)
}

/// Publish one Pulse on the global broadcast (see [`PulseBus::publish`]).
pub fn publish(pulse: Pulse) {
    global().publish(pulse);
}

/// Subscribe to the global Pulse broadcast (see [`PulseBus::subscribe`]). This is
/// the public entrypoint for *external* observers (monitors, dashboards, log
/// aggregators) — the built-in metrics collector uses `PulseBus::subscribe`
/// directly, so this free function has no in-crate caller by design.
#[allow(dead_code)]
pub fn subscribe() -> broadcast::Receiver<Pulse> {
    global().subscribe()
}

/// Read the global Pulse counters (what `:metrics` displays).
pub fn counts() -> PulseCounts {
    global().counts()
}

/// Start the built-in metrics collector on the global bus. Call once at startup;
/// idempotent thereafter.
pub fn start_metrics_collector() {
    global().spawn_collector();
}

/// Render the `:metrics` report — a compact human summary of Pulse activity.
/// Pure (no I/O, no globals), so it is directly unit-testable.
pub fn render_report(counts: &PulseCounts) -> String {
    use std::fmt::Write as _;
    let mut s = String::new();
    let _ = writeln!(s, "\x1b[1mcoordinator pulse metrics\x1b[0m");
    let _ = writeln!(s, "  \x1b[32m✓\x1b[0m tool ok   {}", counts.tool_ok);
    let _ = writeln!(s, "  \x1b[31m✗\x1b[0m tool err  {}", counts.tool_err);
    let _ = writeln!(s, "  \x1b[1;35m⟳\x1b[0m turns     {}", counts.turn);
    let _ = writeln!(s, "  Σ total     {}", counts.total());
    if counts.lagged > 0 {
        let _ = writeln!(
            s,
            "  \x1b[33m⚠\x1b[0m dropped   {} (collector lag)",
            counts.lagged
        );
    }
    if counts.total() == 0 {
        let _ = writeln!(s, "\n  no coordinator activity observed yet");
    }
    s.trim_end().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::worker::Pulse;

    #[test]
    fn total_sums_the_three_event_kinds_and_excludes_lag() {
        let c = PulseCounts {
            tool_ok: 3,
            tool_err: 2,
            turn: 5,
            lagged: 9,
        };
        assert_eq!(c.total(), 10);
    }

    #[test]
    fn record_increments_only_the_matching_counter() {
        let bus = PulseBus::new();
        bus.record(Pulse::ToolOk);
        bus.record(Pulse::ToolOk);
        bus.record(Pulse::ToolErr);
        bus.record(Pulse::Turn);
        let c = bus.counts();
        assert_eq!(c.tool_ok, 2);
        assert_eq!(c.tool_err, 1);
        assert_eq!(c.turn, 1);
        assert_eq!(c.lagged, 0);
        assert_eq!(c.total(), 4);
    }

    #[tokio::test]
    async fn publish_reaches_a_subscriber() {
        let bus = PulseBus::new();
        let mut rx = bus.subscribe();
        bus.publish(Pulse::ToolOk);
        bus.publish(Pulse::Turn);
        assert_eq!(rx.recv().await.unwrap(), Pulse::ToolOk);
        assert_eq!(rx.recv().await.unwrap(), Pulse::Turn);
    }

    #[tokio::test]
    async fn publish_with_no_subscriber_is_a_noop() {
        let bus = PulseBus::new();
        // No receiver attached — send returns Err internally but publish swallows it.
        bus.publish(Pulse::ToolErr);
        // A subscriber attached *after* the publish sees only later events (the
        // broadcast is not replayed), proving publish never panics or blocks.
        let mut rx = bus.subscribe();
        bus.publish(Pulse::Turn);
        assert_eq!(rx.recv().await.unwrap(), Pulse::Turn);
    }

    #[test]
    fn render_report_shows_all_counters() {
        let out = render_report(&PulseCounts {
            tool_ok: 7,
            tool_err: 1,
            turn: 4,
            lagged: 0,
        });
        assert!(out.contains("tool ok   7"));
        assert!(out.contains("tool err  1"));
        assert!(out.contains("turns     4"));
        assert!(out.contains("total     12"));
        assert!(!out.contains("dropped"));
    }

    #[test]
    fn render_report_notes_lag_and_idle() {
        let lagged = render_report(&PulseCounts {
            tool_ok: 0,
            tool_err: 0,
            turn: 0,
            lagged: 3,
        });
        assert!(lagged.contains("dropped   3"));

        let idle = render_report(&PulseCounts::default());
        assert!(idle.contains("no coordinator activity observed yet"));
    }
}
