//! Persistent store — ~/.aish/aish.db (SQLite, with sqlite-vec loaded for
//! vector search).
//!
//! Two tables:
//! - `history`: every REPL input and every model reply, timestamped, with cwd.
//! - `memories`: durable agent memory, written/read by the model through the
//!   remember/recall tools. `embedding` columns exist (and a vec0 index table)
//!   so semantic search can light up once an embedder is wired in; recall is
//!   keyword-based until then.

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension};
use std::path::Path;
use std::sync::{Arc, Mutex};

pub struct Db {
    conn: Connection,
}

impl Db {
    pub fn open(path: &Path) -> Result<Self> {
        // Register sqlite-vec into every connection opened from here on.
        // SAFETY: sqlite3_vec_init is the extension's documented entry point
        // (declared opaque by the crate, hence the transmute to the real
        // extension-init signature); auto_extension is the documented way to
        // register it process-wide.
        type InitFn = unsafe extern "C" fn(
            *mut rusqlite::ffi::sqlite3,
            *mut *mut std::os::raw::c_char,
            *const rusqlite::ffi::sqlite3_api_routines,
        ) -> std::os::raw::c_int;
        unsafe {
            let init: InitFn = std::mem::transmute(sqlite_vec::sqlite3_vec_init as *const ());
            rusqlite::ffi::sqlite3_auto_extension(Some(init));
        }
        let conn = Connection::open(path)
            .with_context(|| format!("can't open {}", path.display()))?;
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             CREATE TABLE IF NOT EXISTS history (
                 id      INTEGER PRIMARY KEY,
                 ts      TEXT NOT NULL DEFAULT current_timestamp,
                 cwd     TEXT,
                 kind    TEXT NOT NULL CHECK (kind IN ('input', 'output')),
                 content TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS memories (
                 id        INTEGER PRIMARY KEY,
                 ts        TEXT NOT NULL DEFAULT current_timestamp,
                 content   TEXT NOT NULL,
                 tags      TEXT,
                 embedding BLOB
             );
             CREATE VIRTUAL TABLE IF NOT EXISTS vec_memories USING vec0(
                 memory_id INTEGER PRIMARY KEY,
                 embedding float[384]
             );
             CREATE TABLE IF NOT EXISTS allowed_tools (
                 tool TEXT PRIMARY KEY,
                 ts   TEXT NOT NULL DEFAULT current_timestamp
             );
             CREATE TABLE IF NOT EXISTS settings (
                 key   TEXT PRIMARY KEY,
                 value TEXT NOT NULL
             );",
        )
        .context("schema init failed")?;
        Ok(Self { conn })
    }

    /// The sqlite-vec version string — proves vector support is actually loaded.
    #[cfg(test)]
    pub fn vec_version(&self) -> Result<String> {
        Ok(self.conn.query_row("SELECT vec_version()", [], |r| r.get(0))?)
    }

    pub fn record(&self, kind: &str, cwd: &str, content: &str) {
        // History is best-effort: a full disk must not break the shell.
        let _ = self.conn.execute(
            "INSERT INTO history (kind, cwd, content) VALUES (?1, ?2, ?3)",
            (kind, cwd, content),
        );
    }

