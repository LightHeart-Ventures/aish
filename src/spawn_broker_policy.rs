//! Security policy for host-brokered sibling spawn — validation, provenance
//! binding, rate limiting, and the forensic audit trail.
//!
//! # Why this exists
//!
//! [`spawn_broker`](crate::spawn_broker) is the transport (a nested worker writes
//! a [`SpawnRequest`] into its state spool) and
//! [`spawn_broker_host`](crate::spawn_broker_host) is the accept loop (claim →
//! gate → launch → gc). Neither one originally asked the question a host MUST
//! ask before it executes work on someone else's behalf: **is this request
//! trustworthy?**
//!
//! A spool file is just bytes on a mounted volume. Everything in a
//! [`SpawnRequest`] is *self-asserted by the requester*, including the fields the
//! host would otherwise act on directly:
//!
//! | Field | If blindly trusted |
//! |-------|--------------------|
//! | `requested_by_worker` | worker A attributes its spawns to worker B — the audit trail lies |
//! | `spawn_budget` | a requester claims `u32::MAX` and the fork-bomb backstop never fires |
//! | `cwd` | `../../..`-style traversal walks the host out of the intended repo |
//! | `task` | an unbounded blob is copied into argv / logs (memory + log-flood DoS) |
//! | `backend` / `base` | unexpected values reach the argv builder |
//! | `created_at_unix` | a stale request replays days later against a changed tree |
//!
//! and nothing at all bounded *how many* requests a single requester could emit.
//!
//! This module is the guard rail. It is deliberately pure (`std::fs` +
//! `serde_json` + `regex`, no tokio, no session/container internals) so every
//! rule is unit-testable without Docker, matching the testability posture of the
//! other three broker modules.
//!
//! # The four controls
//!
//! 1. **Validation** ([`RequestPolicy::validate`]) — schema version, request-id
//!    shape, task/cwd size caps, absolute + traversal-free `cwd`, known backend
//!    and base, a **budget ceiling** so a forged `spawn_budget` cannot outrun the
//!    fork-bomb backstop, and clock-skew / staleness bounds that kill replay.
//! 2. **Provenance binding** ([`worker_id_from_spool_path`] +
//!    [`RequestPolicy::validate`]'s `expected_worker`) — the host does not trust
//!    `requested_by_worker`; it *derives* the requester from the spool directory
//!    the request arrived in. Each worker gets its own state volume
//!    (`state_volume_host` → `/aish/state`), so the arrival path is an
//!    unforgeable capability: worker A cannot write into worker B's spool and
//!    therefore cannot impersonate B. A payload that claims a different worker is
//!    rejected outright rather than silently relabeled.
//! 3. **Rate limiting** ([`RateLimiter`]) — a sliding window per requester. The
//!    budget gate bounds spawn *depth*; this bounds spawn *rate*, which is the
//!    axis a compromised or looping coordinator actually attacks.
//! 4. **Audit trail** ([`audit_append`]) — one append-only JSONL record per
//!    decision (launched / refused / rejected / failed) with the requester, the
//!    session, the cwd, and a **secret-redacted** task preview, written
//!    owner-only. Without it there is no answer to "who spawned what, when".
//!
//! # What this module deliberately does NOT claim to fix
//!
//! * **Privilege separation.** Siblings run as the host aish's uid, not the
//!   requester's. Provenance binding makes the *attribution* trustworthy; it does
//!   not sandbox the sibling. Real separation needs per-worker uids or user
//!   namespaces and is a container-layer change, not a policy-layer one.
//! * **Inter-sibling network isolation.** Siblings share the host daemon's
//!   default bridge network. See
//!   `docs/host-brokered-sibling-spawn-security.md`.
//! * **Secrets in the task text.** Redaction here is best-effort defence in depth
//!   for the *audit log*; the spool payload itself is protected by owner-only
//!   file modes (see [`crate::spawn_broker`]), not by scrubbing.
#![allow(dead_code)]

use std::collections::{HashMap, VecDeque};
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::OnceLock;

use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::spawn_broker::{self, SCHEMA_VERSION, SpawnRequest};

/// Max accepted `task` size. A spawn task is a prompt, not a payload — 64 KiB is
/// already far past any legitimate brief, and the cap keeps a hostile requester
/// from pushing an unbounded blob through argv, the audit log, and the DB row.
pub const MAX_TASK_BYTES: usize = 64 * 1024;

/// Max accepted `cwd` length (POSIX `PATH_MAX` ballpark).
pub const MAX_CWD_BYTES: usize = 4096;

/// Max accepted `request_id` length. Ids are uuids in practice; the cap plus the
/// charset check keeps the id safe to interpolate into a filename.
pub const MAX_REQUEST_ID_LEN: usize = 64;

/// Tolerated clock skew for a request stamped in the future. Beyond this the
/// timestamp is treated as hostile/broken rather than merely imprecise.
pub const MAX_CLOCK_SKEW_SECS: u64 = 300;

