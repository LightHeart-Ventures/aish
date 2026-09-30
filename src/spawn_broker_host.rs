//! Host accept loop for host-brokered sibling spawn (follow-up to #597, item 2).
//!
//! # Where this sits
//!
//! [`spawn_broker`](crate::spawn_broker) is the transport + protocol: a nested
//! worker writes a [`SpawnRequest`] into the spool, and the host claims it,
//! gates it on budget, and launches a sibling. This module is the **host ACCEPT
//! side** of that contract — the poll → claim → validate → rate-limit →
//! budget-gate → launch → audit → gc loop the design doc
//! (`docs/host-brokered-sibling-spawn.md`) lists as the follow-up host accept
//! loop.
//!
//! It is deliberately kept pure (`std::fs` + the broker API, no tokio, no
//! `container.rs`/`worker.rs` internals) and takes the actual sibling launch as
//! a **dependency-injected [`SiblingLauncher`]**. That keeps the meaty accept
//! logic — claim races, the fork-bomb budget backstop, the security policy, spool
//! GC — trivially unit-testable without Docker, exactly as [`spawn_broker`] kept
//! the transport testable without a session. The thin live adapter (a tokio tick
//! that calls [`serve_pending`] with a launcher that runs
//! `worker::coordinator_argv` + `container::run_argv` and registers the sibling
//! in `coordinator_store`) is a follow-up wire-up, matching the foundation-first
//! landing of #597.
//!
//! # The loop, per request
//!
//! For each pending request (oldest first by `created_at_unix`):
//!
//! 1. **claim** — [`spawn_broker::claim`] renames `.json` → `.json.claimed`; the
//!    rename is the mutual-exclusion primitive, so two host pollers (or a
//!    crash-restart) never double-spawn the same request. A claim that fails to
//!    read/parse (truncated, oversized, not a request at all) is recorded as a
//!    [`ServiceOutcome::Rejected`] and the batch CONTINUES — one malformed file
//!    can never wedge the whole spool.
//! 2. **validate** — [`spawn_broker_policy::RequestPolicy::validate`] checks every
//!    self-asserted field (schema, id shape, size caps, absolute traversal-free
//!    `cwd`, known backend/base, the **budget ceiling**, clock skew/staleness) and
//!    binds **provenance**: the requester is derived from the spool path the
//!    request arrived on, and a payload claiming a different worker is refused as
//!    impersonation.
//! 3. **rate-limit** — a sliding window per requester ([`spawn_broker_policy::RateLimiter`]).
//!    The budget gate bounds spawn *depth*; this bounds spawn *rate*.
//! 4. **budget gate** — [`spawn_broker::sibling_budget`] REFUSES at `0` (the
//!    fork-bomb backstop moved from the fork site to here) else yields the
//!    `budget - 1` to stamp on the sibling.
//! 5. **launch** — the injected [`SiblingLauncher`] starts the sibling with the
//!    validated request (its `requested_by_worker` rewritten to the host-derived
//!    identity) + stamped budget.
//! 6. **audit** — every decision, including refusals, appends one JSONL record to
//!    the owner-only spool audit log.
//! 7. **gc** — on launch OR refuse the claimed file is discarded (it is inert and
//!    never re-spawned). On launcher FAILURE the `.claimed` file is left in place
//!    for audit — `claim` already moved it out of the pending set, so a leftover
//!    never causes a re-spawn.
//!
//! A rejected request is **discarded, not requeued**: requeueing would let a
//! hostile requester grow the spool without bound and re-cost the host on every
//! tick. The audit record is the durable trace.
//!
//! The public surface is unreferenced until the live tokio tick adapter lands
//! (a thin follow-up), so — like [`spawn_broker`] — the module opts out of the
//! dead-code lint.
#![allow(dead_code)]

use std::io;
use std::path::{Path, PathBuf};