    /// The content of the most recent `output` history row (a model/agent
    /// reply). Backs TASK-13 last-output addressing — the `$LAST`/`$_` dispatch
    /// binding and the automatic model-prompt context. `None` when no output has
    /// been recorded yet.
    pub fn last_output(&self) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row(
                "SELECT content FROM history WHERE kind = 'output' ORDER BY id DESC LIMIT 1",
                [],
                |r| r.get(0),
            )
            .optional()?)
    }

    pub fn remember(&self, content: &str, tags: Option<&str>) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO memories (content, tags) VALUES (?1, ?2)",
            (content, tags),
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Keyword recall (newest first). `query` empty → most recent memories.
    pub fn recall(&self, query: &str, limit: usize) -> Result<Vec<String>> {
        let pattern = format!("%{query}%");
        let mut stmt = self.conn.prepare(
            "SELECT ts, content, coalesce(tags, '') FROM memories
             WHERE content LIKE ?1 OR tags LIKE ?1
             ORDER BY id DESC LIMIT ?2",
        )?;
        let rows = stmt.query_map((pattern, limit), |r| {
            let (ts, content, tags): (String, String, String) = (r.get(0)?, r.get(1)?, r.get(2)?);
            Ok(if tags.is_empty() {
                format!("[{ts}] {content}")
            } else {
                format!("[{ts}] ({tags}) {content}")
            })
        })?;
        Ok(rows.filter_map(std::result::Result::ok).collect())
    }

    /// Add a tool/command to the persistent always-allow list (idempotent).
    pub fn allow(&self, tool: &str) -> Result<()> {
        self.conn
            .execute("INSERT OR IGNORE INTO allowed_tools (tool) VALUES (?1)", [tool])?;
        Ok(())
    }

    /// Is this tool/command on the always-allow list?
    pub fn is_allowed(&self, tool: &str) -> Result<bool> {
        Ok(self
            .conn
            .query_row("SELECT 1 FROM allowed_tools WHERE tool = ?1", [tool], |_| Ok(()))
            .optional()?
            .is_some())
    }

    /// Every always-allowed tool with its created-at timestamp, alphabetical.
    pub fn allowed_tools(&self) -> Result<Vec<(String, String)>> {
        let mut stmt = self
            .conn
            .prepare("SELECT tool, ts FROM allowed_tools ORDER BY tool")?;
        let rows = stmt.query_map([], |r| {
            let row: (String, String) = (r.get(0)?, r.get(1)?);
            Ok(row)
        })?;
        Ok(rows.filter_map(std::result::Result::ok).collect())
    }

    /// Drop a tool from the always-allow list. Returns true if a row was removed.
    pub fn revoke(&self, tool: &str) -> Result<bool> {
        let n = self
            .conn
            .execute("DELETE FROM allowed_tools WHERE tool = ?1", [tool])?;
        Ok(n > 0)
    }

    /// Persist a key/value setting (e.g. the batch-mode flag). Upserts.
    pub fn set_setting(&self, key: &str, value: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO settings (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            (key, value),
        )?;
        Ok(())
    }

    /// Read a persisted setting. `None` when unset.
    pub fn get_setting(&self, key: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row("SELECT value FROM settings WHERE key = ?1", [key], |r| r.get(0))
            .optional()?)
    }
}

/// One persisted background batch job (the `model` column is recorded but not
/// needed for reattach — the model is already stamped on the submitted batch).
pub struct BatchRow {
    pub local_id: String,
    pub anthropic_id: Option<String>,
    pub task: String,
    pub status: String,
    pub result: Option<String>,
    pub error: Option<String>,
    /// Owning session (uuid) + its friendly name. Null for rows written before
    /// ownership tracking existed.
    pub session_id: Option<String>,
    pub session_name: Option<String>,
    pub created_at: Option<String>,
}

/// Durable store for background batch jobs, kept in its OWN connection so the
/// async poll tasks (which run off the main thread) can persist status updates
/// without sharing the main `Db` connection. Points at the same `aish.db` file;
/// WAL mode (set by `Db::open`) makes the concurrent connections safe. Cloneable
/// — every spawned batch task holds a handle.
#[derive(Clone)]
pub struct BatchStore {
    conn: Arc<Mutex<Connection>>,
}

