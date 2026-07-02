# aish Plugin System Design

## Overview

The aish plugin system extends the shell with modular, composable capabilities. Plugins live in `~/.aish/plugins/<plugin-id>/` and can provide:
- **MCP servers** for integration with external services
- **Skills** (prompts/playbooks) for specialized workflows
- **Tools** (Rust-native functions) for performance-critical operations
- **Webhooks** to listen for events from external services (GitHub, Slack, etc.)
- **Lifecycle hooks** (`on_init`, `on_shell_ready`, `on_shutdown`, …) to react to *plugin* lifecycle points
- **Event-catalog hooks** — entries a plugin contributes to the shell's real **33-event agent-lifecycle hook catalog** (`src/hooks.rs`: `PreToolUse`, `TurnEnd`, `MemoryStored`, …). *See the [Enterprise Addendum](#enterprise-addendum-plugin-contributions-to-the-agent-lifecycle-hook-catalog).* **These two "hook" concepts are distinct — do not conflate them.**
- **Config / env injection** — a plugin can merge servers into the client `.mcp.json`, export session env, and point managed subsystems (skill registry) at a URL
- **Login / auth commands** — a plugin can register a top-level command (e.g. `aish login`) and persist a credential its other capabilities reuse
- **Persistent memory** for state, cache, and configuration
- **JSON schemas** for structured data validation
- **Documentation** and metadata

**First plugin:** GitHub integration (`~/.aish/plugins/github/`).

---

## Architecture

### Directory Structure

```
~/.aish/plugins/
├── github/
│   ├── plugin.json                 # Metadata + config schema
│   ├── config.json                 # Active configuration (generated)
│   ├── .mcp.json                   # MCP server definition
│   ├── README.md                   # User-facing docs
│   ├── requirements.txt             # External dependencies
│   ├── skills/
│   │   ├── github-pr-review.md
│   │   ├── github-issue-triage.md
│   │   └── github-workflows.md
│   ├── tools/
│   │   ├── Cargo.toml
│   │   ├── src/lib.rs              # Tool implementations
│   │   └── src/github.rs
│   ├── webhooks/
│   │   ├── pull_request.rs         # Handler for PR events
│   │   └── issues.rs               # Handler for issue events
│   ├── hooks/
│   │   ├── on_init.sh              # Runs at aish startup
│   │   ├── on_shell_ready.sh       # After REPL live
│   │   ├── on_webhook_url_changed.sh # Webhook URL rotated
│   │   └── on_shutdown.sh          # Before aish exits
│   ├── schemas/
│   │   ├── github-pr.json          # JSON Schema for PR object
│   │   ├── github-issue.json
│   │   └── github-workflow.json
│   └── memory/
│       ├── auth.json               # Encrypted/opaque secrets
│       ├── cache.json              # Rate limits, timestamps
│       └── webhooks.json           # Configured webhook state
└── [future plugins...]
```

### Webhook Architecture (Multi-Option)

aish supports **two webhook delivery models** to fit different network constraints:

#### Option A: Self-Hosted Broker (Recommended)

Deploy a lightweight webhook broker on your infrastructure. All aish instances connect to it.

```
GitHub/Slack/etc
    ↓
POST https://webhook-broker.mycompany.com/webhooks/t_abc123/github
    ↓
aish-webhook-broker (your server)
    ├─ Receives webhook
    ├─ Routes to connected aish client (WS or long-poll)
    └─ Queues if client offline
    ↓
aish client (your machine)
    ├─ Persistent WebSocket to broker
    ├─ Receives webhook message
    ├─ Dispatches to plugin handler
    └─ Applies actions (create task, comment, etc)
```

**Deployment:**
- Docker: `docker run -p 8080:8080 aish-webhook-broker:latest`
- Binary: `./aish-webhook-broker --port 8080 --db /var/lib/aish-broker.db`
- Systemd: `systemctl start aish-webhook-broker`

**You control:**
- Server location (your data center, AWS, DigitalOcean, etc)
- Database (SQLite, PostgreSQL, etc)
- TLS certificates
- Access logs
- Backups
- Network policies

#### Option B: Dynamic Forwarding from aish.sh (Optional)

If you don't want to self-host, use aish.sh as a **public ingress point** that dynamically forwards to your aish instance.

```
GitHub/Slack/etc
    ↓
POST https://webhooks.aish.sh/forward/{token}
    ↓
aish.sh (Cloudflare Worker or lightweight Lambda)
    ├─ Validate token
    ├─ Lookup aish instance endpoint from registry
    └─ Forward webhook to aish client
    ↓
aish client
    ├─ Receives forwarded message
    ├─ Dispatches to plugin handler
    └─ Applies actions
```

**Advantages:**
- No self-hosting required
- Automatic HTTPS + DDoS protection (via Cloudflare)
- Works behind any NAT/firewall
- Minimal aish.sh compute (stateless forwarder)

**Trade-off:** Webhooks briefly touch aish.sh (privacy-sensitive organizations may prefer Option A).

---

## Plugin Configuration

### plugin.json Schema

```json
{
  "id": "github",
  "name": "GitHub Integration",
  "version": "1.0.0",
  "description": "GitHub PR/issue management, code review, and workflow automation",
  "author": "aish-core",
  "license": "MIT",
  "min_aish_version": "0.11.0",
  "tags": ["vcs", "collaboration", "code-review", "devops"],
  "provides": {
    "mcp_server": true,
    "skills": ["github-pr-review", "github-issue-triage"],
    "tools": ["create_pr", "list_issues", "add_comment"],
    "webhooks": ["pull_request", "issues"],
    "hooks": ["on_init", "on_shell_ready", "on_webhook_url_changed"],
    "schemas": ["github-pr", "github-issue"],
    "memory": true
  },
  "webhooks": [
    {
      "id": "gh-pr-webhook",
      "handler": "webhooks/pull_request.rs",
      "events": ["pull_request", "pull_request_review"],
      "event_filter": {
        "action": ["opened", "synchronize", "reopened"],
        "pull_request.draft": false
      },
      "retry_policy": {
        "max_retries": 3,
        "backoff_secs": [1, 5, 30]
      }
    },
    {
      "id": "gh-issue-webhook",
      "handler": "webhooks/issues.rs",
      "events": ["issues"],
      "event_filter": {
        "action": ["opened", "reopened"]
      }
    }
  ],
  "config_schema": {
    "type": "object",
    "properties": {
      "token": {
        "type": "string",
        "description": "GitHub API token (env ref: ${env:GITHUB_TOKEN})",
        "sensitive": true
      },
      "owner": {
        "type": "string",
        "description": "Default GitHub org/user"
      },
      "enable_pr_review": {
        "type": "boolean",
        "default": true
      },
      "webhook_mode": {
        "type": "string",
        "enum": ["self_hosted", "aish_sh"],
        "default": "self_hosted",
        "description": "Webhook delivery: self-hosted broker or aish.sh forwarder"
      },
      "rate_limit_buffer": {
        "type": "integer",
        "default": 100
      }
    },
    "required": ["token"]
  },
  "dependencies": {
    "gh": ">=2.0.0",
    "jq": ">=1.6"
  }
}
```