/// A request older than this is refused as stale — the tree it was written
/// against has likely moved on, and honoring it is a replay.
pub const MAX_REQUEST_AGE_SECS: u64 = 24 * 60 * 60;

/// Backends a sibling may be launched on. Anything else never reaches the argv
/// builder.
pub const ALLOWED_BACKENDS: &[&str] = &["claude", "grok"];

/// Git bases an isolated sibling worktree may branch from.
pub const ALLOWED_BASES: &[&str] = &["main", "head"];

/// Default sliding-window length for the per-requester spawn rate limit.
pub const DEFAULT_RATE_WINDOW_SECS: u64 = 60;

/// Default max spawn requests honored per requester per window.
pub const DEFAULT_RATE_MAX_IN_WINDOW: u32 = 8;

/// Filename (inside the spool dir) of the append-only audit trail.
pub const AUDIT_FILENAME: &str = "audit.jsonl";

/// How much of the task text is kept in an audit record (post-redaction).
pub const AUDIT_PREVIEW_BYTES: usize = 200;

/// Rate-limit key used when neither a path-derived nor a self-asserted requester
/// is available (a request emitted straight by an interactive session).
pub const ANONYMOUS_REQUESTER: &str = "anonymous";

// ─────────────────────────── reject reasons ───────────────────────────

/// Why the host refused to act on a spawn request. Every variant is recorded in
/// the audit trail; none of them are retried (a refused request is discarded, not
/// requeued, so a hostile requester cannot grow the spool unboundedly).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RejectReason {
    /// On-disk schema the host does not speak.
    SchemaVersion { got: u32, want: u32 },
    /// Request id is empty, over-long, or contains characters that are unsafe to
    /// interpolate into a spool filename.
    BadRequestId,
    /// The task text is empty/whitespace — nothing to do.
    EmptyTask,
    /// The task text exceeds [`MAX_TASK_BYTES`].
    TaskTooLarge { bytes: usize, max: usize },
    /// `cwd` is not an absolute path.
    CwdNotAbsolute,
    /// `cwd` contains a `..` component (path traversal).
    CwdTraversal,
    /// `cwd` exceeds [`MAX_CWD_BYTES`].
    CwdTooLarge { bytes: usize, max: usize },
    /// Backend is not in [`ALLOWED_BACKENDS`].
    UnknownBackend(String),
    /// Base is not in [`ALLOWED_BASES`].
    UnknownBase(String),
    /// The self-asserted `spawn_budget` exceeds the host's ceiling — a forged
    /// budget escalation attempt against the fork-bomb backstop.
    BudgetTooHigh { claimed: u32, ceiling: u32 },
    /// Stamped further in the future than [`MAX_CLOCK_SKEW_SECS`].
    FutureTimestamp { created_at: u64, now: u64 },
    /// Older than the configured max age — refused as a replay.
    Stale { age_secs: u64, max: u64 },
    /// The payload claims a requester other than the worker whose spool the
    /// request arrived in — an impersonation attempt.
    ProvenanceMismatch {
        claimed: Option<String>,
        expected: String,
    },
    /// The requester exceeded its sliding-window spawn rate.
    RateLimited {
        key: String,
        window_secs: u64,
        max: u32,
    },
    /// The spool file could not be read/parsed (truncated, oversized, or not a
    /// [`SpawnRequest`] at all).
    Malformed(String),
}

impl std::fmt::Display for RejectReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RejectReason::SchemaVersion { got, want } => {
                write!(f, "unsupported schema_version {got} (host speaks {want})")
            }
            RejectReason::BadRequestId => write!(f, "request_id is empty, over-long, or unsafe"),
            RejectReason::EmptyTask => write!(f, "task is empty"),
            RejectReason::TaskTooLarge { bytes, max } => {
                write!(f, "task is {bytes} bytes (max {max})")
            }
            RejectReason::CwdNotAbsolute => write!(f, "cwd is not an absolute path"),
            RejectReason::CwdTraversal => write!(f, "cwd contains a '..' component"),
            RejectReason::CwdTooLarge { bytes, max } => {
                write!(f, "cwd is {bytes} bytes (max {max})")
            }
            RejectReason::UnknownBackend(b) => write!(f, "unknown backend {b:?}"),
            RejectReason::UnknownBase(b) => write!(f, "unknown base {b:?}"),
            RejectReason::BudgetTooHigh { claimed, ceiling } => {
                write!(
                    f,
                    "claimed spawn_budget {claimed} exceeds ceiling {ceiling}"
                )
            }
            RejectReason::FutureTimestamp { created_at, now } => {
                write!(
                    f,
                    "created_at_unix {created_at} is in the future (now {now})"
                )
            }
            RejectReason::Stale { age_secs, max } => {
                write!(f, "request is {age_secs}s old (max {max}s)")
            }
            RejectReason::ProvenanceMismatch { claimed, expected } => write!(
                f,
                "payload claims requester {claimed:?} but arrived in {expected:?}'s spool"
            ),
            RejectReason::RateLimited {
                key,
                window_secs,
                max,
            } => write!(
                f,
                "requester {key:?} exceeded {max} spawns per {window_secs}s"
            ),
            RejectReason::Malformed(e) => write!(f, "malformed spawn request: {e}"),
        }
    }
}

