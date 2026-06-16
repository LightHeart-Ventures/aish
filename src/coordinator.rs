//! Durable, resumable background coordinator — the DEFAULT background path.
//!
//! Ported from atum_cli's batch-coordinator (`batch-coordinator.ts` /
//! `batch-controller-{plan,round}.ts` / `batch-controller-store.ts`). Where
//! `batch.rs` offloads ONE tool-less request to the Anthropic Batches API, the
//! coordinator is a multi-round agentic loop that:
//!   * runs full-tool turns locally (filesystem, run_program, MCP), AND
//!   * fans heavy, latency-insensitive sub-work out to the Batches API,
//! persisting its phase to SQLite so a crash/exit resumes instead of re-running.
//!
//! ## Phase state machine (borrowed from atum's `runCoordinator`)
//! ```text
//!                ┌──────────────┐  spawn batch   ┌────────────────┐
//!   start ─────► │ coordinating │ ─────────────► │ awaiting_batch │
//!                └──────────────┘ ◄───────────── └────────────────┘
//!                   │       ▲       fold results
//!          done     │       │ (loop another round)
//!                   ▼       │
//!                ┌──────┐   │  cap / error
//!                │ done │   └──────────► failed
//!                └──────┘
//! ```
//! `coordinating` — running agentic turns; the default resting phase.
//! `awaiting_batch` — blocked on a spawned Batches job; heartbeats while it polls.
//! `done` / `failed` — terminal. A `done` row is returned idempotently on resume.
//!
//! ## What aish adapts vs. atum
//! atum injects the agent step + a real Batches client as seams and runs in a
//! container with an external orchestrator lease. aish has neither: a single
//! agentic turn IS `engine::run_turn` (which itself executes tools locally and,
//! when batch-mode tools fire, spawns Batches jobs into `session.batch_jobs`).
//! So aish's "round" = one `run_turn` followed by awaiting any batches that turn
//! spawned. We don't reproduce atum's separate `round`/`plan` tables — the model
//! drives plan→map→reduce inside its own transcript; we persist the coarse phase
//! (the irreducible resume signal) and a heartbeat, which is the reviewable core.
//!
//! TODO(coordinator): atum reattaches to a specific in-flight Anthropic batch id
//! across a process restart (per-round `batchId` persisted before polling). aish
//! already does that reattach for top-level batches via `batch::rehydrate`, but a
//! coordinator that *crashes mid-round* loses its in-memory transcript, so on
//! restart we surface/reap the run rather than resuming its conversation. Full
//! transcript persistence (atum's `saveStep`) is the next increment.

use crate::backend::Backend;
use crate::db::CoordinatorStore;
use crate::session::Session;
use crate::tools;
use std::time::Duration;

/// Upper bound on agentic rounds (a misbehaving model can't spin forever).
/// Mirrors atum's `DEFAULT_MAX_STEPS` backstop, scaled for a shell session.
/// Bounded but generous: real multi-file work (rewrite a crate, iterate to a
/// green build) needs many rounds, and the parent-side stdout capture is already
/// capped at 1MB (`worker::read_capped`), so rounds aren't the OOM lever — a
/// runaway still terminates here. Most work completes well under this.
const MAX_ROUNDS: usize = 36;

/// How often to beat the run's durable heartbeat while awaiting batches — proof
/// of liveness so a restart can tell a live run from an orphaned one. Matches
/// atum's `DEFAULT_HEARTBEAT_INTERVAL_MS`.
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(30);

/// A run is considered orphaned at startup when its owner is gone and its last
/// heartbeat is older than this. Generous so a momentarily-paused awaiting run
/// (a long batch poll) is never falsely reaped.
const ORPHAN_STALE_AFTER: Duration = Duration::from_secs(15 * 60);

/// The coordinator's phase — the durable resume signal. String-backed in SQLite
/// (the `coordinator_runs.phase` CHECK constraint), mirrored here as a type so
/// the transition logic is total and testable (atum keeps it an open string;
/// aish closes the set since the phases are fixed).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// Running agentic turns — the default resting phase.
    Coordinating,
    /// Blocked on a spawned Batches job; heartbeating while it polls.
    AwaitingBatch,
    /// Terminal: the task finished, `result` holds the assembled output.
    Done,
    /// Terminal: the run hit the round cap, errored, or was orphaned.
    Failed,
}

