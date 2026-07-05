//! Sibling registration + read-back mapping for host-brokered spawn (follow-up
//! to #597, item 3).
//!
//! # Where this sits
//!
//! [`spawn_broker`](crate::spawn_broker) is the transport + protocol (a nested
//! worker writes a [`SpawnRequest`] into the spool). `spawn_broker_host` is the
//! host ACCEPT loop that claims a request, gates it on budget, and launches a
//! sibling. This module is the **third leg**: once the host has launched a
//! sibling it must record it in the durable `coordinator_store` so the
//! originating session can read the sibling back through `background_status` /
//! `job_output`, exactly as if the session had launched the coordinator itself.
//!
//! The design doc (`docs/host-brokered-sibling-spawn.md`) lists this as the
//! `coordinator_store` sibling registration for result read-back. Like the other
//! two legs it is kept **pure** (`std::io` + the broker payload, no rusqlite, no
//! `coordinator_store`/`worker` internals) and takes the actual store write as a
//! **dependency-injected [`SiblingRegistrar`]** — mirroring
//! `spawn_broker_host`'s `SiblingLauncher`.
//! That keeps the field-mapping logic (the part with real rules) trivially
//! unit-testable without a database, and leaves the thin live adapter — a
//! closure that calls `CoordinatorStore::register_run` — as a follow-up wire-up,
//! matching the foundation-first landing of #597.
//!
//! # What gets registered
//!
//! A freshly-launched sibling is a brand-new coordinator, so its registry row is
//! deterministic:
//!
//!   * `coord_id`      — the sibling's run id (the host mints it at launch);
//!   * `generation`    — [`INITIAL_GENERATION`] (`1`); a sibling is never a
//!     resurrected process, so it always starts at the first generation;
//!   * `pid`           — the sibling's OS process id;
//!   * `batch_job_id`  — `None`; a sibling starts as a normal turn-driven
//!     coordinator, not one awaiting an Anthropic Batches job;
//!   * `phase`         — [`PHASE_COORDINATING`]; the live, non-orphaned phase the
//!     startup reaper leaves alone;
//!   * `owner_session` — the requester's `launch_session_id`, so the row is
//!     attributed to the ORIGINATING interactive session and shows up in that
//!     session's `background_status` read-back (an empty id normalizes to
//!     `None`, matching a direct/unattributed launch).
//!
//! `requested_by_worker` and `task` are carried alongside for audit / parent
//! linkage and for the row's human-readable task label; they are not part of the
//! `coordinator_registry` primary key.
//!
//! The public surface is unreferenced until the live adapter closure lands (a
//! thin follow-up), so — like the other two broker modules — this one opts out
//! of the dead-code lint.
#![allow(dead_code)]

use std::io;

use crate::spawn_broker::SpawnRequest;

/// Generation stamped on a newly-launched sibling. A sibling is always a fresh
/// process (never a resurrected one), so it starts at the first generation. The
/// `coordinator_store` resurrection path bumps this for a re-registered row.
pub const INITIAL_GENERATION: i64 = 1;

/// The live lifecycle phase a just-launched sibling is registered under. Matches
/// the non-`orphaned` phase the `coordinator_store` startup reaper leaves alone,
/// so the sibling is treated as an in-flight run by read-back.
pub const PHASE_COORDINATING: &str = "coordinating";

/// The fully-resolved registration for a launched sibling — the exact set of
/// values the host writes into `coordinator_registry` (via
/// `CoordinatorStore::register_run`) so the originating session can read the
/// sibling back through `background_status` / `job_output`.
///
/// Produced purely from a [`SpawnRequest`] + the host-minted `coord_id` and OS
/// `pid` by [`SiblingRegistration::from_launch`]; no database contact happens
/// here — the write is the injected [`SiblingRegistrar`]'s job.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SiblingRegistration {
    /// The sibling's durable run id — the `coordinator_registry` primary key.
    pub coord_id: String,
    /// Restart generation. Always [`INITIAL_GENERATION`] for a fresh sibling.
    pub generation: i64,
    /// The sibling's OS process id.
    pub pid: i64,
    /// In-flight Anthropic Batches job id. Always `None` at launch — a sibling
    /// starts turn-driven, not awaiting a batch.
    pub batch_job_id: Option<String>,
    /// Coarse lifecycle phase. [`PHASE_COORDINATING`] at launch.
    pub phase: String,
    /// The originating interactive session (uuid) the row is attributed to, from
    /// the request's `launch_session_id`. `None` when that id is empty (a direct
    /// / unattributed launch).
    pub owner_session: Option<String>,
    /// The worker that emitted the spawn request — carried for audit / parent
    /// linkage. `None` when the request came straight from an interactive
    /// session.
    pub requested_by_worker: Option<String>,
    /// The sibling's task prompt — carried for the row's human-readable label.
    pub task: String,
}

