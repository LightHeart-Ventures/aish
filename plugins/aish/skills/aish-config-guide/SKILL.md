---
name: aish-config-guide
categories: [setup, configuration, operators]
applies-to: [aish]
unwanted-for: [design]
description: Comprehensive guide for configuring aish — environment, runtime settings, backend selection, credentials, MCP servers, and operator preferences.
allowed-tools: Read, Write, Edit, Bash
license: Proprietary
version: 1.0.0
tags: configuration, setup, operators, environment, credentials, mcp, backend, preferences
---

# aish Configuration Guide

You are helping an **operator** configure aish — the AI-native Rust shell. aish configuration lives in **three layers**:

1. **`~/.aishrc`** — shell sourced on startup; primary configuration file
2. **Environment variables** — runtime overrides (ANTHROPIC_API_KEY, MCP URLs, etc.)
3. **`~/.aish/aish.config`** — per-session preferences (optional, auto-created on first run)

This guide walks through each layer, covering typical use cases from "I just installed aish" to "I need to enable local inference" to "I want to use a custom MCP server."

---

## Quick Start: First-Time Setup

### 1. Install aish

```sh
curl https://raw.githubusercontent.com/LightHeart-Ventures/aish/main/aish.sh | sh
# Or if you have a pre-built binary, just place it in PATH
```

After install, aish creates `~/.aishrc` on first run. Check it:

```sh
cat ~/.aishrc
```

### 2. Set your API key (required)

The **only mandatory** configuration is an LLM API key. aish defaults to **Claude via Anthropic**, so:

```sh
# Export your Anthropic API key
export ANTHROPIC_API_KEY="sk-ant-v4-..."

# Persist it in ~/.aishrc
echo 'export ANTHROPIC_API_KEY="sk-ant-v4-..."' >> ~/.aishrc
```

Then reload:

```sh
source ~/.aishrc
aish
```

On first REPL prompt, aish verifies the key by testing a simple request. If it works, you're set.

### 3. (Optional) Pick your backend

By default, aish uses **Claude (Anthropic API)**. If you have a binary built with `--features local`, you can switch to **local inference** (llama.cpp in-process):

```sh
export AISH_BACKEND="local"  # Options: "anthropic" (default), "local"
```

Persist in `~/.aishrc`:

```sh
echo 'export AISH_BACKEND="local"' >> ~/.aishrc
source ~/.aishrc
```

**Local backend notes:**
- Requires a binary built with `--features local` (adds ~200 MB llama.cpp runtime)
- No external API calls; model inference runs in-process
- Slower than Claude on complex reasoning; best for simple tasks
- Does NOT require an API key

---

## Full Configuration Reference

### Environment Variables

#### **LLM Backend & Credentials**

| Variable | Purpose | Default | Example |
|----------|---------|---------|---------|
| `ANTHROPIC_API_KEY` | Anthropic (Claude) API key | (required) | `sk-ant-v4-abc123...` |
| `AISH_BACKEND` | Which backend to use | `anthropic` | `local` \| `anthropic` |
| `OPENAI_API_KEY` | (Reserved) Future OpenAI support | unset | `sk-...` |

**Setting up Anthropic:**

```sh
# One-time: set your key
export ANTHROPIC_API_KEY="sk-ant-v4-..."

# Persist forever
echo 'export ANTHROPIC_API_KEY="sk-ant-v4-..."' >> ~/.aishrc
```