impl Phase {
    /// The SQLite string form (the `coordinator_runs.phase` column values).
    pub fn as_str(self) -> &'static str {
        match self {
            Phase::Coordinating => "coordinating",
            Phase::AwaitingBatch => "awaiting_batch",
            Phase::Done => "done",
            Phase::Failed => "failed",
        }
    }

    /// Parse a stored phase string. Unknown/legacy values map to `Failed` so a
    /// row we can't interpret is treated as a dead run, never resumed blindly.
    pub fn parse(s: &str) -> Phase {
        match s {
            "coordinating" => Phase::Coordinating,
            "awaiting_batch" => Phase::AwaitingBatch,
            "done" => Phase::Done,
            _ => Phase::Failed,
        }
    }

    /// Terminal phases never transition again.
    // Part of the phase-machine's documented surface and exercised by the resume
    // contract test; the in-process resume increment (see the module TODO) is the
    // first non-test caller. `allow(dead_code)` keeps it without churn until then.
    #[allow(dead_code)]
    pub fn is_terminal(self) -> bool {
        matches!(self, Phase::Done | Phase::Failed)
    }

    /// Whether a *resumed* run in this phase can keep running, or is finished.
    /// Borrowed from atum's resume contract: `done` → return stored result;
    /// `coordinating`/`awaiting_batch` → continue; `failed` → terminal.
    #[allow(dead_code)]
    pub fn is_resumable(self) -> bool {
        matches!(self, Phase::Coordinating | Phase::AwaitingBatch)
    }
}

/// Outcome of driving a coordinator run to a terminal state. Mirrors atum's
/// `CoordinatorResult`.
pub struct Outcome {
    pub phase: Phase,
    pub result: Option<String>,
    pub error: Option<String>,
    /// Rounds executed before reaching a terminal phase — informative for logs
    /// and the resume increment; not yet surfaced by the headless caller.
    #[allow(dead_code)]
    pub rounds: usize,
}

