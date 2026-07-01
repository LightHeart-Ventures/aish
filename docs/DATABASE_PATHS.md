# Database Paths

aish keeps all of its on-disk SQLite databases under a single directory,
`~/.aish/database/`, rather than scattering `*.db` files loose in the config
home. This keeps `~/.aish/` tidy and gives callers one canonical place to
resolve a database path.

Path resolution lives in [`src/db_paths.rs`](../src/db_paths.rs). Every DB open
in `src/main.rs` routes through it.

## Layout

```text
~/.aish/
├── .mcp.json              # MCP server config
├── skills/                # local skill catalog
├── registry/              # skill provider registry
├── plugins/               # installed plugins
└── database/              # ← all SQLite databases
    ├── aish.db            # main store: history, memory (vector recall),
    │                      #   batch jobs, coordinator runs
    ├── plugins.db         # plugin-scoped key/value state (Phase 1.5)
    └── *.db-wal, *.db-shm # WAL sidecars (normal for WAL journaling)
```

## Databases

| File | Constant (`db_paths`) | Accessor | Contents |
|------|-----------------------|----------|----------|
| `aish.db` | `MAIN_DB` | `main_db_path()` | Command history, vector memories (sqlite-vec), durable batch jobs, coordinator runs. Opened by `db::Db`, `db::BatchStore`, and `db::CoordinatorStore` — all three share the one file. |
| `plugins.db` | `PLUGIN_STATE_DB` | `plugin_state_db_path()` | Plugin-scoped state store — see [`plugin-state-schema.md`](./plugin-state-schema.md). |

## API

```rust
use crate::db_paths;

db_paths::db_dir()               // ~/.aish/database/   (created on first call)
db_paths::main_db_path()         // ~/.aish/database/aish.db
db_paths::plugin_state_db_path() // ~/.aish/database/plugins.db
```

`db_dir()` calls `fs::create_dir_all` on every invocation (idempotent), so any
of these paths can be handed straight to a DB `open` without a separate mkdir.

## Migration from the old flat layout

Older aish builds stored these files directly in the config home:

- `~/.aish/aish.db`
- `~/.aish/plugins.db`

There is **no automatic migration.** A fresh `~/.aish/database/` is created on
next launch and new databases are initialized empty. If you want to preserve
old history/memory, move the file yourself **before** first launch of the new
build:

```sh
mkdir -p ~/.aish/database
mv ~/.aish/aish.db     ~/.aish/database/aish.db
mv ~/.aish/plugins.db  ~/.aish/database/plugins.db
# also move any WAL sidecars if present:
mv ~/.aish/aish.db-wal ~/.aish/database/ 2>/dev/null
mv ~/.aish/aish.db-shm ~/.aish/database/ 2>/dev/null
```

Otherwise the stale `~/.aish/*.db` files are harmless and can be deleted at any
time:

```sh
rm -f ~/.aish/aish.db ~/.aish/aish.db-wal ~/.aish/aish.db-shm
rm -f ~/.aish/plugins.db ~/.aish/plugins.db-wal ~/.aish/plugins.db-shm
```

## Future databases

New durable stores should live here too and be resolved through `db_paths` —
for example a dedicated coordinator-journal database, a skill-provider cache, or
any per-subsystem store. Add a `*_DB` file-name constant plus a
`*_db_path()` accessor in `src/db_paths.rs` rather than joining a name onto the
config home directly.