use crate::spawn_broker::{self, SpawnRequest};
use crate::spawn_broker_policy::{
    self as policy, ANONYMOUS_REQUESTER, AuditRecord, DECISION_FAILED, DECISION_LAUNCHED,
    DECISION_REFUSED_BUDGET, DECISION_REJECTED, RateLimiter, RejectReason, RequestPolicy,
};

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
    /// The request failed the security policy (malformed, impersonating,
    /// oversized, stale, rate-limited, …). The claimed file is discarded and the
    /// reason is recorded in the audit trail.
    Rejected {
        request_id: String,
        reason: RejectReason,
    },
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
            | ServiceOutcome::Rejected { request_id, .. }
            | ServiceOutcome::Failed { request_id, .. } => request_id,
        }
    }

    /// True when a sibling was actually launched.
    pub fn is_launched(&self) -> bool {
        matches!(self, ServiceOutcome::Launched { .. })
    }

    /// True when the security policy refused the request.
    pub fn is_rejected(&self) -> bool {
        matches!(self, ServiceOutcome::Rejected { .. })
    }

    /// The stable reason slug for a policy rejection, else `None`.
    pub fn reject_code(&self) -> Option<&'static str> {
        match self {
            ServiceOutcome::Rejected { reason, .. } => Some(reason.code()),
            _ => None,
        }
    }
}

/// A host-provided callback that actually launches a sibling coordinator for a
/// validated [`SpawnRequest`], stamped with `sibling_budget` (already decremented
/// from the requester's budget). Returning `Ok(())` means the sibling was
/// started; `Err` is surfaced as [`ServiceOutcome::Failed`] and the claimed file
/// is left for audit.
///
/// The request handed to the launcher has already passed the policy layer and
/// has its `requested_by_worker` rewritten to the **host-derived** requester, so
/// a launcher may treat that field as trustworthy (unlike the raw payload).
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

/// The host's security guard for the accept loop: acceptance policy, the
/// per-requester rate limiter, and the two toggles an operator might want.
///
/// A single instance is owned by the live poll loop and reused across ticks —
/// the rate limiter's sliding window is stateful by design.
#[derive(Clone, Debug)]
pub struct BrokerGuard {
    /// Field-level acceptance rules (sizes, budget ceiling, staleness, …).
    pub policy: RequestPolicy,
    /// Sliding-window spawn rate limit, keyed by the host-derived requester.
    pub limiter: RateLimiter,
    /// Derive the requester from the spool path and refuse payloads that claim a
    /// different worker. Leave this ON — it is the only thing standing between
    /// the audit trail and a worker that lies about who it is.
    pub derive_provenance: bool,
    /// Append a JSONL audit record for every decision.
    pub audit: bool,
}

impl Default for BrokerGuard {
    fn default() -> Self {
        BrokerGuard {
            policy: RequestPolicy::default(),
            limiter: RateLimiter::default(),
            derive_provenance: true,
            audit: true,
        }
    }
}

impl BrokerGuard {
    /// A guard with validation + provenance but no rate limiting and no audit —
    /// for tests and for a one-shot drain where the caller keeps its own log.
    pub fn permissive() -> Self {
        BrokerGuard {
            policy: RequestPolicy::default(),
            limiter: RateLimiter::unlimited(),
            derive_provenance: true,
            audit: false,
        }
    }
}

/// Service EVERY currently-pending spawn request under `state_root`, oldest
/// first, using the default [`BrokerGuard`]. Returns one [`ServiceOutcome`] per
/// request the host managed to claim.
///
/// Requests claimed by a racing poller between listing and claiming are silently
/// skipped (they are not ours). A malformed / policy-failing request yields a
/// [`ServiceOutcome::Rejected`] and does NOT abort the batch — later requests
/// still run.
pub fn serve_pending<L: SiblingLauncher>(
    state_root: &Path,
    launcher: &mut L,
) -> io::Result<Vec<ServiceOutcome>> {
    let mut guard = BrokerGuard::default();
    serve_pending_guarded(state_root, &mut guard, launcher)
}

/// Service every pending request under `state_root` with an explicit guard whose
/// rate-limiter state persists across calls.
pub fn serve_pending_guarded<L: SiblingLauncher>(
    state_root: &Path,
    guard: &mut BrokerGuard,
    launcher: &mut L,
) -> io::Result<Vec<ServiceOutcome>> {
    serve_pending_at(state_root, guard, launcher, spawn_broker::now_unix())
}

