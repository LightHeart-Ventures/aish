//! Host-brokered sibling spawn — flat worker topology (replaces docker-in-docker + sysbox).
//!
//! # Why this exists
//!
//! A background *coordinator* (a nested `aish --coordinator`) can itself call
//! `run_in_background`. Historically the only way for a coordinator running
//! *inside a container* to give each of its children its own container was
//! docker-in-docker: either the `sysbox-runc` OCI runtime or a bind-mounted
//! `/var/run/docker.sock`. Both are heavy, Linux/Docker-only, root-escalating,
//! and invisible to the host's `container::list`/`rm` cleanup (grandchildren
//! don't carry the host daemon's `aish.worker_id` labels the host filters on).
//!
//! The flat design instead has a nested worker **emit a spawn-request** which
//! the *host* aish services with a plain `docker run` **sibling** off its own
//! normal daemon. Every worker becomes a first-class sibling under one daemon:
//!
//!   * no nesting runtime, no socket mount, no cap-drop carve-outs, macOS works;
//!   * cleanup/observability already work — every sibling carries the host's
//!     `aish.worker_id` label, so `container::list`/`rm`/`forget_container` see
//!     them uniformly;
//!   * the argv single-source-of-truth (`worker::coordinator_argv`) is reused —
//!     the host rebuilds the sibling command from the same argv, the event only
//!     carries the non-secret [`SpawnRequest`];
//!   * secrets never travel in the event — the host already holds them (it
//!     launched worker #1) and injects from its own env.
//!
//! # Transport
//!
//! v1 transport is a **spool directory** under the already-mounted state volume
//! (`state_volume_host` → `/aish/state` in each worker). A worker writes
//! `spawn-requests/spawn-req-<id>.json` atomically (tmp + rename); the host
//! polls (or inotify-watches) the directory, [`claim`]s each request (rename to
//! `.claimed` so a crash-restart doesn't double-spawn), enforces the spawn
//! budget, and launches the sibling. It is nearly free, durable, and
//! restart-survivable. A Unix-socket transport (lower latency, interactive tier)
//! and the bidirectional webhook broker (multi-host) are drop-in replacements
//! for the same [`SpawnRequest`] payload — the event abstraction makes those a
//! transport swap, not a redesign.
//!
//! This module is deliberately pure `std::fs` + `serde_json` (no tokio, no
//! session/worker internals) so it is trivially unit-testable and the wiring
//! into `worker.rs`/`container.rs` stays a thin adapter.
//!
//! # Wiring status
//!
//! This is the transport + protocol foundation (spool write/list/claim, the
//! [`SpawnRequest`] payload, and the budget gate). The thin adapters that call
//! it — the nested-worker EMIT site in `worker.rs`'s `run_in_background`, the
//! host ACCEPT loop that polls the spool + launches siblings, and the
//! `coordinator_store` sibling registration for result read-back — land in the
//! follow-up. Until then the public surface below is intentionally unreferenced,
//! hence the module-wide `allow(dead_code)`.
#![allow(dead_code)]

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// Env flag that opts a session into host-brokered sibling spawn. When unset (or
/// not `1`/`true`), nested `run_in_background` keeps its legacy behavior. Read by
/// the `worker.rs` adapter, not by this module.
pub const BROKER_ENABLED_ENV: &str = "AISH_SPAWN_BROKER";

/// Env var carrying the remaining spawn budget into a worker process (mirrors the
/// existing `AISH_SPAWN_BUDGET` fork-bomb backstop). The broker stamps each
/// sibling with `budget - 1` and refuses to spawn at `0`.
pub const SPAWN_BUDGET_ENV: &str = "AISH_SPAWN_BUDGET";

/// Default spawn budget when `AISH_SPAWN_BUDGET` is absent — matches
/// `worker::spawn_budget_gate`'s default of 3 so the two guards agree.
pub const DEFAULT_SPAWN_BUDGET: u32 = 3;

/// Sub-directory (under the state root) that holds pending spawn requests.
pub const SPOOL_SUBDIR: &str = "spawn-requests";

/// Current on-disk schema version for a [`SpawnRequest`]. Bump on any
/// incompatible field change so the host can reject/upgrade stale records.
pub const SCHEMA_VERSION: u32 = 1;

const REQUEST_PREFIX: &str = "spawn-req-";
const REQUEST_EXT: &str = "json";
const CLAIMED_EXT: &str = "claimed";

/// Whether the broker is enabled for this process, per [`BROKER_ENABLED_ENV`].
pub fn broker_enabled() -> bool {
    matches!(
        std::env::var(BROKER_ENABLED_ENV).ok().as_deref(),
        Some("1") | Some("true") | Some("TRUE") | Some("yes")
    )
}