/// Drive a coordinator run to a terminal state, persisting phase transitions to
/// `store` so a restart resumes. This is the headless `--coordinator` body
/// (called by `engine::run_coordinator`): it runs full-tool agentic rounds and,
/// after each round, awaits any Anthropic batches that round spawned (folding
/// them back is implicit — their results auto-print and the next round's turn
/// sees them in history). A round that spawns no batch and produces a final text
/// answer ends the run.
///
/// Adapted from atum's `runCoordinator` loop, collapsed to aish's model where a
/// single `run_turn` IS the agent step and batch fan-out happens inside it.
pub async fn drive(
    backend: &Backend,
    session: &mut Session,
    input: String,
    run_id: &str,
    store: Option<&CoordinatorStore>,
) -> Outcome {
    if let Some(s) = store {
        // session.session_id/name were adopted from the LAUNCHING session at
        // startup (see main.rs), so the row attributes to who asked for the work.
        let _ = s.insert(run_id, &input, &session.session_id, session.name.as_deref());
    }

    let mut rounds = 0usize;
    // The coordinator's model HAS the full toolset (run_turn passes tool_defs),
    // but a model handed a big task headless sometimes rationalizes "I'm a
    // text-only assistant without file access" and refuses on turn 1 instead of
    // calling read_file. Lead the first turn with an explicit assertion of its
    // capabilities to head that off; later rounds use the fold-results message.
    let mut next_input = format!(
        "You are running headless as an autonomous aish coordinator in {cwd}. You have your FULL \
toolset RIGHT NOW — read_file, write_file, list_dir, change_dir, run_program (build, test, git, \
gh, anything), and the connected MCP servers. You CAN read and edit files and run commands on \
this machine. Do NOT claim to be a text-only assistant or that you lack access — call the tools \
and actually do the work, then report what you did with concrete evidence (command output, exit \
codes, diffs).\n\nTASK:\n{input}",
        cwd = session.cwd.display(),
    );

    loop {
        if rounds >= MAX_ROUNDS {
            let error = format!("coordinator exceeded the {MAX_ROUNDS}-round cap");
            if let Some(s) = store {
                let _ = s.set_failed(run_id, &error);
            }
            return Outcome { phase: Phase::Failed, result: None, error: Some(error), rounds };
        }

        // ── coordinating: one full-tool agentic turn ────────────────────────
        if let Some(s) = store {
            let _ = s.set_phase(run_id, Phase::Coordinating.as_str());
        }
        let mut allow = |_: &str| tools::Decision::AllowOnce;
        let turn = crate::engine::run_turn(backend, session, next_input, &mut allow).await;
        rounds += 1;

        let answer = match turn {
            Ok(a) => a,
            Err(e) => {
                let error = format!("{e:#}");
                if let Some(s) = store {
                    let _ = s.set_failed(run_id, &error);
                }
                return Outcome { phase: Phase::Failed, result: None, error: Some(error), rounds };
            }
        };

        // ── awaiting_batch: did this round fan work out to the Batches API? ──
        // The model offloads heavy sub-work via the run_in_background→batch path,
        // which lands jobs in `session.batch_jobs`. If any are running, we are in
        // the awaiting_batch phase: persist it, heartbeat, and block until they
        // finish (their results auto-print; the next round's turn sees them).
        if crate::batch::running_count(&session.batch_jobs) > 0 {
            if let Some(s) = store {
                let _ = s.set_phase(run_id, Phase::AwaitingBatch.as_str());
            }
            // Forwarded to the watcher (via the `📦` sentinel) as the batch-vs-
            // standard indicator: this round fanned work out to the Batches API.
            let n = crate::batch::running_count(&session.batch_jobs);
            eprintln!("📦 fanned {n} sub-task(s) out to the Batches API; awaiting results");
            await_batches_with_heartbeat(session, run_id, store).await;

            // Fold the batch results back: feed the just-completed sub-work into
            // the next round so the model can reduce over it. The results were
            // surfaced inline; this round-trips the coordinator back to
            // `coordinating` to assemble/continue.
            next_input = "The background sub-tasks you offloaded have completed (their results were \
delivered above). Fold them into your work: continue the task, or give the final answer if done."
                .to_string();
            continue;
        }

        // No pending sub-work and the turn produced a text answer → done.
        if let Some(s) = store {
            let _ = s.set_done(run_id, &answer);
        }
        return Outcome { phase: Phase::Done, result: Some(answer), error: None, rounds };
    }
}

