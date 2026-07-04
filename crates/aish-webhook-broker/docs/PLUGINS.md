# Plugins — authoring webhook handlers

A **plugin** turns broker-delivered webhooks into actions. This page documents
the handler contract an aish plugin implements and walks the **GitHub reference
plugin** (PR #517) shipped under `examples/plugins/github/`.

The runtime contract (how handlers are matched, what they receive on stdin/env,
filters, timeouts) is defined by [aish-webhook-client](CLIENT.md) — read that
first. This page is the authoring-side companion.

## Anatomy of a plugin

```text
examples/plugins/github/
├── plugin.json          # identity + declared webhook handlers (client contract)
├── hooks.json           # aish-native lifecycle + webhook → script routing
├── config.json          # plugin configuration
├── login.sh             # auth/bootstrap helper
├── .mcp.json            # MCP server wiring (optional)
├── handlers/            # one script per webhook event
│   ├── pull_request.sh
│   ├── issues.sh
│   └── review.sh
├── hooks/               # lifecycle scripts
│   ├── on_init.sh
│   ├── on_shell_ready.sh
│   └── on_webhook_url_changed.sh
├── schemas/             # JSON schemas for payload shapes
├── tools/               # tool definitions (add_comment, create_pr, list_issues)
└── skills/              # bundled SKILL.md playbooks (triage, PR review)
```

## Declaring handlers — `plugin.json`

The client's `PluginRegistry` reads `webhooks` (alias `handlers`) from each
plugin's `plugin.json`:

```json
{
  "id": "github",
  "name": "GitHub",
  "version": "1.0.0",
  "webhooks": [
    { "event_type": "pull_request", "command": ["handlers/pull_request.sh"] },
    { "event_type": "issues",       "command": ["handlers/issues.sh"] },
    {
      "event_type": "pull_request_review",
      "command": ["handlers/review.sh"],
      "filters": { "action": "submitted" },
      "timeout_secs": 20
    }
  ]
}
```

Field semantics (`event_type`, `command`, `filters`, `timeout_secs`) are in
[CLIENT.md § Handler dispatch contract](CLIENT.md#handler-dispatch-contract).
Key rules:

- `event_type: "*"` subscribes to every event.
- `filters` are AND-combined equality checks over **dotted payload paths**
  (`pull_request.base.ref`, `action`, …). All must match or the handler is skipped.
- `command` is fork/exec'd directly — **no shell**. Relative paths resolve from
  the plugin directory.

## Lifecycle & event routing — `hooks.json`

The GitHub plugin also ships an aish-native `hooks.json` mapping GitHub's
`X-GitHub-Event` header to a script, plus lifecycle moments:

```json
{
  "lifecycle": {
    "on_init": "hooks/on_init.sh",
    "on_shell_ready": "hooks/on_shell_ready.sh",
    "on_webhook_url_changed": "hooks/on_webhook_url_changed.sh"
  },
  "webhooks": {
    "pull_request": "handlers/pull_request.sh",
    "issues": "handlers/issues.sh",
    "pull_request_review": "handlers/review.sh"
  }
}
```

## What a handler gets

Regardless of routing layer, a handler is invoked the same way:

- **stdin** — the raw provider payload JSON.
- **env** — `WEBHOOK_ID`, `WEBHOOK_TENANT_ID`, `WEBHOOK_PLUGIN_ID`,
  `WEBHOOK_EVENT_TYPE` (client dispatcher). The GitHub example additionally
  documents `$GITHUB_EVENT` (event name) and `$GITHUB_DELIVERY` (delivery id)
  for its own scripts.
- **exit code** — `0` = success; anything else is logged but isolated from other
  handlers.

## Handler template

```bash
#!/usr/bin/env bash
# handlers/pull_request.sh
set -euo pipefail

payload="$(cat)"                                  # raw webhook JSON on stdin
action="$(jq -r '.action // empty'   <<<"$payload")"
number="$(jq -r '.number // empty'   <<<"$payload")"
repo="$(  jq -r '.repository.full_name // empty' <<<"$payload")"

echo "[$WEBHOOK_PLUGIN_ID] $WEBHOOK_EVENT_TYPE #$number ($action) on $repo"

case "$action" in
  opened|reopened|synchronize) exec ./do_review.sh "$number" ;;
  *) echo "ignoring action=$action" ;;
esac
```

## Authoring checklist

- [ ] `plugin.json` has a unique `id` and a `webhooks`/`handlers` array.
- [ ] Every `command[0]` exists, is executable, and reads stdin.
- [ ] Handlers exit non-zero on failure (so outcomes log correctly) and finish
      within `timeout_secs`.
- [ ] Long/expensive work is guarded with `filters` to avoid needless fork/exec.
- [ ] No secrets in `plugin.json`; use `login.sh`/env for credentials.
- [ ] Handlers are idempotent — delivery is **at-least-once**, so the same event
      may arrive more than once after a reconnect.
