//! An explicit, validated transition table for the worker / coordinator
//! lifecycle (`stateflow`-inspired).
//!
//! ## Why this exists
//! aish's worker lifecycle — *spawn → running → soft-fail → recover / nudge /
//! flag → done* — was, until this module, **implicit convention** scattered
//! across [`crate::coordinator`] (the `Phase` enum + the auto-recovery loop) and
//! [`crate::worker_store`] (a free-form `status` string of `running` / `done` /
//! `failed`). Nothing declared which edges were *legal*, so a recovery-path bug
//! (e.g. resuming a run that was already flagged, or completing after failing)
//! could only be caught by reading the driver by hand.
//!
//! Borrowing the one real virtue of a `stateflow`-style machine: the legal
//! transitions are **declared once** as a table and **validated** —
//!   * no duplicate state / event definitions,
//!   * every transition's `from`/`to` state exists,
//!   * every transition names a real, non-empty event,
//!   * no duplicate `(from, event)` edge (the machine stays deterministic).
//! Illegal edges are then *rejected* — [`next`] returns `None` — and the whole
//! table can print its own [`TransitionTable::diagram`] for `:workers`.
//!
//! ## Relationship to the existing types (this is a *lens*, not a rewrite)
//! This module does not replace [`crate::coordinator::Phase`],
//! [`crate::loopguard::Disposition`], or the `worker_store` status string — it
//! is the formal *lifecycle lens* over them, with total bridges
//! ([`LifecycleState::from_phase`], [`LifecycleState::from_worker_status`],
//! [`LifecycleEvent::from_disposition`]) so the table is grounded in the real
//! runtime signals rather than floating beside them. The bridges are unit-tested
//! against every arm of the source enums, so if a new `Phase` or `Disposition`
//! variant is added the exhaustive `match` here fails to compile — a compile-time
//! tripwire that the lifecycle map is kept honest.

// The bridge/query surface below (`from_phase`, `from_worker_status`,
// `from_disposition`, `next`, `is_legal`, the accessors) is exercised by this
// module's unit tests today and is the wiring surface for the coordinator /
// worker drivers next — it is deliberately not yet called from the shipping
// binary, so suppress dead-code noise in non-test builds only. Test builds keep
// full dead-code coverage of the module.
#![cfg_attr(not(test), allow(dead_code))]

use std::collections::HashSet;
use std::fmt;

// ---------------------------------------------------------------------------
// States
// ---------------------------------------------------------------------------

/// A node in the worker / coordinator lifecycle. Terminal states
/// ([`LifecycleState::Done`], [`LifecycleState::Failed`]) have no outgoing edges
/// — any event applied to them is illegal (mirrors the `jobs.rs` invariant that
/// stop/resume are no-ops once a job is done).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LifecycleState {
    /// The worker subprocess / coordinator run has been launched but has not yet
    /// executed its first agentic round. `meta.json` is written `running`, but
    /// no work has happened.
    Spawned,
    /// Executing agentic rounds normally — the steady state.
    Running,
    /// A round fanned heavy sub-work out to the Batches API and the coordinator
    /// is blocking on those jobs (maps to `Phase::AwaitingBatch`).
    AwaitingBatch,
    /// The last round ended *abnormally but non-terminally* — a loop was
    /// detected, the budget was exhausted, or a summarize-exit was forced. The
    /// coordinator now routes a recovery [`crate::loopguard::Disposition`].
    SoftFailed,
    /// A deliberate, resumable pause (an operator halt / durable checkpoint —
    /// maps to `Phase::Checkpoint`). Resumable, unlike a soft-fail which is a
    /// stumble rather than a planned stop.
    Checkpoint,
    /// Terminal: the task completed and a final answer was produced.
    Done,
    /// Terminal: the run failed — auto-recovery was exhausted and the operator
    /// was flagged, or an unrecoverable error stopped the run.
    Failed,
}

impl LifecycleState {
    /// Every state, in canonical (roughly lifecycle) order. Used to build and
    /// validate the table and to render the diagram.
    pub const ALL: [LifecycleState; 7] = [
        LifecycleState::Spawned,
        LifecycleState::Running,
        LifecycleState::AwaitingBatch,
        LifecycleState::SoftFailed,
        LifecycleState::Checkpoint,
        LifecycleState::Done,
        LifecycleState::Failed,
    ];