/// Block until every spawned batch reaches a terminal state, beating the run's
/// durable heartbeat on `HEARTBEAT_INTERVAL` so a long poll doesn't look like a
/// dead run. This is aish's analogue of atum's `collectBatch` keep-alive timer:
/// a batch wait has no latency SLA, so liveness must be stamped while we wait.
/// The heartbeat is best-effort — a store error never stalls or sinks the wait.
async fn await_batches_with_heartbeat(
    session: &Session,
    run_id: &str,
    store: Option<&CoordinatorStore>,
) {
    let mut last_beat = tokio::time::Instant::now();
    loop {
        if crate::batch::running_count(&session.batch_jobs) == 0 {
            return;
        }
        if last_beat.elapsed() >= HEARTBEAT_INTERVAL {
            if let Some(s) = store {
                let _ = s.heartbeat(run_id);
            }
            last_beat = tokio::time::Instant::now();
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

/// Startup reattach for `coordinator_runs` (mirrors `batch::rehydrate`).
///
/// A coordinator runs in a child process (or this one when re-exec'd headless),
/// and its live transcript is in-memory, so unlike a platform-side batch we can't
/// reattach to a crashed run's conversation. Instead we:
///   * surface every `done` run's result to the terminal (so a completed
///     background job isn't silently lost across a restart), and
///   * reap orphaned runs — a non-terminal row whose owning session is gone and
///     whose heartbeat is stale — by stamping `failed`, so they don't linger as
///     phantom "running" entries in `background_status`.
///
/// Runs belonging to a *live* owner (another running aish, by session id) or with
/// a fresh heartbeat are left untouched. Idempotent: already-surfaced/terminal
/// rows are cleared, so a second start is a no-op.
pub fn rehydrate(session: &mut Session) {
    // Best-effort: prune git's record of worktrees whose directories are gone
    // (e.g. a crashed isolated worker left a dangling registration). Cheap and
    // safe — `git worktree prune` only drops already-missing entries.
    // TODO(worktree): also `git worktree remove` empty leftover worktree dirs from
    // crashed runs (those with a live dir but no commits) — needs a per-dir scan.
    crate::worker::prune_worktrees(&session.cwd);

    let Some(store) = session.coordinator_store.clone() else {
        return;
    };
    let rows = match store.load_all() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("\x1b[2maish: couldn't load saved coordinator runs: {e:#}\x1b[0m");
            return;
        }
    };
    let own = session.session_id.clone();
    let mut surfaced = 0usize;
    let mut reaped = 0usize;
    for row in rows {
        let phase = Phase::parse(&row.phase);
        match phase {
            Phase::Done => {
                // Surface a completed background coordinator result once, then
                // clear it so we don't re-announce on the next start.
                if let Some(result) = &row.result {
                    if !result.trim().is_empty() {
                        print_completed(&row.run_id, result);
                        surfaced += 1;
                    }
                }
            }
            Phase::Failed => {} // terminal, nothing to do (cleared below)
            Phase::Coordinating | Phase::AwaitingBatch => {
                // Non-terminal. If it's ours (same session id — only possible
                // after an in-process resume, since ids are per-run) or its
                // heartbeat is fresh, leave it; otherwise it's orphaned.
                let mine = row.session_id.as_deref() == Some(own.as_str());
                if !mine && heartbeat_is_stale(row.heartbeat_at.as_deref()) {
                    let _ = store.set_failed(
                        &row.run_id,
                        "orphaned: owner gone and heartbeat stale (reaped on startup)",
                    );
                    reaped += 1;
                }
            }
        }
    }
    // Drop terminal rows (the just-surfaced done runs and any failed/reaped ones)
    // so the store doesn't grow without bound and a restart stays a no-op.
    let _ = store.clear_finished();
    if surfaced > 0 || reaped > 0 {
        eprintln!(
            "\x1b[2maish: reattached coordinator runs ({surfaced} delivered, {reaped} reaped)\x1b[0m"
        );
    }
}

/// Print a completed coordinator result above the prompt, matching the batch /
/// worker completion block style so it reads consistently.
fn print_completed(run_id: &str, result: &str) {
    if crate::present::deferred() {
        // Interactive REPL: let the presenter own the prompt; a dim inline note
        // is enough (the full result is recoverable via background_status/store).
        crate::tools::announce(
            &format!("[{}]", crate::batch::short_id(run_id)),
            "background coordinator finished while away",
        );
        return;
    }
    println!(
        "\x1b[2m── coordinator {} complete ──\x1b[0m\n{}",
        crate::batch::short_id(run_id),
        crate::md::render_stdout(result.trim())
    );
}

/// True when a heartbeat timestamp is older than `ORPHAN_STALE_AFTER` (or
/// missing/unparseable). Compares against SQLite `current_timestamp` strings,
/// which are UTC `"YYYY-MM-DD HH:MM:SS"`.
fn heartbeat_is_stale(heartbeat_at: Option<&str>) -> bool {
    let Some(hb) = heartbeat_at else {
        return true; // no beat recorded → treat as stale
    };
    match parse_sqlite_timestamp(hb) {
        Some(beat_secs) => {
            let now = now_unix_secs();
            now.saturating_sub(beat_secs) > ORPHAN_STALE_AFTER.as_secs() as i64
        }
        None => true, // unparseable → stale (don't keep a row we can't reason about)
    }
}

/// Current UTC time as unix seconds.
fn now_unix_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Parse SQLite's `current_timestamp` form — `"YYYY-MM-DD HH:MM:SS"` in UTC —
/// to unix seconds, with a plain civil-date computation (no chrono dependency).
/// Returns `None` on any malformed field.
fn parse_sqlite_timestamp(s: &str) -> Option<i64> {
    let (date, time) = s.trim().split_once(' ')?;
    let mut d = date.splitn(3, '-');
    let year: i64 = d.next()?.parse().ok()?;
    let month: i64 = d.next()?.parse().ok()?;
    let day: i64 = d.next()?.parse().ok()?;
    let mut t = time.splitn(3, ':');
    let hour: i64 = t.next()?.parse().ok()?;
    let min: i64 = t.next()?.parse().ok()?;
    let sec: i64 = t.next().unwrap_or("0").parse().ok()?;
    Some(civil_to_unix(year, month, day, hour, min, sec))
}

/// Days-from-civil → unix seconds. Howard Hinnant's `days_from_civil` algorithm
/// (public domain), so we don't pull in a date crate just for orphan detection.
fn civil_to_unix(year: i64, month: i64, day: i64, hour: i64, min: i64, sec: i64) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let doy = (153 * (if month > 2 { month - 3 } else { month + 9 }) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    days * 86_400 + hour * 3_600 + min * 60 + sec
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn phase_string_roundtrip_is_total() {
        for p in [Phase::Coordinating, Phase::AwaitingBatch, Phase::Done, Phase::Failed] {
            assert_eq!(Phase::parse(p.as_str()), p);
        }
        // Unknown/legacy phase strings are treated as a dead run.
        assert_eq!(Phase::parse("planning"), Phase::Failed);
        assert_eq!(Phase::parse(""), Phase::Failed);
    }

    #[test]
    fn terminal_and_resumable_partition_the_phases() {
        assert!(!Phase::Coordinating.is_terminal() && Phase::Coordinating.is_resumable());
        assert!(!Phase::AwaitingBatch.is_terminal() && Phase::AwaitingBatch.is_resumable());
        assert!(Phase::Done.is_terminal() && !Phase::Done.is_resumable());
        assert!(Phase::Failed.is_terminal() && !Phase::Failed.is_resumable());
    }

    #[test]
    fn sqlite_timestamp_parses_to_unix() {
        // 1970-01-01 00:00:00 is the unix epoch.
        assert_eq!(parse_sqlite_timestamp("1970-01-01 00:00:00"), Some(0));
        // A known instant: 2021-01-01 00:00:00 UTC = 1609459200.
        assert_eq!(parse_sqlite_timestamp("2021-01-01 00:00:00"), Some(1_609_459_200));
        // Leap-year day handled by the civil algorithm.
        assert_eq!(parse_sqlite_timestamp("2020-02-29 12:00:00"), Some(1_582_977_600));
        // Malformed → None.
        assert_eq!(parse_sqlite_timestamp("not a timestamp"), None);
        assert_eq!(parse_sqlite_timestamp("2021-13"), None);
    }

    #[test]
    fn fresh_heartbeat_is_not_stale_missing_is() {
        // A heartbeat "now" is fresh.
        let now = now_unix_secs();
        let fresh = unix_to_sqlite(now);
        assert!(!heartbeat_is_stale(Some(&fresh)));
        // An ancient heartbeat is stale.
        let old = unix_to_sqlite(now - 60 * 60); // an hour ago > 15m
        assert!(heartbeat_is_stale(Some(&old)));
        // Missing/garbage heartbeats are stale.
        assert!(heartbeat_is_stale(None));
        assert!(heartbeat_is_stale(Some("garbage")));
    }

    /// Test helper: unix seconds → SQLite `current_timestamp` string (UTC).
    /// Inverse of `parse_sqlite_timestamp`, used only to build test fixtures.
    fn unix_to_sqlite(secs: i64) -> String {
        let days = secs.div_euclid(86_400);
        let rem = secs.rem_euclid(86_400);
        let (h, m, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
        // Inverse civil algorithm (Hinnant's civil_from_days).
        let z = days + 719_468;
        let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
        let doe = z - era * 146_097;
        let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
        let y = yoe + era * 400;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let d = doy - (153 * mp + 2) / 5 + 1;
        let mth = if mp < 10 { mp + 3 } else { mp - 9 };
        let year = if mth <= 2 { y + 1 } else { y };
        format!("{year:04}-{mth:02}-{d:02} {h:02}:{m:02}:{s:02}")
    }
}