impl SiblingRegistration {
    /// Build the registration for a sibling the host just launched for `req`,
    /// stamped with the host-minted `coord_id` and the sibling's OS `pid`.
    ///
    /// All the deterministic fields (generation, phase, absent batch id) are
    /// filled from the module constants; `owner_session` is the request's
    /// `launch_session_id` normalized so an empty string becomes `None`.
    pub fn from_launch(req: &SpawnRequest, coord_id: impl Into<String>, pid: i64) -> Self {
        SiblingRegistration {
            coord_id: coord_id.into(),
            generation: INITIAL_GENERATION,
            pid,
            batch_job_id: None,
            phase: PHASE_COORDINATING.to_string(),
            owner_session: normalize_session(&req.launch_session_id),
            requested_by_worker: req.requested_by_worker.clone(),
            task: req.task.clone(),
        }
    }

    /// Whether this sibling is readable from the interactive session identified
    /// by `session_id` — the predicate `background_status` uses to select "our"
    /// runs. True exactly when the row is attributed to that session.
    ///
    /// The read-back half of item 3: a session sees a sibling iff the host
    /// stamped the sibling's row with that session's id as `owner_session`.
    pub fn readable_by(&self, session_id: &str) -> bool {
        self.owner_session.as_deref() == Some(session_id)
    }

    /// The positional arguments for `CoordinatorStore::register_run`, in order —
    /// a convenience for the live adapter so the field-to-column mapping lives in
    /// exactly one place. `register_run(coord_id, generation, pid, batch_job_id,
    /// phase, owner_session)`.
    pub fn register_args(&self) -> (&str, i64, i64, Option<&str>, &str, Option<&str>) {
        (
            &self.coord_id,
            self.generation,
            self.pid,
            self.batch_job_id.as_deref(),
            &self.phase,
            self.owner_session.as_deref(),
        )
    }
}

/// Normalize a `launch_session_id` into an `owner_session`: a non-empty id is
/// kept, an empty one becomes `None` (a direct / unattributed launch).
fn normalize_session(launch_session_id: &str) -> Option<String> {
    if launch_session_id.is_empty() {
        None
    } else {
        Some(launch_session_id.to_string())
    }
}

/// A host-provided callback that durably records a launched sibling in the
/// coordinator store so the originating session can read it back. Returning
/// `Ok(())` means the row was written; `Err` is surfaced to the caller so the
/// host can log/retry.
///
/// Blanket-implemented for any `FnMut(&SiblingRegistration) -> io::Result<()>`,
/// so the live adapter can pass a closure that calls
/// `CoordinatorStore::register_run(reg.register_args())` — mirroring the
/// `spawn_broker_host`'s `SiblingLauncher` contract.
pub trait SiblingRegistrar {
    fn register(&mut self, reg: &SiblingRegistration) -> io::Result<()>;
}

impl<F> SiblingRegistrar for F
where
    F: FnMut(&SiblingRegistration) -> io::Result<()>,
{
    fn register(&mut self, reg: &SiblingRegistration) -> io::Result<()> {
        self(reg)
    }
}