### Global Webhook Configuration

**~/.aish/config/broker.json** (for Option A: self-hosted)

```json
{
  "mode": "self_hosted",
  "broker_url": "https://webhook-broker.mycompany.com",
  "tenant_id": "t_abc123",
  "broker_api_key": "${env:AISH_BROKER_API_KEY}",
  "reconnect_interval_secs": 5,
  "max_queue_size": 1000,
  "websocket_enabled": true,
  "fallback_to_long_poll": true
}
```

**~/.aish/config/broker.json** (for Option B: aish.sh)

```json
{
  "mode": "aish_sh",
  "aish_sh_url": "https://webhooks.aish.sh",
  "forward_token": "${env:AISH_WEBHOOK_TOKEN}",
  "poll_interval_secs": 30,
  "max_queue_size": 1000
}
```

### Plugin Memory Schema

```rust
pub struct PluginMemory {
  pub plugin_id: String,
  pub namespace: String,  // "auth", "cache", "webhooks", "prefs"
  pub data: serde_json::Value,
  pub created_at: SystemTime,
  pub updated_at: SystemTime,
  pub ttl: Option<Duration>,  // Auto-expire cache entries
}

// Storage: ~/.aish/memory/plugins/{plugin_id}/{namespace}.json (0600)
```

Example: **~/.aish/memory/plugins/github/webhooks.json**

```json
{
  "configured_webhooks": [
    {
      "repo": "LightHeart-Ventures/atum_ai_app",
      "hook_id": 12345678,
      "webhook_url": "https://webhook-broker.mycompany.com/webhooks/t_abc123/github",
      "webhook_mode": "self_hosted",
      "events": ["pull_request", "issues"],
      "configured_at": "2026-01-15T09:30:00Z",
      "last_verified_at": "2026-01-15T09:35:00Z"
    }
  ],
  "webhook_deliveries": [
    {
      "event_id": "12345678",
      "repo": "LightHeart-Ventures/atum_ai_app",
      "event_type": "pull_request",
      "action": "opened",
      "pr_number": 842,
      "received_at": "2026-01-15T10:00:00Z",
      "processed_at": "2026-01-15T10:00:01Z",
      "actions_taken": ["task_created"]
    }
  ]
}
```

---

## Webhook Handler Implementation

### Handler Registry

```rust
pub struct WebhookHandler {
  pub plugin_id: String,
  pub webhook_id: String,
  pub events: Vec<String>,              // e.g., ["pull_request"]
  pub event_filter: Option<serde_json::Value>,  // jmespath or AND logic
  pub handler_fn: Box<dyn Fn(WebhookMessage) -> Result<WebhookAction>>,
}

pub struct WebhookMessage {
  pub webhook_id: String,            // "gh-pr-webhook"
  pub event_type: String,            // "pull_request"
  pub payload: serde_json::Value,    // Full webhook payload
  pub received_at: SystemTime,
  pub signature: String,             // HMAC-SHA256 verification
}

pub enum WebhookAction {
  CreateTask {
    title: String,
    description: String,
    projectId: String,
    labels: Vec<String>,
  },
  Comment {
    cardId: String,
    message: String,
  },
  UpdateMemory {
    namespace: String,
    key: String,
    value: serde_json::Value,
  },
  TriggerWorkflow {
    workflowId: String,
    inputPayload: serde_json::Value,
  },
  InvokeAgent {
    agentId: String,
    task: String,
  },
  Log {
    message: String,
    level: String,  // "info", "warn", "error"
  },
}

pub struct WebhookDispatcher {
  pub handlers: HashMap<String, Vec<WebhookHandler>>,  // by event type
}
```

### Example Handler: GitHub PR Webhook

**~/.aish/plugins/github/webhooks/pull_request.rs**

```rust
use aish_webhook::{WebhookMessage, WebhookAction};

pub async fn handle_pr_webhook(msg: WebhookMessage) -> Result<WebhookAction> {
  let pr = msg.payload["pull_request"].clone();
  let action = msg.payload["action"].as_str().unwrap_or("");
  
  match action {
    "opened" => {
      // PR just opened → create task in aish
      let title = format!("Review PR #{}: {}", 
        pr["number"], pr["title"]);
      
      Ok(WebhookAction::CreateTask {
        title,
        description: format!(
          "GitHub PR: {}\nAuthor: {}\nBranch: {}\n\n{}",
          pr["html_url"],
          pr["user"]["login"],
          pr["head"]["ref"],
          pr["body"].as_str().unwrap_or("")
        ),
        projectId: "b_...".to_string(),  // From plugin config
        labels: vec!["github-pr".to_string()],
      })
    },
    "synchronize" => {
      // PR updated with new commits → add comment
      Ok(WebhookAction::Comment {
        cardId: lookup_card_by_pr_number(pr["number"].as_i64()?).await?,
        message: format!(
          "🔄 PR updated with new commit(s)"
        ),
      })
    },
    _ => {
      // Other actions → log for audit trail
      Ok(WebhookAction::UpdateMemory {
        namespace: "webhooks".to_string(),
        key: "last_event".to_string(),
        value: serde_json::json!({
          "action": action,
          "pr_number": pr["number"],
          "received_at": msg.received_at
        }),
      })
    },
  }
}
```

