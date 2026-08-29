//! Durable store for background-coordinator runs — extracted from `db.rs` so
//! all SQLite access for the coordinator lifecycle lives behind ONE module with
//! typed accessors (no raw SQL leaks into `coordinator.rs` or the REPL).
//!
//! Backs the resumable multi-round background coordinator (the default
//! background path): the `coordinator_runs` table (run lifecycle + metrics +
//! stand-down flag), the `coordinator_messages` mailbox (the `:tell` channel),
//! and the immutable `run_aliases` binding (TASK-205 keyed result lookup).
//!
//! Kept in its own connection (the coordinator drives turns + batch waits off
//! the main thread) against the same `aish.db`; WAL makes the concurrent
//! connections safe. The store is `Clone` so the running coordinator and the
//! REPL both hold a handle.

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, Transaction};
use std::path::Path;
use std::sync::{Arc, Mutex};

/// One persisted coordinator run. Mirrors `BatchRow`, but for a durable,
/// resumable multi-round background coordinator (the default background path)
/// rather than a single Anthropic batch.
/// TASK-325 — aggregate token-spend across all persisted coordinator runs; the
/// data behind the `:tokens` dashboard. `tokens_in`/`tokens_out` are summed from
/// every run's stamped metrics (TASK-285); `ratio()` gives input:output, which
/// climbs when prompts bloat or caching regresses. Pre-metrics / crashed-before-
/// finish rows contribute zero but are still counted in `runs`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CostSummary {
    pub runs: i64,
    pub tokens_in: i64,
    pub tokens_out: i64,
    pub turns: i64,
    pub tool_calls: i64,
}

impl CostSummary {
    pub fn total_tokens(self) -> i64 {
        self.tokens_in + self.tokens_out
    }
    /// Input:output token ratio (0.0 when no output has been recorded).
    pub fn ratio(self) -> f64 {
        if self.tokens_out == 0 {
            0.0
        } else {
            self.tokens_in as f64 / self.tokens_out as f64
        }
    }
}


pub struct CoordinatorRow {
    pub run_id: String,
    pub task: String,
    /// 'coordinating' | 'awaiting_batch' | 'checkpoint' | 'done' | 'failed'.
    pub phase: String,
    pub result: Option<String>,
    /// Failure reason when phase='failed'. Persisted for rehydrate/diagnostics;
    /// `background_status` shows the phase, not the message, so it's read only by
    /// the store round-trip test today.
    #[allow(dead_code)]
    pub error: Option<String>,
    /// Owning session (uuid) — the LAUNCHING interactive session, used both to
    /// detect orphaned runs at startup and to mark "your" rows in `:workers`.
    pub session_id: Option<String>,
    /// The launching session's friendly name (`:rename`), for display.
    pub session_name: Option<String>,
    /// Parent coordinator's run id when this run was spawned BY another
    /// coordinator (via `run_in_background`), else `None` for a run launched
    /// directly from an interactive REPL. Stamped from `AISH_PARENT_RUN_ID`
    /// (see `worker::worker_command`). Drives the hierarchical `:workers`
    /// forest — a child indents beneath its parent.
    pub parent_run_id: Option<String>,
    pub created_at: Option<String>,
    /// Last liveness beat (SQLite `current_timestamp` string). A run whose owner
    /// is gone and whose heartbeat is stale is treated as orphaned on reattach.
    pub heartbeat_at: Option<String>,
    /// Cumulative prompt (input) tokens billed across the run, captured from the
    /// coordinator session at terminal exit. 0 for rows created before the
    /// metrics migration or for runs that never took a turn.
    pub tokens_in: u64,
    /// Cumulative completion (output) tokens produced across the run.
    pub tokens_out: u64,
    /// Count of agentic turns taken across the run.
    pub turns: u64,
    /// Count of tool calls executed across the run.
    pub tool_calls: u64,
}

/// Cumulative cost/effort counters for a run, persisted ATOMICALLY with the
/// terminal phase transition (TASK-285). Grouping them into the SAME
/// transaction as the `done`/`failed` write (via [`CoordinatorStore::finish_run`])
/// means a crash mid-write can never tear the row into a half-state — a terminal
/// phase with stale/zero metrics, or fresh metrics under a still-`coordinating`
/// phase. The whole turn-outcome commit is all-or-nothing, so a re-read after a
/// rolled-back write sees the prior (resumable) row unchanged.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RunMetrics {
    /// Prompt tokens consumed across the run.
    pub tokens_in: u64,
    /// Completion tokens produced across the run.
    pub tokens_out: u64,
    /// Agentic turns taken.
    pub turns: u64,
    /// Tool calls issued.
    pub tool_calls: u64,
}

/// One run's terminal payload, read STRICTLY by `run_id` (TASK-205). A single
/// keyed row read with no shared/global "last result" slot, so a concurrent
/// completion of another run can never bleed into this lookup.
#[derive(Debug, Clone, PartialEq)]
pub struct RunResult {
    pub run_id: String,
    /// 'coordinating' | 'awaiting_batch' | 'checkpoint' | 'done' | 'failed'.
    pub phase: String,
    pub result: Option<String>,
    pub error: Option<String>,
}

impl RunResult {
    /// Render the run's own result for display — the done answer, a failure
    /// note, or a still-running status. Mirrors `worker::WorkerJob::fetch`.
    #[allow(dead_code)] // Public accessor kept for API parity; no call site today.
    pub fn rendered(&self) -> String {
        match self.phase.as_str() {
            "done" => self.result.clone().unwrap_or_else(|| "(empty result)".into()),
            "failed" => format!(
                "run {} failed: {}",
                self.run_id,
                self.error.clone().unwrap_or_else(|| "unknown error".into())
            ),
            other => format!("run {} is still running (phase: {other}).", self.run_id),
        }
    }
}

/// One row of the TASK-289 `coordinator_registry` — a live coordinator PROCESS,
/// captured for parent-death recovery + Batches resume. Distinct from
/// [`CoordinatorRow`] (the per-run phase/result view): a registry row is keyed
/// by the coordinator's `coord_id` and records the OS `pid`, the `generation`
/// (restart counter), the in-flight Anthropic `batch_job_id` (the resume
/// handle), the coarse `phase`, and the launching `owner_session`.
#[derive(Debug, Clone, PartialEq)]
pub struct CoordinatorRegistryRow {
    pub coord_id: String,
    /// Restart counter — bumped each time this coord_id is re-registered by a
    /// resurrected process, so a stale row can be told from a fresh generation.
    pub generation: i64,
    /// OS process id of the coordinator. Scanned at startup: a pid that is no
    /// longer alive marks the row `orphaned`.
    pub pid: i64,
    /// In-flight Anthropic Batches job id, when the coordinator is awaiting one.
    /// Its presence is what makes an orphaned row RESURRECTABLE (TASK-291).
    pub batch_job_id: Option<String>,
    /// Coarse lifecycle phase mirror (e.g. `coordinating`, `awaiting_batch`,
    /// `orphaned`). Set to `orphaned` by the startup reaper.
    pub phase: String,
    pub started_at: Option<String>,
    /// Launching interactive session (uuid) — used to attribute a row and to
    /// tell "our" runs from another session's at startup.
    pub owner_session: Option<String>,
}

/// Durable store for background coordinator runs — the resumable equivalent of
/// `BatchStore`, ported from atum_cli's batch-controller store. Kept in its own
/// connection (the coordinator drives turns + batch waits off the main thread)
/// against the same `aish.db`; WAL makes the concurrent connections safe.
/// Cloneable so the running coordinator and the REPL both hold a handle.
#[derive(Clone)]
pub struct CoordinatorStore {
    conn: Arc<Mutex<Connection>>,
}