    /// The stable, lower-kebab name used in the diagram, logs, and parsing.
    pub fn as_str(self) -> &'static str {
        match self {
            LifecycleState::Spawned => "spawned",
            LifecycleState::Running => "running",
            LifecycleState::AwaitingBatch => "awaiting_batch",
            LifecycleState::SoftFailed => "soft_failed",
            LifecycleState::Checkpoint => "checkpoint",
            LifecycleState::Done => "done",
            LifecycleState::Failed => "failed",
        }
    }

    /// Parse a state name back (inverse of [`LifecycleState::as_str`]).
    pub fn parse(s: &str) -> Option<LifecycleState> {
        LifecycleState::ALL.into_iter().find(|st| st.as_str() == s)
    }

    /// Terminal states have no outgoing transitions.
    pub fn is_terminal(self) -> bool {
        matches!(self, LifecycleState::Done | LifecycleState::Failed)
    }

    /// Map a durable coordinator [`crate::coordinator::Phase`] onto a lifecycle
    /// state. Total over every `Phase` arm (exhaustive `match` → compile-time
    /// tripwire if a new phase is added).
    pub fn from_phase(phase: crate::coordinator::Phase) -> LifecycleState {
        use crate::coordinator::Phase;
        match phase {
            Phase::Coordinating => LifecycleState::Running,
            Phase::AwaitingBatch => LifecycleState::AwaitingBatch,
            Phase::Checkpoint => LifecycleState::Checkpoint,
            Phase::Done => LifecycleState::Done,
            Phase::Failed => LifecycleState::Failed,
        }
    }

    /// Map a `worker_store` status string (`running` / `done` / `failed`) onto a
    /// lifecycle state. Returns `None` for an unknown/absent status so the caller
    /// can treat it as indeterminate rather than guessing.
    pub fn from_worker_status(status: &str) -> Option<LifecycleState> {
        match status {
            "running" => Some(LifecycleState::Running),
            "done" => Some(LifecycleState::Done),
            "failed" => Some(LifecycleState::Failed),
            _ => None,
        }
    }
}

impl fmt::Display for LifecycleState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

// ---------------------------------------------------------------------------
// Events
// ---------------------------------------------------------------------------

/// The signal that drives one lifecycle transition. Named for the runtime event
/// that actually causes it (a batch spawned, a soft-fail recovered, the operator
/// flagged) so the table reads as the real driver logic.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LifecycleEvent {
    /// First round begins (`Spawned → Running`).
    Start,
    /// A round offloaded work to the Batches API (`Running → AwaitingBatch`).
    BatchSpawned,
    /// The awaited batches finished (`AwaitingBatch → Running`).
    BatchSettled,
    /// A round ended abnormally-but-recoverably (`Running → SoftFailed`).
    SoftFail,
    /// Auto-resume or nudge back into work (`SoftFailed → Running`).
    Recover,
    /// Auto-recovery exhausted — surface to the operator (`SoftFailed → Failed`).
    Flag,
    /// A deliberate, resumable pause (`Running → Checkpoint`).
    Pause,
    /// Resume from a checkpoint (`Checkpoint → Running`).
    Resume,
    /// Normal completion (`Running → Done`).
    Complete,
    /// Unrecoverable failure straight from work / batch-wait (`* → Failed`).
    Fail,
}

impl LifecycleEvent {
    /// Every event, for table validation and the diagram legend.
    pub const ALL: [LifecycleEvent; 10] = [
        LifecycleEvent::Start,
        LifecycleEvent::BatchSpawned,
        LifecycleEvent::BatchSettled,
        LifecycleEvent::SoftFail,
        LifecycleEvent::Recover,
        LifecycleEvent::Flag,
        LifecycleEvent::Pause,
        LifecycleEvent::Resume,
        LifecycleEvent::Complete,
        LifecycleEvent::Fail,
    ];