impl BatchStore {
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path)
            .with_context(|| format!("can't open batch store at {}", path.display()))?;
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             CREATE TABLE IF NOT EXISTS batch_jobs (
                 local_id     TEXT PRIMARY KEY,
                 anthropic_id TEXT,
                 task         TEXT NOT NULL,
                 model        TEXT NOT NULL,
                 status       TEXT NOT NULL,
                 result       TEXT,
                 error        TEXT,
                 created_at   TEXT NOT NULL DEFAULT current_timestamp,
                 session_id   TEXT,
                 session_name TEXT
             );",
        )
        .context("batch_jobs schema init failed")?;
        // Back-compat: add the ownership columns to a table created before they
        // existed. ALTER errors with "duplicate column name" once present —
        // ignore that so it's idempotent.
        for col in ["session_id", "session_name"] {
            let _ = conn.execute(&format!("ALTER TABLE batch_jobs ADD COLUMN {col} TEXT"), []);
        }
        Ok(Self { conn: Arc::new(Mutex::new(conn)) })
    }

    /// Register a freshly-queued job (no Anthropic id yet, status "running"),
    /// tagged with the spawning session so results auto-deliver only there.
    pub fn insert(
        &self,
        local_id: &str,
        task: &str,
        model: &str,
        session_id: &str,
        session_name: Option<&str>,
    ) -> Result<()> {
        self.conn.lock().unwrap().execute(
            "INSERT INTO batch_jobs (local_id, task, model, status, session_id, session_name) \
             VALUES (?1, ?2, ?3, 'running', ?4, ?5)",
            (local_id, task, model, session_id, session_name),
        )?;
        Ok(())
    }

    /// Record the Anthropic batch id once `create` returns — this is what lets a
    /// later run reattach to a batch in flight.
    pub fn set_anthropic_id(&self, local_id: &str, anthropic_id: &str) -> Result<()> {
        self.conn.lock().unwrap().execute(
            "UPDATE batch_jobs SET anthropic_id = ?2 WHERE local_id = ?1",
            (local_id, anthropic_id),
        )?;
        Ok(())
    }

    pub fn set_status(&self, local_id: &str, status: &str) -> Result<()> {
        self.conn
            .lock()
            .unwrap()
            .execute("UPDATE batch_jobs SET status = ?2 WHERE local_id = ?1", (local_id, status))?;
        Ok(())
    }

    pub fn set_done(&self, local_id: &str, result: &str) -> Result<()> {
        self.conn.lock().unwrap().execute(
            "UPDATE batch_jobs SET status = 'done', result = ?2 WHERE local_id = ?1",
            (local_id, result),
        )?;
        Ok(())
    }

    pub fn set_failed(&self, local_id: &str, error: &str) -> Result<()> {
        self.conn.lock().unwrap().execute(
            "UPDATE batch_jobs SET status = 'failed', error = ?2 WHERE local_id = ?1",
            (local_id, error),
        )?;
        Ok(())
    }

    /// Every persisted job, oldest first — used at startup to rehydrate handles
    /// and reattach poll loops to still-running batches.
    pub fn load_all(&self) -> Result<Vec<BatchRow>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT local_id, anthropic_id, task, status, result, error, session_id, session_name, created_at
             FROM batch_jobs ORDER BY created_at",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(BatchRow {
                local_id: r.get(0)?,
                anthropic_id: r.get(1)?,
                task: r.get(2)?,
                status: r.get(3)?,
                result: r.get(4)?,
                error: r.get(5)?,
                session_id: r.get(6)?,
                session_name: r.get(7)?,
                created_at: r.get(8)?,
            })
        })?;
        Ok(rows.filter_map(std::result::Result::ok).collect())
    }

    /// Drop terminal (done/failed) jobs. Returns how many rows were removed.
    pub fn clear_finished(&self) -> Result<usize> {
        Ok(self
            .conn
            .lock()
            .unwrap()
            .execute("DELETE FROM batch_jobs WHERE status IN ('done', 'failed')", [])?)
    }
}

