# Plugin-Scoped State / Config Store (Phase 1.5)

A single global SQLite database gives every aish plugin a small, durable,
namespaced key/value store. Implemented in [`src/plugin_state.rs`]; tests in
[`tests/plugin_state_tests.rs`].

## Location

```
~/.aish/database/plugins.db
```

All aish databases live under `~/.aish/database/` — see
[`database.md`](../database.md). One file for all plugins. Initialized
once on shell startup (see
[Initialization](#initialization)). WAL sidecars (`plugins.db-wal`,
`plugins.db-shm`) may appear next to it — that is normal for WAL journaling.

## Schema

```sql
CREATE TABLE IF NOT EXISTS plugin_state (
    plugin_id  TEXT NOT NULL,
    key        TEXT NOT NULL,
    value      TEXT NOT NULL,               -- JSON, serialized serde_json::Value
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at TEXT NOT NULL DEFAULT (datetime('now')),
    PRIMARY KEY (plugin_id, key)
);
PRAGMA user_version = 1;                    -- schema/migration marker
```

### Namespacing

Isolation is by the `plugin_id` column. The **composite primary key
`(plugin_id, key)`** means two plugins may use the same `key` without colliding,
and a plugin owns as many keys as it likes.

> **Deviation from the original brief.** The task sketch listed
> `plugin_id TEXT PRIMARY KEY`. A single plugin needs many keys, so a
> plugin-only primary key is wrong — the correct namespace-isolation key is the
> composite `(plugin_id, key)`, which is what ships.

### Values

Values are arbitrary JSON ([`serde_json::Value`]) serialized to TEXT. A plugin
can store a scalar (`json!(42)`), a string, or a nested config blob with one
API. `get`/`list_for_plugin` decode the TEXT back into a `Value`.

### Timestamps

`created_at` / `updated_at` are UTC strings from SQLite's `datetime('now')`.
`created_at` is preserved across updates (via `ON CONFLICT ... DO UPDATE`);
`updated_at` is refreshed on every `set`.

> **Deviation from the original brief.** The brief suggested
> `rusqlite = { features = ["bundled", "chrono"] }`. The repo already pins
> `rusqlite = { version = "0.32", features = ["bundled"] }` (plus
> `serde_json`), and using `datetime('now')` for timestamps avoids pulling
> `chrono` into the dependency graph and churning `Cargo.lock` (which would
> break the `--locked` CI gate). No `Cargo.toml` change was required.

## API

`PluginStateStore` (cloneable — the connection is shared behind
`Arc<Mutex<Connection>>`):

| Method | Signature | Notes |
|--------|-----------|-------|
| `open` | `open(path: &Path) -> Result<Self, String>` | Open/create a file-backed store. |
| `open_in_memory` | `open_in_memory() -> Result<Self, String>` | Private in-memory DB (tests). |
| `get` | `get(plugin_id, key) -> Result<Option<Value>, String>` | `None` if unset. |
| `set` | `set(plugin_id, key, &Value) -> Result<(), String>` | Upsert. |
| `delete` | `delete(plugin_id, key) -> Result<(), String>` | No-op on a missing key. |
| `list_for_plugin` | `list_for_plugin(plugin_id) -> Result<Vec<(String, Value)>, String>` | Ordered by key. |

Free functions for the process-wide instance:

- `init_global(path: &Path) -> Result<&'static PluginStateStore, String>` —
  idempotent; first successful call wins.
- `global() -> Option<&'static PluginStateStore>` — the store once initialized.

Every fallible call returns `Result<T, String>` so plugin-hook callers get a
flat, display-ready error without importing `rusqlite`'s error type.

## Initialization

`src/main.rs` calls `plugin_state::init_global(&db_paths::plugin_state_db_path())`
during interactive/one-shot startup, right after ensuring `~/.aish/` exists
(`db_paths::db_dir()` creates `~/.aish/database/` on first use). The
call is **non-fatal**: on error it logs a yellow `aish:` warning to stderr and
the shell continues — a bad or locked DB must never block launch. Plugin hooks
reach the store via `plugin_state::global()`.

## Error handling

- All DB/JSON failures are converted to `String` with contextual prefixes, e.g.
  `set(<plugin>, <key>): <sqlite error>`.
- A poisoned mutex surfaces as `plugin_state mutex poisoned: ...` rather than a
  panic.
- Concurrency: WAL journaling + a 5s `busy_timeout` let independent connections
  to the same file interleave writes without spurious `SQLITE_BUSY`. A cloned
  store shares one connection and serializes through its mutex.

## Migration path

Schema version is tracked in `PRAGMA user_version` (currently `1`).
`PluginStateStore::migrate` runs on every open:

1. `CREATE TABLE IF NOT EXISTS ...` (idempotent baseline).
2. Read `user_version`; if below `SCHEMA_VERSION`, apply forward migrations and
   stamp the new version.

To evolve the schema: bump `SCHEMA_VERSION`, add a gated arm in `migrate`
(e.g. `if current < 2 { conn.execute_batch("ALTER TABLE ...")?; }`), and update
this doc. Migrations run forward-only and are safe to re-run.

## Testing

```sh
cargo build --no-default-features --locked
cargo test  --no-default-features --locked plugin_state
```

`tests/plugin_state_tests.rs` covers schema init, set/get, namespace isolation,
delete, listing, and concurrent writes (3 tokio tasks × 100 writes to one
file-backed DB). Because `aish` is a binary crate with no library target, the
integration test compiles the module in directly via
`#[path = "../src/plugin_state.rs"]`.
