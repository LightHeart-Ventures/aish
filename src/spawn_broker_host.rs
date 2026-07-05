//! Host accept loop for host-brokered sibling spawn (follow-up to #597, item 2).
//!
//! # Where this sits
//!
//! [`spawn_broker`](crate::spawn_broker) is the transport + protocol: a nested
//! worker writes a [`SpawnRequest`] into the spool, and the host claims it,
//! gates it on budget, and launches a sibling. This module is the **host ACCEPT
//! side** of that contract — the poll → claim → budget-gate → launch → gc loop
//! the design doc (`docs/host-brokered-sibling-spawn.md`) lists as the follow-up
//! host accept loop.
//!
//! It is deliberately kept pure (`std::fs` + the broker API, no tokio, no
//! `container.rs`/`worker.rs` internals) and takes the actual sibling launch as
//! a **dependency-injected [`SiblingLauncher`]**. That keeps the meaty accept
//! logic — claim races, the fork-bomb budget backstop, spool GC — trivially
//! unit-testable without Docker, exactly as [`spawn_broker`] kept the transport
//! testable without a session. The thin live adapter (a tokio tick that calls
//! [`serve_pending`] with a launcher that runs `worker::coordinator_argv` +
//! `container::run_argv` and registers the sibling in `coordinator_store`) is a
//! follow-up wire-up, matching the foundation-first landing of #597.
//!
//! # The loop, per request
//!
//! For each pending request (oldest first by `created_at_unix`):
//!
//! 1. **claim** — [`spawn_broker::claim`] renames `.json` → `.json.claimed`; the
//!    rename is the mutual-exclusion primitive, so two host pollers (or a
//!    crash-restart) never double-spawn the same request.
//! 2. **budget gate** — [`spawn_broker::sibling_budget`] REFUSES at `0` (the
//!    fork-bomb backstop moved from the fork site to here) else yields the
//!    `budget - 1` to stamp on the sibling.
//! 3. **launch** — the injected [`SiblingLauncher`] starts the sibling with the
//!    validated request + stamped budget.
//! 4. **gc** — on launch OR refuse the claimed file is discarded (it is inert
//!    and never re-spawned). On launcher FAILURE the `.claimed` file is left in
//!    place for audit — `claim` already moved it out of the pending set, so a
//!    leftover never causes a re-spawn.
//!
//! The public surface is unreferenced until the live tokio tick adapter lands
//! (a thin follow-up), so — like [`spawn_broker`] — the module opts out of the
//! dead-code lint.
#![allow(dead_code)]

use std::io;
use std::path::{Path, PathBuf};

use crate::spawn_broker::{self, SpawnRequest};

/// What happened when the host serviced one claimed spawn request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ServiceOutcome {
    /// Sibling launched. `sibling_budget` is the budget stamped on it.
    Launched {
        request_id: String,
        sibling_budget: u32,
    },
    /// Budget exhausted at the requester (`spawn_budget == 0`) — the fork-bomb
    /// backstop refused the spawn. The claimed file is discarded.
    RefusedBudget { request_id: String },
    /// The injected launcher returned an error. The `.claimed` file is retained
    /// for audit (it is inert — never re-spawned).
    Failed { request_id: String, error: String },
}

impl ServiceOutcome {
    /// The request id this outcome concerns.
    pub fn request_id(&self) -> &str {
        match self {
            ServiceOutcome::Launched { request_id, .. }
            | ServiceOutcome::RefusedBudget { request_id }
            | ServiceOutcome::Failed { request_id, .. } => request_id,
        }
    }

    /// True when a sibling was actually launched.
    pub fn is_launched(&self) -> bool {
        matches!(self, ServiceOutcome::Launched { .. })
    }
}

/// A host-provided callback that actually launches a sibling coordinator for a
/// validated [`SpawnRequest`], stamped with `sibling_budget` (already decremented
/// from the requester's budget). Returning `Ok(())` means the sibling was
/// started; `Err` is surfaced as [`ServiceOutcome::Failed`] and the claimed file
/// is left for audit.
///
/// Blanket-implemented for any `FnMut(&SpawnRequest, u32) -> io::Result<()>` so
/// callers can pass a closure; the live adapter passes one that builds
/// `worker::coordinator_argv`, runs `container::run_argv`, and registers the
/// sibling in `coordinator_store`.
pub trait SiblingLauncher {
    fn launch(&mut self, req: &SpawnRequest, sibling_budget: u32) -> io::Result<()>;
}