    /// The stable event name used in the diagram and logs.
    pub fn as_str(self) -> &'static str {
        match self {
            LifecycleEvent::Start => "start",
            LifecycleEvent::BatchSpawned => "batch_spawned",
            LifecycleEvent::BatchSettled => "batch_settled",
            LifecycleEvent::SoftFail => "soft_fail",
            LifecycleEvent::Recover => "recover",
            LifecycleEvent::Flag => "flag",
            LifecycleEvent::Pause => "pause",
            LifecycleEvent::Resume => "resume",
            LifecycleEvent::Complete => "complete",
            LifecycleEvent::Fail => "fail",
        }
    }

    /// Map a recovery [`crate::loopguard::Disposition`] onto the lifecycle event
    /// it enacts. `Resume`/`Nudge` both re-enter work (`Recover`); `FlagOperator`
    /// is `Flag`; `None` (a normal turn or an operator interrupt) drives no
    /// lifecycle edge. Total over every `Disposition` arm.
    pub fn from_disposition(disp: crate::loopguard::Disposition) -> Option<LifecycleEvent> {
        use crate::loopguard::Disposition;
        match disp {
            Disposition::Resume | Disposition::Nudge => Some(LifecycleEvent::Recover),
            Disposition::FlagOperator => Some(LifecycleEvent::Flag),
            Disposition::None => None,
        }
    }
}

impl fmt::Display for LifecycleEvent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

// ---------------------------------------------------------------------------
// Transition + table
// ---------------------------------------------------------------------------

/// One declared, legal edge: applying `event` in `from` moves the machine to
/// `to`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Transition {
    pub from: LifecycleState,
    pub event: LifecycleEvent,
    pub to: LifecycleState,
}