impl RejectReason {
    /// Short stable slug for dashboards / audit grouping.
    pub fn code(&self) -> &'static str {
        match self {
            RejectReason::SchemaVersion { .. } => "schema_version",
            RejectReason::BadRequestId => "bad_request_id",
            RejectReason::EmptyTask => "empty_task",
            RejectReason::TaskTooLarge { .. } => "task_too_large",
            RejectReason::CwdNotAbsolute => "cwd_not_absolute",
            RejectReason::CwdTraversal => "cwd_traversal",
            RejectReason::CwdTooLarge { .. } => "cwd_too_large",
            RejectReason::UnknownBackend(_) => "unknown_backend",
            RejectReason::UnknownBase(_) => "unknown_base",
            RejectReason::BudgetTooHigh { .. } => "budget_too_high",
            RejectReason::FutureTimestamp { .. } => "future_timestamp",
            RejectReason::Stale { .. } => "stale",
            RejectReason::ProvenanceMismatch { .. } => "provenance_mismatch",
            RejectReason::RateLimited { .. } => "rate_limited",
            RejectReason::Malformed(_) => "malformed",
        }
    }
}

// ─────────────────────────── validation ───────────────────────────

/// The host's acceptance policy for an inbound [`SpawnRequest`]. Every bound is a
/// field so an operator-facing knob can be threaded later without touching the
/// rule bodies.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RequestPolicy {
    pub max_task_bytes: usize,
    pub max_cwd_bytes: usize,
    /// Ceiling for the self-asserted `spawn_budget`. Defaults to the host's own
    /// [`spawn_broker::DEFAULT_SPAWN_BUDGET`] so a forged budget can never exceed
    /// what the host itself would have granted.
    pub budget_ceiling: u32,
    pub max_clock_skew_secs: u64,
    pub max_age_secs: u64,
}

impl Default for RequestPolicy {
    fn default() -> Self {
        RequestPolicy {
            max_task_bytes: MAX_TASK_BYTES,
            max_cwd_bytes: MAX_CWD_BYTES,
            budget_ceiling: spawn_broker::DEFAULT_SPAWN_BUDGET,
            max_clock_skew_secs: MAX_CLOCK_SKEW_SECS,
            max_age_secs: MAX_REQUEST_AGE_SECS,
        }
    }
}

impl RequestPolicy {
    /// Validate `req` against this policy at wall-clock `now_unix`.
    ///
    /// `expected_worker` is the requester identity the host **derived from the
    /// spool path** (see [`worker_id_from_spool_path`]). When it is `Some`, the
    /// payload's `requested_by_worker` must be absent or identical — anything
    /// else is [`RejectReason::ProvenanceMismatch`]. Pass `None` only when the
    /// arrival path carries no identity (e.g. a host-local spool shared with an
    /// interactive session).
    pub fn validate(
        &self,
        req: &SpawnRequest,
        expected_worker: Option<&str>,
        now_unix: u64,
    ) -> Result<(), RejectReason> {
        if req.schema_version != SCHEMA_VERSION {
            return Err(RejectReason::SchemaVersion {
                got: req.schema_version,
                want: SCHEMA_VERSION,
            });
        }
        if !is_safe_request_id(&req.request_id) {
            return Err(RejectReason::BadRequestId);
        }
        if req.task.trim().is_empty() {
            return Err(RejectReason::EmptyTask);
        }
        if req.task.len() > self.max_task_bytes {
            return Err(RejectReason::TaskTooLarge {
                bytes: req.task.len(),
                max: self.max_task_bytes,
            });
        }
        if req.cwd.len() > self.max_cwd_bytes {
            return Err(RejectReason::CwdTooLarge {
                bytes: req.cwd.len(),
                max: self.max_cwd_bytes,
            });
        }
        let cwd = Path::new(&req.cwd);
        if !cwd.is_absolute() {
            return Err(RejectReason::CwdNotAbsolute);
        }
        if cwd.components().any(|c| c == Component::ParentDir) {
            return Err(RejectReason::CwdTraversal);
        }
        if !ALLOWED_BACKENDS.contains(&req.backend.as_str()) {
            return Err(RejectReason::UnknownBackend(req.backend.clone()));
        }
        if !ALLOWED_BASES.contains(&req.base.as_str()) {
            return Err(RejectReason::UnknownBase(req.base.clone()));
        }
        if req.spawn_budget > self.budget_ceiling {
            return Err(RejectReason::BudgetTooHigh {
                claimed: req.spawn_budget,
                ceiling: self.budget_ceiling,
            });
        }
        if req.created_at_unix > now_unix.saturating_add(self.max_clock_skew_secs) {
            return Err(RejectReason::FutureTimestamp {
                created_at: req.created_at_unix,
                now: now_unix,
            });
        }
        let age = now_unix.saturating_sub(req.created_at_unix);
        if age > self.max_age_secs {
            return Err(RejectReason::Stale {
                age_secs: age,
                max: self.max_age_secs,
            });
        }
        if let Some(expected) = expected_worker {
            match req.requested_by_worker.as_deref() {
                None => {}
                Some(claimed) if claimed == expected => {}
                Some(_) => {
                    return Err(RejectReason::ProvenanceMismatch {
                        claimed: req.requested_by_worker.clone(),
                        expected: expected.to_string(),
                    });
                }
            }
        }
        Ok(())
    }
}

