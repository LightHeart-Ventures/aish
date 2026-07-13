# Runtime Configuration System

## Overview

The runtime configuration system provides pluggable, dynamically-reloadable configuration management for the aish coordinator and worker processes. It enables loading configuration from environment variables, TOML/JSON files, and remote endpoints, with hot-reload support for graceful updates without restarts.

## Architecture

### Components

**ConfigSource** — Union type representing configuration origin:
- `Environment { prefix }` — Load from env vars with given prefix
- `File { path }` — Load from local TOML/JSON file
- `Remote { url }` — Fetch from HTTP endpoint

**ConfigLoader** — Async trait for loading and watching config sources.

**Config** — Runtime schema:
```toml
version = "1.0.0"

[runtime]
max_workers = 10
max_coordinator_turns = 5
token_budget = 200_000
timeout_secs = 120

[limits]
max_file_read_bytes = 5_000
max_glob_results = 1000
max_background_jobs = 50

[features]
# Feature flags can be enabled here
```

**ConfigManager** — Arc-based hot-reload manager using RwLock for lock-free reads.

### Coordinator Integration

The coordinator loads config on startup and optionally watches for changes:

```rust
let loader = Box::new(DefaultConfigLoader::new());
let config_mgr = ConfigManager::new(Arc::new(loader));

// Load initial config
config_mgr.load_from_source(&ConfigSource::Environment { 
    prefix: "AISH_".to_string() 
}).await?;

// Optional: watch for changes
config_mgr.watch(&ConfigSource::File {
    path: PathBuf::from(".aishrc.toml")
}).await?;
```

On each coordinator turn:
1. **pre_run**: Load/refresh config from source
2. **run**: Use current config for decisions (worker count, token budgets, etc.)
3. **post_run**: Optionally persist new limits based on observability

### Hot-Reload Lifecycle

```
ConfigSource detected → watch() fires
  ↓
load() reads new config
  ↓
ConfigManager writes new Config via RwLock
  ↓
Next turn reads fresh values via get()
  ↓
No restarts needed; in-flight work continues
```

## Usage Examples

### Load from environment:

```bash
export AISH_RUNTIME_MAX_WORKERS=20
export AISH_RUNTIME_TOKEN_BUDGET=300000
export AISH_LIMITS_MAX_BACKGROUND_JOBS=100

# Coordinator auto-loads on startup
```

### Load from file:

```rust
config_mgr.load_from_source(&ConfigSource::File {
    path: PathBuf::from("~/.aish/config.toml")
}).await?;
```

### Watch file for changes:

```rust
config_mgr.watch(&ConfigSource::File {
    path: PathBuf::from("~/.aish/config.toml")
}).await?;

// Subsequent turns automatically pick up new values
```

## Defaults

If no explicit config is provided, sensible defaults apply (see `ConfigManager::default_config()`). Coordinator can override at runtime.

## Testing

Run `validate_pr.sh` to verify:
- Config loading from all source types
- Hot-reload without panic
- Backward compatibility
- Default fallbacks

## Future Extensions

- Hierarchical config merging (env + file + remote)
- Config validation schema (JSON Schema)
- Structured logging of config changes
- Admin API for remote config queries
- Per-agent/per-project config overrides