---

## Dynamic Webhook URL Configuration

When aish starts, it must configure external services (GitHub, Slack, etc.) with the correct webhook URL. Since the URL may change (broker restarts, IP changes, migration to aish.sh), plugins use hooks to auto-configure.

### Plugin Initialization Flow

```
[aish startup]
  ├─ Load plugin system
  ├─ Connect to broker (self-hosted OR aish.sh)
  ├─ Obtain webhook_url from broker
  │  └─ Self-hosted: https://webhook-broker.mycompany.com/webhooks/t_abc123/github
  │  └─ aish.sh: https://webhooks.aish.sh/forward/{token}
  ├─ Run on_init hooks (in parallel)
  │  └─ GitHub plugin's on_init.sh:
  │     ├─ Fetch webhook_url via aish webhook-url --plugin github
  │     ├─ Fetch webhook_secret via aish webhook-secret --plugin github
  │     ├─ Query GitHub for existing hooks
  │     ├─ Create or update hook with new URL + secret
  │     └─ Write to plugin memory: configured_webhooks
  ├─ Open connection to broker
  │  └─ Self-hosted: Persistent WS to wss://webhook-broker.mycompany.com/webhooks/stream
  │  └─ aish.sh: Start polling https://webhooks.aish.sh/messages?token=...
  └─ Wait for webhooks
```

### on_init.sh: GitHub Example

**~/.aish/plugins/github/hooks/on_init.sh**

```bash
#!/bin/bash
set -e

# Get configuration
GH_TOKEN="${GITHUB_TOKEN:?No GITHUB_TOKEN set}"
REPO="${GITHUB_REPO:-LightHeart-Ventures/atum_ai_app}"

# Get the webhook URL from aish (works for both self-hosted and aish.sh)
WEBHOOK_URL=$(aish webhook-url --plugin github)
WEBHOOK_SECRET=$(aish webhook-secret --plugin github)

echo "Configuring GitHub webhooks..."
echo "  Repo: $REPO"
echo "  Webhook URL: $WEBHOOK_URL"

# Query existing webhooks
EXISTING=$(gh api repos/$REPO/hooks \
  --field per_page=100 \
  -q '.[] | select(.url | contains("webhook-broker") or contains("webhooks.aish.sh"))' \
  2>/dev/null || echo "")

if [ -n "$EXISTING" ]; then
  HOOK_ID=$(echo "$EXISTING" | jq -r '.id' | head -n1)
  echo "Updating existing webhook (ID: $HOOK_ID)..."
  
  gh api repos/$REPO/hooks/$HOOK_ID \
    -X PATCH \
    -f config[url]="$WEBHOOK_URL" \
    -f config[secret]="$WEBHOOK_SECRET" \
    -f config[content_type]="json" \
    -f 'config[insecure_ssl]'="0" \
    -f events[]="pull_request" \
    -f events[]="pull_request_review" \
    -f events[]="issues" \
    -f events[]="issue_comment" \
    -F active=true
else
  echo "Creating new webhook..."
  
  gh api repos/$REPO/hooks \
    -f name="web" \
    -f config[url]="$WEBHOOK_URL" \
    -f config[secret]="$WEBHOOK_SECRET" \
    -f config[content_type]="json" \
    -f 'config[insecure_ssl]'="0" \
    -f events[]="pull_request" \
    -f events[]="pull_request_review" \
    -f events[]="issues" \
    -f events[]="issue_comment" \
    -F active=true
fi

echo "✅ GitHub webhook configured"
```

### on_webhook_url_changed.sh: Handle Rotation

When the webhook URL changes (broker migration, aish.sh update, etc), this hook re-configures external services.

**~/.aish/plugins/github/hooks/on_webhook_url_changed.sh**

```bash
#!/bin/bash
set -e

NEW_WEBHOOK_URL="$1"
NEW_WEBHOOK_SECRET="$2"

echo "Webhook URL changed: re-configuring GitHub..."

# Re-run the standard on_init logic
exec "$(dirname "$0")/on_init.sh"
```

### Hook Lifecycle

Hooks are shell scripts that run at specific points:

1. **on_init** — Runs once at shell startup
   - Validate configuration
   - Check authentication
   - Warm caches
   - Configure webhooks on external services
   - Return non-zero to abort shell startup

2. **on_shell_ready** — Runs after REPL is live
   - Print banner/status
   - Schedule background tasks
   - Auto-sync data

3. **on_webhook_url_changed** — Runs when webhook URL rotates
   - Re-configure external services with new URL
   - Update cached state