impl<F> SiblingLauncher for F
where
    F: FnMut(&SpawnRequest, u32) -> io::Result<()>,
{
    fn launch(&mut self, req: &SpawnRequest, sibling_budget: u32) -> io::Result<()> {
        self(req, sibling_budget)
    }
}

/// Service EVERY currently-pending spawn request under `state_root`, oldest
/// first. Returns one [`ServiceOutcome`] per request the host managed to claim.
///
/// Requests claimed by a racing poller between listing and claiming are silently
/// skipped (they are not ours). An unreadable / malformed request file surfaces
/// its claim error and does not abort the batch — later requests still run.
pub fn serve_pending<L: SiblingLauncher>(
    state_root: &Path,
    launcher: &mut L,
) -> io::Result<Vec<ServiceOutcome>> {
    let mut pending = list_pending_sorted(state_root)?;
    let mut outcomes = Vec::with_capacity(pending.len());
    for path in pending.drain(..) {
        if let Some(outcome) = service_one(&path, launcher)? {
            outcomes.push(outcome);
        }
    }
    Ok(outcomes)
}

/// List pending requests sorted oldest-first by `created_at_unix` (FIFO service
/// order). Unreadable entries are dropped from ordering but still returned at the
/// end so `claim` can surface their error deterministically.
fn list_pending_sorted(state_root: &Path) -> io::Result<Vec<PathBuf>> {
    let mut paths = spawn_broker::list_pending(state_root)?;
    // Read each request's timestamp for FIFO ordering. A file that fails to
    // parse gets u64::MAX so it sorts last (its claim will report the error).
    paths.sort_by_key(|p| read_created_at(p).unwrap_or(u64::MAX));
    Ok(paths)
}

/// Peek a pending request's `created_at_unix` without claiming it (read-only).
fn read_created_at(pending_path: &Path) -> Option<u64> {
    let bytes = std::fs::read(pending_path).ok()?;
    let req: SpawnRequest = serde_json::from_slice(&bytes).ok()?;
    Some(req.created_at_unix)
}