/// Whether a request id is safe to embed in a spool filename: non-empty, within
/// [`MAX_REQUEST_ID_LEN`], and restricted to `[A-Za-z0-9._-]` (no separators, no
/// dot-dot, no NUL).
pub fn is_safe_request_id(id: &str) -> bool {
    if id.is_empty() || id.len() > MAX_REQUEST_ID_LEN || id == "." || id == ".." {
        return false;
    }
    id.chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

/// Derive the requesting worker's id from the path a request arrived on.
///
/// The spool lives at `<worker-state-dir>/spawn-requests/spawn-req-<id>.json` and
/// each worker's state dir is named for the worker (`~/.aish/workers/<worker_id>`,
/// bind-mounted at `/aish/state`). The arrival directory is therefore an
/// **unforgeable** requester identity: a worker can only write into the volume
/// the host mounted for it.
///
/// Returns `None` when the path has no grandparent directory name to read (a
/// bare/relative spool, or the container-side `/aish/state` mount point whose
/// name carries no worker identity).
pub fn worker_id_from_spool_path(pending_path: &Path) -> Option<String> {
    let state_root = pending_path.parent()?.parent()?;
    let name = state_root.file_name()?.to_str()?;
    if name.is_empty() || name == "state" || name == "/" {
        return None;
    }
    Some(name.to_string())
}

/// Extract the `request_id` from a spool filename, for audit records where the
/// payload itself could not be parsed.
pub fn request_id_from_path(pending_path: &Path) -> String {
    let name = pending_path
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let name = name
        .strip_suffix(".json")
        .map(|s| s.to_string())
        .unwrap_or(name);
    name.strip_prefix("spawn-req-")
        .map(|s| s.to_string())
        .unwrap_or(name)
}

// ─────────────────────────── rate limiting ───────────────────────────

/// A per-requester sliding-window spawn rate limit.
///
/// The budget gate bounds how *deep* the spawn tree can go; nothing bounded how
/// *fast* a single requester could emit. A coordinator stuck in a retry loop (or
/// a compromised one) can write spool files as fast as the filesystem allows, and
/// every one of them costs the host a container launch. This caps that at
/// `max_in_window` per `window_secs` per requester.
///
/// Time is injected (`now` on every call) rather than read from the clock, so the
/// window behavior is deterministically testable.
#[derive(Clone, Debug)]
pub struct RateLimiter {
    window_secs: u64,
    max_in_window: u32,
    hits: HashMap<String, VecDeque<u64>>,
}

impl Default for RateLimiter {
    fn default() -> Self {
        RateLimiter::new(DEFAULT_RATE_WINDOW_SECS, DEFAULT_RATE_MAX_IN_WINDOW)
    }
}

impl RateLimiter {
    pub fn new(window_secs: u64, max_in_window: u32) -> Self {
        RateLimiter {
            window_secs,
            max_in_window,
            hits: HashMap::new(),
        }
    }

    /// A limiter that never refuses — for callers that opt out of rate limiting.
    pub fn unlimited() -> Self {
        RateLimiter::new(1, u32::MAX)
    }

    pub fn window_secs(&self) -> u64 {
        self.window_secs
    }

    pub fn max_in_window(&self) -> u32 {
        self.max_in_window
    }

    /// Record a spawn for `key` at `now` if it fits the window; otherwise refuse
    /// with [`RejectReason::RateLimited`] and record nothing (a refused attempt
    /// does not extend the penalty window).
    pub fn check(&mut self, key: &str, now: u64) -> Result<(), RejectReason> {
        let window = self.window_secs;
        let max = self.max_in_window;
        let cutoff = now.saturating_sub(window);
        let slot = self.hits.entry(key.to_string()).or_default();
        while slot.front().is_some_and(|&t| t < cutoff) {
            slot.pop_front();
        }
        if slot.len() as u64 >= max as u64 {
            return Err(RejectReason::RateLimited {
                key: key.to_string(),
                window_secs: window,
                max,
            });
        }
        slot.push_back(now);
        Ok(())
    }

    /// How many spawns `key` has in the window ending at `now` (read-only).
    pub fn hits_in_window(&self, key: &str, now: u64) -> usize {
        let cutoff = now.saturating_sub(self.window_secs);
        self.hits
            .get(key)
            .map(|q| q.iter().filter(|&&t| t >= cutoff).count())
            .unwrap_or(0)
    }

    /// Drop windows with no hits remaining at `now` (bounded memory for a
    /// long-lived host loop that sees many short-lived requesters).
    pub fn gc(&mut self, now: u64) {
        let cutoff = now.saturating_sub(self.window_secs);
        self.hits.retain(|_, q| {
            while q.front().is_some_and(|&t| t < cutoff) {
                q.pop_front();
            }
            !q.is_empty()
        });
    }
}

// ─────────────────────────── audit trail ───────────────────────────

/// One append-only record of a host decision about a spawn request. Written as a
/// single JSON line to `<spool>/audit.jsonl` with owner-only permissions.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditRecord {
    /// Unix epoch seconds the decision was made.
    pub at_unix: u64,
    /// The request this decision concerns.
    pub request_id: String,
    /// `launched` | `refused_budget` | `rejected` | `failed`.
    pub decision: String,
    /// Stable reason/error slug, when the decision was not a clean launch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason_code: Option<String>,
    /// Human-readable detail (the [`RejectReason`] / launcher error text).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// The requester as the HOST determined it (path-derived when available,
    /// never the unverified payload claim).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requester: Option<String>,
    /// What the payload claimed, retained when it disagreed with `requester`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claimed_requester: Option<String>,
    /// Originating interactive session id.
    pub launch_session_id: String,
    pub cwd: String,
    pub isolate: bool,
    pub base: String,
    /// Budget stamped on the launched sibling, when one was launched.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sibling_budget: Option<u32>,
    /// Redacted, truncated task preview — enough to identify the work without
    /// mirroring an unbounded (or secret-bearing) prompt into the log.
    pub task_preview: String,
}

