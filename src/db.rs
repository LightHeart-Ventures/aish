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