/// Claim, budget-gate, launch, and gc a single pending request. Returns
/// `Ok(None)` when the request was already claimed/removed by a racing poller.
fn service_one<L: SiblingLauncher>(
    pending_path: &Path,
    launcher: &mut L,
) -> io::Result<Option<ServiceOutcome>> {
    let req = match spawn_broker::claim(pending_path)? {
        Some(req) => req,
        None => return Ok(None), // lost the claim race — not ours
    };

    // Budget gate — the fork-bomb backstop, enforced here at the host.
    let sibling_budget = match spawn_broker::sibling_budget(req.spawn_budget) {
        Some(b) => b,
        None => {
            // Refuse: drop the inert claimed file so the spool stays clean.
            let _ = spawn_broker::discard_claimed(pending_path);
            return Ok(Some(ServiceOutcome::RefusedBudget {
                request_id: req.request_id,
            }));
        }
    };

    match launcher.launch(&req, sibling_budget) {
        Ok(()) => {
            // Launched — the claimed file has served its purpose; gc it.
            let _ = spawn_broker::discard_claimed(pending_path);
            Ok(Some(ServiceOutcome::Launched {
                request_id: req.request_id,
                sibling_budget,
            }))
        }
        Err(e) => {
            // Leave the `.claimed` file for audit; it is inert (never re-spawned).
            Ok(Some(ServiceOutcome::Failed {
                request_id: req.request_id,
                error: e.to_string(),
            }))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spawn_broker::{write_request, SpawnRequest};
    use std::cell::RefCell;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    fn temp_root() -> PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "aish-spawn-broker-host-test-{}-{}",
            std::process::id(),
            n
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn req(id: &str, budget: u32) -> SpawnRequest {
        SpawnRequest::new(
            id, "task", "/repo", "claude", "opus", false, "main", budget, "sess-1", None,
        )
    }

    /// A launcher that records every (request_id, sibling_budget) it is asked to
    /// launch, and can be told to fail.
    struct RecordingLauncher {
        seen: RefCell<Vec<(String, u32)>>,
        fail: bool,
    }
    impl RecordingLauncher {
        fn new() -> Self {
            RecordingLauncher {
                seen: RefCell::new(Vec::new()),
                fail: false,
            }
        }
        fn failing() -> Self {
            RecordingLauncher {
                seen: RefCell::new(Vec::new()),
                fail: true,
            }
        }
    }
    impl SiblingLauncher for RecordingLauncher {
        fn launch(&mut self, r: &SpawnRequest, budget: u32) -> io::Result<()> {
            self.seen
                .borrow_mut()
                .push((r.request_id.clone(), budget));
            if self.fail {
                Err(io::Error::new(io::ErrorKind::Other, "boom"))
            } else {
                Ok(())
            }
        }
    }

    #[test]
    fn empty_spool_yields_no_outcomes() {
        let root = temp_root();
        let mut l = RecordingLauncher::new();
        let out = serve_pending(&root, &mut l).unwrap();
        assert!(out.is_empty());
        assert!(l.seen.borrow().is_empty());
    }

    #[test]
    fn launches_within_budget_and_stamps_decremented_budget() {
        let root = temp_root();
        write_request(&root, &req("a", 3)).unwrap();
        let mut l = RecordingLauncher::new();

        let out = serve_pending(&root, &mut l).unwrap();

        assert_eq!(out.len(), 1);
        assert_eq!(
            out[0],
            ServiceOutcome::Launched {
                request_id: "a".into(),
                sibling_budget: 2,
            }
        );
        // Launcher saw budget - 1.
        assert_eq!(l.seen.borrow().as_slice(), &[("a".to_string(), 2)]);
        // Claimed file was gc'd — spool is clean, re-serving does nothing.
        let again = serve_pending(&root, &mut l).unwrap();
        assert!(again.is_empty());
    }

    #[test]
    fn refuses_at_zero_budget_without_launching() {
        let root = temp_root();
        write_request(&root, &req("z", 0)).unwrap();
        let mut l = RecordingLauncher::new();

        let out = serve_pending(&root, &mut l).unwrap();

        assert_eq!(
            out,
            vec![ServiceOutcome::RefusedBudget {
                request_id: "z".into()
            }]
        );
        // Launcher never invoked.
        assert!(l.seen.borrow().is_empty());
        // Spool cleaned; re-serve is a no-op.
        assert!(serve_pending(&root, &mut l).unwrap().is_empty());
    }

    #[test]
    fn launcher_failure_is_reported_and_claimed_file_retained() {
        let root = temp_root();
        let path = write_request(&root, &req("f", 3)).unwrap();
        let mut l = RecordingLauncher::failing();

        let out = serve_pending(&root, &mut l).unwrap();

        assert_eq!(out.len(), 1);
        match &out[0] {
            ServiceOutcome::Failed { request_id, error } => {
                assert_eq!(request_id, "f");
                assert!(error.contains("boom"));
            }
            other => panic!("expected Failed, got {other:?}"),
        }
        // The pending .json is gone (claimed) but the .claimed sidecar remains.
        assert!(!path.exists());
        let claimed = {
            let mut s = path.into_os_string();
            s.push(".claimed");
            PathBuf::from(s)
        };
        assert!(claimed.exists());
        // Re-serving finds nothing pending (it was claimed, not re-spawned).
        let mut l2 = RecordingLauncher::new();
        assert!(serve_pending(&root, &mut l2).unwrap().is_empty());
        assert!(l2.seen.borrow().is_empty());
    }

    #[test]
    fn services_multiple_requests_oldest_first() {
        let root = temp_root();
        // Write out of order; created_at drives FIFO service order.
        let mut newer = req("newer", 3);
        newer.created_at_unix = 2_000;
        let mut older = req("older", 3);
        older.created_at_unix = 1_000;
        write_request(&root, &newer).unwrap();
        write_request(&root, &older).unwrap();

        let mut l = RecordingLauncher::new();
        let out = serve_pending(&root, &mut l).unwrap();

        assert_eq!(out.len(), 2);
        // Oldest first.
        assert_eq!(out[0].request_id(), "older");
        assert_eq!(out[1].request_id(), "newer");
        assert_eq!(
            l.seen.borrow().as_slice(),
            &[("older".to_string(), 2), ("newer".to_string(), 2)]
        );
    }

    #[test]
    fn closure_launcher_is_accepted() {
        let root = temp_root();
        write_request(&root, &req("c", 2)).unwrap();
        let mut launched = Vec::new();
        let mut launcher = |r: &SpawnRequest, b: u32| {
            launched.push((r.request_id.clone(), b));
            Ok(())
        };
        let out = serve_pending(&root, &mut launcher).unwrap();
        assert_eq!(out.len(), 1);
        assert!(out[0].is_launched());
        assert_eq!(launched, vec![("c".to_string(), 1)]);
    }
}
