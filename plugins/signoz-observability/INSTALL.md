# signoz-observability Plugin — Installation Guide

This guide walks through installing the **signoz-observability** plugin, which exposes SigNoz observability tools to aish for querying logs, traces, metrics, and alerts. You'll need both the MCP server binary and access to a running SigNoz instance.

---

## Prerequisites

### 1. SigNoz Instance

You need a running **SigNoz deployment**. Choose one:

#### Option A — SigNoz Cloud (Hosted, Recommended)

1. Sign up at https://cloud.signoz.io
2. Create a new project (note the project name)
3. Copy your **API key** from Settings → API Keys
4. Note the **MCP endpoint URL** (typically `https://cloud.signoz.io` for the service)

#### Option B — Self-Hosted SigNoz (Docker/K8s)

```sh
# Docker Compose quickstart
git clone https://github.com/SigNoz/signoz
cd signoz/deploy
docker compose up -d

# Service will be at http://localhost:3301
# Create an API key in the UI: Settings → API Keys
```

See: https://signoz.io/docs/deployment/docker/

### 2. SigNoz API Key

Once you have a SigNoz instance:

1. Go to **Settings → API Keys**
2. Create a new API key with **read** scope (minimum: logs, traces, metrics query)
3. Copy the key (e.g. `signoz_key_abc123...`)
4. Add to `~/.aishrc`:

```sh
echo 'export SIGNOZ_API_KEY="<your-api-key>"' >> ~/.aishrc
echo 'export SIGNOZ_MCP_URL="http://localhost:3301/v1/mcp"' >> ~/.aishrc  # or your SigNoz Cloud URL
source ~/.aishrc
```

### 3. MCP Server Binary

The plugin needs the **signoz-observability MCP server**. Install one way:

#### Option A — aish-native installer (inside aish REPL)

```
:plugin install signoz-observability
```

This downloads and installs the server to `~/.aish/bin/signoz-observability-mcp`.

#### Option B — Manual download (all platforms)

```sh
# Linux x86_64
curl -sL https://github.com/LightHeart-Ventures/aish-plugins/releases/download/signoz-observability-v1.0.0/signoz-observability-mcp-linux-amd64.tar.gz \
  | tar xz -C ~/.aish/bin/

# macOS arm64
curl -sL https://github.com/LightHeart-Ventures/aish-plugins/releases/download/signoz-observability-v1.0.0/signoz-observability-mcp-darwin-arm64.tar.gz \
  | tar xz -C ~/.aish/bin/

chmod +x ~/.aish/bin/signoz-observability-mcp
```

See all releases: https://github.com/LightHeart-Ventures/aish-plugins/releases

#### Option C — Build from source

```sh
# (Requires this plugin repo checked out)
cd ~/.aish/plugins/signoz-observability/rust-sketch  # or wherever the source is
cargo build --release
cp target/release/signoz-observability-mcp ~/.aish/bin/
```

---

## Verification

### Step 1: Environment variables set

```sh
echo $SIGNOZ_API_KEY     # Should NOT be empty
echo $SIGNOZ_MCP_URL     # Should be your SigNoz URL
```

If empty, edit `~/.aishrc` and reload:

```sh
source ~/.aishrc
```

### Step 2: SigNoz instance is reachable

```sh
# Test health endpoint
curl -s "${SIGNOZ_MCP_URL%/v1/mcp}/health" | jq .
# or for Cloud:
curl -s "https://api.cloud.signoz.io/api/v1/health" -H "Authorization: Bearer $SIGNOZ_API_KEY" | jq .
```

Should return `{"status":"ok"}` or similar.

### Step 3: Binary on PATH

```sh
which signoz-observability-mcp
codebase-memory-mcp --version  # Should print "signoz-observability-mcp 1.0.0" or similar
```

If not found, add to PATH in `~/.aishrc`:

```sh
echo 'export PATH="$HOME/.aish/bin:$PATH"' >> ~/.aishrc
source ~/.aishrc
```

### Step 4: aish plugin discovery

From inside aish:

```
:mcp
```

You should see:

```
signoz-observability (stdio)
  Tools: 20+
    - search_logs
    - aggregate_logs
    - search_traces
    - query_metrics
    - list_alert_rules
    - create_alert
    - ...
```

**If missing:**
- Restart aish: `:restart`
- Check stderr for spawn error
- Verify binary is on PATH and credentials are set