/// Why building a [`TransitionTable`] was rejected. These are the `stateflow`
/// validity checks, surfaced as typed errors so the canonical-table test can
/// assert *exactly* which invariant a malformed table violates.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TableError {
    /// The same state was declared twice in the state set.
    DuplicateState(&'static str),
    /// The same event was declared twice in the event set.
    DuplicateEvent(&'static str),
    /// A transition's event name was empty (no-empty-events rule).
    EmptyEvent,
    /// A transition referenced a `from` state absent from the state set.
    UnknownFromState(&'static str),
    /// A transition referenced a `to` state absent from the state set.
    UnknownToState(&'static str),
    /// A transition referenced an event absent from the event set.
    UnknownEvent(&'static str),
    /// Two transitions share a `(from, event)` key — the machine would be
    /// non-deterministic.
    NondeterministicEdge {
        from: &'static str,
        event: &'static str,
    },
}

impl fmt::Display for TableError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TableError::DuplicateState(s) => write!(f, "duplicate state `{s}`"),
            TableError::DuplicateEvent(e) => write!(f, "duplicate event `{e}`"),
            TableError::EmptyEvent => write!(f, "transition has an empty event name"),
            TableError::UnknownFromState(s) => write!(f, "transition from unknown state `{s}`"),
            TableError::UnknownToState(s) => write!(f, "transition to unknown state `{s}`"),
            TableError::UnknownEvent(e) => write!(f, "transition on unknown event `{e}`"),
            TableError::NondeterministicEdge { from, event } => {
                write!(f, "duplicate edge `{from}` --{event}-->")
            }
        }
    }
}

impl std::error::Error for TableError {}

/// A validated set of legal lifecycle transitions. Construct with
/// [`TransitionTable::build`] (validates) or [`TransitionTable::canonical`] (the
/// real aish machine, guaranteed valid by test).
#[derive(Debug, Clone)]
pub struct TransitionTable {
    states: Vec<LifecycleState>,
    events: Vec<LifecycleEvent>,
    transitions: Vec<Transition>,
}

impl TransitionTable {
    /// Validate and build a table from explicit state / event / transition sets.
    /// Runs the full `stateflow` validity gate; returns the first violation.
    pub fn build(
        states: Vec<LifecycleState>,
        events: Vec<LifecycleEvent>,
        transitions: Vec<Transition>,
    ) -> Result<TransitionTable, TableError> {
        // Duplicate-state check.
        let mut seen_states: HashSet<&'static str> = HashSet::new();
        for st in &states {
            if !seen_states.insert(st.as_str()) {
                return Err(TableError::DuplicateState(st.as_str()));
            }
        }
        // Duplicate-event check.
        let mut seen_events: HashSet<&'static str> = HashSet::new();
        for ev in &events {
            if !seen_events.insert(ev.as_str()) {
                return Err(TableError::DuplicateEvent(ev.as_str()));
            }
        }
        // Per-transition existence + no-empty-event + determinism checks.
        let mut edges: HashSet<(&'static str, &'static str)> = HashSet::new();
        for t in &transitions {
            if t.event.as_str().is_empty() {
                return Err(TableError::EmptyEvent);
            }
            if !seen_states.contains(t.from.as_str()) {
                return Err(TableError::UnknownFromState(t.from.as_str()));
            }
            if !seen_states.contains(t.to.as_str()) {
                return Err(TableError::UnknownToState(t.to.as_str()));
            }
            if !seen_events.contains(t.event.as_str()) {
                return Err(TableError::UnknownEvent(t.event.as_str()));
            }
            if !edges.insert((t.from.as_str(), t.event.as_str())) {
                return Err(TableError::NondeterministicEdge {
                    from: t.from.as_str(),
                    event: t.event.as_str(),
                });
            }
        }
        Ok(TransitionTable {
            states,
            events,
            transitions,
        })
    }

    /// The canonical aish worker / coordinator lifecycle machine. Panics only if
    /// the hand-written table is malformed — which the `canonical_table_is_valid`
    /// test guarantees never happens in a shipped build.
    pub fn canonical() -> TransitionTable {
        use LifecycleEvent as E;
        use LifecycleState as S;
        let t = |from, event, to| Transition { from, event, to };
        let transitions = vec![
            // spawn → running
            t(S::Spawned, E::Start, S::Running),
            // batch fan-out round-trip
            t(S::Running, E::BatchSpawned, S::AwaitingBatch),
            t(S::AwaitingBatch, E::BatchSettled, S::Running),
            // soft-fail → recover / flag
            t(S::Running, E::SoftFail, S::SoftFailed),
            t(S::SoftFailed, E::Recover, S::Running),
            t(S::SoftFailed, E::Flag, S::Failed),
            // deliberate resumable pause
            t(S::Running, E::Pause, S::Checkpoint),
            t(S::Checkpoint, E::Resume, S::Running),
            // terminal exits
            t(S::Running, E::Complete, S::Done),
            t(S::Running, E::Fail, S::Failed),
            t(S::AwaitingBatch, E::Fail, S::Failed),
        ];
        TransitionTable::build(
            LifecycleState::ALL.to_vec(),
            LifecycleEvent::ALL.to_vec(),
            transitions,
        )
        .expect("canonical lifecycle table must be valid")
    }

    /// The resulting state of applying `event` in `state`, or `None` if that edge
    /// is illegal (the core guard: illegal transitions are *rejected*, not
    /// silently applied).
    pub fn next(&self, state: LifecycleState, event: LifecycleEvent) -> Option<LifecycleState> {
        self.transitions
            .iter()
            .find(|t| t.from == state && t.event == event)
            .map(|t| t.to)
    }

    /// True iff applying `event` in `state` is a declared, legal edge.
    pub fn is_legal(&self, state: LifecycleState, event: LifecycleEvent) -> bool {
        self.next(state, event).is_some()
    }

    /// All legal outgoing `(event, to)` edges from `state`, in table order.
    pub fn outgoing(&self, state: LifecycleState) -> Vec<(LifecycleEvent, LifecycleState)> {
        self.transitions
            .iter()
            .filter(|t| t.from == state)
            .map(|t| (t.event, t.to))
            .collect()
    }

    /// The declared states.
    pub fn states(&self) -> &[LifecycleState] {
        &self.states
    }

    /// The declared events.
    pub fn events(&self) -> &[LifecycleEvent] {
        &self.events
    }

    /// The declared transitions.
    pub fn transitions(&self) -> &[Transition] {
        &self.transitions
    }

    /// Render the machine as a human diagram — one block per state listing its
    /// legal edges, terminal states marked. This is what `:workers` shows so an
    /// operator can see the lifecycle the coordinator actually enforces.
    pub fn diagram(&self) -> String {
        let mut out = String::from("worker/coordinator lifecycle\n");
        for st in &self.states {
            let terminal = if st.is_terminal() { "  (terminal)" } else { "" };
            out.push_str(&format!("\n{}{}\n", st.as_str(), terminal));
            let edges = self.outgoing(*st);
            if edges.is_empty() {
                if st.is_terminal() {
                    out.push_str("  · no outgoing transitions\n");
                }
            } else {
                for (ev, to) in edges {
                    out.push_str(&format!("  --{}--> {}\n", ev.as_str(), to.as_str()));
                }
            }
        }
        out
    }
}

impl fmt::Display for TransitionTable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.diagram())
    }
}

