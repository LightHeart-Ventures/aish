//! Unified operator-control channel for the background coordinator (TASK-297).
//!
//! Historically the coordinator had **three separate** operator-control paths,
//! each with its own transport and its own ad-hoc handling site in the `drive`
//! loop (steps 12-14 of the coordinator build-out):
//!
//!   1. **`:tell` steer messages** — durable, delivered via the SQLite
//!      `coordinator_runs` mailbox (`CoordinatorStore::drain_messages`) and
//!      folded into the next round's input.
//!   2. **Ctrl-C interrupt latch** — a process-global `AtomicBool`
//!      (`coordinator::request_interrupt` / `take_interrupt`) set by the SIGINT
//!      handler and read at the engine turn seam / round boundary.
//!   3. **Live-stream (`:worker-output`) toggle** — the tri-state
//!      [`WorkerOutputMode`] that gates whether a coordinator's activity is
//!      forwarded to the operator's terminal.
//!
//! Three transports, three handling sites, three chances for an ordering race.
//! This module collapses them into ONE prioritized queue of typed
//! [`ControlSignal`]s. The `drive` loop normalizes each heterogeneous source
//! into the channel at the round boundary and then drains it **once, in strict
//! priority order** (`interrupt > steer > output-mode`), so there is a single
//! place that decides what the operator asked for and in what order it takes
//! effect. New steering signals (pause, re-scope, model swap, …) become a new
//! enum variant + priority rank rather than a fourth bespoke mechanism.
//!
//! ## Why an `mpsc` and not just a `Vec`
//! The channel is a genuine [`tokio::sync::mpsc`] so producers can live in other
//! tasks/threads — the SIGINT handler task, a future cross-process control
//! socket, or the parent pushing an output-mode change — and hand a signal to
//! the coordinator without sharing mutable state. The [`ControlSender`] is
//! `Clone + Send + 'static`, so any number of async producers can hold one. The
//! coordinator is the sole consumer and drains the queue at each round boundary
//! ([`ControlChannel::drain_prioritized`]).
//!
//! ## Priority semantics
//! Priorities describe **precedence**, not literal text order. When several
//! signals are pending in one round the consumer applies them highest-first:
//! an interrupt supersedes a steer (it replaces the pending continuation with a
//! reassess directive), a steer augments the round (folded as context), and an
//! output-mode change is a side effect (weakest). `drain_prioritized` returns
//! the drained signals already sorted highest-priority-first, with FIFO order
//! preserved **within** a priority tier (a stable sort), so two `:tell`s arrive
//! in the order they were sent.

use crate::worker::WorkerOutputMode;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};

/// A single normalized operator-control signal on the unified channel.
///
/// The variants are ordered by declaration to mirror their priority, but the
/// authoritative ranking is [`ControlSignal::priority`] (used by the drain sort)
/// — do not rely on `derive`d ordering.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ControlSignal {
    /// Operator pressed Ctrl-C: interrupt the current line of work and reassess.
    /// Highest priority — it supersedes any pending steer/continuation.
    Interrupt,
    /// Operator sent a `:tell` steer message to fold into the next round.
    Steer(String),
    /// Operator flipped the live-stream (`:worker-output`) mode. Lowest priority
    /// — a display side effect that never preempts real work.
    OutputMode(WorkerOutputMode),
}

impl ControlSignal {
    /// Precedence rank — **lower is higher priority**. `interrupt(0) >
    /// steer(1) > output-mode(2)` per TASK-297 AC2. Used as the stable-sort key
    /// in [`ControlChannel::drain_prioritized`].
    pub fn priority(&self) -> u8 {
        match self {
            ControlSignal::Interrupt => 0,
            ControlSignal::Steer(_) => 1,
            ControlSignal::OutputMode(_) => 2,
        }
    }

    /// Short human-readable tag for dim operator-facing markers / logs.
    #[allow(dead_code)] // forward-looking: dim markers / structured control logs
    pub fn tag(&self) -> &'static str {
        match self {
            ControlSignal::Interrupt => "interrupt",
            ControlSignal::Steer(_) => "steer",
            ControlSignal::OutputMode(_) => "output-mode",
        }
    }
}

/// A cloneable producer handle onto the unified control channel.
///
/// `Clone + Send + 'static`, so it can be moved into the SIGINT handler task, a
/// future control socket, or any other async producer. Every `send*` is
/// non-blocking (unbounded channel) and best-effort: a send after the consumer
/// (the `drive` loop) has dropped the receiver returns `false` rather than
/// panicking, because at that point the run is already terminating and the
/// signal is moot.
#[derive(Clone)]
pub struct ControlSender {
    tx: UnboundedSender<ControlSignal>,
}

impl ControlSender {
    /// Push an arbitrary signal. Returns `false` if the receiver is gone.
    pub fn send(&self, signal: ControlSignal) -> bool {
        self.tx.send(signal).is_ok()
    }

    /// Enqueue an operator interrupt (Ctrl-C). Returns `false` if the receiver
    /// is gone.
    pub fn interrupt(&self) -> bool {
        self.send(ControlSignal::Interrupt)
    }

    /// Enqueue a `:tell` steer message. Returns `false` if the receiver is gone.
    pub fn steer(&self, message: impl Into<String>) -> bool {
        self.send(ControlSignal::Steer(message.into()))
    }

    /// Enqueue a live-stream (`:worker-output`) mode change. Returns `false` if
    /// the receiver is gone.
    pub fn output_mode(&self, mode: WorkerOutputMode) -> bool {
        self.send(ControlSignal::OutputMode(mode))
    }
}