/// Build the [`SiblingRegistration`] for a sibling the host just launched for
/// `req` (stamped with `coord_id` + `pid`) and hand it to `registrar` to persist.
/// Returns the registration on success so the caller can log or correlate it.
///
/// This is the one call the live host accept-loop adapter makes right after a
/// successful `SiblingLauncher::launch`:
/// launch the sibling, then `register_launched(&req, coord_id, pid, &mut store_writer)`.
pub fn register_launched<R: SiblingRegistrar>(
    req: &SpawnRequest,
    coord_id: impl Into<String>,
    pid: i64,
    registrar: &mut R,
) -> io::Result<SiblingRegistration> {
    let reg = SiblingRegistration::from_launch(req, coord_id, pid);
    registrar.register(&reg)?;
    Ok(reg)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A request mirroring what a nested worker emits: attributed to a launching
    /// session and a parent worker.
    fn req(launch_session: &str, parent: Option<&str>) -> SpawnRequest {
        SpawnRequest::new(
            "req-1",
            "do the thing",
            "/repo",
            "claude",
            "claude-haiku-4-5",
            false,
            "main",
            3,
            launch_session,
            parent.map(str::to_string),
        )
    }

    #[test]
    fn from_launch_maps_every_field() {
        let r = req("sess-abc", Some("worker-7"));
        let reg = SiblingRegistration::from_launch(&r, "coord-xyz", 4242);

        assert_eq!(reg.coord_id, "coord-xyz");
        assert_eq!(reg.generation, INITIAL_GENERATION);
        assert_eq!(reg.generation, 1);
        assert_eq!(reg.pid, 4242);
        assert_eq!(reg.batch_job_id, None);
        assert_eq!(reg.phase, PHASE_COORDINATING);
        assert_eq!(reg.owner_session.as_deref(), Some("sess-abc"));
        assert_eq!(reg.requested_by_worker.as_deref(), Some("worker-7"));
        assert_eq!(reg.task, "do the thing");
    }

    #[test]
    fn empty_launch_session_normalizes_to_none() {
        let r = req("", None);
        let reg = SiblingRegistration::from_launch(&r, "coord-1", 10);
        // A direct/unattributed launch leaves the row with no owner session.
        assert_eq!(reg.owner_session, None);
        // A direct request also has no parent worker.
        assert_eq!(reg.requested_by_worker, None);
    }

    #[test]
    fn readable_by_matches_only_the_owning_session() {
        let r = req("sess-abc", Some("worker-7"));
        let reg = SiblingRegistration::from_launch(&r, "coord-xyz", 1);
        assert!(reg.readable_by("sess-abc"));
        assert!(!reg.readable_by("sess-other"));
    }

    #[test]
    fn readable_by_is_false_for_an_unattributed_row() {
        let r = req("", None);
        let reg = SiblingRegistration::from_launch(&r, "coord-1", 1);
        // No owner session ⇒ no session can read it back by ownership.
        assert!(!reg.readable_by(""));
        assert!(!reg.readable_by("sess-abc"));
    }

    #[test]
    fn register_args_are_in_register_run_order() {
        let r = req("sess-abc", Some("worker-7"));
        let reg = SiblingRegistration::from_launch(&r, "coord-xyz", 99);
        let (coord_id, generation, pid, batch, phase, owner) = reg.register_args();
        assert_eq!(coord_id, "coord-xyz");
        assert_eq!(generation, 1);
        assert_eq!(pid, 99);
        assert_eq!(batch, None);
        assert_eq!(phase, "coordinating");
        assert_eq!(owner, Some("sess-abc"));
    }

    #[test]
    fn register_launched_invokes_the_registrar_once_and_returns_the_reg() {
        let r = req("sess-abc", Some("worker-7"));
        let mut seen: Vec<SiblingRegistration> = Vec::new();
        let out = {
            let mut registrar = |reg: &SiblingRegistration| {
                seen.push(reg.clone());
                Ok(())
            };
            register_launched(&r, "coord-xyz", 7, &mut registrar).unwrap()
        };
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0], out);
        assert_eq!(out.coord_id, "coord-xyz");
        assert_eq!(out.owner_session.as_deref(), Some("sess-abc"));
    }

    #[test]
    fn register_launched_propagates_a_registrar_error() {
        let r = req("sess-abc", None);
        let mut registrar =
            |_: &SiblingRegistration| Err(io::Error::new(io::ErrorKind::Other, "db down"));
        let err = register_launched(&r, "coord-xyz", 7, &mut registrar).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::Other);
        assert!(err.to_string().contains("db down"));
    }

    #[test]
    fn trait_object_style_closure_is_accepted() {
        // Exercises the blanket impl for a plain FnMut registrar.
        let r = req("sess-1", Some("w1"));
        let mut count = 0u32;
        let mut registrar = |_: &SiblingRegistration| {
            count += 1;
            Ok(())
        };
        let reg = SiblingRegistration::from_launch(&r, "c", 1);
        registrar.register(&reg).unwrap();
        assert_eq!(count, 1);
    }
}