/// Convenience: the resulting state of applying `event` in `state` under the
/// canonical machine. `None` for an illegal edge.
pub fn next(state: LifecycleState, event: LifecycleEvent) -> Option<LifecycleState> {
    TransitionTable::canonical().next(state, event)
}

/// Convenience: the canonical machine's self-describing diagram (for `:workers`).
pub fn diagram() -> String {
    TransitionTable::canonical().diagram()
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // --- validation gate -----------------------------------------------------

    #[test]
    fn canonical_table_is_valid() {
        // The shipped machine passes every validity check.
        let table = TransitionTable::canonical();
        assert_eq!(table.states().len(), 7);
        assert_eq!(table.events().len(), 10);
        assert_eq!(table.transitions().len(), 11);
    }

    #[test]
    fn duplicate_state_is_rejected() {
        let err = TransitionTable::build(
            vec![LifecycleState::Running, LifecycleState::Running],
            vec![LifecycleEvent::Complete],
            vec![],
        )
        .unwrap_err();
        assert_eq!(err, TableError::DuplicateState("running"));
    }

    #[test]
    fn duplicate_event_is_rejected() {
        let err = TransitionTable::build(
            vec![LifecycleState::Running],
            vec![LifecycleEvent::Complete, LifecycleEvent::Complete],
            vec![],
        )
        .unwrap_err();
        assert_eq!(err, TableError::DuplicateEvent("complete"));
    }

    #[test]
    fn unknown_from_state_is_rejected() {
        // `Spawned` is not in the declared state set → the transition's `from`
        // dangles.
        let err = TransitionTable::build(
            vec![LifecycleState::Running, LifecycleState::Done],
            vec![LifecycleEvent::Start],
            vec![Transition {
                from: LifecycleState::Spawned,
                event: LifecycleEvent::Start,
                to: LifecycleState::Running,
            }],
        )
        .unwrap_err();
        assert_eq!(err, TableError::UnknownFromState("spawned"));
    }

    #[test]
    fn unknown_to_state_is_rejected() {
        let err = TransitionTable::build(
            vec![LifecycleState::Running],
            vec![LifecycleEvent::Complete],
            vec![Transition {
                from: LifecycleState::Running,
                event: LifecycleEvent::Complete,
                to: LifecycleState::Done, // not in the state set
            }],
        )
        .unwrap_err();
        assert_eq!(err, TableError::UnknownToState("done"));
    }

    #[test]
    fn unknown_event_is_rejected() {
        let err = TransitionTable::build(
            vec![LifecycleState::Running, LifecycleState::Done],
            vec![LifecycleEvent::Fail], // `Complete` not declared
            vec![Transition {
                from: LifecycleState::Running,
                event: LifecycleEvent::Complete,
                to: LifecycleState::Done,
            }],
        )
        .unwrap_err();
        assert_eq!(err, TableError::UnknownEvent("complete"));
    }

    #[test]
    fn nondeterministic_edge_is_rejected() {
        // Two edges share `(running, soft_fail)` → the machine would be
        // ambiguous.
        let err = TransitionTable::build(
            vec![
                LifecycleState::Running,
                LifecycleState::SoftFailed,
                LifecycleState::Failed,
            ],
            vec![LifecycleEvent::SoftFail],
            vec![
                Transition {
                    from: LifecycleState::Running,
                    event: LifecycleEvent::SoftFail,
                    to: LifecycleState::SoftFailed,
                },
                Transition {
                    from: LifecycleState::Running,
                    event: LifecycleEvent::SoftFail,
                    to: LifecycleState::Failed,
                },
            ],
        )
        .unwrap_err();
        assert_eq!(
            err,
            TableError::NondeterministicEdge {
                from: "running",
                event: "soft_fail",
            }
        );
    }

    // --- legal / illegal edges ----------------------------------------------

    #[test]
    fn happy_path_spawn_to_done() {
        let m = TransitionTable::canonical();
        let mut st = LifecycleState::Spawned;
        st = m.next(st, LifecycleEvent::Start).unwrap();
        assert_eq!(st, LifecycleState::Running);
        st = m.next(st, LifecycleEvent::Complete).unwrap();
        assert_eq!(st, LifecycleState::Done);
        assert!(st.is_terminal());
    }

    #[test]
    fn soft_fail_recover_then_complete() {
        let m = TransitionTable::canonical();
        let st = LifecycleState::Running;
        let st = m.next(st, LifecycleEvent::SoftFail).unwrap();
        assert_eq!(st, LifecycleState::SoftFailed);
        // recover back into work…
        let st = m.next(st, LifecycleEvent::Recover).unwrap();
        assert_eq!(st, LifecycleState::Running);
        // …and finish.
        let st = m.next(st, LifecycleEvent::Complete).unwrap();
        assert_eq!(st, LifecycleState::Done);
    }

    #[test]
    fn soft_fail_flag_to_failed() {
        let m = TransitionTable::canonical();
        let st = m
            .next(LifecycleState::Running, LifecycleEvent::SoftFail)
            .unwrap();
        let st = m.next(st, LifecycleEvent::Flag).unwrap();
        assert_eq!(st, LifecycleState::Failed);
        assert!(st.is_terminal());
    }

    #[test]
    fn batch_round_trip() {
        let m = TransitionTable::canonical();
        let st = m
            .next(LifecycleState::Running, LifecycleEvent::BatchSpawned)
            .unwrap();
        assert_eq!(st, LifecycleState::AwaitingBatch);
        let st = m.next(st, LifecycleEvent::BatchSettled).unwrap();
        assert_eq!(st, LifecycleState::Running);
    }

    #[test]
    fn checkpoint_round_trip() {
        let m = TransitionTable::canonical();
        let st = m
            .next(LifecycleState::Running, LifecycleEvent::Pause)
            .unwrap();
        assert_eq!(st, LifecycleState::Checkpoint);
        let st = m.next(st, LifecycleEvent::Resume).unwrap();
        assert_eq!(st, LifecycleState::Running);
    }

    #[test]
    fn module_level_convenience_fns() {
        // The free `next` / `diagram` helpers operate on the canonical machine.
        assert_eq!(
            super::next(LifecycleState::Spawned, LifecycleEvent::Start),
            Some(LifecycleState::Running)
        );
        assert_eq!(
            super::next(LifecycleState::Spawned, LifecycleEvent::Complete),
            None
        );
        assert!(super::diagram().contains("--start--> running"));
    }

    #[test]
    fn is_legal_matches_next() {
        let m = TransitionTable::canonical();
        for &st in &LifecycleState::ALL {
            for ev in LifecycleEvent::ALL {
                assert_eq!(m.is_legal(st, ev), m.next(st, ev).is_some());
            }
        }
        assert!(m.is_legal(LifecycleState::Running, LifecycleEvent::Complete));
        assert!(!m.is_legal(LifecycleState::Done, LifecycleEvent::Start));
    }

    #[test]
    fn illegal_edges_are_rejected() {
        let m = TransitionTable::canonical();
        // Can't complete straight from spawn (must Start first).
        assert!(m.next(LifecycleState::Spawned, LifecycleEvent::Complete).is_none());
        // Can't recover a run that never soft-failed.
        assert!(m.next(LifecycleState::Running, LifecycleEvent::Recover).is_none());
        // Can't flag from plain running (only from soft-failed).
        assert!(m.next(LifecycleState::Running, LifecycleEvent::Flag).is_none());
        // A recovered soft-fail can't be recovered again without re-failing.
        assert!(m.next(LifecycleState::SoftFailed, LifecycleEvent::Complete).is_none());
    }

    #[test]
    fn terminal_states_have_no_outgoing_edges() {
        let m = TransitionTable::canonical();
        for term in [LifecycleState::Done, LifecycleState::Failed] {
            assert!(term.is_terminal());
            assert!(m.outgoing(term).is_empty());
            for ev in LifecycleEvent::ALL {
                assert!(
                    m.next(term, ev).is_none(),
                    "terminal {term} accepted {ev}"
                );
            }
        }
    }

    #[test]
    fn every_non_terminal_state_is_reachable_and_escapable() {
        let m = TransitionTable::canonical();
        // Every non-terminal state has at least one outgoing edge (no dead ends
        // that aren't terminal), and every state except Spawned is some edge's
        // target (reachable).
        let mut targets: HashSet<LifecycleState> = HashSet::new();
        for t in m.transitions() {
            targets.insert(t.to);
        }
        for st in m.states() {
            if !st.is_terminal() {
                assert!(!m.outgoing(*st).is_empty(), "non-terminal {st} is a dead end");
            }
            if *st != LifecycleState::Spawned {
                assert!(targets.contains(st), "{st} is unreachable");
            }
        }
    }

    // --- diagram -------------------------------------------------------------

    #[test]
    fn diagram_renders_states_and_edges() {
        let d = TransitionTable::canonical().diagram();
        assert!(d.contains("worker/coordinator lifecycle"));
        assert!(d.contains("spawned"));
        assert!(d.contains("--start--> running"));
        assert!(d.contains("--soft_fail--> soft_failed"));
        assert!(d.contains("--flag--> failed"));
        assert!(d.contains("done"));
        assert!(d.contains("(terminal)"));
        // Display forwards to diagram().
        assert_eq!(format!("{}", TransitionTable::canonical()), d);
    }

    // --- name round-trips ----------------------------------------------------

    #[test]
    fn state_name_round_trip() {
        for st in LifecycleState::ALL {
            assert_eq!(LifecycleState::parse(st.as_str()), Some(st));
        }
        assert_eq!(LifecycleState::parse("nonsense"), None);
    }

    #[test]
    fn all_arrays_have_no_dupes() {
        // The ALL arrays feed the table; a copy-paste dupe there would be a real
        // bug the build() gate would catch — assert it directly too.
        let states: HashSet<_> = LifecycleState::ALL.iter().map(|s| s.as_str()).collect();
        assert_eq!(states.len(), LifecycleState::ALL.len());
        let events: HashSet<_> = LifecycleEvent::ALL.iter().map(|e| e.as_str()).collect();
        assert_eq!(events.len(), LifecycleEvent::ALL.len());
    }

    // --- bridges to the real runtime types -----------------------------------

    #[test]
    fn phase_bridge_is_total_and_correct() {
        use crate::coordinator::Phase;
        assert_eq!(LifecycleState::from_phase(Phase::Coordinating), LifecycleState::Running);
        assert_eq!(
            LifecycleState::from_phase(Phase::AwaitingBatch),
            LifecycleState::AwaitingBatch
        );
        assert_eq!(LifecycleState::from_phase(Phase::Checkpoint), LifecycleState::Checkpoint);
        assert_eq!(LifecycleState::from_phase(Phase::Done), LifecycleState::Done);
        assert_eq!(LifecycleState::from_phase(Phase::Failed), LifecycleState::Failed);
    }

    #[test]
    fn worker_status_bridge() {
        assert_eq!(
            LifecycleState::from_worker_status("running"),
            Some(LifecycleState::Running)
        );
        assert_eq!(LifecycleState::from_worker_status("done"), Some(LifecycleState::Done));
        assert_eq!(
            LifecycleState::from_worker_status("failed"),
            Some(LifecycleState::Failed)
        );
        assert_eq!(LifecycleState::from_worker_status("weird"), None);
    }

    #[test]
    fn disposition_bridge_maps_recovery_events() {
        use crate::loopguard::Disposition;
        assert_eq!(
            LifecycleEvent::from_disposition(Disposition::Resume),
            Some(LifecycleEvent::Recover)
        );
        assert_eq!(
            LifecycleEvent::from_disposition(Disposition::Nudge),
            Some(LifecycleEvent::Recover)
        );
        assert_eq!(
            LifecycleEvent::from_disposition(Disposition::FlagOperator),
            Some(LifecycleEvent::Flag)
        );
        assert_eq!(LifecycleEvent::from_disposition(Disposition::None), None);
    }

    #[test]
    fn disposition_then_transition_end_to_end() {
        // The real recovery flow: a soft-failed run, a FlagOperator disposition
        // → the Flag event → the Failed terminal state. Proves the bridge and the
        // table compose into the lifecycle the coordinator enforces.
        let m = TransitionTable::canonical();
        let st = LifecycleState::SoftFailed;
        let ev = LifecycleEvent::from_disposition(crate::loopguard::Disposition::FlagOperator)
            .expect("flag maps to an event");
        assert_eq!(m.next(st, ev), Some(LifecycleState::Failed));
    }
}