impl CoordinatorStore {
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path)
            .with_context(|| format!("can't open coordinator store at {}", path.display()))?;
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA busy_timeout = 5000;
             PRAGMA synchronous = NORMAL;
             CREATE TABLE IF NOT EXISTS coordinator_runs (
                 run_id       TEXT PRIMARY KEY,
                 task         TEXT NOT NULL,
                 phase        TEXT NOT NULL CHECK (phase IN
                              ('coordinating', 'awaiting_batch', 'checkpoint', 'done', 'failed')),
                 result       TEXT,
                 error        TEXT,
                 session_id   TEXT,
                 session_name TEXT,
                 created_at   TEXT NOT NULL DEFAULT current_timestamp,
                 heartbeat_at TEXT NOT NULL DEFAULT current_timestamp,
                 -- Stand-down control flag (the `:stop` / `stop` channel — a
                 -- harsher sibling of `:tell`). When a parent raises it, the
                 -- live coordinator takes ONE final graceful wrap-up turn at its
                 -- next round boundary and then terminates (see
                 -- `coordinator::drive`). 0 = run normally, 1 = stand down.
                 stand_down   INTEGER NOT NULL DEFAULT 0,
                 -- Checkpoint control flag (the `:checkpoint` channel, TASK-294 —
                 -- a resumable-PAUSE sibling of `:stop`). When a parent raises it,
                 -- the live coordinator halts at its next round boundary WITHOUT
                 -- finishing, persists phase='checkpoint', and returns; the run can
                 -- be resumed manually later. 0 = run normally, 1 = pause.
                 checkpoint   INTEGER NOT NULL DEFAULT 0
             );
             -- Operator → coordinator mailbox (the :tell / SendMessage channel).
             -- A row is a clarification/instruction queued for an in-flight run;
             -- the coordinator drains (and deletes) its messages at each round
             -- boundary. Indexed by run_id since every read is run-scoped.
             CREATE TABLE IF NOT EXISTS coordinator_messages (
                 id           INTEGER PRIMARY KEY,
                 run_id       TEXT NOT NULL,
                 message      TEXT NOT NULL,
                 from_session TEXT,
                 created_at   TEXT NOT NULL DEFAULT current_timestamp
             );
             CREATE INDEX IF NOT EXISTS idx_coord_msg_run
                 ON coordinator_messages (run_id);
             -- TASK-205: immutable alias->run_id binding, written ONCE at run
             -- start. `:result <alias>` resolves alias->run_id then reads that
             -- run's own result strictly by run_id -- never a shared/global
             -- result slot -- so concurrent worker completions can't corrupt a
             -- lookup. The alias row is never mutated after creation.
             CREATE TABLE IF NOT EXISTS run_aliases (
                 alias      TEXT PRIMARY KEY,
                 run_id     TEXT NOT NULL,
                 pr         TEXT,
                 created_at TEXT NOT NULL DEFAULT current_timestamp
             );
             CREATE INDEX IF NOT EXISTS idx_run_aliases_run
                 ON run_aliases (run_id);
             -- TASK-289: durable coordinator registry for parent-death recovery
             -- + Batches resume. One row per live coordinator PROCESS (distinct
             -- from `coordinator_runs`, which is the per-run phase/result view):
             -- captures the OS pid, the generation (restart counter), the
             -- in-flight Anthropic batch job id (the resume handle), the coarse
             -- phase, and the owning interactive session. On REPL startup the
             -- registry is scanned: rows whose `pid` is no longer alive are
             -- marked `orphaned`, and any that carried a `batch_job_id` are
             -- logged as resurrectable (full resurrection is TASK-291/SPR-059).
             CREATE TABLE IF NOT EXISTS coordinator_registry (
                 coord_id      TEXT PRIMARY KEY,
                 generation    INTEGER NOT NULL DEFAULT 0,
                 pid           INTEGER NOT NULL,
                 batch_job_id  TEXT,
                 phase         TEXT NOT NULL,
                 started_at    TEXT NOT NULL DEFAULT current_timestamp,
                 owner_session TEXT
             );
             CREATE INDEX IF NOT EXISTS idx_coord_registry_session
                 ON coordinator_registry (owner_session);",
        )
        .context("coordinator_runs schema init failed")?;
        // Back-compat: add session_name to a table created before it existed.
        // (session_id predates this; ignore the error when the column is present.)
        let _ = conn.execute(
            "ALTER TABLE coordinator_runs ADD COLUMN session_name TEXT",
            [],
        );
        // Back-compat: add the stand-down control flag to a table created before
        // it existed. Additive `ADD COLUMN` with a constant default — ignored
        // (duplicate-column error swallowed) once present, so it's idempotent.
        let _ = conn.execute(
            "ALTER TABLE coordinator_runs ADD COLUMN stand_down INTEGER NOT NULL DEFAULT 0",
            [],
        );
        // S9.1: cross-reference the container backing a run (id + name + engine)
        // so `:workers` / S9.5 discovery can map a run to its container. Additive
        // `ADD COLUMN` — errors with "duplicate column name" once present, which
        // is ignored so the migration is idempotent.
        for col in ["container_id", "container_name", "runtime"] {
            let _ = conn.execute(
                &format!("ALTER TABLE coordinator_runs ADD COLUMN {col} TEXT"),
                [],
            );
        }
        // Worker-run cost/effort metrics captured from the coordinator session
        // totals at terminal exit: cumulative prompt/completion tokens, agentic
        // turns, and tool-call count. Additive `ADD COLUMN` with a constant
        // default — the duplicate-column error is swallowed once present, so the
        // migration is idempotent, and existing rows read back 0.
        for col in ["tokens_in", "tokens_out", "turns", "tool_calls"] {
            let _ = conn.execute(
                &format!("ALTER TABLE coordinator_runs ADD COLUMN {col} INTEGER NOT NULL DEFAULT 0"),
                [],
            );
        }
        // TASK-294: widen the phase CHECK constraint to admit the resumable
        // 'checkpoint' phase and add its `checkpoint` control-flag column. SQLite
        // can't ALTER a CHECK, so a table predating the checkpoint phase is
        // rebuilt: copy every row into a new table whose CHECK includes
        // 'checkpoint' and which carries the flag column (default 0). Detect the
        // old shape by the ABSENCE of the quoted 'checkpoint' literal in the
        // stored CREATE SQL. Idempotent — once migrated (or freshly created with
        // the widened schema) the literal is present and this is a no-op.
        let needs_checkpoint_migration = conn
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type='table' AND name='coordinator_runs'",
                [],
                |r| r.get::<_, String>(0),
            )
            .optional()?
            .map(|sql| !sql.contains("'checkpoint'"))
            .unwrap_or(false);
        if needs_checkpoint_migration {
            conn.execute_batch(
                "BEGIN;
                 CREATE TABLE coordinator_runs_new (
                     run_id       TEXT PRIMARY KEY,
                     task         TEXT NOT NULL,
                     phase        TEXT NOT NULL CHECK (phase IN
                                  ('coordinating', 'awaiting_batch', 'checkpoint', 'done', 'failed')),
                     result       TEXT,
                     error        TEXT,
                     session_id   TEXT,
                     session_name TEXT,
                     created_at   TEXT NOT NULL DEFAULT current_timestamp,
                     heartbeat_at TEXT NOT NULL DEFAULT current_timestamp,
                     stand_down   INTEGER NOT NULL DEFAULT 0,
                     checkpoint   INTEGER NOT NULL DEFAULT 0,
                     container_id   TEXT,
                     container_name TEXT,
                     runtime        TEXT,
                     tokens_in    INTEGER NOT NULL DEFAULT 0,
                     tokens_out   INTEGER NOT NULL DEFAULT 0,
                     turns        INTEGER NOT NULL DEFAULT 0,
                     tool_calls   INTEGER NOT NULL DEFAULT 0
                 );
                 INSERT INTO coordinator_runs_new
                     (run_id, task, phase, result, error, session_id, session_name,
                      created_at, heartbeat_at, stand_down,
                      container_id, container_name, runtime,
                      tokens_in, tokens_out, turns, tool_calls)
                 SELECT
                     run_id, task, phase, result, error, session_id, session_name,
                     created_at, heartbeat_at, stand_down,
                     container_id, container_name, runtime,
                     tokens_in, tokens_out, turns, tool_calls
                 FROM coordinator_runs;
                 DROP TABLE coordinator_runs;
                 ALTER TABLE coordinator_runs_new RENAME TO coordinator_runs;
                 COMMIT;",
            )
            .context("coordinator_runs checkpoint-phase migration failed")?;
        }
        // Parentage link (hierarchical `:workers`): the parent coordinator's
        // run id, stamped when a coordinator spawns a sub-coordinator. Additive
        // `ADD COLUMN` — swallowed once present, so idempotent; existing rows
        // read back NULL (a root). Placed AFTER the checkpoint rebuild so the
        // rebuild (whose fixed column list omits this) can't drop it.
        let _ = conn.execute(
            "ALTER TABLE coordinator_runs ADD COLUMN parent_run_id TEXT",
            [],
        );
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    /// Back-compat 4-arg insert for a run with NO parent (a root coordinator
    /// launched from an interactive REPL, and every test call site). Delegates
    /// to `insert_with_parent` with `parent_run_id = None`.
    pub fn insert(
        &self,
        run_id: &str,
        task: &str,
        session_id: &str,
        session_name: Option<&str>,
    ) -> Result<()> {
        self.insert_with_parent(run_id, task, session_id, session_name, None)
    }

    /// Register a freshly-started run, optionally linked to the parent
    /// coordinator that spawned it (`parent_run_id`, from `AISH_PARENT_RUN_ID`).
    /// Idempotent on `run_id`.
    pub fn insert_with_parent(
        &self,
        run_id: &str,
        task: &str,
        session_id: &str,
        session_name: Option<&str>,
        parent_run_id: Option<&str>,
    ) -> Result<()> {
        self.conn.lock().unwrap().execute(
            "INSERT INTO coordinator_runs (run_id, task, phase, session_id, session_name, parent_run_id)
             VALUES (?1, ?2, 'coordinating', ?3, ?4, ?5)
             ON CONFLICT(run_id) DO NOTHING",
            (run_id, task, session_id, session_name, parent_run_id),
        )?;
        Ok(())
    }

    /// Insert a terminal `failed` SALVAGE row for a run whose normal row was lost
    /// to an early termination, reconstructed from a surviving worktree. The
    /// worktree (with its un-pushed work) is the durable source of truth; this
    /// re-derives the missing store row so the otherwise-invisible failure shows
    /// up in `:workers` / `background_status` again. Idempotent (ON CONFLICT DO
    /// NOTHING) so a re-derive on the next startup can't duplicate it. `error`
    /// carries the recoverable branch/path. (coordinator-lifecycle bug.)
    pub fn insert_salvaged(&self, run_id: &str, task: &str, error: &str) -> Result<()> {
        self.conn.lock().unwrap().execute(
            "INSERT INTO coordinator_runs (run_id, task, phase, error) \
             VALUES (?1, ?2, 'failed', ?3) ON CONFLICT(run_id) DO NOTHING",
            (run_id, task, error),
        )?;
        Ok(())
    }

    /// Advance the run's phase marker (and bump the heartbeat, since a phase
    /// transition is itself proof of liveness).
    pub fn set_phase(&self, run_id: &str, phase: &str) -> Result<()> {
        self.conn.lock().unwrap().execute(
            "UPDATE coordinator_runs SET phase = ?2, heartbeat_at = current_timestamp \
             WHERE run_id = ?1",
            (run_id, phase),
        )?;
        Ok(())
    }

    /// Stamp a liveness beat — written periodically while awaiting a batch so a
    /// restart can tell a live run from an orphaned one.
    pub fn heartbeat(&self, run_id: &str) -> Result<()> {
        self.conn.lock().unwrap().execute(
            "UPDATE coordinator_runs SET heartbeat_at = current_timestamp WHERE run_id = ?1",
            [run_id],
        )?;
        Ok(())
    }

    pub fn set_done(&self, run_id: &str, result: &str) -> Result<()> {
        self.conn.lock().unwrap().execute(
            "UPDATE coordinator_runs \
             SET phase = 'done', result = ?2, heartbeat_at = current_timestamp WHERE run_id = ?1",
            (run_id, result),
        )?;
        Ok(())
    }

    /// Detect and mark stalled runs (non-terminal phase + heartbeat > 5 minutes old)
    /// as failed. Called automatically by `load_all()` to proactively clean up
    /// hung coordinators without requiring an aish restart.
    fn cleanup_stalled_runs(&self) -> Result<()> {
        const STALL_THRESHOLD_SECS: i64 = 5 * 60;
        
        let conn = self.conn.lock().unwrap();
        // Mark any active phase with stale heartbeat as failed.
        conn.execute(
            "UPDATE coordinator_runs 
             SET phase = 'failed', error = 'stalled: no heartbeat activity for 5+ minutes'
             WHERE phase IN ('coordinating', 'awaiting_batch')
             AND (heartbeat_at IS NULL OR (strftime('%s', 'now') - strftime('%s', heartbeat_at)) > ?)",
            [STALL_THRESHOLD_SECS],
        )?;
        Ok(())
    }

    pub fn set_failed(&self, run_id: &str, error: &str) -> Result<()> {
        self.conn.lock().unwrap().execute(
            "UPDATE coordinator_runs \
             SET phase = 'failed', error = ?2, heartbeat_at = current_timestamp WHERE run_id = ?1",
            (run_id, error),
        )?;
        Ok(())
    }

    /// Run `f` inside a single SQLite transaction against the store connection,
    /// committing on `Ok` and rolling back on `Err` (TASK-285). The rusqlite
    /// [`Transaction`] guard also rolls back when dropped WITHOUT a commit — so a
    /// panic inside `f`, or a hard crash/SIGKILL of the process before COMMIT,
    /// leaves nothing half-written: the WAL never gains a commit frame for the
    /// aborted transaction, and the next open recovers the prior state. This is
    /// the atomic seam behind the coordinator's turn-state writes: a
    /// multi-statement mutation (terminal phase + metrics) either lands whole or
    /// not at all, so a mid-write failure never yields a torn, un-resumable row.
    pub fn transact<T>(
        &self,
        f: impl FnOnce(&Transaction) -> Result<T>,
    ) -> Result<T> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        let out = f(&tx)?;
        tx.commit()?;
        Ok(out)
    }

    /// Atomically finalize a run to a terminal `phase` (`done` or `failed`)
    /// TOGETHER with its cumulative [`RunMetrics`], in ONE transaction (TASK-285).
    /// Replaces the former `set_done`/`set_failed` followed by a SEPARATE
    /// `record_metrics` write at the coordinator's terminal exits: a crash
    /// between those two statements used to leave the row half-updated (terminal
    /// phase with zero/stale metrics, or metrics under a non-terminal phase).
    /// Wrapped in [`Self::transact`], the phase, result, error, heartbeat, and
    /// metrics commit as a unit — a re-read after a rolled-back write sees the
    /// prior resumable `coordinating` row intact. `result` is set for a `done`
    /// exit (with `error = None`); `error` is set for a `failed` exit.
    pub fn finish_run(
        &self,
        run_id: &str,
        phase: &str,
        result: Option<&str>,
        error: Option<&str>,
        metrics: RunMetrics,
    ) -> Result<()> {
        self.transact(|tx| {
            tx.execute(
                "UPDATE coordinator_runs \
                 SET phase = ?2, result = ?3, error = ?4, \
                     tokens_in = ?5, tokens_out = ?6, turns = ?7, tool_calls = ?8, \
                     heartbeat_at = current_timestamp \
                 WHERE run_id = ?1",
                rusqlite::params![
                    run_id,
                    phase,
                    result,
                    error,
                    metrics.tokens_in,
                    metrics.tokens_out,
                    metrics.turns,
                    metrics.tool_calls,
                ],
            )?;
            Ok(())
        })
    }

    /// Every persisted run, oldest first — used at startup to surface completed
    /// runs and reap orphaned ones. Automatically marks any stalled runs as failed
    /// before returning (stall threshold: 5 minutes no heartbeat activity).
    pub fn load_all(&self) -> Result<Vec<CoordinatorRow>> {
        // First, detect and mark any stalled runs.
        self.cleanup_stalled_runs()?;
        
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT run_id, task, phase, result, error, session_id, session_name, created_at, heartbeat_at, \
                    tokens_in, tokens_out, turns, tool_calls, parent_run_id
             FROM coordinator_runs ORDER BY created_at",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(CoordinatorRow {
                run_id: r.get(0)?,
                task: r.get(1)?,
                phase: r.get(2)?,
                result: r.get(3)?,
                error: r.get(4)?,
                session_id: r.get(5)?,
                session_name: r.get(6)?,
                created_at: r.get(7)?,
                heartbeat_at: r.get(8)?,
                tokens_in: r.get(9)?,
                tokens_out: r.get(10)?,
                turns: r.get(11)?,
                tool_calls: r.get(12)?,
                parent_run_id: r.get(13)?,
            })
        })?;
        Ok(rows.filter_map(std::result::Result::ok).collect())
    }

    /// TASK-325 AC2/AC3 — sum token/turn/tool metrics across every persisted
    /// run. Backs the `:tokens` dashboard and the in:out ratio. `COALESCE(...,0)`
    /// keeps the aggregate well-defined on an empty / all-null table.
    pub fn cost_summary(&self) -> Result<CostSummary> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT COUNT(*), COALESCE(SUM(tokens_in),0), COALESCE(SUM(tokens_out),0), \
                    COALESCE(SUM(turns),0), COALESCE(SUM(tool_calls),0) \
             FROM coordinator_runs",
            [],
            |r| {
                Ok(CostSummary {
                    runs: r.get(0)?,
                    tokens_in: r.get(1)?,
                    tokens_out: r.get(2)?,
                    turns: r.get(3)?,
                    tool_calls: r.get(4)?,
                })
            },
        )
        .map_err(Into::into)
    }

    /// TASK-325 AC3 — the `limit` runs with the highest total (in+out) token
    /// spend, descending; ties broken by most-recent. Zero-metric runs sort last.
    pub fn top_expensive_runs(&self, limit: usize) -> Result<Vec<CoordinatorRow>> {
        let mut rows = self.load_all()?;
        rows.sort_by(|a, b| {
            (b.tokens_in + b.tokens_out)
                .cmp(&(a.tokens_in + a.tokens_out))
                .then(b.created_at.cmp(&a.created_at))
        });
        rows.truncate(limit);
        Ok(rows)
    }


    /// Count prior terminal-`failed` runs whose `task` text matches `task`
    /// exactly. This backs the coordinator's pre-dispatch circuit breaker
    /// (`coordinator::drive`): if the same task has already failed N times, a
    /// fresh dispatch is refused fast instead of looping a known-bad request.
    /// The match is exact on the stored task string — `drive` normalizes the
    /// task before keying, so callers compare like with like. Best-effort
    /// semantics live at the call site; this is just the count.
    ///
    /// NOTE (durability): `clear_finished` now purges only `done` rows; `failed`
    /// rows are RETAINED (bounded by `coordinator::reap_failed_runs`'s
    /// keep-recent + max-age window — #129 item 5). So this counter persists
    /// ACROSS restarts within that retention window, not just per-session: a task
    /// that keeps failing stays known-bad until its failed rows age/count out.
    /// Salvage rows carry a synthetic task string, so they never trip a real
    /// task's breaker.
    pub fn failed_attempts(&self, task: &str) -> Result<i64> {
        Ok(self.conn.lock().unwrap().query_row(
            "SELECT count(*) FROM coordinator_runs WHERE task = ?1 AND phase = 'failed'",
            [task],
            |r| r.get(0),
        )?)
    }

    /// Queue an operator message for an in-flight coordinator run — the write
    /// side of the `:tell` / SendMessage channel. The message is picked up at
    /// the start of the run's next round (see `coordinator::drive`), so delivery
    /// is round-boundary, not instantaneous. `from_session` records the sender
    /// for provenance. Returns the new message row id.
    pub fn enqueue_message(
        &self,
        run_id: &str,
        message: &str,
        from_session: Option<&str>,
    ) -> Result<i64> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO coordinator_messages (run_id, message, from_session) \
             VALUES (?1, ?2, ?3)",
            (run_id, message, from_session),
        )?;
        Ok(conn.last_insert_rowid())
    }

    /// Atomically take — and delete — every queued message for `run_id`, oldest
    /// first. Delete-on-read: a message is folded into the coordinator's
    /// transcript exactly once and must not repeat on the next round. The select
    /// and delete run in one transaction, so a message inserted concurrently
    /// (after the select, before the delete) is preserved for the next drain
    /// rather than dropped. Returns the message texts in send order.
    pub fn drain_messages(&self, run_id: &str) -> Result<Vec<String>> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        let taken: Vec<(i64, String)> = {
            let mut stmt = tx.prepare(
                "SELECT id, message FROM coordinator_messages WHERE run_id = ?1 ORDER BY id",
            )?;
            let rows = stmt.query_map([run_id], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
            })?;
            rows.filter_map(std::result::Result::ok).collect()
        };
        if let Some(max_id) = taken.last().map(|(id, _)| *id) {
            // Delete only the ids we actually read (id <= max_id), so a row
            // inserted after the select survives for the next round.
            tx.execute(
                "DELETE FROM coordinator_messages WHERE run_id = ?1 AND id <= ?2",
                (run_id, max_id),
            )?;
        }
        tx.commit()?;
        Ok(taken.into_iter().map(|(_, m)| m).collect())
    }

    /// How many messages are currently queued for a run (peek, no delete) — for
    /// status display and the `:tell` confirmation line.
    pub fn pending_message_count(&self, run_id: &str) -> Result<i64> {
        Ok(self.conn.lock().unwrap().query_row(
            "SELECT count(*) FROM coordinator_messages WHERE run_id = ?1",
            [run_id],
            |r| r.get(0),
        )?)
    }

    /// Raise the STAND-DOWN flag on a run — the write side of the `:stop` /
    /// `stop` channel, a harsher sibling of `:tell`. Where a tell queues a
    /// message the coordinator folds in and keeps working, a stand-down orders
    /// it to STOP: at its next round boundary the coordinator takes one final
    /// graceful wrap-up turn (preserve in-flight work, report a status) and then
    /// terminates (see `coordinator::drive`). Durable, so it survives a restart
    /// and applies cross-session. Idempotent — raising an already-raised flag,
    /// or one on a finished run, is a harmless no-op (a terminal run's loop has
    /// already exited and will never read it). Also bumps the heartbeat, since
    /// touching the row is itself proof the parent is alive.
    pub fn request_stand_down(&self, run_id: &str) -> Result<()> {
        self.conn.lock().unwrap().execute(
            "UPDATE coordinator_runs SET stand_down = 1, heartbeat_at = current_timestamp \
             WHERE run_id = ?1",
            [run_id],
        )?;
        Ok(())
    }

    /// Peek the stand-down flag for a run (no clear). The coordinator checks this
    /// at every round boundary; once it's set the run wraps up and exits, so
    /// there's nothing to clear. Returns `false` when the row is absent or the
    /// flag was never raised.
    pub fn stand_down_requested(&self, run_id: &str) -> Result<bool> {
        Ok(self
            .conn
            .lock()
            .unwrap()
            .query_row(
                "SELECT stand_down FROM coordinator_runs WHERE run_id = ?1",
                [run_id],
                |r| r.get::<_, i64>(0),
            )
            .optional()?
            .is_some_and(|v| v != 0))
    }

    /// Raise the CHECKPOINT flag on a run — the write side of the `:checkpoint`
    /// channel (TASK-294), a resumable-PAUSE sibling of `:stop`. Where a
    /// stand-down orders the coordinator to take a final wrap-up turn and
    /// TERMINATE, a checkpoint asks it to HALT at its next round boundary WITHOUT
    /// finishing: `coordinator::drive` persists phase='checkpoint' and returns,
    /// leaving the transcript/worktree intact so the run can be resumed manually
    /// later. Durable (survives a restart, applies cross-session) and idempotent
    /// — re-raising, or raising on a finished run, is a harmless no-op. Bumps the
    /// heartbeat since touching the row proves the parent is alive.
    ///
    /// `#[allow(dead_code)]`: this is the write side of the future `:checkpoint`
    /// REPL command; it's exercised today by the store round-trip test and read
    /// by `coordinator::drive` via [`Self::checkpoint_requested`].
    #[allow(dead_code)]
    pub fn request_checkpoint(&self, run_id: &str) -> Result<()> {
        self.conn.lock().unwrap().execute(
            "UPDATE coordinator_runs SET checkpoint = 1, heartbeat_at = current_timestamp \
             WHERE run_id = ?1",
            [run_id],
        )?;
        Ok(())
    }

    /// Peek the checkpoint flag for a run (no clear). The coordinator checks this
    /// at every round boundary; once set the run pauses at `checkpoint` and
    /// exits, so there's nothing to clear here (a resume clears it explicitly).
    /// Returns `false` when the row is absent or the flag was never raised.
    pub fn checkpoint_requested(&self, run_id: &str) -> Result<bool> {
        Ok(self
            .conn
            .lock()
            .unwrap()
            .query_row(
                "SELECT checkpoint FROM coordinator_runs WHERE run_id = ?1",
                [run_id],
                |r| r.get::<_, i64>(0),
            )
            .optional()?
            .is_some_and(|v| v != 0))
    }

    /// Bind `alias`->`run_id` ONCE at run creation (TASK-205 AC1). The write is
    /// immutable: a second bind for the same alias is a no-op
    /// (`ON CONFLICT(alias) DO NOTHING`), so the mapping captured at run start
    /// can never be mutated by a later — possibly racing — writer. `pr` records
    /// the opened pull request when known; it is informational and never affects
    /// resolution. Idempotent, so it is safe to call again on a resume.
    #[allow(dead_code)] // Alias API exercised by tests; production resolves via result_for_run today.
    pub fn bind_alias(&self, alias: &str, run_id: &str, pr: Option<&str>) -> Result<()> {
        self.conn.lock().unwrap().execute(
            "INSERT INTO run_aliases (alias, run_id, pr) VALUES (?1, ?2, ?3) \
             ON CONFLICT(alias) DO NOTHING",
            (alias, run_id, pr),
        )?;
        Ok(())
    }

    /// Resolve an alias to its immutably-bound `run_id` (TASK-205 AC2). `None`
    /// when the alias was never bound. A single keyed read — no shared state.
    #[allow(dead_code)] // Alias API exercised by tests; production resolves via result_for_run today.
    pub fn resolve_alias(&self, alias: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .lock()
            .unwrap()
            .query_row("SELECT run_id FROM run_aliases WHERE alias = ?1", [alias], |r| r.get(0))
            .optional()?)
    }

    /// Read ONE run's terminal payload strictly by `run_id` (TASK-205 AC2/AC3).
    /// A single keyed row lookup against `coordinator_runs` — there is no
    /// global/shared "last result" slot, so a concurrent completion of a
    /// different run can never corrupt this read. `None` when the run is unknown.
    pub fn result_for_run(&self, run_id: &str) -> Result<Option<RunResult>> {
        Ok(self
            .conn
            .lock()
            .unwrap()
            .query_row(
                "SELECT run_id, phase, result, error FROM coordinator_runs WHERE run_id = ?1",
                [run_id],
                |r| {
                    Ok(RunResult {
                        run_id: r.get(0)?,
                        phase: r.get(1)?,
                        result: r.get(2)?,
                        error: r.get(3)?,
                    })
                },
            )
            .optional()?)
    }

    /// Resolve `alias`->`run_id`->that run's own result (TASK-205 AC2). The alias
    /// binding and the result read are each a single exact-key lookup, so the
    /// whole path is free of any shared/global cache a racing completion could
    /// clobber. Tries `alias` as a bound alias first, then falls back to treating
    /// it as a literal `run_id` (the two coincide for an aish worker).
    #[allow(dead_code)] // Alias API exercised by tests; production resolves via result_for_run today.
    pub fn result_for_alias(&self, alias: &str) -> Result<Option<RunResult>> {
        if let Some(run_id) = self.resolve_alias(alias)? {
            return self.result_for_run(&run_id);
        }
        self.result_for_run(alias)
    }

    /// Drop terminal `done` runs (a delivered/surfaced result needs no further
    /// retention). `failed` runs are intentionally RETAINED so a reaped orphan
    /// or errored run stays inspectable in `background_status` / `:workers`
    /// instead of vanishing — their bounded retention (keep-recent + max-age) is
    /// handled separately by `delete_runs` via `coordinator::reap_failed_runs`,
    /// so the table still can't grow without bound. Also purges any orphaned
    /// mailbox messages — those whose target run no longer exists — so the
    /// mailbox can't grow without bound. Returns how many `done` runs were
    /// removed. (coordinator-lifecycle bug #129 item 5: stop destroying the
    /// forensic trail of failed runs.)
    pub fn clear_finished(&self) -> Result<usize> {
        let conn = self.conn.lock().unwrap();
        let n = conn.execute("DELETE FROM coordinator_runs WHERE phase = 'done'", [])?;
        let _ = conn.execute(
            "DELETE FROM coordinator_messages \
             WHERE run_id NOT IN (SELECT run_id FROM coordinator_runs)",
            [],
        );
        Ok(n)
    }

    /// Purge mailbox messages whose target run no longer exists. `clear_finished`
    /// does this as a side effect, but the startup rehydrate path skips
    /// `clear_finished` when the digest is suppressed (it retains `done` rows so
    /// their results stay retrievable), so it calls this directly to keep the
    /// mailbox from growing unbounded. Best-effort — a store error is ignored.
    pub fn purge_orphan_messages(&self) {
        let conn = self.conn.lock().unwrap();
        let _ = conn.execute(
            "DELETE FROM coordinator_messages \
             WHERE run_id NOT IN (SELECT run_id FROM coordinator_runs)",
            [],
        );
    }

    /// Delete the given runs by id (and purge any now-orphaned mailbox
    /// messages). Backs the bounded `failed`-row retention sweep
    /// (`coordinator::reap_failed_runs`): `clear_finished` keeps `failed` rows
    /// for forensics, so a separate age/count-bounded reaper trims them here.
    /// Returns how many run rows were deleted. No-op (returns 0) for an empty
    /// slice. (coordinator-lifecycle bug #129 item 5.)
    pub fn delete_runs(&self, run_ids: &[String]) -> Result<usize> {
        if run_ids.is_empty() {
            return Ok(0);
        }
        let conn = self.conn.lock().unwrap();
        let mut deleted = 0usize;
        for id in run_ids {
            deleted += conn.execute("DELETE FROM coordinator_runs WHERE run_id = ?1", [id])?;
        }
        let _ = conn.execute(
            "DELETE FROM coordinator_messages \
             WHERE run_id NOT IN (SELECT run_id FROM coordinator_runs)",
            [],
        );
        Ok(deleted)
    }

    /// TASK-289: register (or re-register) a live coordinator PROCESS in the
    /// `coordinator_registry`. Keyed by `coord_id`; a re-register from a
    /// resurrected process upserts the pid/batch/phase and bumps `generation`
    /// so a stale row can be told from a fresh generation.
    pub fn register_run(
        &self,
        coord_id: &str,
        generation: i64,
        pid: i64,
        batch_job_id: Option<&str>,
        phase: &str,
        owner_session: Option<&str>,
    ) -> Result<()> {
        self.conn.lock().unwrap().execute(
            "INSERT INTO coordinator_registry
                 (coord_id, generation, pid, batch_job_id, phase, owner_session)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(coord_id) DO UPDATE SET
                 generation    = excluded.generation,
                 pid           = excluded.pid,
                 batch_job_id  = excluded.batch_job_id,
                 phase         = excluded.phase,
                 owner_session = excluded.owner_session",
            (coord_id, generation, pid, batch_job_id, phase, owner_session),
        )?;
        Ok(())
    }

    /// TASK-289: all registry rows NOT yet marked `orphaned` — the candidate set
    /// the startup reaper scans for dead pids. Ordered by `started_at`.
    pub fn get_live_runs(&self) -> Result<Vec<CoordinatorRegistryRow>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT coord_id, generation, pid, batch_job_id, phase, started_at, owner_session
             FROM coordinator_registry
             WHERE phase != 'orphaned'
             ORDER BY started_at",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(CoordinatorRegistryRow {
                coord_id: r.get(0)?,
                generation: r.get(1)?,
                pid: r.get(2)?,
                batch_job_id: r.get(3)?,
                phase: r.get(4)?,
                started_at: r.get(5)?,
                owner_session: r.get(6)?,
            })
        })?;
        Ok(rows.filter_map(std::result::Result::ok).collect())
    }

    /// TASK-289: flip a registry row to the `orphaned` phase — called by the
    /// startup reaper for a `coord_id` whose `pid` is no longer alive. Rows that
    /// carried a `batch_job_id` stay resurrectable (TASK-291/SPR-059).
    pub fn mark_orphaned(&self, coord_id: &str) -> Result<()> {
        self.conn.lock().unwrap().execute(
            "UPDATE coordinator_registry SET phase = 'orphaned' WHERE coord_id = ?1",
            [coord_id],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coordinator_store_records_run_metrics() {
        let path =
            std::env::temp_dir().join(format!("aish_coord_metrics_{}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let store = CoordinatorStore::open(&path).unwrap();

        store
            .insert("run_m", "capture worker telemetry", "sess-m", None)
            .unwrap();
        // Fresh rows read back zeroed (migration default), so downstream renders
        // treat "no metrics captured" distinctly from a real zero-effort run.
        let before = store.load_all().unwrap();
        let r0 = before.iter().find(|r| r.run_id == "run_m").unwrap();
        assert_eq!(
            (r0.tokens_in, r0.tokens_out, r0.turns, r0.tool_calls),
            (0, 0, 0, 0)
        );

        // Stamp metrics + terminal phase atomically (TASK-285 finish_run).
        store
            .finish_run(
                "run_m",
                "done",
                Some("done"),
                None,
                RunMetrics { tokens_in: 12_345, tokens_out: 6_789, turns: 7, tool_calls: 42 },
            )
            .unwrap();

        // Survives a restart (persisted columns, not in-memory state).
        let reopened = CoordinatorStore::open(&path).unwrap();
        let rows = reopened.load_all().unwrap();
        let r = rows.iter().find(|r| r.run_id == "run_m").unwrap();
        assert_eq!(r.tokens_in, 12_345);
        assert_eq!(r.tokens_out, 6_789);
        assert_eq!(r.turns, 7);
        assert_eq!(r.tool_calls, 42);
        assert_eq!(r.phase, "done"); // finish_run set phase + metrics as one unit
        // TASK-325: the aggregate dashboard sees this run's stamped metrics.
        let cs = store.cost_summary().unwrap();
        assert_eq!(cs.runs, 1);
        assert_eq!(cs.tokens_in, 12_345);
        assert_eq!(cs.tokens_out, 6_789);
        assert_eq!(cs.total_tokens(), 19_134);
        assert_eq!(cs.turns, 7);
        assert_eq!(cs.tool_calls, 42);
        assert_eq!(r.result.as_deref(), Some("done"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn run_aliases_are_immutable_and_resolve_by_run_id() {
        let path = std::env::temp_dir().join(format!("aish_alias_{}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let store = CoordinatorStore::open(&path).unwrap();

        store.insert("run_a", "task a", "s", None).unwrap();
        store.bind_alias("w_a", "run_a", Some("#75")).unwrap();
        // Immutable: a second bind for the same alias is a no-op (AC1).
        store.bind_alias("w_a", "run_OTHER", Some("#999")).unwrap();
        assert_eq!(store.resolve_alias("w_a").unwrap().as_deref(), Some("run_a"));

        store.set_done("run_a", "PR #75 opened").unwrap();
        let r = store.result_for_alias("w_a").unwrap().unwrap();
        assert_eq!(r.run_id, "run_a");
        assert_eq!(r.phase, "done");
        assert_eq!(r.result.as_deref(), Some("PR #75 opened"));
        // An unbound alias falls back to a literal run_id lookup.
        assert_eq!(
            store.result_for_alias("run_a").unwrap().unwrap().result.as_deref(),
            Some("PR #75 opened")
        );
        // An unknown alias resolves to nothing (not someone else's result).
        assert!(store.result_for_alias("nope").unwrap().is_none());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn concurrent_worker_completions_do_not_corrupt_result_lookup() {
        // TASK-205 regression: complete N workers in parallel, then assert each
        // `:result <alias>` returns its OWN run's data — no shared slot a racing
        // completion can overwrite.
        use std::thread;
        let path =
            std::env::temp_dir().join(format!("aish_alias_conc_{}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let store = CoordinatorStore::open(&path).unwrap();

        const N: usize = 12;
        let mut handles = Vec::new();
        for i in 0..N {
            let store = store.clone();
            handles.push(thread::spawn(move || {
                let run_id = format!("run_{i}");
                let alias = format!("w_{i}");
                let result = format!("PR #{} done", 100 + i);
                store.insert(&run_id, "parallel work", "s", None).unwrap();
                store.bind_alias(&alias, &run_id, None).unwrap();
                store.set_done(&run_id, &result).unwrap();
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        // Every alias resolves to exactly its own run's result.
        for i in 0..N {
            let r = store.result_for_alias(&format!("w_{i}")).unwrap().unwrap();
            assert_eq!(r.run_id, format!("run_{i}"));
            assert_eq!(r.result.as_deref(), Some(format!("PR #{} done", 100 + i).as_str()));
        }
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn coordinator_store_roundtrip_and_resume() {
        let path = std::env::temp_dir().join(format!("aish_coord_{}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let store = CoordinatorStore::open(&path).unwrap();

        store
            .insert("run_1", "audit the repo", "sess-a", Some("alpha"))
            .unwrap();
        // Idempotent insert (resume path) must not clobber the existing row.
        store.set_phase("run_1", "awaiting_batch").unwrap();
        store
            .insert("run_1", "audit the repo", "sess-a", Some("alpha"))
            .unwrap();
        store.heartbeat("run_1").unwrap();

        store
            .insert("run_2", "draft release notes", "sess-b", None)
            .unwrap();
        store.set_done("run_2", "the notes").unwrap();

        // A fresh store over the same file sees both — the restart path.
        let reopened = CoordinatorStore::open(&path).unwrap();
        let rows = reopened.load_all().unwrap();
        assert_eq!(rows.len(), 2);
        let one = rows.iter().find(|r| r.run_id == "run_1").unwrap();
        assert_eq!(one.phase, "awaiting_batch"); // resumable, insert didn't reset it
        assert_eq!(one.session_id.as_deref(), Some("sess-a"));
        assert_eq!(one.session_name.as_deref(), Some("alpha"));
        assert!(one.heartbeat_at.is_some());
        let two = rows.iter().find(|r| r.run_id == "run_2").unwrap();
        assert_eq!(two.phase, "done");
        assert_eq!(two.result.as_deref(), Some("the notes"));
        assert_eq!(two.session_name, None);

        store.set_failed("run_1", "exceeded round cap").unwrap();
        let rows = reopened.load_all().unwrap();
        let one = rows.iter().find(|r| r.run_id == "run_1").unwrap();
        assert_eq!(one.phase, "failed");
        assert_eq!(one.error.as_deref(), Some("exceeded round cap"));

        // clear_finished now purges only `done` runs; `failed` rows are RETAINED
        // for forensics (#129 item 5) and trimmed separately by delete_runs /
        // coordinator::reap_failed_runs.
        assert_eq!(reopened.clear_finished().unwrap(), 1); // only run_2 (done)
        let after = reopened.load_all().unwrap();
        assert_eq!(after.len(), 1);
        assert_eq!(after[0].run_id, "run_1");
        assert_eq!(after[0].phase, "failed");
        // delete_runs trims a retained failed row explicitly (the reaper's primitive).
        assert_eq!(reopened.delete_runs(&["run_1".to_string()]).unwrap(), 1);
        assert!(reopened.load_all().unwrap().is_empty());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn checkpoint_flag_and_phase_persist_across_reopen() {
        // TASK-294: the checkpoint control flag round-trips, and the CHECK
        // constraint admits phase='checkpoint' which survives a store reopen as a
        // resumable (non-terminal) pause.
        let path = std::env::temp_dir().join(format!("aish_coordckpt_{}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let store = CoordinatorStore::open(&path).unwrap();

        store
            .insert("run_c", "long build", "sess-c", Some("gamma"))
            .unwrap();
        // Flag defaults to unset, is raised by request_checkpoint, and reads back.
        assert!(!store.checkpoint_requested("run_c").unwrap());
        store.request_checkpoint("run_c").unwrap();
        assert!(store.checkpoint_requested("run_c").unwrap());
        // An absent run reads back false (no row), never errors.
        assert!(!store.checkpoint_requested("missing").unwrap());

        // The coordinator halts by persisting phase='checkpoint' — the widened
        // CHECK must accept it.
        store.set_phase("run_c", "checkpoint").unwrap();

        // A fresh store over the same file sees the paused, resumable run.
        let reopened = CoordinatorStore::open(&path).unwrap();
        let rows = reopened.load_all().unwrap();
        let c = rows.iter().find(|r| r.run_id == "run_c").unwrap();
        assert_eq!(c.phase, "checkpoint");
        assert!(reopened.checkpoint_requested("run_c").unwrap());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn coordinator_messages_enqueue_drain_and_purge() {
        let path = std::env::temp_dir().join(format!("aish_coordmsg_{}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let store = CoordinatorStore::open(&path).unwrap();
        store
            .insert("run_1", "audit the repo", "sess-a", Some("alpha"))
            .unwrap();

        // No messages yet.
        assert_eq!(store.pending_message_count("run_1").unwrap(), 0);
        assert!(store.drain_messages("run_1").unwrap().is_empty());

        // Enqueue two messages for run_1 and one for an unrelated run.
        store
            .enqueue_message("run_1", "focus on the auth module first", Some("sess-b"))
            .unwrap();
        store
            .enqueue_message("run_1", "skip the e2e tests", None)
            .unwrap();
        store
            .enqueue_message("run_2", "different run", None)
            .unwrap();
        assert_eq!(store.pending_message_count("run_1").unwrap(), 2);

        // Drain run_1 — ordered oldest-first, scoped to run_1, delete-on-read.
        let drained = store.drain_messages("run_1").unwrap();
        assert_eq!(
            drained,
            vec![
                "focus on the auth module first".to_string(),
                "skip the e2e tests".to_string(),
            ]
        );
        // Second drain is empty (delete-on-read), and run_2's message is untouched.
        assert!(store.drain_messages("run_1").unwrap().is_empty());
        assert_eq!(store.pending_message_count("run_1").unwrap(), 0);
        assert_eq!(store.pending_message_count("run_2").unwrap(), 1);

        // A message survives across a process restart (fresh connection).
        store
            .enqueue_message("run_1", "one more note", None)
            .unwrap();
        let reopened = CoordinatorStore::open(&path).unwrap();
        assert_eq!(
            reopened.drain_messages("run_1").unwrap(),
            vec!["one more note".to_string()]
        );

        // clear_finished purges orphaned messages (run_2 was never inserted as a
        // run, so its queued message has no owning run row → purged).
        reopened.clear_finished().unwrap();
        assert_eq!(reopened.pending_message_count("run_2").unwrap(), 0);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn stand_down_flag_roundtrips_and_persists() {
        let path = std::env::temp_dir().join(format!("aish_standdown_{}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let store = CoordinatorStore::open(&path).unwrap();
        store.insert("run_sd", "long task", "sess-a", None).unwrap();

        // Freshly-inserted run defaults to NOT standing down.
        assert!(!store.stand_down_requested("run_sd").unwrap());
        // An unknown run reads false (not an error), so a stale id is harmless.
        assert!(!store.stand_down_requested("nope").unwrap());

        // Raise it; the peek now reports true, and it's idempotent.
        store.request_stand_down("run_sd").unwrap();
        store.request_stand_down("run_sd").unwrap();
        assert!(store.stand_down_requested("run_sd").unwrap());

        // Durable across a restart (fresh connection to the same file).
        let reopened = CoordinatorStore::open(&path).unwrap();
        assert!(reopened.stand_down_requested("run_sd").unwrap());

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn failed_attempts_counts_only_matching_failed_runs() {
        let p = std::env::temp_dir().join(format!("aish_failattempts_{}.db", std::process::id()));
        let _ = std::fs::remove_file(&p);
        let store = CoordinatorStore::open(&p).unwrap();

        // No history → zero.
        assert_eq!(store.failed_attempts("fix the build").unwrap(), 0);

        // Two failed runs for the same task, one still coordinating, one done,
        // and one failed run for a DIFFERENT task.
        store.insert("r1", "fix the build", "s", None).unwrap();
        store.set_failed("r1", "boom").unwrap();
        store.insert("r2", "fix the build", "s", None).unwrap();
        store.set_failed("r2", "boom again").unwrap();
        store.insert("r3", "fix the build", "s", None).unwrap(); // coordinating
        store.insert("r4", "fix the build", "s", None).unwrap();
        store.set_done("r4", "ok").unwrap(); // done, not failed
        store.insert("r5", "ship the docs", "s", None).unwrap();
        store.set_failed("r5", "nope").unwrap(); // different task

        // Only the two failed rows for the exact task are counted.
        assert_eq!(store.failed_attempts("fix the build").unwrap(), 2);
        assert_eq!(store.failed_attempts("ship the docs").unwrap(), 1);
        assert_eq!(store.failed_attempts("unrelated").unwrap(), 0);

        let _ = std::fs::remove_file(&p);
    }

    // TASK-289: coordinator_registry round-trip + register/get/mark ops.
    #[test]
    fn registry_round_trip_and_orphan_flip() {
        let path = std::env::temp_dir().join(format!("aish_coordreg_{}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let store = CoordinatorStore::open(&path).unwrap();

        // Empty registry → no live runs.
        assert!(store.get_live_runs().unwrap().is_empty());

        // Register two runs; one carries an in-flight batch job (resurrectable).
        store
            .register_run("c1", 0, 4242, None, "coordinating", Some("sess-a"))
            .unwrap();
        store
            .register_run("c2", 0, 4243, Some("batch_zzz"), "awaiting_batch", Some("sess-a"))
            .unwrap();

        let live = store.get_live_runs().unwrap();
        assert_eq!(live.len(), 2);
        let c2 = live.iter().find(|r| r.coord_id == "c2").unwrap();
        assert_eq!(c2.generation, 0);
        assert_eq!(c2.pid, 4243);
        assert_eq!(c2.batch_job_id.as_deref(), Some("batch_zzz"));
        assert_eq!(c2.phase, "awaiting_batch");
        assert_eq!(c2.owner_session.as_deref(), Some("sess-a"));
        assert!(c2.started_at.is_some());

        // Re-register c1 (resurrected process): upsert bumps generation + pid,
        // does NOT create a duplicate row.
        store
            .register_run("c1", 1, 5555, Some("batch_new"), "awaiting_batch", Some("sess-b"))
            .unwrap();
        let live = store.get_live_runs().unwrap();
        assert_eq!(live.len(), 2, "re-register must upsert, not duplicate");
        let c1 = live.iter().find(|r| r.coord_id == "c1").unwrap();
        assert_eq!(c1.generation, 1);
        assert_eq!(c1.pid, 5555);
        assert_eq!(c1.batch_job_id.as_deref(), Some("batch_new"));
        assert_eq!(c1.owner_session.as_deref(), Some("sess-b"));

        // Mark c2 orphaned → dropped from the live set, c1 remains.
        store.mark_orphaned("c2").unwrap();
        let live = store.get_live_runs().unwrap();
        assert_eq!(live.len(), 1);
        assert_eq!(live[0].coord_id, "c1");

        // Survives a restart (persisted columns, not in-memory state).
        let reopened = CoordinatorStore::open(&path).unwrap();
        let live = reopened.get_live_runs().unwrap();
        assert_eq!(live.len(), 1);
        assert_eq!(live[0].coord_id, "c1");
        assert_eq!(live[0].generation, 1);

        let _ = std::fs::remove_file(&path);
    }

    // ── TASK-285: transactional turn-state writes ───────────────────────────

    /// `finish_run` writes the terminal phase, result/error, and metrics as one
    /// atomic unit and survives a restart — the `done` and `failed` shapes.
    #[test]
    fn finish_run_persists_phase_and_metrics_atomically() {
        let path =
            std::env::temp_dir().join(format!("aish_t285_finish_{}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let store = CoordinatorStore::open(&path).unwrap();

        store.insert("run_done", "ship it", "s", None).unwrap();
        store.insert("run_fail", "ship it", "s", None).unwrap();

        let m = RunMetrics { tokens_in: 100, tokens_out: 55, turns: 4, tool_calls: 9 };
        store
            .finish_run("run_done", "done", Some("delivered"), None, m)
            .unwrap();
        store
            .finish_run("run_fail", "failed", None, Some("kaboom"), m)
            .unwrap();

        // Survives a reopen (persisted columns, not in-memory state).
        let reopened = CoordinatorStore::open(&path).unwrap();
        let rows = reopened.load_all().unwrap();

        let d = rows.iter().find(|r| r.run_id == "run_done").unwrap();
        assert_eq!(d.phase, "done");
        assert_eq!(d.result.as_deref(), Some("delivered"));
        assert_eq!(d.error, None);
        assert_eq!((d.tokens_in, d.tokens_out, d.turns, d.tool_calls), (100, 55, 4, 9));

        let f = rows.iter().find(|r| r.run_id == "run_fail").unwrap();
        assert_eq!(f.phase, "failed");
        assert_eq!(f.error.as_deref(), Some("kaboom"));
        assert_eq!(f.result, None);
        assert_eq!((f.tokens_in, f.tokens_out, f.turns, f.tool_calls), (100, 55, 4, 9));

        let _ = std::fs::remove_file(&path);
    }

    /// A transaction that ERRORS mid-write (stand-in for a panic/crash before
    /// COMMIT) must roll back cleanly, leaving the row in its prior resumable
    /// `coordinating` state rather than a torn terminal half-write. This is the
    /// core panic-safety guarantee behind TASK-285.
    #[test]
    fn transact_rolls_back_on_mid_write_error() {
        let path =
            std::env::temp_dir().join(format!("aish_t285_rollback_{}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let store = CoordinatorStore::open(&path).unwrap();

        store.insert("run_tx", "long task", "s", None).unwrap();
        store.set_phase("run_tx", "coordinating").unwrap();

        // Perform a terminal write, then bail BEFORE the implicit commit — the
        // rusqlite Transaction guard rolls the whole thing back on drop.
        let res: Result<()> = store.transact(|tx| {
            tx.execute(
                "UPDATE coordinator_runs \
                 SET phase = 'done', result = 'SHOULD_NOT_PERSIST', tokens_in = 999 \
                 WHERE run_id = 'run_tx'",
                [],
            )?;
            anyhow::bail!("simulated crash mid-write");
        });
        assert!(res.is_err(), "the closure error must propagate");

        // Re-read (including across a reopen): the aborted write left nothing.
        for s in [&store, &CoordinatorStore::open(&path).unwrap()] {
            let rows = s.load_all().unwrap();
            let r = rows.iter().find(|r| r.run_id == "run_tx").unwrap();
            assert_eq!(r.phase, "coordinating", "phase must roll back — row stays resumable");
            assert_eq!(r.result, None, "result must roll back");
            assert_eq!(r.tokens_in, 0, "metrics must roll back");
        }

        let _ = std::fs::remove_file(&path);
    }

    /// AC #3: SIGKILL a child process mid-write and verify the row is still
    /// resumable. A re-exec of this very test binary opens the SAME db, begins a
    /// terminal `finish_run`-shaped transaction, and hangs BEFORE commit; the
    /// parent SIGKILLs it (`Child::kill()` sends SIGKILL on Unix). SQLite's WAL
    /// never gained a commit frame for the aborted transaction, so on reopen the
    /// run is still `coordinating` — never the torn `done` the child was writing.
    #[test]
    fn sigkill_mid_transaction_leaves_row_resumable() {
        // Child leg: hold an uncommitted terminal write, then block so the parent
        // can SIGKILL us mid-transaction. Guarded by an env flag so it only runs
        // in the re-exec'd child, never during the normal suite pass.
        if let Ok(db) = std::env::var("AISH_T285_KILL_DB") {
            let store = CoordinatorStore::open(std::path::Path::new(&db)).unwrap();
            let _ = store.transact::<()>(|tx| {
                tx.execute(
                    "UPDATE coordinator_runs \
                     SET phase = 'done', result = 'SHOULD_NOT_PERSIST' WHERE run_id = 'run_kill'",
                    [],
                )?;
                // Never reaches commit: block until the parent kills us.
                std::thread::sleep(std::time::Duration::from_secs(30));
                Ok(())
            });
            std::process::exit(0);
        }

        let path =
            std::env::temp_dir().join(format!("aish_t285_kill_{}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let store = CoordinatorStore::open(&path).unwrap();
        store.insert("run_kill", "long task", "s", None).unwrap();
        store.set_phase("run_kill", "coordinating").unwrap();
        // Drop our handle so the child is the only writer (avoids our own WAL
        // lock interfering with the kill/reopen recovery).
        drop(store);

        let exe = std::env::current_exe().unwrap();
        let mut child = std::process::Command::new(exe)
            // Unique substring filter — matches ONLY this test regardless of the
            // crate's module-path prefix (no `--exact`, which would need the full
            // `…::tests::…` path). The env guard above makes the child leg run.
            .args(["sigkill_mid_transaction_leaves_row_resumable", "--test-threads=1"])
            .env("AISH_T285_KILL_DB", &path)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();

        // Give the child time to open the db and enter the write transaction.
        std::thread::sleep(std::time::Duration::from_millis(2000));
        // SIGKILL mid-write (std Child::kill == SIGKILL on Unix).
        child.kill().unwrap();
        let _ = child.wait();

        // Reopen: the uncommitted terminal write must have rolled back.
        let reopened = CoordinatorStore::open(&path).unwrap();
        let rows = reopened.load_all().unwrap();
        let r = rows.iter().find(|r| r.run_id == "run_kill").unwrap();
        assert_eq!(
            r.phase, "coordinating",
            "a SIGKILL'd mid-write must roll back — the run stays resumable"
        );
        assert_eq!(r.result, None, "the child's uncommitted result must not persist");

        let _ = std::fs::remove_file(&path);
    }
}
