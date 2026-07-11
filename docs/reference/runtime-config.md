# Runtime Configuration — Environment Variables & Config File

aish is configured via environment variables and an optional config file. This document covers **all runtime-tunable knobs** — when to use each, defaults, ranges, and examples.

## Quick Start

**Config file** (`~/.aish/aish.config`): Optional INI-format file for persistent settings. Enables tuning without exporting env vars each session.

**Environment variables**: Override config file at runtime. Use for temporary tweaks or one-off commands.

**Example:**
```bash
# Use config file defaults for this session
aish

# Override coordinator max_rounds for this session only
AISH_COORDINATOR_MAX_ROUNDS=100 aish

# Combine: config file + env override
AISH_UPDATE_CHANNEL=dev aish
```

---

## Configuration File: `~/.aish/aish.config`

An **optional** INI-format file. If missing, all defaults are baked into the code. If present, it provides persistent defaults that can be overridden by environment variables.

### File Format

```ini
[section]
key = value
```

**Rules:**
- Sections are labels in brackets: `[coordinator]`, `[worker]`, etc.
- Key-value pairs are `name = value`.
- Values can be empty (blank string).
- Comments start with `#`.
- Environment variables **always** override the file (precedence: env var > config file > code default).

### Location

`~/.aish/aish.config` (in the user's `~/.aish/` directory).

---

## Environment Variables — Full Reference

All environment variables below can be set to override config file or code defaults.

### Coordinator & Loop Guards

Prevent infinite coordinator loops and tune iteration limits.

| Env Var | Config Key | Default | Range | Effect |
|---------|-----------|---------|-------|--------|
| `AISH_COORDINATOR_MAX_ROUNDS` | `[coordinator] max_rounds` | `48` | 1–1000 | Hard cap on turns per coordinator run. **Bandaid:** increase only if a legitimate task exhausts its budget. |
| `AISH_COORDINATOR_MAX_FAILED_ATTEMPTS` | `[coordinator] max_failed_attempts` | `3` | 0–1000 | Circuit breaker: fail fast on known-bad tasks. If the same task text has already failed ≥ N times, refuse to dispatch. `0` = disable. |
| `AISH_COORDINATOR_FAILED_KEEP` | `[coordinator] failed_keep` | `50` | 0–100000 | Max retained terminal `failed` rows for forensics. Older rows are reaped to keep the table bounded. `0` = keep none. |
| `AISH_COORDINATOR_FAILED_MAX_AGE_DAYS` | `[coordinator] failed_max_age_days` | `14` | 0–3650 | Retention window for failed runs (days). Works with `failed_keep` as a dual-bound. |

**When to tune:**
- **`max_rounds`**: A legitimate agent task is starving. Increase temporarily (e.g., `100`) to lift the cap without rebuilding. Default (48) covers 99% of work.
- **`max_failed_attempts`**: A task keeps failing; you want to skip re-trying the same approach. Set to `2` to fail faster, or `5` to tolerate more transient errors.
- **`failed_keep` / `failed_max_age_days`**: Heavy coordinator usage. Increase `failed_keep` to retain more history, or raise `failed_max_age_days` (e.g., `30`) to keep failed rows longer.

**Example:**
```bash
# Extend a single run that hit max_rounds
AISH_COORDINATOR_MAX_ROUNDS=100 aish

# Be aggressive on circuit breaking (fail after 2 attempts)
AISH_COORDINATOR_MAX_FAILED_ATTEMPTS=2 aish

# Retain all failed runs for a week
AISH_COORDINATOR_FAILED_KEEP=1000 AISH_COORDINATOR_FAILED_MAX_AGE_DAYS=7 aish
```

---

### Serial Chain Yield

Threshold before yielding long dependent-call chains.

| Env Var | Config Key | Default | Range | Effect |
|---------|-----------|---------|-------|--------|
| `AISH_SERIAL_CHAIN_YIELD_DEPTH` | `[serial_chain] yield_depth` | `12` | 1–1000 | Consecutive single-tool-call rounds allowed before gracefully yielding. Raise for genuinely-serial workloads whose dependent calls can't be batched. |

**When to tune:**
- Rarely. Only if you see false `serial-chain-yield` recoveries on legitimate work that is actually serial. Default (12) is conservative.

**Example:**
```bash
# Allow longer serial chains (e.g., deep dependency walks)
AISH_SERIAL_CHAIN_YIELD_DEPTH=30 aish
```

---

### Alerts & Audible Notifications

Configure bells on condition fire and worker completion.

| Env Var | Config Key | Default | Effect |
|---------|-----------|---------|--------|
| `AISH_ALERT_BELL` | `[alerts] bell` | `true` | Enable/disable audible bell when a `:alert` condition fires. Set to `false` in CI or headless servers. |
| `AISH_ALERT_BELL_CMD` | `[alerts] bell_cmd` | (system default) | Custom command to run when alert fires. Example: `paplay /usr/share/sounds/freedesktop/stereo/complete.oga`. |
| `AISH_WORKER_BELL` | `[alerts] bell_worker` | `true` | Enable/disable audible bell when `run_in_background` worker completes. |

**When to use:**
- **CI/CI runners**: Set `AISH_ALERT_BELL=false` to silence notifications.
- **Custom audio**: Set `bell_cmd` to play a custom sound (must be a quick executable).
- **Headless**: Disable both bells in non-interactive environments.

**Example:**
```bash
# Disable bells (e.g., in CI)
AISH_ALERT_BELL=false AISH_WORKER_BELL=false aish

# Use custom sound
AISH_ALERT_BELL_CMD="paplay /path/to/alert.oga" aish
```

---

### Background Dispatch & Deduplication

Control how `run_in_background` deduplicates overlapping tasks.

| Env Var | Config Key | Default | Effect |
|---------|-----------|---------|--------|
| `AISH_DISPATCH_DEDUP_SECS` | `[dispatch] dedup_secs` | (dynamic) | Time window (seconds) within which identical tasks are merged. `0` = no dedup. |

**When to tune:**
- Rarely. Only if dedup is stale or you want to disable it for testing. Default uses smart logic per task type.

---

### Plugin System

Configure the plugin registry and discovery.

| Env Var | Config Key | Default | Effect |
|---------|-----------|---------|--------|
| `AISH_PLUGINS_DIR` | `[plugins] dir` | `~/.aish/plugins` | Base directory where plugins are installed. Override to scan multiple sources or use a custom location. |

**Example:**
```bash
# Use a monorepo plugins directory
AISH_PLUGINS_DIR=/path/to/monorepo/plugins aish
```

---

### Telemetry & Logging

Configure logging and observability output.

| Env Var | Config Key | Default | Effect |
|---------|-----------|---------|--------|
| `AISH_REASONING_LOG` | `[telemetry] reasoning_log` | `~/.aish/reasoning-telemetry.jsonl` | Path to reasoning journal (traces reasoning loop steps). Leave empty to disable. |
| `AISH_REASONING_ROTATE_MB` | `[telemetry] reasoning_rotate_mb` | `100` | Rotate reasoning log at this size (MB). `0` = no rotation. |
| `AISH_REASONING_MEMO` | `[telemetry] reasoning_memo` | `~/.aish/reasoning-memo.jsonl` | Path to reasoning memoranda (persisted reasoning analysis). Leave empty to disable. |
| `AISH_CODEBASE_LOG` | `[telemetry] codebase_log` | `~/.aish/codebase-memory.jsonl` | Path to codebase memory journal. Leave empty to disable. |

**When to use:**
- **Troubleshooting reasoning loops**: Enable `reasoning_log` to see the step-by-step reasoning trace.
- **Disk space**: Disable or rotate logs if storage is constrained.
- **Offline analysis**: Point to a shared volume for centralized observability.

**Example:**
```bash
# Enable reasoning logging to troubleshoot a stuck coordinator
AISH_REASONING_LOG=/tmp/aish-reasoning.jsonl aish

# Disable codebase memory to save space
AISH_CODEBASE_LOG="" aish
```

---

### Model & Inference

Configure local vs remote model inference.

| Env Var | Config Key | Default | Effect |
|---------|-----------|---------|--------|
| `AISH_LOCAL_MODEL_PATH` | `[inference] local_model_path` | (empty) | Path to local llama.cpp model file (e.g., `llama-2-7b-q4_K_M.gguf`). Leave empty to use Claude (Anthropic API). Only affects runs when `local` feature is compiled. |
| `AISH_LOCAL_N_GPU_LAYERS` | `[inference] local_n_gpu_layers` | `0` | GPU layers for local model. `0` = CPU-only. Set to N > 0 to use GPU (CUDA/Metal). Requires GPU support. |
| `AISH_HF_BASE` | `[inference] hf_base` | `https://huggingface.co` | HuggingFace base URL for model fetching. Override for private mirrors or proxies. |
| `AISH_HF_REVISION` | `[inference] hf_revision` | `main` | HuggingFace git revision to fetch (branch, tag, or commit SHA). Use for version pinning. |

**When to use:**
- **Local inference**: Set `local_model_path` to a model file and optionally `local_n_gpu_layers` for GPU acceleration.
- **Bandwidth-limited environments**: Use `hf_base` to point to a local mirror.
- **Stable model versions**: Pin `hf_revision` to a specific commit for reproducibility.

**Example:**
```bash
# Use local llama-2 on CPU
AISH_LOCAL_MODEL_PATH=~/.models/llama-2-7b-q4_K_M.gguf aish

# Use local model with GPU (16 layers on GPU)
AISH_LOCAL_MODEL_PATH=~/.models/llama-2-13b.gguf AISH_LOCAL_N_GPU_LAYERS=16 aish

# Use a private HF mirror
AISH_HF_BASE=https://hf-mirror.example.com aish

# Pin to a stable model version
AISH_HF_REVISION=v1.2.3-stable aish
```

---

### Worker & Orchestration

Configure background worker container execution.

| Env Var | Config Key | Default | Effect |
|---------|-----------|---------|--------|
| `AISH_WORKER_RUNTIME` | `[worker] runtime` | `docker` | Container runtime for workers. Options: `docker`, `podman`, `nerdctl`. Used for `run_in_background` worktree isolation. |
| `AISH_WORKER_CPUS` | `[worker] cpus` | (no limit) | CPU limit for worker containers (e.g., `"2"`, `"0.5"`). Leave empty for no limit. |
| `AISH_WORKER_NETWORK` | `[worker] network` | `bridge` | Network mode for worker containers (e.g., `"host"`, `"none"`, `"bridge"`). Leave empty for default bridge. |
| `AISH_WORKER_STATE_DIR` | `[worker] worker_state_dir` | `~/.aish/worker-state` | Base directory for worker state. |
| `AISH_WORKTREE_DIR` | `[worker] worktree_dir` | `~/.aish/worktrees` | Base directory for worktrees. Use fast storage for heavy build/test workloads. |

**When to tune:**
- **podman users**: Set `runtime=podman` instead of Docker.
- **Resource-constrained environments**: Set `cpus` to limit worker container load (e.g., `"1"` for single-core).
- **Offline/air-gapped**: Set `network=none` for isolated workers.
- **High-performance storage**: Point `worktree_dir` to NVMe or ramdisk for faster builds.

**Example:**
```bash
# Use podman instead of docker
AISH_WORKER_RUNTIME=podman aish

# Limit worker to 1 CPU
AISH_WORKER_CPUS=1 aish

# Use SSD for worktrees (fast builds)
AISH_WORKTREE_DIR=/mnt/nvme/aish-worktrees aish

# Isolated network (offline workers)
AISH_WORKER_NETWORK=none aish
```

---

### Updates & Versioning

Control how aish discovers and installs updates.

| Env Var | Config Key | Default | Effect |
|---------|-----------|---------|--------|
| `AISH_UPDATE_CHANNEL` | `[updates] channel` | `release` | Release channel. Options: `release` (stable vX.Y.Z), `dev` (pre-release snapshots). Use `dev` to get latest features faster. |
| `AISH_UPDATE_REPO` | `[updates] repo` | `LightHeart-Ventures/aish` | GitHub repository for updates. Override for forks or mirrors. |
| `AISH_GITHUB_RAW_BASE` | `[updates] github_raw_base` | `https://raw.githubusercontent.com` | Base URL for GitHub raw content. Override to use a mirror. |

**When to use:**
- **Early adopter**: Set `channel=dev` to get latest snapshots before stable releases.
- **Corporate mirror**: Override `repo` and `github_raw_base` for internal releases.
- **Bandwidth savings**: Point `github_raw_base` to a cached mirror.

**Example:**
```bash
# Switch to dev release channel
AISH_UPDATE_CHANNEL=dev aish

# Use corporate fork
AISH_UPDATE_REPO=acme-corp/aish aish

# Use mirror for offline deployment
AISH_GITHUB_RAW_BASE=https://mirror.acme.internal aish
```

---

### Sessions & Debugging

Control session behavior and debug output.

| Env Var | Config Key | Default | Effect |
|---------|-----------|---------|--------|
| `AISH_LAUNCH_SESSION_NAME` | `[session] launch_session_name` | (none) | Named session ID to resume on launch (e.g., `"work"`, `"testing"`). Leave empty to start fresh each time. |
| `AISH_STARTUP_DIGEST` | `[session] startup_digest` | `false` | Print coordinator state digest on startup (useful for debugging). |

**Example:**
```bash
# Resume a named session
AISH_LAUNCH_SESSION_NAME=work aish

# Debug session state on startup
AISH_STARTUP_DIGEST=true aish
```

---

### Tooling & Sandboxing

Fine-grained tool access control.

| Env Var | Config Key | Default | Effect |
|---------|-----------|---------|--------|
| `AISH_TOOL_ALLOWLIST` | `[tools] allowlist` | (all tools) | Comma-separated list of allowed tools. Leave empty to allow all. Example: `"run_program,read_file,write_file"`. Used for sandboxing/testing. |

**When to use:**
- **Sandboxing**: Restrict coordinator to specific tools for safety testing.
- **Testing**: Block dangerous tools (e.g., `run_program`, `write_file`) in test scenarios.

**Example:**
```bash
# Allow only read/grep (no write/execute)
AISH_TOOL_ALLOWLIST="read_file,grep_files,glob_expand" aish

# For CI: allow only safe tools
AISH_TOOL_ALLOWLIST="run_program,read_file" aish
```

---

## Credentials & Secrets

**IMPORTANT:** Never put secrets in `~/.aish/aish.config`. Use environment variables or `.bashrc`/`.aishrc` instead.

**Required environment variables** (set these in your shell, not the config file):

| Env Var | Purpose | Example |
|---------|---------|---------|
| `ANTHROPIC_API_KEY` | Claude API key (for remote inference) | `sk-ant-...` (from console.anthropic.com) |
| `ATUM_TENANT` | Atum platform tenant ID | `t_abc123...` |
| `ATUM_API_KEY` | Atum API key | (from Atum console) |
| `ATUM_API_KEY_ID` | Atum API key ID | (from Atum console) |
| `SIGNOZ_API_KEY` | SigNoz observability API key | (from SigNoz console) |

**Setup (add to `~/.bashrc` or `~/.aishrc`):**
```bash
export ANTHROPIC_API_KEY="sk-ant-..."
export ATUM_TENANT="t_abc123..."
export ATUM_API_KEY="..."
export ATUM_API_KEY_ID="..."
export SIGNOZ_API_KEY="..."
```

Then source it before running aish:
```bash
source ~/.aishrc
aish
```

---

## Config File Example

Here's a typical `~/.aish/aish.config` for a developer with a local GPU:

```ini
[coordinator]
max_rounds = 100
max_failed_attempts = 2
failed_keep = 100
failed_max_age_days = 7

[alerts]
bell = true
bell_worker = true

[inference]
local_model_path = ~/.models/llama-2-13b.gguf
local_n_gpu_layers = 20

[worker]
runtime = docker
cpus = 2
worktree_dir = /mnt/nvme/aish-worktrees

[updates]
channel = dev

[telemetry]
reasoning_log = ~/.aish/reasoning-telemetry.jsonl
codebase_log = ~/.aish/codebase-memory.jsonl
```

---

## Troubleshooting

### "No such file or directory: ~/.aish/aish.config"
The config file is **optional**. If it doesn't exist, aish uses built-in defaults. No error.

### "I set AISH_COORDINATOR_MAX_ROUNDS=100 but it didn't work"
1. Verify the env var is exported: `echo $AISH_COORDINATOR_MAX_ROUNDS`
2. Check that you're using the right binary: `which aish`
3. Some shells (e.g., `sh`) don't inherit env vars from command line. Use `export` first:
   ```bash
   export AISH_COORDINATOR_MAX_ROUNDS=100
   aish
   ```

### "Config file is being ignored"
The file must be at `~/.aish/aish.config` (not `.config/aish` or other paths). Check the location:
```bash
cat ~/.aish/aish.config
```

### "Reasoning log is huge"
Set `AISH_REASONING_ROTATE_MB` to a smaller value (e.g., `10` MB) or disable:
```bash
AISH_REASONING_LOG="" aish
```

---

## See Also

- `docs/reference/coordinator/loop-guards.md` — Deep dive on loop prevention and circuit breakers
- `docs/RELEASE.md` — Release procedures and versioning strategy
- `docs/ARCHITECTURE.md` — System architecture and design