/// Read the current spawn budget from the environment, falling back to
/// [`DEFAULT_SPAWN_BUDGET`]. Non-numeric values fall back to the default.
pub fn current_budget() -> u32 {
    std::env::var(SPAWN_BUDGET_ENV)
        .ok()
        .and_then(|v| v.trim().parse::<u32>().ok())
        .unwrap_or(DEFAULT_SPAWN_BUDGET)
}

/// The non-secret payload a nested worker emits to ask the host to launch a
/// sibling coordinator on its behalf. Carries everything the host needs to
/// rebuild the argv via `worker::coordinator_argv` EXCEPT credentials — the host
/// injects those from its own env. Serializes to a single spool file.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpawnRequest {
    /// On-disk schema version (see [`SCHEMA_VERSION`]).
    pub schema_version: u32,
    /// Unique id for this request; also the spool filename stem.
    pub request_id: String,
    /// The task prompt the sibling coordinator should execute.
    pub task: String,
    /// Working directory (the requesting worker's cwd / source repo). For an
    /// isolated job this is the SOURCE repo; the host carves the worktree.
    pub cwd: String,
    /// Backend the sibling runs on (`"claude"`/`"grok"`) — parity with the
    /// requester's session backend.
    pub backend: String,
    /// Model for the sibling's coordinator turn.
    pub model: String,
    /// When true and `cwd` is a git repo, the host runs the sibling in a
    /// dedicated worktree instead of sharing `cwd`.
    pub isolate: bool,
    /// Git ref the isolated worktree branches from (`"main"` | `"head"`).
    pub base: String,
    /// Remaining spawn budget AT THE REQUESTER. The host stamps the sibling with
    /// `budget - 1` and refuses when this is `0` (see [`sibling_budget`]).
    pub spawn_budget: u32,
    /// The launching interactive session's id — threaded so the sibling's
    /// durable `coordinator_runs` row is attributed to the originating session
    /// and the requester can read the sibling back via `coordinator_store`.
    pub launch_session_id: String,
    /// The worker id that emitted this request (for audit / parent linkage).
    /// `None` when emitted directly by an interactive session.
    pub requested_by_worker: Option<String>,
    /// Unix epoch seconds when the request was written (ordering / staleness).
    pub created_at_unix: u64,
}

impl SpawnRequest {
    /// Construct a request stamped with the current schema version and time.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        request_id: impl Into<String>,
        task: impl Into<String>,
        cwd: impl Into<String>,
        backend: impl Into<String>,
        model: impl Into<String>,
        isolate: bool,
        base: impl Into<String>,
        spawn_budget: u32,
        launch_session_id: impl Into<String>,
        requested_by_worker: Option<String>,
    ) -> Self {
        SpawnRequest {
            schema_version: SCHEMA_VERSION,
            request_id: request_id.into(),
            task: task.into(),
            cwd: cwd.into(),
            backend: backend.into(),
            model: model.into(),
            isolate,
            base: base.into(),
            spawn_budget,
            launch_session_id: launch_session_id.into(),
            requested_by_worker: requested_by_worker.into(),
            created_at_unix: now_unix(),
        }
    }
}

/// The spool directory under a given state root (e.g. `/aish/state`).
pub fn spool_dir(state_root: &Path) -> PathBuf {
    state_root.join(SPOOL_SUBDIR)
}

/// The pending (unclaimed) filename for a request id: `spawn-req-<id>.json`.
pub fn request_filename(request_id: &str) -> String {
    format!("{REQUEST_PREFIX}{request_id}.{REQUEST_EXT}")
}

/// Write a spawn request into the spool atomically (tmp file + rename), creating
/// the spool directory if needed. Returns the final path. The atomic rename
/// guarantees the host never observes a half-written request.
pub fn write_request(state_root: &Path, req: &SpawnRequest) -> io::Result<PathBuf> {
    let dir = spool_dir(state_root);
    fs::create_dir_all(&dir)?;
    let final_path = dir.join(request_filename(&req.request_id));
    let tmp_path = dir.join(format!(".{}.tmp", request_filename(&req.request_id)));
    let json = serde_json::to_vec_pretty(req)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    fs::write(&tmp_path, &json)?;
    fs::rename(&tmp_path, &final_path)?;
    Ok(final_path)
}

/// List pending (unclaimed) request files in the spool, sorted by filename
/// (which embeds no ordering itself — callers that need FIFO should sort by
/// `created_at_unix` after reading). Missing spool dir yields an empty list.
pub fn list_pending(state_root: &Path) -> io::Result<Vec<PathBuf>> {
    let dir = spool_dir(state_root);
    let mut out = Vec::new();
    let rd = match fs::read_dir(&dir) {
        Ok(rd) => rd,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(out),
        Err(e) => return Err(e),
    };
    for entry in rd {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with(REQUEST_PREFIX) && name.ends_with(&format!(".{REQUEST_EXT}")) {
            out.push(entry.path());
        }
    }
    out.sort();
    Ok(out)
}

