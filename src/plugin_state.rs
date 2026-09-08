//! Plugin-scoped state / config store (Phase 1.5).
//!
//! A single global SQLite database at `~/.aish/database/plugins.db` gives every aish
//! plugin a small, durable, namespaced key/value store. Namespacing is by
//! `plugin_id`: two plugins can use the same `key` without colliding because the
//! primary key is the composite `(plugin_id, key)`. Values are arbitrary JSON
//! ([`serde_json::Value`]) serialized to TEXT, so a plugin can persist a scalar,
//! a string, or a nested config blob with one API.
//!
//! Design decisions (from the Phase 1.5 task brief):
//!   * ONE global DB file, not one-per-plugin — cheap to open, easy to back up.
//!   * Per-plugin namespace via a `plugin_id` column prefix rather than a table
//!     per plugin — no runtime DDL, no table-name injection surface.
//!   * `Result<T, String>` on every fallible call so plugin-hook callers get a
//!     flat, display-ready error without pulling `rusqlite`'s error type into
//!     their signatures.
//!
//! NOTE on the brief's schema sketch: it listed `plugin_id TEXT PRIMARY KEY`,
//! but a single plugin obviously needs many keys, so the PRIMARY KEY here is the
//! composite `(plugin_id, key)` — the only correct choice for namespace
//! isolation. Timestamps are stored as SQLite `datetime('now')` TEXT (UTC) so we
//! avoid pulling `chrono` into the build; see `docs/reference/plugins/state.md`.
//!
//! This module is intentionally self-contained (only `std`, `rusqlite`, and
//! `serde_json`) so `tests/plugin_state_tests.rs` can include it directly with
//! `#[path = "../src/plugin_state.rs"]` — the crate is a binary, so there is no
//! library target for an integration test to `use aish::...` from.

// The public API (get/set/delete/list_for_plugin, global accessors) exists for
// plugin hooks that land in later phases; only `init_global` is wired at
// startup today, so quiet the not-yet-consumed-surface warnings.
#![allow(dead_code)]

use rusqlite::{Connection, OptionalExtension};
use serde_json::Value;
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};

/// Schema DDL. `IF NOT EXISTS` makes init idempotent — safe to run on every
/// startup. The `PRAGMA user_version` doubles as a migration marker (see the
/// migration path in `docs/reference/plugins/state.md`).
const INIT_SQL: &str = "\
CREATE TABLE IF NOT EXISTS plugin_state (
    plugin_id  TEXT NOT NULL,
    key        TEXT NOT NULL,
    value      TEXT NOT NULL,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at TEXT NOT NULL DEFAULT (datetime('now')),
    PRIMARY KEY (plugin_id, key)
);";

/// Current on-disk schema version. Bump when the schema changes and add a
/// migration arm keyed off `PRAGMA user_version`.
const SCHEMA_VERSION: i64 = 1;

/// A namespaced key/value store backed by one SQLite database.
///
/// Cloning is cheap: the connection is shared behind an `Arc<Mutex<..>>`, so a
/// clone points at the SAME underlying connection. That makes the store trivial
/// to hand to async tasks (`tokio::spawn`) or store in a global — every access
/// serializes through the mutex, which is exactly the mutual exclusion a single
/// SQLite connection needs.
#[derive(Clone)]
pub struct PluginStateStore {
    conn: Arc<Mutex<Connection>>,
}

impl PluginStateStore {
    /// Open (creating if absent) the store at `path` and ensure the schema
    /// exists. Enables WAL + a busy timeout so multiple independent connections
    /// to the same file can write concurrently without spurious `SQLITE_BUSY`.
    pub fn open(path: &Path) -> Result<Self, String> {
        let conn = Connection::open(path).map_err(|e| format!("open plugins.db: {e}"))?;
        Self::from_conn(conn)
    }

    /// Open a private in-memory store — used by tests for isolation. Each
    /// in-memory database lives only as long as its connection.
    pub fn open_in_memory() -> Result<Self, String> {
        let conn = Connection::open_in_memory().map_err(|e| format!("open :memory:: {e}"))?;
        Self::from_conn(conn)
    }

