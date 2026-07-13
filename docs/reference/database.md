# Database Path Migration

## Status

**Active migration in progress:** SQLite databases are being consolidated from the flat `~/.aish/` layout into an organized `~/.aish/database/` subdirectory.

## Old Layout (Deprecated)

```text
~/.aish/
├── aish.db                  # ❌ DEPRECATED
├── plugins.db               # ❌ DEPRECATED
├── goal-coordinator-plan.db # ❌ DEPRECATED (exploratory)
├── ...other config files
```

## New Layout (Canonical)

```text
~/.aish/
├── database/
│   ├── aish.db              # ✅ Main store (history, memory, batch, coordinator runs)
│   ├── plugins.db           # ✅ Plugin-scoped key/value state
│   └── goal-coordinator-plan.db  # ✅ Goal planning & history (FUTURE)
├── ...other config files
```

## Rationale

As the number of on-disk databases grows (main store, plugin state, goal coordinator, future stores), the flat `~/.aish/` layout becomes noisy and hard to navigate. Consolidating all databases into `~/.aish/database/` keeps the config home tidy and gives callers one canonical place to resolve any DB path.

## Migration Path

**No automatic migration is performed.** Users must manually delete/rename old files:

```bash
# If you have old databases at ~/.aish/:
rm ~/.aish/aish.db
rm ~/.aish/plugins.db
rm ~/.aish/goal-coordinator-plan.db  # if it exists
```

The runtime will create fresh databases at the new locations on first startup after deletion.

## Code Pattern

All new database accesses **must** use `db_paths.rs` module:

```rust
use crate::db_paths::{main_db_path, plugin_state_db_path, db_dir};

// Canonical locations:
let main_db = main_db_path();      // ~/.aish/database/aish.db
let plugin_db = plugin_state_db_path();  // ~/.aish/database/plugins.db
let db_dir = db_dir();              // ~/.aish/database/

// For new databases (e.g., goal coordinator):
let goal_plan_db = db_dir().join("goal-coordinator-plan.db");
```

**DO NOT** hardcode paths like `~/.aish/goal-coordinator-plan.db` directly in code.

## Transition Timeline

- **Current (v0.33.0+):** New locations are canonical. Old files ignored.
- **v0.34.0+:** Consider warning users on startup if old `~/.aish/*.db` files are detected (UX improvement).
- **v0.35.0+:** Consider removing legacy hardcoded paths entirely (breaking change).

## Testing

All database accesses are tested through `db_paths.rs` tests:

```bash
cargo test db_paths
```