### Step 5: Test a query

```
search_logs { service: "my-service", limit: 10 }
```

Should return recent logs from your service. If you get auth errors, check `SIGNOZ_API_KEY`.

### Step 6: Load the skill

```
:skill add signoz-observability/signoz-observability
```

This downloads the full tool reference and usage workflows.

---

## Installation Summary

| Step | Command | Expected Output |
|------|---------|-----------------|
| 1. Sign up for SigNoz | Visit https://cloud.signoz.io or self-host | SigNoz dashboard accessible |
| 2. Create API key | Settings → API Keys → New | API key copied |
| 3. Set env vars | `echo 'export SIGNOZ_API_KEY="..."' >> ~/.aishrc` | Credentials in shell |
| 4. Install binary | `:plugin install signoz-observability` (or manual download) | Binary on PATH |
| 5. Restart aish | `:restart` | aish re-opens |
| 6. Check enrollment | `:mcp` | `signoz-observability` shows 20+ tools |
| 7. Test query | `search_logs { service: "..." }` | Logs returned, no auth error |

---

## Troubleshooting

| Symptom | Cause | Fix |
|---------|-------|-----|
| `:mcp` doesn't show `signoz-observability` | Plugin not discovered, or binary not found | Restart aish, ensure binary is on PATH |
| "failed to spawn signoz-observability-mcp" | Binary not on PATH or not executable | `which signoz-observability-mcp`, `chmod +x ~/.aish/bin/signoz-observability-mcp` |
| "401 Unauthorized" in queries | API key is invalid or missing | Check `echo $SIGNOZ_API_KEY`, regenerate key in SigNoz UI, reload shell |
| "Connection refused" / "404 Not Found" | SigNoz URL is wrong or instance is down | Check `echo $SIGNOZ_MCP_URL`, test `curl "${SIGNOZ_MCP_URL%/v1/mcp}/health"` |
| Queries timeout | SigNoz is slow or overwhelmed | Reduce time range or limit in query, check SigNoz CPU/memory |
| Stale data / old logs | SigNoz retention is shorter than expected | Check Settings → Data Retention in SigNoz UI |

---

## Configuration

### Environment Variables

```sh
# Required
export SIGNOZ_API_KEY="..."                        # SigNoz API key
export SIGNOZ_MCP_URL="http://localhost:3301/v1/mcp"  # MCP endpoint (SigNoz Cloud uses https://...)

# Optional
export SIGNOZ_TIMEOUT="30"                         # Query timeout in seconds (default: 30)
export SIGNOZ_LOG_LEVEL="info"                     # MCP server log level
```

### Plugin Configuration

Edit `~/.aish/plugins/signoz-observability/.mcp.json` to customize the server:

```json
{
  "mcpServers": {
    "signoz-observability": {
      "type": "stdio",
      "command": "signoz-observability-mcp",
      "args": ["--log-level", "debug"],
      "env": {
        "SIGNOZ_API_KEY": "${SIGNOZ_API_KEY}",
        "SIGNOZ_MCP_URL": "${SIGNOZ_MCP_URL}"
      }
    }
  }
}
```

---

## Next Steps

1. **Read the skill:**
   ```
   :skill add signoz-observability/signoz-observability
   ```

2. **Try the main workflows:**
   - Query logs: `search_logs { service: "payment-api", severity: "ERROR" }`
   - Query traces: `search_traces { service: "checkout", error: true }`
   - Query metrics: `query_metrics { metricName: "http_server_duration" }`
   - List alerts: `list_alert_rules {}`

3. **Set up alerting:**
   - Create a metric alert: `create_alert { alert: "HighErrorRate", ... }`
   - View alert history: `get_alert_history { id: "..." }`

4. **Monitor dashboards:**
   - Create a dashboard via the SigNoz UI
   - Reference it from aish queries or notes

5. **Report issues:**
   - aish: https://github.com/LightHeart-Ventures/aish/issues
   - SigNoz: https://github.com/SigNoz/signoz/issues

---

## Links

- **Plugin repo:** This directory (`~/.aish/plugins/signoz-observability/`)
- **SigNoz Cloud:** https://cloud.signoz.io
- **SigNoz self-hosted:** https://github.com/SigNoz/signoz
- **SigNoz docs:** https://signoz.io/docs/
- **aish docs:** https://github.com/LightHeart-Ventures/aish