/// One persisted coordinator run. Mirrors `BatchRow`, but for a durable,
/// resumable multi-round background coordinator (the default background path)
/// rather than a single Anthropic batch.
pub struct CoordinatorRow {
    pub run_id: String,
    pub task: String,
    /// 'coordinating' | 'awaiting_batch' | 'done' | 'failed'.
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
    /// The launching session's friendly name (`:name`), for display.
    pub session_name: Option<String>,
    pub created_at: Option<String>,
    /// Last liveness beat (SQLite `current_timestamp` string). A run whose owner
    /// is gone and whose heartbeat is stale is treated as orphaned on reattach.
    pub heartbeat_at: Option<String>,
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
             CREATE TABLE IF NOT EXISTS coordinator_runs (
                 run_id       TEXT PRIMARY KEY,
                 task         TEXT NOT NULL,
                 phase        TEXT NOT NULL CHECK (phase IN
                              ('coordinating', 'awaiting_batch', 'done', 'failed')),
                 result       TEXT,
                 error        TEXT,
                 session_id   TEXT,
                 session_name TEXT,
                 created_at   TEXT NOT NULL DEFAULT current_timestamp,
                 heartbeat_at TEXT NOT NULL DEFAULT current_timestamp
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
                 ON coordinator_messages (run_id);",
        )
        .context("coordinator_runs schema init failed")?;
        // Back-compat: add session_name to a table created before it existed.
        // (session_id predates this; ignore the error when the column is present.)
        let _ = conn.execute("ALTER TABLE coordinator_runs ADD COLUMN session_name TEXT", []);
        Ok(Self { conn: Arc::new(Mutex::new(conn)) })
    }

    /// Register a freshly-started run in the `coordinating` phase. Idempotent —
    /// re-inserting the same `run_id` (e.g. a resumed run) leaves the existing
    /// row untouched so its persisted phase/result survives.
    pub fn insert(
        &self,
        run_id: &str,
        task: &str,
        session_id: &str,
        session_name: Option<&str>,
    ) -> Result<()> {
        self.conn.lock().unwrap().execute(
            "INSERT INTO coordinator_runs (run_id, task, phase, session_id, session_name)
             VALUES (?1, ?2, 'coordinating', ?3, ?4)
             ON CONFLICT(run_id) DO NOTHING",
            (run_id, task, session_id, session_name),
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

    pub fn set_failed(&self, run_id: &str, error: &str) -> Result<()> {
        self.conn.lock().unwrap().execute(
            "UPDATE coordinator_runs \
             SET phase = 'failed', error = ?2, heartbeat_at = current_timestamp WHERE run_id = ?1",
            (run_id, error),
        )?;
        Ok(())
    }

    /// Every persisted run, oldest first — used at startup to surface completed
    /// runs and reap orphaned ones.
    pub fn load_all(&self) -> Result<Vec<CoordinatorRow>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT run_id, task, phase, result, error, session_id, session_name, created_at, heartbeat_at
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
            })
        })?;
        Ok(rows.filter_map(std::result::Result::ok).collect())
    }

    /// Count prior terminal-`failed` runs whose `task` text matches `task`
    /// exactly. This backs the coordinator's pre-dispatch circuit breaker
    /// (`coordinator::drive`): if the same task has already failed N times, a
    /// fresh dispatch is refused fast instead of looping a known-bad request.
    /// The match is exact on the stored task string — `drive` normalizes the
    /// task before keying, so callers compare like with like. Best-effort
    /// semantics live at the call site; this is just the count.
    ///
    /// NOTE (durability): `clear_finished` purges terminal rows on a clean
    /// restart, so this counter is effectively per-session-lifetime — it stops
    /// in-session re-dispatch storms (a goal loop re-launching the same task),
    /// not a cross-restart history. That's the same boundary the rest of the
    /// store's terminal bookkeeping lives within.
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
            let rows = stmt
                .query_map([run_id], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?;
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

    /// Drop terminal (done/failed) runs. Returns how many runs were removed.
    /// Also purges any orphaned mailbox messages — those whose target run no
    /// longer exists (delivered runs, reaped orphans) — so the mailbox can't
    /// grow without bound.
    pub fn clear_finished(&self) -> Result<usize> {
        let conn = self.conn.lock().unwrap();
        let n = conn.execute("DELETE FROM coordinator_runs WHERE phase IN ('done', 'failed')", [])?;
        let _ = conn.execute(
            "DELETE FROM coordinator_messages \
             WHERE run_id NOT IN (SELECT run_id FROM coordinator_runs)",
            [],
        );
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_db(name: &str) -> Db {
        let path = std::env::temp_dir().join(format!("aish_test_{name}_{}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        Db::open(&path).unwrap()
    }

    #[test]
    fn vector_extension_is_loaded() {
        let db = temp_db("vec");
        let v = db.vec_version().unwrap();
        assert!(v.starts_with('v'), "unexpected vec_version: {v}");
    }

    #[test]
    fn history_and_memories_roundtrip() {
        let db = temp_db("roundtrip");
        db.record("input", "/tmp", "ls -la");
        db.record("output", "/tmp", "total 0");
        let n: i64 = db.conn.query_row("SELECT count(*) FROM history", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 2);

        db.remember("user prefers terse replies", Some("preference")).unwrap();
        db.remember("project aios is a rust AI shell", Some("project")).unwrap();
        let hits = db.recall("terse", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert!(hits[0].contains("terse replies"));
        let all = db.recall("", 10).unwrap();
        assert_eq!(all.len(), 2);
    }

    #[test]
    fn last_output_returns_most_recent_output_row() {
        let db = temp_db("last_output");
        // No output yet.
        assert_eq!(db.last_output().unwrap(), None);
        // Inputs don't count — only `output` rows.
        db.record("input", "/tmp", "ls -la");
        assert_eq!(db.last_output().unwrap(), None);
        db.record("output", "/tmp", "first reply");
        db.record("input", "/tmp", "next question");
        db.record("output", "/tmp", "second reply");
        assert_eq!(db.last_output().unwrap().as_deref(), Some("second reply"));
    }

    #[test]
    fn settings_roundtrip() {
        let db = temp_db("settings");
        assert_eq!(db.get_setting("batch_mode").unwrap(), None);
        db.set_setting("batch_mode", "true").unwrap();
        assert_eq!(db.get_setting("batch_mode").unwrap().as_deref(), Some("true"));
        db.set_setting("batch_mode", "false").unwrap(); // upsert
        assert_eq!(db.get_setting("batch_mode").unwrap().as_deref(), Some("false"));
    }

    #[test]
    fn batch_store_roundtrip_and_reattach() {
        let path = std::env::temp_dir().join(format!("aish_batch_{}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let store = BatchStore::open(&path).unwrap();

        store.insert("batch_1", "summarize logs", "claude-opus-4-8", "sess-a", Some("alpha")).unwrap();
        store.set_anthropic_id("batch_1", "msgbatch_abc").unwrap();
        store.set_status("batch_1", "in_progress").unwrap();
        store.insert("batch_2", "translate", "claude-opus-4-8", "sess-b", None).unwrap();
        store.set_done("batch_2", "the result").unwrap();

        // A fresh store over the same file sees both — this is the restart path.
        let reopened = BatchStore::open(&path).unwrap();
        let rows = reopened.load_all().unwrap();
        assert_eq!(rows.len(), 2);
        let one = rows.iter().find(|r| r.local_id == "batch_1").unwrap();
        assert_eq!(one.anthropic_id.as_deref(), Some("msgbatch_abc"));
        assert_eq!(one.status, "in_progress"); // resumable
        assert_eq!(one.session_id.as_deref(), Some("sess-a"));
        assert_eq!(one.session_name.as_deref(), Some("alpha"));
        let two = rows.iter().find(|r| r.local_id == "batch_2").unwrap();
        assert_eq!(two.status, "done");
        assert_eq!(two.result.as_deref(), Some("the result"));
        assert_eq!(two.session_name, None);

        store.set_failed("batch_1", "timeout").unwrap();
        assert_eq!(reopened.clear_finished().unwrap(), 2); // both terminal now
        assert!(reopened.load_all().unwrap().is_empty());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn coordinator_store_roundtrip_and_resume() {
        let path = std::env::temp_dir().join(format!("aish_coord_{}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let store = CoordinatorStore::open(&path).unwrap();

        store.insert("run_1", "audit the repo", "sess-a", Some("alpha")).unwrap();
        // Idempotent insert (resume path) must not clobber the existing row.
        store.set_phase("run_1", "awaiting_batch").unwrap();
        store.insert("run_1", "audit the repo", "sess-a", Some("alpha")).unwrap();
        store.heartbeat("run_1").unwrap();

        store.insert("run_2", "draft release notes", "sess-b", None).unwrap();
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

        assert_eq!(reopened.clear_finished().unwrap(), 2); // both terminal now
        assert!(reopened.load_all().unwrap().is_empty());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn coordinator_messages_enqueue_drain_and_purge() {
        let path = std::env::temp_dir().join(format!("aish_coordmsg_{}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let store = CoordinatorStore::open(&path).unwrap();
        store.insert("run_1", "audit the repo", "sess-a", Some("alpha")).unwrap();

        // No messages yet.
        assert_eq!(store.pending_message_count("run_1").unwrap(), 0);
        assert!(store.drain_messages("run_1").unwrap().is_empty());

        // Enqueue two messages for run_1 and one for an unrelated run.
        store.enqueue_message("run_1", "focus on the auth module first", Some("sess-b")).unwrap();
        store.enqueue_message("run_1", "skip the e2e tests", None).unwrap();
        store.enqueue_message("run_2", "different run", None).unwrap();
        assert_eq!(store.pending_message_count("run_1").unwrap(), 2);

        // Drain run_1 — ordered oldest-first, scoped to run_1, delete-on-read.
        let drained = store.drain_messages("run_1").unwrap();
        assert_eq!(drained, vec![
            "focus on the auth module first".to_string(),
            "skip the e2e tests".to_string(),
        ]);
        // Second drain is empty (delete-on-read), and run_2's message is untouched.
        assert!(store.drain_messages("run_1").unwrap().is_empty());
        assert_eq!(store.pending_message_count("run_1").unwrap(), 0);
        assert_eq!(store.pending_message_count("run_2").unwrap(), 1);

        // A message survives across a process restart (fresh connection).
        store.enqueue_message("run_1", "one more note", None).unwrap();
        let reopened = CoordinatorStore::open(&path).unwrap();
        assert_eq!(reopened.drain_messages("run_1").unwrap(), vec!["one more note".to_string()]);

        // clear_finished purges orphaned messages (run_2 was never inserted as a
        // run, so its queued message has no owning run row → purged).
        reopened.clear_finished().unwrap();
        assert_eq!(reopened.pending_message_count("run_2").unwrap(), 0);

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

    #[test]
    fn allowlist_roundtrip() {
        let db = temp_db("allow");
        assert!(!db.is_allowed("git").unwrap());
        db.allow("git").unwrap();
        db.allow("git").unwrap(); // idempotent
        db.allow("npm").unwrap();
        assert!(db.is_allowed("git").unwrap());
        let names = |db: &Db| {
            db.allowed_tools().unwrap().into_iter().map(|(t, _)| t).collect::<Vec<_>>()
        };
        assert_eq!(names(&db), vec!["git", "npm"]);
        assert!(db.revoke("git").unwrap());
        assert!(!db.revoke("git").unwrap()); // already gone
        assert!(!db.is_allowed("git").unwrap());
        assert_eq!(names(&db), vec!["npm"]);
    }
}