4. **on_shutdown** — Runs before shell exits
   - Cleanup resources
   - Flush cache
   - (Don't deactivate webhooks; keep them live for next startup)

**Hook contract:**
- Exit 0 = success
- Exit non-zero = error (logs, but doesn't block plugin)
- Stdout goes to user
- Stderr goes to logs
- Timeout: 30s (configurable)

---

## Supporting CLI Commands

```rust
pub fn webhook_url(plugin_id: &str) -> Result<String> {
  // Load broker config and construct URL
  let broker = BrokerConfig::load()?;
  match broker.mode {
    BrokerMode::SelfHosted => {
      Ok(format!(
        "{}/webhooks/{}/{}",
        broker.broker_url, broker.tenant_id, plugin_id
      ))
    },
    BrokerMode::AishSh => {
      Ok(format!(
        "{}/forward/{}",
        broker.aish_sh_url, broker.forward_token
      ))
    },
  }
}

pub fn webhook_secret(plugin_id: &str) -> Result<String> {
  // Return current secret (rotated on each startup)
  let broker = BrokerConfig::load()?;
  Ok(broker.webhook_secret)
}

pub fn webhook_status() -> Result<WebhookStatus> {
  let broker = BrokerConnection::current()?;
  Ok(WebhookStatus {
    connected: broker.is_connected(),
    mode: broker.mode(),
    pending_webhooks: broker.pending_count(),
    last_heartbeat: broker.last_heartbeat(),
  })
}
```

### REPL Commands

```
:plugin list                          # List all plugins
:plugin info github                   # Show metadata + config
:plugin info github --schema          # List available schemas
:plugin config github                 # Show active config
:plugin config github --set key val   # Update config
:plugin enable github                 # Enable plugin
:plugin disable github                # Disable plugin
:plugin reload github                 # Reload (run hooks)
:plugin memory github webhooks        # Inspect webhook state
:webhook status                       # Show broker connection status
:webhook test github                  # Send test webhook payload
```

---

## Implementation Phases

### Phase 1: Core Infrastructure (Foundation)
**Scope:** Plugin discovery, loading, lifecycle. **Estimate:** 8 SP

- [ ] 1.1 Create `~/.aish/plugins/` directory structure and docs
- [ ] 1.2 Implement `Plugin` struct and `PluginMetadata` parsing
- [ ] 1.3 Implement `discover_plugins()` — scan and validate plugin.json
- [ ] 1.4 Implement config loading + `${env:VAR}` substitution
- [ ] 1.5 Implement hook system (on_init, on_shell_ready, on_shutdown)
- [ ] 1.6 Integrate plugin init into shell startup sequence
- [ ] 1.7 Add `:plugin list` / `:plugin info` REPL commands
- [ ] 1.8 Tests: discovery, config validation, hook execution, error cases

### Phase 2: Plugin Memory & State Management
**Scope:** Persistent plugin data. **Estimate:** 5 SP

- [ ] 2.1 Design plugin memory schema (file-based)
- [ ] 2.2 Implement `plugin_memory_get/set/append/delete/clear`
- [ ] 2.3 Namespace separation (auth, cache, webhooks, prefs)
- [ ] 2.4 File perms enforcement (0600 for auth namespace)
- [ ] 2.5 Add `:plugin memory` REPL commands
- [ ] 2.6 Tests: read/write, namespace isolation, TTL expiry

### Phase 3: Schemas & Structured Data Validation
**Scope:** JSON Schema validation for tool returns. **Estimate:** 5 SP

- [ ] 3.1 Load schemas from `plugins/{id}/schemas/*.json`
- [ ] 3.2 Implement JSON Schema validator
- [ ] 3.3 Attach schemas to SkillManifest / ToolManifest
- [ ] 3.4 Validate tool returns against schema at runtime
- [ ] 3.5 Add `:plugin info --schema` to list schemas
- [ ] 3.6 Tests: validation, schema discovery, error reporting

### Phase 4: Self-Hosted Webhook Broker
**Scope:** Broker service + aish client integration. **Estimate:** 13 SP

- [ ] 4.1 Create aish-webhook-broker project (separate binary)
- [ ] 4.2 Implement broker REST endpoints (register, receive webhooks)
- [ ] 4.3 Implement WebSocket server for persistent client connections
- [ ] 4.4 Implement long-poll fallback for restrictive networks
- [ ] 4.5 Broker database schema (registrations, messages, deliveries)
- [ ] 4.6 Implement message queue (persist if client offline)
- [ ] 4.7 Broker Docker image + systemd service
- [ ] 4.8 aish client: broker connection manager (WS + long-poll)
- [ ] 4.9 aish client: message loop + dispatcher
- [ ] 4.10 HMAC-SHA256 signature verification
- [ ] 4.11 Tests: broker endpoints, message routing, reconnection

### Phase 5: Webhook Handler Registration & Dispatch
**Scope:** Plugin webhook handlers. **Estimate:** 5 SP

- [ ] 5.1 Load webhook handlers from plugin.json
- [ ] 5.2 Implement WebhookDispatcher (match event type → handlers)
- [ ] 5.3 Apply event filters (jmespath or AND logic)
- [ ] 5.4 Execute handler functions (with error isolation)
- [ ] 5.5 Log handler execution + errors to plugin memory
- [ ] 5.6 Tests: handler dispatch, filtering, error handling

### Phase 6: GitHub Plugin (First Real Plugin)
**Scope:** Fully-functional GitHub integration. **Estimate:** 13 SP

- [ ] 6.1 Create `~/.aish/plugins/github/` scaffold
- [ ] 6.2 Write `plugin.json` with webhook config
- [ ] 6.3 Implement `on_init.sh` hook (configure GitHub webhooks)
- [ ] 6.4 Implement `on_webhook_url_changed.sh` hook
- [ ] 6.5 Implement `on_shell_ready.sh` hook
- [ ] 6.6 Create GitHub PR schema (`schemas/github-pr.json`)
- [ ] 6.7 Create GitHub issue schema (`schemas/github-issue.json`)
- [ ] 6.8 Implement webhook handlers (PR, issue, review)
- [ ] 6.9 Implement GitHub skills (pr-review, issue-triage)
- [ ] 6.10 Implement GitHub tools (create_pr, list_issues, etc)
- [ ] 6.11 Set up MCP server config (`.mcp.json`)
- [ ] 6.12 Test end-to-end (plugin load, webhooks, skills, tools)
- [ ] 6.13 Tests: hook execution, handler dispatch, actions

### Phase 7: aish.sh Dynamic Forwarding (Optional)
**Scope:** Public ingress point for webhook forwarding. **Estimate:** 8 SP

- [ ] 7.1 Deploy aish.sh as Cloudflare Worker (or lightweight Lambda)
- [ ] 7.2 Implement /forward/{token} endpoint (lookup + forward)
- [ ] 7.3 Token registry (maps token → aish instance endpoint)
- [ ] 7.4 aish client: polling mode (fetch messages from aish.sh)
- [ ] 7.5 aish client: mode selection (self-hosted vs aish.sh)
- [ ] 7.6 Documentation: how to opt into aish.sh forwarding
- [ ] 7.7 Tests: token validation, forwarding, polling
- [ ] 7.8 Monitoring: webhook delivery latency, error rates

### Phase 8: Plugin Configuration & Management
**Scope:** User-friendly config interface. **Estimate:** 5 SP

- [ ] 8.1 Implement `~/.aish/config/broker.json` loading
- [ ] 8.2 Implement `~/.aish/config/plugins/{id}.json` loading
- [ ] 8.3 Add `:plugin config` / `:plugin config --set` commands
- [ ] 8.4 Config schema validation on set
- [ ] 8.5 Support `${env:VAR}` references
- [ ] 8.6 Tests: config loading, validation, env substitution

### Phase 9: Plugin Enable/Disable & Reload
**Scope:** Runtime plugin management. **Estimate:** 3 SP

- [ ] 9.1 Implement `enabled` flag in PluginRegistry
- [ ] 9.2 Add `:plugin enable / :plugin disable` commands
- [ ] 9.3 Add `:plugin reload` (re-run hooks, reload config)
- [ ] 9.4 Tests: enable/disable state persistence, reload safety

### Phase 10: Webhook Testing & Debugging
**Scope:** Developer tools for webhook testing. **Estimate:** 5 SP

- [ ] 10.1 Implement `:webhook test` command (send test payload)
- [ ] 10.2 Implement `:webhook logs` (view delivery history)
- [ ] 10.3 Implement `:webhook replay` (re-send a previous event)
- [ ] 10.4 Add webhook delivery metadata to plugin memory
- [ ] 10.5 Tests: test payload validation, log retrieval

### Phase 11: Error Handling & Robustness
**Scope:** Graceful degradation, audit trail. **Estimate:** 5 SP

- [ ] 11.1 Handle missing plugin.json (skip with warning)
- [ ] 11.2 Handle hook timeout (warn, don't block shell)
- [ ] 11.3 Handle config validation failure (disable plugin)
- [ ] 11.4 Handle memory I/O errors (best-effort logging)
- [ ] 11.5 Handle broker connection failures (auto-reconnect, backoff)
- [ ] 11.6 Add plugin error reporting to `:status`
- [ ] 11.7 Tests: error modes, recovery, audit trail

### Phase 12: Documentation & Examples
**Scope:** Guides and templates. **Estimate:** 5 SP

- [ ] 12.1 Write plugin developer guide
- [ ] 12.2 Write webhook handler guide
- [ ] 12.3 Create plugin scaffold generator (`aish plugin create`)
- [ ] 12.4 Write GitHub plugin README
- [ ] 12.5 Write broker deployment guide (self-hosted + aish.sh)
- [ ] 12.6 Tests: scaffold generator, docs completeness

---

## Webhook Delivery Models Comparison

| Aspect | Self-Hosted Broker | aish.sh Forwarding |
|--------|--------------------|--------------------|
| **Setup** | Run broker on your server | Opt-in, zero setup |
| **Privacy** | Webhooks stay in your infra | Brief touch on aish.sh |
| **Control** | Full (you own it) | Limited (aish.sh controlled) |
| **Cost** | Server compute (minimal) | Free tier; pay if > usage |
| **Compliance** | On-prem eligible | Cloud-based |
| **Network** | Requires outbound WS to broker | HTTP polling, very tolerant |
| **Latency** | ~100ms (same network) | ~500ms (cross-cloud) |
| **Multi-tenant** | One broker serves your org | One aish.sh serves everyone |

**Recommendation:** Start with **self-hosted** for production use (privacy, control). Offer **aish.sh** as an optional convenience for users who prefer zero-ops.

---

## Testing Strategy

### Unit Tests (per phase)

- **Discovery:** Valid plugin.json, missing fields, version mismatch
- **Config:** Env substitution, validation, required fields
- **Hooks:** Execution, timeout, error handling
- **Memory:** Read/write, namespaces, TTL, file perms
- **Broker:** Register, message routing, reconnection
- **Webhooks:** Handler dispatch, filtering, signature verification
- **GitHub plugin:** Skills callable, tools return valid schema, hooks work

### Integration Tests

- **Full lifecycle:** Discover → init → connect broker → ready → webhook → action
- **Multi-plugin:** Load multiple plugins without conflicts
- **Error recovery:** Plugin fails init; shell still starts; other plugins load
- **Webhook delivery:** Send test payload → handler → action → audit
- **Broker reconnection:** Broker restarts; clients reconnect; queue drains
- **URL rotation:** Webhook URL changes; on_webhook_url_changed fires

### Manual Testing (First Plugin)

- [ ] Deploy broker to `webhook-broker.mycompany.com`
- [ ] Configure aish: `aish config set broker.broker_url "https://webhook-broker.mycompany.com"`
- [ ] `:plugin list` shows github as enabled
- [ ] `:plugin info github` shows metadata + config
- [ ] `:plugin reload github` (on_init hook runs, configures GitHub webhooks)
- [ ] Open PR on GitHub
- [ ] Broker receives webhook → routes to aish
- [ ] aish handler creates task in Atum
- [ ] `:plugin memory github webhooks` shows delivery in audit log
- [ ] Webhook URL changes (test by rotating secret on aish startup)
- [ ] `:plugin reload github` re-configures GitHub with new URL
- [ ] `:webhook status` shows broker connected + message count

---

## Dependencies

- **jsonschema** crate — schema validation
- **serde_json** — config/memory serialization
- **tokio-tungstenite** — WebSocket (broker + client)
- **tempfile** — atomic writes (temp + rename)
- **reqwest** — HTTP client (aish.sh polling)
- **sqlx** or **rusqlite** — broker database (self-hosted)
- **hmac + sha2** — webhook signature verification

---

## Success Criteria

1. ✅ **Plugin discovery** — discover all valid plugins in `~/.aish/plugins/*`
2. ✅ **Config management** — load, validate, substitute env vars
3. ✅ **Lifecycle hooks** — on_init, on_shell_ready, on_webhook_url_changed, on_shutdown work reliably
4. ✅ **Plugin memory** — persist and retrieve plugin state atomically
5. ✅ **Webhook delivery** — self-hosted broker OR aish.sh forwarding works end-to-end
6. ✅ **Webhook handlers** — plugin handlers dispatch, execute, log correctly
7. ✅ **GitHub plugin ships** — fully functional PR/issue webhooks + integration
8. ✅ **Schemas** — validate tool returns, discoverable via REPL
9. ✅ **Error resilience** — plugin errors don't crash shell; audit logged
10. ✅ **REPL UX** — `:plugin` and `:webhook` commands are ergonomic and clear

---

## Open Questions

1. **Default webhook mode:** Start with self-hosted, add aish.sh as optional?
   - *Recommendation:* Yes. Self-hosted is the secure default; aish.sh is opt-in for convenience.

2. **Broker scaling:** What if multiple aish instances → one broker?
   - *Recommendation:* Broker is tenant-aware (tenant_id in config); one broker serves entire org.

3. **Webhook retries:** If aish is offline, does broker queue messages?
   - *Recommendation:* Yes, up to `max_queue_size` (default 1000). Older messages are dropped when queue full.

4. **Plugin conflicts:** What if two plugins define a webhook with the same event?
   - *Recommendation:* Both handlers execute; errors in one don't block others. Log all results.

5. **Webhook signature rotation:** Should secrets rotate periodically?
   - *Recommendation:* Rotate on every aish startup (new secret in broker.json). External services re-configure via on_init hook.

---

## Implementation status

**Shipped — skill-registry expansion (the first, smallest slice):**

aish discovers plugins under `~/.aish/plugins/<plugin-id>/` and merges each
enabled plugin's **skills** into the same catalog the agent sees for
`~/.aish/skills`. A plugin is any directory containing a readable, parseable
`plugin.json`; its skills use the standard installed-skill layout
`skills/<skill-name>/SKILL.md` (subdir + `SKILL.md`), so they load through the
exact same parser as `~/.aish/skills` — this is a deliberate, small divergence
from the flat `skills/<name>.md` sketch above, chosen for parser reuse and
consistency with the existing skill convention.

- Code: `src/plugins.rs` (discovery + `plugin_skills`), `skills::load_catalog`
  (merge; installed skills win on a name collision).
- Wired into startup (`main.rs`), the deferred interactive MCP handshake
  (`repl.rs`), and mid-session `:skill` reloads (`session.rs`).
- Disabled (`"enabled": false`), malformed, or manifest-less directories are
  skipped silently — a broken plugin never blocks startup.
- Runnable example: [`examples/plugins/hello-world/`](../examples/plugins/hello-world/)
  — a plugin whose only job is to contribute one `hello-world` greeting skill,
  proving plugin discovery + skill expansion end to end.

Everything else in this document (MCP servers, tools, webhooks, hooks, memory,
schemas) remains forward-looking. Unknown `plugin.json` keys are ignored today,
so a manifest can grow into the richer schema without breaking existing plugins.

---

## Timeline

- **Phases 1–3 (Core):** Sprint S10 (weeks 1–2) — 18 SP
- **Phase 4 (Broker):** Sprint S10 (weeks 2–3) — 13 SP
- **Phase 5 (Handlers):** Sprint S11 (week 1) — 5 SP
- **Phase 6 (GitHub):** Sprint S11 (weeks 1–2) — 13 SP
- **Phase 7 (aish.sh):** Sprint S11 (week 2) — 8 SP (optional, deferrable)
- **Phases 8–12 (Polish):** Sprint S12 (weeks 1–2) — 23 SP

---

## Enterprise Addendum: Plugin Contributions to the Agent-Lifecycle Hook Catalog

> **Status:** draft-evolving. Added while designing the first commercial plugin,
> `aish_enterprise` (the aish.sh control-plane integration). This addendum closes the
> single biggest gap between this design and a shippable enterprise plugin: **plugins
> cannot currently contribute to the shell's real event catalog**, which is precisely
> the seam every control-plane feature (managed memory, LLM tracing, governance,
> fleet observability) attaches to.

### The two meanings of "hook" — reconciled

This document (and the initial GitHub plugin) uses **hook** to mean a *plugin
lifecycle* shell script. But the shipped shell already has a **second, unrelated**
hook system: the **33-event agent-lifecycle catalog** in `src/hooks.rs`, merged from
`~/.aish/hooks.json` (user) and `.aish/hooks.json` (project). They are different
mechanisms and must not be conflated:

| | **Lifecycle hook** | **Event-catalog hook** |
|---|---|---|
| What fires it | plugin load/unload | the agent loop (per turn / tool / memory / session) |
| Events | `on_init`, `on_shell_ready`, `on_webhook_url_changed`, `on_shutdown` | `PreToolUse`, `PostToolUse`, `TurnEnd`, `MemoryStored`, `PreCompact`, `SessionStart`, `WorkerStart`, … (33 total) |
| Declared in | `plugin.json → provides.lifecycle_hooks` (was `provides.hooks`, now a deprecated alias) | `~/.aish/hooks.json` catalog (`src/hooks.rs`) |
| Dispatch | plugin loader, at lifecycle points | `src/hooks.rs` dispatcher, fork/exec, JSON on stdin |
| Can block a turn? | no | **yes** — `PreToolUse` returns `Decision::Deny(reason)` |

**The enterprise plugin needs the second kind.** Its entire job is to observe/govern
the agent loop, which only the `src/hooks.rs` catalog exposes.

### New capability: `event_hooks_file` (required for enterprise)

A plugin may ship a `hooks.json` fragment whose entries are **merged into the client's
`src/hooks.rs` catalog** at load time.

```jsonc
// plugin.json
"provides": {
  "lifecycle_hooks": ["on_init", "on_shell_ready", "on_shutdown"],  // renamed from "hooks"
  "event_hooks": true
},
"event_hooks_file": "hooks.json"   // merged into the 33-event catalog
```

**Merge & precedence rules:**
- Precedence is **user (`~/.aish/hooks.json`) > project (`.aish/hooks.json`) >
  plugin**. Plugin entries are lowest, so a user/org can always override or disable a
  plugin-contributed hook by name.
- Multiple plugins may register on the same event; all fire (observe entries in
  parallel, error-isolated). This mirrors Open Question #4's resolution.
- **Only one blocking entry per event is honored** for `PreToolUse`-class events; if
  several are present, the highest-precedence wins and the rest degrade to observe.
- Plugin entries carry an implicit `source: "plugin:<id>"` tag for audit and for
  `:hooks list` provenance.
- The existing trust model is unchanged and **must** hold for plugin entries too:
  fork/exec (no shell), **no credential values in payloads** (export names only), the
  `AISH_IN_HOOK` recursion guard, and per-event timeouts.

### New capability: `provides.config` (config / env injection) (required)

A plugin may inject three kinds of client configuration at load:

1. **MCP servers** — a plugin `.mcp.json` is merged into the client MCP set
   (`src/mcp.rs`), same schema as the user's own `.mcp.json`. This is how the
   enterprise gateway is added with zero manual edits.
2. **Session env exports** — a lifecycle hook may emit `KEY=VALUE` lines on stdout
   that the loader adds to the session environment. Used to point the OSS skill
   provider at an org registry (`AISH_SKILL_REGISTRY`) and to pass the gateway URL /
   tenant into the injected `.mcp.json` and event-hook forwarder.
3. **Staged managed config** — a plugin may write a `managed.json` that the client
   merges (e.g. an org-pushed `hooks.json`/policy bundle) at `SessionStart`.

Declared via `provides.config: ["mcp_gateway", "skill_registry", "managed_hooks"]`.
All injected env still resolves `${env:…}` / credential-profile refs through the
existing substitution path — **secret values never enter plugin.json or payloads.**

### New capability: `provides.login` (auth command) (required)

A plugin may register a top-level command (e.g. `aish login`) and persist a
credential (to `~/.aish/credentials` under a plugin profile) that its MCP server,
event-hook forwarder, and lifecycle hooks reuse. Device-code / browser flows are the
plugin's concern; the client only needs to (a) route the command to the plugin and
(b) expose the credential to the plugin's own capabilities via the profile ref
mechanism already used by `.mcp.json`.

#### 0.5.5 implementation notes (login routing + credential persistence)

**Command routing.** The REPL command dispatcher recognizes a bare
`login <plugin-id>` line (both as `aish login <id>` at launch and `login <id>` at the
prompt). It scans the discovered plugin manifests for one whose
`provides.login == <plugin-id>` (see `PluginManifest::login_command()` in
`src/plugins.rs`) and invokes that plugin's auth handler. Unknown ids fail loudly with
`no plugin provides \`login <id>\`` and write nothing.

**Auth handler contract.** The handler is `~/.aish/plugins/<plugin-id>/login.sh`
(falls back to `login` if no `.sh`). It is spawned with:

- `AISH_PLUGIN_ID`, `AISH_LOGIN_NAME` — the plugin id / profile name
- `AISH_TENANT_ID` — the current tenant id when known (else empty)
- `AISH_CREDENTIALS_FILE` — absolute path aish will persist the profile to

stdin/stderr are inherited, so the handler can print a device-code URL to stderr and
read interactive input while keeping stdout clean for the JSON result.

On success it prints a **flat JSON object** of credential fields to stdout and exits 0:

```json
{"access_token":"…","refresh_token":"…","expires_at":"2025-01-01T00:00:00Z"}
```

`string` values are stored verbatim; `number`/`bool` are stringified; `null` is
dropped; nested objects/arrays are rejected (`malformed handler output`). A non-zero
exit (or non-JSON stdout) aborts the login and persists nothing — stderr is surfaced
to the user.

**Credential persistence.** Fields are written to `~/.aish/credentials` under an INI
section `[profile:<plugin-id>]` (the same format `.mcp.json` `${profile:…}` refs read
via `crate::mcp::load_profile`). Existing sections are preserved; only the target
profile is rewritten. The file is created / re-chmod'd to **0600** (user-only) on every
write.

**Credential-ref resolution.** Two consumers reuse the stored profile:

- **`.mcp.json`** — `${profile:<plugin-id>}` in an MCP server's `url`/headers resolves
  through the existing profile loader (unchanged from 0.5.3).
- **Lifecycle hooks** — `profile_env(<plugin-id>)` flattens the profile into
  `AISH_PROFILE_<PLUGIN>_<FIELD>=value` env pairs (name + field folded to
  `[A-Z0-9]`→`_`, upper-cased), e.g. `AISH_PROFILE_MYCOMPANY_ACCESS_TOKEN`. A hook's
  `on_init.sh` reads these directly. (Wiring into the live hook runner lands with
  0.5.4; the resolver + round-trip are covered by tests now.)

**Example.** `examples/plugins/hello-world/login.sh` is a minimal device-code-style
handler that emits a fake token object, demonstrating the stdout JSON contract.

### What is explicitly NOT required for the enterprise plugin

The **webhook broker / dynamic-forwarding apparatus (Phases 4, 5, 7, 10)** is *not* a
prerequisite. `aish_enterprise` consumes the **existing agent-lifecycle event
catalog**, not a new outbound-webhook subsystem. Keep those phases on their own
track; they are orthogonal to shipping the control-plane plugin.

### Phase 0.5: Minimal Viable Plugin Capabilities (the enterprise unlock)

**Scope:** the three generic capabilities above — useful to *every* plugin author, not
just enterprise. **Estimate:** ~8 SP. Slots **before** Phase 4.

- [x] 0.5.1 Rename `provides.hooks` → `provides.lifecycle_hooks` (keep `hooks` as a
      deprecated alias for one release) to free the word "hooks" for the event catalog.
      *Done: `Provides` struct in `src/plugins.rs` parses `lifecycle_hooks` (canonical)
      and the deprecated `hooks` alias; `PluginManifest::lifecycle_hooks()` resolves the
      effective list (canonical wins) and `discover` emits a one-time deprecation warning
      when only the old key is present.*
- [ ] 0.5.2 Implement `event_hooks_file` merge into `src/hooks.rs` (precedence,
      multi-plugin fan-out, single-blocking-winner, `source` tagging).
- [x] 0.5.3 Implement `.mcp.json` merge from plugins into the client MCP set.
      *Done — see "0.5.3 detail" below. `plugins::plugin_mcp_paths` feeds each
      `<plugin>/.mcp.json` into `mcp::McpHost::start` after project + user scope;
      `collect_plugin_mcp_servers` models the same first-one-wins policy for
      diagnostics; `mcp::interpolate` gained explicit `${env:VAR}` / `${profile:KEY}`
      forms resolved at connect time on both stdio and HTTP transports.*
- [x] 0.5.4 Implement session-env injection from lifecycle-hook stdout (`KEY=VALUE`).
      *Done: `plugins::collect_lifecycle_env` fork/execs each enabled plugin's
      `hooks/on_init.sh` (NO shell, `AISH_IN_HOOK=1`, bounded timeout), parses the
      `KEY=VALUE` stdout via `parse_hook_env`, rejects credential-like keys, and
      injects survivors into the session env in `main.rs` (ambient/user env wins;
      alphabetically-first plugin wins on a clash; `AISH_ENV_INJECTION_DISABLED=1`
      disables). Covered by `plugins::tests` parse/run/collect cases.*
- [x] 0.5.5 Implement `provides.login` command registration + credential-profile
      persistence. *Done: `src/plugin_auth.rs` routes `login <plugin-id>`, invokes the
      plugin's auth handler, and persists its JSON output to `~/.aish/credentials` under
      `[profile:<plugin-id>]` at mode 0600; the credential is reusable via
      `${profile:<plugin-id>}` in `.mcp.json` and exported to lifecycle hooks as
      `AISH_PROFILE_<PLUGIN>_<FIELD>`. See "0.5.5 implementation notes" below.*
- [ ] 0.5.6 `:hooks list` shows plugin-contributed entries with provenance; `:plugin
      info <id>` shows which catalog events it registers.
- [ ] 0.5.7 Tests: catalog merge + precedence, blocking-veto from a plugin entry,
      `.mcp.json` merge, env injection, login round-trip, override/disable a plugin hook.

**Why 0.5 is the true unlock:** with 0.5.2–0.5.5 in place, the entire commercial
client footprint collapses to *one plugin install + `aish login`* — the enterprise
plugin (`aish_enterprise`) ships without any enterprise-specific code upstream. The
control-plane's "first 5 to build" (trace capture, org skill registry, usage caps,
tiered memory, `aish doctor`) all attach to catalog events the plugin now contributes.

### 0.5.3 detail: plugin `.mcp.json` merge

**What a plugin ships.** A plugin may place a `.mcp.json` at its root using the
**same schema** as the user's `~/.aish/.mcp.json`:

```jsonc
{
  "mcpServers": {
    "hello-world-demo": {
      "command": "some-mcp-binary",
      "args": ["--stdio"],
      "env": { "TOKEN": "${env:HELLO_WORLD_TOKEN}", "KEY": "${profile:hello-world}" }
    }
  }
}
```

Both stdio (`command`/`args`/`env`) and HTTP (`url`/`headers` + optional
`credentials: { file, profile }`) server shapes are accepted — identical to the
user config, so there is nothing new for authors to learn.

**Load path.** At startup (`main.rs`, before the REPL) the MCP path list is
assembled as:

```
[ ./.mcp.json (project),  ~/.aish/.mcp.json (user),  <plugin>/.mcp.json … (id-sorted) ]
```

`plugins::plugin_mcp_paths(&plugins_dir)` appends every existing
`<plugin>/.mcp.json` in **plugin-id (alphabetical) order** after the two config
scopes, then the whole list is handed to `mcp::McpHost::start`.

**Collision policy — first-one-wins.** `McpHost::start` connects paths in order
and **skips any server name already connected**, so the earliest path to claim a
name keeps it. Effective precedence:

```
project config  >  user config  >  plugin (alphabetically-first id)  >  later plugins
```

Rationale: a plugin can *offer* a server but never silently *override* an
operator's explicitly-configured one, and the policy is deterministic across
runs (id-sorted) rather than filesystem-order-dependent. `collect_plugin_mcp_servers`
returns the same `(servers, collisions)` decision without connecting, so `:plugin
info` / diagnostics can show exactly what merged and what lost (and to whom).

**Malformed / absent files** never abort startup: a missing file is skipped, and
a syntactically-broken `.mcp.json` earns a warning from `McpHost` and is skipped
(mirrors the forgiving `discover` contract). `read_plugin_mcp` returns `None`.

**Credential-ref resolution (never on disk).** Secret references are resolved by
`mcp::interpolate` **at connect time**, from the process environment or the
server's referenced credentials profile — the config file only ever holds the
reference, never the secret. Three forms are supported on both transports:

| Form              | Resolves from                                              |
|-------------------|------------------------------------------------------------|
| `${env:VAR}`      | process environment variable `VAR`                         |
| `${profile:KEY}`  | key `KEY` in the server's `credentials` profile (INI)      |
| `${NAME}`         | legacy: profile first, then process env (back-compat)      |

An unresolvable ref is left **verbatim** so the failure surfaces loudly at the
server rather than silently connecting unauthenticated; a `credentials` block
pointing at a missing profile hard-errors before connect. As of 0.5.3 stdio
`args` and `env` values are interpolated too (previously HTTP-only), so a plugin
stdio server can reference secrets without writing them to disk.

**Example fixture:** `examples/plugins/hello-world/.mcp.json`.


### Open questions (addendum)

1. **Blocking-hook trust from plugins.** Should a plugin-contributed `PreToolUse`
   veto require explicit user opt-in at install (since it can deny tool calls)?
   *Leaning:* yes — surface "this plugin can block tool use" in the install consent,
   and gate it behind a `policy_enforcement` config the user controls.
2. **Catalog-event allow-list per plugin.** Should `plugin.json` have to *declare*
   which of the 33 events it registers (auditable), and the loader reject undeclared
   entries? *Leaning:* yes — declare in `provides.event_hooks` as an array of event
   names rather than a bare `true`.
3. **Managed-config push cadence.** Pull-at-`SessionStart` vs a live channel for org
   policy updates. *Leaning:* start with pull; a live channel can reuse the broker
   work from Phase 4 later.

---

## References

- aish shell architecture: `/docs/ARCHITECTURE.md`
- MCP specification: [https://modelcontextprotocol.io](https://modelcontextprotocol.io)
- JSON Schema: [https://json-schema.org](https://json-schema.org)
- GitHub Webhooks: [https://docs.github.com/en/webhooks](https://docs.github.com/en/webhooks)
- Webhook Signature Verification: [https://docs.github.com/en/webhooks-and-events/webhooks/securing-your-webhooks](https://docs.github.com/en/webhooks-and-events/webhooks/securing-your-webhooks)