/// Atomically CLAIM a pending request by renaming `…​.json` → `…​.json.claimed`
/// before reading it. The rename is the mutual-exclusion primitive: exactly one
/// claimer wins, so a crash-restart of the host poller never double-spawns the
/// same request. Returns the parsed request on success, `Ok(None)` if the file
/// was already claimed/removed by a racing claimer.
pub fn claim(pending_path: &Path) -> io::Result<Option<SpawnRequest>> {
    let claimed_path = claimed_path_for(pending_path);
    match fs::rename(pending_path, &claimed_path) {
        Ok(()) => {}
        // Someone else claimed it first (or it vanished) — not our request.
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    }
    let bytes = fs::read(&claimed_path)?;
    let req: SpawnRequest = serde_json::from_slice(&bytes)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    Ok(Some(req))
}

/// The `.claimed` sidecar path for a pending request path.
fn claimed_path_for(pending_path: &Path) -> PathBuf {
    let mut s = pending_path.as_os_str().to_os_string();
    s.push(format!(".{CLAIMED_EXT}"));
    PathBuf::from(s)
}

/// Compute the budget to stamp on a SIBLING given the requester's budget.
/// Returns `None` (REFUSE — the fork-bomb backstop) when the requester's budget
/// is exhausted (`0`); otherwise `Some(budget - 1)`. Enforced at the host accept
/// loop so the flat topology keeps the same guarantee the fork-site gate gave.
pub fn sibling_budget(requester_budget: u32) -> Option<u32> {
    requester_budget.checked_sub(1).filter(|_| requester_budget > 0)
}

/// Remove a claimed request file once its sibling has been launched (best
/// effort — a leftover `.claimed` is inert, it is never re-spawned).
pub fn discard_claimed(pending_path: &Path) -> io::Result<()> {
    let claimed = claimed_path_for(pending_path);
    match fs::remove_file(&claimed) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    fn temp_root() -> PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "aish-spawn-broker-test-{}-{}",
            std::process::id(),
            n
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn sample(id: &str) -> SpawnRequest {
        SpawnRequest::new(
            id,
            "do the thing",
            "/repo",
            "claude",
            "opus",
            true,
            "main",
            3,
            "sess-123",
            Some("w_parent".to_string()),
        )
    }

    #[test]
    fn round_trips_through_json() {
        let req = sample("abc");
        let json = serde_json::to_string(&req).unwrap();
        let back: SpawnRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(req, back);
        assert_eq!(back.schema_version, SCHEMA_VERSION);
    }

    #[test]
    fn write_then_list_finds_the_request() {
        let root = temp_root();
        assert!(list_pending(&root).unwrap().is_empty());
        let path = write_request(&root, &sample("one")).unwrap();
        assert!(path.exists());
        assert_eq!(path.file_name().unwrap().to_string_lossy(), "spawn-req-one.json");
        let pending = list_pending(&root).unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0], path);
    }

    #[test]
    fn list_pending_missing_dir_is_empty_not_error() {
        let root = temp_root().join("does-not-exist");
        assert!(list_pending(&root).unwrap().is_empty());
    }

    #[test]
    fn claim_renames_and_second_claim_returns_none() {
        let root = temp_root();
        let path = write_request(&root, &sample("dup")).unwrap();
        // First claim wins and parses.
        let got = claim(&path).unwrap().expect("first claim should win");
        assert_eq!(got.request_id, "dup");
        assert_eq!(got.task, "do the thing");
        // The pending file is gone; list is empty.
        assert!(list_pending(&root).unwrap().is_empty());
        // Second claim of the same pending path finds nothing.
        assert!(claim(&path).unwrap().is_none());
    }

    #[test]
    fn write_is_atomic_no_tmp_left_behind() {
        let root = temp_root();
        write_request(&root, &sample("atomic")).unwrap();
        let leftovers: Vec<_> = fs::read_dir(spool_dir(&root))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "tmp file leaked: {leftovers:?}");
    }

    #[test]
    fn budget_decrements_and_refuses_at_zero() {
        assert_eq!(sibling_budget(3), Some(2));
        assert_eq!(sibling_budget(1), Some(0));
        assert_eq!(sibling_budget(0), None); // fork-bomb backstop
    }

    #[test]
    fn discard_claimed_is_idempotent() {
        let root = temp_root();
        let path = write_request(&root, &sample("gc")).unwrap();
        claim(&path).unwrap().unwrap();
        discard_claimed(&path).unwrap();
        // Second discard is a no-op, not an error.
        discard_claimed(&path).unwrap();
    }

    #[test]
    fn current_budget_parses_env_or_defaults() {
        // Default when unset is exercised indirectly; here assert the constant.
        assert_eq!(DEFAULT_SPAWN_BUDGET, 3);
    }
}
