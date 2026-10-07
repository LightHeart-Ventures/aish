# Plugins — authoring webhook handlers

The authoring guide for webhook handlers lives with the rest of the plugin docs:

- **[docs/WEBHOOK_HANDLERS.md](../../../docs/WEBHOOK_HANDLERS.md)** — declaring
  `webhooks[]`, matching and filters, the handler contract (argv, stdin payload,
  `WEBHOOK_*` env, stdout → statusline flash), timeouts, signing, delivery log,
  `:webhook test|replay`, troubleshooting.
- **[docs/PLUGIN_DEVELOPER.md](../../../docs/PLUGIN_DEVELOPER.md)** — the full
  `plugin.json` reference and plugin lifecycle.
- **[plugins/github/](../../../plugins/github/README.md)** — the reference
  webhook plugin (push, pull_request, issues, workflow_run, release) with
  fixtures and tests.

Minimal declaration (the client's `PluginRegistry` reads only `webhooks`; the
legacy `handlers` key was removed in TASK-447 and a manifest still using it is
skipped with a warning):

```json
{
  "id": "github",
  "webhooks": [
    { "event_type": "pull_request", "command": ["handlers/pr-review.sh"],
      "filters": { "action": "opened" }, "timeout_secs": 30 }
  ]
}
```

The wire protocol between the broker and the client is in [CLIENT.md](CLIENT.md).