Fetch a key at [console.anthropic.com](https://console.anthropic.com/account/keys).

#### **Model Selection**

| Variable | Purpose | Default | Example |
|----------|---------|---------|---------|
| `AISH_MODEL` | Override default Claude model | (auto-selected) | `claude-opus-4-1` \| `claude-3-5-sonnet-20241022` |
| `AISH_BACKEND_CONFIG` | Backend-specific JSON config | `{}` | `{"max_tokens":4000}` |

**Example: Force Haiku for cost control**

```sh
export AISH_MODEL="claude-3-5-haiku-20241022"
```

#### **Update Channel**

| Variable | Purpose | Default | Example |
|----------|---------|---------|---------|
| `AISH_UPDATE_CHANNEL` | Stable or dev releases | `stable` | `dev` \| `stable` |

**Example: Track dev releases**

```sh
export AISH_UPDATE_CHANNEL="dev"
aish
:update  # Fetches latest dev-vX.Y.Z-dev.N instead of vX.Y.Z
```

#### **MCP Servers**

| Variable | Purpose | Default | Example |
|----------|---------|---------|---------|
| `AISH_MCP_SERVERS` | JSON array of MCP server configs | `[]` | `[{"name":"atum","type":"stdio","command":"uvx atum"}]` |
| `AISH_MCP_ENDPOINT` | (Reserved) REST gateway endpoint | unset | `http://localhost:3000` |

See **"MCP Configuration"** section below for details.

#### **Logging & Observability**

| Variable | Purpose | Default | Example |
|----------|---------|---------|---------|
| `RUST_LOG` | Rust tracing level | `info` | `debug` \| `trace` \| `warn` |
| `AISH_JOURNAL_PATH` | Path to background run journal | `~/.atum/run-*.jsonl` | `/var/log/aish/runs/` |
| `AISH_DB_PATH` | SQLite coordinator database | `~/.aish/aish.db` | `/tmp/aish.db` |

**Example: Debug mode**

```sh
export RUST_LOG="debug"
aish  # Verbose stderr logging
```

#### **Development & Testing**

| Variable | Purpose | Default | Example |
|----------|---------|---------|---------|
| `AISH_TEST_MODE` | Disable real API calls (testing only) | `false` | `true` |
| `AISH_COORDINATOR_TIMEOUT` | Max seconds for background runs | `3600` | `7200` |
| `AISH_DISABLE_UPDATES` | Prevent automatic upgrade checks | `false` | `true` |

---

### `~/.aishrc` — Primary Configuration File

The **`~/.aishrc`** file is shell-sourced on aish startup. It's a bash script — use it to:

- Set environment variables
- Alias commands
- Define functions
- Conditional logic based on platform/context

**Example `.aishrc`:**

```bash
#!/bin/bash

# === Credentials ===
export ANTHROPIC_API_KEY="sk-ant-v4-..."

# === Backend Choice ===
# Use local inference if available, fallback to Anthropic
if command -v llama-cpp-python >/dev/null 2>&1; then
  export AISH_BACKEND="local"
else
  export AISH_BACKEND="anthropic"
fi

# === MCP Servers (see section below) ===
export AISH_MCP_SERVERS='[
  {
    "name": "atum",
    "type": "stdio",
    "command": "uvx atum",
    "enabled": true
  },
  {
    "name": "github",
    "type": "stdio",
    "command": "uvx @modelcontextprotocol/server-github --gh-token ${GITHUB_TOKEN}",
    "enabled": true
  }
]'

# === Update Channel ===
export AISH_UPDATE_CHANNEL="stable"

# === Logging ===
export RUST_LOG="info"

# === Platform-specific ===
if [[ "$OSTYPE" == "darwin"* ]]; then
  # macOS: enable native Keychain for secrets
  export AISH_KEYCHAIN="macos"
fi
```

**Best practices:**

- ✅ Use absolute paths for credentials files
- ✅ Conditionally set variables (check `command -v` before assuming a tool exists)
- ✅ Keep secrets out of version control — use `export VAR=$(cat ~/.secrets/key)` instead of inlining
- ❌ Don't set `AISH_BACKEND` to an unsupported value
- ❌ Don't share your `.aishrc` if it contains real API keys

---

### `~/.aish/aish.config` — Session Preferences

After the first aish session, a JSON file `~/.aish/aish.config` is created to store **runtime preferences**. This is auto-generated and typically human-readable:

```json
{
  "preferred_model": "claude-opus-4-1",
  "mode": "orchestrated",
  "background_coordinator_max_turns": 100,
  "turn_budget_per_coordinator": 500,
  "default_run_timeout_seconds": 3600,
  "telemetry_enabled": false,
  "interactive_mode": true
}
```

**Fields:**

| Field | Meaning | Default |
|-------|---------|---------|
| `preferred_model` | Your favorite model for a session | (auto-selected) |
| `mode` | `orchestrated` (cloud) or `local` | `orchestrated` |
| `background_coordinator_max_turns` | Max depth a background task can go | `100` |
| `turn_budget_per_coordinator` | Max total turns across all coordinators | `500` |
| `default_run_timeout_seconds` | Kill background runs after N seconds | `3600` |
| `telemetry_enabled` | Send usage stats (opt-in) | `false` |
| `interactive_mode` | Enable `:` commands in REPL | `true` |

You can edit this file directly:

```sh
nano ~/.aish/aish.config
```

On next `:reload`, the changes take effect.

---

## Common Scenarios

### Scenario 1: Set Up Local Inference (llama.cpp)

**Goal:** Run aish with local models, no external API.

**Steps:**

1. Get a binary with `--features local`:
   ```sh
   # Download a release built with local support (v0.20.0+)
   aish :update  # If you already have aish, check the version first
   ```

2. Set the backend:
   ```sh
   export AISH_BACKEND="local"
   echo 'export AISH_BACKEND="local"' >> ~/.aishrc
   ```

3. (Optional) Remove or clear ANTHROPIC_API_KEY if you want to ensure no external calls:
   ```sh
   # Comment it out in ~/.aishrc
   # export ANTHROPIC_API_KEY="..."
   ```

4. Test:
   ```sh
   aish
   # REPL prompt should appear; try a simple task
   say hello
   ```

**Notes:**
- Local inference is slower for complex reasoning
- First startup may download the model (~5–10 GB)
- No internet required after model is cached

### Scenario 2: Use Custom Claude Model

**Goal:** Use a specific Claude version (e.g., Haiku for cost control, or Opus 4.1 for reasoning).

**Steps:**

1. Edit `~/.aishrc`:
   ```sh
   export AISH_MODEL="claude-3-5-haiku-20241022"
   ```

2. Reload:
   ```sh
   source ~/.aishrc
   ```

3. Verify in REPL:
   ```sh
   aish
   :version  # Should show your model choice
   ```

**Available Claude models** (as of 2024):
- `claude-opus-4-1` — Best reasoning, highest cost
- `claude-3-5-sonnet-20241022` — Balanced, recommended
- `claude-3-5-haiku-20241022` — Fast & cheap, basic reasoning

### Scenario 3: Enable MCP Servers

**Goal:** Connect aish to external data sources or services via MCP.

**Steps:**

1. Install the MCP server binary (e.g., Atum):
   ```sh
   uvx atum --version  # Ensure it's callable
   ```

2. Add to `~/.aishrc`:
   ```sh
   export AISH_MCP_SERVERS='[
     {
       "name": "atum",
       "type": "stdio",
       "command": "uvx atum",
       "enabled": true
     }
   ]'
   ```

3. Reload and test:
   ```sh
   aish
   # On startup, aish connects to the MCP server
   # Try a tool from that server
   ```

**Troubleshooting MCP:**
- Check the server is callable: `uvx atum --help`
- Enable debug logging: `export RUST_LOG="debug"`
- Watch `~/.atum/mcp-*.log` for connection errors

### Scenario 4: Configure GitHub Token for GitHub MCP

**Goal:** Use the GitHub MCP server to query repos, PRs, issues, etc.

**Steps:**

1. Create a GitHub token:
   - Go to [github.com/settings/tokens](https://github.com/settings/tokens)
   - Create a **Personal Access Token (Fine-grained)** with repo + workflow scopes
   - Copy the token

2. Store it securely:
   ```sh
   mkdir -p ~/.secrets
   echo "ghp_xxxxx..." > ~/.secrets/github_token
   chmod 600 ~/.secrets/github_token
   ```

3. Add to `~/.aishrc`:
   ```sh
   export GITHUB_TOKEN=$(cat ~/.secrets/github_token)
   export AISH_MCP_SERVERS='[
     {
       "name": "github",
       "type": "stdio",
       "command": "uvx @modelcontextprotocol/server-github --gh-token ${GITHUB_TOKEN}",
       "enabled": true
     }
   ]'
   ```

4. Reload:
   ```sh
   source ~/.aishrc
   aish
   ```

### Scenario 5: Debug Background Coordinator Issues

**Goal:** Understand why a background task is slow or stuck.

**Steps:**

1. Enable debug logging:
   ```sh
   export RUST_LOG="debug"
   ```

2. Trigger a background task:
   ```sh
   aish
   :run echo "test" &  # & dispatches to background
   ```

3. Check the journal:
   ```sh
   tail -f ~/.atum/run-*.jsonl  # Watch the latest run
   ```

4. Check coordinator database:
   ```sh
   sqlite3 ~/.aish/aish.db "SELECT * FROM runs WHERE status='running';"
   ```

5. Adjust timeout if needed:
   ```sh
   export AISH_COORDINATOR_TIMEOUT="7200"  # 2 hours instead of 1
   ```

### Scenario 6: Team/Shared Machine Setup

**Goal:** Set up aish for multiple users on the same machine.

**Each user should have their own:**

1. `~/.aishrc` — per-user API key & preferences
2. `~/.aish/aish.config` — per-user session settings
3. `~/.aish/aish.db` — per-user run journal (auto-isolated by home directory)

**Shared/optional:**

- A centralized MCP server catalog at `/opt/mcp/servers.json` (configure path in `~/.aishrc`)

**Example team `~/.aishrc`:**

```bash
# Team defaults (shared via wiki or dotfiles repo)
export AISH_BACKEND="anthropic"
export AISH_UPDATE_CHANNEL="stable"

# Per-user credentials (never share)
export ANTHROPIC_API_KEY=$(cat ~/.secrets/anthropic_key)
export GITHUB_TOKEN=$(cat ~/.secrets/github_token)

# Shared MCP config
export AISH_MCP_SERVERS=$(cat /opt/mcp/servers.json)
```

---

## Environment Variable Precedence

aish resolves configuration in this order (first match wins):

1. **Command-line flags** (e.g., `aish --model claude-opus-4-1`)
2. **Environment variables** (e.g., `export AISH_MODEL="..."`)
3. **`~/.aishrc`** (sourced on startup)
4. **`~/.aish/aish.config`** (session defaults)
5. **Built-in defaults** (Claude, orchestrated mode, 3600s timeout)

**Practical implication:** if you set `AISH_MODEL` in `~/.aishrc`, it overrides the session config. To use session config, leave the environment variable unset.

---

## Troubleshooting Configuration

### "API key not found" / "unauthorized"

```sh
# Check if ANTHROPIC_API_KEY is set
echo $ANTHROPIC_API_KEY

# If empty, verify ~/.aishrc exports it
grep "ANTHROPIC_API_KEY" ~/.aishrc

# Reload and try again
source ~/.aishrc
aish
```

### "MCP server failed to connect"

```sh
# Check the server is callable
uvx atum --version  # or whatever server

# Enable debug logs
export RUST_LOG="debug"
aish

# Check MCP logs
ls -la ~/.atum/mcp-*.log
tail -f ~/.atum/mcp-*.log
```

### "Backend not recognized"

```sh
# Valid values: "anthropic", "local"
export AISH_BACKEND="anthropic"

# Check if local is available (requires binary built with --features local)
aish --version | grep local
```

### "aish is slow" / "background tasks timing out"

```sh
# Increase timeout
export AISH_COORDINATOR_TIMEOUT="7200"

# Check if coordinator is stuck
sqlite3 ~/.aish/aish.db "SELECT id, status, started_at FROM runs ORDER BY started_at DESC LIMIT 5;"

# Kill a stuck run (if necessary)
# aish :stop <run-id>
```

---

## Summary

**3-step setup:**

```sh
# 1. Set your API key
export ANTHROPIC_API_KEY="sk-ant-v4-..."
echo 'export ANTHROPIC_API_KEY="sk-ant-v4-..."' >> ~/.aishrc

# 2. Source it
source ~/.aishrc

# 3. Run aish
aish
```

**To go deeper:**
- Consult `~/.aishrc` for environment variables
- Edit `~/.aish/aish.config` for session preferences
- Check `~/.atum/run-*.jsonl` for background run details
- Enable `RUST_LOG=debug` for troubleshooting

For release management, configuration troubleshooting, or aish bugs, see the **aish_sre** skill.
