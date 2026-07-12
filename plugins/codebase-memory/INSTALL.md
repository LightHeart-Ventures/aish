# codebase-memory Plugin — Installation Guide

This guide walks through installing the **codebase-memory** plugin, which exposes the **DeusData/codebase-memory-mcp** MCP server for aish. The plugin gives you graph-based code intelligence tools (search, trace, architecture analysis) instead of grep.

See [`PREREQUISITES.md`](./PREREQUISITES.md) for detailed external dependency setup.

---

## Prerequisites

The plugin only supplies the **aish wiring**. You need the actual **codebase-memory-mcp** binary (MIT license).

### Option A — Homebrew (macOS / Linuxbrew, if available)

```sh
brew install deusdata/tap/codebase-memory-mcp
# or, if available in the default tap:
brew install codebase-memory-mcp

# Verify
which codebase-memory-mcp
codebase-memory-mcp --version  # Should print "codebase-memory-mcp 0.9.0" or later
```

If the tap name has changed upstream, check: https://github.com/DeusData/codebase-memory-mcp

### Option B — aish-native installer (recommended)

From inside aish REPL:

```
:codebase install
```

This downloads the latest **platform-matched release** (Linux x86_64/arm64, macOS x86_64/arm64) and installs it to `~/.aish/bin/codebase-memory-mcp`. 

Verify:

```
:codebase status
```

Then restart aish (`:restart` or exit and re-run `aish`).

### Option C — Manual download (all platforms)

**Supported targets:** `x86_64-unknown-linux-gnu`, `aarch64-unknown-linux-gnu`, `x86_64-apple-darwin`, `aarch64-apple-darwin`.

```sh
# Determine your platform
uname -s  # Linux / Darwin
uname -m  # x86_64 / arm64

# Linux x86_64 (example)
curl -sL https://github.com/DeusData/codebase-memory-mcp/releases/download/v0.9.0/codebase-memory-mcp-linux-amd64.tar.gz \
  | tar xz -C ~/.aish/bin/

# macOS arm64 (example)
curl -sL https://github.com/DeusData/codebase-memory-mcp/releases/download/v0.9.0/codebase-memory-mcp-darwin-arm64.tar.gz \
  | tar xz -C ~/.aish/bin/

# Make it executable
chmod +x ~/.aish/bin/codebase-memory-mcp

# Verify
~/.aish/bin/codebase-memory-mcp --version
```

See all releases: https://github.com/DeusData/codebase-memory-mcp/releases

### Option D — Build from source (Rust required)

```sh
git clone https://github.com/DeusData/codebase-memory-mcp
cd codebase-memory-mcp
cargo build --release

# Install to PATH
install -m 0755 target/release/codebase-memory-mcp ~/.aish/bin/
# OR to system:
sudo install -m 0755 target/release/codebase-memory-mcp /usr/local/bin/

# Verify
codebase-memory-mcp --version
```

---

## Verification

Follow these steps in order. Stop and fix if any step fails.

### Step 1: Binary on PATH

The plugin's `.mcp.json` invokes the bare command `codebase-memory-mcp`, so it must be reachable on `$PATH`.

```sh
which codebase-memory-mcp
# Output: e.g. /home/user/.aish/bin/codebase-memory-mcp
#         or /usr/local/bin/codebase-memory-mcp
#         or /opt/homebrew/bin/codebase-memory-mcp

# If not found, add ~/.aish/bin to PATH in ~/.aishrc:
echo 'export PATH="$HOME/.aish/bin:$PATH"' >> ~/.aishrc
source ~/.aishrc

# Then verify again
which codebase-memory-mcp
```

### Step 2: Check aish plugin discovery

From inside aish:

```
:mcp
```

You should see:

```
codebase-memory (stdio)
  Tools: 14
    - index_repository
    - search_graph
    - trace_path
    - query_graph
    - get_code_snippet
    - get_architecture
    - ...
```

**If missing or "skipped":**
- Restart aish: `:restart` or exit + `aish`
- Check stderr for spawn error (e.g. "No such file or directory")
- Confirm binary is on PATH: `which codebase-memory-mcp`

### Step 3: Index the current repo

The graph is empty until you index a repository. From inside aish:

```
index_repository { repo_path: ".", mode: "moderate" }
```

This builds the code graph. On first run, may take 30–60 seconds for a large monorepo. You'll see progress updates.

**Modes:**
- `fast` — text parsing only, no semantic analysis (seconds)
- `moderate` — text + light semantic edges (recommended, 30–120s depending on size)
- `full` — text + semantic + similarity (slower, 2–10 min)

### Step 4: Try a search

```
search_graph { project: ".", query: "function main" }
```

Should return a list of matching functions/symbols. If empty, the repo wasn't indexed yet (go back to Step 3).

### Step 5: Run the skill

Load and read the code-intelligence skill:

```
:skill add code-intelligence
```

This downloads the canonical skill documentation. Read it to learn the tools and workflows.

---

## Installation Summary

| Step | Command | Expected Output |
|------|---------|-----------------|
| 1. Install binary | `brew install codebase-memory-mcp` (or `:codebase install` inside aish) | Binary on PATH |
| 2. Restart aish | `:restart` | aish exits and re-opens |
| 3. Check enrollment | `:mcp` | `codebase-memory` appears with 14 tools |
| 4. Index repo | `index_repository { repo_path: "." }` | "Graph indexed: 42 functions, 15 imports, …" |
| 5. Test a query | `search_graph { project: ".", query: "function" }` | Function list returned |

---

## Troubleshooting

| Symptom | Cause | Fix |
|---------|-------|-----|
| `:mcp` doesn't show `codebase-memory` | Plugin not discovered, or binary not found | Restart aish, ensure binary is on PATH |
| "failed to spawn codebase-memory-mcp: No such file" | Binary not on PATH or wrong filename in `.mcp.json` | Run `which codebase-memory-mcp`, add `~/.aish/bin` to PATH in `~/.aishrc` |
| `index_repository` hangs / times out | Repo too large or indexing mode too aggressive | Try `mode: "fast"` instead of `"full"` |
| `search_graph` returns empty results | Repo not indexed yet, or graph is stale | Run `index_repository` first, or again if repo changed |
| "Connection refused" when fetching releases | Network issue or GitHub API rate limit | Check internet connection, retry in a minute, or use manual download |

---

## Next Steps

1. **Read the skill:**
   ```
   :skill add code-intelligence
   ```

2. **Try the main workflows:**
   - Find a definition: `search_graph { project: ".", query: "YourFunctionName" }`
   - Trace callers: `trace_path { project: ".", function_name: "main", direction: inbound }`
   - Find hot paths: `query_graph { project: ".", query: "MATCH (f:Function) WHERE f.cyclomatic >= 5 RETURN f.qualified_name" }`
   - Get architecture: `get_architecture { project: "." }`

3. **Configure optional settings:**
   - Edit `~/.aish/plugins/codebase-memory/.mcp.json` to change server options.
   - Set `AISH_CODEBASE_AUTO_INDEX=1` in `~/.aishrc` to warm the graph on every repo-open.

4. **Report issues:**
   - aish: https://github.com/LightHeart-Ventures/aish/issues
   - codebase-memory-mcp: https://github.com/DeusData/codebase-memory-mcp/issues

---

## Links

- **Plugin repo:** This directory (`~/.aish/plugins/codebase-memory/`)
- **Upstream server:** https://github.com/DeusData/codebase-memory-mcp
- **Releases:** https://github.com/DeusData/codebase-memory-mcp/releases
- **aish docs:** https://github.com/LightHeart-Ventures/aish
