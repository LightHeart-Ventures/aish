# TASK-268: Plugin Webhook Handler Dispatch (Phase 5)

**Status**: Design spec for implementation  
**Scope**: Inbound webhook handler routing for Phase 5 of plugin webhook integration  
**References**: 
- `plugin-webhook-events.md` (Phase 1.6 outbound)
- `docs/design/{slack,blacksmith,fly,mailgun}-plugin-integration.md` (Gap #1 + Phase 5)
- Atum task: TASK-268

---

## 1. Problem Statement

Today aish ships:
- **Phase 1.6 (outbound webhooks)**: `plugin_dispatcher.rs` routes *aish events* → plugins (fire-and-forget)
- **Webhook broker (Phase 4)**: A separate HTTP server (`crates/aish-webhook-broker/`) receives webhooks

**Missing (Phase 5)**: **No handler dispatch inside the broker**. When the broker receives a webhook (e.g., GitHub `workflow_run`, Slack `app_mention`, Blacksmith run-complete), it has **no way to route it to the right plugin**.

This blocks features **B** (react to CI completion, Slack actions) in all integration designs:
- Blacksmith: notify on workflow_run completion
- Slack: react to slash commands, button clicks
- Fly: deployment webhooks
- GitHub: PR review workflows

---

## 2. Target State: Handler Registration & Dispatch

### 2.1 What a handler does

A **handler** is a registered callback — typically a webhook-command (shell script) — that runs when the broker receives a matching webhook.

```
Incoming webhook (GitHub workflow_run) 
  ↓
  Broker receives POST /webhooks/github
  ↓
  Broker queries: which plugins handle GitHub workflow_run?
  ↓
  Plugin "blacksmith" registered: webhook_command "sh ~/.aish/plugins/blacksmith/on_workflow_run.sh"
  ↓
  Broker spawns that command, pipes the webhook body on stdin
  ↓
  Handler runs, may mutate plugin state / emit events back to shell
```

### 2.2 Plugin manifest: how handlers are declared

A plugin declares handlers in its `plugin.json` under a new `provides.webhook_handlers` array:

```json
{
  "id": "blacksmith",
  "provides": {
    "lifecycle_hooks": ["on_shell_ready"],
    "webhook_handlers": [
      {
        "source": "github",
        "event_type": "workflow_run",
        "webhook_command": "sh ~/.aish/plugins/blacksmith/on_workflow_run.sh"
      }
    ]
  }
}
```

Or with both URL and command:

```json
{
  "id": "slack",
  "webhook_handlers": [
    {
      "source": "slack",
      "event_type": "slash_command",
      "webhook_url": "https://internal.company.com/slack-handler",
      "webhook_command": "logger -t aish slack command"
    }
  ]
}
```

**Wire names** (stable, lowercase snake_case):
- `github.workflow_run`, `github.pull_request`, `github.release`, …
- `slack.app_mention`, `slack.slash_command`, `slack.button`, …
- `mailgun.message_click`, `mailgun.delivery`, …
- `fly.deploy`, …

### 2.3 Broker ingress: HTTP endpoints

The broker exposes typed paths for each source:

```
POST /webhooks/github        ← GitHub App sends workflow_run, PR events, …
POST /webhooks/slack         ← Slack slash commands, app mentions, …
POST /webhooks/mailgun       ← Mailgun delivery/click/bounce events
POST /webhooks/fly           ← Fly deployments, scale events
POST /webhooks/{source}      ← Catch-all for other sources
```

The broker receives the raw webhook body (GitHub JSON, Slack JSON envelope, Mailgun form-encoded, …) and:

1. **Normalizes** it to an internal `InboundWebhook` struct:
   ```rust
   pub struct InboundWebhook {
       pub source: String,        // "github", "slack", "mailgun"
       pub event_type: String,    // "workflow_run", "slash_command"
       pub timestamp: u64,        // Unix epoch seconds
       pub raw_body: Vec<u8>,     // Original HTTP body (for handler verification)
       pub headers: HashMap<String, String>, // X-Signature headers, etc.
   }
   ```

2. **Dispatches** to all registered handlers matching `(source, event_type)`.

3. **Returns** a 202 (Accepted) immediately — handlers are fire-and-forget.

### 2.4 Dispatch algorithm

```
1. Receive raw webhook → parse to InboundWebhook
2. Read plugin manifests (cache-friendly scan, similar to plugin_dispatcher.rs)
3. Collect handlers where: source == InboundWebhook.source AND event_type == InboundWebhook.event_type
4. For each handler, spawn a tokio task:
   a. If webhook_url is set: HTTP POST the raw body to it (10s timeout, signed)
   b. If webhook_command is set: spawn sh -c, pipe raw body on stdin
   c. Capture exit code / response status
   d. Write result to plugin state store under {plugin_id}:last_webhook_input (for auditing)
5. Return 202 immediately (non-blocking)
```

Same **error handling** as Phase 1.6:
- HTTP timeout: logged, not fatal
- Command spawn failure: logged, not fatal
- Bad/missing manifest: skipped silently
- Slow handler: runs to completion in background (broker doesn't wait)

---

## 3. Integration Points

### 3.1 Plugin manifest parsing (`src/plugins.rs`)

Extend `PluginManifest` struct:

```rust
#[derive(Debug, Clone, Deserialize)]
pub struct PluginManifest {
    pub id: String,
    pub name: String,
    pub version: String,
    pub description: String,
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(default)]
    pub webhook_url: Option<String>,
    #[serde(default)]
    pub webhook_command: Option<String>,
    #[serde(default)]
    pub provides: Option<Provides>,
    // NEW:
    #[serde(default)]
    pub webhook_handlers: Vec<WebhookHandler>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct WebhookHandler {
    pub source: String,           // "github", "slack", "mailgun", …
    pub event_type: String,       // "workflow_run", "slash_command", …
    #[serde(default)]
    pub webhook_url: Option<String>,
    #[serde(default)]
    pub webhook_command: Option<String>,
}
```

### 3.2 Plugin state store audit trail

When a handler runs, persist the invocation to `plugin_state.rs` under a new key:

```
{plugin_id}:webhook_inputs

Value: JSON array of recent webhook invocations (last 100 or 1MB):
[
  {
    "source": "github",
    "event_type": "workflow_run",
    "timestamp": 1717000000,
    "exit_code": 0,
    "stdout": "…",
    "stderr": "…",
    "handler_duration_ms": 250
  },
  …
]
```

Enables diagnostics: `:memory` show would include "last webhook inputs" context.

### 3.3 Broker HTTP layer (`crates/aish-webhook-broker/`)

The broker is a **separate async Tokio service** that:

1. Listens on a local socket (e.g., `127.0.0.1:3334`) or Unix socket
2. Exposes the typed `/webhooks/{source}` endpoints
3. Performs source-specific **signature validation**:
   - GitHub: `X-Hub-Signature-256` HMAC-SHA256 (requires GitHub App secret)
   - Slack: `X-Slack-Request-Timestamp` + `X-Slack-Signature` (requires signing secret)
   - Mailgun: `signature` field (requires API key)
4. Hands off to dispatch (see 2.4 above)

### 3.4 Handler result capture

Each handler invocation produces a result (`InboundWebhookResult`):

```rust
pub struct InboundWebhookResult {
    pub plugin_id: String,
    pub source: String,
    pub event_type: String,
    pub timestamp: u64,
    pub handler_type: HandlerType, // "webhook_url" or "webhook_command"
    pub exit_code: Option<i32>,    // None = killed by signal
    pub http_status: Option<u16>,  // If webhook_url was used
    pub stdout: String,            // If webhook_command
    pub stderr: String,            // If webhook_command
    pub duration_ms: u64,
}
```

Persisted to the plugin state store for audit + observability.

---

## 4. Observability & Debugging

### 4.1 Logging channel

Reuse the existing `plugin-events` channel from Phase 1.6:

```
AISH_PLUGIN_EVENTS=1  # Enable debug logging

[plugin-events] dispatch webhook github.workflow_run → 3 handler(s)
[plugin-events] blacksmith webhook github.workflow_run → exit 0 (150ms)
[plugin-events] blacksmith webhook github.workflow_run → exit 0 (HTTP 202 via Slack notifier)
```

### 4.2 Plugin state inspection

The `:memory` command (via `plugin_memory.rs`) exposes recent webhook inputs:

```
$ aish :memory -plugin blacksmith

Plugin state:
  webhook_inputs (last 10):
    [0] github.workflow_run @ 2026-02-01 14:32:01 → exit 0
    [1] github.workflow_run @ 2026-02-01 14:15:44 → exit 1 (stderr: "no job found")
```

### 4.3 Broker status endpoint

The broker exposes `GET /health` + `GET /stats`:

```json
GET /health
{
  "status": "healthy",
  "uptime_seconds": 86400,
  "plugins_loaded": 5,
  "handlers_registered": 12
}

GET /stats
{
  "webhooks_received": 1234,
  "webhooks_dispatched": 1200,
  "webhooks_failed": 34,
  "handler_timeouts": 2,
  "avg_handler_duration_ms": 180
}
```

---

## 5. Design Patterns & Tradeoffs

### 5.1 Fire-and-forget dispatch

**Decision**: Handlers run **non-blocking**, fire-and-forget (like Phase 1.6).

**Rationale**:
- Broker always returns 202 immediately (fast, never stalls on slow handlers)
- Slow plugins can't DOS the webhook receiver
- Handler results are captured for audit, not returned to the caller

**Consequence**: A handler can't tell the shell "block the tool call" synchronously. That's a **blocking hook** (Phase 2+), not a webhook handler.

### 5.2 No request/response coupling

**Decision**: Handlers don't send a response body back to the webhook source.

**Rationale**:
- Decouples handler from source protocol (GitHub HMAC validation, Slack formatting, …)
- Handlers that need to reply (e.g., Slack button "acking") POST back to Slack directly
- Keeps dispatch simple: "fire and forget"

**Consequence**: A handler can't implement a blocking approval flow in-turn. That requires the **blocking hook** track (Phase 2+).

### 5.3 Raw body preservation

**Decision**: Store the raw webhook body, not a normalized schema.

**Rationale**:
- Signature validation works on the original bytes (GitHub HMAC, Slack timestamp)
- Plugins can opt into custom parsing (GitHub payload differs from Slack JSON)
- Schema evolution: if GitHub adds fields, the raw body still validates

**Consequence**: Handlers must parse their own source format. That's OK; Slack/GitHub handlers are source-specific anyway.

### 5.4 Plugin manifest vs. dynamic registration

**Decision**: Handlers are declared **statically** in `plugin.json`, not registered at runtime.

**Rationale**:
- Manifest is the source of truth (survives plugin reload/restart)
- Broker doesn't need to track plugin-to-handler state in memory
- Plugin auth (login command) sets credentials; manifest defines the handler
- Aligns with Phase 0.5+ pattern (lifecycle hooks in manifest)

**Consequence**: Handlers can't be added/removed dynamically without reloading the plugin manifest (acceptable).

---

## 6. Implementation Phases

### Phase 5A: Broker HTTP layer + dispatch core
- [ ] Extend `PluginManifest` with `webhook_handlers` array
- [ ] Implement `InboundWebhook` + `InboundWebhookResult` structs
- [ ] Broker `/webhooks/{source}` ingress (raw POST)
- [ ] Basic dispatch to matching handlers (webhook_command)
- [ ] Plugin state audit trail (`webhook_inputs` key)
- [ ] Observability: `plugin-events` channel logging
- **Deliverable**: Broker can receive a webhook and dispatch to shell commands

### Phase 5B: Source-specific ingress + verification
- [ ] GitHub App signature validation (`X-Hub-Signature-256`)
- [ ] Slack signature validation (`X-Slack-Request-Timestamp` + `X-Slack-Signature`)
- [ ] Mailgun signature validation
- [ ] Fly signature validation (if applicable)
- [ ] Source-specific error responses (e.g., 401 on bad signature)
- **Deliverable**: Broker validates webhooks before dispatch

### Phase 5C: HTTP handler support + reference implementations
- [ ] HTTP POST handler dispatch (`webhook_url` field)
- [ ] Handler result capture (HTTP status, response body)
- [ ] Reference `blacksmith` plugin (GitHub workflow_run handler)
- [ ] Reference `slack` plugin (slash command handler)
- **Deliverable**: Plugins can respond to webhooks via both shell commands and HTTP

### Phase 5D: Testing + hardening
- [ ] Unit tests: handler dispatch logic
- [ ] Integration tests: broker ↔ plugin communication
- [ ] E2E tests: full GitHub/Slack webhook flow
- [ ] Error handling: timeouts, spawn failures, malformed manifests
- [ ] Docs: operator guide for webhook setup
- **Deliverable**: Production-ready handler dispatch

---

## 7. Manifest Examples

### Blacksmith plugin

```json
{
  "id": "blacksmith",
  "name": "Blacksmith CI Acceleration",
  "version": "1.0.0",
  "provides": {
    "login": "blacksmith",
    "lifecycle_hooks": ["on_shell_ready"]
  },
  "webhook_handlers": [
    {
      "source": "github",
      "event_type": "workflow_run",
      "webhook_command": "sh $HOME/.aish/plugins/blacksmith/handlers/on_workflow_run.sh"
    }
  ]
}
```

### Slack plugin

```json
{
  "id": "slack",
  "name": "Slack Integration",
  "version": "1.0.0",
  "provides": {
    "login": "slack"
  },
  "webhook_handlers": [
    {
      "source": "slack",
      "event_type": "slash_command",
      "webhook_command": "sh $HOME/.aish/plugins/slack/handlers/on_slash_command.sh"
    },
    {
      "source": "slack",
      "event_type": "button_click",
      "webhook_command": "sh $HOME/.aish/plugins/slack/handlers/on_button_click.sh"
    }
  ]
}
```

### Mailgun plugin

```json
{
  "id": "mailgun",
  "name": "Mailgun Email Events",
  "version": "1.0.0",
  "webhook_handlers": [
    {
      "source": "mailgun",
      "event_type": "message_click",
      "webhook_url": "http://127.0.0.1:3334/internal/mailgun-click-handler"
    }
  ]
}
```

---

## 8. Acceptance Criteria

- [ ] `PluginManifest` parses `webhook_handlers` array from `plugin.json`
- [ ] Broker `/webhooks/{source}` accepts POST and returns 202
- [ ] Dispatch collects handlers matching `(source, event_type)` from all enabled plugins
- [ ] Each handler spawns a tokio task; command receives raw webhook body on stdin
- [ ] Handler result (exit code, stdout/stderr) is captured
- [ ] Plugin state store persists handler invocations under `{plugin_id}:webhook_inputs`
- [ ] `AISH_PLUGIN_EVENTS=1` logs dispatch activity
- [ ] Broker `/health` and `/stats` endpoints return meaningful data
- [ ] Integration test: receive a GitHub webhook, dispatch to a shell script, verify it ran
- [ ] Integration test: receive a Slack webhook, dispatch to an HTTP endpoint
- [ ] Docs: operator guide covering webhook setup (GitHub App, Slack, Mailgun)
- [ ] Reference `blacksmith` plugin ships with `on_workflow_run.sh` handler
- [ ] Reference `slack` plugin ships with slash command + button handlers

---

## 9. Non-Goals (Phase 6+)

- **Blocking webhooks** (stall tool execution until approval) → **Phase 2 (hooks track)**
- **Mutating webhooks** (inject context into turns) → **Phase 3 (hooks track)**
- **Plugin-contributed tools** (`testbox run`, `slack_post`) → separate roadmap
- **Plugin-contributed MCP servers** → separate roadmap
- **Query/index plugin state** → separate roadmap
