#!/bin/bash
# Scripts to support runtime config file reading
# These are placeholder stubs for the PR. Integration happens in:
# - src/config.rs (new): Config file parser (INI format)
# - src/lib.rs: expose load_config()
# - src/main.rs: call load_config() at startup
# - src/coordinator.rs: read from config for coordinator knobs
# - src/update.rs: read from config for update settings
# - etc.

cat <<'EOF'
===============================================================================
RUNTIME CONFIG INTEGRATION PLAN
===============================================================================

## Files Created/Modified

### NEW FILES
1. ~/.aish/aish.config — Sample INI config file (already created)
2. docs/reference/runtime-config.md — Comprehensive reference (already created)
3. src/config.rs (NEW) — Config file parser module

### MODIFIED FILES
1. docs/INDEX.md — Link to runtime-config.md (already done)
2. src/lib.rs — Export config module + load_config() function
3. src/main.rs — Call load_config() at startup
4. src/coordinator.rs — Read AISH_COORDINATOR_* from env/config
5. src/update.rs — Read AISH_UPDATE_* from env/config
6. src/tools.rs — Read AISH_WORKER_BELL, AISH_ALERT_BELL from env/config
7. src/engine.rs — Read AISH_SERIAL_CHAIN_YIELD_DEPTH from env/config
8. src/session.rs — Read session-related knobs

## Implementation Pattern

Each subsystem follows this precedence (highest to lowest):
1. Environment variable (AISH_*)
2. Config file (~/.aish/aish.config)
3. Code default (hardcoded)

### Example: Coordinator max_rounds

Before:
```rust
const MAX_ROUNDS: usize = 36;  // hardcoded
let max_rounds = MAX_ROUNDS;
```

After:
```rust
use crate::config::Config;

// Load config file (if exists) + env overrides
let config = Config::load().expect("config");

// Precedence: env > config file > default
let max_rounds = std::env::var("AISH_COORDINATOR_MAX_ROUNDS")
    .ok()
    .and_then(|s| s.parse().ok())
    .or_else(|| config.coordinator.max_rounds)
    .unwrap_or(48);  // code default
```

## Config Parser Implementation (src/config.rs)

A lightweight INI parser that:
- Reads ~/.aish/aish.config (or returns empty config if missing)
- Parses [section] and key=value pairs
- Returns a Config struct with typed fields
- Ignores comments (#) and blank lines
- Does NOT require external deps (use standard lib only)

## Testing

Unit tests in src/config.rs:
- Parse sample config file
- Verify precedence (env > config > default)
- Gracefully handle missing file
- Validate type conversions (string → int, string → bool)

## Documentation

- README or INSTALL.md should mention ~/.aish/aish.config
- Help text (`:help config`) or new `:config show` command (optional)
- Example config file already in ~/.aish/aish.config
- Reference doc (docs/reference/runtime-config.md) is comprehensive

===============================================================================
EOF