/// The unified control channel: an unbounded mpsc plus the single consuming
/// receiver held by the coordinator's `drive` loop.
pub struct ControlChannel {
    tx: UnboundedSender<ControlSignal>,
    rx: UnboundedReceiver<ControlSignal>,
}

impl Default for ControlChannel {
    fn default() -> Self {
        Self::new()
    }
}

impl ControlChannel {
    /// Create an empty channel. The coordinator owns the [`ControlChannel`] and
    /// hands out [`ControlSender`] clones to producers via [`Self::sender`].
    pub fn new() -> Self {
        let (tx, rx) = unbounded_channel();
        Self { tx, rx }
    }

    /// Mint a cloneable producer handle. Call once per producer (SIGINT task,
    /// control socket, …); the handle itself is `Clone` for further fan-out.
    pub fn sender(&self) -> ControlSender {
        ControlSender {
            tx: self.tx.clone(),
        }
    }

    /// Non-blocking drain of every currently-queued signal, returned in strict
    /// priority order (`interrupt > steer > output-mode`) with FIFO order
    /// preserved within each priority tier.
    ///
    /// Called at the round boundary: the coordinator normalizes its
    /// heterogeneous sources (interrupt latch, `:tell` mailbox, output-mode)
    /// into the channel, then drains here and dispatches the result. Signals
    /// that arrive *after* this call wait for the next boundary — delivery is
    /// round-boundary by design so a mid-turn message can't tear a turn.
    pub fn drain_prioritized(&mut self) -> Vec<ControlSignal> {
        let mut drained = Vec::new();
        // try_recv never awaits; loop until the queue is momentarily empty.
        while let Ok(signal) = self.rx.try_recv() {
            drained.push(signal);
        }
        // Stable sort by precedence keeps within-tier FIFO (two :tells stay in
        // send order) while ordering tiers highest-priority-first.
        drained.sort_by_key(|s| s.priority());
        drained
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn priority_ranks_interrupt_over_steer_over_output_mode() {
        assert!(ControlSignal::Interrupt.priority() < ControlSignal::Steer(String::new()).priority());
        assert!(
            ControlSignal::Steer(String::new()).priority()
                < ControlSignal::OutputMode(WorkerOutputMode::Off).priority()
        );
    }

    #[test]
    fn drain_orders_by_priority_regardless_of_send_order() {
        let mut ch = ControlChannel::new();
        let tx = ch.sender();
        // Send lowest-priority first, interrupt last.
        assert!(tx.output_mode(WorkerOutputMode::On));
        assert!(tx.steer("do X"));
        assert!(tx.interrupt());

        let drained = ch.drain_prioritized();
        assert_eq!(
            drained,
            vec![
                ControlSignal::Interrupt,
                ControlSignal::Steer("do X".to_string()),
                ControlSignal::OutputMode(WorkerOutputMode::On),
            ]
        );
    }

    #[test]
    fn drain_preserves_fifo_within_a_priority_tier() {
        let mut ch = ControlChannel::new();
        let tx = ch.sender();
        tx.steer("first");
        tx.steer("second");
        tx.steer("third");

        let drained = ch.drain_prioritized();
        assert_eq!(
            drained,
            vec![
                ControlSignal::Steer("first".to_string()),
                ControlSignal::Steer("second".to_string()),
                ControlSignal::Steer("third".to_string()),
            ]
        );
    }

    #[test]
    fn interrupt_precedes_a_batch_of_steers_but_steers_keep_order() {
        let mut ch = ControlChannel::new();
        let tx = ch.sender();
        tx.steer("a");
        tx.interrupt();
        tx.steer("b");

        let drained = ch.drain_prioritized();
        assert_eq!(
            drained,
            vec![
                ControlSignal::Interrupt,
                ControlSignal::Steer("a".to_string()),
                ControlSignal::Steer("b".to_string()),
            ]
        );
    }

    #[test]
    fn empty_channel_drains_to_nothing() {
        let mut ch = ControlChannel::new();
        assert!(ch.drain_prioritized().is_empty());
    }

    #[test]
    fn multiple_drains_are_independent() {
        let mut ch = ControlChannel::new();
        let tx = ch.sender();
        tx.interrupt();
        assert_eq!(ch.drain_prioritized(), vec![ControlSignal::Interrupt]);
        // Nothing left after the first drain.
        assert!(ch.drain_prioritized().is_empty());
        // Channel is reusable for the next round.
        tx.steer("later");
        assert_eq!(
            ch.drain_prioritized(),
            vec![ControlSignal::Steer("later".to_string())]
        );
    }

    #[test]
    fn cloned_senders_feed_the_same_queue() {
        let mut ch = ControlChannel::new();
        let a = ch.sender();
        let b = a.clone();
        a.steer("from-a");
        b.steer("from-b");
        assert_eq!(ch.drain_prioritized().len(), 2);
    }

    #[test]
    fn send_after_receiver_dropped_is_false_not_panic() {
        let tx = {
            let ch = ControlChannel::new();
            ch.sender()
        }; // ch (and its receiver) dropped here.
        assert!(!tx.interrupt());
    }

    #[test]
    fn tags_are_stable() {
        assert_eq!(ControlSignal::Interrupt.tag(), "interrupt");
        assert_eq!(ControlSignal::Steer(String::new()).tag(), "steer");
        assert_eq!(
            ControlSignal::OutputMode(WorkerOutputMode::Off).tag(),
            "output-mode"
        );
    }
}
