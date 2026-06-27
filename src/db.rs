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

/// Run a memory-organization (dedup) pass every Nth `remember`, so the store
/// self-prunes near-identical rows without an explicit command. Best-effort.
const ORGANIZE_EVERY: i64 = 25;

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
        let conn =
            Connection::open(path).with_context(|| format!("can't open {}", path.display()))?;
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
             -- History-compaction transcripts live HERE, never in `memories`, so a
             -- routine recall of curated facts can't drag an MB-scale transcript in
             -- front of them (the offload token-blowup fix). Bounded by reap_offloads.
             CREATE TABLE IF NOT EXISTS offloads (
                 id      INTEGER PRIMARY KEY,
                 ts      TEXT NOT NULL DEFAULT current_timestamp,
                 content TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS allowed_tools (
                 tool TEXT PRIMARY KEY,
                 ts   TEXT NOT NULL DEFAULT current_timestamp
             );
             CREATE TABLE IF NOT EXISTS allowed_dirs (
                 perm TEXT NOT NULL,
                 dir  TEXT NOT NULL,
                 ts   TEXT NOT NULL DEFAULT current_timestamp,
                 PRIMARY KEY (perm, dir)
             );
             CREATE TABLE IF NOT EXISTS settings (
                 key   TEXT PRIMARY KEY,
                 value TEXT NOT NULL
             );",
        )
        .context("schema init failed")?;
        let db = Self { conn };
        db.migrate_memory_store();
        Ok(db)
    }

    /// One-time, idempotent memory-store migrations (safe to run on every open):
    ///   1. **Quarantine offloads** — move any legacy `context-offload` rows out
    ///      of `memories` into the dedicated `offloads` table so curated recall
    ///      never co-mingles with 2 MB transcripts (the token-blowup fix).
    ///   2. **FTS5 index** — create the `memories_fts` keyword index and backfill
    ///      it, so recall MATCHes an index instead of a raw `LIKE '%q%'` scan.
    ///   3. **Embeddings** — populate the long-dormant `embedding` column (and the
    ///      `vec_memories` mirror) for any un-embedded curated row, so recall can
    ///      rank by relevance instead of pure recency.
    ///
    /// Every step is best-effort: a failure (e.g. FTS5 not compiled in) degrades
    /// gracefully — recall falls back to a substring scan / recency ranking — and
    /// never sinks `open`.
    fn migrate_memory_store(&self) {
        // 1. Quarantine legacy offload transcripts into the offloads table.
        let _ = self.conn.execute(
            "INSERT INTO offloads (ts, content)
                 SELECT ts, content FROM memories WHERE tags = ?1",
            [crate::memory::OFFLOAD_TAG],
        );
        let _ = self
            .conn
            .execute("DELETE FROM memories WHERE tags = ?1", [crate::memory::OFFLOAD_TAG]);
        // 2. Keyword index (best-effort — needs FTS5 in the SQLite build).
        let _ = self.conn.execute_batch(
            "CREATE VIRTUAL TABLE IF NOT EXISTS memories_fts USING fts5(content, tags);",
        );
        let _ = self.conn.execute(
            "INSERT INTO memories_fts(rowid, content, tags)
                 SELECT id, content, coalesce(tags, '') FROM memories
                 WHERE id NOT IN (SELECT rowid FROM memories_fts)",
            [],
        );
        // 3. Backfill embeddings for any row that predates the embedder.
        self.backfill_embeddings();
    }

    /// Compute + store the embedding for every curated memory whose `embedding`
    /// column is still NULL (older rows, or rows written before the embedder was
    /// wired in). Best-effort and idempotent — once a row is embedded it's
    /// skipped on subsequent opens. The store is tiny (offloads are quarantined),
    /// so this is a cheap one-pass loop.
    fn backfill_embeddings(&self) {
        let rows: Vec<(i64, String)> = {
            let Ok(mut stmt) = self
                .conn
                .prepare("SELECT id, content FROM memories WHERE embedding IS NULL")
            else {
                return;
            };
            let Ok(mapped) = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?))) else {
                return;
            };
            mapped.filter_map(std::result::Result::ok).collect()
        };
        for (id, content) in rows {
            self.index_embedding(id, &content);
        }
    }

    /// Compute the embedding for `content` and persist it: the `memories.embedding`
    /// BLOB (read back for relevance ranking) plus the `vec_memories` vec0 mirror
    /// (the schema's KNN index, kept consistent for a future learned embedder).
    /// Both writes are best-effort so a vector hiccup never fails the caller.
    fn index_embedding(&self, id: i64, content: &str) {
        let v = crate::memory::embed(content);
        let blob = crate::memory::embed_to_blob(&v);
        let _ = self
            .conn
            .execute("UPDATE memories SET embedding = ?2 WHERE id = ?1", (id, &blob));
        // Mirror into the vec0 index (text JSON is the format sqlite-vec accepts).
        let _ = self
            .conn
            .execute("DELETE FROM vec_memories WHERE memory_id = ?1", [id]);
        let _ = self.conn.execute(
            "INSERT INTO vec_memories(memory_id, embedding) VALUES (?1, ?2)",
            (id, crate::memory::embed_to_json(&v)),
        );
    }

    /// The sqlite-vec version string — proves vector support is actually loaded.
    #[cfg(test)]
    pub fn vec_version(&self) -> Result<String> {
        Ok(self
            .conn
            .query_row("SELECT vec_version()", [], |r| r.get(0))?)
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

    /// The most recent command `input` rows (newest in the DB), returned in
    /// chronological order (oldest first) — the running command context the
    /// next-command suggestion (S6.3 / TASK-137) feeds the model. Capped at
    /// `limit`. Empty when nothing has been typed yet.
    pub fn recent_inputs(&self, limit: usize) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT content FROM history WHERE kind = 'input' ORDER BY id DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map([limit], |r| r.get::<_, String>(0))?;
        let mut out: Vec<String> = rows.filter_map(std::result::Result::ok).collect();
        out.reverse(); // newest-first query → chronological (oldest first)
        Ok(out)
    }

    pub fn remember(&self, content: &str, tags: Option<&str>) -> Result<i64> {
        let embedding = crate::memory::embed_to_blob(&crate::memory::embed(content));
        self.conn.execute(
            "INSERT INTO memories (content, tags, embedding) VALUES (?1, ?2, ?3)",
            (content, tags, &embedding),
        )?;
        let id = self.conn.last_insert_rowid();
        // Index the new row for keyword recall (best-effort — degrades to the
        // LIKE-scan fallback if FTS5 isn't available) and mirror its embedding
        // into the vec0 index. Neither must ever fail the remember itself.
        let _ = self.conn.execute(
            "INSERT INTO memories_fts(rowid, content, tags) VALUES (?1, ?2, ?3)",
            (id, content, tags.unwrap_or("")),
        );
        let _ = self.conn.execute(
            "INSERT INTO vec_memories(memory_id, embedding) VALUES (?1, ?2)",
            (id, crate::memory::embed_to_json(&crate::memory::embed(content))),
        );
        // Periodic self-organization: every ORGANIZE_EVERY writes, prune duplicate
        // memories so the store doesn't accumulate near-identical rows. Best-effort
        // — a failure here must never fail the remember itself.
        if ORGANIZE_EVERY > 0 && id % ORGANIZE_EVERY == 0 {
            let _ = self.organize_memories();
        }
        Ok(id)
    }

    /// Persist a history-compaction transcript to the dedicated `offloads`
    /// table (NOT `memories`), then bound the table so it can't grow without
    /// limit. Keeping transcripts out of `memories` is what stops a routine
    /// `recall` of curated facts from dragging an MB-scale blob into a tool
    /// result. Returns the new offload row id.
    pub fn remember_offload(&self, content: &str) -> Result<i64> {
        self.conn
            .execute("INSERT INTO offloads (content) VALUES (?1)", [content])?;
        let id = self.conn.last_insert_rowid();
        // Bound retention on every write (the table is small + the delete is
        // cheap). Best-effort — a reap failure must never fail the offload.
        let _ = self.reap_offloads(
            crate::memory::OFFLOAD_KEEP_RECENT,
            crate::memory::OFFLOAD_MAX_AGE_DAYS,
        );
        Ok(id)
    }

    /// The most-recent offload transcripts (newest first), each truncated to the
    /// recall hit cap so even a rehydration can't blow the context window. Backs
    /// the `recall "context-offload"` rehydration path.
    pub fn recall_offloads(&self, limit: usize) -> Result<Vec<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT ts, content FROM offloads ORDER BY id DESC LIMIT ?1")?;
        let rows = stmt.query_map([limit], |r| {
            let (ts, content): (String, String) = (r.get(0)?, r.get(1)?);
            Ok(format!(
                "[{ts}] {}",
                crate::memory::truncate_hit(&content, crate::memory::RECALL_HIT_MAX_CHARS)
            ))
        })?;
        Ok(rows.filter_map(std::result::Result::ok).collect())
    }

    /// Bound the `offloads` table: delete any transcript beyond the `keep_recent`
    /// most-recent rows OR older than `max_age_days`. Mirrors the coordinator's
    /// failed-run retention semantics (survive only when recent AND fresh).
    /// Returns the number of rows reaped.
    pub fn reap_offloads(&self, keep_recent: usize, max_age_days: i64) -> Result<usize> {
        let n = self.conn.execute(
            "DELETE FROM offloads
                 WHERE id NOT IN (SELECT id FROM offloads ORDER BY id DESC LIMIT ?1)
                    OR (julianday('now') - julianday(ts)) > ?2",
            (keep_recent as i64, max_age_days as f64),
        )?;
        Ok(n)
    }

    /// Relevance-ranked recall over CURATED memories (offloads are excluded —
    /// they live in their own table). An empty query returns the most-recent
    /// facts. Otherwise: generate keyword candidates via the FTS5 index (falling
    /// back to a `LIKE` scan when FTS is unavailable or matches nothing), then
    /// re-rank them by embedding cosine so the best match leads instead of merely
    /// the newest. Every hit is truncated to the recall cap. `query` empty → most
    /// recent memories.
    pub fn recall(&self, query: &str, limit: usize) -> Result<Vec<String>> {
        let q = query.trim();
        if q.is_empty() {
            return self.recent_memories(limit);
        }
        // FTS first (indexed); fall back to a substring scan on error/empty so a
        // SQLite build without FTS5 — or a query the tokenizer can't match —
        // still recalls.
        let mut cands = self
            .fts_candidates(q, crate::memory::RECALL_CANDIDATE_CAP)
            .unwrap_or_default();
        if cands.is_empty() {
            cands = self.like_candidates(q, crate::memory::RECALL_CANDIDATE_CAP)?;
        }
        // Relevance re-rank by embedding cosine when the query has a usable
        // embedding. Rows without an embedding sink below ranked ones but are
        // still returned (preserving recall), keeping their FTS/recency order.
        let qv = crate::memory::embed(q);
        if qv.iter().any(|&x| x != 0.0) {
            cands.sort_by(|a, b| {
                let score = |e: &Option<Vec<f32>>| {
                    e.as_ref()
                        .map(|v| crate::memory::cosine(&qv, v))
                        .unwrap_or(f32::MIN)
                };
                score(&b.embedding)
                    .partial_cmp(&score(&a.embedding))
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
        }
        Ok(cands.into_iter().take(limit).map(|c| c.render()).collect())
    }

    /// Most-recent curated memories (newest first), offloads excluded, each
    /// truncated to the recall cap — the empty-query recall path.
    fn recent_memories(&self, limit: usize) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT ts, content, coalesce(tags, '') FROM memories
                 WHERE tags IS NULL OR tags != ?2
                 ORDER BY id DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map((limit, crate::memory::OFFLOAD_TAG), |r| {
            Ok(MemoryHit {
                ts: r.get(0)?,
                content: r.get(1)?,
                tags: r.get(2)?,
                embedding: None,
            })
        })?;
        Ok(rows
            .filter_map(std::result::Result::ok)
            .map(|h| h.render())
            .collect())
    }

    /// Keyword candidates from the FTS5 index, ordered by bm25 rank. Returns an
    /// error when FTS5 is unavailable so `recall` can fall back to a scan.
    fn fts_candidates(&self, query: &str, cap: usize) -> Result<Vec<MemoryHit>> {
        let Some(m) = crate::memory::fts_match_query(query) else {
            return Ok(Vec::new());
        };
        let mut stmt = self.conn.prepare(
            "SELECT m.ts, m.content, coalesce(m.tags, ''), m.embedding
                 FROM memories_fts f JOIN memories m ON m.id = f.rowid
                 WHERE f MATCH ?1
                 ORDER BY rank LIMIT ?2",
        )?;
        let rows = stmt.query_map((m, cap), MemoryHit::from_row)?;
        Ok(rows.filter_map(std::result::Result::ok).collect())
    }

    /// Legacy substring candidates (`LIKE '%q%'`), offloads excluded — the
    /// fallback when FTS5 is unavailable or matched nothing.
    fn like_candidates(&self, query: &str, cap: usize) -> Result<Vec<MemoryHit>> {
        let pattern = format!("%{query}%");
        let mut stmt = self.conn.prepare(
            "SELECT ts, content, coalesce(tags, ''), embedding FROM memories
                 WHERE (content LIKE ?1 OR tags LIKE ?1)
                   AND (tags IS NULL OR tags != ?3)
                 ORDER BY id DESC LIMIT ?2",
        )?;
        let rows = stmt.query_map((pattern, cap, crate::memory::OFFLOAD_TAG), MemoryHit::from_row)?;
        Ok(rows.filter_map(std::result::Result::ok).collect())
    }

    /// Number of stored memories.
    pub fn memory_count(&self) -> Result<i64> {
        Ok(self
            .conn
            .query_row("SELECT count(*) FROM memories", [], |r| r.get(0))?)
    }

    /// Every stored memory (id ascending) — the input to an organization pass.
    pub fn all_memories(&self) -> Result<Vec<MemoryRow>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, content, coalesce(tags, '') FROM memories ORDER BY id")?;
        let rows = stmt.query_map([], |r| {
            Ok(MemoryRow {
                id: r.get(0)?,
                content: r.get(1)?,
                tags: r.get(2)?,
            })
        })?;
        Ok(rows.filter_map(std::result::Result::ok).collect())
    }

    /// Delete one memory (and any paired vector row) by id.
    pub fn delete_memory(&self, id: i64) -> Result<()> {
        self.conn
            .execute("DELETE FROM memories WHERE id = ?1", [id])?;
        // Keep the keyword index and the vec0 embedding mirror paired with the
        // row so neither outlives its memory (both best-effort).
        let _ = self
            .conn
            .execute("DELETE FROM memories_fts WHERE rowid = ?1", [id]);
        let _ = self
            .conn
            .execute("DELETE FROM vec_memories WHERE memory_id = ?1", [id]);
        Ok(())
    }

    /// Organize the memory store: prune exact-duplicate memories, keeping the
    /// newest of each duplicate set. Returns how many rows were removed.
    /// Deterministic + idempotent — the dedup decision is the pure [`dedup_plan`].
    pub fn organize_memories(&self) -> Result<usize> {
        let rows = self.all_memories()?;
        let victims = dedup_plan(&rows);
        for id in &victims {
            self.delete_memory(*id)?;
        }
        Ok(victims.len())
    }

    /// Add a tool/command to the persistent always-allow list (idempotent).
    pub fn allow(&self, tool: &str) -> Result<()> {
        self.conn.execute(
            "INSERT OR IGNORE INTO allowed_tools (tool) VALUES (?1)",
            [tool],
        )?;
        Ok(())
    }

    /// Is this tool/command on the always-allow list?
    pub fn is_allowed(&self, tool: &str) -> Result<bool> {
        Ok(self
            .conn
            .query_row(
                "SELECT 1 FROM allowed_tools WHERE tool = ?1",
                [tool],
                |_| Ok(()),
            )
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

    /// Grant `perm` (read|write|delete) recursively for everything under `dir`
    /// — the directory ('d') answer at a confirmation prompt. Idempotent.
    pub fn allow_dir(&self, perm: &str, dir: &str) -> Result<()> {
        self.conn.execute(
            "INSERT OR IGNORE INTO allowed_dirs (perm, dir) VALUES (?1, ?2)",
            (perm, dir),
        )?;
        Ok(())
    }

    /// Is `path` covered by a directory grant for `perm`? A grant on `dir`
    /// covers `dir` itself and everything beneath it (component-wise prefix, so
    /// `/a/b` grants `/a/b/c` but never `/a/bc`).
    pub fn is_dir_allowed(&self, perm: &str, path: &str) -> Result<bool> {
        let target = Path::new(path);
        let mut stmt = self
            .conn
            .prepare("SELECT dir FROM allowed_dirs WHERE perm = ?1")?;
        let dirs = stmt.query_map([perm], |r| r.get::<_, String>(0))?;
        for dir in dirs.filter_map(std::result::Result::ok) {
            if target.starts_with(&dir) {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Every directory grant as `(perm, dir, ts)`, ordered — backs the `:allow`
    /// listing.
    pub fn allowed_dirs(&self) -> Result<Vec<(String, String, String)>> {
        let mut stmt = self
            .conn
            .prepare("SELECT perm, dir, ts FROM allowed_dirs ORDER BY perm, dir")?;
        let rows = stmt.query_map([], |r| {
            let row: (String, String, String) = (r.get(0)?, r.get(1)?, r.get(2)?);
            Ok(row)
        })?;
        Ok(rows.filter_map(std::result::Result::ok).collect())
    }

    /// Drop a directory grant. Returns true if a row was removed.
    pub fn revoke_dir(&self, perm: &str, dir: &str) -> Result<bool> {
        let n = self.conn.execute(
            "DELETE FROM allowed_dirs WHERE perm = ?1 AND dir = ?2",
            (perm, dir),
        )?;
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
            .query_row("SELECT value FROM settings WHERE key = ?1", [key], |r| {
                r.get(0)
            })
            .optional()?)
    }
}

/// One recall candidate: the display fields plus the row's embedding (when
/// present) for relevance re-ranking. Rendered to the `[ts] (tags) content`
/// string the model sees, truncated to the recall hit cap.
struct MemoryHit {
    ts: String,
    content: String,
    tags: String,
    embedding: Option<Vec<f32>>,
}

impl MemoryHit {
    /// Build a hit from a `(ts, content, tags, embedding BLOB)` row.
    fn from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        let blob: Option<Vec<u8>> = r.get(3)?;
        Ok(MemoryHit {
            ts: r.get(0)?,
            content: r.get(1)?,
            tags: r.get(2)?,
            embedding: blob.as_deref().and_then(crate::memory::blob_to_embed),
        })
    }

    /// The `[ts] (tags) content` line the model sees, content truncated to the
    /// recall hit cap so no single fact can balloon a tool result.
    fn render(&self) -> String {
        let content = crate::memory::truncate_hit(&self.content, crate::memory::RECALL_HIT_MAX_CHARS);
        if self.tags.is_empty() {
            format!("[{}] {content}", self.ts)
        } else {
            format!("[{}] ({}) {content}", self.ts, self.tags)
        }
    }
}

/// One stored memory row — the unit an organization pass operates on.
#[derive(Debug, Clone)]
pub struct MemoryRow {
    pub id: i64,
    pub content: String,
    /// Tags carried with the row for completeness (and future tag-aware
    /// consolidation); dedup keys on `content` today, so this is write-only.
    #[allow(dead_code)]
    pub tags: String,
}

/// Normalize memory content for duplicate detection: trim ends, lowercase, and
/// collapse internal whitespace runs to a single space. So "User  Prefers Terse"
/// and "user prefers terse" are recognized as the same memory.
fn normalize_memory(s: &str) -> String {
    s.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase()
}

/// Pure dedup plan: given memories in any order, return the ids to DELETE so each
/// distinct normalized content keeps only its NEWEST row (highest id). The
/// returned ids are the older duplicates, sorted ascending. Order-independent
/// and idempotent (re-running on the survivors returns an empty plan).
pub fn dedup_plan(rows: &[MemoryRow]) -> Vec<i64> {
    use std::collections::HashMap;
    // normalized content -> highest id seen (the keeper).
    let mut keeper: HashMap<String, i64> = HashMap::new();
    for r in rows {
        let key = normalize_memory(&r.content);
        let slot = keeper.entry(key).or_insert(r.id);
        if r.id > *slot {
            *slot = r.id;
        }
    }
    let mut victims: Vec<i64> = rows
        .iter()
        .filter(|r| keeper.get(&normalize_memory(&r.content)).copied() != Some(r.id))
        .map(|r| r.id)
        .collect();
    victims.sort_unstable();
    victims
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
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
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
        self.conn.lock().unwrap().execute(
            "UPDATE batch_jobs SET status = ?2 WHERE local_id = ?1",
            (local_id, status),
        )?;
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
        Ok(self.conn.lock().unwrap().execute(
            "DELETE FROM batch_jobs WHERE status IN ('done', 'failed')",
            [],
        )?)
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
    /// The launching session's friendly name (`:rename`), for display.
    pub session_name: Option<String>,
    pub created_at: Option<String>,
    /// Last liveness beat (SQLite `current_timestamp` string). A run whose owner
    /// is gone and whose heartbeat is stale is treated as orphaned on reattach.
    pub heartbeat_at: Option<String>,
}

/// One run's terminal payload, read STRICTLY by `run_id` (TASK-205). A single
/// keyed row read with no shared/global "last result" slot, so a concurrent
/// completion of another run can never bleed into this lookup.
#[derive(Debug, Clone, PartialEq)]
pub struct RunResult {
    pub run_id: String,
    /// 'coordinating' | 'awaiting_batch' | 'done' | 'failed'.
    pub phase: String,
    pub result: Option<String>,
    pub error: Option<String>,
}

impl RunResult {
    /// Render the run's own result for display — the done answer, a failure
    /// note, or a still-running status. Mirrors `worker::WorkerJob::fetch`.
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
             PRAGMA synchronous = NORMAL;
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
                 ON run_aliases (run_id);",
        )
        .context("coordinator_runs schema init failed")?;
        // Back-compat: add session_name to a table created before it existed.
        // (session_id predates this; ignore the error when the column is present.)
        let _ = conn.execute(
            "ALTER TABLE coordinator_runs ADD COLUMN session_name TEXT",
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
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
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

    /// Bind `alias`->`run_id` ONCE at run creation (TASK-205 AC1). The write is
    /// immutable: a second bind for the same alias is a no-op
    /// (`ON CONFLICT(alias) DO NOTHING`), so the mapping captured at run start
    /// can never be mutated by a later — possibly racing — writer. `pr` records
    /// the opened pull request when known; it is informational and never affects
    /// resolution. Idempotent, so it is safe to call again on a resume.
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
        let n: i64 = db
            .conn
            .query_row("SELECT count(*) FROM history", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 2);

        db.remember("user prefers terse replies", Some("preference"))
            .unwrap();
        db.remember("project aios is a rust AI shell", Some("project"))
            .unwrap();
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
    fn recent_inputs_returns_chronological_capped() {
        let db = temp_db("recent_inputs");
        // Nothing typed yet.
        assert!(db.recent_inputs(10).unwrap().is_empty());
        // Outputs are ignored; only `input` rows are returned.
        db.record("input", "/tmp", "cd repo");
        db.record("output", "/tmp", "ok");
        db.record("input", "/tmp", "git status");
        db.record("input", "/tmp", "cargo test");
        // Chronological (oldest first), outputs excluded.
        assert_eq!(
            db.recent_inputs(10).unwrap(),
            vec![
                "cd repo".to_string(),
                "git status".to_string(),
                "cargo test".to_string()
            ]
        );
        // Cap keeps the NEWEST `limit`, still chronological.
        assert_eq!(
            db.recent_inputs(2).unwrap(),
            vec!["git status".to_string(), "cargo test".to_string()]
        );
    }

    #[test]
    fn settings_roundtrip() {
        let db = temp_db("settings");
        assert_eq!(db.get_setting("batch_mode").unwrap(), None);
        db.set_setting("batch_mode", "true").unwrap();
        assert_eq!(
            db.get_setting("batch_mode").unwrap().as_deref(),
            Some("true")
        );
        db.set_setting("batch_mode", "false").unwrap(); // upsert
        assert_eq!(
            db.get_setting("batch_mode").unwrap().as_deref(),
            Some("false")
        );
    }

    #[test]
    fn batch_store_roundtrip_and_reattach() {
        let path = std::env::temp_dir().join(format!("aish_batch_{}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let store = BatchStore::open(&path).unwrap();

        store
            .insert(
                "batch_1",
                "summarize logs",
                "claude-opus-4-8",
                "sess-a",
                Some("alpha"),
            )
            .unwrap();
        store.set_anthropic_id("batch_1", "msgbatch_abc").unwrap();
        store.set_status("batch_1", "in_progress").unwrap();
        store
            .insert("batch_2", "translate", "claude-opus-4-8", "sess-b", None)
            .unwrap();
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
    fn dedup_plan_keeps_newest_of_each_duplicate() {
        let rows = vec![
            MemoryRow {
                id: 1,
                content: "user prefers terse replies".into(),
                tags: "".into(),
            },
            // Same content modulo case + whitespace → a duplicate of #1.
            MemoryRow {
                id: 2,
                content: "User Prefers  Terse Replies".into(),
                tags: "pref".into(),
            },
            MemoryRow {
                id: 3,
                content: "project is a rust shell".into(),
                tags: "".into(),
            },
        ];
        // Keeps the newest of the dup pair (id 2) + the distinct id 3; deletes id 1.
        assert_eq!(dedup_plan(&rows), vec![1]);
        // Idempotent: running again on the survivors removes nothing.
        let survivors: Vec<MemoryRow> = rows.into_iter().filter(|r| r.id != 1).collect();
        assert!(dedup_plan(&survivors).is_empty());
        // No duplicates → empty plan.
        let distinct = vec![
            MemoryRow {
                id: 1,
                content: "a".into(),
                tags: "".into(),
            },
            MemoryRow {
                id: 2,
                content: "b".into(),
                tags: "".into(),
            },
        ];
        assert!(dedup_plan(&distinct).is_empty());
    }

    #[test]
    fn organize_memories_prunes_duplicates_in_store() {
        let db = temp_db("organize");
        db.remember("hello world", None).unwrap();
        db.remember("HELLO   world", Some("greet")).unwrap(); // dup of the first
        db.remember("a distinct fact", None).unwrap();
        assert_eq!(db.memory_count().unwrap(), 3);

        let removed = db.organize_memories().unwrap();
        assert_eq!(removed, 1);
        assert_eq!(db.memory_count().unwrap(), 2);

        // The surviving duplicate is the NEWEST row (the tagged one); recall it.
        let hits = db.recall("hello", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert!(hits[0].contains("greet"));

        // Idempotent — a second pass removes nothing.
        assert_eq!(db.organize_memories().unwrap(), 0);
    }

    #[test]
    fn offloads_are_quarantined_from_curated_recall() {
        let db = temp_db("offloads");
        db.remember("user prefers dark mode", Some("preference"))
            .unwrap();
        db.remember_offload("[context-offload] huge transcript about errors and builds")
            .unwrap();
        // Curated recall surfaces the fact, never the offload transcript.
        let dark = db.recall("dark", 10).unwrap();
        assert_eq!(dark.len(), 1);
        assert!(dark[0].contains("dark mode"));
        let trans = db.recall("transcript", 10).unwrap();
        assert!(
            trans.iter().all(|h| !h.contains("huge transcript")),
            "offload must not appear in curated recall: {trans:?}"
        );
        // memory_count counts only curated rows (offloads live elsewhere).
        assert_eq!(db.memory_count().unwrap(), 1);
        // The offload is reachable on its own channel.
        let offs = db.recall_offloads(10).unwrap();
        assert_eq!(offs.len(), 1);
        assert!(offs[0].contains("huge transcript"));
    }

    #[test]
    fn recall_truncates_oversized_hits() {
        let db = temp_db("trunc");
        let big = format!("alpha {}", "x".repeat(5000));
        db.remember(&big, None).unwrap();
        let hits = db.recall("alpha", 5).unwrap();
        assert_eq!(hits.len(), 1);
        assert!(hits[0].contains("KB elided"), "expected elision marker: {}", hits[0]);
        assert!(
            hits[0].chars().count() < big.chars().count(),
            "hit should be capped well under the original"
        );
    }

    #[test]
    fn reap_offloads_bounds_by_count_and_age() {
        let db = temp_db("reapoff");
        for i in 0..5 {
            db.remember_offload(&format!("offload number {i}")).unwrap();
        }
        // remember_offload reaps with the generous default keep — all 5 survive.
        assert_eq!(db.recall_offloads(50).unwrap().len(), 5);
        // Keep only the 2 most recent → 3 reaped.
        assert_eq!(db.reap_offloads(2, 9_999).unwrap(), 3);
        assert_eq!(db.recall_offloads(50).unwrap().len(), 2);
        // An ancient row is reaped by the age bound regardless of the keep count.
        db.conn
            .execute(
                "INSERT INTO offloads (ts, content) VALUES ('2000-01-01 00:00:00', 'ancient')",
                [],
            )
            .unwrap();
        let removed = db.reap_offloads(50, 7).unwrap();
        assert!(removed >= 1, "ancient offload should be aged out");
        assert!(
            db.recall_offloads(50)
                .unwrap()
                .iter()
                .all(|o| !o.contains("ancient")),
            "aged-out offload must be gone"
        );
    }

    #[test]
    fn recall_ranks_by_relevance_not_recency() {
        let db = temp_db("rank");
        // Older row, but a near-perfect token overlap with the query.
        db.remember("the rust compiler optimizes release builds", None)
            .unwrap();
        // Newer row, shares only the token "rust".
        db.remember("rust is a programming language", None).unwrap();
        let hits = db.recall("rust compiler optimizes release builds", 5).unwrap();
        assert!(!hits.is_empty());
        assert!(
            hits[0].contains("optimizes release builds"),
            "the highly-relevant older row must outrank the newer low-overlap one: {hits:?}"
        );
    }

    #[test]
    fn recall_falls_back_when_no_token_matches() {
        // A query whose tokens match nothing returns no curated hits (the LIKE
        // fallback also finds nothing) rather than erroring.
        let db = temp_db("nomatch");
        db.remember("alpha beta gamma", None).unwrap();
        assert!(db.recall("zzzznope", 5).unwrap().is_empty());
    }

    #[test]
    fn legacy_offload_rows_migrate_out_of_memories_on_open() {
        let path =
            std::env::temp_dir().join(format!("aish_migrate_{}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let db = Db::open(&path).unwrap();
        // Simulate the pre-migration state: an offload transcript living in
        // `memories` under the reserved tag (how compaction used to store it).
        db.conn
            .execute(
                "INSERT INTO memories (content, tags) VALUES (?1, ?2)",
                ("[context-offload] legacy transcript body", "context-offload"),
            )
            .unwrap();
        db.remember("a real curated fact", None).unwrap();
        drop(db);
        // Reopening runs the idempotent migration.
        let db2 = Db::open(&path).unwrap();
        // The legacy offload is gone from curated memories…
        assert_eq!(db2.memory_count().unwrap(), 1);
        assert!(db2
            .recall("legacy", 10)
            .unwrap()
            .iter()
            .all(|h| !h.contains("legacy transcript")));
        // …and now lives in the offloads table.
        assert!(db2
            .recall_offloads(10)
            .unwrap()
            .iter()
            .any(|o| o.contains("legacy transcript body")));
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
    fn dir_allowlist_roundtrip_and_prefix_match() {
        let db = temp_db("allowdir");
        // Nothing granted yet.
        assert!(!db.is_dir_allowed("read", "/tmp/proj/Cargo.toml").unwrap());
        // Grant read on a directory; it covers the dir and everything beneath.
        db.allow_dir("read", "/tmp/proj").unwrap();
        db.allow_dir("read", "/tmp/proj").unwrap(); // idempotent
        assert!(db.is_dir_allowed("read", "/tmp/proj/Cargo.toml").unwrap());
        assert!(db.is_dir_allowed("read", "/tmp/proj/src/main.rs").unwrap());
        assert!(db.is_dir_allowed("read", "/tmp/proj").unwrap());
        // Component-wise prefix: a sibling sharing a name prefix is NOT covered.
        assert!(!db.is_dir_allowed("read", "/tmp/proj2/x").unwrap());
        // Perm is scoped: a read grant is not a write grant.
        assert!(!db.is_dir_allowed("write", "/tmp/proj/Cargo.toml").unwrap());
        // Listing + revoke.
        db.allow_dir("write", "/tmp/other").unwrap();
        let dirs = db.allowed_dirs().unwrap();
        assert_eq!(
            dirs.iter()
                .map(|(p, d, _)| (p.as_str(), d.as_str()))
                .collect::<Vec<_>>(),
            vec![("read", "/tmp/proj"), ("write", "/tmp/other")]
        );
        assert!(db.revoke_dir("read", "/tmp/proj").unwrap());
        assert!(!db.revoke_dir("read", "/tmp/proj").unwrap()); // already gone
        assert!(!db.is_dir_allowed("read", "/tmp/proj/Cargo.toml").unwrap());
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
            db.allowed_tools()
                .unwrap()
                .into_iter()
                .map(|(t, _)| t)
                .collect::<Vec<_>>()
        };
        assert_eq!(names(&db), vec!["git", "npm"]);
        assert!(db.revoke("git").unwrap());
        assert!(!db.revoke("git").unwrap()); // already gone
        assert!(!db.is_allowed("git").unwrap());
        assert_eq!(names(&db), vec!["npm"]);
    }
}