/// Decision slug for a clean launch.
pub const DECISION_LAUNCHED: &str = "launched";
/// Decision slug for the fork-bomb budget backstop firing.
pub const DECISION_REFUSED_BUDGET: &str = "refused_budget";
/// Decision slug for a policy rejection.
pub const DECISION_REJECTED: &str = "rejected";
/// Decision slug for a launcher error.
pub const DECISION_FAILED: &str = "failed";

impl AuditRecord {
    /// Build a record for a parsed request.
    pub fn for_request(
        req: &SpawnRequest,
        requester: Option<&str>,
        decision: &str,
        now_unix: u64,
    ) -> Self {
        let claimed = req.requested_by_worker.clone();
        let claimed_requester = match (requester, claimed.as_deref()) {
            (Some(host), Some(payload)) if host != payload => claimed.clone(),
            _ => None,
        };
        AuditRecord {
            at_unix: now_unix,
            request_id: req.request_id.clone(),
            decision: decision.to_string(),
            reason_code: None,
            detail: None,
            requester: requester.map(|s| s.to_string()).or(claimed),
            claimed_requester,
            launch_session_id: req.launch_session_id.clone(),
            cwd: req.cwd.clone(),
            isolate: req.isolate,
            base: req.base.clone(),
            sibling_budget: None,
            task_preview: task_preview(&req.task),
        }
    }

    /// Build a record for a request that could not even be parsed — all we have
    /// is the arrival path.
    pub fn for_unparsable(
        request_id: &str,
        requester: Option<&str>,
        reason: &RejectReason,
        now_unix: u64,
    ) -> Self {
        AuditRecord {
            at_unix: now_unix,
            request_id: request_id.to_string(),
            decision: DECISION_REJECTED.to_string(),
            reason_code: Some(reason.code().to_string()),
            detail: Some(reason.to_string()),
            requester: requester.map(|s| s.to_string()),
            claimed_requester: None,
            launch_session_id: String::new(),
            cwd: String::new(),
            isolate: false,
            base: String::new(),
            sibling_budget: None,
            task_preview: String::new(),
        }
    }

    /// Attach a reject reason (sets `reason_code` + `detail`).
    pub fn with_reason(mut self, reason: &RejectReason) -> Self {
        self.reason_code = Some(reason.code().to_string());
        self.detail = Some(reason.to_string());
        self
    }

    /// Attach a free-form error detail (launcher failure).
    pub fn with_error(mut self, code: &str, detail: impl Into<String>) -> Self {
        self.reason_code = Some(code.to_string());
        self.detail = Some(detail.into());
        self
    }

    /// Attach the budget stamped on the launched sibling.
    pub fn with_sibling_budget(mut self, budget: u32) -> Self {
        self.sibling_budget = Some(budget);
        self
    }
}

/// Path of the audit trail under a given state root.
pub fn audit_path(state_root: &Path) -> PathBuf {
    spawn_broker::spool_dir(state_root).join(AUDIT_FILENAME)
}

/// Append one record to the audit trail, creating the spool dir (owner-only) and
/// the log (owner-only) as needed. Each record is a single JSON line so the log
/// is append-safe, greppable, and cheap to tail.
pub fn audit_append(state_root: &Path, rec: &AuditRecord) -> io::Result<()> {
    let dir = spawn_broker::spool_dir(state_root);
    fs::create_dir_all(&dir)?;
    spawn_broker::harden_dir(&dir)?;
    let path = audit_path(state_root);
    let mut line =
        serde_json::to_string(rec).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    line.push('\n');

    let mut opts = OpenOptions::new();
    opts.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        // Owner-only at CREATION time (no window where the log is group/world
        // readable), belt-and-braces with the harden_file below.
        opts.mode(spawn_broker::SPOOL_FILE_MODE);
    }
    let mut f = opts.open(&path)?;
    f.write_all(line.as_bytes())?;
    f.flush()?;
    drop(f);
    spawn_broker::harden_file(&path)?;
    Ok(())
}