/// [`serve_pending_guarded`] with an injected wall clock, so staleness, skew, and
/// rate-limit windows are deterministically testable.
pub fn serve_pending_at<L: SiblingLauncher>(
    state_root: &Path,
    guard: &mut BrokerGuard,
    launcher: &mut L,
    now_unix: u64,
) -> io::Result<Vec<ServiceOutcome>> {
    let mut pending = list_pending_sorted(state_root)?;
    let mut outcomes = Vec::with_capacity(pending.len());
    for path in pending.drain(..) {
        if let Some(outcome) = service_one(state_root, &path, guard, launcher, now_unix) {
            outcomes.push(outcome);
        }
    }
    guard.limiter.gc(now_unix);
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
/// Bounded by the same size cap the claim path enforces.
fn read_created_at(pending_path: &Path) -> Option<u64> {
    let len = std::fs::metadata(pending_path).ok()?.len();
    if len > spawn_broker::MAX_REQUEST_BYTES {
        return None;
    }
    let bytes = std::fs::read(pending_path).ok()?;
    let req: SpawnRequest = serde_json::from_slice(&bytes).ok()?;
    Some(req.created_at_unix)
}

/// Claim, validate, rate-limit, budget-gate, launch, audit, and gc a single
/// pending request. Returns `None` when the request was already claimed/removed
/// by a racing poller.
///
/// Never returns an error: every failure mode is expressed as an outcome so one
/// bad request cannot abort the batch (that would be a trivial denial of service
/// against every other coordinator's spawns).
fn service_one<L: SiblingLauncher>(
    state_root: &Path,
    pending_path: &Path,
    guard: &mut BrokerGuard,
    launcher: &mut L,
    now_unix: u64,
) -> Option<ServiceOutcome> {
    // The requester identity the HOST derives — unforgeable, because a worker can
    // only write into the state volume the host mounted for it.
    let derived = if guard.derive_provenance {
        policy::worker_id_from_spool_path(pending_path)
    } else {
        None
    };

    let req = match spawn_broker::claim(pending_path) {
        Ok(Some(req)) => req,
        Ok(None) => return None, // lost the claim race — not ours
        Err(e) => {
            // Unreadable / oversized / unparsable: record and move on. The
            // `.claimed` file stays for forensics; it is inert.
            let reason = RejectReason::Malformed(e.to_string());
            let request_id = policy::request_id_from_path(pending_path);
            audit(
                guard,
                state_root,
                AuditRecord::for_unparsable(&request_id, derived.as_deref(), &reason, now_unix),
            );
            return Some(ServiceOutcome::Rejected { request_id, reason });
        }
    };

    // 1. Field validation + provenance binding.
    if let Err(reason) = guard.policy.validate(&req, derived.as_deref(), now_unix) {
        return Some(reject(
            guard,
            state_root,
            pending_path,
            &req,
            derived.as_deref(),
            reason,
            now_unix,
        ));
    }

    // 2. Rate limit, keyed by the host-derived requester (falling back to the
    //    payload's claim only when the arrival path carries no identity).
    let rate_key = derived
        .clone()
        .or_else(|| req.requested_by_worker.clone())
        .unwrap_or_else(|| ANONYMOUS_REQUESTER.to_string());
    if let Err(reason) = guard.limiter.check(&rate_key, now_unix) {
        return Some(reject(
            guard,
            state_root,
            pending_path,
            &req,
            derived.as_deref(),
            reason,
            now_unix,
        ));
    }

    // 3. Budget gate — the fork-bomb backstop, enforced here at the host. The
    //    claimed budget was already ceiling-checked by `validate`.
    let sibling_budget = match spawn_broker::sibling_budget(req.spawn_budget) {
        Some(b) => b,
        None => {
            let _ = spawn_broker::discard_claimed(pending_path);
            audit(
                guard,
                state_root,
                AuditRecord::for_request(
                    &req,
                    derived.as_deref(),
                    DECISION_REFUSED_BUDGET,
                    now_unix,
                ),
            );
            return Some(ServiceOutcome::RefusedBudget {
                request_id: req.request_id,
            });
        }
    };

    // 4. Launch with the HOST-derived provenance stamped in, so downstream
    //    consumers (registry rows, logs) record who really asked.
    let mut authenticated = req.clone();
    if let Some(worker) = derived.as_deref() {
        authenticated.requested_by_worker = Some(worker.to_string());
    }

    match launcher.launch(&authenticated, sibling_budget) {
        Ok(()) => {
            // Launched — the claimed file has served its purpose; gc it.
            let _ = spawn_broker::discard_claimed(pending_path);
            audit(
                guard,
                state_root,
                AuditRecord::for_request(&req, derived.as_deref(), DECISION_LAUNCHED, now_unix)
                    .with_sibling_budget(sibling_budget),
            );
            Some(ServiceOutcome::Launched {
                request_id: req.request_id,
                sibling_budget,
            })
        }
        Err(e) => {
            // Leave the `.claimed` file for audit; it is inert (never re-spawned).
            audit(
                guard,
                state_root,
                AuditRecord::for_request(&req, derived.as_deref(), DECISION_FAILED, now_unix)
                    .with_error("launcher_error", e.to_string()),
            );
            Some(ServiceOutcome::Failed {
                request_id: req.request_id,
                error: e.to_string(),
            })
        }
    }
}

/// Discard a policy-rejected request, record it, and build its outcome.
fn reject(
    guard: &BrokerGuard,
    state_root: &Path,
    pending_path: &Path,
    req: &SpawnRequest,
    derived: Option<&str>,
    reason: RejectReason,
    now_unix: u64,
) -> ServiceOutcome {
    let _ = spawn_broker::discard_claimed(pending_path);
    audit(
        guard,
        state_root,
        AuditRecord::for_request(req, derived, DECISION_REJECTED, now_unix).with_reason(&reason),
    );
    ServiceOutcome::Rejected {
        request_id: req.request_id.clone(),
        reason,
    }
}

/// Best-effort audit append. A failing log must never swallow a launch decision,
/// but it must also never fail silently — it goes to stderr like the rest of the
/// host's operational complaints.
fn audit(guard: &BrokerGuard, state_root: &Path, rec: AuditRecord) {
    if !guard.audit {
        return;
    }
    if let Err(e) = policy::audit_append(state_root, &rec) {
        eprintln!(
            "aish: spawn-broker audit append failed for request {}: {e}",
            rec.request_id
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spawn_broker::{SpawnRequest, write_request};
    use std::cell::RefCell;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    const NOW: u64 = 1_800_000_000;

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

    /// The worker id the host will derive for requests spooled under `root`.
    fn derived_id(root: &Path) -> String {
        root.file_name().unwrap().to_string_lossy().into_owned()
    }

    fn req(id: &str, budget: u32) -> SpawnRequest {
        let mut r = SpawnRequest::new(
            id, "task", "/repo", "claude", "opus", false, "main", budget, "sess-1", None,
        );
        r.created_at_unix = NOW;
        r
    }

    /// A launcher that records every (request_id, sibling_budget, requested_by)
    /// it is asked to launch, and can be told to fail.
    struct RecordingLauncher {
        seen: RefCell<Vec<(String, u32)>>,
        provenance: RefCell<Vec<Option<String>>>,
        fail: bool,
    }
    impl RecordingLauncher {
        fn new() -> Self {
            RecordingLauncher {
                seen: RefCell::new(Vec::new()),
                provenance: RefCell::new(Vec::new()),
                fail: false,
            }
        }
        fn failing() -> Self {
            RecordingLauncher {
                seen: RefCell::new(Vec::new()),
                provenance: RefCell::new(Vec::new()),
                fail: true,
            }
        }
    }
    impl SiblingLauncher for RecordingLauncher {
        fn launch(&mut self, r: &SpawnRequest, budget: u32) -> io::Result<()> {
            self.seen.borrow_mut().push((r.request_id.clone(), budget));
            self.provenance
                .borrow_mut()
                .push(r.requested_by_worker.clone());
            if self.fail {
                Err(io::Error::other("boom"))
            } else {
                Ok(())
            }
        }
    }

    fn serve(root: &Path, l: &mut RecordingLauncher) -> Vec<ServiceOutcome> {
        let mut g = BrokerGuard::default();
        serve_pending_at(root, &mut g, l, NOW).unwrap()
    }

    #[test]
    fn empty_spool_yields_no_outcomes() {
        let root = temp_root();
        let mut l = RecordingLauncher::new();
        let out = serve(&root, &mut l);
        assert!(out.is_empty());
        assert!(l.seen.borrow().is_empty());
    }

    #[test]
    fn launches_within_budget_and_stamps_decremented_budget() {
        let root = temp_root();
        write_request(&root, &req("a", 3)).unwrap();
        let mut l = RecordingLauncher::new();

        let out = serve(&root, &mut l);

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
        let again = serve(&root, &mut l);
        assert!(again.is_empty());
    }

    #[test]
    fn refuses_at_zero_budget_without_launching() {
        let root = temp_root();
        write_request(&root, &req("z", 0)).unwrap();
        let mut l = RecordingLauncher::new();

        let out = serve(&root, &mut l);

        assert_eq!(
            out,
            vec![ServiceOutcome::RefusedBudget {
                request_id: "z".into()
            }]
        );
        // Launcher never invoked.
        assert!(l.seen.borrow().is_empty());
        // Spool cleaned; re-serve is a no-op.
        assert!(serve(&root, &mut l).is_empty());
    }

    #[test]
    fn launcher_failure_is_reported_and_claimed_file_retained() {
        let root = temp_root();
        let path = write_request(&root, &req("f", 3)).unwrap();
        let mut l = RecordingLauncher::failing();

        let out = serve(&root, &mut l);

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
        assert!(serve(&root, &mut l2).is_empty());
        assert!(l2.seen.borrow().is_empty());
    }

    #[test]
    fn services_multiple_requests_oldest_first() {
        let root = temp_root();
        // Write out of order; created_at drives FIFO service order.
        let mut newer = req("newer", 3);
        newer.created_at_unix = NOW - 10;
        let mut older = req("older", 3);
        older.created_at_unix = NOW - 100;
        write_request(&root, &newer).unwrap();
        write_request(&root, &older).unwrap();

        let mut l = RecordingLauncher::new();
        let out = serve(&root, &mut l);

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
        let mut launcher = |r: &SpawnRequest, b: u32| -> io::Result<()> {
            launched.push((r.request_id.clone(), b));
            Ok(())
        };
        let mut g = BrokerGuard::default();
        let out = serve_pending_at(&root, &mut g, &mut launcher, NOW).unwrap();
        assert_eq!(out.len(), 1);
        assert!(out[0].is_launched());
        assert_eq!(launched, vec![("c".to_string(), 1)]);
    }

    // ───────────────────────── security regressions ─────────────────────────

    #[test]
    fn impersonating_request_is_rejected_not_launched() {
        let root = temp_root();
        let mut forged = req("forged", 3);
        forged.requested_by_worker = Some("w_someone_else".to_string());
        write_request(&root, &forged).unwrap();

        let mut l = RecordingLauncher::new();
        let out = serve(&root, &mut l);

        assert_eq!(out.len(), 1);
        assert_eq!(out[0].reject_code(), Some("provenance_mismatch"));
        assert!(l.seen.borrow().is_empty(), "impersonator must not launch");
    }

    #[test]
    fn host_derived_provenance_is_stamped_on_the_launched_request() {
        let root = temp_root();
        write_request(&root, &req("p", 3)).unwrap();
        let mut l = RecordingLauncher::new();

        serve(&root, &mut l);

        assert_eq!(
            l.provenance.borrow().as_slice(),
            &[Some(derived_id(&root))],
            "launcher must see the path-derived requester, not the payload's claim"
        );
    }

    #[test]
    fn forged_budget_escalation_is_rejected() {
        let root = temp_root();
        write_request(&root, &req("greedy", u32::MAX)).unwrap();
        let mut l = RecordingLauncher::new();

        let out = serve(&root, &mut l);

        assert_eq!(out[0].reject_code(), Some("budget_too_high"));
        assert!(l.seen.borrow().is_empty());
    }

    #[test]
    fn traversing_cwd_is_rejected() {
        let root = temp_root();
        let mut bad = req("trav", 3);
        bad.cwd = "/repo/../../etc".to_string();
        write_request(&root, &bad).unwrap();
        let mut l = RecordingLauncher::new();

        let out = serve(&root, &mut l);

        assert_eq!(out[0].reject_code(), Some("cwd_traversal"));
        assert!(l.seen.borrow().is_empty());
    }

    #[test]
    fn stale_request_is_not_replayed() {
        let root = temp_root();
        let mut old = req("ancient", 3);
        old.created_at_unix = NOW - policy::MAX_REQUEST_AGE_SECS - 1;
        write_request(&root, &old).unwrap();
        let mut l = RecordingLauncher::new();

        let out = serve(&root, &mut l);

        assert_eq!(out[0].reject_code(), Some("stale"));
        assert!(l.seen.borrow().is_empty());
    }

    #[test]
    fn malformed_request_does_not_wedge_the_batch() {
        let root = temp_root();
        // A junk file alongside a perfectly good request.
        let dir = spawn_broker::spool_dir(&root);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join(spawn_broker::request_filename("junk")),
            b"{ not json",
        )
        .unwrap();
        write_request(&root, &req("good", 3)).unwrap();

        let mut l = RecordingLauncher::new();
        let out = serve(&root, &mut l);

        assert_eq!(out.len(), 2, "both files must be accounted for");
        assert!(out.iter().any(|o| o.reject_code() == Some("malformed")));
        assert!(
            out.iter().any(|o| o.is_launched()),
            "a junk file must not block the healthy request: {out:?}"
        );
        assert_eq!(l.seen.borrow().len(), 1);
    }

    #[test]
    fn rate_limit_caps_a_single_requester_per_window() {
        let root = temp_root();
        let mut g = BrokerGuard {
            limiter: RateLimiter::new(60, 2),
            ..BrokerGuard::default()
        };
        let mut l = RecordingLauncher::new();

        for i in 0..4 {
            write_request(&root, &req(&format!("r{i}"), 3)).unwrap();
        }
        let out = serve_pending_at(&root, &mut g, &mut l, NOW).unwrap();

        assert_eq!(out.len(), 4);
        assert_eq!(out.iter().filter(|o| o.is_launched()).count(), 2);
        assert_eq!(
            out.iter()
                .filter(|o| o.reject_code() == Some("rate_limited"))
                .count(),
            2
        );
        assert_eq!(
            l.seen.borrow().len(),
            2,
            "only 2 spawns should reach Docker"
        );
    }

    #[test]
    fn rate_limit_window_state_persists_across_ticks() {
        let root = temp_root();
        let mut g = BrokerGuard {
            limiter: RateLimiter::new(60, 1),
            ..BrokerGuard::default()
        };
        let mut l = RecordingLauncher::new();

        write_request(&root, &req("t1", 3)).unwrap();
        let first = serve_pending_at(&root, &mut g, &mut l, NOW).unwrap();
        assert!(first[0].is_launched());

        // Second tick, same window — refused.
        write_request(&root, &req("t2", 3)).unwrap();
        let second = serve_pending_at(&root, &mut g, &mut l, NOW + 1).unwrap();
        assert_eq!(second[0].reject_code(), Some("rate_limited"));

        // Past the window — allowed again.
        write_request(&root, &req("t3", 3)).unwrap();
        let third = serve_pending_at(&root, &mut g, &mut l, NOW + 120).unwrap();
        assert!(third[0].is_launched());
    }

    #[test]
    fn every_decision_lands_in_the_audit_trail() {
        let root = temp_root();
        write_request(&root, &req("ok", 3)).unwrap();
        write_request(&root, &req("broke", 0)).unwrap();
        let mut forged = req("forged", 3);
        forged.requested_by_worker = Some("w_other".to_string());
        write_request(&root, &forged).unwrap();

        let mut l = RecordingLauncher::new();
        serve(&root, &mut l);

        let recs = policy::audit_read(&root).unwrap();
        assert_eq!(recs.len(), 3, "one record per decision: {recs:?}");
        let decisions: Vec<&str> = recs.iter().map(|r| r.decision.as_str()).collect();
        assert!(decisions.contains(&DECISION_LAUNCHED));
        assert!(decisions.contains(&DECISION_REFUSED_BUDGET));
        assert!(decisions.contains(&DECISION_REJECTED));
        // The launched record carries the host-derived requester + stamped budget.
        let launched = recs
            .iter()
            .find(|r| r.decision == DECISION_LAUNCHED)
            .unwrap();
        assert_eq!(
            launched.requester.as_deref(),
            Some(derived_id(&root).as_str())
        );
        assert_eq!(launched.sibling_budget, Some(2));
        // The forged record preserves the disputed claim for forensics.
        let rejected = recs
            .iter()
            .find(|r| r.decision == DECISION_REJECTED)
            .unwrap();
        assert_eq!(rejected.claimed_requester.as_deref(), Some("w_other"));
        assert_eq!(rejected.reason_code.as_deref(), Some("provenance_mismatch"));
    }

    #[test]
    fn audit_can_be_disabled() {
        let root = temp_root();
        write_request(&root, &req("quiet", 3)).unwrap();
        let mut g = BrokerGuard::permissive();
        let mut l = RecordingLauncher::new();
        serve_pending_at(&root, &mut g, &mut l, NOW).unwrap();
        assert!(policy::audit_read(&root).unwrap().is_empty());
    }

    #[test]
    fn rejected_requests_are_discarded_not_requeued() {
        let root = temp_root();
        let mut forged = req("forged", 3);
        forged.requested_by_worker = Some("w_other".to_string());
        write_request(&root, &forged).unwrap();

        let mut l = RecordingLauncher::new();
        assert_eq!(serve(&root, &mut l).len(), 1);
        // Second tick sees nothing — a rejection does not accumulate in the spool.
        assert!(serve(&root, &mut l).is_empty());
    }
}
