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

    /// Every always-allowed tool, alphabetical.
    pub fn allowed_tools(&self) -> Result<Vec<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT tool FROM allowed_tools ORDER BY tool")?;
        let rows = stmt.query_map([], |r| r.get(0))?;
        Ok(rows.filter_map(std::result::Result::ok).collect())
    }

    /// Drop a tool from the always-allow list. Returns true if a row was removed.
    pub fn revoke(&self, tool: &str) -> Result<bool> {
        let n = self
            .conn
            .execute("DELETE FROM allowed_tools WHERE tool = ?1", [tool])?;
        Ok(n > 0)
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
    fn allowlist_roundtrip() {
        let db = temp_db("allow");
        assert!(!db.is_allowed("git").unwrap());
        db.allow("git").unwrap();
        db.allow("git").unwrap(); // idempotent
        db.allow("npm").unwrap();
        assert!(db.is_allowed("git").unwrap());
        assert_eq!(db.allowed_tools().unwrap(), vec!["git", "npm"]);
        assert!(db.revoke("git").unwrap());
        assert!(!db.revoke("git").unwrap()); // already gone
        assert!(!db.is_allowed("git").unwrap());
        assert_eq!(db.allowed_tools().unwrap(), vec!["npm"]);
    }
}