/// Read the audit trail back (oldest first). Unparsable lines are skipped so a
/// partially-written tail never breaks inspection. Missing log yields an empty
/// vec.
pub fn audit_read(state_root: &Path) -> io::Result<Vec<AuditRecord>> {
    let path = audit_path(state_root);
    let text = match fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    Ok(text
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str::<AuditRecord>(l).ok())
        .collect())
}

// ─────────────────────────── redaction ───────────────────────────

fn secret_kv_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"(?i)\b(pass(?:word|wd|phrase)?|secret|token|api[_-]?key|access[_-]?key|private[_-]?key|credentials?)\b\s*[:=]\s*\S+",
        )
        .expect("static secret regex compiles")
    })
}

fn bearer_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?i)\bbearer\s+[A-Za-z0-9._\-]{8,}").expect("static bearer regex compiles")
    })
}

/// Best-effort scrub of obvious credential shapes before a string is written to
/// the audit log. Defence in depth only — the authoritative protection for spool
/// payloads is the owner-only file mode, not this.
pub fn redact_secrets(text: &str) -> String {
    let once = secret_kv_re().replace_all(text, "${1}=<redacted>");
    bearer_re()
        .replace_all(&once, "Bearer <redacted>")
        .into_owned()
}

/// Redact then truncate the task text to [`AUDIT_PREVIEW_BYTES`] on a char
/// boundary.
pub fn task_preview(task: &str) -> String {
    truncate_on_char_boundary(&redact_secrets(task), AUDIT_PREVIEW_BYTES)
}