    /// Wrap an existing connection: apply pragmas, run migrations, wrap it.
    fn from_conn(conn: Connection) -> Result<Self, String> {
        // Best-effort concurrency pragmas. WAL is a no-op / harmless on an
        // in-memory database; a 5s busy timeout lets a blocked writer wait for
        // the lock instead of erroring out immediately.
        let _ = conn.pragma_update(None, "journal_mode", "WAL");
        let _ = conn.pragma_update(None, "busy_timeout", 5000);
        Self::migrate(&conn)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    /// Idempotent schema init + forward migration, keyed off
    /// `PRAGMA user_version`. Version 0 (fresh DB) creates the table and stamps
    /// the version; later versions would add ALTER arms here.
    fn migrate(conn: &Connection) -> Result<(), String> {
        conn.execute_batch(INIT_SQL)
            .map_err(|e| format!("create schema: {e}"))?;
        let current: i64 = conn
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .map_err(|e| format!("read user_version: {e}"))?;
        if current < SCHEMA_VERSION {
            // Future migrations: `if current < 2 { conn.execute_batch(...); }`
            conn.pragma_update(None, "user_version", SCHEMA_VERSION)
                .map_err(|e| format!("set user_version: {e}"))?;
        }
        Ok(())
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Connection>, String> {
        self.conn
            .lock()
            .map_err(|e| format!("plugin_state mutex poisoned: {e}"))
    }

    /// Fetch the JSON value stored under `(plugin_id, key)`, or `None` if unset.
    pub fn get(&self, plugin_id: &str, key: &str) -> Result<Option<Value>, String> {
        let conn = self.lock()?;
        let raw: Option<String> = conn
            .query_row(
                "SELECT value FROM plugin_state WHERE plugin_id = ?1 AND key = ?2",
                (plugin_id, key),
                |row| row.get(0),
            )
            .optional()
            .map_err(|e| format!("get({plugin_id}, {key}): {e}"))?;
        match raw {
            Some(s) => serde_json::from_str(&s)
                .map(Some)
                .map_err(|e| format!("decode value for ({plugin_id}, {key}): {e}")),
            None => Ok(None),
        }
    }

    /// Upsert `value` under `(plugin_id, key)`. Preserves `created_at` on an
    /// update and always refreshes `updated_at`.
    pub fn set(&self, plugin_id: &str, key: &str, value: &Value) -> Result<(), String> {
        let encoded = serde_json::to_string(value)
            .map_err(|e| format!("encode value for ({plugin_id}, {key}): {e}"))?;
        let conn = self.lock()?;
        conn.execute(
            "INSERT INTO plugin_state (plugin_id, key, value, created_at, updated_at)
             VALUES (?1, ?2, ?3, datetime('now'), datetime('now'))
             ON CONFLICT(plugin_id, key)
             DO UPDATE SET value = excluded.value, updated_at = datetime('now')",
            (plugin_id, key, &encoded),
        )
        .map_err(|e| format!("set({plugin_id}, {key}): {e}"))?;
        Ok(())
    }

    /// Remove `(plugin_id, key)`. Deleting a missing key is a no-op (Ok).
    pub fn delete(&self, plugin_id: &str, key: &str) -> Result<(), String> {
        let conn = self.lock()?;
        conn.execute(
            "DELETE FROM plugin_state WHERE plugin_id = ?1 AND key = ?2",
            (plugin_id, key),
        )
        .map_err(|e| format!("delete({plugin_id}, {key}): {e}"))?;
        Ok(())
    }

    /// Every `(key, value)` for one plugin, ordered by key. An empty vec means
    /// the plugin has stored nothing (or was never seen).
    pub fn list_for_plugin(&self, plugin_id: &str) -> Result<Vec<(String, Value)>, String> {
        let conn = self.lock()?;
        let mut stmt = conn
            .prepare("SELECT key, value FROM plugin_state WHERE plugin_id = ?1 ORDER BY key")
            .map_err(|e| format!("list_for_plugin prepare: {e}"))?;
        let rows = stmt
            .query_map((plugin_id,), |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .map_err(|e| format!("list_for_plugin query: {e}"))?;
        let mut out = Vec::new();
        for row in rows {
            let (k, raw) = row.map_err(|e| format!("list_for_plugin row: {e}"))?;
            let val = serde_json::from_str(&raw)
                .map_err(|e| format!("decode value for ({plugin_id}, {k}): {e}"))?;
            out.push((k, val));
        }
        Ok(out)
    }
}

/// Process-wide store, initialized once on shell startup.
static GLOBAL: OnceLock<PluginStateStore> = OnceLock::new();

/// Initialize (once) and return the global plugin-state store backed by the file
/// at `path` (typically `~/.aish/database/plugins.db`). Idempotent: the first successful
/// call wins and subsequent calls return the already-initialized store,
/// ignoring `path`. Startup wiring calls this and logs — but does not fail on —
/// an error, so a bad DB never blocks the shell from launching.
pub fn init_global(path: &Path) -> Result<&'static PluginStateStore, String> {
    if let Some(existing) = GLOBAL.get() {
        return Ok(existing);
    }
    let store = PluginStateStore::open(path)?;
    // Race-safe: if another thread set it first, keep theirs.
    let _ = GLOBAL.set(store);
    Ok(GLOBAL.get().expect("global set above"))
}

/// The global store if [`init_global`] has run successfully, else `None`.
/// Plugin hooks call this to reach the store without threading it through every
/// call site.
pub fn global() -> Option<&'static PluginStateStore> {
    GLOBAL.get()
}