fn truncate_on_char_boundary(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_string();
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    fn temp_root() -> PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "aish-spawn-policy-test-{}-{}",
            std::process::id(),
            n
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    const NOW: u64 = 1_800_000_000;

    fn req() -> SpawnRequest {
        let mut r = SpawnRequest::new(
            "11111111-2222-3333-4444-555555555555",
            "do the thing",
            "/repo",
            "claude",
            "opus",
            true,
            "main",
            3,
            "sess-1",
            Some("w_parent".to_string()),
        );
        r.created_at_unix = NOW;
        r
    }

    // ── validation ──

    #[test]
    fn accepts_a_well_formed_request() {
        let p = RequestPolicy::default();
        assert_eq!(p.validate(&req(), Some("w_parent"), NOW), Ok(()));
    }

    #[test]
    fn rejects_wrong_schema_version() {
        let mut r = req();
        r.schema_version = SCHEMA_VERSION + 7;
        let err = RequestPolicy::default()
            .validate(&r, None, NOW)
            .unwrap_err();
        assert_eq!(err.code(), "schema_version");
    }

    #[test]
    fn rejects_unsafe_request_ids() {
        for bad in ["", "..", "a/b", "a\0b", "x y"] {
            let mut r = req();
            r.request_id = bad.to_string();
            assert_eq!(
                RequestPolicy::default()
                    .validate(&r, None, NOW)
                    .unwrap_err()
                    .code(),
                "bad_request_id",
                "expected {bad:?} to be rejected"
            );
        }
        let mut long = req();
        long.request_id = "a".repeat(MAX_REQUEST_ID_LEN + 1);
        assert_eq!(
            RequestPolicy::default()
                .validate(&long, None, NOW)
                .unwrap_err()
                .code(),
            "bad_request_id"
        );
    }

    #[test]
    fn rejects_empty_and_oversized_tasks() {
        let mut empty = req();
        empty.task = "   \n ".to_string();
        assert_eq!(
            RequestPolicy::default()
                .validate(&empty, None, NOW)
                .unwrap_err()
                .code(),
            "empty_task"
        );

        let mut big = req();
        big.task = "x".repeat(MAX_TASK_BYTES + 1);
        assert_eq!(
            RequestPolicy::default()
                .validate(&big, None, NOW)
                .unwrap_err()
                .code(),
            "task_too_large"
        );
    }

    #[test]
    fn rejects_relative_and_traversing_cwd() {
        let mut rel = req();
        rel.cwd = "repo/sub".to_string();
        assert_eq!(
            RequestPolicy::default()
                .validate(&rel, None, NOW)
                .unwrap_err()
                .code(),
            "cwd_not_absolute"
        );

        let mut trav = req();
        trav.cwd = "/repo/../../etc".to_string();
        assert_eq!(
            RequestPolicy::default()
                .validate(&trav, None, NOW)
                .unwrap_err()
                .code(),
            "cwd_traversal"
        );
    }

    #[test]
    fn rejects_unknown_backend_and_base() {
        let mut b = req();
        b.backend = "evil".to_string();
        assert_eq!(
            RequestPolicy::default()
                .validate(&b, None, NOW)
                .unwrap_err()
                .code(),
            "unknown_backend"
        );

        let mut base = req();
        base.base = "; rm -rf /".to_string();
        assert_eq!(
            RequestPolicy::default()
                .validate(&base, None, NOW)
                .unwrap_err()
                .code(),
            "unknown_base"
        );
    }

    #[test]
    fn rejects_forged_budget_escalation() {
        let mut r = req();
        r.spawn_budget = u32::MAX;
        let err = RequestPolicy::default()
            .validate(&r, None, NOW)
            .unwrap_err();
        assert_eq!(err.code(), "budget_too_high");
        assert!(err.to_string().contains("exceeds ceiling"));
    }

    #[test]
    fn rejects_future_and_stale_timestamps() {
        let mut future = req();
        future.created_at_unix = NOW + MAX_CLOCK_SKEW_SECS + 1;
        assert_eq!(
            RequestPolicy::default()
                .validate(&future, None, NOW)
                .unwrap_err()
                .code(),
            "future_timestamp"
        );

        let mut stale = req();
        stale.created_at_unix = NOW - MAX_REQUEST_AGE_SECS - 1;
        assert_eq!(
            RequestPolicy::default()
                .validate(&stale, None, NOW)
                .unwrap_err()
                .code(),
            "stale"
        );

        // Small skew inside the tolerance is fine.
        let mut skewed = req();
        skewed.created_at_unix = NOW + 10;
        assert_eq!(
            RequestPolicy::default().validate(&skewed, None, NOW),
            Ok(())
        );
    }

    #[test]
    fn rejects_impersonation_but_allows_absent_claim() {
        // Worker A's payload claims it is worker B — refused.
        let mut forged = req();
        forged.requested_by_worker = Some("w_victim".to_string());
        let err = RequestPolicy::default()
            .validate(&forged, Some("w_attacker"), NOW)
            .unwrap_err();
        assert_eq!(err.code(), "provenance_mismatch");
        assert!(err.to_string().contains("w_victim"));

        // No claim at all is fine — the host supplies the path-derived identity.
        let mut anon = req();
        anon.requested_by_worker = None;
        assert_eq!(
            RequestPolicy::default().validate(&anon, Some("w_real"), NOW),
            Ok(())
        );
    }

    // ── provenance derivation ──

    #[test]
    fn derives_worker_id_from_spool_path() {
        let p = Path::new("/home/me/.aish/workers/w_abc123/spawn-requests/spawn-req-x.json");
        assert_eq!(worker_id_from_spool_path(p).as_deref(), Some("w_abc123"));
    }

    #[test]
    fn container_state_mount_carries_no_worker_identity() {
        let p = Path::new("/aish/state/spawn-requests/spawn-req-x.json");
        assert_eq!(worker_id_from_spool_path(p), None);
    }

    #[test]
    fn request_id_recovered_from_filename_for_unparsable_payloads() {
        let p = Path::new("/s/w_1/spawn-requests/spawn-req-deadbeef.json");
        assert_eq!(request_id_from_path(p), "deadbeef");
    }

    // ── rate limiting ──

    #[test]
    fn rate_limiter_allows_up_to_max_then_refuses() {
        let mut rl = RateLimiter::new(60, 3);
        for i in 0..3 {
            assert_eq!(rl.check("w_a", NOW + i), Ok(()), "hit {i} should pass");
        }
        let err = rl.check("w_a", NOW + 3).unwrap_err();
        assert_eq!(err.code(), "rate_limited");
        assert_eq!(rl.hits_in_window("w_a", NOW + 3), 3);
    }

    #[test]
    fn rate_limiter_is_per_requester() {
        let mut rl = RateLimiter::new(60, 1);
        assert_eq!(rl.check("w_a", NOW), Ok(()));
        assert!(rl.check("w_a", NOW).is_err());
        // A different requester is unaffected.
        assert_eq!(rl.check("w_b", NOW), Ok(()));
    }

    #[test]
    fn rate_limiter_window_slides() {
        let mut rl = RateLimiter::new(10, 2);
        assert_eq!(rl.check("w", NOW), Ok(()));
        assert_eq!(rl.check("w", NOW + 1), Ok(()));
        assert!(rl.check("w", NOW + 2).is_err());
        // Past the window the early hits age out.
        assert_eq!(rl.check("w", NOW + 12), Ok(()));
    }

    #[test]
    fn refused_attempt_does_not_extend_the_window() {
        let mut rl = RateLimiter::new(10, 1);
        assert_eq!(rl.check("w", NOW), Ok(()));
        assert!(rl.check("w", NOW + 1).is_err());
        assert_eq!(
            rl.hits_in_window("w", NOW + 1),
            1,
            "refusal was not recorded"
        );
    }

    #[test]
    fn unlimited_limiter_never_refuses() {
        let mut rl = RateLimiter::unlimited();
        for i in 0..100 {
            assert_eq!(rl.check("w", NOW + i), Ok(()));
        }
    }

    #[test]
    fn gc_drops_expired_windows() {
        let mut rl = RateLimiter::new(10, 5);
        rl.check("w", NOW).unwrap();
        assert_eq!(rl.hits_in_window("w", NOW), 1);
        rl.gc(NOW + 100);
        assert_eq!(rl.hits_in_window("w", NOW + 100), 0);
    }

    // ── audit trail ──

    #[test]
    fn audit_append_then_read_round_trips() {
        let root = temp_root();
        let r = req();
        audit_append(
            &root,
            &AuditRecord::for_request(&r, Some("w_parent"), DECISION_LAUNCHED, NOW)
                .with_sibling_budget(2),
        )
        .unwrap();
        audit_append(
            &root,
            &AuditRecord::for_request(&r, Some("w_parent"), DECISION_REJECTED, NOW + 1)
                .with_reason(&RejectReason::EmptyTask),
        )
        .unwrap();

        let recs = audit_read(&root).unwrap();
        assert_eq!(recs.len(), 2);
        assert_eq!(recs[0].decision, DECISION_LAUNCHED);
        assert_eq!(recs[0].sibling_budget, Some(2));
        assert_eq!(recs[0].requester.as_deref(), Some("w_parent"));
        assert_eq!(recs[1].decision, DECISION_REJECTED);
        assert_eq!(recs[1].reason_code.as_deref(), Some("empty_task"));
    }

    #[test]
    fn audit_read_missing_log_is_empty_not_error() {
        let root = temp_root().join("nope");
        assert!(audit_read(&root).unwrap().is_empty());
    }

    #[test]
    fn audit_skips_corrupt_lines() {
        let root = temp_root();
        audit_append(
            &root,
            &AuditRecord::for_request(&req(), None, DECISION_LAUNCHED, NOW),
        )
        .unwrap();
        // Simulate a torn tail.
        let mut f = OpenOptions::new()
            .append(true)
            .open(audit_path(&root))
            .unwrap();
        f.write_all(b"{not json\n").unwrap();
        drop(f);
        assert_eq!(audit_read(&root).unwrap().len(), 1);
    }

    #[test]
    fn audit_records_the_disputed_claim_separately() {
        let mut r = req();
        r.requested_by_worker = Some("w_victim".to_string());
        let rec = AuditRecord::for_request(&r, Some("w_attacker"), DECISION_REJECTED, NOW)
            .with_reason(&RejectReason::ProvenanceMismatch {
                claimed: Some("w_victim".into()),
                expected: "w_attacker".into(),
            });
        assert_eq!(rec.requester.as_deref(), Some("w_attacker"));
        assert_eq!(rec.claimed_requester.as_deref(), Some("w_victim"));
    }

    #[cfg(unix)]
    #[test]
    fn audit_log_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let root = temp_root();
        audit_append(
            &root,
            &AuditRecord::for_request(&req(), None, DECISION_LAUNCHED, NOW),
        )
        .unwrap();
        let mode = fs::metadata(audit_path(&root))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "audit log must not be group/world readable");
        let dmode = fs::metadata(spawn_broker::spool_dir(&root))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(
            dmode, 0o700,
            "spool dir must not be group/world traversable"
        );
    }

    // ── redaction ──

    #[test]
    fn redacts_common_credential_shapes() {
        let cases = [
            "deploy with password=hunter2 now",
            "set API_KEY: sk-abc123def456",
            "token=ghp_AAAABBBBCCCC",
            "use access-key = AKIAIOSFODNN7EXAMPLE",
        ];
        for c in cases {
            let out = redact_secrets(c);
            assert!(out.contains("<redacted>"), "not redacted: {c} -> {out}");
            assert!(!out.contains("hunter2"));
            assert!(!out.contains("sk-abc123def456"));
            assert!(!out.contains("ghp_AAAABBBBCCCC"));
            assert!(!out.contains("AKIAIOSFODNN7EXAMPLE"));
        }
        let bearer = redact_secrets("Authorization: Bearer eyJhbGciOiJIUzI1NiJ9");
        assert!(bearer.contains("Bearer <redacted>"));
        assert!(!bearer.contains("eyJhbGciOiJIUzI1NiJ9"));
    }

    #[test]
    fn redaction_leaves_ordinary_text_alone() {
        let plain = "refactor the tokenizer and update docs";
        assert_eq!(redact_secrets(plain), plain);
    }

    #[test]
    fn task_preview_truncates_on_char_boundary() {
        let task = "é".repeat(AUDIT_PREVIEW_BYTES); // 2 bytes each
        let preview = task_preview(&task);
        assert!(preview.len() <= AUDIT_PREVIEW_BYTES + 3); // + the ellipsis
        assert!(preview.ends_with('…'));
        // Slicing did not split a codepoint (constructing the String proved it).
        assert!(preview.chars().all(|c| c == 'é' || c == '…'));
    }

    #[test]
    fn task_preview_keeps_short_tasks_verbatim() {
        assert_eq!(task_preview("short brief"), "short brief");
    }
}
